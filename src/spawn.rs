//! Task zooids started from a brief, each in its own systemd user unit.
//!
//! One input fixes the whole zooid: the brief. Its blob handle names the
//! spawn (the first 32 bits of the handle as two proquints, after the
//! colony's label when the spawner has one colony), it tints the memory
//! cover the zooid wakes into, and it is the prompt. An identical brief
//! relaunches the same zooid under the name it was first given; a changed
//! brief is a new one.
//! `zooid goal` renders the brief from a Compass goal's current revision, so
//! a new note on the goal makes a new zooid.
//!
//! This module holds the pure parts: naming, the fragments a launch and a
//! settlement publish, the opening context, and the unit script. Everything
//! that touches the pile, Relations, Message or systemd is in [`operations`].

pub mod cli;
pub mod colony;
pub mod continuation;
pub mod cover;
pub mod drive_task;
pub mod mcp;
pub mod operations;

pub use colony::{
    colony_view, render_colony, ColonyOptions, ColonyRole, ColonyRoles, ColonyView, Evidence,
    ParentSource, ParentView,
};
pub use operations::{
    choose_provider, inbox_failure, keep_lock_path, CoverWish, GoalRequest, IgniteDecision,
    Ignition, InboxAnswer, InboxHook, InboxRequest, KeepConfig, KeepPass, KeepStep, LaunchConfig,
    Launched, Launcher, LibraryRenderer, Provider, Renderer, RequestInput, RequestView, Requested,
    RunRequest, RunView, SettleRequest, Settled, Spawn, SpawnView, SystemdLauncher, UnitSpec,
    ZooidRef,
};

use anyhow::{bail, Result};
use triblespace::core::metadata;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Handle, NsTAIInterval};
use triblespace::prelude::*;

use crate::schemas::archive::archive as archive_attrs;
use crate::schemas::blockdag;
use crate::schemas::imagine::imagine as imagine_attrs;
use crate::schemas::spawn::{spawn as attrs, KIND_OUTCOME, KIND_REQUEST, KIND_RUN, KIND_SPAWN};

pub type TextHandle = Inline<Handle<UTF8String>>;
pub type IntervalValue = Inline<NsTAIInterval>;
/// A host, named by its node key as custody and the network name peers.
pub type HostKey = Inline<inlineencodings::ED25519PublicKey>;

/// Prefix of the unit a zooid runs in, before its name.
pub const UNIT_PREFIX: &str = "spawn-";
/// Effort levels `claude --effort` accepts.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// Bytes of topic text that tint the memory cover. WeMM embeds a query of at
/// most 256 token ids, framing and its terminal embedding token included, and
/// refuses a longer one rather than truncating it. Its tokenizer is byte-level
/// BPE, so no byte sequence costs more than one id per byte: 240 bytes fit
/// whatever the text is, with 16 ids left for the framing.
pub const ABOUT_BYTES: usize = 240;

const CONSONANTS: &[u8; 16] = b"bdfghjklmnprstvz";
const VOWELS: &[u8; 4] = b"aiou";

/// One 16-bit word as a proquint syllable group: consonant, vowel,
/// consonant, vowel, consonant, taking 4, 2, 4, 2 and 4 bits from the top
/// (Wilkerson, "A Proposal for Proquints", 2009).
pub fn proquint(word: u16) -> String {
    let c = |shift: u16| CONSONANTS[usize::from((word >> shift) & 0xF)] as char;
    let v = |shift: u16| VOWELS[usize::from((word >> shift) & 0x3)] as char;
    [c(12), v(10), c(6), v(4), c(0)].into_iter().collect()
}

/// Big-endian 16-bit words as proquints joined by `-`.
pub fn proquints(bytes: &[u8]) -> String {
    bytes
        .chunks(2)
        .map(|pair| proquint(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)])))
        .collect::<Vec<_>>()
        .join("-")
}

/// The zooid's name for a brief handle: the first 32 bits of the hash.
pub fn name_for(brief: TextHandle) -> String {
    proquints(&brief.raw[..4])
}

/// The unit a zooid runs in.
pub fn unit_for(name: &str) -> String {
    format!("{UNIT_PREFIX}{name}")
}

/// The unit a request's run runs in, derived from the request alone, so
/// systemd itself refuses a second start of one request while it runs.
pub fn unit_for_request(request: Id) -> String {
    format!("{UNIT_PREFIX}req-{request:x}")
}

/// A host key as 64 hex digits, the way `TRIBLESPACE_PEERS` writes it.
pub fn host_text(host: HostKey) -> String {
    hex::encode(host.raw)
}

/// A host key from 64 hex digits.
pub fn parse_host(text: &str) -> Result<HostKey> {
    let bytes = hex::decode(text.trim())
        .map_err(|_| anyhow::anyhow!("host {text:?} is not a hex node key"))?;
    let raw: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("host {text:?} is not a 32-byte node key"))?;
    Ok(Inline::new(raw))
}

/// `blake3:<hex>` for a handle, as other faculties print one.
pub fn handle_text(handle: TextHandle) -> String {
    format!("blake3:{}", hex::encode(handle.raw))
}

/// The handle a text has, with no store involved.
pub fn text_handle(text: &str) -> TextHandle {
    Fragment::empty().put::<UTF8String, _>(text.to_owned())
}

/// A zooid's name: the colony's label, when it has one, before the
/// proquints of its brief (`aurora-kuvud-dajiz`). The name is chosen once,
/// when the zooid is first named, and stored; nothing recomputes it.
pub fn prefixed_name(prefix: Option<&str>, brief: TextHandle) -> String {
    match prefix {
        Some(prefix) => format!("{prefix}-{}", name_for(brief)),
        None => name_for(brief),
    }
}

/// A spawn's intrinsic core: the brief alone. Returns the fragment, the
/// spawn id and the brief handle.
pub fn spawn_core(brief: &str) -> Result<(Fragment, Id, TextHandle)> {
    if brief.trim().is_empty() {
        bail!("the brief is empty");
    }
    let mut fragment = Fragment::empty();
    let handle = fragment.put::<UTF8String, _>(brief.to_owned());
    let core = entity! { _ @
        metadata::tag: KIND_SPAWN,
        attrs::brief: handle,
    };
    let id = core.root().expect("a spawn has one intrinsic root");
    fragment += core;
    Ok((fragment, id, handle))
}

/// A spawn's name, annotating it, and the colony it was taken from.
pub fn name_fragment(spawn: Id, name: &str, origin_colony: Option<Id>) -> Fragment {
    let mut fragment = Fragment::empty();
    let name_handle = fragment.put::<UTF8String, _>(name.to_owned());
    fragment += entity! { ExclusiveId::force_ref(&spawn) @
        attrs::name: name_handle,
        attrs::origin_colony?: origin_colony,
    };
    fragment
}

/// A spawn with its unprefixed name: [`spawn_core`] and [`name_fragment`].
/// Returns the fragment, the spawn id, the brief handle and the name.
pub fn spawn_fragment(brief: &str) -> Result<(Fragment, Id, TextHandle, String)> {
    let (mut fragment, id, handle) = spawn_core(brief)?;
    let name = name_for(handle);
    fragment += name_fragment(id, &name, None);
    Ok((fragment, id, handle, name))
}

