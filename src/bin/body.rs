//! `body` — the Reachy Mini body: perception in, action out, and the
//! deliberate sensory/touch captures it keeps in the pile.

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use faculties::schemas::body::{capture, intent, BODY_BRANCH_NAME, KIND_CAPTURE, KIND_INTENT};
use hifitime::efmt::consts::ISO8601;
use hifitime::efmt::Formatter;
use hifitime::Epoch;
use rand_core::OsRng;
use std::path::{Path, PathBuf};
use std::process::Command as PCommand;
use std::time::{Duration, Instant};
use triblespace::core::metadata;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::prelude::*;

use nalgebra::{Matrix4, Vector3};
use faculties::reachy_kinematics::Kinematics;

type RawHandle = Inline<inlineencodings::Handle<blobencodings::RawBytes>>;
type TextHandle = Inline<inlineencodings::Handle<blobencodings::LongString>>;

const DEFAULT_DAEMON: &str = "http://localhost:8000";
const DEFAULT_PYTHON: &str = "/Users/jp/Desktop/chatbot/liora/reachy-venv/bin/python";

const FRAME_SHIM: &str = include_str!("body_frame.py");

// ── CLI ──────────────────────────────────────────────────────────────────
#[derive(Parser)]
#[command(
    version = faculties::GIT_VERSION,
    name = "body",
    about = "The Reachy Mini body: perception in, action out, deliberate captures to the pile"
)]
struct Cli {
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    #[arg(long)]
    branch_id: Option<String>,
    #[arg(long, env = "REACHY_DAEMON", default_value = DEFAULT_DAEMON)]
    daemon: String,
    #[arg(long, env = "REACHY_PYTHON", default_value = DEFAULT_PYTHON)]
    python: String,
    /// Acknowledge that this unfinished backend talks directly to the motor
    /// bus. The Reachy daemon must be stopped before using read-only probes.
    #[arg(long)]
    experimental_native_serial: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Pose,
    Feel {
        #[arg(long)]
        secs: Option<f64>,
        #[arg(long = "loop")]
        loop_: bool,
        #[arg(long)]
        keep: bool,
        #[arg(long)]
        respond: bool,
        #[arg(long)]
        note: Option<String>,
    },
    Gesture {
        name: String,
    },
    Intent {
        text: Option<String>,
    },
    Look {
        #[arg(long)]
        note: Option<String>,
    },
    List,
    Get {
        id: String,
        output: Option<String>,
    },
    Wake,
    Sleep,
    Observe {
        #[arg(long)]
        frame: Option<PathBuf>,
        #[arg(long)]
        no_frame: bool,
    },
    Act {
        #[arg(allow_hyphen_values = true)]
        pose: String,
        #[arg(long, default_value_t = 0.5)]
        duration: f64,
        #[arg(long, default_value_t = 0.04)]
        dt: f64,
        #[arg(long)]
        now: bool,
    },
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
    let resp = http().get(&url).send()?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("GET {url} -> {status}");
    }
    serde_json::from_str(&body).with_context(|| format!("parse JSON from {url}"))
}

// ── Native Rust Motor Control ───────────────────────────────────────────

#[allow(dead_code)]
fn enable_torque(port: &mut dyn serialport::SerialPort) -> Result<()> {
    let mut dph = rustypot::DynamixelProtocolHandler::v2();
    let ids: Vec<u8> = (10..=18).collect();
    let byte_data = vec![vec![1]; ids.len()];
    dph.sync_write(port, &ids, 64, &byte_data)
        .map_err(|e| anyhow::anyhow!("enable torque: {:?}", e))?;
    Ok(())
}

