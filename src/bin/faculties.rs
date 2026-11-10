//! Native aggregate entrypoint: the MCP server, over stdio or HTTP, and (in
//! a wemm build) the maintainer of the derived collections only this host can
//! compute.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use faculties::hear;
use faculties::mcp::catalog::{Catalog, Config as CatalogConfig};
use faculties::mcp::http;
use faculties::mcp::{self, Faculty, Limits};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version = faculties::GIT_VERSION, about = "Native Faculties frontends")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Discover configured workspaces without selecting one or starting services.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Serve native faculty tools over stdio, or opt into authenticated HTTP.
    Mcp {
        /// Pile configured by the local launcher, never an MCP tool argument.
        #[arg(long, env = "PILE")]
        pile: PathBuf,
        /// Existing durable signing-key path, configured by the local launcher.
        #[arg(long, env = "TRIBLESPACE_KEY")]
        key: Option<PathBuf>,
        /// Optional LinkedIn DMA bearer credential, owned by the launcher.
        #[arg(long, env = "LINKEDIN_TOKEN", hide_env_values = true)]
        linkedin_token: Option<String>,
        /// Optional existing Duplex session directory, never a tool input.
        #[arg(long, env = "DUPLEX_SESSION")]
        duplex_session: Option<PathBuf>,
        /// Hear model pile; configuring Hear requires both JSON assets below.
        #[arg(long, env = "HEAR_MODEL_PILE", requires_all = ["hear_config_json", "hear_tokenizer_json"])]
        hear_model_pile: Option<PathBuf>,
        #[arg(long, env = "HEAR_MODEL", default_value = hear::DEFAULT_MODEL)]
        hear_model: String,
        /// Local Gemma configuration; never downloaded during discovery.
        #[arg(long, env = "HEAR_CONFIG_JSON", requires = "hear_model_pile")]
        hear_config_json: Option<PathBuf>,
        #[arg(long, env = "HEAR_TOKENIZER_JSON", requires = "hear_model_pile")]
        hear_tokenizer_json: Option<PathBuf>,
        /// World server base URL for the world tools (default localhost:8080).
        #[arg(long, env = "WORLD_URL")]
        world_url: Option<String>,
        /// Session file shared with the `world` CLI; in memory when unset.
        #[arg(long, env = "WORLD_SESSION_FILE")]
        world_session_file: Option<PathBuf>,
        /// Bind a Streamable HTTP listener instead of using stdio.
        #[arg(long, value_name = "ADDR", requires = "http_token_file")]
        http_listen: Option<SocketAddr>,
        /// Read the HTTP bearer credential from this launcher-owned file.
        #[arg(
            long,
            env = "FACULTIES_MCP_TOKEN_FILE",
            hide_env_values = true,
            value_name = "PATH",
            requires = "http_listen"
        )]
        http_token_file: Option<PathBuf>,
        /// Allow this exact browser origin; repeat for additional origins.
        #[arg(long, value_name = "URL", requires = "http_listen")]
        http_origin: Vec<String>,
    },
    /// Keep the WeMM indexes of Memory, Wiki and Files derived: bind the model
    /// once, derive every commit that has no rows, and again whenever the pile
    /// grows. Prints a line for an index only when a pass derived something
    /// or left something missing. Exits 0 when a compaction replaces the
    /// pile file, so its supervisor restarts it on the new one.
    /// Needs a GB10, the complete WeMM root in the models collection
    /// TRIBLESPACE_COLLECTION_MODELS names (the root is looked for there and
    /// nowhere else), and WEMM_ROOT naming it. WEMM_PROFILE chooses the
    /// kernel profile: bf16 (unset, the default) from a BF16 root, or w4a4
    /// (the decoder's projections as NVFP4 W4A4) from a root that stores
    /// them as NVFP4. An index's identity is the collection, the root and
    /// the profile, so a process derives only the indexes of the three it
    /// binds, two daemons configured differently keep two sets of indexes,
    /// and a query reads the indexes of its own configuration. WEMM_COPY_WEIGHTS=1 copies the weights into
    /// device memory instead of reading them from the pile in place; it is
    /// refused unless host MemAvailable covers the weights plus
    /// WEMM_COPY_MIN_FREE_BYTES (default 16 GiB), a margin that must also
    /// cover CubeCL's pooling and the copy's transient host staging (about
    /// 4.7 GiB for WeMM-9B), so 0 can overshoot MemAvailable.
    /// WEMM_PROBE_DIR=<dir> turns on a probe self-check that logs agreement
    /// or disagreement of a fixed batch (faculties::wemm::probe). Copying and
    /// the probe are read once at startup and are off when unset.
    #[cfg(feature = "wemm")]
    Maintain {
        #[arg(long, env = "PILE")]
        pile: PathBuf,
        #[arg(long, env = "TRIBLESPACE_KEY")]
        key: Option<PathBuf>,
        /// How often to look whether the pile has grown. The working pile
        /// grows every few seconds, and a pass holds a shared pile lock while
        /// it opens three collections, so this also bounds how often a pass
        /// can run.
        #[arg(long, value_name = "SECONDS", default_value_t = 60)]
        poll: u64,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// List exact current revisions, routes and unavailable entries.
    List {
        /// Existing node pile; workspace discovery never creates one.
        #[arg(long, env = "PILE")]
        pile: PathBuf,
        /// Existing durable signing-key path, separate from workspace selection.
        #[arg(long, env = "TRIBLESPACE_KEY")]
        key: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let Cli { command } = Cli::parse();
    run(command)
}

fn run(command: Command) -> Result<()> {
    match command {
        Command::Workspace {
            command: WorkspaceCommand::List { pile, key },
        } => faculties::cli::with_output("workspace", |out| {
            faculties::cli_workspace::list(&pile, key.as_deref(), out)
        }),
        Command::Mcp {
            pile,
            key,
            linkedin_token,
            duplex_session,
            hear_model_pile,
            hear_model,
            hear_config_json,
            hear_tokenizer_json,
            world_url,
            world_session_file,
            http_listen,
            http_token_file,
            http_origin,
        } => {
            anyhow::ensure!(
                std::env::var_os("FACULTIES_WORKSPACE").is_none(),
                "standalone `faculties mcp` does not support workspace selection; unset FACULTIES_WORKSPACE for legacy mode, or use Erlkonig's explicitly routed integration"
            );
            let hear = match (hear_model_pile, hear_config_json, hear_tokenizer_json) {
                (None, None, None) => None,
                (Some(pile), Some(config_json), Some(tokenizer_json)) => Some(hear::ModelConfig {
                    pile,
                    model: hear_model,
                    config_json,
                    tokenizer_json,
                }),
                _ => anyhow::bail!(
                    "Hear requires its model pile, configuration JSON, and tokenizer JSON together"
                ),
            };
            let catalog = Catalog::new(CatalogConfig {
                pile,
                key,
                linkedin_token,
                duplex_session,
                hear,
                world_url,
                world_session: world_session_file,
            });
            let result = match http_listen {
                Some(bind) => {
                    let token_file = http_token_file
                        .as_deref()
                        .context("HTTP requires --http-token-file")?;
                    let token = http::BearerToken::from_file(token_file)?;
                    let mut config = http::Config::new(bind, token);
                    config.allowed_origins = http_origin;
                    detach_handler_stdio()?;
                    http::serve(&catalog.registrations(), config)
                }
                None => serve_stdio(&catalog.sync_registrations()),
            };
            catalog.finish(result)
        }
        #[cfg(feature = "wemm")]
        Command::Maintain { pile, key, poll } => maintain(pile, key, poll),
    }
}

/// The derived collections whose mapping only a bound model computes. The
/// CPU maintenance daemon skips them (`computable_here` is false there), so
/// this process is what keeps them current; no faculty has an index command.
#[cfg(feature = "wemm")]
fn maintain(pile: PathBuf, key: Option<PathBuf>, poll: u64) -> Result<()> {
    anyhow::ensure!(
        std::env::var_os("FACULTIES_WORKSPACE").is_none(),
        "faculties maintain does not support workspace selection; unset FACULTIES_WORKSPACE to use its explicit model bootstrap"
    );
    use faculties::{files::Files, memory::Memory, wiki::Wiki};

    let storage = faculties::storage::Storage::shared(pile.clone(), key);
    let memory = Memory::with_storage(storage.clone());
    let wiki = Wiki::with_storage(storage.clone());
    let files = Files::with_storage(storage.clone());
    let session = faculties::wemm::Session::from_env(&pile)?;
    // Off unless WEMM_PROBE_DIR names a directory (faculties::wemm::probe).
    session.start_probe_from_env()?;
    let result = faculties::pile_watch::run_until_replaced(
        &pile,
        session.backing(),
        std::time::Duration::from_secs(poll),
        || {
            // A pass that found everything derived says nothing: the pile
            // grows all day, and most passes are such passes.
            let report = |name: &str, derived: faculties::wemm::Derived| {
                if derived.derived > 0 || derived.missing > 0 {
                    eprintln!("maintain: {name} {derived}");
                }
            };
            report("memory", memory.derive_wemm(&session).context("memory")?);
            report("wiki", wiki.derive_wemm(&session).context("wiki")?);
            report("files", files.derive_wemm(&session).context("files")?);
            Ok(())
        },
    );
    if result.is_ok() {
        eprintln!(
            "maintain: {} was replaced; exiting so a fresh bind maps the new file",
            pile.display()
        );
    }
    let closed = storage.close();
    result.and(closed)
}

/// Take ownership of the process transport before handlers or their children
/// run. Ordinary stdin is EOF and ordinary stdout is diagnostic stderr: neither
/// a stray print nor a /dev/std{in,out} file path may consume/corrupt JSON-RPC.
/// This is stdio hygiene, not a filesystem sandbox against arbitrary fd access.
#[cfg(unix)]
fn serve_stdio(registrations: &[&(dyn Faculty + Sync)]) -> Result<()> {
    use std::fs::File;
    use std::io::BufReader;
    use std::os::fd::AsFd;

    // OwnedFd clones are close-on-exec; subprocesses must not inherit the
    // private protocol descriptors and keep a disconnected transport alive.
    let input = File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let output = File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    detach_handler_stdio()?;
    mcp::serve(
        registrations,
        Limits::default(),
        BufReader::new(input),
        output,
    )
}

/// Both executable transports isolate handlers from launcher input before
/// starting threads. The reusable library API does not change process fds.
#[cfg(unix)]
fn detach_handler_stdio() -> Result<()> {
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let empty = File::open("/dev/null").context("open empty handler stdin")?;
    std::io::stdout().flush()?;
    for (from, to) in [
        (empty.as_raw_fd(), libc::STDIN_FILENO),
        (libc::STDERR_FILENO, libc::STDOUT_FILENO),
    ] {
        // SAFETY: called once from main before any handler/runtime is started.
        // The source descriptors stay open through both calls; dup2 changes
        // only the process's standard streams, not our owned transport clones.
        if unsafe { libc::dup2(from, to) } == -1 {
            return Err(std::io::Error::last_os_error()).context("detach handler stdio");
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn detach_handler_stdio() -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn serve_stdio(registrations: &[&(dyn Faculty + Sync)]) -> Result<()> {
    mcp::serve(
        registrations,
        Limits::default(),
        std::io::stdin().lock(),
        std::io::stdout(),
    )
}

#[cfg(test)]
mod workspace_tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn discovery_has_bootstrap_but_no_selection_argument() {
        let command = Cli::command();
        let list = command
            .find_subcommand("workspace")
            .unwrap()
            .find_subcommand("list")
            .unwrap();
        assert!(list.get_arguments().any(|arg| arg.get_id() == "pile"));
        assert!(list.get_arguments().any(|arg| arg.get_id() == "key"));
        assert!(!list.get_arguments().any(|arg| arg.get_id() == "workspace"));
        let parsed = Cli::try_parse_from([
            "faculties",
            "workspace",
            "list",
            "--pile",
            "/unopened/node.pile",
            "--key",
            "/unopened/node.key",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Command::Workspace {
                command: WorkspaceCommand::List { .. },
            }
        ));
    }

    #[test]
    fn workspace_help_does_not_require_bootstrap_or_start_mcp() {
        let error = Cli::try_parse_from(["faculties", "workspace", "list", "--help"])
            .err()
            .expect("help exits parsing");
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    fn launcher_child(case: &str, selector: &std::ffi::OsStr) -> std::process::Output {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_tests::launcher_guard_child",
                "--nocapture",
            ])
            .env_clear()
            .env("FACULTIES_MCP_GUARD_TEST", case)
            .env("FACULTIES_WORKSPACE", selector)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    #[test]
    fn inherited_selector_presence_is_refused_before_bootstrap_or_transport() {
        let selectors = [
            std::ffi::OsString::from(format!("{:X}", triblespace::prelude::genid().id)),
            std::ffi::OsString::from("not-a-workspace"),
            std::ffi::OsString::new(),
        ];
        for selector in selectors {
            let output = launcher_child("mcp", &selector);
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let output = launcher_child("mcp", &std::ffi::OsString::from_vec(vec![0xff]));
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn inherited_selector_does_not_affect_help_version_or_workspace_listing() {
        for case in ["information", "list", "explicit"] {
            let output = launcher_child(case, std::ffi::OsStr::new("deliberately-invalid"));
            assert!(
                output.status.success(),
                "{case}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if case == "list" {
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("No configured workspaces")
                );
            }
        }
    }

    #[test]
    fn launcher_guard_child() {
        let Ok(case) = std::env::var("FACULTIES_MCP_GUARD_TEST") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("node.pile");
        let key = directory.path().join("node.key");
        match case.as_str() {
            "mcp" => {
                // Missing token prevents binding a listener even if the guard
                // regresses. No child ever starts an MCP server or network.
                let token = directory.path().join("missing.token");
                let cli = Cli::try_parse_from([
                    "faculties",
                    "mcp",
                    "--pile",
                    pile.to_str().unwrap(),
                    "--key",
                    key.to_str().unwrap(),
                    "--http-listen",
                    "127.0.0.1:0",
                    "--http-token-file",
                    token.to_str().unwrap(),
                ])
                .unwrap();
                let error = run(cli.command).unwrap_err();
                assert!(error
                    .to_string()
                    .contains("standalone `faculties mcp` does not support workspace selection"));
                assert!(!pile.exists());
                assert!(!key.exists());
                assert!(!token.exists());
            }
            "information" => {
                for args in [
                    vec!["faculties", "--help"],
                    vec!["faculties", "--version"],
                    vec!["faculties", "mcp", "--help"],
                    vec!["faculties", "workspace", "list", "--help"],
                ] {
                    let error = Cli::try_parse_from(args)
                        .err()
                        .expect("information exits parsing");
                    assert!(matches!(
                        error.kind(),
                        clap::error::ErrorKind::DisplayHelp
                            | clap::error::ErrorKind::DisplayVersion
                    ));
                }
                assert!(!pile.exists());
                assert!(!key.exists());
            }
            "list" => {
                std::fs::File::create(&pile).unwrap();
                faculties::storage::initialize_signer(&pile, Some(&key)).unwrap();
                let before = std::fs::read(&pile).unwrap();
                let cli = Cli::try_parse_from([
                    "faculties",
                    "workspace",
                    "list",
                    "--pile",
                    pile.to_str().unwrap(),
                    "--key",
                    key.to_str().unwrap(),
                ])
                .unwrap();
                run(cli.command).unwrap();
                assert_eq!(before, std::fs::read(&pile).unwrap());
            }
            "explicit" => {
                let selector = format!("{:X}", triblespace::prelude::genid().id);
                let error = Cli::try_parse_from([
                    "faculties",
                    "mcp",
                    "--pile",
                    pile.to_str().unwrap(),
                    "--workspace",
                    &selector,
                ])
                .err()
                .expect("MCP has no selector argument");
                assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
                assert!(!pile.exists());
                assert!(!key.exists());
            }
            _ => panic!("unknown guard fixture"),
        }
    }
}