/// The Compass goal revision a brief was rendered from, annotating the spawn.
pub fn goal_fragment(spawn: Id, goal: Id, revision: &[Id]) -> Fragment {
    let revision = revision.to_vec();
    entity! { ExclusiveId::force_ref(&spawn) @
        attrs::goal: goal,
        attrs::goal_revision*: revision,
    }
}

/// What one launch knows when it starts.
#[derive(Clone, Debug)]
pub struct RunFacts {
    pub spawn: Id,
    pub started_at: IntervalValue,
    pub persona: Id,
    pub spawner: Id,
    pub unit: String,
    pub workdir: String,
    pub effort: String,
    pub prompt: String,
    pub provider: String,
    pub provider_reason: String,
    /// The model the agent was told to use; `None` when none was configured.
    pub model: Option<String>,
    /// Why no model was named, when `model` is `None`.
    pub model_unset: Option<String>,
    /// Why the recollection is untinted, when its topic query failed.
    pub untinted: Option<String>,
    /// The request this run serves, when a keeper started it.
    pub request: Option<Id>,
    /// The archived node a local run continues its zooid's history from.
    pub resumes: Option<Id>,
    /// How the cover was sized, when the launch asked for a share of the
    /// context window (`--cover`).
    pub cover: Option<cover::CoverRecord>,
}

/// A run: the spawn and start time as its intrinsic core, the rest as
/// annotations. Returns the fragment and the run id.
pub fn run_fragment(run: &RunFacts) -> (Fragment, Id) {
    let core = entity! { _ @
        metadata::tag: KIND_RUN,
        attrs::of: run.spawn,
        metadata::started_at: run.started_at,
    };
    let id = core.root().expect("a run has one intrinsic root");
    let mut fragment = Fragment::empty();
    fragment += core;
    let unit = fragment.put::<UTF8String, _>(run.unit.clone());
    let workdir = fragment.put::<UTF8String, _>(run.workdir.clone());
    let effort = fragment.put::<UTF8String, _>(run.effort.clone());
    let prompt = fragment.put::<UTF8String, _>(run.prompt.clone());
    let provider = fragment.put::<UTF8String, _>(run.provider.clone());
    let provider_reason = fragment.put::<UTF8String, _>(run.provider_reason.clone());
    let untinted = run
        .untinted
        .as_ref()
        .map(|text| fragment.put::<UTF8String, _>(text.clone()));
    let model = run
        .model
        .as_ref()
        .map(|text| fragment.put::<UTF8String, _>(text.clone()));
    let model_unset = run
        .model_unset
        .as_ref()
        .map(|text| fragment.put::<UTF8String, _>(text.clone()));
    fragment += entity! { ExclusiveId::force_ref(&id) @
        attrs::persona: run.persona,
        archive_attrs::author: run.spawner,
        attrs::unit: unit,
        attrs::workdir: workdir,
        attrs::effort: effort,
        attrs::prompt: prompt,
        attrs::provider: provider,
        attrs::provider_reason: provider_reason,
        attrs::untinted?: untinted,
        attrs::model?: model,
        attrs::model_unset?: model_unset,
        attrs::request?: run.request,
        attrs::resumes?: run.resumes,
    };
    if let Some(cover) = &run.cover {
        fragment += cover_fragment(id, cover);
    }
    (fragment, id)
}

/// How a run's cover was sized, annotating the run.
pub fn cover_fragment(run: Id, cover: &cover::CoverRecord) -> Fragment {
    let mut fragment = Fragment::empty();
    let source = fragment.put::<UTF8String, _>(cover.window_source.clone());
    let summary = fragment.put::<UTF8String, _>(cover.summary.clone());
    let reduction = cover
        .reduction
        .as_ref()
        .map(|text| fragment.put::<UTF8String, _>(text.clone()));
    let calibration = cover.calibration.as_ref();
    let basis =
        calibration.map(|calibration| fragment.put::<UTF8String, _>(calibration.basis.clone()));
    let count = |value: Option<usize>| value.map(|value| value as u64);
    fragment += entity! { ExclusiveId::force_ref(&run) @
        attrs::cover_percent: cover.percent,
        attrs::window?: cover.window,
        attrs::window_source: source,
        attrs::chars_per_token?: calibration.map(|calibration| calibration.chars_per_token),
        attrs::chars_per_token_n?: calibration.and_then(|calibration| calibration.n),
        attrs::chars_per_token_basis?: basis,
        attrs::cover_target_chars?: count(cover.target_chars),
        attrs::fixed_chars?: count(cover.fixed_chars),
        attrs::memory_budget: cover.memory_budget as u64,
        attrs::cover_chars: cover.cover_chars as u64,
        attrs::prompt_tokens_estimate?: cover.estimate,
        attrs::headroom?: cover.headroom,
        attrs::cover_reduction?: reduction,
        attrs::cover_sizing: summary,
    };
    fragment
}

/// What a run request says. It names no workdir, command, tool or budget:
/// the host that runs it resolves those itself.
#[derive(Clone, Debug)]
pub struct RequestFacts {
    pub zooid: Id,
    pub host: HostKey,
    pub host_label: Option<String>,
    /// The provider policy asked for: `claude`, `codex`, `auto` or `local`.
    pub provider: String,
    pub effort: String,
    pub previous: Option<Id>,
    pub handoff: Option<String>,
    /// The cover share asked for (`--cover`), carried like the provider and
    /// effort. Absent keeps the identity a request had before it existed.
    pub cover_percent: Option<f64>,
    pub requested_at: IntervalValue,
    /// The persona that asked, messaged when the run ends. A reply route,
    /// not authority: authority is the COMMIT signer's WRITE admission.
    pub spawner: Id,
}

/// A request: everything it asks for is its intrinsic core (known when it is
/// written, the same for every reader), the label and the asker annotate it.
/// Returns the fragment and the request id.
pub fn request_fragment(request: &RequestFacts) -> (Fragment, Id) {
    let mut fragment = Fragment::empty();
    let provider = fragment.put::<UTF8String, _>(request.provider.clone());
    let effort = fragment.put::<UTF8String, _>(request.effort.clone());
    let handoff = request
        .handoff
        .as_ref()
        .map(|text| fragment.put::<UTF8String, _>(text.clone()));
    let core = entity! { _ @
        metadata::tag: KIND_REQUEST,
        attrs::zooid: request.zooid,
        attrs::host: request.host,
        attrs::provider: provider,
        attrs::effort: effort,
        attrs::previous?: request.previous,
        attrs::handoff?: handoff,
        attrs::cover_percent?: request.cover_percent,
        metadata::created_at: request.requested_at,
    };
    let id = core.root().expect("a request has one intrinsic root");
    fragment += core;
    let label = request
        .host_label
        .as_ref()
        .map(|text| fragment.put::<UTF8String, _>(text.clone()));
    fragment += entity! { ExclusiveId::force_ref(&id) @
        archive_attrs::author: request.spawner,
        attrs::host_label?: label,
    };
    (fragment, id)
}

