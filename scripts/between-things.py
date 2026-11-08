#!/usr/bin/env python3
# Habit predicate: exit 0 (due) when this zooid is between things.
#
#   colony-coverage --due --persona LABEL [--fresh-mins 45] [--lock-tag PREFIX] [--cache-secs 600]
#
# JP, 2026-11-06: Compass is for persistent goals and autonomy, not a todo list,
# so this never measures busyness. It only notices the gap after a finished step,
# and the nudge asks what the zooid wants to pursue next. Covered (not due) when,
# within the last --fresh-mins, this persona itself did any of:
#   - moved or noted a goal that is now doing or blocked (shared rows count only
#     through one's own activity, never through a tag alone);
#   - set a status that says it is paused or waiting;
# or when it holds a Spark reservation (a gb10 lock whose tag starts with
# --lock-tag). A quiet stretch is a prompt to look, never a verdict of idleness:
# timestamps cannot see reading, thinking or a long build. Exit 0 means due,
# 1 means covered, and 2 means unknown/error, with a diagnostic on stderr.
# A stale or missing cache starts the existing detached refresh. A recent
# verdict remains usable for one cache interval while refreshing; unknown state
# never becomes a verdict. Errors are cached explicitly, including timeouts.
# Cache context includes the resolved pile, freshness window and reservation
# prefix; old boolean-only caches cannot preserve previously swallowed errors.
# The exclusive flock is never unlinked, and only one refresher owns it.
# The whole refresh remains bounded by REFRESH_BOUND; publishing the result
# blocks that signal so a timeout cannot misreport what was published.
import calendar, fcntl, json, os, re, shutil, signal, subprocess, sys, time

def arg(name, default):
    if name in sys.argv:
        i = sys.argv.index(name)
        if i + 1 < len(sys.argv):
            return sys.argv[i + 1]
    return default

persona = arg("--persona", None)
if not persona:
    print("colony-coverage: --persona is required", file=sys.stderr); sys.exit(2)
fresh = int(arg("--fresh-mins", "45")) * 60
lock_tag = arg("--lock-tag", None)
cache_secs = int(arg("--cache-secs", "600"))
pile = os.environ.get("PILE")
if not pile:
    print("colony-coverage: predicate PILE is missing", file=sys.stderr); sys.exit(2)
pile = os.path.abspath(pile)
os.environ["PILE"] = pile
context = {"version": 2, "pile": pile, "fresh": fresh, "lock_tag": lock_tag}
now = time.time()
REFRESH_BOUND = 480

