//! What both v3 importers (Claude Code, Codex) and the relations share for
//! reading one source record: the JSON reader ([`json`]), the table-driven
//! record check ([`check`]), the refusal ([`Failure`]) and the timestamp
//! reading ([`time`]); and the table-driven R1 look-back ([`look_back`]),
//! which decides whether a record's marked rendering is built. Neither
//! importer owns them.

use std::fmt;
use std::str::FromStr;

use hifitime::Epoch;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

pub(crate) mod check;
pub mod json;
pub(crate) mod look_back;

/// Why a unit was refused: a normalized source path and a fixed reason.
/// Neither ever holds a source value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub path: String,
    pub reason: &'static str,
}

impl Failure {
    pub(crate) fn new(path: &str, reason: &'static str) -> Self {
        Failure {
            path: path.to_owned(),
            reason,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

impl std::error::Error for Failure {}

pub(crate) fn time(text: &str) -> Option<Inline<NsTAIInterval>> {
    let epoch = Epoch::from_str(text.trim()).ok()?;
    (epoch, epoch).try_to_inline().ok()
}
