# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

## 0.13.1 — 2026-05-07

- **CI-only fix.** v0.13.0's release workflow died on every job
  with `wasm32-unknown-unknown target may not be installed`:
  triblespace 0.37 pulls `wasmi 0.31`, whose build script
  invokes rustc against `wasm32-unknown-unknown`. The workflow's
  rust-toolchain step only installed the per-target host
  triple. Fix:
  - add `wasm32-unknown-unknown` to the toolchain install,
  - swap `cross` for native arm64 Linux (GitHub now provides
    `ubuntu-24.04-arm` runners for free public repos), so the
    aarch64-linux job can install the wasm32 target via
    rustup like every other job.
  Lib source identical to v0.13.0; not republished to
  crates.io.

## 0.13.0 — 2026-05-07

- **Bump `triblespace` 0.36 → 0.37.** Aligns the CLI faculties
  + shared lib with the same triblespace release that GORBIE
  0.13 ships against — no more split between binaries on 0.36
  and the optional widgets stack pulling 0.37 transitively.
  Pre-1.0 minor bump, breaking for downstreams that pin
  `faculties = "0.12"`. (Bundles the v0.12.2 changes, which
  are not separately published.)

## 0.12.2 — 2026-05-07 (unpublished)

- **Bump optional `GORBIE` dep 0.12 → 0.13.** Picks up the
  GORBIE 0.13.x line: stacked floats no longer drag in
  lockstep, tall floats render at natural content height
  without a viewport-multiple cap, and the infinite-scroll
  feedback loop when a wiki/compass float was open is fixed.
  See GORBIE's CHANGELOG for the full notes.
- **Drop manual drag detection in `wiki` and `timeline`
  widgets.** Switch to egui's `Sense::click_and_drag` +
  z-aware `dragged()` / `drag_delta()`. Floats dragged
  across the wiki graph or the activity timeline no longer
  pan them in lockstep; the manual `primary_pressed && in_rect`
  + memory-id bookkeeping is gone.
- **Fix wiki graph label flip-overshoot.** When a node's
  label would overflow the right edge, the mirror-to-left
  path used `Align2::RIGHT_CENTER.anchor_rect` against an
  already-shifted origin — the label landed one whole
  galley-width further left than intended, sometimes
  clipping or appearing to wrap around to the viewport's
  left side. Pass the unshifted `left_anchor` so the
  label's right edge sits cleanly just left of the node.

## 0.12.1 — 2026-05-05

- **Drop stray `[patch.crates-io]` GORBIE local override.** v0.12.0
  shipped with `GORBIE = { path = "../GORBIE" }` in the manifest,
  which broke the release workflow (the GH runner has no sibling
  GORBIE checkout). Local dev overrides belong in
  `~/.cargo/config.toml` or a gitignored override file, not the
  published manifest. v0.12.0 source is identical otherwise; this
  is a CI-only fix.

## 0.12.0 — 2026-05-05

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
