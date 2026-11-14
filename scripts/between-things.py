#!/usr/bin/env python3
"""ISOLATED candidate. Requires --result-v1 and an opted-in typed Habit reader.

One atomic cache record holds verdict, fixed attempt and retry failure latch.
Immutable attempt-scoped failure markers retain observations during ownership.
No foreground wait. Existing flock is passed continuously to the refresher.
Do not install or evaluate against live paths without separate migration authority.
"""
import fcntl
import json
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid

REFRESH_BOUND = 480
FAILURE_DIAGNOSTIC_MAX_BYTES = 1800
# Worst case: each input byte is NUL, encoded as six ASCII bytes. Fixed JSON
# overhead is 58 bytes for the exact two fields and a 32-ASCII-byte token.
FAILURE_RECORD_MAX_BYTES = 6 * FAILURE_DIAGNOSTIC_MAX_BYTES + len(
    json.dumps({'attempt': '0' * 32, 'why': ''}, ensure_ascii=False).encode('utf-8'))


def clipped(value, limit):
    if isinstance(value, bytes):
        value = value.decode('utf-8', 'replace')
    return ' '.join(str(value or '').split()).encode('utf-8')[:limit].decode('utf-8', 'ignore')


def validate_failure_record(value, token):
    if (not isinstance(token, str) or not re.fullmatch(r'[0-9a-f]{32}', token)
            or not isinstance(value, dict) or set(value) != {'attempt', 'why'}
            or value['attempt'] != token or not isinstance(value['why'], str)
            or not value['why'] or clipped(value['why'], FAILURE_DIAGNOSTIC_MAX_BYTES) != value['why']):
        raise ValueError('invalid refresh failure marker')
    return value['why']


def encode_failure_record(token, why):
    value = {'attempt': token, 'why': clipped(why, FAILURE_DIAGNOSTIC_MAX_BYTES)}
    validate_failure_record(value, token)
    data = json.dumps(value, ensure_ascii=False).encode('utf-8')
    if len(data) > FAILURE_RECORD_MAX_BYTES:
        raise ValueError('oversized refresh failure marker')
    return data


def decode_failure_record(data, token):
    if len(data) > FAILURE_RECORD_MAX_BYTES:
        raise ValueError('oversized refresh failure marker')
    return validate_failure_record(json.loads(data.decode('utf-8')), token)


def failure(why):
    return ('error', clipped(why, 1800))


def envelope(outcome):
    result, value = outcome
    if result == 'error':
        return 2, '', value
    if result in ('due', 'covered'):
        return 0, json.dumps({'version': 1, 'result': 'true' if result == 'due' else 'false'}), ''
    return 0, json.dumps({'version': 1, 'result': 'deferred',
                         'reason': 'cache refresh pending', 'attempt': value['id'],
                         'started_at': value['started_at'], 'expires_at': value['expires_at']}), ''


def real_clock():
    with open('/proc/sys/kernel/random/boot_id') as stream:
        boot = stream.read().strip()
    return time.time(), time.monotonic(), boot


def finite(value):
    return type(value) in (int, float) and 0 <= value < float('inf')


def validate_verdict(v):
    if (not isinstance(v, dict) or v.get('result') not in ('due', 'covered', 'error')
            or not isinstance(v.get('why'), str)
            or not all(finite(v.get(k)) for k in ('at', 'until')) or v['until'] < v['at']):
        raise ValueError('invalid cached verdict')


