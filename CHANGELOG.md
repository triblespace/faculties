# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

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
