//! Running one rung of the ladder.
//!
//! The ladder ([`super::ladder`]) says which rungs are due; a [`RungRunner`]
//! runs one and says how it ended. Operations record the step afterwards,
//! whatever the outcome, so one dead channel never stalls the escalation.

use triblespace::prelude::*;

use super::ladder::{Capabilities, Due};
use super::Outcome;

/// What a rung says about one alarm, read once from its raise.
#[derive(Clone, Debug, PartialEq)]
pub struct Subject {
    pub alarm: Id,
    pub raiser: Id,
    pub raiser_label: String,
    /// The class's CLI name (data-loss, ...).
    pub class: String,
    /// The host it was raised on.
    pub host: String,
    pub what: String,
    pub decays: String,
    pub evidence: String,
    pub test: bool,
    /// TAI nanoseconds.
    pub raised_at: i128,
}

impl Subject {
    /// The short id people type and the Discord line carries.
    pub fn id8(&self) -> String {
        format!("{:x}", self.alarm)[..8].to_owned()
    }
}

/// How one rung ended, and what it left that a later reader joins on.
#[derive(Clone, Debug, PartialEq)]
pub struct RungResult {
    pub outcome: Outcome,
    /// The snowflake of the Discord message the rung posted.
    pub discord_message_id: Option<String>,
    /// The say request the rung published.
    pub say_request: Option<Id>,
}

impl RungResult {
    pub fn delivered() -> Self {
        Self {
            outcome: Outcome::Delivered,
            discord_message_id: None,
            say_request: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Failed(error.into()),
            discord_message_id: None,
            say_request: None,
        }
    }
}

/// The executor of the ladder's rungs on one host.
pub trait RungRunner {
    /// What this host cannot run, and whether JP is in the voice channel.
    fn capabilities(&mut self, subject: &Subject) -> Capabilities;
    /// Run one rung. Never panics on a dead channel: a failure is an outcome.
    fn run(&mut self, subject: &Subject, due: Due) -> RungResult;
}
