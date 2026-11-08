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
    /// Needs a GB10, the complete WeMM root in the pile's models collection,
    /// and WEMM_ROOT naming it.
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

fn main() -> Result<()> {
    let Cli { command } = Cli::parse();
    match command {
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
    use faculties::{files::Files, memory::Memory, wiki::Wiki};

    let storage = faculties::storage::Storage::shared(pile.clone(), key);
    let memory = Memory::with_storage(storage.clone());
    let wiki = Wiki::with_storage(storage.clone());
    let files = Files::with_storage(storage.clone());
    let session = faculties::wemm::Session::from_env(&pile)?;
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
