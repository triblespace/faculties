#!/usr/bin/env python3
"""Translate local Git history to a native Wiki/Files import plan, without opening a pile.

All reachable commit trees are read as Git objects, without checking out files,
following symlinks, executing repository code, or expanding AsciiDoc includes.
Recognized articles become Wiki revisions with exact source-file citations.
Original page files and other tracked file versions are stored in Files.
Formats without a native renderer retain their text as an inert Wiki source view.

An image the source shows inline (Markdown `![alt](src)`, a Gollum `[[x.png]]`
tag, an AsciiDoc `image:` macro) is embedded as `#image("plan:source/<oid>")`,
which the importer resolves to `#image("files:<content hash>")`, when it names
displayable bytes in the commit's own tree: a relative or root path, or an
absolute URL under a `--source-base` the wiki was served from. Anything else
keeps the earlier link rendering. Every revision with an embedded image also
lists, per image, the earlier rendering and its exact place in that earlier
body, so already imported histories can be corrected in place.
"""
import argparse
import hashlib
import importlib.machinery
import importlib.util
import json
import os
import posixpath
import re
import subprocess
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import unquote, urlsplit

HERE = Path(__file__).resolve().parent


def load(name, filename):
    loader = importlib.machinery.SourceFileLoader(name, str(HERE / filename))
    spec = importlib.util.spec_from_loader(name, loader)
    result = importlib.util.module_from_spec(spec)
    loader.exec_module(result)
    return result


markdown = load("gollum_markdown", "gollum-plan")
asciidoc = load("gollum_asciidoc", "gollum-asciidoc.py")
MARKDOWN = set(markdown.PAGE_EXTENSIONS)
ASCIIDOC = {".asciidoc", ".adoc", ".asc"}
OTHER = set(markdown.OTHER_MARKUP) - ASCIIDOC
MARKUP = MARKDOWN | ASCIIDOC | OTHER
markdown.PAGE_EXTENSIONS.update(MARKUP)
MIME = {".pdf": "application/pdf", ".png": "image/png", ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg", ".gif": "image/gif", ".svg": "image/svg+xml",
        ".gz": "application/gzip", ".zip": "application/zip", ".html": "text/html",
        ".css": "text/css", ".js": "text/javascript", ".json": "application/json",
        ".gollum": "text/plain", **{s: "text/markdown" for s in MARKDOWN},
        **{s: "text/asciidoc" for s in ASCIIDOC}, **{s: "text/plain" for s in OTHER}}


SITE_OPEN, SITE_CLOSE = asciidoc.SITE_OPEN, "\ue001"
SITE = re.compile(SITE_OPEN + r"(\d+)" + SITE_CLOSE)


def image_kind(data):
    """The raster or vector format Typst can show, sniffed from the bytes."""
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "png"
    if data.startswith(b"\xff\xd8\xff"):
        return "jpg"
    if data[:6] in (b"GIF87a", b"GIF89a"):
        return "gif"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "webp"
    head = data[:4096].lstrip(b"\xef\xbb\xbf \t\r\n")
    if head.startswith(b"<") and b"<svg" in head:
        return "svg"
    return None


class SourceBase:
    """Where the original wiki was served, so absolute links back to it name
    files in its own tree. `http://host/wiki/` or a root path such as `/wiki/`."""

    def __init__(self, raw):
        parts = urlsplit(raw)
        if parts.scheme:
            if parts.scheme.lower() not in ("http", "https") or not parts.hostname:
                raise SystemExit(f"--source-base {raw!r}: expected http(s)://host/ or /path/")
            self.host = (parts.hostname.lower(), parts.port)
        elif raw.startswith("/") and not raw.startswith("//"):
            self.host = None
        else:
            raise SystemExit(f"--source-base {raw!r}: expected http(s)://host/ or /path/")
        if parts.query or parts.fragment:
            raise SystemExit(f"--source-base {raw!r}: no query or fragment")
        self.path = unquote(parts.path or "/").rstrip("/") + "/"

    def relative(self, target):
        """The tree path an absolute target names under this base, or None."""
        parts = urlsplit(target)
        if self.host is None:
            if parts.scheme or parts.netloc:
                return None
        elif (parts.scheme.lower() not in ("http", "https") or not parts.hostname
              or (parts.hostname.lower(), parts.port) != self.host):
            return None
        path = unquote(parts.path)
        if not (path + "/").startswith(self.path) or path + "/" == self.path:
            return None
        return path[len(self.path):]


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args])


