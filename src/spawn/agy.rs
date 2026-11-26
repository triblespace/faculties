//! The Antigravity CLI (`agy`) as a zooid's agent: the pure parts.
//!
//! agy reads its MCP servers, hooks, rules and conversations from
//! `$HOME/.gemini`, always, and no variable moves it: `--settings` refuses
//! `mcpServers` and `hooks`, and a workspace's `.agents/` would be written
//! into the zooid's working tree. So an agy zooid runs with a home of its
//! own ([`prepare_home`]): spawn writes its `.gemini/config`, links the
//! login, and links every other entry of the real home, so `~` still finds
//! the zooid's tools. The host's own `~/.gemini` (the rules, MCP server and
//! conversations of an interactive agy there) is never read or written by a zooid.
//!
//! agy compacts a conversation at a token threshold that cannot be
//! switched off; spawn sets it to the whole window ([`COMPACT_AT`]) and aims
//! to end the zooid's life before it is reached ([`life`]), after which the
//! next run of the same zooid wakes fresh into the cover. The limit is a
//! bound from the largest step seen, not a guarantee: one larger step can
//! still reach the threshold. And the hooks that enforce it have not been
//! run against a real agy yet.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

/// `AGY_CLI_AUTO_COMPACT_OVERRIDE` for every agy zooid: compact only when
/// the model's whole window is full, which a zooid's life never reaches.
pub const COMPACT_AT: &str = "100%";

/// The window a zooid's life is measured against, in tokens, when nothing
/// else names one: Gemini 4 Neon's, as Google states it (512K; the smaller
/// reading, 512,000, so a life ends early rather than late).
/// `SPAWN_AGY_WINDOW` overrides it.
pub const DEFAULT_WINDOW: u64 = 512_000;

/// Tokens kept free at the end of a life for the zooid's last reply: its
/// report, and the refusals of the tool calls it asked for after the limit.
pub const REPORT_RESERVE: u64 = 32_000;

/// The step growth assumed until a conversation has shown one of its own.
/// Measured in the first hour of an interactive agy session (25 Nov 2026):
/// one step of nine file reads grew its context from 80,759 to 190,561
/// tokens, 109,802, the largest growth between its twelve calls.
pub const GROWTH_FLOOR: u64 = 109_802;

/// The home directory's entry an agy zooid gets its own copy of.
pub const GEMINI_DIR: &str = ".gemini";

/// The login file, relative to a home.
pub const TOKEN_FILE: &str = ".gemini/antigravity-cli/antigravity-oauth-token";

/// What agy printed for one print-mode run (`--output-format json`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgyResult {
    pub conversation_id: Option<String>,
    /// `SUCCESS` or `ERROR`.
    pub status: Option<String>,
    pub response: Option<String>,
    pub error: Option<String>,
    pub finished_reason: Option<String>,
    pub usage: super::Usage,
    pub thinking_tokens: Option<u64>,
}

/// agy's result object. A conversation id that is empty, or not a plain
/// name, is none: it is used as a directory name and a `--conversation`
/// argument. `None` when the bytes are not agy's result. The output of a
/// stream-json run, one event per line and a result event after each turn,
/// is read by its last result event: the run's last reply.
pub fn agy_result(bytes: &[u8]) -> Option<AgyResult> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(value) => one_result(value),
        Err(_) => bytes
            .split(|&byte| byte == b'\n')
            .rev()
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .find(|value| value.get("event").is_some_and(|event| event == "result"))
            .and_then(one_result),
    }
}

/// One stream-json user event: what the feeder writes for each input.
pub fn user_event(text: &str) -> String {
    let event = serde_json::json!({ "event": "user", "message": { "content": text } });
    format!("{event}\n")
}