/// One outcome of a request: the request and the outcome kind are its
/// core, so recording it again names the same entity; when it was observed,
/// the run, the unit and the reason annotate it.
pub fn outcome_fragment(
    request: Id,
    outcome: Id,
    at: IntervalValue,
    run: Option<Id>,
    unit: Option<&str>,
    reason: Option<&str>,
) -> (Fragment, Id) {
    let core = entity! { _ @
        metadata::tag: KIND_OUTCOME,
        attrs::request: request,
        attrs::outcome: outcome,
    };
    let id = core.root().expect("an outcome has one intrinsic root");
    let mut fragment = Fragment::empty();
    fragment += core;
    let unit = unit.map(|text| fragment.put::<UTF8String, _>(text.to_owned()));
    let reason = reason.map(|text| fragment.put::<UTF8String, _>(text.to_owned()));
    fragment += entity! { ExclusiveId::force_ref(&id) @
        metadata::created_at: at,
        attrs::outcome_run?: run,
        attrs::unit?: unit,
        attrs::reason?: reason,
    };
    (fragment, id)
}

/// The run's link to the request it serves, with the tag a keeper looks for.
/// Launch publishes the same two facts in the run record; settlement
/// publishes them again from inside the unit, so a keeper that stopped
/// between starting the unit and publishing the record still finds the run.
pub fn run_request_link(run: Id, request: Id) -> Fragment {
    entity! { ExclusiveId::force_ref(&run) @
        metadata::tag: KIND_RUN,
        attrs::request: request,
    }
}

/// A note about a run that is not part of its end: the message settlement
/// sent, why a settlement step failed, the goal note of its launch.
pub fn run_annotation(
    run: Id,
    message: Option<Id>,
    settle_failure: Option<&str>,
    goal_note: Option<Id>,
) -> Fragment {
    let mut fragment = Fragment::empty();
    let failure = settle_failure.map(|text| fragment.put::<UTF8String, _>(text.to_owned()));
    fragment += entity! { ExclusiveId::force_ref(&run) @
        attrs::message?: message,
        attrs::settle_failure?: failure,
        attrs::goal_note?: goal_note,
    };
    fragment
}

/// How the zooid's main process ended, as systemd reports it to a
/// stop-post step (`EXIT_CODE`, `EXIT_STATUS`, `SERVICE_RESULT`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Exit {
    pub code: Option<u64>,
    pub signal: Option<String>,
    pub result: Option<String>,
}

impl Exit {
    /// `kind` is systemd's `EXIT_CODE` (`exited`, `killed`, `dumped`) and
    /// `status` its `EXIT_STATUS`: a number when exited, a signal name
    /// otherwise. Empty values are absent, not errors: a unit that never
    /// started its main process has neither.
    pub fn from_systemd(kind: &str, status: &str, result: &str) -> Self {
        let present = |value: &str| (!value.trim().is_empty()).then(|| value.trim().to_owned());
        let status = status.trim();
        let (code, signal) = match kind.trim() {
            "exited" => (status.parse().ok(), None),
            "" if status.parse::<u64>().is_ok() => (status.parse().ok(), None),
            _ => (None, present(status)),
        };
        Self {
            code,
            signal,
            result: present(result),
        }
    }

    /// `exit 0`, `signal KILL`, or the bare result when there is nothing else.
    pub fn describe(&self) -> String {
        let end = match (&self.code, &self.signal, &self.result) {
            (Some(code), _, _) => format!("exit {code}"),
            (None, Some(signal), _) => format!("signal {signal}"),
            (None, None, Some(result)) => result.clone(),
            (None, None, None) => "exit unknown".to_owned(),
        };
        match self.result.as_deref() {
            Some(TIMEOUT_RESULT) if end != TIMEOUT_RESULT => format!("{end}, {RUNTIME_LIMIT_NOTE}"),
            Some(TIMEOUT_RESULT) => RUNTIME_LIMIT_NOTE.to_owned(),
            _ => end,
        }
    }

    /// Whether the run ended as a success: exit 0, and, for a hosted run,
    /// not stopped by its runtime limit (an agent that exits 0 on the
    /// limit's SIGTERM did not finish). A `local` run lives in Drive's loop
    /// until it is interrupted, by its limit too, so exit 0 after that
    /// interrupt is its ordinary teardown.
    pub fn clean(&self, local: bool) -> bool {
        self.code == Some(0) && (local || self.result.as_deref() != Some(TIMEOUT_RESULT))
    }
}

/// systemd's `SERVICE_RESULT` when `RuntimeMaxSec` stopped the unit.
pub const TIMEOUT_RESULT: &str = "timeout";

/// How a run's end says its runtime limit stopped it.
pub const RUNTIME_LIMIT_NOTE: &str = "stopped at its runtime limit";

/// What a run's agent reported it used, read at settlement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// The agent's own estimate from the prices in the run's configuration,
    /// not the provider's bill.
    pub cost_usd: Option<f64>,
}

/// The usage in vibe's `export.json` (`--output-dir`): `usage.*` and
/// `cost_usd`. A field vibe did not write is absent, not zero.
pub fn vibe_usage(export: &str) -> Result<Usage> {
    let value: serde_json::Value =
        serde_json::from_str(export).map_err(|error| anyhow::anyhow!("export.json: {error}"))?;
    let usage = &value["usage"];
    let count = |field: &str| usage[field].as_u64();
    let usage = Usage {
        input_tokens: count("input_tokens"),
        cached_input_tokens: count("cached_input_tokens"),
        output_tokens: count("output_tokens"),
        total_tokens: count("total_tokens"),
        cost_usd: value["cost_usd"].as_f64().filter(|cost| cost.is_finite()),
    };
    if usage == Usage::default() {
        bail!("export.json names no usage and no cost");
    }
    Ok(usage)
}

/// What the provider itself reported about a run's context, read at
/// settlement: the window for its model, the input tokens of its first
/// model call (overhead, prompt and cover in one total), and the
/// conversation it recorded, as the archive's `session/<id>` locator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reported {
    pub context_window: Option<u64>,
    pub first_call_input_tokens: Option<u64>,
    pub source_locator: Option<String>,
}

/// The end of a run, annotating the run itself. `report` is a hosted run's
/// final reply, or a local run's closing message when it left one; both are
/// recorded as the run's `report`.
pub fn settlement_fragment(
    run: Id,
    finished_at: IntervalValue,
    exit: &Exit,
    report: Option<&str>,
    usage: Option<&Usage>,
    reported: &Reported,
) -> (Fragment, Option<TextHandle>) {
    let mut fragment = Fragment::empty();
    let report = report.map(|text| fragment.put::<UTF8String, _>(text.to_owned()));
    let result = exit
        .result
        .as_ref()
        .map(|text| fragment.put::<UTF8String, _>(text.clone()));
    let signal = exit
        .signal
        .as_ref()
        .map(|text| fragment.put::<UTF8String, _>(text.clone()));
    fragment += entity! { ExclusiveId::force_ref(&run) @
        metadata::finished_at: finished_at,
        attrs::exit_code?: exit.code,
        attrs::result?: result,
        attrs::signal?: signal,
        attrs::report?: report,
    };
    if let Some(usage) = usage {
        fragment += entity! { ExclusiveId::force_ref(&run) @
            imagine_attrs::input_tokens?: usage.input_tokens,
            attrs::cached_input_tokens?: usage.cached_input_tokens,
            imagine_attrs::output_tokens?: usage.output_tokens,
            imagine_attrs::total_tokens?: usage.total_tokens,
            attrs::cost_usd?: usage.cost_usd,
        };
    }
    if reported != &Reported::default() {
        // The archive's own attribute, so a run and the receipts of the
        // conversation it recorded share one locator.
        let locator = reported
            .source_locator
            .as_ref()
            .map(|text| fragment.put::<UTF8String, _>(text.clone()));
        fragment += entity! { ExclusiveId::force_ref(&run) @
            attrs::context_window?: reported.context_window,
            attrs::first_call_input_tokens?: reported.first_call_input_tokens,
            blockdag::source_projection::source_locator?: locator,
        };
    }
    (fragment, report)
}

