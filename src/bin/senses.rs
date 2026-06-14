//! `senses` — interact with the Reachy Mini body and keep deliberate
//! sensory captures in the pile.
//!
//! Architecture (Rust-tightness audit, 2026-06-14): the Reachy Mini daemon
//! exposes a full REST surface on :8000, so proprioception (`pose`) and
//! motion (`wake`/`sleep`) are pure Rust over reqwest — no Python, no
//! websocket. The single Python island is the camera frame grab (`look`):
//! frame pixels only flow over the daemon's WebRTC/GStreamer pipeline with no
//! HTTP snapshot endpoint, so a thin embedded shim (`senses_frame.py`) pulls
//! one frame via the vendor's already-Rust-backed media pipeline. That shim
//! is the obvious target for a native gstreamer-rs path once the VLA loop
//! needs the continuous stream.
//!
//! Deliberate captures only: there is no continuous-capture command, so the
//! ephemerality of the live perception stream is structural (periphery
//! principle), not a policy. `look`/`listen` mint a fact; nothing else does.

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use faculties::schemas::senses::{KIND_CAPTURE, SENSES_BRANCH_NAME, capture};
use hifitime::Epoch;
use hifitime::efmt::Formatter;
use hifitime::efmt::consts::ISO8601;
use rand_core::OsRng;
use std::path::{Path, PathBuf};
use std::process::Command as PCommand;
use triblespace::core::metadata;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::prelude::*;

type RawHandle = Inline<inlineencodings::Handle<blobencodings::RawBytes>>;
type TextHandle = Inline<inlineencodings::Handle<blobencodings::LongString>>;

const DEFAULT_DAEMON: &str = "http://localhost:8000";
const DEFAULT_PYTHON: &str =
    "/Users/jp/Desktop/chatbot/liora/reachy-venv/bin/python";

/// The embedded frame-grab shim — written to a temp file at runtime so there
/// is no loose script to lose.
const FRAME_SHIM: &str = include_str!("senses_frame.py");

// ── CLI ──────────────────────────────────────────────────────────────────
#[derive(Parser)]
#[command(
    name = "senses",
    about = "Interact with the Reachy Mini body; keep deliberate sensory captures in the pile"
)]
struct Cli {
    /// Path to the pile file
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Branch id (hex). Overrides name-based lookup.
    #[arg(long)]
    branch_id: Option<String>,
    /// Daemon base URL
    #[arg(long, env = "REACHY_DAEMON", default_value = DEFAULT_DAEMON)]
    daemon: String,
    /// Python interpreter for the frame-grab shim (the reachy venv)
    #[arg(long, env = "REACHY_PYTHON", default_value = DEFAULT_PYTHON)]
    python: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Read the body's current proprioceptive state (head pose, body yaw,
    /// antennas, audio direction) and daemon status. Read-only, pure Rust.
    Pose,
    /// Capture one camera frame into the pile and return a handle. Stores the
    /// proprioceptive pose alongside the frame so it can be grounded later.
    Look {
        /// Why you chose to remember this moment (the deliberate note).
        #[arg(long)]
        note: Option<String>,
    },
    /// List deliberate captures kept in the pile.
    List,
    /// Extract a capture's payload. Use @- for stdout, or omit for a default name.
    Get {
        /// Capture entity id (or prefix).
        id: String,
        /// Output path. Omit for a default name, @- for stdout.
        output: Option<String>,
    },
    /// Gentle wake-up motion (daemon-defined, bounded).
    Wake,
    /// Gentle go-to-sleep motion (daemon-defined, bounded).
    Sleep,
}

// ── helpers ──────────────────────────────────────────────────────────────

fn now_tai() -> Inline<inlineencodings::NsTAIInterval> {
    let now = Epoch::now().unwrap_or(Epoch::from_unix_seconds(0.0));
    (now, now).try_to_inline().expect("valid TAI interval")
}

fn interval_key(interval: Inline<inlineencodings::NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.try_from_inline().expect("valid TAI interval");
    lower.to_tai_duration().total_nanoseconds()
}

fn format_time(tai_ns: i128) -> String {
    const NANOS_PER_CENTURY: i128 = 3_155_760_000_000_000_000;
    let centuries = (tai_ns / NANOS_PER_CENTURY) as i16;
    let nanos = (tai_ns % NANOS_PER_CENTURY) as u64;
    let dur = hifitime::Duration::from_parts(centuries, nanos);
    let epoch = Epoch::from_tai_duration(dur);
    Formatter::new(epoch, ISO8601).to_string()
}

fn fmt_id(id: Id) -> String {
    format!("{id:x}")
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("build http client")
}