def validate_state(s, context):
    if not isinstance(s, dict) or s.get('context') != context:
        raise ValueError('cache context differs')
    if s.get('format') != 'typed-result-v1':
        # Read-only conversion of exact predecessor cache; first publication
        # replaces it. Old refreshers must be quiescent before migration.
        validate_verdict(s)
        return {'format': 'typed-result-v1', 'context': context, 'attempt': None,
                'verdict': s, 'failure': s['why'] if s['result'] == 'error' else None}
    if set(s) != {'format', 'context', 'attempt', 'verdict', 'failure'}:
        raise ValueError('invalid typed cache fields')
    if s['failure'] is not None and (not isinstance(s['failure'], str) or not s['failure']):
        raise ValueError('invalid failure latch')
    if s['verdict'] is not None:
        validate_verdict(s['verdict'])
    a = s['attempt']
    if a is not None:
        if (not isinstance(a, dict) or set(a) != {'id', 'started_at', 'expires_at',
                'mono_start', 'mono_deadline', 'boot', 'status'}
                or not isinstance(a['id'], str) or not re.fullmatch(r'[0-9a-f]{32}', a['id'])
                or type(a['started_at']) is not int or type(a['expires_at']) is not int
                or not 0 <= a['started_at'] < a['expires_at'] <= a['started_at'] + REFRESH_BOUND
                or not finite(a['mono_start']) or not finite(a['mono_deadline'])
                or not a['mono_start'] < a['mono_deadline'] <= a['mono_start'] + REFRESH_BOUND
                or not isinstance(a['boot'], str) or not a['boot']
                or a['status'] not in ('pending', 'complete')):
            raise ValueError('invalid fixed refresh attempt')
        if a['status'] == 'complete' and s['verdict'] is None:
            raise ValueError('completed attempt lacks verdict')
    return s


def attempt_problem(a, clock):
    wall, mono, boot = clock
    if boot != a['boot']:
        return 'refresh attempt %s interrupted by boot change' % a['id']
    if wall < a['started_at'] or mono < a['mono_start']:
        return 'refresh attempt %s clock moved backwards' % a['id']
    if wall >= a['expires_at'] or mono >= a['mono_deadline']:
        return 'refresh attempt %s exceeded fixed deadline' % a['id']
    return None


class Producer:
    """Small injected I/O seam for offline fixtures, not an evaluator/service."""
    def __init__(self, io, context, cache_secs, clock=None, token=lambda: uuid.uuid4().hex):
        self.io, self.context, self.cache_secs = io, context, cache_secs
        self.clock, self.token = real_clock if clock is None else clock, token

    def empty(self, why=None):
        return {'format': 'typed-result-v1', 'context': self.context,
                'attempt': None, 'verdict': None, 'failure': why}

    def read(self):
        try:
            state = validate_state(self.io.read(), self.context)
            a, v = state['attempt'], state['verdict']
            if a is not None and (a['status'] == 'pending' or v['result'] == 'error'):
                observed = self.io.read_failure(a['id'])
                if observed:
                    state = dict(state, failure=observed)
            return state
        except FileNotFoundError:
            return self.empty()
        except Exception as error:
            return self.empty(clipped('cache unreadable: ' + str(error), 1800))

    def poll(self):
        held, state, handed_off = None, None, False
        try:
            held = self.io.acquire()
            state = self.read()  # Re-read AFTER nonblocking acquisition attempt.
            wall, mono, boot = clock = self.clock()
            a, v, old_failure = state['attempt'], state['verdict'], state['failure']
            if a is not None and a['status'] == 'pending':
                why = attempt_problem(a, clock)
                if held is not None:
                    why = why or 'refresh attempt %s abandoned without publication' % a['id']
                old_failure = old_failure or why
                if held is None:
                    if why and not state['failure']:
                        old_failure = self.io.observe_failure(a['id'], why)
                    return failure(old_failure) if old_failure else ('deferred', a)
            else:
                if v is not None and wall < v['at']:
                    old_failure = old_failure or 'cached verdict starts in the future'
                elif v is not None and wall < v['until']:
                    if v['result'] == 'error' or old_failure:
                        return failure(old_failure or v['why'])
                    return v['result'], v['why']
                if v is not None and v['result'] == 'error':
                    old_failure = old_failure or v['why']
            if held is None:
                return failure(old_failure or 'refresh owner has no valid pending attempt')
            # A new retry never replaces a predecessor error with pending grace.
            started = int(wall)
            a = {'id': self.token(), 'started_at': started,
                 'expires_at': started + REFRESH_BOUND, 'mono_start': mono,
                 'mono_deadline': mono + (started + REFRESH_BOUND - wall),
                 'boot': boot, 'status': 'pending'}
            state = dict(state, attempt=a, failure=old_failure)
            self.io.write(state, durable=True)
            problem = attempt_problem(a, self.clock())
            if problem:
                raise RuntimeError(problem)
            # From this point the child MAY exist, including ambiguous spawn
            # exceptions. Parent must never replace the cached snapshot again.
            handed_off = True
            self.io.spawn(held, a['id'])
            # No fallible post-handoff clock observation; generic decoding still
            # checks expiry at capture. The producer never extends this deadline.
            return failure(old_failure) if old_failure else ('deferred', a)
        except Exception as error:
            why = (state or {}).get('failure') or clipped('cannot start refresh: ' + str(error), 1800)
            if handed_off:
                try:
                    why = self.io.observe_failure(state['attempt']['id'], why)
                except Exception as retained_error:
                    why = clipped('cannot retain refresh failure: ' + str(retained_error), 1800)
            elif held is not None and state is not None:
                try:
                    self.io.write(dict(state, failure=why), durable=True)
                except Exception:
                    pass  # No child: unresolved pending is failure on next free-lock poll.
            return failure(why)
        finally:
            if held is not None:
                self.io.close(held)  # Never LOCK_UN: child shares this flock description.

    def finish(self, token, result, why, details=None):
        state = self.read()
        a = state['attempt']
        if a is None or a['id'] != token or a['status'] != 'pending':
            raise RuntimeError('refresh attempt changed before publication')
        if result != 'error':
            problem = attempt_problem(a, self.clock())
            if problem:
                result, why = 'error', problem
        wall = self.clock()[0]
        verdict = {'result': result, 'why': clipped(why, 1800),
                   'at': wall, 'until': wall + self.cache_secs}
        if details is not None:
            verdict['diagnostic'] = details
        final = dict(state, attempt=dict(a, status='complete'), verdict=verdict,
                     failure=(state['failure'] or verdict['why']) if result == 'error' else None)

        def before_replace():
            if result != 'error':
                problem = attempt_problem(a, self.clock())
                if problem:
                    raise RuntimeError(problem)

        try:
            self.io.write(final, durable=False, before_replace=before_replace)
        except Exception as error:
            # Keep the pending token and first failure; a later free-lock poll
            # retries visibly. Never publish an unvalidated successful verdict.
            why = state['failure'] or clipped('cache publication failed: ' + str(error), 1800)
            try:
                self.io.write(dict(state, failure=why), durable=True)
            except Exception:
                pass  # Old durable pending remains; owner loss/expiry fails closed.
            return failure(why)
        return failure(final['failure']) if final['failure'] else (result, verdict['why'])


