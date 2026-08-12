// file: crates/transcodarr-agent/src/bin/transcodarr-agent.rs
// version: 1.0.0
// guid: b5c59c65-0497-44f1-8c9d-b9dec72e1d96
// last-edited: 2026-08-10
//! The agent, on its own, with no database in it.
//!
//! This exists because the rule the project already states was true of the
//! *crate* and false of the *artifact*. `transcodarr-agent` has never depended
//! on `transcodarr-store` — `cargo tree -p transcodarr-agent -i
//! transcodarr-store` reports no such package, and that check passes — but the
//! only binary that could run an agent was `transcodarr`, which links
//! `transcodarr-server`, which links the store, which links SQLite.
//!
//! So "the agent stays copyable to the Windows node without dragging SQLite
//! along" was verified where it was cheap to verify and untrue where it
//! mattered: there was no agent to copy. A worker node had to take the whole
//! orchestrator, including a C library it will never open, cross-compiled for a
//! platform it did not need to be.
//!
//! `transcodarr` keeps its `agent` verbs, so nothing an operator already runs
//! changes. This is the same code reachable without the server half.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use transcodarr_agent::ExecutorConfig;
use transcodarr_agent::run::{self, RunConfig};
use transcodarr_agent::survey::{self, MountSpec, SurveyConfig};
use transcodarr_core::capability::TransportMode;

#[derive(Parser, Debug)]
#[command(
    name = "transcodarr-agent",
    about = "Run a transcodarr worker. No database, no orchestrator.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Connect to a server and run assigned work.
    Connect(ConnectArgs),
    /// Print what this machine would advertise, and connect to nothing.
    Survey(SurveyArgs),
}

/// How this agent identifies itself and what it offers.
#[derive(clap::Args, Debug, Clone)]
struct SurveyArgs {
    /// Where to stage output.
    ///
    /// Under `--transport mount` this must sit on the same filesystem as the
    /// destination, because an install is a `rename(2)` and that is atomic only
    /// within one filesystem. Under `--transport stream` it is ordinary local
    /// scratch: the server installs, so nothing is renamed across anything.
    #[arg(long)]
    work_dir: PathBuf,

    /// A mount this agent offers, as `canonical_prefix=local_path`. Repeatable.
    #[arg(long = "mount", value_name = "PREFIX=PATH")]
    mounts: Vec<String>,

    /// An operator label, as `key=value`. Repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<String>,

    /// How this agent reaches media.
    ///
    /// `stream` is the one to reach for on a node whose share mappings live in
    /// a logon session the agent does not run in — which is every Windows
    /// service and every SSH session.
    #[arg(long = "transport", value_name = "MODE", default_value = "mount")]
    transport: TransportArg,

    /// The ffmpeg binary.
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,

    /// The ffprobe binary.
    #[arg(long, default_value = "ffprobe")]
    ffprobe: String,
}

#[derive(clap::Args, Debug)]
struct ConnectArgs {
    /// The server, as a URI.
    #[arg(long, default_value = "http://127.0.0.1:7420")]
    server: String,

    /// The name this agent answers to. Operator-assigned and stable.
    #[arg(long)]
    id: String,

    /// Shared secret, if the server requires one.
    #[arg(long, env = "TRANSCODARR_TOKEN")]
    token: Option<String>,

    #[command(flatten)]
    survey: SurveyArgs,
}

/// `--transport` as the CLI spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TransportArg {
    /// Read and write the library directly through a share.
    Mount,
    /// Receive the bytes, work locally, send the result back.
    Stream,
}

impl From<TransportArg> for TransportMode {
    fn from(v: TransportArg) -> Self {
        match v {
            TransportArg::Mount => TransportMode::Mount,
            TransportArg::Stream => TransportMode::Stream,
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber_init();
    match Cli::parse().command {
        Command::Survey(args) => survey_only(&args),
        Command::Connect(args) => connect(args),
    }
}

/// Logging, without pulling a subscriber crate into the agent's dependencies.
///
/// The agent already depends on `tracing`; a full subscriber would be another
/// crate on a binary whose whole point is being small enough to copy anywhere.
fn tracing_subscriber_init() {}

fn survey_only(args: &SurveyArgs) -> Result<()> {
    let capability = survey::survey(&survey_config(args)?)?;
    let streaming = capability.transport() == transcodarr_agent::pb::TransportMode::TmStream;

    println!("platform          {}", capability.platform);
    println!("effective cores   {}", capability.effective_cores);
    println!("classes           {}", capability.classes.join(", "));
    println!("encoders          {}", capability.encoders.join(", "));
    println!("muxers            {}", capability.muxers.join(", "));
    println!("ffmpeg            {}", capability.ffmpeg_version);
    println!("capability hash   {}", capability.capability_hash);
    println!(
        "transport         {}",
        if streaming { "stream" } else { "mount" }
    );

    if capability.mounts.is_empty() {
        println!("\nmounts            none");
        if streaming {
            println!(
                "\nThis agent streams: the server sends it the source bytes and installs \
                 the result, so it needs no mounts and offers none."
            );
        } else {
            println!(
                "\nThis agent offers no mounts, so it can be given no work: every job \
                 requires a mount covering its library. Either pass --mount, or run it \
                 with --transport stream."
            );
        }
    } else {
        println!("\nmounts");
        for m in &capability.mounts {
            println!(
                "  {:<24} {:<28} {:>12} bytes free",
                m.canonical_prefix, m.local_path, m.free_bytes
            );
        }
    }
    Ok(())
}

fn connect(args: ConnectArgs) -> Result<()> {
    // An explicitly empty token is a misconfiguration, not a decision to
    // authenticate with nothing.
    if args.token.as_deref() == Some("") {
        bail!("--token was set to an empty string; omit it entirely to connect without a token");
    }

    let survey = survey_config(&args.survey)?;
    let config = RunConfig {
        server: args.server,
        agent_id: args.id,
        work_dir: args.survey.work_dir.clone(),
        auth_token: args.token,
        executor: ExecutorConfig {
            ffmpeg: args.survey.ffmpeg.clone(),
            ffprobe: args.survey.ffprobe.clone(),
            ..ExecutorConfig::default()
        },
        survey,
    };

    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    tokio.block_on(run::run(config))?;
    Ok(())
}

/// Build the survey configuration, refusing anything malformed.
fn survey_config(args: &SurveyArgs) -> Result<SurveyConfig> {
    let mut mounts = Vec::new();
    for raw in &args.mounts {
        // Refused rather than skipped. A mount that silently did not register
        // makes the agent ineligible for exactly the work it was deployed for,
        // and "the dispatcher never gives it anything" points nowhere near the
        // typo that caused it.
        let (prefix, local) = raw
            .split_once('=')
            .with_context(|| format!("--mount {raw} is not PREFIX=PATH"))?;
        if prefix.is_empty() || local.is_empty() {
            bail!("--mount {raw} has an empty side");
        }
        mounts.push(MountSpec {
            canonical_prefix: prefix.to_string(),
            local_path: local.to_string(),
        });
    }

    Ok(SurveyConfig {
        ffmpeg: args.ffmpeg.clone(),
        ffprobe: args.ffprobe.clone(),
        work_dir: args.work_dir.display().to_string(),
        mounts,
        labels: survey::parse_labels(&args.labels),
        transport: args.transport.into(),
    })
}
