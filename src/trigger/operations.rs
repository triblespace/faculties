//! Native Trigger operations. A run intent is durable before the check starts;
//! its result is a separate durable observation, never a delivery receipt.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{Collection, CollectionSnapshotExt, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;

use super::model::{self, Bytes, Text};
use super::runner::{self, Context, Outcome, Status};
use crate::clock;
use crate::collection_names::{open_configured, require_command_write_admission};
use crate::habits::{materialize_script, Script};
use crate::schemas::trigger::{attrs, DEFAULT_SCOPE_ID, KIND_RESULT, KIND_RUN, KIND_TRIGGER};
use crate::storage::{FactArchive, Storage};

#[derive(Clone, Debug)]
pub struct Triggers {
    storage: Storage,
}

/// A short-lived projection returned to a caller, not a retained catalogue.
#[derive(Clone, Debug)]
pub struct Definition {
    pub id: Id,
    pub label: String,
    pub context: Context,
    pub command: std::result::Result<String, String>,
    pub interval_seconds: Option<u64>,
    pub event: Option<String>,
    pub current: bool,
    pub active: bool,
}

#[derive(Clone, Copy)]
pub struct ExecutionOptions<'a> {
    pub directory: &'a Path,
    pub stdin: &'a [u8],
    pub timeout: Duration,
    pub output_limit: usize,
}

/// Operation-local input captured from the selected snapshot. Filesystem
/// materialization happens after the corresponding intent has been flushed.
enum PreparedCommand {
    Literal(String),
    Carried { script: Script, suffix: String },
}

#[derive(Clone, Debug)]
pub struct Execution {
    pub trigger: Id,
    pub run: Id,
    pub result: Id,
    pub outcome: Outcome,
    /// A synchronous caller verdict is separate from whether the check
    /// executed successfully, including when replaying persisted evidence.
    pub verdict: Option<i32>,
}

#[derive(Clone, Debug)]
pub enum Report {
    /// No result understood by this reader exists in its selected observation.
    /// The check may still be running, or may have been interrupted. This is
    /// unknown/incomplete evidence, not success and not an invented failure.
    Pending {
        trigger: Id,
        run: Id,
        context: Context,
    },
    Result {
        trigger: Id,
        run: Id,
        result: Id,
        outcome: Outcome,
        verdict: Option<i32>,
    },
    Unavailable {
        trigger: Id,
        run: Id,
        result: Id,
        reason: String,
    },
}

impl Triggers {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    /// Publish a complete caller-authored definition/state/provenance unit.
    /// This stores the fragment but never runs a check.
    pub fn publish(&self, fragment: Fragment) -> Result<Id> {
        let id = fragment
            .root()
            .ok_or_else(|| anyhow!("Trigger fragment has no exported root"))?;
        self.with_facts(false, |pile, signer, collection, _, _| {
            publish_flushed(pile, signer, collection, fragment)?;
            Ok(id)
        })
    }

    /// Passive inspection: no shell invocation, scheduling change or receipt.
    /// Missing command bytes remain visible as an error on the projection.
    pub fn list(&self, selected: Option<Id>) -> Result<Vec<Definition>> {
        self.with_facts(false, |_, _, _, snapshot, facts| {
            let mut definitions = Vec::new();
            // Operation-local output projections. An exact-ID inspection
            // constrains the query before touching any command payload.
            let projections: Vec<_> = match selected {
                Some(id) => find!(
                    (label: String, command: Text, context: String),
                    pattern!(facts, [{ id @
                        metadata::tag: &KIND_TRIGGER,
                        attrs::label: ?label,
                        attrs::command: ?command,
                        attrs::context: ?context,
                    }])
                )
                .map(|(label, command, context)| (id, label, command, context))
                .collect(),
                None => find!(
                (id: Id, label: String, command: Text, context: String),
                pattern!(facts, [{ ?id @
                    metadata::tag: &KIND_TRIGGER,
                    attrs::label: ?label,
                    attrs::command: ?command,
                    attrs::context: ?context,
                }])
                )
                .collect(),
            };
            for (id, label, command, context) in projections {
                let Some(context) = parse_context(&context) else {
                    continue;
                };
                let mut intervals: Vec<_> = find!(seconds: u64,
                    pattern!(facts, [{ id @ attrs::interval_seconds: ?seconds }])
                )
                .map(Some)
                .collect();
                let mut events: Vec<_> = find!(event: String,
                    pattern!(facts, [{ id @ attrs::event: ?event }])
                )
                .map(Some)
                .collect();
                if intervals.is_empty() {
                    intervals.push(None);
                }
                if events.is_empty() {
                    events.push(None);
                }
                for interval_seconds in &intervals {
                    for event in &events {
                        definitions.push(Definition {
                            id,
                            label: label.clone(),
                            context,
                            command: read_command(snapshot, command),
                            interval_seconds: *interval_seconds,
                            event: event.clone(),
                            current: model::current(facts, id),
                            active: model::active(facts, id),
                        });
                    }
                }
            }
            Ok(definitions)
        })
    }

    /// Record one attempted timer slot. The persistent owner serializes these
    /// calls; this is not a distributed exactly-once lock. Refresh before the
    /// duplicate query so a durable intent cannot disappear behind our own
    /// lagging derived view after a restart.
    pub fn begin_timer(
        &self,
        definition: Id,
        command: Text,
        script: Option<Bytes>,
        persona: Id,
        scheduled: Epoch,
    ) -> Result<Option<Id>> {
        self.with_facts(true, |pile, signer, collection, _, facts| {
            let executor = signer.verifying_key();
            if model::attempted(
                facts, definition, command, script, executor, persona, scheduled,
            ) {
                return Ok(None);
            }
            let mut fragment =
                model::timer_run(definition, command, script, executor, persona, scheduled)?;
            let id = fragment.root().expect("timer run has one root");
            // Actual attempt time annotates the already established slot
            // identity; retries must never get a fresh identity from a clock.
            fragment += entity! { ExclusiveId::force_ref(&id) @
                metadata::started_at: clock::point_now()?,
            };
            publish_flushed(pile, signer, collection, fragment)?;
            Ok(Some(id))
        })
    }