/// The fixed preamble that tells a zooid who it is.
pub fn preamble(name: &str, spawner: &str, local: bool) -> String {
    if local {
        return local_preamble(name, spawner);
    }
    format!(
        "You are {name}, a task zooid of this colony, spawned by {spawner} to do the one task in the \
         brief below. Your name is your persona: PERSONA is set to it, and your faculty writes \
         are attributed to it. Your context ends when the task ends. Your final reply is your \
         report: it is recorded in the pile and {spawner} is messaged when you exit, so make it \
         the answer, not a farewell. Do not arm a colony watcher (no `orient wait`, no \
         `orient_next`): you are not a window, and news would fire into a context that ends \
         with this task. You have the faculties MCP tools, served for you alone. Their write \
         tools never read PERSONA, so pass {name} wherever a write tool takes `from`, \
         `persona`, `by` or `reader`. Your name is also an address: {spawner} or another zooid \
         may message you while you work. On a longer task, check between steps with \
         `orient_poll` (persona {name}, peek true), answer with `message_send` from {name}, and \
         `message_ack` what you have handled. The wake above is the shared self, its memories \
         recalled with this brief in mind.\n"
    )
}

/// A local zooid lives in Drive's loop on this host, which reads its own
/// wake: only what is true there. No task end, no closing tool; the host
/// stops her, and her history lets her be resumed.
fn local_preamble(name: &str, spawner: &str) -> String {
    format!(
        "You are {name}, a zooid of this colony, started by {spawner} with the brief below. You \
         live in Drive's loop on this host; the host stops you. Everything you perceive and do \
         is archived, so you can be resumed into your own history later. Your name is your \
         persona, and the faculties are your native tools: pass {name} wherever a write tool \
         takes `from`, `persona`, `by` or `reader`. Your name is also an address: you reach \
         {spawner} and the colony with `message_send` from {name}, and news for you arrives \
         through Orient; check with `orient_poll` (persona {name}, peek true) and \
         `message_ack` what you have handled. The wake before this is the shared self, its \
         memories recalled with this brief in mind.\n"
    )
}

/// The opening context in its fixed order: wake (one cover, tinted by the
/// brief), preamble, brief, and a request's handoff last, once, when there
/// is one.
pub fn assemble_prompt(wake: &str, preamble: &str, brief: &str, handoff: Option<&str>) -> String {
    let mut prompt = String::with_capacity(
        wake.len() + preamble.len() + brief.len() + handoff.map_or(0, str::len),
    );
    let sections = [
        ("Wake", Some(wake)),
        ("Who you are", Some(preamble)),
        ("Brief", Some(brief)),
        ("Handoff", handoff),
    ];
    for (title, body) in sections
        .into_iter()
        .filter_map(|(title, body)| body.map(|body| (title, body)))
    {
        prompt.push_str("# ");
        prompt.push_str(title);
        prompt.push_str("\n\n");
        prompt.push_str(body.trim_end());
        prompt.push_str("\n\n");
    }
    prompt
}

/// A local zooid's first input: [`assemble_prompt`] without the wake, for
/// an executor that reads its own (Drive's `--brief-file`).
pub fn assemble_brief(preamble: &str, brief: &str, handoff: Option<&str>) -> String {
    let mut text = String::new();
    for (title, body) in [
        ("Who you are", Some(preamble)),
        ("Brief", Some(brief)),
        ("Handoff", handoff),
    ]
    .into_iter()
    .filter_map(|(title, body)| body.map(|body| (title, body)))
    {
        text.push_str("# ");
        text.push_str(title);
        text.push_str("\n\n");
        text.push_str(body.trim_end());
        text.push_str("\n\n");
    }
    text
}

/// The longest prefix of a topic that fits [`ABOUT_BYTES`], ending on a
/// character boundary: the memory cover's tint query.
pub fn about(topic: &str) -> &str {
    let topic = topic.trim_start();
    if topic.len() <= ABOUT_BYTES {
        return topic;
    }
    let mut end = ABOUT_BYTES;
    while !topic.is_char_boundary(end) {
        end -= 1;
    }
    &topic[..end]
}

/// The environment variables of collection routes that `colony_env` names
/// and `resolved` lacks. `resolved` holds the variable names a launch pinned.
/// Lines are read as `[export ]NAME=...`; anything else is ignored.
pub fn missing_routes(colony_env: &str, resolved: &[(String, String)]) -> Vec<String> {
    let known: Vec<String> = crate::collection_names::table()
        .into_iter()
        .map(|(scope, _)| crate::collection_names::override_env_name(scope))
        .collect();
    let mut missing: Vec<String> = colony_env
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            let (name, _) = line.split_once('=')?;
            Some(name.trim().to_owned())
        })
        .filter(|name| known.contains(name))
        .filter(|name| !resolved.iter().any(|(pinned, _)| pinned == name))
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// At most this many of a goal's latest notes go into its brief.
pub const GOAL_NOTES: usize = 6;
/// Notes stop being added once their text reaches this many characters; the
/// latest note is always included whole.
pub const GOAL_NOTE_CHARS: usize = 6_000;

/// A brief rendered from one Compass goal revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalBrief {
    pub text: String,
    /// The status event and notes it was rendered from.
    pub revision: Vec<Id>,
    /// The memory cover's tint topic: the title and its purpose.
    pub about: String,
}

/// The brief for a goal: its title, the purpose its parent chain gives it,
/// its latest notes as the current decisions, the zooid's authority over
/// the goal, and the spawner's own instructions last.
pub fn goal_brief(reading: &crate::compass::GoalReading, extra: Option<&str>) -> GoalBrief {
    let title = if reading.titles.is_empty() {
        "(untitled)".to_owned()
    } else {
        reading.titles.join(" / ")
    };
    let mut text = String::new();
    let mut revision = Vec::new();
    text.push_str(&format!(
        "# Goal: {title}\n\nCompass goal {:x}",
        reading.goal
    ));
    match &reading.status {
        Some((event, status)) => {
            text.push_str(&format!(", status {status} (event {event:x}).\n"));
            revision.push(*event);
        }
        None => text.push_str(", no status recorded.\n"),
    }
    if !reading.ancestors.is_empty() {
        text.push_str("\n## Purpose\n\nThis goal serves, nearest first:\n");
        for (id, ancestor) in &reading.ancestors {
            text.push_str(&format!("- {ancestor} ({id:x})\n"));
        }
    }
    let mut chosen = Vec::new();
    let mut chars = 0;
    for note in reading.notes.iter().rev().take(GOAL_NOTES) {
        if !chosen.is_empty() && chars + note.text.chars().count() > GOAL_NOTE_CHARS {
            break;
        }
        chars += note.text.chars().count();
        chosen.push(note);
    }
    chosen.reverse();
    if !chosen.is_empty() {
        text.push_str(&format!(
            "\n## Current decisions\n\nThe latest {} of the goal's {} current notes, oldest \
             first. `compass show {:x}` has every note.\n",
            chosen.len(),
            reading.notes.len(),
            reading.goal
        ));
        for note in chosen {
            revision.push(note.id);
            text.push_str(&format!("\n### Note {:x}, {}\n\n", note.id, note.at));
            text.push_str(note.text.trim_end());
            text.push('\n');
        }
    }
    text.push_str(&format!(
        "\n## This goal and your run\n\n\
         This goal prompted the run. You may add notes to it and move it (`compass note {id:x}`, \
         `compass move {id:x}`), as you may any goal, under the colony's ordinary rules for \
         attribution and coordination. The brief steers your attention; it does not fence it. \
         If you find more important work along the way, you may follow it: say so in a note on \
         this goal and in your report. Your process ending does not achieve the goal, and \
         nothing moves it for you. When something useful happens, a note on the goal is a good \
         place for it: a result with its evidence (commit, file, test result), the remaining \
         step or dependency, or a review question naming the exact candidate. When the goal is \
         done you may move it to `done` with an outcome note. None of these is a condition for \
         ending your run.\n",
        id = reading.goal
    ));
    if let Some(extra) = extra.filter(|extra| !extra.trim().is_empty()) {
        text.push_str("\n## Instructions from the spawner\n\n");
        text.push_str(extra.trim_end());
        text.push('\n');
    }
    let purpose = reading
        .ancestors
        .iter()
        .map(|(_, title)| title.as_str())
        .collect::<Vec<_>>();
    let about_text = if purpose.is_empty() {
        title
    } else {
        format!("{title}. Serves: {}", purpose.join("; "))
    };
    GoalBrief {
        text,
        revision,
        about: about(&about_text).to_owned(),
    }
}