fn one_result(value: serde_json::Value) -> Option<AgyResult> {
    // stream-json wraps the same object as {"event":"result","result":{..}}.
    let value = match value.get("event") {
        Some(event) if event == "result" => value.get("result")?.clone(),
        _ => value,
    };
    value.get("status")?;
    let text = |field: &str| value[field].as_str().map(str::to_owned);
    let usage = &value["usage"];
    let count = |field: &str| usage[field].as_u64();
    Some(AgyResult {
        conversation_id: text("conversation_id").filter(|id| plain_id(id)),
        status: text("status"),
        response: text("response"),
        error: text("error").filter(|error| !error.is_empty()),
        finished_reason: text("finished_reason"),
        usage: super::Usage {
            // agy counts cache reads apart from input; the run's input is
            // both, as vibe's and claude's are.
            input_tokens: match (count("input_tokens"), count("cache_read_tokens")) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
            },
            cached_input_tokens: count("cache_read_tokens"),
            output_tokens: count("output_tokens"),
            total_tokens: count("total_tokens"),
            cost_usd: None,
        },
        thinking_tokens: count("thinking_tokens"),
    })
}

/// A conversation id spawn may use as a path component and an argument.
pub fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// One model call in a transcript: its step and how much context it read,
/// uncached and cache-read together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    pub step: u64,
    pub occupancy: u64,
    pub output: u64,
}

/// The model calls of an agy `transcript_full.jsonl`, in step order: every
/// `PLANNER_RESPONSE` that carries `input_tokens`. Lines that are not JSON
/// (a record still being written) are skipped.
pub fn calls<L: AsRef<str>>(lines: impl IntoIterator<Item = L>) -> Vec<Call> {
    let mut calls: Vec<Call> = lines
        .into_iter()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line.as_ref()).ok()?;
            if value["type"] != "PLANNER_RESPONSE" {
                return None;
            }
            let input = value["input_tokens"].as_u64()?;
            let step = match &value["step_index"] {
                serde_json::Value::String(text) => text.trim().parse().ok()?,
                other => other.as_u64()?,
            };
            Some(Call {
                step,
                occupancy: input + value["cache_read_tokens"].as_u64().unwrap_or(0),
                output: value["output_tokens"].as_u64().unwrap_or(0),
            })
        })
        .collect();
    calls.sort_by_key(|call| call.step);
    calls
}

/// Where a zooid's life stands, from its transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Life {
    /// The context of the latest call, in tokens.
    pub occupancy: u64,
    /// The largest growth between two calls of this conversation, or
    /// [`GROWTH_FLOOR`] while it has shown none as large.
    pub growth: u64,
    pub window: u64,
    /// The latest occupancy at which one more step still leaves the
    /// report its reserve: `window - growth - REPORT_RESERVE`.
    pub limit: u64,
    /// The life is over: no more tool calls, only the report.
    pub over: bool,
}

/// Whether one more step fits. A step may add as much as the largest step
/// seen so far (never less than [`GROWTH_FLOOR`]), and after it the zooid
/// must still have [`REPORT_RESERVE`] tokens for its report. Measured only
/// from the provider's own per-call usage: nothing estimates characters.
pub fn life(calls: &[Call], window: u64) -> Life {
    let occupancy = calls.last().map_or(0, |call| call.occupancy);
    let growth = calls
        .windows(2)
        .map(|pair| pair[1].occupancy.saturating_sub(pair[0].occupancy))
        .max()
        .unwrap_or(0)
        .max(GROWTH_FLOOR);
    let limit = window.saturating_sub(growth).saturating_sub(REPORT_RESERVE);
    Life {
        occupancy,
        growth,
        window,
        limit,
        over: occupancy > limit,
    }
}

/// What the zooid is told when its life is over, as the reason a tool call
/// is refused.
pub fn life_over_reason(life: &Life) -> String {
    format!(
        "Your context holds {} of {} tokens, and one more step could add {}: this life ends \
         here, before the window fills (it is never summarised). No more tool calls will run. \
         Write your report now as your final reply: what you did, what is left, and where it \
         stands. The next run of this zooid wakes fresh into the cover.",
        life.occupancy, life.window, life.growth
    )
}

/// The feeder's last input to a life past its limit: what the hooks say to
/// a turn, for a life that crossed the limit between turns.
pub fn life_over_input() -> String {
    "This life has reached its limit, before the window fills (it is never summarised). No \
     more tool calls will run. Give your closing reply now: what you did, what is left, and \
     what your next life should know. The next life wakes fresh into the wake."
        .to_owned()
}