class NativeIO:
    def __init__(self, cache, argv):
        self.cache, self.argv = cache, argv

    def acquire(self):
        os.makedirs(os.path.dirname(self.cache), exist_ok=True)
        fd = os.open(self.cache + '.lock', os.O_CREAT | os.O_RDWR, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            os.close(fd)
            return None
        except BaseException:
            os.close(fd)
            raise
        return fd

    def close(self, fd):
        os.close(fd)

    def read(self):
        with open(self.cache) as stream:
            return json.load(stream)

    def failure_path(self, token):
        if not re.fullmatch(r'[0-9a-f]{32}', token):
            raise ValueError('invalid failure marker token')
        return self.cache + '.attempt-' + token + '.failure'

    def read_failure(self, token):
        try:
            with open(self.failure_path(token), 'rb') as stream:
                data = stream.read(FAILURE_RECORD_MAX_BYTES + 1)
        except FileNotFoundError:
            return None
        return decode_failure_record(data, token)

    def observe_failure(self, token, why):
        # Separate immutable namespace: never replaces cache or another marker.
        # Atomic hard-link commits the complete file; first observer wins.
        directory = os.path.dirname(self.cache)
        target = self.failure_path(token)
        data = encode_failure_record(token, why)  # Validate before any staging write.
        fd, tmp = tempfile.mkstemp(prefix='.coverage-failure-', dir=directory)
        try:
            with os.fdopen(fd, 'wb') as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            try:
                os.link(tmp, target)
            except FileExistsError:
                pass
            directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
            return self.read_failure(token)
        finally:
            if os.path.exists(tmp):
                os.unlink(tmp)

    def write(self, state, durable, before_replace=lambda: None):
        directory = os.path.dirname(self.cache)
        fd, tmp = tempfile.mkstemp(prefix='.coverage-', dir=directory)
        try:
            with os.fdopen(fd, 'w') as stream:
                json.dump(state, stream, allow_nan=False)
                stream.flush()
                os.fsync(stream.fileno())
            before_replace()
            os.replace(tmp, self.cache)  # Sole successful verdict publication boundary.
            if durable:
                directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(directory_fd)
                finally:
                    os.close(directory_fd)
        finally:
            if os.path.exists(tmp):
                os.unlink(tmp)

    def spawn(self, fd, token):
        subprocess.Popen([sys.executable, '-I', os.path.abspath(__file__), *self.argv,
                          '--refresh-fd', str(fd), '--attempt', token],
                         pass_fds=(fd,), close_fds=True, stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)


class OutOfTime(BaseException):
    pass


def refresh_worker(producer, fd, token):
    global now, refresh_clock, phase, call_index, call
    # This fd is passed only by NativeIO.spawn; do not reopen the lock and create
    # a handoff gap. Reassert the existing lock; make descendants close it.
    expected = os.stat(producer.io.cache + '.lock')
    actual = os.fstat(fd)
    if (expected.st_dev, expected.st_ino) != (actual.st_dev, actual.st_ino):
        raise RuntimeError('inherited refresh fd is not the cache lock')
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    os.set_inheritable(fd, False)
    try:
        state = producer.read()
        a = state['attempt']
        if a is None or a['id'] != token or a['status'] != 'pending':
            return failure('inherited refresh attempt does not match')
        wall, mono, _ = clock = producer.clock()
        problem = attempt_problem(a, clock)
        if problem:
            return producer.finish(token, 'error', problem)
        now, refresh_clock = wall, a['mono_start']
        phase, call_index, call = 'decide', 0, None

        def out_of_time(*_):
            raise OutOfTime('refresh exceeded fixed attempt deadline')

        signal.signal(signal.SIGALRM, out_of_time)
        signal.setitimer(signal.ITIMER_REAL, min(a['expires_at'] - wall, a['mono_deadline'] - mono))
        details = None
        try:
            try:
                due, why = decide()
                result = 'due' if due else 'covered'
                phase = 'publish'
                signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGALRM})
            except (Exception, OutOfTime) as error:
                signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGALRM})
                result = 'error'
                why, details = diagnostic(error)
            return producer.finish(token, result, why, details)
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, signal.SIG_IGN)
            signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGALRM})
    finally:
        os.close(fd)