fn init_kinematics() -> Kinematics {
    let config = include_str!("../../assets/kinematics_data.json");
    let json: serde_json::Value = serde_json::from_str(config).unwrap();
    let mal = json["motor_arm_length"].as_f64().unwrap();
    let rl = json["rod_length"].as_f64().unwrap();
    let mut kinematics = Kinematics::new(mal, rl);
    for m in json["motors"].as_array().unwrap() {
        let bp = m["branch_position"].as_array().unwrap();
        let bp_vec = Vector3::new(
            bp[0].as_f64().unwrap(),
            bp[1].as_f64().unwrap(),
            bp[2].as_f64().unwrap(),
        );

        let t = m["T_motor_world"].as_array().unwrap();
        let mut t_mat = Matrix4::zeros();
        for i in 0..4 {
            let row = t[i].as_array().unwrap();
            for j in 0..4 {
                t_mat[(i, j)] = row[j].as_f64().unwrap();
            }
        }

        let sol = if m["solution"].as_f64().unwrap() != 0.0 {
            1.0
        } else {
            -1.0
        };
        kinematics.add_branch(bp_vec, t_mat.try_inverse().unwrap(), sol);
    }

    let hz = json["head_z_offset"].as_f64().unwrap();
    let t_world_platform = nalgebra::Matrix4::new_translation(&Vector3::new(0.0, 0.0, hz));
    kinematics.reset_forward_kinematics(t_world_platform);
    kinematics
}

fn open_serial() -> Result<Box<dyn serialport::SerialPort>> {
    serialport::new("/dev/cu.usbmodem5AF71345631", 1_000_000)
        .timeout(Duration::from_millis(50))
        .open()
        .context("open serial port")
}

fn require_native_backend_ready() -> Result<()> {
    bail!(
        "native Reachy serial access is quarantined: this branch preserves an experiment, not a usable hardware backend"
    )
}

const OFFSETS: [i32; 9] = [0, 1024, -1024, 1024, -1024, 1024, -1024, 0, 0];

fn read_state(port: &mut dyn serialport::SerialPort, kine: &mut Kinematics) -> Result<[f64; 9]> {
    let mut dph = rustypot::DynamixelProtocolHandler::v2();
    let ids: Vec<u8> = (10..=18).collect();
    let data = dph
        .sync_read(port, &ids, 132, 4)
        .map_err(|e| anyhow::anyhow!("sync read present_position: {:?}", e))?;
    if data.len() != 9 {
        bail!("Failed to read from all 9 motors, only got {}", data.len());
    }

    let mut rads = [0.0; 9];
    for (i, bytes) in data.iter().enumerate() {
        let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i32;
        let actual_raw = raw - OFFSETS[i];
        rads[i] =
            (2.0 * std::f64::consts::PI * (actual_raw as f64) / 4096.0) - std::f64::consts::PI;
    }

    let body_yaw = rads[0];
    let stewart = vec![rads[1], rads[2], rads[3], rads[4], rads[5], rads[6]];
    let ant_l = rads[8]; // ID 18
    let ant_r = rads[7]; // ID 17

    let t_plat = kine.forward_kinematics(stewart, Some(-body_yaw));

    let x = t_plat[(0, 3)];
    let y = t_plat[(1, 3)];
    let z = t_plat[(2, 3)];

    let rot = t_plat.fixed_view::<3, 3>(0, 0);
    let (roll, pitch, yaw) =
        nalgebra::Rotation3::from_matrix_unchecked(rot.into_owned()).euler_angles();

    Ok([x, y, z, roll, pitch, yaw, body_yaw, ant_l, ant_r])
}

