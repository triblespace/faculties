# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

- **Faculties are real Cargo binaries now.** Every faculty moved
  from a `rust-script` shebang at the repo root into `src/bin/`,
  with the unioned dep set hoisted into `Cargo.toml`. Install
  with `cargo install --git ... --bins` (or grab a precompiled
  tarball from a tagged release). Invocation drops the `.rs`
  suffix: `wiki list`, `compass add ...`, etc. The `faculties`
  lib (schemas + widgets) is unchanged; binaries `use faculties::...`
  the same way external crates would.
- **GitHub Actions release workflow.** `v*` tags trigger per-target
  builds (`x86_64-linux-gnu`, `aarch64-linux-gnu` via cross,
  `x86_64-apple-darwin`, `aarch64-apple-darwin`) and attach
  tarballs + sha256s to the GH release. Restricted sandboxes can
  fetch binaries without a Rust toolchain.

## 0.11.2 — 2026-04-19

- **Theme-adaptive compass + messages.** `color_frame`, `card_bg`,
  `color_bubble`, `color_muted` now branch on `ui.visuals().dark_mode`
  so light-mode notebooks don't end up with dark-on-dark text.
- **Drags don't fight.** Both the wiki graph and the activity
  timeline only latch onto a drag whose press *started* inside
  their viewport — dragging a floating card across them no longer
  yanks the graph pan or the timeline offset.
- **Release hygiene.** LICENSE-MIT + LICENSE-APACHE committed,
  Cargo.toml gains authors/homepage/readme/keywords/categories,
  `.gitignore` excludes `*.pile` and `.bak-*` backups.

## 0.11.1 — 2026-04-19

- **`faculties-viewer` binary.** `cargo install faculties --features widgets`
  now installs a binary that composes all four widgets (activity
  timeline, wiki graph, compass kanban, local-messages) against a
  single pile. Mirror of `examples/pile_inspector.rs`.
- **Widgets polish.** Dozens of small rendering fixes for the demo:
  edge-to-edge viewports for the timeline and wiki graph; SPAN +
  zoom-hint overlay inside each viewport; colorhash-tinted fragment
  IDs, person chips, and tag chips; compass lanes now stack
  vertically; centered empty-state placeholders; search-miss banner.
- **GORBIE 0.12.** Bumps the optional GORBIE dep to 0.12.0 which
  pulls in the egui 0.34 hit-test workarounds.
- **CLI faculties on the published crate.** All rust-script
  faculties (`compass.rs`, `wiki.rs`, etc.) depend on
  `faculties = "0.11"` from crates.io instead of an absolute local
  path, so cloners build out of the box.

## 0.11.0

Previous releases were internal/path-based. See git history.
