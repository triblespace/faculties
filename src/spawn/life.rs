//! A resident zooid's next life: whether its last life calls for one here,
//! and the one launch that starts it.
//!
//! A resident's persona outlives each run, and a life ends before its
//! context fills. Nothing in a run's end starts the next one: [`next_life`]
//! reads whether one follows on this host, and [`Spawn::continue_life`]
//! makes that one launch. Deciding when to ask, and how many zooids a
//! colony runs, is not Spawn's: a rule (Avalanche) or a person asks, and
//! asking again is harmless.
//!
//! A next life follows only from a life that filled its context. Each of
//! these must hold, and the first that does not is the reason nothing
//! follows:
//!
//! - one declaration stands for the zooid and places it on this host
//!   ([`super::declaration::placed`]): a rest, an open or other host, two
//!   heads, or no declaration at all starts nothing;
//! - its newest run is resident, has ended, and realises that standing
//!   declaration, or realises none while the standing declaration follows
//!   none (a first life, lived before anything was declared). A life
//!   started under an earlier declaration does not carry over a rest, or a
//!   new placement after one: that is a new start;
//! - that run ended cleanly with its context past its life limit, as
//!   settlement observed it from the provider's own transcript. A clean
//!   exit is required beside the observation: a life stopped, killed or
//!   failed past its limit did not end because it filled;
//! - its conversation is in the archive (the launch checks this again);
//! - its unit does not run, and its bound model, if any, is one this host
//!   can honour.
//!
//! The launch admits it again under the zooid's lock: the newest run must
//! still be the one this life follows, and the declaration must still
//! stand here, so a repeated or concurrent ask starts at most one life per
//! predecessor, and a rest recorded in between starts none.
//!
//! [`next_life`]: Spawn::next_life

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use triblespace::prelude::Id;

use super::declaration::Standing;
use super::operations::{
    ended_clean, is_running, CoverWish, Launched, Provider, RunRequest, RunView, Spawn, ZooidWish,
};

/// A next life that follows here: the launch that starts it.
#[derive(Clone, Debug)]
pub struct Continuation {
    pub name: String,
    /// The life it follows.
    pub previous: Id,
    /// The declaration it makes true.
    pub declaration: Id,
    /// What the previous life's end showed, in words.
    pub evidence: String,
    pub request: RunRequest,
}

/// Whether a next life follows here.
#[derive(Clone, Debug)]
pub enum NextLife {
    Follows(Continuation),
    /// Nothing follows here, and why.
    Nothing { reason: String },
}

/// What [`Spawn::continue_life`] did.
#[derive(Clone, Debug)]
pub enum Continued {
    Started {
        continuation: Continuation,
        launched: Box<Launched>,
    },
    /// Nothing was started, and why.
    Nothing { reason: String },
}


fn nothing<T: Into<String>>(reason: T) -> Result<NextLife> {
    Ok(NextLife::Nothing {
        reason: reason.into(),
    })
}

/// Whether `run` ended at its life limit: a clean end past it, or why not.
fn filled(run: &RunView) -> std::result::Result<String, String> {
    let (occupancy, limit) = match (run.life_occupancy.first(), run.life_limit.first()) {
        (Some(occupancy), Some(limit)) => (*occupancy, *limit),
        _ => {
            return Err(format!(
                "its life {:x} ended with no life-limit observation: an ordinary end, a stop \
                 or a failure, or a provider whose transcript settlement does not measure",
                run.id
            ))
        }
    };
    if !ended_clean(run, false) {
        return Err(format!(
            "its life {:x} was past its limit ({occupancy} of {limit} tokens) but did not end \
             cleanly ({}): a stop or a failure, not a filled context",
            run.id,
            super::operations::describe_end(run)
        ));
    }
    Ok(format!(
        "its life {:x} ended cleanly with {occupancy} tokens of context, past its life limit of \
         {limit}",
        run.id
    ))
}