fn set_target_motor(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    head: Option<(f64, f64, f64, f64, f64, f64)>,
    antennas: Option<[f64; 2]>,
    body_yaw: Option<f64>,
) -> Result<()> {
    let current_state = read_state(port, kine)?;
    let h = head.unwrap_or((
        current_state[0],
        current_state[1],
        current_state[2],
        current_state[3],
        current_state[4],
        current_state[5],
    ));
    let by = body_yaw.unwrap_or(current_state[6]);
    let a = antennas.unwrap_or([current_state[7], current_state[8]]);

    let rot = nalgebra::Rotation3::from_euler_angles(h.3, h.4, h.5);
    let mut t_plat = rot.to_homogeneous();
    t_plat[(0, 3)] = h.0;
    t_plat[(1, 3)] = h.1;
    t_plat[(2, 3)] = h.2;

    let stewart_angles = kine.inverse_kinematics_safe(t_plat, Some(-by), None, None);

    let mut rads = vec![0.0; 9];
    rads[0] = -stewart_angles[0]; // body yaw (negated back)
    rads[1] = stewart_angles[1];
    rads[2] = stewart_angles[2];
    rads[3] = stewart_angles[3];
    rads[4] = stewart_angles[4];
    rads[5] = stewart_angles[5];
    rads[6] = stewart_angles[6];
    rads[7] = a[1]; // ID 17 is right
    rads[8] = a[0]; // ID 18 is left

    let mut dph = rustypot::DynamixelProtocolHandler::v2();
    let ids: Vec<u8> = (10..=18).collect();
    let mut byte_data = Vec::new();
    for (i, rad) in rads.iter().enumerate() {
        let mut actual_raw =
            (4096.0 * (std::f64::consts::PI + rad) / (2.0 * std::f64::consts::PI)) as i32;
        actual_raw += OFFSETS[i];
        let raw_u32 = actual_raw.clamp(0, 4095) as u32;
        byte_data.push(raw_u32.to_le_bytes().to_vec());
    }

    dph.sync_write(port, &ids, 116, &byte_data)
        .map_err(|e| anyhow::anyhow!("sync write goal_position: {:?}", e))?;
    Ok(())
}