/// One word for a POSIX shell, single-quoted unless it needs no quoting.
pub fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./:=@+,".contains(&byte))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Refuse a path systemd would reinterpret in a unit property: whitespace,
/// quotes, backslashes, `%` specifiers and `$` expansions.
pub fn require_unit_safe(what: &str, path: &str) -> Result<()> {
    if path.is_empty()
        || path
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || "%$\\\"'".contains(ch))
    {
        bail!("{what} {path:?} cannot be named in a systemd unit property");
    }
    Ok(())
}

/// Everything the unit script needs, all of it literal.
#[derive(Clone, Debug)]
pub struct ScriptFacts<'a> {
    pub name: &'a str,
    pub run: Id,
    /// The request the run serves, when a keeper started it. Settlement
    /// links the run to it, whatever happened to the keeper.
    pub request: Option<Id>,
    pub pile: &'a str,
    pub key: &'a str,
    pub path: &'a str,
    pub colony_env: Option<&'a str>,
    /// `TRIBLESPACE_COLLECTION_*` variable and value for every collection
    /// route the launch itself resolved. Exported after the colony file, so
    /// the zooid and its settlement open exactly what the launch recorded in.
    pub routes: &'a [(String, String)],
    /// The agent command line, without its input and output redirections.
    pub agent: &'a [String],
    /// Variables exported for the agent only, after the settle branch.
    pub agent_env: &'a [(String, String)],
    /// Variables the agent gets from Secrets: the variable and the exact
    /// Secrets version. Each is unset for every branch at the top, so
    /// neither settlement nor the MCP server (which the agent starts with
    /// its own environment) ever holds it, and read with `secrets get` just
    /// before the agent starts. The value appears in no file and no log.
    pub agent_secrets: &'a [(String, String)],
    /// The `secrets` command, found on `path` inside the unit.
    pub secrets: &'a str,
    /// A file in which the agent leaves its usage (vibe's `export.json`),
    /// for settlement to record on the run.
    pub usage: Option<&'a str>,
    /// Whether the agent's stdout is its report. When not, the agent writes
    /// the report file itself (a local executor only when the zooid leaves a
    /// closing message) and its stdout goes to the unit's log.
    pub stdout_is_report: bool,
    pub prompt: &'a str,
    pub report: &'a str,
    /// The unit's log. The MCP server's stderr is appended to it, so a
    /// server that fails to start says why; its stdout is the protocol.
    pub log: &'a str,
    pub spawn: &'a str,
    /// The `faculties` binary, absolute. The script run as `mcp` is the
    /// zooid's own stdio MCP server, started by the agent from its MCP
    /// config, in the same environment as the CLI calls and settlement.
    pub faculties: &'a str,
}