def main(argv=None):
    global persona, fresh, lock_tag, pile
    argv = sys.argv[1:] if argv is None else argv
    def arg(name, default=None):
        if name in argv:
            index = argv.index(name)
            if index + 1 >= len(argv):
                raise ValueError('missing argument for ' + name)
            return argv[index + 1]
        return default
    try:
        if '--result-v1' not in argv:
            raise ValueError('requires --result-v1 with when-result-v1 reader opt-in')
        persona, lock_tag = arg('--persona'), arg('--lock-tag')
        if not persona or not re.fullmatch(r'[A-Za-z0-9_-]+', persona):
            raise ValueError('persona must be a safe cache basename')
        fresh, cache_secs = int(arg('--fresh-mins', '45')) * 60, int(arg('--cache-secs', '600'))
        if fresh <= 0 or cache_secs <= 0:
            raise ValueError('freshness and cache seconds must be positive')
        pile = os.environ.get('PILE')
        if not pile:
            raise ValueError('predicate PILE is missing')
        pile = os.path.abspath(pile)
        os.environ['PILE'] = pile
        context = {'version': 3, 'pile': pile, 'fresh': fresh, 'lock_tag': lock_tag}
        cache_dir = os.path.join(os.environ.get('XDG_CACHE_HOME') or os.path.expanduser('~/.cache'), 'colony-coverage')
        cache = os.path.join(cache_dir, persona + '.json')
        producer = Producer(NativeIO(cache, argv), context, cache_secs)
        refresh_fd = arg('--refresh-fd')
        outcome = (refresh_worker(producer, int(refresh_fd), arg('--attempt'))
                   if refresh_fd is not None else producer.poll())
    except Exception as error:
        outcome = failure(error)
    code, stdout, stderr = envelope(outcome)
    if stdout:
        print(stdout)
    if stderr:
        print(stderr, file=sys.stderr)
    return code


# Decision and diagnostic code retained from pinned predecessor; child subprocesses
# explicitly close inherited descriptors.
def observed_call(label, argv, timeout, **kwargs):
    global phase, call_index, call
    call_index += 1
    call = {"label": label, "start": time.monotonic(), "elapsed": None, "stderr": ""}
    phase = "wait"
    try:
        out = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, close_fds=True, **kwargs)
    except subprocess.TimeoutExpired as e:
        call["stderr"] = clipped(e.stderr, 768)
        raise
    finally:
        call["elapsed"] = time.monotonic() - call["start"]
    phase = "read-result"
    if out.returncode:
        call["stderr"] = clipped(out.stderr, 768)
    return out