fn goto(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    head: Option<(f64, f64, f64, f64, f64, f64)>,
    antennas: Option<[f64; 2]>,
    body_yaw: Option<f64>,
    duration: f64,
) -> Result<()> {
    let start_state = read_state(port, kine)?;
    let target = [
        head.map(|h| h.0).unwrap_or(start_state[0]),
        head.map(|h| h.1).unwrap_or(start_state[1]),
        head.map(|h| h.2).unwrap_or(start_state[2]),
        head.map(|h| h.3).unwrap_or(start_state[3]),
        head.map(|h| h.4).unwrap_or(start_state[4]),
        head.map(|h| h.5).unwrap_or(start_state[5]),
        body_yaw.unwrap_or(start_state[6]),
        antennas.map(|a| a[0]).unwrap_or(start_state[7]),
        antennas.map(|a| a[1]).unwrap_or(start_state[8]),
    ];

    if duration <= 0.05 {
        return set_target_motor(port, kine, head, antennas, body_yaw);
    }

    let steps = (duration * 50.0).max(1.0) as usize;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let ease = t * t * (3.0 - 2.0 * t);
        let mut interp = [0.0; 9];
        for j in 0..9 {
            interp[j] = start_state[j] + (target[j] - start_state[j]) * ease;
        }
        set_target_motor(
            port,
            kine,
            Some((
                interp[0], interp[1], interp[2], interp[3], interp[4], interp[5],
            )),
            Some([interp[7], interp[8]]),
            Some(interp[6]),
        )?;
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn wiggle(port: &mut dyn serialport::SerialPort, kine: &mut Kinematics) -> Result<()> {
    for _ in 0..2 {
        goto(port, kine, None, Some([0.5, -0.5]), None, 0.22)?;
        goto(port, kine, None, Some([-0.5, 0.5]), None, 0.22)?;
    }
    goto(port, kine, None, Some([0.0, 0.0]), None, 0.22)
}

fn cmd_gesture(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    name: &str,
) -> Result<()> {
    match name.to_lowercase().as_str() {
        "nod" | "yes" => {
            goto(
                port,
                kine,
                Some((0., 0., 0., 0., 0.18, 0.)),
                None,
                None,
                0.4,
            )?;
            goto(
                port,
                kine,
                Some((0., 0., 0., 0., -0.05, 0.)),
                None,
                None,
                0.4,
            )?;
            goto(port, kine, Some((0., 0., 0., 0., 0., 0.)), None, None, 0.4)?;
        }
        "shake" | "no" => {
            goto(port, kine, Some((0., 0., 0., 0., 0., 0.3)), None, None, 0.4)?;
            goto(
                port,
                kine,
                Some((0., 0., 0., 0., 0., -0.3)),
                None,
                None,
                0.5,
            )?;
            goto(port, kine, Some((0., 0., 0., 0., 0., 0.)), None, None, 0.4)?;
        }
        "wiggle" | "happy" => wiggle(port, kine)?,
        "perk" => goto(port, kine, None, Some([0.7, 0.7]), None, 0.4)?,
        "look-left" => goto(port, kine, Some((0., 0., 0., 0., 0., 0.4)), None, None, 0.6)?,
        "look-right" => goto(
            port,
            kine,
            Some((0., 0., 0., 0., 0., -0.4)),
            None,
            None,
            0.6,
        )?,
        "center" | "rest" => goto(
            port,
            kine,
            Some((0., 0., 0., 0., 0., 0.)),
            Some([0., 0.]),
            Some(0.),
            0.6,
        )?,
        _ => bail!("unknown gesture '{name}'"),
    }
    Ok(())
}

fn cmd_pose(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    daemon: &str,
) -> Result<()> {
    let s = read_state(port, kine)?;
    println!(
        "Head     : x={:.3} y={:.3} z={:.3} (metres)",
        s[0], s[1], s[2]
    );
    println!(
        "           roll={:.3} pitch={:.3} yaw={:.3} (rad)",
        s[3], s[4], s[5]
    );
    println!(
        "           roll={:.1}° pitch={:.1}° yaw={:.1}°",
        s[3].to_degrees(),
        s[4].to_degrees(),
        s[5].to_degrees()
    );
    println!("Body yaw : {:.3} rad ({:.1}°)", s[6], s[6].to_degrees());
    println!("Antennas : l={:.3} r={:.3} (rad)", s[7], s[8]);

    // Warn if daemon is dead
    if daemon_get(daemon, "/api/state/doa").is_err() {
        println!(
            "\n[Warning] Audio Daemon is unreachable at {daemon} — audio touch sensing is offline."
        );
    } else {
        println!("\nAudio Daemon is active.");
    }
    Ok(())
}

fn cmd_observe(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    daemon: &str,
    python: &str,
    frame: Option<&Path>,
    no_frame: bool,
) -> Result<()> {
    let state = read_state(port, kine)?;
    let touch = daemon_get(daemon, "/api/state/doa").ok();
    let (frame_path, fw, fh) = if no_frame {
        (None, 0u64, 0u64)
    } else {
        let p = frame
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir().join("body_observe.png"));
        let (w, h) = grab_frame(python, &p)?;
        (Some(p), w, h)
    };
    let obs = serde_json::json!({
        "t": format_time(interval_key(now_tai())),
        "frame": frame_path.as_ref().map(|p| p.display().to_string()),
        "frame_size": [fw, fh],
        "state": state,
        "state_layout": ["head_x_m","head_y_m","head_z_m","head_roll_rad","head_pitch_rad","head_yaw_rad","body_yaw_rad","antenna_l_rad","antenna_r_rad"],
        "touch": touch.map(|d| serde_json::json!({
            "doa_angle_rad": d["angle"].as_f64(),
            "doa_speech": d["speech_detected"].as_bool(),
        })),
        "raw": true,
        "note": "no resize/normalize — VLA owns preprocessing",
    });
    println!("{}", serde_json::to_string_pretty(&obs)?);
    Ok(())
}

fn grab_frame(python: &str, out_png: &Path) -> Result<(u64, u64)> {
    let shim_path = std::env::temp_dir().join("body_frame.py");
    std::fs::write(&shim_path, FRAME_SHIM).context("write frame shim")?;
    let mut child = PCommand::new(python)
        .arg(&shim_path)
        .arg(out_png)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            bail!("frame grab timed out");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("frame grab failed");
    }
    let dims = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(dims
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .unwrap_or((0, 0)))
}