    /// Select due checks from one frozen observation. Each selected command is
    /// prepared from that snapshot, then its intent is flushed before launching
    /// a child outside the storage lock. A result-write error returns an error;
    /// neither this operation nor a later timer sweep repeats that same slot.
    pub fn run_due(
        &self,
        persona: Id,
        now: Epoch,
        options: ExecutionOptions<'_>,
    ) -> Result<Vec<Execution>> {
        let selected = self.with_facts(true, |_, _, _, snapshot, facts| {
            Ok(model::scheduled(facts, persona, now)
                .into_iter()
                .flat_map(|(timer, scheduled)| {
                    prepare_commands(snapshot, facts, timer.id, timer.command)
                        .into_iter()
                        .map(move |(script, command)| {
                            (timer.id, timer.command, script, scheduled, command)
                        })
                })
                .collect::<Vec<_>>())
        })?;
        let mut executions = Vec::new();
        for (definition, handle, script, scheduled, command) in selected {
            let Some(run) = self.begin_timer(definition, handle, script, persona, scheduled)?
            else {
                continue;
            };
            let outcome = execute_prepared(command, Context::Timer, options);
            let result = self.record_result(run, &outcome, None).with_context(|| {
                format!("check ran as {run:x}; persisting its result failed; do not rerun it")
            })?;
            executions.push(Execution {
                trigger: definition,
                run,
                result,
                outcome,
                verdict: None,
            });
        }
        Ok(executions)
    }

    /// Execute each understood command/event projection. A supplied occurrence
    /// is the caller's opaque delivery identity; retries reuse completed
    /// evidence, while an unfinished intent stays unknown and is not rerun.
    /// Without an occurrence, this call denotes a fresh event.
    pub fn run_event(
        &self,
        definition: Id,
        persona: Id,
        context: Context,
        event_filter: Option<&str>,
        occurrence: Option<Id>,
        options: ExecutionOptions<'_>,
    ) -> Result<Vec<Execution>> {
        if context == Context::Timer {
            bail!("run_event needs advisory-event or synchronous-event context");
        }
        let occurrence = occurrence.unwrap_or_else(|| *triblespace::core::id::genid());
        let selected = self.with_facts(true, |_, _, _, snapshot, facts| {
            if !model::current(facts, definition) || !model::active(facts, definition)
                || !model::addressed_to(facts, definition, persona) {
                bail!("event Trigger {definition:x} is not live, active and addressed to this persona");
            }
            let commands: Vec<(Text, String)> = if let Some(event) = event_filter {
                find!(command: Text, pattern!(facts, [{ definition @
                    metadata::tag: &KIND_TRIGGER,
                    attrs::command: ?command,
                    attrs::context: context.as_str(),
                    attrs::event: event,
                }])).map(|command| (command, event.to_owned())).collect()
            } else {
                find!((command: Text, event: String), pattern!(facts, [{ definition @
                    metadata::tag: &KIND_TRIGGER,
                    attrs::command: ?command,
                    attrs::context: context.as_str(),
                    attrs::event: ?event,
                }])).collect()
            };
            let projections = commands.into_iter().flat_map(|(handle, event)| {
                prepare_commands(snapshot, facts, definition, handle).into_iter()
                    .map(move |(script, command)| (handle, event.clone(), script, command))
            })
                .collect::<Vec<_>>();
            if projections.is_empty() {
                bail!("no event check projection for Trigger {definition:x} in {} context", context.as_str());
            }
            Ok(projections)
        })?;
        let mut executions = Vec::new();
        for (handle, event, script, command) in selected {
            let (runs, created) = self.with_facts(true, |pile, signer, collection, _, facts| {
                let runs: Vec<_> = find!(run: Id, pattern!(facts, [{ ?run @
                    metadata::tag: &KIND_RUN,
                    attrs::trigger_of: definition,
                    attrs::command: handle,
                    attrs::executor: signer.verifying_key(),
                    attrs::persona: persona,
                    attrs::context: context.as_str(),
                    attrs::event: event.as_str(),
                    attrs::occurrence: occurrence,
                }]))
                .filter(|run| match script {
                    Some(script) => exists!(pattern!(facts, [{ run @ attrs::script: script }])),
                    None => !exists!(pattern!(facts, [{ run @ attrs::script: _?script }])),
                })
                .collect();
                if !runs.is_empty() {
                    return Ok((runs, false));
                }
                let mut fragment = entity! {
                    metadata::tag: KIND_RUN,
                    attrs::trigger_of: definition,
                    attrs::command: handle,
                    attrs::script?: script,
                    attrs::executor: signer.verifying_key(),
                    attrs::persona: persona,
                    attrs::context: context.as_str(),
                    attrs::event: event.as_str(),
                    attrs::occurrence: occurrence,
                };
                let run = fragment.root().expect("event intent has one root");
                fragment += entity! { ExclusiveId::force_ref(&run) @
                    metadata::started_at: clock::point_now()?,
                };
                publish_flushed(pile, signer, collection, fragment)?;
                Ok((vec![run], true))
            })?;
            if created {
                let run = runs[0];
                let outcome = execute_prepared(command, context, options);
                let verdict = outcome.synchronous_verdict();
                let result = self
                    .record_result(run, &outcome, verdict)
                    .with_context(|| {
                        format!(
                            "check ran as {run:x}; persisting its result failed; do not rerun it"
                        )
                    })?;
                executions.push(Execution {
                    trigger: definition,
                    run,
                    result,
                    outcome,
                    verdict,
                });
            } else {
                for run in runs {
                    let completed = self.with_facts(false, |_, signer, _, snapshot, facts| {
                        let mut completed = Vec::new();
                        for (result, status, detail, stdout, stderr) in find!(
                            (result: Id, status: String, detail: Text, stdout: Bytes, stderr: Bytes),
                            pattern!(facts, [
                                { run @
                                    metadata::tag: &KIND_RUN,
                                    attrs::trigger_of: definition,
                                    attrs::executor: signer.verifying_key(),
                                    attrs::persona: persona,
                                    attrs::context: context.as_str(),
                                },
                                { ?result @
                                    metadata::tag: &KIND_RESULT,
                                    attrs::run_of: run,
                                    attrs::context: context.as_str(),
                                    attrs::status: ?status,
                                    attrs::detail: ?detail,
                                    attrs::stdout: ?stdout,
                                    attrs::stderr: ?stderr,
                                },
                            ])
                        ) {
                            let Some(outcome) = read_outcome(snapshot, context, &status, detail, stdout, stderr)? else {
                                continue;
                            };
                            let verdicts: Vec<_> = find!(verdict: i32,
                                pattern!(facts, [{ result @ attrs::verdict: ?verdict }])
                            ).collect();
                            if verdicts.is_empty() {
                                completed.push(Execution { trigger: definition, run, result, outcome, verdict: None });
                            } else {
                                for verdict in verdicts {
                                    completed.push(Execution {
                                        trigger: definition, run, result,
                                        outcome: outcome.clone(), verdict: Some(verdict),
                                    });
                                }
                            }
                        }
                        if completed.is_empty() {
                            bail!("event {occurrence:x} already has intent {run:x} but no readable result; state is unknown, not rerunning");
                        }
                        Ok(completed)
                    })?;
                    executions.extend(completed);
                }
            }
        }
        Ok(executions)
    }