def safe_path(raw):
    """Represent non-UTF8 or control-containing names reversibly in JSON/Typst."""
    prefix = "git-path-hex/"
    try:
        name = raw.decode("utf8")
        if not name.startswith(prefix) and all(ord(c) >= 32 and ord(c) != 127 for c in name):
            return name
    except UnicodeDecodeError:
        pass
    suffix = posixpath.splitext(raw)[1]
    return prefix + raw.hex() + (suffix.decode() if re.fullmatch(rb"\.[A-Za-z0-9]+", suffix) else "")


def item_key(prefix, sha, path):
    return prefix + "/" + sha + "/" + hashlib.sha256(path.encode()).hexdigest()


def source_key(oid):
    return "source/" + oid


def link(key, caption):
    return '#link("plan:' + key + '")[' + caption + ']'


def page(path, item):
    return item[0] in ("100644", "100755") and Path(path).suffix.lower() in MARKUP


def read_tree(repo, commit):
    entries = {}
    for record in git(repo, "ls-tree", "-rz", commit).split(b"\0"):
        if record:
            head, name = record.split(b"\t", 1)
            mode, _, oid = head.decode().split()
            entries[safe_path(name)] = (mode, oid)
    return entries


def renames(repo, parent, sha):
    fields = git(repo, "diff-tree", "-r", "--no-ext-diff", "--no-textconv", "-M50%",
                 "--name-status", "-z", parent, sha).split(b"\0")
    result = {}
    index = 0
    while fields[index]:
        status = fields[index]
        if status.startswith((b"R", b"C")):
            if status.startswith(b"R"):
                result[safe_path(fields[index + 2])] = safe_path(fields[index + 1])
            index += 3
        else:
            index += 2
    return result


class Rendering:
    """A page body with embedded images marked, rendered both ways."""

    def __init__(self, body, sites):
        self.body = body
        self.sites = sites

    def replace(self, old, new):
        return Rendering(self.body.replace(old, new),
                         [(a.replace(old, new), b.replace(old, new)) for a, b in self.sites])

    def text(self, legacy=False):
        return SITE.sub(lambda m: self.sites[int(m[1])][0 if legacy else 1], self.body)

    def images(self, prefix=""):
        """Each embedded image with its earlier rendering and that rendering's
        exact place in the earlier body: its ordinal among equal snippets in the
        whole body, and the lines it spans with its ordinal among them there."""
        rows, cursor, start = [], 0, len(prefix)
        for m in SITE.finditer(self.body):
            snippet, shown = self.sites[int(m[1])]
            start += m.start() - cursor
            rows.append((start, snippet, shown))
            start += len(snippet)
            cursor = m.end()
        whole = prefix + self.text(legacy=True)
        result = []
        for start, snippet, shown in rows:
            first = whole.rfind("\n", 0, start) + 1
            last = whole.find("\n", start + len(snippet))
            anchor = whole[first:last if last != -1 else len(whole)]
            result.append(dict(legacy=snippet, display=shown,
                               ordinal=occurrence(whole, snippet, start),
                               anchor=anchor, anchor_ordinal=occurrence(anchor, snippet, start - first)))
        return result


def occurrence(text, snippet, start):
    """Index of the non-overlapping occurrence of `snippet` at `start`, or None."""
    index, at = 0, text.find(snippet)
    while at != -1 and at < start:
        index += 1
        at = text.find(snippet, at + len(snippet))
    return index if at == start else None


@dataclass
class State:
    tree: dict
    pages: dict  # path -> (logical page key, exact native predecessor key)
    removed: dict  # logical page key -> terminal native revision key