fn open_repo(path: &Path) -> Result<Repository<Pile>> {
    let mut pile = Pile::open(path)?;
    if let Err(e) = pile.refresh() {
        let _ = pile.close();
        bail!("refresh pile: {:?}", e);
    }
    let signing_key = SigningKey::generate(&mut OsRng);
    Repository::new(pile, signing_key, TribleSet::new()).map_err(|e| anyhow::anyhow!("{:?}", e))
}

fn with_body<T>(
    pile: &Path,
    explicit_branch: Option<&str>,
    f: impl FnOnce(&mut Repository<Pile>, &mut Workspace<Pile>) -> Result<T>,
) -> Result<T> {
    let mut repo = open_repo(pile)?;
    let branch_id = if let Some(hex) = explicit_branch {
        Id::from_hex(hex.trim()).ok_or_else(|| anyhow::anyhow!("invalid branch id '{hex}'"))?
    } else {
        repo.ensure_branch(BODY_BRANCH_NAME, None)
            .map_err(|e| anyhow::anyhow!("{:?}", e))?
    };
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let result = f(&mut repo, &mut ws);
    let _ = repo.close();
    result
}

struct Felt {
    samples: usize,
    sweeps: usize,
    angle_min: f64,
    angle_max: f64,
    max_speed: f64,
    head_deflect: f64,
    speech_ticks: usize,
    signature_json: String,
}

impl Felt {
    fn touched(&self) -> bool {
        self.head_deflect > 0.02
    }
}

