//! Task zooids started from a brief, each in its own systemd user unit.
//!
//! One input fixes the whole zooid: the brief. Its blob handle names the
//! spawn (the first 32 bits of the handle as two proquints), it
//! tints the memory tail the zooid wakes into, and it is the prompt. An
//! identical brief relaunches the same zooid; a changed brief is a new one.
//!
//! This module holds the pure parts: naming, the fragments a launch and a
//! settlement publish, the opening context, and the unit script. Everything
//! that touches the pile, Relations, Message or systemd is in [`operations`].

pub mod cli;
pub mod mcp;
pub mod operations;

pub use operations::{
    LaunchConfig, Launched, Launcher, LibraryRenderer, Renderer, RunRequest, RunView,
    SettleRequest, Settled, Spawn, SpawnView, SystemdLauncher, UnitSpec,
};

use anyhow::{bail, Result};
use triblespace::core::metadata;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Handle, NsTAIInterval};
use triblespace::prelude::*;

use crate::schemas::archive::archive as archive_attrs;
use crate::schemas::spawn::{spawn as attrs, KIND_RUN, KIND_SPAWN};

pub type TextHandle = Inline<Handle<UTF8String>>;
pub type IntervalValue = Inline<NsTAIInterval>;

/// Prefix of the unit a zooid runs in, before its name.
pub const UNIT_PREFIX: &str = "spawn-";
/// Effort levels `claude --effort` accepts.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// Characters of the brief that tint the memory tail.
pub const ABOUT_CHARS: usize = 2_000;

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

/// `blake3:<hex>` for a handle, as other faculties print one.
pub fn handle_text(handle: TextHandle) -> String {
    format!("blake3:{}", hex::encode(handle.raw))
}

/// The handle a text has, with no store involved.
pub fn text_handle(text: &str) -> TextHandle {
    Fragment::empty().put::<UTF8String, _>(text.to_owned())
}

/// A spawn: the brief as its intrinsic core, the name as an annotation.
/// Returns the fragment, the spawn id, the brief handle and the name.
pub fn spawn_fragment(brief: &str) -> Result<(Fragment, Id, TextHandle, String)> {
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
    let name = name_for(handle);
    let name_handle = fragment.put::<UTF8String, _>(name.clone());
    fragment += entity! { ExclusiveId::force_ref(&id) @ attrs::name: name_handle };
    Ok((fragment, id, handle, name))
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
    fragment += entity! { ExclusiveId::force_ref(&id) @
        attrs::persona: run.persona,
        archive_attrs::author: run.spawner,
        attrs::unit: unit,
        attrs::workdir: workdir,
        attrs::effort: effort,
        attrs::prompt: prompt,
    };
    (fragment, id)
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
        match (&self.code, &self.signal, &self.result) {
            (Some(code), _, _) => format!("exit {code}"),
            (None, Some(signal), _) => format!("signal {signal}"),
            (None, None, Some(result)) => result.clone(),
            (None, None, None) => "exit unknown".to_owned(),
        }
    }
}

/// The end of a run, annotating the run itself.
pub fn settlement_fragment(
    run: Id,
    finished_at: IntervalValue,
    exit: &Exit,
    report: Option<&str>,
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
    (fragment, report)
}

/// The fixed preamble that tells a zooid who it is.
pub fn preamble(name: &str, spawner: &str) -> String {
    format!(
        "You are {name}, a task zooid of this colony, spawned by {spawner} to do the one task in the \
         brief below. Your name is your persona: PERSONA is set to it, and your faculty writes \
         are attributed to it. Your context ends when the task ends. Your final reply is your \
         report: it is recorded in the pile and {spawner} is messaged when you exit, so make it \
         the answer, not a farewell. Do not arm a colony watcher (no `orient wait`): you are \
         not a window, and news would fire into a context that ends with this task. The wake \
         above is the shared self; the recollection after it was chosen for this brief.\n"
    )
}

/// The opening context in its fixed order: wake, memory tail, preamble, brief.
pub fn assemble_prompt(wake: &str, tail: &str, preamble: &str, brief: &str) -> String {
    let mut prompt = String::with_capacity(wake.len() + tail.len() + preamble.len() + brief.len());
    for (title, body) in [
        ("Wake", wake),
        ("Recollection for this task", tail),
        ("Who you are", preamble),
        ("Brief", brief),
    ] {
        prompt.push_str("# ");
        prompt.push_str(title);
        prompt.push_str("\n\n");
        prompt.push_str(body.trim_end());
        prompt.push_str("\n\n");
    }
    prompt
}