impl Spawn {
    /// Whether a next life of the resident zooid `name` follows on this
    /// host, read from its declarations and runs. Starts and writes nothing.
    pub fn next_life(&self, name: &str) -> Result<NextLife> {
        let host = self.local_host()?;
        let placed = self.placed(host)?;
        let view = self.show(name)?;
        let declared = placed
            .zooids
            .iter()
            .find(|declared| declared.zooid == view.id);
        let placement = match declared.map(|declared| &declared.standing) {
            Some(Standing::Here(placement)) => placement.clone(),
            Some(Standing::Nothing { reason }) => {
                return nothing(format!("{name} is not placed here: {reason}"))
            }
            None => {
                return nothing(format!(
                    "{name} declares nothing: seeing no declaration starts nothing"
                ))
            }
        };
        let Some(last) = view.runs.last() else {
            return nothing(format!(
                "{name} has not lived yet: a first life is a start (`zooid spawn --resident`), \
                 not a continuation"
            ));
        };
        if !last.resident {
            return nothing(format!("{name}'s newest run {:x} is not resident", last.id));
        }
        if !last.ended() {
            return nothing(format!(
                "{name}'s life {:x} has no recorded end: it runs, or its settlement is owed",
                last.id
            ));
        }
        // A life lived under the standing declaration continues it. One that
        // realised none (a first life, started before anything was declared)
        // continues only under a declaration that follows none: then no rest
        // or move can stand between them.
        let follows_none = declared.is_some_and(|declared| {
            declared
                .heads
                .iter()
                .filter(|head| head.id == placement.declaration)
                .all(|head| head.follows.is_empty())
        });
        let root = follows_none && last.realises.is_empty();
        if !last.realises.contains(&placement.declaration) && !root {
            return nothing(format!(
                "{name}'s life {:x} did not realise the standing declaration {:x}: a life under \
                 it is a new start, not a continuation of an older one",
                last.id, placement.declaration
            ));
        }
        let evidence = match filled(last) {
            Ok(evidence) => evidence,
            Err(why) => return nothing(format!("{name}: {why}")),
        };
        let provider = match last.provider.as_slice() {
            [one] => Provider::parse(one)?,
            other => {
                return nothing(format!(
                    "{name}'s life {:x} names {} providers: {}",
                    last.id,
                    other.len(),
                    other.join(", ")
                ))
            }
        };
        if let Some(spec) = placement.model_spec {
            if provider != Provider::Local {
                return nothing(format!(
                    "{name} binds model {spec:x}, which a {} life cannot honour",
                    provider.name()
                ));
            }
        }
        if matches!(provider, Provider::Agy | Provider::Claude) && last.archive_imported.is_empty()
        {
            return nothing(format!(
                "{name}'s life {:x} is not in the archive yet{}",
                last.id,
                last.archive_outcome
                    .last()
                    .map(|outcome| format!(" ({outcome})"))
                    .unwrap_or_default()
            ));
        }
        let one = |values: &[String], what: &str| -> std::result::Result<String, String> {
            match values {
                [one] => Ok(one.clone()),
                other => Err(format!(
                    "{name}'s life {:x} records {} {what}s",
                    last.id,
                    other.len()
                )),
            }
        };
        let (unit, workdir, effort) = match (
            one(&last.unit, "unit"),
            one(&last.workdir, "workdir"),
            one(&last.effort, "effort"),
        ) {
            (Ok(unit), Ok(workdir), Ok(effort)) => (unit, workdir, effort),
            (Err(why), _, _) | (_, Err(why), _) | (_, _, Err(why)) => return nothing(why),
        };
        match self.unit_state(&unit) {
            Ok(state) if is_running(&state) => {
                return nothing(format!("{name}'s unit {unit} is {state}"))
            }
            Ok(_) => {}
            Err(error) => {
                return nothing(format!(
                    "whether {name}'s unit {unit} runs is unknown ({error:#})"
                ))
            }
        }
        let spawner = match last.spawner.as_slice() {
            [one] => format!("{one:x}"),
            _ => return nothing(format!("{name}'s life {:x} names no one spawner", last.id)),
        };
        let brief = view
            .brief_text
            .clone()
            .ok_or_else(|| anyhow!("{name}'s brief is not here"))?;
        Ok(NextLife::Follows(Continuation {
            name: view.name.clone(),
            previous: last.id,
            declaration: placement.declaration,
            evidence,
            request: RunRequest {
                brief,
                workdir: PathBuf::from(workdir),
                effort,
                spawner,
                provider,
                cover: CoverWish::default(),
                zooid: ZooidWish {
                    name: Some(view.name.clone()),
                    resident: true,
                    realises: Some(placement.declaration),
                    follows: Some(last.id),
                },
            },
        }))
    }

    /// Start the next life of `name` when one follows here
    /// ([`Spawn::next_life`]); the launch admits it again under the zooid's
    /// lock. Asking again, or twice at once, starts at most one life after
    /// each life: the second finds it followed, or its unit running.
    pub fn continue_life(&self, name: &str) -> Result<Continued> {
        match self.next_life(name)? {
            NextLife::Nothing { reason } => Ok(Continued::Nothing { reason }),
            NextLife::Follows(continuation) => {
                let launched = self.run(&continuation.request)?;
                Ok(Continued::Started {
                    continuation,
                    launched: Box::new(launched),
                })
            }
        }
    }
}