fn feel_window(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    daemon: &str,
    secs: f64,
) -> Felt {
    const SWEEP_DEG: f64 = 15.0;
    const SWEEP_WIN: f64 = 0.6;
    let start = Instant::now();
    let dur = Duration::from_secs_f64(secs);

    let mut t_series = Vec::new();
    let mut a_series = Vec::new();
    let mut speech_ticks = 0usize;
    let (mut rmin, mut rmax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut pmin, mut pmax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);

    while start.elapsed() < dur {
        let t = start.elapsed().as_secs_f64();
        if let Ok(d) = daemon_get(daemon, "/api/state/doa") {
            if let Some(a) = d["angle"].as_f64() {
                t_series.push(t);
                a_series.push(a.to_degrees());
            }
            if d["speech_detected"].as_bool().unwrap_or(false) {
                speech_ticks += 1;
            }
        }
        if let Ok(s) = read_state(port, kine) {
            let r = s[3];
            let p = s[4];
            let y = s[5];
            rmin = rmin.min(r);
            rmax = rmax.max(r);
            pmin = pmin.min(p);
            pmax = pmax.max(p);
            ymin = ymin.min(y);
            ymax = ymax.max(y);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut sweeps = 0usize;
    let mut i = 0usize;
    while i < a_series.len() {
        let t0 = t_series[i];
        let mut j = i;
        let (mut lo, mut hi) = (a_series[i], a_series[i]);
        while j < a_series.len() && t_series[j] - t0 <= SWEEP_WIN {
            lo = lo.min(a_series[j]);
            hi = hi.max(a_series[j]);
            j += 1;
        }
        if hi - lo > SWEEP_DEG {
            sweeps += 1;
            i = j;
        } else {
            i += 1;
        }
    }

    let mut max_speed = 0.0f64;
    for k in 1..a_series.len() {
        let dt = t_series[k] - t_series[k - 1];
        if dt > 0.0 {
            max_speed = max_speed.max(((a_series[k] - a_series[k - 1]) / dt).abs());
        }
    }

    let angle_min = a_series.iter().cloned().fold(f64::INFINITY, f64::min);
    let angle_max = a_series.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let head_deflect = (rmax - rmin).max(pmax - pmin).max(ymax - ymin).max(0.0);

    let signature_json = serde_json::json!({
        "modality": "touch",
        "sweeps": sweeps,
        "angle_deg": { "min": angle_min, "max": angle_max },
        "max_speed_deg_s": max_speed,
        "head_deflect_rad": head_deflect,
        "speech_ticks": speech_ticks,
        "samples": a_series.len(),
        "secs": secs,
    })
    .to_string();

    Felt {
        samples: a_series.len(),
        sweeps,
        angle_min: if angle_min.is_finite() {
            angle_min
        } else {
            0.0
        },
        angle_max: if angle_max.is_finite() {
            angle_max
        } else {
            0.0
        },
        max_speed,
        head_deflect: if head_deflect.is_finite() {
            head_deflect
        } else {
            0.0
        },
        speech_ticks,
        signature_json,
    }
}

fn report_felt(felt: &Felt) {
    println!(
        "I felt it — your hand tipped my head {:.0} mrad ({:.1}°).",
        felt.head_deflect * 1000.0,
        felt.head_deflect.to_degrees()
    );
    if felt.angle_max - felt.angle_min > 20.0 {
        println!(
            "  and I heard it move across the mics, {:.0}–{:.0}°.",
            felt.angle_min, felt.angle_max
        );
    }
}

fn keep_felt(
    repo: &mut Repository<Pile>,
    ws: &mut Workspace<Pile>,
    felt: &Felt,
    note: Option<&str>,
) -> Result<()> {
    let pose_h: TextHandle = ws.put(felt.signature_json.clone());
    let note_h: Option<TextHandle> = note
        .map(|n| ws.put(n.to_string()))
        .or_else(|| Some(ws.put("a touch on the head".to_string())));
    let frag = entity! {
        metadata::tag: &KIND_CAPTURE,
        metadata::created_at: now_tai(),
        capture::modality: "touch",
        capture::pose: pose_h,
        capture::note?: note_h,
    };
    let id = frag.root().unwrap();
    ws.commit(frag, "body feel");
    repo.push(ws).map_err(|e| anyhow::anyhow!("{:?}", e))?;
    println!("  kept it — {}", &fmt_id(id)[..12]);
    Ok(())
}

fn cmd_feel(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    repo: &mut Repository<Pile>,
    ws: &mut Workspace<Pile>,
    daemon: &str,
    secs: Option<f64>,
    loop_: bool,
    keep: bool,
    respond: bool,
    note: Option<&str>,
) -> Result<()> {
    let s = secs.unwrap_or(if loop_ { 300.0 } else { 12.0 });
    let deadline = Instant::now() + Duration::from_secs_f64(s);
    println!("Feeling for touches...");
    while Instant::now() < deadline {
        let f = feel_window(port, kine, daemon, 1.0);
        if f.touched() {
            report_felt(&f);
            if keep {
                keep_felt(repo, ws, &f, note)?;
            }
            if respond {
                wiggle(port, kine)?;
            }
        }
        if !loop_ {
            break;
        }
    }
    Ok(())
}

fn cmd_intent(
    repo: &mut Repository<Pile>,
    ws: &mut Workspace<Pile>,
    text: Option<&str>,
) -> Result<()> {
    match text {
        Some(t) => {
            let text_h: TextHandle = ws.put(t.to_string());
            let frag = entity! {
                metadata::tag: &KIND_INTENT,
                metadata::created_at: now_tai(),
                intent::text: text_h,
            };
            let id = frag.root().expect("intent id");
            ws.commit(frag, "body intent");
            repo.push(ws).map_err(|e| anyhow::anyhow!("push: {e:?}"))?;
            println!("  intent {} set: {t}", &fmt_id(id)[..12]);
        }
        None => {
            let space = ws
                .checkout(..)
                .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
            let mut best: Option<(i128, TextHandle)> = None;
            for (h, created) in find!(
                (h: TextHandle, t: Inline<inlineencodings::NsTAIInterval>),
                pattern!(&space, [{
                    _?i @
                        metadata::tag: KIND_INTENT,
                        intent::text: ?h,
                        metadata::created_at: ?t,
                }])
            ) {
                let k = interval_key(created);
                if best.as_ref().map_or(true, |(bk, _)| k > *bk) {
                    best = Some((k, h));
                }
            }
            match best {
                Some((k, h)) => {
                    let v: View<str> = ws
                        .get(h)
                        .map_err(|e| anyhow::anyhow!("read intent: {e:?}"))?;
                    eprintln!("  ({})", format_time(k));
                    println!("{}", v.as_ref());
                }
                None => println!("(no intent yet — gemma hasn't reasoned anything)"),
            }
        }
    }
    Ok(())
}

fn cmd_look(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    repo: &mut Repository<Pile>,
    ws: &mut Workspace<Pile>,
    _daemon: &str,
    python: &str,
    note: Option<&str>,
) -> Result<()> {
    let state = read_state(port, kine)?;
    let p = std::env::temp_dir().join("body_look.png");
    grab_frame(python, &p)?;
    let bytes = std::fs::read(&p)?;
    let frame_h: RawHandle = ws.put(bytes);

    let pose_json = serde_json::json!({
        "state": state,
        "state_layout": ["head_x_m","head_y_m","head_z_m","head_roll_rad","head_pitch_rad","head_yaw_rad","body_yaw_rad","antenna_l_rad","antenna_r_rad"]
    }).to_string();
    let pose_h: TextHandle = ws.put(pose_json);
    let note_h: Option<TextHandle> = note.map(|n| ws.put(n.to_string()));

    let frag = entity! {
        metadata::tag: &KIND_CAPTURE,
        metadata::created_at: now_tai(),
        capture::modality: "vision",
        capture::frame: frame_h,
        capture::pose: pose_h,
        capture::note?: note_h,
    };
    let id = frag.root().unwrap();
    ws.commit(frag, "body look");
    repo.push(ws).map_err(|e| anyhow::anyhow!("{:?}", e))?;
    println!("Captured look — {}", &fmt_id(id)[..12]);
    Ok(())
}

fn cmd_list(ws: &mut Workspace<Pile>) -> Result<()> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
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
        println!("no captures yet — `body look` keeps a frame, `body feel --keep` a touch.");
        return Ok(());
    }
    for (k, cid, modality, note) in rows {
        let when = format_time(k);
        let suffix = if note.is_empty() {
            String::new()
        } else {
            format!("  — {note}")
        };
        println!("{}  {:<6}  {when}{suffix}", &fmt_id(cid)[..12], modality);
    }
    Ok(())
}