    /// Also usable by native checks whose successful execution yields a
    /// separate denying verdict. It never interprets that verdict as failure.
    pub fn record_result(&self, run: Id, outcome: &Outcome, verdict: Option<i32>) -> Result<Id> {
        let fragment = model::result_fragment(run, outcome, verdict, clock::now()?)?;
        let result = fragment.root().expect("result has one root");
        self.with_facts(true, |pile, signer, collection, _, facts| {
            if !exists!(pattern!(facts, [{ run @
                metadata::tag: &KIND_RUN,
                attrs::executor: signer.verifying_key(),
                attrs::context: outcome.context.as_str(),
            }])) {
                bail!("no matching local Trigger run {run:x} for this result");
            }
            publish_flushed(pile, signer, collection, fragment)?;
            Ok(result)
        })
    }

    /// Query local-host, exact-persona execution evidence, independent of all
    /// completion and presentation receipts. Quiet results remain inspectable;
    /// notification callers use Outcome::presentation to omit them.
    pub fn reports(&self, persona: Id) -> Result<Vec<Report>> {
        self.with_facts(false, |_, signer, _, snapshot, facts| {
            let executor = signer.verifying_key();
            let mut reports = Vec::new();
            for (run, trigger, context) in find!(
                (run: Id, trigger: Id, context: String),
                pattern!(facts, [{ ?run @
                    metadata::tag: &KIND_RUN,
                    attrs::trigger_of: ?trigger,
                    attrs::executor: executor,
                    attrs::persona: &persona,
                    attrs::context: ?context,
                }])
            ) {
                let Some(context) = parse_context(&context) else {
                    continue;
                };
                let mut understood = false;
                for (result, status, detail, stdout, stderr) in find!(
                    (result: Id, status: String, detail: Text, stdout: Bytes, stderr: Bytes),
                    pattern!(facts, [{ ?result @
                        metadata::tag: &KIND_RESULT,
                        attrs::run_of: run,
                        attrs::context: context.as_str(),
                        attrs::status: ?status,
                        attrs::detail: ?detail,
                        attrs::stdout: ?stdout,
                        attrs::stderr: ?stderr,
                    }])
                ) {
                    let read = read_outcome(snapshot, context, &status, detail, stdout, stderr);
                    match read {
                        Ok(None) => continue,
                        Ok(Some(outcome)) => {
                            understood = true;
                            let verdicts: Vec<i32> = find!(
                                verdict: i32,
                                pattern!(facts, [{ result @ attrs::verdict: ?verdict }])
                            )
                            .collect();
                            if verdicts.is_empty() {
                                reports.push(Report::Result {
                                    trigger,
                                    run,
                                    result,
                                    outcome,
                                    verdict: None,
                                });
                            } else {
                                for verdict in verdicts {
                                    reports.push(Report::Result {
                                        trigger,
                                        run,
                                        result,
                                        outcome: outcome.clone(),
                                        verdict: Some(verdict),
                                    });
                                }
                            }
                        }
                        Err(error) => {
                            understood = true;
                            reports.push(Report::Unavailable {
                                trigger,
                                run,
                                result,
                                reason: format!("{error:#}"),
                            });
                        }
                    }
                }
                if !understood {
                    reports.push(Report::Pending {
                        trigger,
                        run,
                        context,
                    });
                }
            }
            Ok(reports)
        })
    }