/// The zooid's own MCP server, as agy's `mcp_config.json`: its unit script
/// run as `mcp`. A cold first call can take long; agy's timeout is per call.
pub fn mcp_config(script: &Path) -> String {
    let config = serde_json::json!({
        "mcpServers": {
            "faculties": {
                "command": "/bin/bash",
                "args": [script.display().to_string(), "mcp"],
                "timeoutSeconds": 3600,
            }
        }
    });
    format!("{config:#}\n")
}

/// The session-only settings (`--settings`): the workdir is trusted, so
/// print mode never stops to ask.
pub fn settings(workdir: &Path) -> String {
    let settings = serde_json::json!({
        "trustedWorkspaces": [workdir.display().to_string()],
    });
    format!("{settings}\n")
}

/// Make `home` an agy zooid's home: `.gemini/config` holding only what
/// spawn wrote (`mcp_config.json`, `hooks.json`), the login linked from
/// `token`, so a refresh one agy writes is the one every agy here reads,
/// and every entry of `real` except `.gemini` linked, so the zooid's shell
/// finds `~/.cargo`, `~/.gitconfig` and the rest. What agy keeps in the
/// home (conversations, logs) stays from run to run, so a later run can
/// resume a conversation. An existing entry that is not a link is left as
/// it is: something wrote it there, and spawn does not delete it.
pub fn prepare_home(
    home: &Path,
    real: &Path,
    token: Option<&Path>,
    mcp: &str,
    hooks: &str,
) -> Result<()> {
    let config = home.join(GEMINI_DIR).join("config");
    std::fs::create_dir_all(&config).with_context(|| format!("create {}", config.display()))?;
    for (name, text) in [("mcp_config.json", mcp), ("hooks.json", hooks)] {
        let path = config.join(name);
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    }
    if let Some(token) = token {
        if !token.is_file() {
            bail!(
                "agy is not logged in here: {} is not a file, so an agy zooid would be refused. \
                 Nothing was started",
                token.display()
            );
        }
        let link = home.join(TOKEN_FILE);
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        relink(&link, token)?;
    }
    link_entries(real, home, home)?;
    Ok(())
}

/// Link every entry of `real` into `into`, except `.gemini` at the top. An
/// entry that holds the home (`~/.local` holds `~/.local/state/spawn/...`)
/// becomes a real directory whose own entries are linked the same way, so
/// the home never contains a link to itself and `~/.local/bin` still
/// resolves.
fn link_entries(real: &Path, into: &Path, home: &Path) -> Result<()> {
    for entry in std::fs::read_dir(real).with_context(|| format!("read {}", real.display()))? {
        let entry = entry.with_context(|| format!("read an entry of {}", real.display()))?;
        let name = entry.file_name();
        let source = entry.path();
        if into == home && name == GEMINI_DIR || source == home {
            continue;
        }
        let link = into.join(&name);
        if home.starts_with(&source) {
            match std::fs::symlink_metadata(&link) {
                Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(&link)
                    .with_context(|| format!("replace {}", link.display()))?,
                _ => {}
            }
            std::fs::create_dir_all(&link).with_context(|| format!("create {}", link.display()))?;
            link_entries(&source, &link, home)?;
            continue;
        }
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => relink(&link, &source)?,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::os::unix::fs::symlink(&source, &link)
                    .with_context(|| format!("link {}", link.display()))?
            }
            Err(error) => return Err(error).with_context(|| format!("inspect {}", link.display())),
        }
    }
    Ok(())
}

fn relink(link: &Path, target: &Path) -> Result<()> {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(link).ok().as_deref() == Some(target) {
                return Ok(());
            }
            std::fs::remove_file(link).with_context(|| format!("replace {}", link.display()))?
        }
        Ok(_) => bail!(
            "{} exists and is not a link; spawn does not replace it",
            link.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", link.display())),
    }
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("link {} to {}", link.display(), target.display()))
}