def diagnostic(error):
    ended = time.time()
    elapsed = time.monotonic() - refresh_clock
    current = "none" if call is None else call["label"]
    duration = 0 if call is None else (
        call["elapsed"] if call["elapsed"] is not None else time.monotonic() - call["start"])
    # Do not stringify TimeoutExpired: its argv may contain a private selector
    # or executable path. Command labels below contain only fixed words/IDs.
    detail = ("call exceeded %s s" % error.timeout if isinstance(error, subprocess.TimeoutExpired)
              else clipped(error, 384))
    detail = "%s: %s; phase=%s call=%d [%s]" % (
        type(error).__name__, detail, phase, call_index, current)
    if call is not None and call["stderr"]:
        detail += "; stderr=" + call["stderr"]
    # Orient uses the full warning as its attention identity. Keep changing
    # timings in this cache's optional diagnostic field, never in `why`.
    timing = {"refresh_start": now, "refresh_end": ended, "refresh_elapsed": elapsed,
              "phase": phase, "call": None if call is None else {
                  "index": call_index, "command": current, "elapsed": duration}}
    # Leave room for the Habit evaluator's exit-status prefix within 2048 bytes.
    return clipped(detail, 1800), timing

def run(*argv):
    exe = shutil.which(argv[0]) or os.path.expanduser("~/.local/bin/" + argv[0])
    # A moving cutoff is call input, never attention identity.
    label = ("relations show <persona>" if argv[0] == "relations" else
             "compass has-activity <actor> <since> doing blocked" if argv[:2] == ("compass", "has-activity") else
             " ".join(argv))
    out = observed_call(label, [exe, *argv[1:]], 120, stdin=subprocess.DEVNULL)
    if out.returncode != 0:
        raise RuntimeError("%s exited %d" % (label, out.returncode))
    return out.stdout

def decide():
    # Exact anchors need no authored status or profile lookup. Labels/aliases
    # use Relations' existing selector, which reports ambiguity as an error.
    if re.fullmatch(r"[0-9a-fA-F]{32}", persona) and int(persona, 16) != 0:
        me = persona.lower()
    else:
        head = run("relations", "show", persona).splitlines()
        match = re.fullmatch(r"person: ([0-9a-f]{32})", head[0] if head else "")
        if not match or int(match.group(1), 16) == 0:
            raise ValueError("relations show: no person id for %s" % persona)
        me = match.group(1)
    if lock_tag:
        lock = os.path.expanduser("~/liora/mary/scripts/gb10-lock.sh")
        if os.access(lock, os.X_OK):
            for box in ("sky", "stars"):
                checked = observed_call("reservation check " + box, [lock, "check", box], 30)
                held = checked.stdout
                if checked.returncode not in (0, 3):
                    raise RuntimeError("reservation check %s exited %d" % (box, checked.returncode))
                if not ((checked.returncode == 0 and held.strip() == "FREE")
                        or (checked.returncode == 3 and "tag=" in held)):
                    raise ValueError("reservation check %s: unrecognized result" % box)
                if re.search(r"tag=%s" % re.escape(lock_tag), held):
                    return False, "covered: holds the %s reservation" % box
    # Preserve the previous predicate's boundary: it read whole TAI calendar
    # seconds as Unix/UTC seconds, discarding fractional seconds. A whole-second
    # TAI cutoff at ceil(now-fresh) is equivalent; this is not a leap-second fix.
    since = time.strftime("%Y-%m-%dT%H:%M:%S TAI", time.gmtime(math.ceil(now - fresh)))
    result = json.loads(run("compass", "has-activity", "--actor", me,
                            "--since", since, "--status", "doing", "--status", "blocked"))
    if type(result) is not bool:
        raise ValueError("compass has-activity: expected a JSON boolean")
    if result:
        return False, "covered: own activity on a doing/blocked goal within %d min" % (fresh // 60)
    return True, "between things: no own move or note on a doing/blocked goal within %d min, no reservation" % (fresh // 60)


if __name__ == '__main__':
    sys.exit(main())
