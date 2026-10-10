# Native Code Git seam tests

This small test crate compiles the **production** `src/code/git.rs` directly,
without linking the rest of the Faculty cohort. Fixtures use libgit2 to create
temporary repositories; they do not read a user's repositories, pile or keys.

Use the normal host build reservation and a disposable `CARGO_TARGET_DIR`:

```sh
cargo test --manifest-path tests/code_git_native/Cargo.toml --lib
cargo test --manifest-path tests/code_git_native/Cargo.toml --lib -- --ignored
```

The ignored test is an explicit differential control against the previous Git
commands, operating only on its own fixture. The ordinary tests need **no Git
executable**. After compilation, run the test binary directly with an empty
`PATH` and isolated `HOME`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL=/dev/null`,
`GIT_CONFIG_NOSYSTEM=1` (unset inherited `GIT_*` overrides and live Faculty
environment settings). Leave the differential test ignored for that run.

The native seam supports HEAD/revision resolution, index paths and tracked
status, recursive blob trees, ordered one-object-at-a-time reads, and literal
per-file `-S` history. Default history excludes merge diffs but visits both
parents. Root commits compare against an empty tree. Rename detection honors
`diff.renames` and `diff.renameLimit`. Paths retain literal whitespace rather
than Git's display quoting. Invalid UTF-8 paths remain lossy at the existing
public `String` boundary, as in the previous adapter.

This is not a claim that libgit2 emulates every Git configuration. Its inexact
rename/copy similarity heuristic can differ on ambiguous pairs. The explicit
control retains one observed example: binary `needle\0aaaa` at `old.rs` becoming
text `needle` at `new.rs` is paired by libgit2, but treated as delete/add by Git.
The resulting path list and pickaxe hit membership differ. This is tested as
a known boundary, not hidden by suppressing binary changes.

History uses libgit2's topological/time ordering. Clock-skewed merge graphs and
timestamp ties can differ from Git's default traversal. External textconv, filters,
credential helpers and diff programs are not executed: pickaxe searches the
stored bytes. Git object formats unsupported by the linked libgit2 return an
error, with no subprocess fallback. The enabled library features are local
vendored libgit2 only, not SSH/HTTPS transports.

The root Faculty lockfile must also resolve this dependency when integrating
into its complete sibling source cohort. This crate's lockfile freezes the
small independently testable dependency graph; it does not validate all
Faculty frontend linking or the macOS app graph.
