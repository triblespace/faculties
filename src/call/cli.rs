//! `call` command line: `join` runs the resident line, `say` queues for it.
use super::{Line, Session};
use anyhow::{anyhow, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::num::NonZeroU64;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "call",
    about = "A resident voice line in a Discord voice channel: `join` runs it, `say` speaks on it"
)]
pub struct Cli {
    /// Session directory shared by `join` and `say`: queued lines wait in
    /// `say/`, spoken ones are kept in `said/`.
    #[arg(long, env = "CALL_SESSION", default_value_os_t = Session::default_root())]
    session: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Join a voice channel and speak queued lines until stopped. Runs in the
    /// foreground; run it under a user unit so it outlives the shell.
    Join {
        /// Discord bot token, or @path to read it from a file.
        #[arg(long, env = "DISCORD_TOKEN", hide_env_values = true)]
        token: String,
        /// Guild (server) id.
        #[arg(long)]
        guild: NonZeroU64,
        /// Voice channel id.
        #[arg(long)]
        channel: NonZeroU64,
        /// Text channel to tell when the line is open (a bot cannot ring).
        #[arg(long)]
        announce: Option<NonZeroU64>,
        /// What to post in the announce channel.
        #[arg(long, default_value = "I'm in voice.")]
        greeting: String,
    },
    /// Queue one line for the resident process to speak.
    Say {
        /// What to say.
        text: String,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    let session = Session::new(cli.session);
    match command {
        Command::Join {
            token,
            guild,
            channel,
            announce,
            greeting,
        } => {
            let token = match token.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path)
                    .with_context(|| format!("read the Discord token from {path}"))?,
                None => token,
            };
            let token = token.trim().to_owned();
            if token.is_empty() {
                return Err(anyhow!("empty Discord token"));
            }
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("start the async runtime")?
                .block_on(super::run(Line {
                    token,
                    guild,
                    channel,
                    announce,
                    greeting,
                    session,
                }))
        }
        Command::Say { text } => crate::cli::with_output("call", |out| {
            let queued = session.enqueue(&text)?;
            out.line(format!("queued {}", queued.display()))
        }),
    }
}