/// A conversation's full transcript inside an agy home.
pub fn transcript(home: &Path, conversation: &str) -> Result<std::path::PathBuf> {
    if !plain_id(conversation) {
        bail!("conversation id {conversation:?} is not a plain name");
    }
    Ok(home
        .join(".gemini/antigravity-cli/brain")
        .join(conversation)
        .join(".system_generated/logs/transcript_full.jsonl"))
}

/// The conversation whose transcript changed last in an agy home: the
/// running one, for a hook whose input names none.
pub fn latest_conversation(home: &Path) -> Result<Option<String>> {
    let brain = home.join(".gemini/antigravity-cli/brain");
    let entries = match std::fs::read_dir(&brain) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", brain.display())),
    };
    let mut latest = None;
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !plain_id(&name) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(transcript(home, &name)?) else {
            continue;
        };
        let modified = meta.modified().map_err(|error| anyhow!("{error}"))?;
        if latest.as_ref().is_none_or(|(at, _)| modified > *at) {
            latest = Some((modified, name));
        }
    }
    Ok(latest.map(|(_, name)| name))
}

/// Every conversation in an agy home whose transcript's modification time
/// `changed` accepts, sorted: the conversations a run wrote, for one that
/// recorded none.
pub fn conversations_changed_since(
    home: &Path,
    changed: impl Fn(std::time::SystemTime) -> bool,
) -> Result<Vec<String>> {
    let brain = home.join(".gemini/antigravity-cli/brain");
    let entries = match std::fs::read_dir(&brain) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", brain.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", brain.display()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !plain_id(&name) {
            continue;
        }
        let path = transcript(home, &name)?;
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        let modified = meta
            .modified()
            .with_context(|| format!("read the modification time of {}", path.display()))?;
        if changed(modified) {
            found.push(name);
        }
    }
    found.sort();
    Ok(found)
}

/// Seconds agy gives one hook before it stops it: a cold pile read for the
/// inbox takes seconds.
pub const HOOK_TIMEOUT_SECS: u64 = 120;

/// The zooid's hooks, as agy's `hooks.json` (hook names mapping to their
/// events): its unit script run as `inbox agy-tool` before every tool call,
/// `inbox agy-invocation` before every model call and `inbox agy-stop` when
/// it would end. PreToolUse takes a matcher and nests its handlers; the
/// other events list their handlers directly.
pub fn hooks_config(script: &Path) -> String {
    let command = |hook: &str| {
        format!(
            "/bin/bash {} inbox {hook}",
            super::shell_word(&script.display().to_string())
        )
    };
    let handler = |hook: &str| {
        serde_json::json!({
            "type": "command",
            "command": command(hook),
            "timeout": HOOK_TIMEOUT_SECS,
        })
    };
    let config = serde_json::json!({
        "zooid-life": {
            "PreToolUse": [{ "matcher": "*", "hooks": [handler("agy-tool")] }],
        },
        "zooid-inbox": {
            "PreInvocation": [handler("agy-invocation")],
            "Stop": [handler("agy-stop")],
        },
    });
    format!("{config:#}\n")
}

/// One of an agy zooid's hooks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgyHook {
    /// PreToolUse: refuses every tool call once the life is over.
    Tool,
    /// PreInvocation: before each model call, shows new unread messages and,
    /// once the life is over, says so; stops the run before a call that
    /// would leave no room for the report.
    Invocation,
    /// Stop: continues once while messages are unread, as a claude zooid's
    /// Stop hook blocks; never once the life is over.
    Stop,
}

impl AgyHook {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "agy-tool" => Some(Self::Tool),
            "agy-invocation" => Some(Self::Invocation),
            "agy-stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// What agy told the hook: the conversation, and for Stop whether this stop
/// already follows a continue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookInput {
    pub conversation: Option<String>,
    pub continuation_active: bool,
}

pub fn hook_input(text: &str) -> Result<HookInput> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    Ok(HookInput {
        conversation: value["conversationId"]
            .as_str()
            .filter(|id| plain_id(id))
            .map(str::to_owned),
        continuation_active: value["continuation_active"].as_bool().unwrap_or(false),
    })
}