/// The unit's script. Run without arguments it is the zooid: the agent on
/// the prompt, writing the report. Run as `settle` it is the stop-post step,
/// which systemd runs however the main process ended. Run as `inbox stop` or
/// `inbox post` it is the agent's Stop or PostToolUse hook, showing the zooid
/// the messages it has not read (`zooid inbox`). Run as `mcp` it is the
/// zooid's faculties MCP server: the agent starts it as a child, so it runs
/// inside the unit and ends with it, and the environment it gets is the one
/// exported here for the CLI, not a second copy that could drift.
pub fn unit_script(facts: &ScriptFacts<'_>) -> String {
    let q = |word: &str| shell_word(word);
    let mut script = String::new();
    script.push_str("#!/bin/bash\n");
    script.push_str(&format!(
        "# Written by `zooid spawn` for zooid {}, run {:x}. The unit runs it twice:\n\
         # bare as the zooid, and with `settle` as its ExecStopPost step.\n",
        facts.name, facts.run
    ));
    script.push_str("# The user manager hands over its own environment: drop its routes. The\n");
    script.push_str("# colony file supplies what the launch did not resolve (peers, routes this\n");
    script.push_str("# build does not know); every route the launch did resolve is pinned after\n");
    script.push_str("# it, so the zooid and its settlement open the collections the run is in.\n");
    let secret_vars = facts
        .agent_secrets
        .iter()
        .map(|(variable, _)| format!(" {variable}"))
        .collect::<String>();
    script.push_str(&format!(
        "unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT{secret_vars} ${{!TRIBLESPACE_@}}\n"
    ));
    if let Some(env) = facts.colony_env {
        script.push_str(&format!("set -a; . {} || exit 78; set +a\n", q(env)));
    }
    for (variable, value) in facts.routes {
        script.push_str(&format!("export {}={}\n", variable, q(value)));
    }
    script.push_str(&format!(
        "export PILE={} TRIBLESPACE_KEY={} PERSONA={} PATH={}\n",
        q(facts.pile),
        q(facts.key),
        q(facts.name),
        q(facts.path)
    ));
    script.push_str("if [ \"${1:-}\" = settle ]; then\n");
    let request = facts
        .request
        .map(|request| format!(" --request {request:x}"))
        .unwrap_or_default();
    script.push_str(&format!(
        "  exec {} settle {} --run {:x}{request} --code \"${{EXIT_CODE:-}}\" --status \"${{EXIT_STATUS:-}}\" \
         --result \"${{SERVICE_RESULT:-}}\" --report {}{}\n",
        q(facts.spawn),
        q(facts.name),
        facts.run,
        q(facts.report),
        facts
            .usage
            .map(|path| format!(" --usage {}", q(path)))
            .unwrap_or_default()
    ));
    script.push_str("fi\n");
    // Run as `inbox stop|post` it is the agent's hook: the agent passes the
    // hook's JSON on stdin, which exec keeps. Its stdout is the hook's answer,
    // so only stderr goes to the log.
    script.push_str(&format!(
        "if [ \"${{1:-}}\" = inbox ]; then exec {} inbox {} --run {:x} --hook \"${{2:-}}\" 2>> {}; fi\n",
        q(facts.spawn),
        q(facts.name),
        facts.run,
        q(facts.log)
    ));
    script.push_str(&format!(
        "if [ \"${{1:-}}\" = mcp ]; then exec {} mcp 2>> {}; fi\n",
        q(facts.faculties),
        q(facts.log)
    ));
    for (variable, value) in facts.agent_env {
        script.push_str(&format!("export {}={}\n", variable, q(value)));
    }
    for (variable, version) in facts.agent_secrets {
        script.push_str(&format!(
            "{variable}=$({} get --secret {}) && [ -n \"${variable}\" ] || \
             {{ echo \"zooid: reading {variable} from Secrets failed\" >&2; exit 77; }}\n\
             export {variable}\n",
            q(facts.secrets),
            q(version)
        ));
    }
    let agent = facts
        .agent
        .iter()
        .map(|word| q(word))
        .collect::<Vec<_>>()
        .join(" ");
    if facts.stdout_is_report {
        script.push_str(&format!(
            "exec {agent} < {} > {}\n",
            q(facts.prompt),
            q(facts.report)
        ));
    } else {
        script.push_str(&format!("exec {agent} < {}\n", q(facts.prompt)));
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proquints_match_the_published_examples() {
        // Wilkerson's examples: IPv4 addresses as two 16-bit words.
        assert_eq!(proquints(&[127, 0, 0, 1]), "lusab-babad");
        assert_eq!(proquints(&[63, 84, 220, 193]), "gutih-tugad");
        assert_eq!(proquints(&[63, 118, 7, 35]), "gutuk-bisog");
        assert_eq!(proquints(&[140, 98, 193, 141]), "mudof-sakat");
        assert_eq!(proquints(&[64, 255, 6, 200]), "haguz-biram");
        assert_eq!(proquints(&[128, 30, 52, 45]), "mabiv-gibot");
        assert_eq!(proquints(&[147, 67, 119, 2]), "natag-lisaf");
        assert_eq!(proquints(&[212, 58, 253, 68]), "tibup-zujah");
        assert_eq!(proquints(&[216, 35, 68, 215]), "tobog-higil");
        assert_eq!(proquints(&[216, 68, 232, 21]), "todah-vobij");
        assert_eq!(proquints(&[198, 81, 129, 136]), "sinid-makam");
        assert_eq!(proquints(&[12, 110, 110, 204]), "budov-kuras");
        assert_eq!(proquint(0), "babab");
        assert_eq!(proquint(u16::MAX), "zuzuz");
    }

    #[test]
    fn the_same_brief_is_the_same_zooid_and_a_changed_one_is_not() {
        let (_, first_id, first_handle, first_name) = spawn_fragment("build the thing").unwrap();
        let (_, again_id, again_handle, again_name) = spawn_fragment("build the thing").unwrap();
        let (_, other_id, _, other_name) = spawn_fragment("build the other thing").unwrap();
        assert_eq!(
            (first_id, first_handle, &first_name),
            (again_id, again_handle, &again_name)
        );
        assert_ne!(first_id, other_id);
        assert_ne!(first_name, other_name);
        assert_eq!(first_name, name_for(text_handle("build the thing")));
        assert_eq!(first_name, proquints(&first_handle.raw[..4]));
        let shape = first_name.as_str();
        assert_eq!(shape.len(), 11);
        assert_eq!(shape.as_bytes()[5], b'-');
        assert!(spawn_fragment("  \n").is_err());
    }

    #[test]
    fn the_name_is_an_annotation_not_part_of_the_spawn_identity() {
        let (fragment, id, handle, _) = spawn_fragment("x").unwrap();
        let core = entity! { _ @ metadata::tag: KIND_SPAWN, attrs::brief: handle };
        assert_eq!(core.root(), Some(id));
        assert!(fragment.facts().len() > core.facts().len());
    }

    #[test]
    fn a_request_is_what_it_asks_for_and_an_outcome_is_its_request_and_kind() {
        let at = |n: u64| -> IntervalValue {
            crate::clock::point(hifitime::Epoch::from_tai_seconds(n as f64)).unwrap()
        };
        let facts = |label: &str, spawner: u8, when: u64| RequestFacts {
            zooid: Id::new([1; 16]).unwrap(),
            host: Inline::new([2; 32]),
            host_label: Some(label.to_owned()),
            provider: "claude".to_owned(),
            effort: "high".to_owned(),
            previous: None,
            handoff: Some("h".to_owned()),
            cover_percent: None,
            requested_at: at(when),
            spawner: Id::new([spawner; 16]).unwrap(),
        };
        let (_, first) = request_fragment(&facts("sky", 3, 10));
        // The label and the asker annotate; they are not identity.
        assert_eq!(request_fragment(&facts("other", 4, 10)).1, first);
        // A request asked at another time is another request.
        assert_ne!(request_fragment(&facts("sky", 3, 11)).1, first);
        // A cover share is part of what a request asks for.
        let mut covered = facts("sky", 3, 10);
        covered.cover_percent = Some(20.0);
        assert_ne!(request_fragment(&covered).1, first);
        let mut different = facts("sky", 3, 10);
        different.provider = "codex".to_owned();
        assert_ne!(request_fragment(&different).1, first);
        let kind = Id::new([9; 16]).unwrap();
        let (_, a) = outcome_fragment(first, kind, at(1), None, None, Some("x"));
        let (_, b) = outcome_fragment(first, kind, at(2), Some(first), Some("u"), None);
        assert_eq!(a, b);
        assert_ne!(
            outcome_fragment(first, Id::new([8; 16]).unwrap(), at(1), None, None, None).1,
            a
        );
        assert_eq!(unit_for_request(first), format!("spawn-req-{first:x}"));
        let host = parse_host(&"ab".repeat(32)).unwrap();
        assert_eq!(host_text(host), "ab".repeat(32));
        assert!(parse_host("abcd").is_err() && parse_host(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn systemd_exit_values_map_to_a_code_or_a_signal() {
        assert_eq!(
            Exit::from_systemd("exited", "3", "exit-code"),
            Exit {
                code: Some(3),
                signal: None,
                result: Some("exit-code".to_owned())
            }
        );
        let killed = Exit::from_systemd("killed", "KILL", "signal");
        assert_eq!(killed.code, None);
        assert_eq!(killed.signal.as_deref(), Some("KILL"));
        assert_eq!(killed.describe(), "signal KILL");
        assert_eq!(Exit::from_systemd("", "0", "").describe(), "exit 0");
        assert_eq!(Exit::from_systemd("", "", "").describe(), "exit unknown");
        assert_eq!(
            Exit::from_systemd("", "", "oom-kill").describe(),
            "oom-kill"
        );
        assert!(Exit::from_systemd("exited", "0", "success").clean(false));
        assert!(!Exit::from_systemd("exited", "3", "exit-code").clean(false));
        // The runtime limit fired: never a success, even if the agent caught
        // the SIGTERM and exited 0.
        let stopped = Exit::from_systemd("killed", "TERM", "timeout");
        assert!(!stopped.clean(false));
        assert!(!stopped.clean(true));
        assert_eq!(
            stopped.describe(),
            "signal TERM, stopped at its runtime limit"
        );
        let caught = Exit::from_systemd("exited", "0", "timeout");
        assert!(!caught.clean(false));
        // A local zooid's interrupt is her ordinary end: exit 0 after it is clean.
        assert!(caught.clean(true));
        assert_eq!(caught.describe(), "exit 0, stopped at its runtime limit");
        assert_eq!(
            Exit::from_systemd("", "", "timeout").describe(),
            "stopped at its runtime limit"
        );
    }

    #[test]
    fn prompt_order_is_wake_preamble_brief() {
        let prompt = assemble_prompt("WAKE", "PREAMBLE", "BRIEF", None);
        let at = |needle: &str| prompt.find(needle).unwrap();
        assert!(at("WAKE") < at("PREAMBLE"));
        assert!(!prompt.contains("# Recollection for this task"));
        assert!(at("PREAMBLE") < at("BRIEF"));
        assert!(prompt.ends_with("BRIEF\n\n"));
        assert!(!prompt.contains("# Handoff"));
        let prompt = assemble_prompt("WAKE", "PREAMBLE", "BRIEF", Some("HANDOFF"));
        assert!(prompt.find("BRIEF").unwrap() < prompt.find("# Handoff").unwrap());
        assert_eq!(prompt.matches("HANDOFF").count(), 1);
        assert!(prompt.ends_with("HANDOFF\n\n"));
    }

    #[test]
    fn the_preamble_names_the_zooid_as_an_address_it_polls() {
        let text = preamble("lusab-babad", "tutor", false);
        // Writes are attributed by name, and the name is also its inbox.
        assert!(text.contains(
            "pass lusab-babad wherever a write tool takes `from`, `persona`, `by` or `reader`"
        ));
        assert!(text.contains("`orient_poll` (persona lusab-babad, peek true)"));
        assert!(text.contains("tutor or another zooid may message you"));
        assert!(text.contains("Your final reply is your report"));
        assert!(text.contains("faculties MCP tools"));
        assert!(!text.contains("despawn"));
    }

    #[test]
    fn a_local_zooid_is_told_she_lives_in_drive_and_is_stopped_by_the_host() {
        let text = preamble("lusab-babad", "tutor", true);
        assert!(text.contains("You live in Drive's loop on this host; the host stops you."));
        assert!(text.contains("you can be resumed into your own history"));
        assert!(
            text.contains("you reach tutor and the colony with `message_send` from lusab-babad")
        );
        // Nothing Drive does not have, and no ban on the Orient it arms.
        for absent in [
            "despawn",
            "task_finish",
            "no `orient_next`",
            "final reply",
            "MCP",
        ] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }
        assert!(text.contains(
            "pass lusab-babad wherever a write tool takes `from`, `persona`, `by` or `reader`"
        ));
        assert!(text.contains("`orient_poll` (persona lusab-babad, peek true)"));
    }

    #[test]
    fn a_local_first_input_is_the_prompt_without_its_wake() {
        let prompt = assemble_prompt("THE WAKE", "who", "the brief", Some("the handoff"));
        let first = assemble_brief("who", "the brief", Some("the handoff"));
        assert_eq!(prompt, format!("# Wake\n\nTHE WAKE\n\n{first}"));
        assert!(!first.contains("THE WAKE"));
    }

    #[test]
    fn about_fits_the_embedding_query_budget_on_a_char_boundary() {
        let brief = "ä".repeat(ABOUT_BYTES);
        let topic = about(&brief);
        assert!(topic.len() <= ABOUT_BYTES);
        assert_eq!(topic.chars().count(), ABOUT_BYTES / 2);
        // A three-byte character straddling the limit is left out whole.
        let brief = format!("{}€tail", "a".repeat(ABOUT_BYTES - 1));
        assert_eq!(about(&brief), "a".repeat(ABOUT_BYTES - 1));
        assert_eq!(about("  short"), "short");
        // The brief that failed a launch on 2026-11-16 was 2000 characters.
        assert!(about(&"word ".repeat(400)).len() <= ABOUT_BYTES);
    }

    #[test]
    fn missing_routes_names_what_the_colony_file_routes_and_the_launch_did_not() {
        let env = "# comment\n\
                   export TRIBLESPACE_COLLECTION_SPAWN=blake3:00\n\
                   TRIBLESPACE_COLLECTION_MESSAGE=blake3:11\n\
                   export TRIBLESPACE_PEERS=x\n\
                   export TRIBLESPACE_COLLECTION_NOT_A_FACULTY=blake3:22\n\
                   export WEMM_ROOT=y\n";
        let pinned = vec![(
            "TRIBLESPACE_COLLECTION_MESSAGE".to_owned(),
            "blake3:11".to_owned(),
        )];
        assert_eq!(
            missing_routes(env, &pinned),
            ["TRIBLESPACE_COLLECTION_SPAWN"]
        );
        let all = vec![
            ("TRIBLESPACE_COLLECTION_MESSAGE".to_owned(), "a".to_owned()),
            ("TRIBLESPACE_COLLECTION_SPAWN".to_owned(), "b".to_owned()),
        ];
        assert!(missing_routes(env, &all).is_empty());
    }

    fn reading(notes: usize) -> crate::compass::GoalReading {
        let id = |byte: u8| Id::new([byte; 16]).unwrap();
        crate::compass::GoalReading {
            goal: id(1),
            titles: vec!["Ship the thing".to_owned()],
            status: Some((id(2), "doing".to_owned())),
            ancestors: vec![(id(3), "The bigger thing".to_owned())],
            notes: (0..notes)
                .map(|n| crate::compass::GoalNote {
                    id: id(10 + n as u8),
                    at: format!("2026-11-16T0{n}:00:00"),
                    by: vec![],
                    text: format!("decision {n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn a_goal_brief_carries_its_revision_purpose_latest_notes_and_authority() {
        let brief = goal_brief(&reading(8), Some("Use the lane at /x."));
        let text = &brief.text;
        assert!(text.starts_with("# Goal: Ship the thing\n"));
        assert!(text.contains("The bigger thing"));
        // The latest six of eight notes, oldest first; the two oldest are out.
        assert!(!text.contains("decision 0") && !text.contains("decision 1\n"));
        assert!(text.find("decision 2").unwrap() < text.find("decision 7").unwrap());
        let id = |byte: u8| Id::new([byte; 16]).unwrap();
        assert_eq!(
            brief.revision,
            [id(2), id(12), id(13), id(14), id(15), id(16), id(17)]
        );
        assert!(text.contains("This goal prompted the run."));
        assert!(text.contains("move it to `done` with an outcome note"));
        assert!(text.contains("None of these is a condition for ending your run."));
        assert!(!text.contains("assigned to you") && !text.contains("other goals' status"));
        assert!(text.contains(
            "If you find more important work along the way, you may follow it: say so in a \
             note on this goal and in your report."
        ));
        assert!(text.trim_end().ends_with("Use the lane at /x."));
        assert_eq!(brief.about, "Ship the thing. Serves: The bigger thing");
        // A new note is a new revision, so a new brief and a new zooid.
        let later = goal_brief(&reading(9), Some("Use the lane at /x."));
        assert_ne!(
            name_for(text_handle(&later.text)),
            name_for(text_handle(text))
        );
        // The same revision renders the same brief.
        assert_eq!(goal_brief(&reading(8), Some("Use the lane at /x.")), brief);
    }

    #[test]
    fn goal_notes_stop_at_the_character_budget_but_keep_the_latest() {
        let mut long = reading(3);
        for note in &mut long.notes {
            note.text = "x".repeat(GOAL_NOTE_CHARS);
        }
        let brief = goal_brief(&long, None);
        assert_eq!(
            brief.revision.len(),
            2,
            "the status and the one latest note"
        );
        assert!(!brief.text.contains("Instructions from the spawner"));
    }

    #[test]
    fn shell_words_survive_quotes() {
        assert_eq!(shell_word("/a/b-c.d"), "/a/b-c.d");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word(""), "''");
        assert!(require_unit_safe("dir", "/home/x/lane").is_ok());
        for bad in ["/a b", "/a%h", "/a$X", "/a\"", ""] {
            assert!(require_unit_safe("dir", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn vibe_usage_reads_the_export_and_refuses_one_without_usage() {
        let usage = vibe_usage(
            r#"{"usage":{"input_tokens":10,"output_tokens":2,"cached_input_tokens":8,
               "total_tokens":12},"cost_usd":0.25,"config":{}}"#,
        )
        .unwrap();
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(10),
                cached_input_tokens: Some(8),
                output_tokens: Some(2),
                total_tokens: Some(12),
                cost_usd: Some(0.25),
            }
        );
        // A field vibe left out is absent, not zero.
        let partial = vibe_usage(r#"{"cost_usd":0.5}"#).unwrap();
        assert_eq!(partial.input_tokens, None);
        assert_eq!(partial.cost_usd, Some(0.5));
        assert!(vibe_usage(r#"{"outcome":"finished"}"#).is_err());
        assert!(vibe_usage("not json").is_err());
    }

    #[test]
    fn the_script_runs_claude_bare_and_settle_as_stop_post() {
        let run = Id::new([7; 16]).unwrap();
        let script = unit_script(&ScriptFacts {
            name: "lusab-babad",
            run,
            request: None,
            pile: "/p/self.pile",
            key: "/p/self.key",
            path: "/bin",
            colony_env: Some("/c/colony.env"),
            routes: &[(
                "TRIBLESPACE_COLLECTION_SPAWN".to_owned(),
                "blake3:abcd".to_owned(),
            )],
            agent: &["/c/claude".to_owned(), "-p".to_owned()],
            agent_env: &[],
            agent_secrets: &[],
            secrets: "secrets",
            usage: None,
            stdout_is_report: true,
            prompt: "/s/n.prompt",
            report: "/s/n.report",
            log: "/s/n.log",
            spawn: "zooid",
            faculties: "/b/faculties",
        });
        assert!(script.contains("set -a; . /c/colony.env || exit 78; set +a\n"));
        assert!(script.contains("PERSONA=lusab-babad"));
        assert!(script.contains(&format!("settle lusab-babad --run {run:x}")));
        assert!(script.contains("exec /c/claude -p < /s/n.prompt > /s/n.report\n"));
        let unset = script.find("unset ").unwrap();
        let sourced = script.find(". /c/colony.env").unwrap();
        let pinned = script
            .find("export TRIBLESPACE_COLLECTION_SPAWN=blake3:abcd\n")
            .unwrap();
        // The launch's own routes win over the colony file, and both come
        // before either the zooid or its settlement runs.
        assert!(unset < sourced && sourced < pinned);
        let settle = script.find("= settle ]").unwrap();
        assert!(pinned < settle);
        assert!(settle < script.find("exec /c/claude").unwrap());
        // Run as `mcp` the script is the zooid's own MCP server, in the same
        // environment as settlement and the CLI, and never the agent.
        let mcp = script
            .find("if [ \"${1:-}\" = mcp ]; then exec /b/faculties mcp 2>> /s/n.log; fi\n")
            .unwrap();
        assert!(settle < mcp && mcp < script.find("exec /c/claude").unwrap());
        // Run as `inbox stop|post` it is the agent's hook, answered by
        // `zooid inbox` for this run, its stderr in the log.
        let inbox = script
            .find(&format!(
                "if [ \"${{1:-}}\" = inbox ]; then exec zooid inbox lusab-babad --run {run:x} \
                 --hook \"${{2:-}}\" 2>> /s/n.log; fi\n"
            ))
            .unwrap_or_else(|| panic!("{script}"));
        assert!(pinned < inbox && inbox < script.find("exec /c/claude").unwrap());
    }

    /// The `inbox` dispatch, run: the hook's stdin reaches `zooid inbox`, its
    /// stdout is the hook's answer, its stderr goes to the log, and neither
    /// the agent nor settlement runs.
    #[test]
    fn the_script_run_as_inbox_hands_the_hook_input_to_spawn_inbox() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).display().to_string();
        let fake = |name: &str, body: &str| {
            let file = dir.path().join(name);
            std::fs::write(&file, format!("#!/bin/bash\n{body}\n")).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            file.display().to_string()
        };
        let spawn = fake(
            "zooid",
            "echo \"args: $*\"; echo \"persona: $PERSONA\"; echo \"stdin: $(cat)\"; echo oops >&2",
        );
        let agent = fake("agent", "echo agent ran");
        let run = Id::new([7; 16]).unwrap();
        let log = path("n.log");
        let script_text = unit_script(&ScriptFacts {
            name: "lusab-babad",
            run,
            request: None,
            pile: "/p/self.pile",
            key: "/p/self.key",
            path: "/usr/bin:/bin",
            colony_env: None,
            routes: &[],
            agent: &[agent],
            agent_env: &[],
            agent_secrets: &[],
            secrets: "secrets",
            usage: None,
            stdout_is_report: true,
            prompt: "/dev/null",
            report: &path("n.report"),
            log: &log,
            spawn: &spawn,
            faculties: "/b/faculties",
        });
        let script = dir.path().join("n.sh");
        std::fs::write(&script, script_text).unwrap();
        let mut child = std::process::Command::new("/bin/bash")
            .arg(&script)
            .args(["inbox", "post"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"hook_event_name":"PostToolUse"}"#)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "args: inbox lusab-babad --run {run:x} --hook post\npersona: lusab-babad\n\
                 stdin: {{\"hook_event_name\":\"PostToolUse\"}}\n"
            )
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "oops\n");
        assert!(!dir.path().join("n.report").exists());
    }

    #[test]
    fn an_agent_that_writes_its_own_report_keeps_stdout_for_the_log() {
        let script = unit_script(&ScriptFacts {
            name: "lusab-babad",
            run: Id::new([7; 16]).unwrap(),
            request: None,
            pile: "/p/self.pile",
            key: "/p/self.key",
            path: "/bin",
            colony_env: None,
            routes: &[],
            agent: &["codex".to_owned(), "exec".to_owned(), "-".to_owned()],
            agent_env: &[("CODEX_HOME".to_owned(), "/s/n.codex".to_owned())],
            agent_secrets: &[],
            secrets: "secrets",
            usage: None,
            stdout_is_report: false,
            prompt: "/s/n.prompt",
            report: "/s/n.report",
            log: "/s/n.log",
            spawn: "zooid",
            faculties: "/b/faculties",
        });
        assert!(script.contains("exec codex exec - < /s/n.prompt\n"));
        assert!(!script.contains(". /"));
        // The agent's own home is exported for the agent, not for settlement.
        let home = script.find("export CODEX_HOME=/s/n.codex\n").unwrap();
        assert!(script.find("= settle ]").unwrap() < home);
        // Nor for the MCP server, which needs no codex home.
        assert!(script.find("= mcp ]").unwrap() < home);
        assert!(home < script.find("exec codex").unwrap());
    }
}