    /// Storage plumbing only: callers perform their own typed queries over
    /// this one frozen Rank9 view, rather than constructing a shadow catalogue.
    pub(super) fn with_facts<T>(
        &self,
        refresh: bool,
        operation: impl FnOnce(
            &mut Pile,
            &SigningKey,
            Collection<SimpleArchive>,
            &PileSnapshot,
            &FactArchive,
        ) -> Result<T>,
    ) -> Result<T> {
        self.storage.with_pile(|pile, signer| {
            let collection = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
            let policy = collection.policy(&pile.snapshot()?)?;
            let succinct = pile.derive::<SuccinctArchiveBlob>(collection, (), policy.clone())?;
            let rank9 = pile.derive::<Rank9AcceleratedSuccinctArchiveBlob>(succinct, (), policy)?;
            if refresh {
                require_command_write_admission(
                    pile,
                    collection,
                    signer,
                    "Trigger",
                    "trigger list",
                )?;
                pollster::block_on(async {
                    drop(pile.ensure(collection, signer).await?);
                    drop(pile.maintain(succinct, signer).await?);
                    drop(pile.maintain(rank9, signer).await?);
                    Ok::<_, anyhow::Error>(())
                })?;
            }
            let snapshot = pile.snapshot().context("freeze Trigger observation")?;
            let facts = snapshot.collection(rank9)?.view::<FactArchive>()?;
            operation(pile, signer, collection, &snapshot, &facts)
        })
    }
}

fn publish_flushed(
    pile: &mut Pile,
    signer: &SigningKey,
    collection: Collection<SimpleArchive>,
    fragment: Fragment,
) -> Result<()> {
    require_command_write_admission(pile, collection, signer, "Trigger", "trigger list")?;
    pile.commit(collection, signer, fragment)
        .context("publish Trigger evidence")?;
    // In particular this barrier precedes every child invocation, even when
    // Storage is shared and its eventual close would otherwise defer flushing.
    pile.flush().context("flush Trigger evidence")?;
    drop(pollster::block_on(crate::storage::ensure_downstream(
        pile, collection, signer,
    ))?);
    pile.flush().context("flush Trigger derived visibility")?;
    Ok(())
}

fn read_command(snapshot: &PileSnapshot, handle: Text) -> std::result::Result<String, String> {
    let text: anybytes::View<str> = snapshot.get(handle).map_err(|error| {
        format!(
            "check command payload {} is unavailable: {error}",
            hex::encode(handle.raw)
        )
    })?;
    Ok(text.to_string())
}

fn read_outcome(
    snapshot: &PileSnapshot,
    context: Context,
    status: &str,
    detail: Text,
    stdout: Bytes,
    stderr: Bytes,
) -> Result<Option<Outcome>> {
    // Unknown result types are outside this reader's projection, not broken
    // known results. Do not even fetch their payloads: a future type may use
    // these handles differently or may not have arrived on this host yet.
    if !matches!(
        status,
        "success" | "exit" | "signal" | "timeout" | "output-limit" | "spawn-failed" | "io-failed"
    ) {
        return Ok(None);
    }
    let detail: anybytes::View<str> = snapshot
        .get(detail)
        .context("read persisted check status detail")?;
    let Some(status) = parse_status(status, &detail) else {
        return Ok(None);
    };
    let stdout: anybytes::Bytes = snapshot
        .get(stdout)
        .context("read persisted check stdout")?;
    let stderr: anybytes::Bytes = snapshot
        .get(stderr)
        .context("read persisted check stderr")?;
    Ok(Some(Outcome {
        context,
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
        status,
    }))
}