/// The brief's first [`ABOUT_CHARS`] characters, the memory tail's topic.
pub fn about(brief: &str) -> &str {
    match brief.char_indices().nth(ABOUT_CHARS) {
        Some((end, _)) => &brief[..end],
        None => brief,
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
    pub pile: &'a str,
    pub key: &'a str,
    pub path: &'a str,
    pub colony_env: Option<&'a str>,
    pub claude: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub mcp_config: &'a str,
    pub prompt: &'a str,
    pub report: &'a str,
    pub spawn: &'a str,
}

/// The unit's script. Run without arguments it is the zooid: claude on the
/// prompt, its stdout the report. Run as `settle` it is the stop-post step,
/// which systemd runs however the main process ended.
pub fn unit_script(facts: &ScriptFacts<'_>) -> String {
    let q = |word: &str| shell_word(word);
    let mut script = String::new();
    script.push_str("#!/bin/bash\n");
    script.push_str(&format!(
        "# Written by `spawn run` for zooid {}, run {:x}. The unit runs it twice:\n\
         # bare as the zooid, and with `settle` as its ExecStopPost step.\n",
        facts.name, facts.run
    ));
    script.push_str("# The user manager hands over its own environment: drop its routes and\n");
    script.push_str("# take the colony's, so the zooid reads and writes what its spawner does.\n");
    script.push_str("unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT ${!TRIBLESPACE_@}\n");
    if let Some(env) = facts.colony_env {
        script.push_str(&format!("set -a; . {} || exit 78; set +a\n", q(env)));
    }
    script.push_str(&format!(
        "export PILE={} TRIBLESPACE_KEY={} PERSONA={} PATH={}\n",
        q(facts.pile),
        q(facts.key),
        q(facts.name),
        q(facts.path)
    ));
    script.push_str("if [ \"${1:-}\" = settle ]; then\n");
    script.push_str(&format!(
        "  exec {} settle {} --run {:x} --code \"${{EXIT_CODE:-}}\" --status \"${{EXIT_STATUS:-}}\" \
         --result \"${{SERVICE_RESULT:-}}\" --report {}\n",
        q(facts.spawn),
        q(facts.name),
        facts.run,
        q(facts.report)
    ));
    script.push_str("fi\n");
    script.push_str(&format!(
        "exec {} -p --model {} --effort {} --dangerously-skip-permissions --strict-mcp-config \
         --mcp-config {} --output-format text < {} > {}\n",
        q(facts.claude),
        q(facts.model),
        q(facts.effort),
        q(facts.mcp_config),
        q(facts.prompt),
        q(facts.report)
    ));
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
    }

    #[test]
    fn prompt_order_is_wake_tail_preamble_brief() {
        let prompt = assemble_prompt("WAKE", "TAIL", "PREAMBLE", "BRIEF");
        let at = |needle: &str| prompt.find(needle).unwrap();
        assert!(at("WAKE") < at("TAIL"));
        assert!(at("TAIL") < at("PREAMBLE"));
        assert!(at("PREAMBLE") < at("BRIEF"));
        assert!(prompt.ends_with("BRIEF\n\n"));
    }

    #[test]
    fn about_is_a_char_boundary_prefix() {
        let brief = "ä".repeat(ABOUT_CHARS + 10);
        assert_eq!(about(&brief).chars().count(), ABOUT_CHARS);
        assert_eq!(about("short"), "short");
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
    fn the_script_runs_claude_bare_and_settle_as_stop_post() {
        let run = Id::new([7; 16]).unwrap();
        let script = unit_script(&ScriptFacts {
            name: "lusab-babad",
            run,
            pile: "/p/self.pile",
            key: "/p/self.key",
            path: "/bin",
            colony_env: Some("/c/colony.env"),
            claude: "/c/claude",
            model: "claude-opus-5-5",
            effort: "high",
            mcp_config: "/s/empty-mcp.json",
            prompt: "/s/n.prompt",
            report: "/s/n.report",
            spawn: "spawn",
        });
        assert!(script.contains("set -a; . /c/colony.env || exit 78; set +a\n"));
        assert!(script.contains("PERSONA=lusab-babad"));
        assert!(script.contains(&format!("settle lusab-babad --run {run:x}")));
        assert!(script.contains("< /s/n.prompt > /s/n.report\n"));
        let unset = script.find("unset ").unwrap();
        assert!(unset < script.find(". /c/colony.env").unwrap());
        assert!(script.find("settle").unwrap() < script.find("exec /c/claude").unwrap());
    }
}