fn cmd_get(ws: &mut Workspace<Pile>, id: &str, output: Option<&str>) -> Result<()> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
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
    .ok_or_else(|| anyhow::anyhow!("capture has no frame payload (a touch capture has no file)"))?;
    let bytes: anybytes::Bytes = ws
        .get::<anybytes::Bytes, _>(h)
        .map_err(|e| anyhow::anyhow!("get blob: {e:?}"))?;

    if output == Some("@-") {
        use std::io::Write;
        std::io::stdout()
            .write_all(bytes.as_ref())
            .context("write to stdout")?;
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

fn parse_pose(s: &str) -> Result<[f64; 9]> {
    let v: Vec<f64> = s
        .split(',')
        .map(|x| x.trim().parse::<f64>())
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("bad pose: {e}"))?;
    if v.len() != 9 {
        bail!("pose needs 9 reals");
    }
    Ok([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8]])
}

fn cmd_act(
    port: &mut dyn serialport::SerialPort,
    kine: &mut Kinematics,
    pose: &str,
    duration: f64,
    dt: f64,
    now: bool,
) -> Result<()> {
    if let Some(spec) = pose.strip_prefix('@') {
        let text = if spec == "-" {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        } else {
            std::fs::read_to_string(spec)?
        };
        let chunk: Vec<Vec<f64>> = serde_json::from_str(&text)?;
        for row in chunk {
            set_target_motor(
                port,
                kine,
                Some((row[0], row[1], row[2], row[3], row[4], row[5])),
                Some([row[7], row[8]]),
                Some(row[6]),
            )?;
            std::thread::sleep(Duration::from_secs_f64(dt));
        }
    } else {
        let p = parse_pose(pose)?;
        if now {
            set_target_motor(
                port,
                kine,
                Some((p[0], p[1], p[2], p[3], p[4], p[5])),
                Some([p[7], p[8]]),
                Some(p[6]),
            )?;
        } else {
            goto(
                port,
                kine,
                Some((p[0], p[1], p[2], p[3], p[4], p[5])),
                Some([p[7], p[8]]),
                Some(p[6]),
                duration,
            )?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let pile = cli.pile.clone();
    let branch = cli.branch_id.as_deref();
    let daemon = cli.daemon.clone();
    let python = cli.python.clone();

    // Commands that don't need motors
    if let Some(Command::List) = cli.command {
        return with_body(&pile, branch, |_repo, ws| cmd_list(ws));
    }
    if let Some(Command::Get { id, output }) = &cli.command {
        return with_body(&pile, branch, |_repo, ws| {
            cmd_get(ws, id, output.as_deref())
        });
    }
    if let Some(Command::Intent { text }) = &cli.command {
        return with_body(&pile, branch, |repo, ws| {
            cmd_intent(repo, ws, text.as_deref())
        });
    }

    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }

    let motion_requested = matches!(
        cli.command.as_ref(),
        Some(Command::Wake | Command::Sleep | Command::Gesture { .. } | Command::Act { .. })
    ) || matches!(
        cli.command.as_ref(),
        Some(Command::Feel { respond: true, .. })
    );
    if motion_requested {
        bail!(
            "native Reachy actuation is quarantined: present goals are not pinned and the coordinate/limit mapping has not been validated"
        );
    }
    if !cli.experimental_native_serial {
        bail!(
            "the unfinished native serial probe is disabled by default; stop the Reachy daemon, then pass --experimental-native-serial for read-only commands"
        );
    }
    require_native_backend_ready()?;

    let mut port = open_serial()?;
    let mut kine = init_kinematics();

    match cli.command {
        Some(Command::Pose) => cmd_pose(port.as_mut(), &mut kine, &daemon)?,
        Some(Command::Wake) => goto(
            port.as_mut(),
            &mut kine,
            Some((0.0, 0.0, 0.0, 0.0, 0.0, 0.0)),
            Some([0.0, 0.0]),
            Some(0.0),
            1.5,
        )?,
        Some(Command::Sleep) => goto(
            port.as_mut(),
            &mut kine,
            Some((0.0, 0.0, -0.05, 0.0, 0.4, 0.0)),
            Some([-0.5, 0.5]),
            Some(0.0),
            1.5,
        )?,
        Some(Command::Feel {
            secs,
            loop_,
            keep,
            respond,
            note,
        }) => with_body(&pile, branch, |repo, ws| {
            cmd_feel(
                port.as_mut(),
                &mut kine,
                repo,
                ws,
                &daemon,
                secs,
                loop_,
                keep,
                respond,
                note.as_deref(),
            )
        })?,
        Some(Command::Gesture { name }) => cmd_gesture(port.as_mut(), &mut kine, &name)?,
        Some(Command::Observe { frame, no_frame }) => cmd_observe(
            port.as_mut(),
            &mut kine,
            &daemon,
            &python,
            frame.as_deref(),
            no_frame,
        )?,
        Some(Command::Act {
            pose,
            duration,
            dt,
            now,
        }) => cmd_act(port.as_mut(), &mut kine, &pose, duration, dt, now)?,
        Some(Command::Look { note }) => with_body(&pile, branch, |repo, ws| {
            cmd_look(
                port.as_mut(),
                &mut kine,
                repo,
                ws,
                &daemon,
                &python,
                note.as_deref(),
            )
        })?,
        _ => {}
    }
    Ok(())
}
