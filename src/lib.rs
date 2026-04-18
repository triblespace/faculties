//! Shared infrastructure for triblespace-backed faculties.
//!
//! The individual rust-script faculties at the root of this repo (e.g.
//! `compass.rs`, `wiki.rs`, `local_messages.rs`) all store data in
//! triblespace piles using attribute IDs defined here. Centralizing the
//! schemas means every consumer — the rust-script itself, other faculties
//! that cross-reference, the playground dashboard, and any GORBIE notebook
//! that embeds a faculty widget — uses the same attribute IDs.

pub mod schemas;