fn prepare_commands(
    snapshot: &PileSnapshot,
    facts: &FactArchive,
    definition: Id,
    command: Text,
) -> Vec<(Option<Bytes>, std::result::Result<PreparedCommand, String>)> {
    let command = match read_command(snapshot, command) {
        Ok(command) => command,
        Err(error) => return vec![(None, Err(error))],
    };
    let Some(suffix) = command
        .trim_start()
        .strip_prefix("@script")
        .filter(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
    else {
        // Attachments are not part of an ordinary literal command. In
        // particular, an unrelated missing blob must not prevent it running.
        return vec![(None, Ok(PreparedCommand::Literal(command)))];
    };
    let mut prepared = Vec::new();
    for handle in find!(script: Bytes, pattern!(facts, [{ definition @ attrs::script: ?script }])) {
        let script: std::result::Result<anybytes::Bytes, _> = snapshot.get(handle);
        let command = script
            .map(|bytes| PreparedCommand::Carried {
                script: Script {
                    handle,
                    bytes: bytes.to_vec(),
                },
                suffix: suffix.to_owned(),
            })
            .map_err(|error| {
                format!(
                    "check script payload {} is unavailable: {error}; no shell was launched",
                    hex::encode(handle.raw),
                )
            });
        prepared.push((Some(handle), command));
    }
    if prepared.is_empty() {
        prepared.push((
            None,
            Err(
                "check names @script but no carried script is present; no shell was launched"
                    .into(),
            ),
        ));
    }
    prepared
}

fn execute_prepared(
    command: std::result::Result<PreparedCommand, String>,
    context: Context,
    options: ExecutionOptions<'_>,
) -> Outcome {
    let command = command.and_then(|command| match command {
        PreparedCommand::Literal(command) => Ok(command),
        PreparedCommand::Carried { script, suffix } => {
            let path = materialize_script(&script)?;
            let path = path.to_str().ok_or_else(|| {
                "script cache path is not UTF-8; no shell was launched".to_owned()
            })?;
            // Only the leading command word is replaced. Arguments, shell
            // syntax and embedded @script strings stay exactly as authored.
            Ok(format!("'{}'{suffix}", path.replace('\'', r"'\''")))
        }
    });
    match command {
        Ok(command) => runner::execute(runner::Invocation {
            command: &command,
            directory: options.directory,
            stdin: options.stdin,
            context,
            timeout: options.timeout,
            output_limit: options.output_limit,
        }),
        Err(error) => Outcome {
            context,
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: Status::IoFailed(error),
        },
    }
}

fn parse_context(value: &str) -> Option<Context> {
    match value {
        "timer" => Some(Context::Timer),
        "advisory-event" => Some(Context::AdvisoryEvent),
        "synchronous-event" => Some(Context::SynchronousEvent),
        _ => None,
    }
}

fn parse_status(status: &str, detail: &str) -> Option<Status> {
    Some(match status {
        "success" => Status::Success,
        "exit" => Status::Exit(detail.parse().ok()?),
        "signal" => Status::Signal(detail.parse().ok()?),
        "timeout" => Status::TimedOut,
        "output-limit" => Status::OutputLimit,
        "spawn-failed" => Status::SpawnFailed(detail.to_owned()),
        "io-failed" => Status::IoFailed(detail.to_owned()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        directory: tempfile::TempDir,
        pile: PathBuf,
        key: PathBuf,
        persona: Id,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let pile = directory.path().join("trigger.pile");
            let key = directory.path().join("trigger.key");
            std::fs::File::create(&pile).unwrap();
            crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
            Self {
                directory,
                pile,
                key,
                persona: *triblespace::core::id::genid(),
            }
        }
        fn triggers(&self) -> Triggers {
            Triggers::new(self.pile.clone(), Some(self.key.clone()))
        }
        fn options(&self) -> ExecutionOptions<'_> {
            ExecutionOptions {
                directory: self.directory.path(),
                stdin: b"",
                timeout: Duration::from_secs(3),
                output_limit: 8192,
            }
        }
        fn timer(&self, command: &str) -> Id {
            self.triggers()
                .publish(
                    model::timer_definition("fixture", command, 60, time(0.), &[], &[]).unwrap(),
                )
                .unwrap()
        }
        fn command(&self, id: Id) -> Text {
            self.triggers()
                .with_facts(false, |_, _, _, _, facts| {
                    Ok(
                        find!(command: Text, pattern!(facts, [{ id @ attrs::command: ?command }]))
                            .next()
                            .unwrap(),
                    )
                })
                .unwrap()
        }
        fn event_intent(&self, id: Id, occurrence: Id) -> Id {
            let run = triblespace::core::id::genid();
            let command = self.command(id);
            self.triggers()
                .with_facts(false, |pile, signer, collection, _, _| {
                    publish_flushed(
                        pile,
                        signer,
                        collection,
                        entity! { &run @
                            metadata::tag: KIND_RUN,
                            attrs::trigger_of: id,
                            attrs::command: command,
                            attrs::executor: signer.verifying_key(),
                            attrs::persona: self.persona,
                            attrs::context: "synchronous-event",
                            attrs::event: "pre-push",
                            attrs::occurrence: occurrence,
                        },
                    )
                })
                .unwrap();
            *run
        }
    }

    fn time(seconds: f64) -> Epoch {
        Epoch::from_unix_seconds(seconds)
    }

    // The legacy content-addressed materializer selects its cache from the
    // process environment. Isolate that environment in a child test process;
    // never race another test by changing this process's cache or use the
    // operator's real cache. The apostrophe exercises shell path quoting.
    fn isolated_script_cache(test: &str) -> bool {
        if std::env::var("FACULTIES_TRIGGER_SCRIPT_TEST").as_deref() == Ok(test) {
            return false;
        }
        let cache = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("FACULTIES_TRIGGER_SCRIPT_TEST", test)
            .env(
                "FACULTIES_HABIT_SCRIPT_CACHE",
                cache.path().join("script ' cache"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child script test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        true
    }

    #[test]
    fn carried_scripts_are_resident_qualified_and_replay_uses_persisted_results() {
        if isolated_script_cache("trigger::operations::tests::carried_scripts_are_resident_qualified_and_replay_uses_persisted_results") {
            return;
        }
        let fixture = Fixture::new();
        for context in [Context::Timer, Context::SynchronousEvent] {
            let mut definition = if context == Context::Timer {
                model::timer_definition("carried", "@script 'hello world'", 60, time(0.), &[], &[])
                    .unwrap()
            } else {
                model::event_definition(
                    "carried",
                    "@script 'hello world'",
                    "pre-push",
                    context,
                    &[],
                    &[],
                )
                .unwrap()
            };
            let id = definition.root().unwrap();
            let first: Bytes =
                definition.put(b"#!/bin/sh\nprintf '%s' \"$1\"\nprintf a >> calls\n".to_vec());
            let second: Bytes = definition
                .put(b"#!/bin/sh\nprintf '%s' \"$1\"\nprintf b >> calls\nexit 1\n".to_vec());
            definition += entity! { ExclusiveId::force_ref(&id) @ attrs::script*: [first, second] };
            fixture.triggers().publish(definition).unwrap();
            let occurrence = *triblespace::core::id::genid();
            let run = || {
                if context == Context::Timer {
                    fixture
                        .triggers()
                        .run_due(fixture.persona, time(10.), fixture.options())
                } else {
                    fixture.triggers().run_event(
                        id,
                        fixture.persona,
                        context,
                        Some("pre-push"),
                        Some(occurrence),
                        fixture.options(),
                    )
                }
            };
            let executions = run().unwrap();
            assert_eq!(executions.len(), 2);
            assert_ne!(executions[0].run, executions[1].run);
            assert!(executions
                .iter()
                .all(|execution| execution.outcome.stdout == b"hello world"));
            assert_eq!(
                executions
                    .iter()
                    .filter(|execution| execution.outcome.status == Status::Exit(1))
                    .count(),
                1
            );
            for execution in &executions {
                let run = execution.run;
                fixture.triggers().with_facts(false, |_, _, _, _, facts| {
                    let attached: Vec<_> = find!(script: Bytes, pattern!(facts, [{ run @ attrs::script: ?script }])).collect();
                    assert_eq!(attached.len(), 1);
                    assert!(attached[0] == first || attached[0] == second);
                    Ok(())
                }).unwrap();
            }
            let calls = std::fs::read(fixture.directory.path().join("calls")).unwrap();
            let replay = run().unwrap();
            if context == Context::Timer {
                assert!(replay.is_empty());
            } else {
                assert_eq!(replay.len(), 2);
                assert_eq!(
                    replay
                        .iter()
                        .filter(|execution| execution.verdict == Some(1))
                        .count(),
                    1
                );
            }
            assert_eq!(
                std::fs::read(fixture.directory.path().join("calls")).unwrap(),
                calls
            );
        }
    }

    #[test]
    fn missing_carried_payload_records_failure_and_its_handle() {
        let fixture = Fixture::new();
        let mut definition =
            model::timer_definition("missing-script", "@script", 60, time(0.), &[], &[]).unwrap();
        let id = definition.root().unwrap();
        let missing = Bytes::new([67; 32]);
        definition += entity! { ExclusiveId::force_ref(&id) @ attrs::script: missing };
        fixture.triggers().publish(definition).unwrap();
        let executions = fixture
            .triggers()
            .run_due(fixture.persona, time(1.), fixture.options())
            .unwrap();
        assert_eq!(executions.len(), 1);
        assert!(matches!(&executions[0].outcome.status,
            Status::IoFailed(error) if error.contains("script payload") && error.contains("unavailable")));
        let run = executions[0].run;
        fixture
            .triggers()
            .with_facts(false, |_, _, _, _, facts| {
                assert!(exists!(pattern!(facts, [{ run @ attrs::script: missing }])));
                Ok(())
            })
            .unwrap();
        assert!(fixture
            .triggers()
            .reports(fixture.persona)
            .unwrap()
            .iter()
            .any(|report| {
                matches!(report, Report::Result { run: found, outcome, .. }
                if *found == run && matches!(outcome.status, Status::IoFailed(_)))
            }));
        assert!(fixture
            .triggers()
            .run_due(fixture.persona, time(10.), fixture.options())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn literal_command_does_not_read_or_execute_unrelated_scripts() {
        let fixture = Fixture::new();
        let mut definition = model::timer_definition(
            "literal",
            "printf '%s' '@script @scripture'",
            60,
            time(0.),
            &[],
            &[],
        )
        .unwrap();
        let id = definition.root().unwrap();
        definition += entity! { ExclusiveId::force_ref(&id) @ attrs::script*: [Bytes::new([91; 32]), Bytes::new([92; 32])] };
        fixture.triggers().publish(definition).unwrap();
        let executions = fixture
            .triggers()
            .run_due(fixture.persona, time(1.), fixture.options())
            .unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].outcome.status, Status::Success);
        assert_eq!(executions[0].outcome.stdout, b"@script @scripture");
        let run = executions[0].run;
        fixture
            .triggers()
            .with_facts(false, |_, _, _, _, facts| {
                assert!(!exists!(
                    pattern!(facts, [{ run @ attrs::script: _?script }])
                ));
                Ok(())
            })
            .unwrap();
        assert!(fixture
            .triggers()
            .run_due(fixture.persona, time(10.), fixture.options())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn passive_list_does_not_execute_and_an_intent_survives_reopen_without_rerun() {
        let fixture = Fixture::new();
        let id = fixture.timer("printf ran >> calls");
        assert_eq!(fixture.triggers().list(None).unwrap().len(), 1);
        assert!(!fixture.directory.path().join("calls").exists());
        let handle = fixture.command(id);
        let run = fixture
            .triggers()
            .begin_timer(id, handle, None, fixture.persona, time(0.))
            .unwrap()
            .unwrap();
        fixture
            .triggers()
            .with_facts(false, |_, signer, _, _, facts| {
                assert!(exists!(
                    pattern!(facts, [{ run @ metadata::started_at: _?started }])
                ));
                assert_eq!(
                    model::timer_run(
                        id,
                        handle,
                        None,
                        signer.verifying_key(),
                        fixture.persona,
                        time(0.)
                    )
                    .unwrap()
                    .root(),
                    Some(run)
                );
                Ok(())
            })
            .unwrap();
        let reopened = fixture.triggers();
        assert!(reopened
            .run_due(fixture.persona, time(30.), fixture.options())
            .unwrap()
            .is_empty());
        assert!(!fixture.directory.path().join("calls").exists());
        assert!(reopened
            .reports(fixture.persona)
            .unwrap()
            .iter()
            .any(|report| {
                matches!(report, Report::Pending { run: found, .. } if *found == run)
            }));
    }

    #[test]
    fn result_survives_reopen_and_failure_advances_without_receipt_or_done() {
        let fixture = Fixture::new();
        fixture.timer("printf x >> calls; printf problem >&2; exit 1");
        let first = fixture
            .triggers()
            .run_due(fixture.persona, time(10.), fixture.options())
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].outcome.status, Status::Exit(1));
        assert!(fixture
            .triggers()
            .run_due(fixture.persona, time(30.), fixture.options())
            .unwrap()
            .is_empty());
        let reports = fixture.triggers().reports(fixture.persona).unwrap();
        assert!(reports.iter().any(|report| matches!(report,
            Report::Result { result, outcome, .. }
                if *result == first[0].result && outcome.stderr == b"problem"
                    && outcome.status == Status::Exit(1)
        )));
        assert_eq!(
            fixture
                .triggers()
                .run_due(fixture.persona, time(61.), fixture.options())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            std::fs::read(fixture.directory.path().join("calls")).unwrap(),
            b"xx"
        );
        assert!(fixture
            .triggers()
            .reports(*triblespace::core::id::genid())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn successful_quiet_results_are_persisted_and_inspectable() {
        let fixture = Fixture::new();
        fixture.timer("printf diagnostic >&2");
        let executions = fixture
            .triggers()
            .run_due(fixture.persona, time(1.), fixture.options())
            .unwrap();
        assert_eq!(
            executions[0].outcome.presentation(),
            runner::Presentation::Quiet
        );
        assert!(
            matches!(&fixture.triggers().reports(fixture.persona).unwrap()[0],
                Report::Result { outcome, .. } if outcome.stderr == b"diagnostic"
                    && outcome.status == Status::Success
            )
        );
    }

    #[test]
    fn unavailable_command_is_persisted_as_failed_check_after_intent() {
        let fixture = Fixture::new();
        let id = triblespace::core::id::genid();
        let missing = Text::new([23; 32]);
        fixture
            .triggers()
            .publish(entity! { &id @
                metadata::tag: KIND_TRIGGER,
                attrs::label: "missing",
                attrs::command: missing,
                attrs::context: "timer",
                attrs::interval_seconds: 60u64,
                attrs::scheduled_at: clock::point(time(0.)).unwrap(),
            })
            .unwrap();
        let runs = fixture
            .triggers()
            .run_due(fixture.persona, time(1.), fixture.options())
            .unwrap();
        assert!(
            matches!(&runs[0].outcome.status, Status::IoFailed(detail) if detail.contains("unavailable"))
        );
        assert!(!fixture
            .triggers()
            .reports(fixture.persona)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn carried_placeholder_is_explicit_failure_and_never_a_shell_command() {
        let fixture = Fixture::new();
        fixture.timer("@script --check");
        let runs = fixture
            .triggers()
            .run_due(fixture.persona, time(1.), fixture.options())
            .unwrap();
        assert!(
            matches!(&runs[0].outcome.status, Status::IoFailed(detail) if detail.contains("no shell was launched"))
        );
    }

    #[test]
    fn synchronous_event_has_a_persisted_verdict_while_advisory_does_not() {
        let fixture = Fixture::new();
        for context in [Context::SynchronousEvent, Context::AdvisoryEvent] {
            let id = fixture
                .triggers()
                .publish(
                    model::event_definition("event", "exit 7", "pre-push", context, &[], &[])
                        .unwrap(),
                )
                .unwrap();
            let executions = fixture
                .triggers()
                .run_event(id, fixture.persona, context, None, None, fixture.options())
                .unwrap();
            let execution = &executions[0];
            assert_eq!(execution.outcome.status, Status::Exit(7));
            let expected = (context == Context::SynchronousEvent).then_some(7);
            assert!(fixture
                .triggers()
                .reports(fixture.persona)
                .unwrap()
                .iter()
                .any(|report| {
                    matches!(report, Report::Result { result, verdict, .. }
                    if *result == execution.result && *verdict == expected)
                }));
        }
    }

    #[test]
    fn event_redelivery_reuses_the_result_without_running_again() {
        let fixture = Fixture::new();
        let id = fixture
            .triggers()
            .publish(
                model::event_definition(
                    "event",
                    "printf x >> calls; exit 7",
                    "pre-push",
                    Context::SynchronousEvent,
                    &[],
                    &[],
                )
                .unwrap(),
            )
            .unwrap();
        let occurrence = *triblespace::core::id::genid();
        let first = fixture
            .triggers()
            .run_event(
                id,
                fixture.persona,
                Context::SynchronousEvent,
                None,
                Some(occurrence),
                fixture.options(),
            )
            .unwrap();
        let again = fixture
            .triggers()
            .run_event(
                id,
                fixture.persona,
                Context::SynchronousEvent,
                None,
                Some(occurrence),
                fixture.options(),
            )
            .unwrap();
        assert_eq!(first[0].run, again[0].run);
        assert_eq!(first[0].result, again[0].result);
        assert_eq!(again[0].outcome.synchronous_verdict(), Some(7));
        assert_eq!(again[0].verdict, Some(7));
        assert_eq!(
            std::fs::read(fixture.directory.path().join("calls")).unwrap(),
            b"x"
        );
        let run = first[0].run;
        fixture
            .triggers()
            .with_facts(false, |_, _, _, _, facts| {
                assert!(exists!(
                    pattern!(facts, [{ run @ metadata::started_at: _?started }])
                ));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn opaque_imported_event_intent_stays_unknown_without_reexecuting() {
        let fixture = Fixture::new();
        let id = fixture
            .triggers()
            .publish(
                model::event_definition(
                    "event",
                    "printf unexpected >> calls",
                    "pre-push",
                    Context::SynchronousEvent,
                    &[],
                    &[],
                )
                .unwrap(),
            )
            .unwrap();
        let occurrence = *triblespace::core::id::genid();
        let run = fixture.event_intent(id, occurrence);
        let error = fixture
            .triggers()
            .run_event(
                id,
                fixture.persona,
                Context::SynchronousEvent,
                None,
                Some(occurrence),
                fixture.options(),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("state is unknown, not rerunning"));
        assert!(!fixture.directory.path().join("calls").exists());
        assert!(fixture
            .triggers()
            .reports(fixture.persona)
            .unwrap()
            .iter()
            .any(|report| {
                matches!(report, Report::Pending { run: found, .. } if *found == run)
            }));
    }

    #[test]
    fn linked_partial_result_does_not_hide_incomplete_run() {
        let fixture = Fixture::new();
        let id = fixture.timer("printf unexpected >> calls");
        let run = fixture
            .triggers()
            .begin_timer(id, fixture.command(id), None, fixture.persona, time(0.))
            .unwrap()
            .unwrap();
        fixture
            .triggers()
            .publish(entity! {
                metadata::tag: KIND_RESULT,
                attrs::run_of: run,
                attrs::context: "timer",
            })
            .unwrap();
        assert!(fixture
            .triggers()
            .reports(fixture.persona)
            .unwrap()
            .iter()
            .any(|report| {
                matches!(report, Report::Pending { run: found, .. } if *found == run)
            }));
        assert!(fixture
            .triggers()
            .run_due(fixture.persona, time(10.), fixture.options())
            .unwrap()
            .is_empty());
        assert!(!fixture.directory.path().join("calls").exists());
    }

    #[test]
    fn replay_preserves_native_denial_separately_from_successful_execution() {
        let fixture = Fixture::new();
        let id = fixture
            .triggers()
            .publish(
                model::event_definition(
                    "event",
                    "printf unexpected >> calls",
                    "pre-push",
                    Context::SynchronousEvent,
                    &[],
                    &[],
                )
                .unwrap(),
            )
            .unwrap();
        let occurrence = *triblespace::core::id::genid();
        let run = fixture.event_intent(id, occurrence);
        let outcome = Outcome {
            context: Context::SynchronousEvent,
            status: Status::Success,
            stdout: b"denied by native policy".to_vec(),
            stderr: Vec::new(),
        };
        fixture
            .triggers()
            .record_result(run, &outcome, Some(17))
            .unwrap();
        let replay = fixture
            .triggers()
            .run_event(
                id,
                fixture.persona,
                Context::SynchronousEvent,
                None,
                Some(occurrence),
                fixture.options(),
            )
            .unwrap();
        assert_eq!(replay[0].outcome.status, Status::Success);
        assert_eq!(replay[0].verdict, Some(17));
        assert!(!fixture.directory.path().join("calls").exists());
    }

    #[test]
    fn future_result_types_do_not_hide_readable_completed_occurrence() {
        let fixture = Fixture::new();
        let id = fixture
            .triggers()
            .publish(
                model::event_definition(
                    "future-compatible",
                    "printf x >> calls; printf known; exit 7",
                    "pre-push",
                    Context::SynchronousEvent,
                    &[],
                    &[],
                )
                .unwrap(),
            )
            .unwrap();
        let occurrence = *triblespace::core::id::genid();
        let first = fixture
            .triggers()
            .run_event(
                id,
                fixture.persona,
                Context::SynchronousEvent,
                None,
                Some(occurrence),
                fixture.options(),
            )
            .unwrap();
        let run = first[0].run;
        let result = first[0].result;
        fixture
            .triggers()
            .publish(entity! { ExclusiveId::force_ref(&result) @
                attrs::status: "future-status",
            })
            .unwrap();
        // A separate future projection deliberately has unavailable bodies.
        // None of those bodies is required to understand the known result.
        fixture
            .triggers()
            .publish(entity! {
                metadata::tag: KIND_RESULT,
                attrs::run_of: run,
                attrs::context: "synchronous-event",
                attrs::status: "future-status",
                attrs::detail: Text::new([71; 32]),
                attrs::stdout: Bytes::new([72; 32]),
                attrs::stderr: Bytes::new([73; 32]),
                attrs::verdict: 0i64,
            })
            .unwrap();
        let replay = fixture
            .triggers()
            .run_event(
                id,
                fixture.persona,
                Context::SynchronousEvent,
                None,
                Some(occurrence),
                fixture.options(),
            )
            .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].result, result);
        assert_eq!(replay[0].outcome.status, Status::Exit(7));
        assert_eq!(replay[0].verdict, Some(7));
        assert_eq!(replay[0].outcome.stdout, b"known");
        assert_eq!(
            std::fs::read(fixture.directory.path().join("calls")).unwrap(),
            b"x"
        );
        let reports = fixture.triggers().reports(fixture.persona).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(matches!(&reports[0], Report::Result { result: found, .. } if *found == result));
    }

    #[test]
    fn unknown_only_result_remains_pending_and_never_reexecutes() {
        let fixture = Fixture::new();
        let id = fixture
            .triggers()
            .publish(
                model::event_definition(
                    "future-only",
                    "printf unexpected >> calls",
                    "pre-push",
                    Context::SynchronousEvent,
                    &[],
                    &[],
                )
                .unwrap(),
            )
            .unwrap();
        let occurrence = *triblespace::core::id::genid();
        let run = fixture.event_intent(id, occurrence);
        fixture
            .triggers()
            .publish(entity! {
                metadata::tag: KIND_RESULT,
                attrs::run_of: run,
                attrs::context: "synchronous-event",
                attrs::status: "future-status",
                attrs::detail: Text::new([74; 32]),
                attrs::stdout: Bytes::new([75; 32]),
                attrs::stderr: Bytes::new([76; 32]),
                attrs::verdict: 0i64,
            })
            .unwrap();
        let error = fixture
            .triggers()
            .run_event(
                id,
                fixture.persona,
                Context::SynchronousEvent,
                None,
                Some(occurrence),
                fixture.options(),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("state is unknown, not rerunning"));
        assert!(!fixture.directory.path().join("calls").exists());
        let reports = fixture.triggers().reports(fixture.persona).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(matches!(reports[0], Report::Pending { run: found, .. } if found == run));
    }

    #[test]
    fn known_result_with_missing_body_is_unavailable_not_unknown_or_success() {
        let fixture = Fixture::new();
        let id = fixture
            .triggers()
            .publish(
                model::event_definition(
                    "missing-output",
                    "printf unexpected >> calls",
                    "pre-push",
                    Context::SynchronousEvent,
                    &[],
                    &[],
                )
                .unwrap(),
            )
            .unwrap();
        let occurrence = *triblespace::core::id::genid();
        let run = fixture.event_intent(id, occurrence);
        fixture
            .triggers()
            .publish(entity! {
                metadata::tag: KIND_RESULT,
                attrs::run_of: run,
                attrs::context: "synchronous-event",
                attrs::status: "success",
                attrs::detail: String::new(),
                attrs::stdout: Bytes::new([77; 32]),
                attrs::stderr: Vec::<u8>::new(),
                attrs::verdict: 0i64,
            })
            .unwrap();
        let error = fixture
            .triggers()
            .run_event(
                id,
                fixture.persona,
                Context::SynchronousEvent,
                None,
                Some(occurrence),
                fixture.options(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("read persisted check stdout"));
        assert!(!fixture.directory.path().join("calls").exists());
        let reports = fixture.triggers().reports(fixture.persona).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(
            matches!(&reports[0], Report::Unavailable { run: found, reason, .. }
            if *found == run && reason.contains("read persisted check stdout"))
        );
    }
}