/// The conversation's life, read from its transcript in the zooid's home.
/// `Ok(None)` when no model call has been recorded yet.
pub fn read_life(home: &Path, conversation: Option<&str>, window: u64) -> Result<Option<Life>> {
    let conversation = match conversation {
        Some(id) => id.to_owned(),
        None => match latest_conversation(home)? {
            Some(id) => id,
            None => return Ok(None),
        },
    };
    let path = transcript(home, &conversation)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let calls = calls(text.lines());
    Ok((!calls.is_empty()).then(|| life(&calls, window)))
}

/// A hook's answer: the JSON agy reads from stdout, if any.
///
/// `life` is the conversation's life (none before the first call, or when
/// it could not be read: then no limit is enforced and the caller logs
/// why). `inbox` answers the claude-shaped inbox hook it stands for:
/// PreInvocation is the post hook (new messages as context), Stop the stop
/// hook (a block becomes a continue).
pub fn hook_answer(
    hook: AgyHook,
    life: Option<&Life>,
    inbox: impl FnOnce() -> Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    let over = life.filter(|life| life.over);
    match hook {
        AgyHook::Tool => over.map(
            |life| serde_json::json!({ "decision": "deny", "reason": life_over_reason(life) }),
        ),
        AgyHook::Invocation => {
            let mut steps = Vec::new();
            if let Some(life) = over {
                if life.occupancy.saturating_add(REPORT_RESERVE) >= life.window {
                    // Not even the report fits: end before the model reads
                    // an input the CLI would have to compact.
                    return Some(serde_json::json!({
                        "injectSteps": [{ "errorMessage": { "userMessage": format!(
                            "zooid life limit: the context holds {} of {} tokens, too full \
                             for a report; the run ends before the next model call",
                            life.occupancy, life.window
                        ) } }],
                        "terminationBehavior": "terminate",
                    }));
                }
                steps.push(system_step(&life_over_reason(life)));
            }
            if let Some(context) = inbox().and_then(|value| {
                value["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .map(str::to_owned)
            }) {
                steps.push(system_step(&context));
            }
            (!steps.is_empty()).then(|| serde_json::json!({ "injectSteps": steps }))
        }
        AgyHook::Stop => {
            if over.is_some() {
                return None;
            }
            let answer = inbox()?;
            (answer["decision"] == "block").then(|| {
                serde_json::json!({ "decision": "continue", "reason": answer["reason"].clone() })
            })
        }
    }
}

fn system_step(text: &str) -> serde_json::Value {
    serde_json::json!({ "systemMessage": { "systemMessage": text } })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One planner record in agy 1.5.17's shape (keys as written on sky,
    /// 25 Nov 2026; the numbers are what matters).
    fn planner(step: u64, input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"step_index":{step},"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T22:37:58Z","model":"gemini-4-neon-high","input_tokens":{input},"cache_read_tokens":{cached},"output_tokens":{output},"tool_calls":[]}}"#
        )
    }

    /// The twelve calls of that first hour (input, cache read, output).
    const FIRST_HOUR: [(u64, u64, u64, u64); 12] = [
        (1, 11992, 0, 613),
        (5, 3706, 9935, 166),
        (7, 2230, 11730, 319),
        (10, 13044, 13961, 1101),
        (16, 53753, 27006, 1129),
        (26, 109801, 80760, 1218),
        (37, 103262, 190562, 566),
        (45, 15920, 293825, 406),
        (51, 14707, 309746, 464),
        (56, 18856, 324454, 1095),
        (59, 3978, 341261, 1537),
        (63, 2864, 344406, 1020),
    ];

    fn first_hour() -> Vec<String> {
        FIRST_HOUR
            .iter()
            .map(|(step, input, cached, output)| planner(*step, *input, *cached, *output))
            .collect()
    }

    #[test]
    fn occupancy_is_uncached_input_plus_cache_reads_in_step_order() {
        let mut lines = first_hour();
        lines.reverse();
        lines.push(r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"ERROR","error":"file does not exist","content":"x"}"#.to_owned());
        lines.push("{\"step_index\":64,\"source\":\"MO".to_owned());
        let calls = calls(&lines);
        assert_eq!(calls.len(), 12);
        assert_eq!(
            calls[0],
            Call {
                step: 1,
                occupancy: 11992,
                output: 613
            }
        );
        assert_eq!(calls[11].occupancy, 347_270);
        // Uncached input is smaller than the cache read on most calls, so
        // input_tokens cannot already include the cache reads.
        assert!(FIRST_HOUR.iter().filter(|(_, i, c, _)| i < c).count() >= 7);
    }

    #[test]
    fn the_first_hour_leaves_room_and_its_largest_step_sets_the_margin() {
        let calls = calls(first_hour());
        let life = life(&calls, DEFAULT_WINDOW);
        assert_eq!(life.growth, 109_802);
        assert_eq!(life.limit, DEFAULT_WINDOW - 109_802 - REPORT_RESERVE);
        assert_eq!(life.limit, 370_198);
        assert_eq!(life.occupancy, 347_270);
        assert!(!life.over);
        // One more step as large as its largest, and the life is over.
        let mut more = calls.clone();
        more.push(Call {
            step: 70,
            occupancy: 347_270 + 109_802,
            output: 0,
        });
        let over = super::life(&more, DEFAULT_WINDOW);
        assert!(over.over);
        assert!(over.occupancy + over.growth + REPORT_RESERVE > DEFAULT_WINDOW);
    }

    #[test]
    fn a_step_larger_than_any_before_widens_the_margin() {
        let calls = [
            Call {
                step: 1,
                occupancy: 10_000,
                output: 0,
            },
            Call {
                step: 2,
                occupancy: 210_000,
                output: 0,
            },
        ];
        let life = life(&calls, DEFAULT_WINDOW);
        assert_eq!(life.growth, 200_000);
        assert_eq!(life.limit, DEFAULT_WINDOW - 200_000 - REPORT_RESERVE);
        // The boundary itself: at the limit one more step still fits.
        let at = [Call {
            step: 1,
            occupancy: 370_198,
            output: 0,
        }];
        assert!(!super::life(&at, DEFAULT_WINDOW).over);
        let past = [Call {
            step: 1,
            occupancy: 370_199,
            output: 0,
        }];
        assert!(super::life(&past, DEFAULT_WINDOW).over);
    }

    fn over_life() -> Life {
        life(
            &[Call {
                step: 1,
                occupancy: 400_000,
                output: 0,
            }],
            DEFAULT_WINDOW,
        )
    }

    #[test]
    fn hooks_refuse_tools_and_say_so_once_the_life_is_over() {
        let ok = life(
            &[Call {
                step: 1,
                occupancy: 10_000,
                output: 0,
            }],
            DEFAULT_WINDOW,
        );
        let never = || -> Option<serde_json::Value> { panic!("inbox read") };
        assert_eq!(hook_answer(AgyHook::Tool, Some(&ok), never), None);
        assert_eq!(hook_answer(AgyHook::Tool, None, never), None);
        let over = over_life();
        let denied = hook_answer(AgyHook::Tool, Some(&over), never).unwrap();
        assert_eq!(denied["decision"], "deny");
        assert!(denied["reason"]
            .as_str()
            .unwrap()
            .contains("400000 of 512000"));
        let told = hook_answer(AgyHook::Invocation, Some(&over), || None).unwrap();
        let text = told["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap();
        assert!(text.contains("Write your report now"), "{text}");
        assert!(told.get("terminationBehavior").is_none());
        // Too full for even the report: stop before the model call.
        let full = life(
            &[Call {
                step: 1,
                occupancy: 490_000,
                output: 0,
            }],
            DEFAULT_WINDOW,
        );
        let stopped = hook_answer(AgyHook::Invocation, Some(&full), || None).unwrap();
        assert_eq!(stopped["terminationBehavior"], "terminate");
        // A life that is over ends even with messages unread.
        let unread = || Some(serde_json::json!({"decision": "block", "reason": "unread"}));
        assert_eq!(hook_answer(AgyHook::Stop, Some(&over), unread), None);
    }

    #[test]
    fn the_inbox_reaches_agy_as_a_system_message_and_a_continue() {
        let post = || {
            Some(serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PostToolUse", "additionalContext": "message 1 from cc"}}))
        };
        let shown = hook_answer(AgyHook::Invocation, None, post).unwrap();
        assert_eq!(
            shown,
            serde_json::json!({"injectSteps": [{"systemMessage": {"systemMessage": "message 1 from cc"}}]})
        );
        assert_eq!(hook_answer(AgyHook::Invocation, None, || None), None);
        let block = || Some(serde_json::json!({"decision": "block", "reason": "unread: 2"}));
        assert_eq!(
            hook_answer(AgyHook::Stop, None, block).unwrap(),
            serde_json::json!({"decision": "continue", "reason": "unread: 2"})
        );
        assert_eq!(hook_answer(AgyHook::Stop, None, || None), None);
    }

    #[test]
    fn the_result_object_and_its_stream_form_parse_alike() {
        let json = br#"{"conversation_id":"de757c9f-94f5-4a44-aa7e-05886331f4ef","status":"SUCCESS","response":"done","duration_seconds":3.5,"num_turns":1,"usage":{"input_tokens":100,"output_tokens":20,"thinking_tokens":5,"cache_read_tokens":900,"total_tokens":1025}}"#;
        let result = agy_result(json).unwrap();
        assert_eq!(
            result.conversation_id.as_deref(),
            Some("de757c9f-94f5-4a44-aa7e-05886331f4ef")
        );
        assert_eq!(result.response.as_deref(), Some("done"));
        assert_eq!(result.usage.input_tokens, Some(1000));
        assert_eq!(result.usage.cached_input_tokens, Some(900));
        assert_eq!(result.usage.output_tokens, Some(20));
        let stream = format!(
            r#"{{"event":"result","result":{}}}"#,
            std::str::from_utf8(json).unwrap()
        );
        assert_eq!(agy_result(stream.as_bytes()).unwrap(), result);
        // The failure seen on sky with no login: no conversation, an error.
        let failed = br#"{"conversation_id":"","status":"ERROR","response":"","error":"authentication failed or timed out","duration_seconds":0,"num_turns":0,"usage":{"input_tokens":0,"output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":0},"finished_reason":"error"}"#;
        let failed = agy_result(failed).unwrap();
        assert_eq!(failed.conversation_id, None);
        assert_eq!(
            failed.error.as_deref(),
            Some("authentication failed or timed out")
        );
        assert_eq!(agy_result(b"plain text"), None);
        assert!(!plain_id("../x") && !plain_id("") && plain_id("abc-1_2"));
    }

    #[test]
    fn a_stream_runs_report_is_its_last_result_event() {
        let result = |response: &str| {
            format!(
                r#"{{"event":"result","result":{{"conversation_id":"c-7","status":"SUCCESS","response":"{response}","usage":{{"input_tokens":10,"output_tokens":2,"cache_read_tokens":90,"total_tokens":102}}}}}}"#
            )
        };
        let stream = [
            r#"{"event":"init","conversation_id":"c-7"}"#.to_owned(),
            result("the first turn"),
            r#"{"event":"assistant","message":{"content":"working"}}"#.to_owned(),
            result("the last turn"),
            r#"{"event":"assistant","message":{"content":"cut"#.to_owned(),
        ]
        .join("\n");
        let parsed = agy_result(stream.as_bytes()).unwrap();
        assert_eq!(parsed.response.as_deref(), Some("the last turn"));
        assert_eq!(parsed.conversation_id.as_deref(), Some("c-7"));
        assert_eq!(parsed.usage.input_tokens, Some(100));
        // Events with no result yet are no result.
        assert_eq!(agy_result(br#"{"event":"init"}"#), None);
        // What the feeder writes is one user event per line.
        let event = user_event("line one\nline two");
        assert!(
            event.ends_with('\n') && event.matches('\n').count() == 1,
            "{event}"
        );
        let value: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(value["event"], "user");
        assert_eq!(value["message"]["content"], "line one\nline two");
    }

    #[test]
    fn hooks_config_names_each_event_in_agys_shape() {
        let config: serde_json::Value =
            serde_json::from_str(&hooks_config(Path::new("/s/n.sh"))).unwrap();
        let tool = &config["zooid-life"]["PreToolUse"][0];
        assert_eq!(tool["matcher"], "*");
        assert_eq!(
            tool["hooks"][0]["command"],
            "/bin/bash /s/n.sh inbox agy-tool"
        );
        assert_eq!(
            config["zooid-inbox"]["PreInvocation"][0]["command"],
            "/bin/bash /s/n.sh inbox agy-invocation"
        );
        assert_eq!(
            config["zooid-inbox"]["Stop"][0]["command"],
            "/bin/bash /s/n.sh inbox agy-stop"
        );
        let mcp: serde_json::Value =
            serde_json::from_str(&mcp_config(Path::new("/s/n.sh"))).unwrap();
        assert_eq!(mcp["mcpServers"]["faculties"]["args"][1], "mcp");
        assert_eq!(
            hook_input(r#"{"conversationId":"c-1","continuation_active":true}"#).unwrap(),
            HookInput {
                conversation: Some("c-1".to_owned()),
                continuation_active: true
            }
        );
    }

    #[test]
    fn the_home_links_everything_but_gemini_and_never_itself() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("real");
        let state = real.join(".local/state/spawn");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(real.join(".local/bin")).unwrap();
        std::fs::create_dir_all(real.join(".gemini/config")).unwrap();
        std::fs::write(real.join(".gemini/GEMINI.md"), "the host's own rules").unwrap();
        std::fs::write(real.join(".gitconfig"), "[user]").unwrap();
        std::fs::write(state.join("other.prompt"), "x").unwrap();
        let token = real.join("token");
        std::fs::write(&token, "login").unwrap();
        let home = state.join("n.agy");
        for _ in 0..2 {
            // Twice: a later run of the same zooid finds it prepared.
            prepare_home(&home, &real, Some(&token), "{\"mcp\":1}", "{\"hooks\":1}").unwrap();
        }
        let link = |path: &str| std::fs::read_link(home.join(path)).unwrap();
        assert_eq!(link(".gitconfig"), real.join(".gitconfig"));
        assert_eq!(link(".local/bin"), real.join(".local/bin"));
        assert_eq!(
            link(".local/state/spawn/other.prompt"),
            state.join("other.prompt")
        );
        assert_eq!(link(TOKEN_FILE), token);
        assert!(!home.join(".local/state/spawn/n.agy").exists());
        assert!(!home.join(".gemini/GEMINI.md").exists());
        assert!(std::fs::symlink_metadata(home.join(".gemini"))
            .unwrap()
            .is_dir());
        assert_eq!(
            std::fs::read_to_string(home.join(".gemini/config/mcp_config.json")).unwrap(),
            "{\"mcp\":1}"
        );
        // The host's own agy files were not touched.
        assert_eq!(
            std::fs::read_to_string(real.join(".gemini/GEMINI.md")).unwrap(),
            "the host's own rules"
        );
        assert!(!real.join(".gemini/config/mcp_config.json").exists());
        // A missing login refuses.
        assert!(prepare_home(&home, &real, Some(&real.join("absent")), "", "").is_err());
    }

    #[test]
    fn life_is_read_from_the_conversation_in_the_zooids_home() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        assert_eq!(read_life(home, None, DEFAULT_WINDOW).unwrap(), None);
        let path = transcript(home, "c-1").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, first_hour().join("\n")).unwrap();
        let life = read_life(home, None, DEFAULT_WINDOW).unwrap().unwrap();
        assert_eq!(life.occupancy, 347_270);
        assert_eq!(
            read_life(home, Some("c-1"), DEFAULT_WINDOW).unwrap(),
            Some(life)
        );
        assert_eq!(read_life(home, Some("c-2"), DEFAULT_WINDOW).unwrap(), None);
        assert!(transcript(home, "../c").is_err());
    }
}