cache_dir = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "colony-coverage")
cache = os.path.join(cache_dir, "%s.json" % persona)
def try_lock(path):
    """An open fd holding an exclusive flock on `path`, or None if another
    process holds it. Closing the fd (or exiting) releases it."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        return None
    return fd

def report(result, why):
    print(why, file=sys.stderr if result == "error" else sys.stdout)
    sys.exit({"due": 0, "covered": 1, "error": 2}[result])

refresh = "--refresh" in sys.argv
if not refresh:
    c = None
    problem = "no cached result"
    try:
        with open(cache) as f:
            c = json.load(f)
        if c.get("context") != context:
            raise ValueError("cache context differs or predates error-preserving results")
        if (c.get("result") not in ("due", "covered", "error")
                or not isinstance(c.get("why"), str)
                or not all(type(c.get(k)) in (int, float) and 0 <= c[k] < float("inf")
                           for k in ("at", "until"))
                or c["until"] < c["at"]):
            raise ValueError("invalid cached result")
    except Exception as e:
        c = None
        problem = "cache unreadable: %s" % e
    if c is None or now >= c["until"]:
        try:
            # Existing one-refresh flock protocol; no retry or new watcher.
            probe = try_lock(cache + ".lock")
            if probe is not None:
                os.close(probe)
                subprocess.Popen([sys.executable, "-I", os.path.abspath(__file__), *sys.argv[1:], "--refresh"],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True)
        except Exception as e:
            report("error", "cannot start refresh: %s" % e)
    if c is None or now >= c["until"] + cache_secs:
        report("error", "%s for %s (refresh requested)" %
               (problem if c is None else "cached result expired", persona))
    report(c["result"], "%s (checked %ds ago)" % (c["why"], now - c["at"]))

def run(*argv):
    exe = shutil.which(argv[0]) or os.path.expanduser("~/.local/bin/" + argv[0])
    out = subprocess.run([exe, *argv[1:]], capture_output=True, text=True, timeout=120,
                         stdin=subprocess.DEVNULL)
    if out.returncode != 0:
        raise RuntimeError("%s exited %d: %s" % (" ".join(argv), out.returncode, out.stderr.strip()[:2048] or "no stderr"))
    return out.stdout

STAMP = re.compile(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.\d+)? TAI")

def utc_age(stamp):
    # TAI runs 37 s ahead of UTC; irrelevant at minute granularity.
    return now - calendar.timegm(time.strptime(stamp, "%Y-%m-%dT%H:%M:%S"))

def decide():
    head = run("status", "show", persona).splitlines()
    m = re.search(r"\(([0-9a-f]{32})\)", head[0] if head else "")
    if not m:
        raise ValueError("status show: no persona id for %s" % persona)
    me = m.group(1)
    if len(head) < 2:
        raise ValueError("status show: missing current status")
    # An explicit pause or wait in the current status, if it is fresh.
    for line in head[1:2]:
        cur = re.match(r"\*\s+(.*)\s+\((\d+)([smhd]) ago\)\s*$", line)
        if not cur and line != "- (no status set)":
            raise ValueError("status show: unrecognized current status")
        if cur:
            text, n, unit = cur.group(1), int(cur.group(2)), cur.group(3)
            secs = n * {"s": 1, "m": 60, "h": 3600, "d": 86400}[unit]
            if secs <= fresh and re.match(r"(paused|waiting)\b", text.strip(), re.I):
                return False, "covered: fresh status says %s" % text.split()[0].lower()
    if lock_tag:
        lock = os.path.expanduser("~/liora/mary/scripts/gb10-lock.sh")
        if os.access(lock, os.X_OK):
            for box in ("sky", "stars"):
                checked = subprocess.run([lock, "check", box], capture_output=True, text=True,
                                         timeout=30)
                held = checked.stdout
                if checked.returncode not in (0, 3):
                    raise RuntimeError("reservation check %s exited %d: %s" %
                                       (box, checked.returncode, (checked.stderr or held).strip()[:2048]))
                if not ((checked.returncode == 0 and held.strip() == "FREE")
                        or (checked.returncode == 3 and "tag=" in held)):
                    raise ValueError("reservation check %s: unrecognized result" % box)
                if re.search(r"tag=%s" % re.escape(lock_tag), held):
                    return False, "covered: holds the %s reservation" % box
    listing = run("compass", "list", "doing", "blocked")
    ids = re.findall(r"^\s*- \[([0-9a-f]{32})\]", listing, re.M)
    counts = re.findall(r"^== (?:DOING|BLOCKED) \((\d+)\) ==$", listing, re.M)
    if listing.strip() != "No goals yet." and (not counts or sum(map(int, counts)) != len(ids)):
        raise ValueError("compass list: unrecognized or incomplete board")
    newest = None
    for gid in ids:
        show = run("compass", "show", gid)
        if not show.startswith("Goal %s\n" % gid):
            raise ValueError("compass show %s: missing goal header" % gid)
        for line in show.splitlines():
            if ("by " + me) not in line:
                continue
            s = STAMP.search(line)
            if not s and line.startswith("- "):
                raise ValueError("compass show %s: unrecognized activity timestamp" % gid)
            if s:
                a = utc_age(s.group(1))
                if a <= fresh:
                    return False, "covered: own activity on doing/blocked goal %s %d min ago" % (gid[:8], a // 60)
                if newest is None or a < newest[0]:
                    newest = (a, gid[:8])
    if newest and newest[0] <= fresh:
        return False, "covered: own activity on doing/blocked goal %s %d min ago" % (newest[1], newest[0] // 60)
    since = "never" if newest is None else "%d min ago (goal %s)" % (newest[0] // 60, newest[1])
    return True, "between things: no own move or note on a doing/blocked goal within %d min (last %s), no fresh pause, no reservation" % (fresh // 60, since)

try:
    held = try_lock(cache + ".lock")
except Exception as e:
    report("error", "refresh lock failed: %s" % e)
if held is None:
    report("error", "a refresh for %s is already running" % persona)

class OutOfTime(BaseException):
    """Keep the whole-refresh bound distinct from individual call failures."""

def out_of_time(*_):
    raise OutOfTime("refresh past %d s" % REFRESH_BOUND)
signal.signal(signal.SIGALRM, out_of_time)
signal.alarm(REFRESH_BOUND)
try:
    try:
        due, why = decide()
        result = "due" if due else "covered"
    except (Exception, OutOfTime) as e:
        result, why = "error", "%s: %s" % (type(e).__name__, str(e)[:4096])
    signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGALRM})
    tmp = "%s.tmp.%d" % (cache, os.getpid())
    try:
        with open(tmp, "w") as f:
            json.dump({"result": result, "why": why, "at": now,
                       "until": now + cache_secs, "context": context}, f)
        os.replace(tmp, cache)
    except Exception as e:
        result, why = "error", "cache publication failed: %s" % e
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
        signal.alarm(0)
        signal.signal(signal.SIGALRM, signal.SIG_IGN)
        signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGALRM})
finally:
    os.close(held)
report(result, why)
