//! Embodiment in a Clawblox-protocol world, as plain text.
//!
//! The world speaks HTTP and JSON (join, observe, input). This faculty turns
//! what the body perceives into short text and turns verbs into inputs. Text
//! is the only shape every Drive mind admits: an inadmissible sensed record
//! ends a Drive session, so nothing here ever emits JSON as perception.
//!
//! `world watch` is the sense. With `DRIVE_ENDPOINT` set its text reaches
//! Drive's organ as records from the sense `world`; she starts it and kills it
//! like any other faculty, so the stream is never something Drive spawns.
//! Every other verb is an action whose printed result is the observation.

pub mod cli;
pub mod mcp;
pub mod operations;
pub mod render;
