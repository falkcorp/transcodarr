// file: crates/transcodarr-cli/src/agent.rs
// version: 1.0.0
// guid: e5a20c74-9b18-4d36-8fa1-72c604e9351d
// last-edited: 2026-08-06
//! `transcodarr agent` — run a worker, or ask it what it thinks it is.
//!
//! `connect` runs until stopped. `survey` prints the same capability document
//! without talking to anything, which is the command to reach for when a
//! dispatcher is refusing an agent work: it answers "what did this machine
//! actually advertise" without needing a server to ask.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use transcodarr_agent::ExecutorConfig;
use transcodarr_agent::run::{self, RunConfig};
use transcodarr_agent::survey::{self, MountSpec, SurveyConfig};

/// Worker-side commands.
#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    /// Connect to a server and run assigned work.
    Connect(ConnectArgs),

    /// Print what this machine would advertise, and connect to nothing.
    Survey(SurveyArgs),
}

/// How this agent identifies itself and what it offers.
#[derive(Args, Debug, Clone)]
pub struct SurveyArgs {
    /// Where to stage output. Must be on the same filesystem as the
    /// destination — an install is a `rename(2)`, which is atomic only within
    /// one filesystem.
    #[arg(long)]
    pub work_dir: PathBuf,

    /// A mount this agent offers, as `canonical_prefix=local_path`.
    ///
    /// Repeatable. The canonical prefix is what the *server* calls the pool;
    /// the local path is where this agent sees it, which differs on the Windows
    /// node.
    #[arg(long = "mount", value_name = "PREFIX=PATH")]
    pub mounts: Vec<String>,

    /// An operator label, as `key=value`. Repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub labels: Vec<String>,

    /// The ffmpeg binary.
    #[arg(long, default_value = "ffmpeg")]
    pub ffmpeg: String,

    /// The ffprobe binary.
    #[arg(long, default_value = "ffprobe")]
    pub ffprobe: String,
}

/// Connect to a server and run work.
#[derive(Args, Debug)]
pub struct ConnectArgs {
    /// The server, as a URI.
    #[arg(long, default_value = "http://127.0.0.1:7420")]
    pub server: String,

    /// The name this agent answers to. Operator-assigned and stable.
    #[arg(long)]
    pub id: String,

    /// Shared secret, if the server requires one.
    #[arg(long, env = "TRANSCODARR_TOKEN")]
    pub token: Option<String>,

    #[command(flatten)]
    pub survey: SurveyArgs,
}

/// Run an agent command.
pub fn run(cmd: AgentCommand) -> Result<()> {
    match cmd {
        AgentCommand::Survey(args) => survey_only(args),
        AgentCommand::Connect(args) => connect(args),
    }
}

/// Print the capability document this machine would register with.
fn survey_only(args: SurveyArgs) -> Result<()> {
    let capability = survey::survey(&survey_config(&args)?)?;
    println!("platform          {}", capability.platform);
    println!("effective cores   {}", capability.effective_cores);
    println!("physical cores    {}", capability.physical_cores);
    println!("classes           {}", capability.classes.join(", "));
    println!("encoders          {}", capability.encoders.join(", "));
    println!("muxers            {}", capability.muxers.join(", "));
    println!("work area free    {} bytes", capability.workarea_free_bytes);
    println!("ffmpeg            {}", capability.ffmpeg_version);
    println!("ffprobe           {}", capability.ffprobe_version);
    println!("capability hash   {}", capability.capability_hash);

    if capability.mounts.is_empty() {
        println!("\nmounts            none");
        println!(
            "\nThis agent offers no mounts, so it can be given no work: every job \
             requires a mount covering its library."
        );
    } else {
        println!("\nmounts");
        for m in &capability.mounts {
            // The verdict is what decides commit_eligible, so it is printed
            // rather than summarised: RP_UNTESTED grants nothing, and an
            // operator needs to see which mount is the one holding it back.
            println!(
                "  {:<24} {:<28} {:>12} bytes free  rename_probe={}",
                m.canonical_prefix,
                m.local_path,
                m.free_bytes,
                probe_label(m.rename_probe)
            );
        }
        if capability.mounts.iter().any(|m| {
            m.rename_probe != transcodarr_agent::pb::RenameProbeStatus::RpAtomicVerified as i32
        }) {
            println!(
                "\nNot commit-eligible: every mount must pass the rename probe. This \
                 agent can produce output but may not install it."
            );
        }
    }
    Ok(())
}

/// The wire enum, spelled for an operator.
fn probe_label(value: i32) -> &'static str {
    match transcodarr_agent::pb::RenameProbeStatus::try_from(value) {
        Ok(transcodarr_agent::pb::RenameProbeStatus::RpAtomicVerified) => "ATOMIC_VERIFIED",
        Ok(transcodarr_agent::pb::RenameProbeStatus::RpNotAtomic) => "NOT_ATOMIC",
        _ => "UNTESTED",
    }
}

/// Connect and run until stopped.
fn connect(args: ConnectArgs) -> Result<()> {
    // Same rule as `serve`: an explicitly empty token is a misconfiguration,
    // not a decision to authenticate with nothing.
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
        // and the symptom — "the dispatcher never gives it anything" — points
        // nowhere near the typo that caused it.
        let (prefix, local) = raw
            .split_once('=')
            .with_context(|| format!("--mount {raw} is not PREFIX=PATH"))?;
        if prefix.is_empty() || local.is_empty() {
            bail!("--mount {raw} has an empty prefix or path");
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(mounts: Vec<&str>) -> SurveyArgs {
        SurveyArgs {
            work_dir: PathBuf::from("/w"),
            mounts: mounts.into_iter().map(String::from).collect(),
            labels: Vec::new(),
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
        }
    }

    #[test]
    fn a_mount_is_parsed_into_its_two_halves() {
        let cfg = survey_config(&args(vec!["/mnt/media=/media"])).unwrap();
        assert_eq!(cfg.mounts.len(), 1);
        assert_eq!(cfg.mounts[0].canonical_prefix, "/mnt/media");
        assert_eq!(cfg.mounts[0].local_path, "/media");
    }

    /// A Windows agent sees the pool at a drive letter, and the path therefore
    /// contains no `=` but plenty else. Splitting on the *first* `=` is what
    /// keeps that working.
    #[test]
    fn a_windows_local_path_survives_parsing() {
        let cfg = survey_config(&args(vec!["/mnt/media=Z:\\media"])).unwrap();
        assert_eq!(cfg.mounts[0].local_path, "Z:\\media");
    }

    /// Refused, not skipped: an agent silently missing a mount is ineligible
    /// for the work it was deployed for, and the symptom points nowhere near
    /// the cause.
    #[test]
    fn a_malformed_mount_is_refused() {
        assert!(survey_config(&args(vec!["/mnt/media"])).is_err());
        assert!(survey_config(&args(vec!["=/media"])).is_err());
        assert!(survey_config(&args(vec!["/mnt/media="])).is_err());
    }
}