class Converter(markdown.Page):
    def __init__(self, owner, sha, path, tree, pages):
        super().__init__(owner, sha, path, pages, {p for p, v in tree.items() if not page(p, v) and v[0] != "160000"})
        self.tree = tree
        self.sites = []  # (earlier rendering, embedded rendering)
        self.quiet = 0

    def finding(self, kind, detail, loss):
        if not self.quiet:
            super().finding(kind, detail, loss)

    def link(self, label, target, image=False, display=None):
        if display is not None:
            shown = self.show(display)
            if shown is not None:
                # The earlier rendering of the same construct, without its
                # findings or link dependencies: only its text is recorded.
                self.quiet += 1
                links = list(self.links)
                try:
                    legacy = self.plain_link(label, target, image)
                finally:
                    self.quiet -= 1
                    self.links = links
                self.sites.append((display.get("legacy_prefix", "") + legacy, shown))
                return SITE_OPEN + str(len(self.sites) - 1) + SITE_CLOSE
        return self.plain_link(label, target, image)

    def show(self, display):
        """Typst embedding the image `display` names, or None to keep a link."""
        target = unquote(display["target"].strip().strip("<>"))
        clean = target.split("#", 1)[0].split("?", 1)[0]
        if re.match(r"^[A-Za-z][A-Za-z0-9+.-]*:", target) or clean.startswith("//"):
            relative = next((r for r in (b.relative(clean) for b in self.driver.bases) if r is not None), None)
            if relative is None:
                self.finding("external-image-kept-as-link", "", False)
                return None
            candidates = [relative.lstrip("/")]
        else:
            relative = next((r for r in (b.relative(clean) for b in self.driver.bases if b.host is None)
                             if r is not None), None)
            direct = (relative if relative is not None else clean).lstrip("/")
            candidates = [posixpath.normpath(posixpath.join(self.dir, clean)).lstrip("/"), direct]
            fallback = self.file_target(direct)
            if fallback:
                candidates.append(fallback)
        match = next((c for c in candidates if c in self.tree), None)
        if not match or self.tree[match][0] not in ("100644", "100755") or page(match, self.tree[match]):
            self.finding("unresolved-image-kept-as-link", "", False)
            return None
        oid = self.tree[match][1]
        if image_kind((self.driver.out / "blobs" / oid).read_bytes()) is None:
            self.finding("undisplayable-image-kept-as-link", "", False)
            return None
        args = asciidoc._string("plan:" + source_key(oid))
        if display.get("alt"):
            args += ", alt: " + asciidoc._string(display["alt"])
        caption = display.get("caption_typst")
        if caption is None and display.get("caption"):
            caption = markdown.escape_plain(display["caption"], False)
        if caption:
            return "#figure(image(" + args + "), caption: [" + caption + "])"
        return "#image(" + args + ")"

    def plain_link(self, label, target, image=False):
        original = target
        target = unquote(target.strip().strip("<>"))
        if re.match(r"^[A-Za-z][A-Za-z0-9+.-]*:", target):
            if re.match(r"(?i)^(https?|mailto):", target):
                return "#link(" + asciidoc._string(target) + ")[" + (label or markdown.escape_plain(target, False)) + "]"
            self.finding("non-web-uri-as-text", "", True)
            return self.literal_target(original, label)
        path_query, _, anchor = target.partition("#")
        clean, _, query = path_query.partition("?")
        if not clean:
            if anchor:
                self.finding("local-anchor-as-text", "", False)
                return label or markdown.escape_plain(anchor.replace("-", " "), False)
            return self.literal_target(original, label)
        rel = posixpath.normpath(posixpath.join(self.dir, clean)).lstrip("/")
        direct = clean.lstrip("/")
        match = next((p for p in (rel, direct) if p in self.tree), None)
        if not match:
            match = self.page_target(direct) or self.file_target(direct)
        if not match or self.tree[match][0] == "160000":
            self.finding("unresolved-source-target", "", True)
            return self.literal_target(original, label)
        caption = label or markdown.escape_plain(Path(match).name, False)
        if anchor or query:
            self.finding("section-or-query-omitted-from-page-link", "", True)
        if match in self.tree_pages:
            key = self.tree_pages[match]
            self.links.append(key)
        else:
            key = source_key(self.tree[match][1])
            if image:
                self.finding("image-as-versioned-file-link", "", False)
        return link(key, caption)

    @staticmethod
    def literal_target(original, label):
        escaped = markdown.escape_plain(original, False)
        if not label or label == escaped:
            return escaped
        return label + " (" + asciidoc._raw(original) + ")"


