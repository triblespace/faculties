//! Home: Kim's lights through the Hue bridge's local API.
//!
//! Every read asks the bridge; light state is never copied into the pile,
//! because the bridge (and the wall switch) owns it. The `home` collection
//! holds only the bridge binding (which bridge, where, and the exact Secrets
//! version of its username) and one action record per state change Home sent.
//! Rooms and zones are named by their Hue name, ignoring case, whitespace runs
//! and typographic apostrophes (the Hue app writes `Kim’s`), with the
//! group id as a fallback.

mod bridge;
pub mod cli;
pub mod mcp;
mod operations;
pub mod presentation;

pub use bridge::{
    bri_to_percent, percent_to_bri, BridgeConfig, Discovered, Group, Light, Scene, MAX_BRI,
};
pub use operations::{
    action_fragment, binding_fragment, binding_status, resolve_place, resolve_scene, same_name,
    ActionReport, Binding, BindingStatus, Home, LightsView, Request, RoomsView, ScenesView,
};