fn daemon_get(daemon: &str, path: &str) -> Result<serde_json::Value> {
    let url = format!("{daemon}{path}");
    let resp = http()
        .get(&url)
        .send()
        .with_context(|| format!("GET {url} — is the Reachy Mini daemon running?"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("GET {url} → {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parse JSON from {url}"))
}

fn daemon_post(daemon: &str, path: &str) -> Result<()> {
    let url = format!("{daemon}{path}");
    let resp = http()
        .post(&url)
        .send()
        .with_context(|| format!("POST {url} — is the Reachy Mini daemon running?"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        bail!("POST {url} → {status}: {body}");
    }
    Ok(())
}

fn open_repo(path: &Path) -> Result<Repository<Pile>> {
    let mut pile = Pile::open(path)
        .map_err(|e| anyhow::anyhow!("open pile {}: {e:?}", path.display()))?;
    if let Err(err) = pile.restore() {
        let _ = pile.close();
        return Err(anyhow::anyhow!("restore pile {}: {err:?}", path.display()));
    }
    let signing_key = SigningKey::generate(&mut OsRng);
    Repository::new(pile, signing_key, TribleSet::new())
        .map_err(|err| anyhow::anyhow!("create repository: {err:?}"))
}

fn with_senses<T>(
    pile: &Path,
    explicit_branch: Option<&str>,
    f: impl FnOnce(&mut Repository<Pile>, &mut Workspace<Pile>) -> Result<T>,
) -> Result<T> {
    let mut repo = open_repo(pile)?;
    let branch_id = if let Some(hex) = explicit_branch {
        Id::from_hex(hex.trim()).ok_or_else(|| anyhow::anyhow!("invalid branch id '{hex}'"))?
    } else {
        repo.ensure_branch(SENSES_BRANCH_NAME, None)
            .map_err(|e| anyhow::anyhow!("ensure senses branch: {e:?}"))?
    };
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow::anyhow!("pull senses workspace: {e:?}"))?;
    let result = f(&mut repo, &mut ws);
    let close_res = repo.close().map_err(|e| anyhow::anyhow!("close pile: {e:?}"));
    if let Err(err) = close_res {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }
    result
}

// ── commands ───────────────────────────────────────────────────────────────

fn cmd_pose(daemon: &str) -> Result<()> {
    let state = daemon_get(daemon, "/api/state/full")?;
    let status = daemon_get(daemon, "/api/daemon/status").unwrap_or_default();

    let hp = &state["head_pose"];
    let f = |k: &str| hp[k].as_f64().unwrap_or(f64::NAN);
    println!("head pose:");
    println!(
        "  position   x={:+.4} y={:+.4} z={:+.4} (m)",
        f("x"), f("y"), f("z")
    );
    println!(
        "  rotation   roll={:+.4} pitch={:+.4} yaw={:+.4} (rad)",
        f("roll"), f("pitch"), f("yaw")
    );
    if let Some(by) = state["body_yaw"].as_f64() {
        println!("body yaw:    {by:+.4} rad");
    }
    if let Some(ant) = state["antennas_position"].as_array() {
        let vals: Vec<String> = ant.iter().map(|v| format!("{:+.4}", v.as_f64().unwrap_or(f64::NAN))).collect();
        println!("antennas:    [{}] rad", vals.join(", "));
    }
    match state["doa"].as_f64() {
        Some(doa) => println!("audio dir:   {doa:+.1}° (direction of arrival)"),
        None => println!("audio dir:   — (no sound localised)"),
    }
    if let Some(ts) = state["timestamp"].as_str() {
        println!("daemon time: {ts}");
    }
    if let Some(name) = status["robot_name"].as_str() {
        let st = status["state"].as_str().unwrap_or("?");
        let cam = status["camera_specs_name"].as_str().unwrap_or("?");
        println!("body:        {name} ({st}), camera={cam}");
    }
    Ok(())
}

fn cmd_look(
    repo: &mut Repository<Pile>,
    ws: &mut Workspace<Pile>,
    daemon: &str,
    python: &str,
    note: Option<&str>,
) -> Result<()> {
    // 1. Grab one frame via the embedded shim (the one Python island).
    let tmp = std::env::temp_dir();
    let shim_path = tmp.join("senses_frame.py");
    std::fs::write(&shim_path, FRAME_SHIM).context("write frame shim")?;
    let out_png = tmp.join(format!("senses_capture_{}.png", std::process::id()));

    let mut child = PCommand::new(python)
        .arg(&shim_path)
        .arg(&out_png)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("run frame shim with {python}"))?;
    // Cold WebRTC negotiation can occasionally stall; never hang `look`.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    loop {
        if child.try_wait().context("poll frame shim")?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("frame grab timed out after 45s (cold WebRTC negotiation stalled — retry)");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let output = child.wait_with_output().context("collect frame shim output")?;
    if !output.status.success() {
        bail!(
            "frame grab failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let dims = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let (w, h) = dims
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
        .unwrap_or((0, 0));

    let bytes = std::fs::read(&out_png).with_context(|| format!("read {}", out_png.display()))?;
    let nbytes = bytes.len();
    let _ = std::fs::remove_file(&out_png);

    // 2. Capture the proprioceptive pose alongside the frame, for grounding.
    let pose_json = daemon_get(daemon, "/api/state/full")
        .map(|v| v.to_string())
        .unwrap_or_default();

    // 3. Mint the capture entity.
    let frame_h: RawHandle = ws.put::<blobencodings::RawBytes, _>(bytes);
    let pose_h: TextHandle = ws.put(pose_json);
    let note_h: Option<TextHandle> = note.map(|n| ws.put(n.to_string()));
    let w_val: Inline<inlineencodings::U256BE> = w.to_inline();
    let h_val: Inline<inlineencodings::U256BE> = h.to_inline();

    let frag = entity! {
        metadata::tag: &KIND_CAPTURE,
        metadata::created_at: now_tai(),
        capture::frame: frame_h,
        capture::mime: "image/png",
        capture::modality: "vision",
        capture::width: w_val,
        capture::height: h_val,
        capture::pose: pose_h,
        capture::note?: note_h,
    };
    let cap_id = frag.root().expect("capture has an id");
    ws.commit(frag, "senses look");
    repo.push(ws).map_err(|e| anyhow::anyhow!("push: {e:?}"))?;

    println!("captured {w}x{h} vision frame ({} KiB)", nbytes / 1024);
    println!("  id   {}", fmt_id(cap_id));
    if let Some(n) = note {
        println!("  note {n}");
    }
    Ok(())
}

fn cmd_list(ws: &mut Workspace<Pile>) -> Result<()> {
    let space = ws.checkout(..).map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
    let mut rows: Vec<(i128, Id, String, String)> = Vec::new();
    for (cid, modality, created) in find!(
        (c: Id, m: String, t: Inline<inlineencodings::NsTAIInterval>),
        pattern!(&space, [{
            ?c @
                metadata::tag: KIND_CAPTURE,
                capture::modality: ?m,
                metadata::created_at: ?t,
        }])
    ) {
        let note = find!(
            (h: Inline<inlineencodings::Handle<blobencodings::LongString>>),
            pattern!(&space, [{ cid @ capture::note: ?h }])
        )
        .next()
        .and_then(|(h,)| {
            let v: Result<View<str>, _> = ws.get(h);
            v.ok().map(|s| s.to_string())
        })
        .unwrap_or_default();
        rows.push((interval_key(created), cid, modality, note));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    if rows.is_empty() {
        println!("no captures yet — `senses look` keeps one.");
        return Ok(());
    }
    for (k, cid, modality, note) in rows {
        let when = format_time(k);
        let suffix = if note.is_empty() { String::new() } else { format!("  — {note}") };
        println!("{}  {:<6}  {when}{suffix}", &fmt_id(cid)[..12], modality);
    }
    Ok(())
}

fn cmd_get(ws: &mut Workspace<Pile>, id: &str, output: Option<&str>) -> Result<()> {
    let space = ws.checkout(..).map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
    let needle = id.to_lowercase();
    let cap_id = find!(
        (c: Id),
        pattern!(&space, [{ ?c @ metadata::tag: KIND_CAPTURE }])
    )
    .map(|(c,)| c)
    .find(|c| fmt_id(*c).starts_with(&needle))
    .ok_or_else(|| anyhow::anyhow!("no capture matching '{id}'"))?;

    let h = find!(
        (h: RawHandle),
        pattern!(&space, [{ cap_id @ capture::frame: ?h }])
    )
    .next()
    .map(|(h,)| h)
    .ok_or_else(|| anyhow::anyhow!("capture has no frame payload"))?;
    let bytes: anybytes::Bytes = ws
        .get::<anybytes::Bytes, _>(h)
        .map_err(|e| anyhow::anyhow!("get blob: {e:?}"))?;

    if output == Some("@-") {
        use std::io::Write;
        std::io::stdout().write_all(bytes.as_ref()).context("write to stdout")?;
    } else {
        let out_path = output
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{}.png", &fmt_id(cap_id)[..12])));
        std::fs::write(&out_path, bytes.as_ref())
            .with_context(|| format!("write {}", out_path.display()))?;
        eprintln!("Wrote {} ({} KiB)", out_path.display(), bytes.len() / 1024);
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let pile = cli.pile.clone();
    let branch = cli.branch_id.as_deref();
    let daemon = cli.daemon.clone();
    let python = cli.python.clone();

    match cli.command {
        None => {
            Cli::command().print_help().ok();
            println!();
        }
        Some(Command::Pose) => cmd_pose(&daemon)?,
        Some(Command::Wake) => {
            daemon_post(&daemon, "/api/move/play/wake_up")?;
            println!("waking up");
        }
        Some(Command::Sleep) => {
            daemon_post(&daemon, "/api/move/play/goto_sleep")?;
            println!("going to sleep");
        }
        Some(Command::Look { note }) => {
            with_senses(&pile, branch, |repo, ws| {
                cmd_look(repo, ws, &daemon, &python, note.as_deref())
            })?
        }
        Some(Command::List) => with_senses(&pile, branch, |_repo, ws| cmd_list(ws))?,
        Some(Command::Get { id, output }) => {
            with_senses(&pile, branch, |_repo, ws| cmd_get(ws, &id, output.as_deref()))?
        }
    }
    Ok(())
}