class Adapter:
    def __init__(self, repo, out, bases=()):
        self.repo = repo
        self.out = out
        self.bases = list(bases)
        self.commits = []
        self.changes = []
        self.findings = []
        self.aliases = {}
        self.versions = set()
        self.order = 0
        self.images = 0

    def finding(self, kind, commit, path, detail, loss):
        self.findings.append(dict(kind=kind, commit=commit, path=path, rendering_loss=loss))

    def canonical(self, key):
        trail = []
        while key in self.aliases:
            trail.append(key)
            key = self.aliases[key]
        for previous in trail:
            self.aliases[previous] = key
        return key

    def reconcile(self, origins, sha, path):
        if not origins:
            return item_key("page", sha, path)
        keys = sorted({self.canonical(key) for key, _ in origins})
        for key in keys[1:]:
            self.aliases[key] = keys[0]
        return keys[0]

    def archive(self, path, item, attachments):
        mode, oid = item
        if mode == "160000" or (path, oid) in self.versions:
            return
        rel = "blobs/" + oid
        dest = self.out / rel
        if not dest.exists():
            with dest.open("wb") as fh:
                subprocess.run(["git", "-C", str(self.repo), "cat-file", "blob", oid], stdout=fh, check=True)
        mime = "text/plain" if mode == "120000" else MIME.get(Path(path).suffix.lower(), "application/octet-stream")
        attachments.append(dict(key=source_key(oid), path=path, file=rel, media_type=mime))
        self.versions.add((path, oid))

    def render(self, sha, path, tree, targets):
        oid = tree[path][1]
        raw = (self.out / "blobs" / oid).read_bytes()
        try:
            text = raw.decode("utf8")
        except UnicodeDecodeError:
            self.finding("non-utf8-original-bytes", sha, path, "", True)
            text = raw.decode("utf8", errors="replace")
        cv = Converter(self, sha, path, tree, targets)
        ext = Path(path).suffix.lower()
        if ext in ASCIIDOC:
            result = asciidoc.convert_asciidoc(
                text, lambda t, s, d=None: cv.link(markdown.escape_plain(s, False), t, display=d))
        elif ext in MARKDOWN:
            # Gollum accepts literal spaces in unquoted local Markdown hrefs.
            text = re.sub(r'\]\((/[^()\n]* [^()\n]*)\)',
                          lambda m: m[0] if re.search(r"""\s["']""", m[1])
                          else "](" + m[1].replace(" ", "%20") + ")", text)
            result = cv.convert(text)
        else:
            result = asciidoc._raw(text, block=True)
            self.finding("source-markup-shown-literally", sha, path, "", False)
        body = result.rstrip() + "\n\n" + link(source_key(oid), "Originalquelle") + "\n"
        return Rendering(body, cv.sites), cv.links

    def emit(self, rows, key, logical, path, text, predecessors, source=None):
        self.order += 1
        rel = f"pages/{self.order:06}.typ"
        if not isinstance(text, Rendering):
            text = Rendering(text, [])
        prefix = ""
        if not predecessors:
            # Native unanchored revisions derive identity from title/body/author.
            # Separate identical original roots (including re-created paths)
            # without adding visible text or relying on global traversal order.
            prefix = "// Gollum source root " + key + "\n"
        (self.out / rel).write_text(prefix + text.text(), encoding="utf8")
        revision = dict(key=key, page=logical, path=path, title=markdown.title_of(path),
                        content=rel, predecessors=list(dict.fromkeys(predecessors)))
        if source is not None:
            revision["source"] = source
        if text.sites:
            legacy = f"pages/{self.order:06}.legacy.typ"
            (self.out / legacy).write_text(prefix + text.text(legacy=True), encoding="utf8")
            revision["legacy_content"] = legacy
            revision["images"] = text.images(prefix)
            self.images += len(text.sites)
        rows.append(revision)

    def commit(self, sha, parents, states, author, date):
        tree = read_tree(self.repo, sha)
        moves = [renames(self.repo, par, sha) for par in parents]
        previous = [states[par] for par in parents]
        attachments, revisions, changes = [], [], []
        pages, pending, removed = {}, {}, {}
        for state in previous:
            removed.update(state.removed)
        for path, item in sorted(tree.items()):
            if not any(state.tree.get(path) == item for state in previous):
                self.archive(path, item, attachments)
                changes.append(dict(path=path, mode=item[0], blob=item[1]))
            if not page(path, item):
                continue
            origins = []
            unchanged = True
            for state, renamed in zip(previous, moves):
                old = renamed.get(path, path)
                if old in state.pages:
                    origins.append(state.pages[old])
                    unchanged &= old == path and state.tree[old] == item
            logical = self.reconcile(origins, sha, path)
            predecessors = [rev for _, rev in origins]
            for state in previous:
                for old, rev in state.removed.items():
                    if self.canonical(old) == logical:
                        predecessors.append(rev)
            predecessors = list(dict.fromkeys(predecessors))
            if unchanged and len(predecessors) == 1:
                pages[path] = (logical, predecessors[0])
            else:
                key = item_key("rev", sha, path)
                pages[path] = (logical, key)
                pending[path] = predecessors
            for old in list(removed):
                if self.canonical(old) == logical:
                    del removed[old]

        targets = {path: key for path, (_, key) in pages.items()}
        native = {path: self.render(sha, path, tree, targets) for path in pending}
        # Emit dependencies before their callers. Only a back-edge on the active
        # path is impossible to pin: a content-derived ID cannot cite itself.
        paths = {targets[path]: path for path in pending}
        dependencies = {path: sorted({paths[key] for key in refs if key in paths})
                        for path, (_, refs) in native.items()}
        emitted = set()
        for start in sorted(pending):
            if start in emitted:
                continue
            stack = [(start, iter(dependencies[start]))]
            active = {start}
            while stack:
                path, children = stack[-1]
                for child in children:
                    if child in active:
                        text, refs = native[path]
                        text = text.replace('"plan:' + targets[child] + '"',
                                            '"plan:' + source_key(tree[child][1]) + '"')
                        native[path] = text, refs
                        self.finding("cyclic-link-to-exact-original", sha, path, "", False)
                    elif child not in emitted:
                        active.add(child)
                        stack.append((child, iter(dependencies[child])))
                        break
                else:
                    logical, rev = pages[path]
                    self.emit(revisions, rev, logical, path, native[path][0], pending[path], source_key(tree[path][1]))
                    emitted.add(path)
                    active.remove(path)
                    stack.pop()

        live = {self.canonical(key) for key, _ in pages.values()}
        deletions = {}
        for parent, state, renamed in zip(parents, previous, moves):
            for old_path in sorted(state.tree.keys() - tree.keys()):
                changes.append(dict(parent=parent, removed=old_path))
            changes.extend(dict(parent=parent, renamed_from=old, path=new) for new, old in sorted(renamed.items()))
            for old_path, (old_key, old_rev) in state.pages.items():
                key = self.canonical(old_key)
                if key not in live:
                    dest = deletions.setdefault(key, (old_path, []))
                    dest[1].append(old_rev)
        for key, (path, links) in sorted(deletions.items()):
            rev = item_key("rev", sha, path) + "/deleted/" + hashlib.sha256(key.encode()).hexdigest()
            for state in previous:
                links.extend(value for old, value in state.removed.items() if self.canonical(old) == key)
            text = "#emph[Im Gollum-Repository wurde diese Seite mit Git-Commit " + sha + " gelöscht.]\n"
            self.emit(revisions, rev, key, path, text, links)
            removed[key] = rev
        self.commits.append(dict(commit=sha, author=author, date=date, attachments=attachments, revisions=revisions))
        self.changes.append(dict(commit=sha, parents=parents, changes=changes))
        return State(tree, pages, removed)

    def run(self):
        # Sort object ids rather than mutable ref labels for a reproducible topological order.
        refs = git(self.repo, "for-each-ref", "--format=%(objectname)").splitlines()
        tips = set()
        for ref in [b"HEAD", *refs]:
            result = subprocess.run(["git", "-C", str(self.repo), "rev-parse", "--verify", "--quiet",
                                     ref + b"^{commit}"], stdout=subprocess.PIPE)
            if result.returncode == 0:
                tips.add(result.stdout.strip().decode())
        if not tips:
            raise SystemExit("Gollum repository has no reachable commit")
        history = git(self.repo, "log", "--reverse", "--topo-order", "-z", "--format=%H%x00%P%x00%an <%ae>%x00%aI",
                      *sorted(tips)).split(b"\0")
        assert history.pop() == b""
        records = [(history[i].decode(), history[i+1].decode().split(), history[i+2].decode("utf8"),
                    history[i+3].decode()) for i in range(0, len(history), 4)]
        needed = Counter(p for _, parents, _, _ in records for p in parents)
        states = {}
        for sha, parents, author, date in records:
            states[sha] = self.commit(sha, parents, states, author, date)
            for parent in parents:
                needed[parent] -= 1
                if needed[parent] == 0:
                    del states[parent]
        for commit in self.commits:
            for rev in commit["revisions"]:
                rev["page"] = self.canonical(rev["page"])
        plan = dict(commits=self.commits)
        for name, obj in [("plan.json", plan), ("source-index.json", dict(tips=sorted(tips), commits=self.changes)),
                          ("render-findings.json", self.findings)]:
            with (self.out / name).open("w", encoding="utf8") as fh:
                json.dump(obj, fh, ensure_ascii=False, indent=1)
                fh.write("\n")
        losses = sum(f["rendering_loss"] for f in self.findings)
        print(f"{len(records)} original commits, {self.order} Wiki revisions, "
              f"{len(self.versions)} original path/blob versions, {self.images} embedded images, "
              f"{losses} rendering-loss findings")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--source-base", action="append", default=[], metavar="URL",
                        help="where the wiki was served (http(s)://host/ or /path/); repeatable")
    args = parser.parse_args()
    bases = [SourceBase(raw) for raw in args.source_base]
    os.umask(0o077)
    args.output.mkdir(mode=0o700, parents=True, exist_ok=True)
    if any(args.output.iterdir()):
        parser.error("output directory must be empty")
    (args.output / "pages").mkdir()
    (args.output / "blobs").mkdir()
    Adapter(args.repo, args.output, bases).run()
