#!/usr/bin/env python3
"""Translate local Git history to a native Wiki/Files import plan, without opening a pile.

All reachable commit trees are read as Git objects, without checking out files,
following symlinks, executing repository code, or expanding AsciiDoc includes.
The original bundle preserves commit/tree/tag objects, file modes and raw names.
Each original path/blob version also gets a Files row. Wiki handles the two
common Gollum formats natively; other Gollum markup uses an inert source view.
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
from urllib.parse import unquote

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


@dataclass
class State:
    tree: dict
    pages: dict  # path -> (logical page key, exact native predecessor key)
    removed: dict  # logical page key -> terminal native revision key


class Converter(markdown.Page):
    def __init__(self, owner, sha, path, tree, pages):
        super().__init__(owner, sha, path, pages, {p for p, v in tree.items() if not page(p, v) and v[0] != "160000"})
        self.tree = tree

    def link(self, label, target, image=False):
        original = target
        target = unquote(target.strip().strip("<>"))
        if re.match(r"^[A-Za-z][A-Za-z0-9+.-]*:", target):
            if re.match(r"(?i)^(https?|mailto):", target):
                return "#link(" + asciidoc._string(target) + ")[" + (label or markdown.escape_plain(target, False)) + "]"
            self.finding("non-web-uri-as-text", "", True)
            return self.literal_target(original, label)
        clean = target.split("#", 1)[0].split("?", 1)[0]
        if not clean:
            self.finding("local-anchor-retained-in-original", "", True)
            return self.literal_target(original, label)
        rel = posixpath.normpath(posixpath.join(self.dir, clean)).lstrip("/")
        direct = clean.lstrip("/")
        match = next((p for p in (rel, direct) if p in self.tree), None)
        if not match:
            match = self.page_target(direct) or self.file_target(direct)
        if not match or self.tree[match][0] == "160000":
            self.finding("unresolved-source-target", "", True)
            return self.literal_target(original, label)
        if "#" in target or "?" in target:
            self.finding("fragment-or-query-retained-in-original", "", True)
        caption = label or markdown.escape_plain(Path(match).name, False)
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
    def __init__(self, repo, out):
        self.repo = repo
        self.out = out
        self.commits = []
        self.changes = []
        self.findings = []
        self.aliases = {}
        self.versions = set()
        self.order = 0

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
        count = len(self.findings)
        try:
            text = raw.decode("utf8")
        except UnicodeDecodeError:
            self.finding("non-utf8-original-bytes", sha, path, "", True)
            text = raw.decode("utf8", errors="replace")
        cv = Converter(self, sha, path, tree, targets)
        ext = Path(path).suffix.lower()
        if ext in ASCIIDOC:
            result = asciidoc.convert_asciidoc(text, lambda t, s: cv.link(markdown.escape_plain(s, False), t))
        elif ext in MARKDOWN:
            # Gollum accepts literal spaces in unquoted local Markdown hrefs.
            text = re.sub(r'\]\((/[^()\n]* [^()\n]*)\)',
                          lambda m: m[0] if re.search(r"""\s["']""", m[1])
                          else "](" + m[1].replace(" ", "%20") + ")", text)
            result = cv.convert(text)
        else:
            result = asciidoc._raw(text, block=True)
            self.finding("source-markup-shown-literally", sha, path, "", False)
        if ext not in MARKDOWN or any(f["rendering_loss"] for f in self.findings[count:]):
            result = result.rstrip() + "\n\n" + link(source_key(oid), "Originalquelle") + "\n"
        return result, cv.links

    def emit(self, rows, key, logical, path, text, predecessors):
        self.order += 1
        rel = f"pages/{self.order:06}.typ"
        if not predecessors:
            # Native unanchored revisions derive identity from title/body/author.
            # Separate identical original roots (including re-created paths)
            # without adding visible text or relying on global traversal order.
            text = "// Gollum source root " + key + "\n" + text
        (self.out / rel).write_text(text, encoding="utf8")
        rows.append(dict(key=key, page=logical, path=path, title=markdown.title_of(path),
                         content=rel, predecessors=list(dict.fromkeys(predecessors))))

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
        remaining = set(pending)
        while remaining:
            busy = {targets[path]: path for path in remaining}
            ready = sorted(p for p in remaining if not set(native[p][1]).intersection(busy))
            path = (ready or sorted(remaining))[0]
            text, links = native[path]
            for dep in sorted(set(links).intersection(busy)):
                # Exact source links break cycles without inventing a target revision.
                text = text.replace('"plan:' + dep + '"', '"plan:' + source_key(tree[busy[dep]][1]) + '"')
                self.finding("cyclic-link-to-exact-original", sha, path, "", False)
            logical, rev = pages[path]
            self.emit(revisions, rev, logical, path, text, pending[path])
            remaining.remove(path)

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

    def run(self, bundle):
        # Sort object ids rather than mutable ref labels for a reproducible topological order.
        refs = git(self.repo, "for-each-ref", "--format=%(refname)").splitlines()
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
        if bundle:
            plan["bundle"] = bundle.lower()
        for name, obj in [("plan.json", plan), ("source-index.json", dict(tips=sorted(tips), commits=self.changes)),
                          ("render-findings.json", self.findings)]:
            with (self.out / name).open("w", encoding="utf8") as fh:
                json.dump(obj, fh, ensure_ascii=False, indent=1)
                fh.write("\n")
        losses = sum(f["rendering_loss"] for f in self.findings)
        print(f"{len(records)} original commits, {self.order} Wiki revisions, "
              f"{len(self.versions)} path/blob versions, {losses} rendering-loss findings "
              "(original bytes retained in Files)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--bundle-id")
    args = parser.parse_args()
    if args.bundle_id and not re.fullmatch(r"[A-Fa-f0-9]{32}", args.bundle_id):
        parser.error("--bundle-id must be a 32-hex Files entity id")
    os.umask(0o077)
    args.output.mkdir(mode=0o700, parents=True, exist_ok=True)
    if any(args.output.iterdir()):
        parser.error("output directory must be empty")
    (args.output / "pages").mkdir()
    (args.output / "blobs").mkdir()
    Adapter(args.repo, args.output).run(args.bundle_id)
