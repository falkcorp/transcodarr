// file: crates/transcodarr-cli/src/main.rs
// version: 1.1.0
// guid: 0f9e8d7c-6b5a-4c3d-2e1f-0a9b8c7d6e5f
// last-edited: 2026-08-06
//! `transcodarr` command-line entry point.
//!
//! The binary is one executable with several faces: `local` for a standalone
//! transcode, `admin` for operator work, `serve` for the orchestrator and
//! `agent` for a worker.
//!
//! Every face is argument parsing and nothing else. The work lives in
//! `transcodarr-server` and `transcodarr-agent`, which is what keeps SQL, the
//! store's connection lifetimes and the single-writer rule out of a crate whose
//! job is to read a command line.

mod admin;
mod agent;
mod local;
mod serve;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Transcode media while preserving metadata (ffmpeg wrapper)",
    long_about = None,
    // The legacy verbs still work, so top-level help has to say so. Hiding them
    // behind `local` without a signpost would be a silent usability regression
    // for anyone with them in muscle memory or in a script.
    after_help = "Compatibility:\n  The verbs `info`, `transcode` and `batch` are \
accepted at the top level and\n  behave identically to `local <verb>`. \
`transcodarr transcode in.mp4` and\n  `transcodarr local transcode in.mp4` are the same command."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a transcode on this machine, with no server and no orchestration.
    ///
    /// This is the escape hatch used when the orchestrator itself is broken —
    /// which is exactly when you least want it to depend on the orchestrator.
    #[command(subcommand)]
    Local(local::LocalCommand),

    /// Operator diagnostics and maintenance.
    #[command(subcommand)]
    Admin(admin::AdminCommand),

    /// Run the orchestrator.
    Serve(serve::ServeArgs),

    /// Run a worker, or ask it what it thinks it is.
    #[command(subcommand)]
    Agent(agent::AgentCommand),
}

/// Verbs that sat at the top level before the `local` grouping existed.
const LEGACY_VERBS: &[&str] = &["info", "transcode", "batch"];

/// Rewrite `transcodarr <verb> ...` into `transcodarr local <verb> ...`.
///
/// Argument compatibility is a hard constraint, not a courtesy: these verbs are
/// in muscle memory and in scripts. Doing the rewrite before clap ever sees the
/// arguments keeps exactly one definition of each verb, instead of duplicating
/// them at two levels of the command tree where they could drift apart.
fn rewrite_legacy_verbs(mut args: Vec<String>) -> Vec<String> {
    if let Some(first) = args.get(1) {
        if LEGACY_VERBS.contains(&first.as_str()) {
            args.insert(1, "local".to_string());
        }
    }
    args
}

/// Start logging for the long-running verbs.
///
/// Only `serve` and `agent`, and deliberately: `local` and `admin` print their
/// results to stdout and are read by a person or a script, so emitting log
/// lines into the same stream would corrupt output that something is parsing.
/// A server, by contrast, has nowhere else to say anything — and one that logs
/// nothing is indistinguishable from one that is not running.
fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("TRANSCODARR_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

fn main() -> Result<()> {
    let cli = Cli::parse_from(rewrite_legacy_verbs(std::env::args().collect()));
    if matches!(cli.command, Commands::Serve(_) | Commands::Agent(_)) {
        init_logging();
    }
    match cli.command {
        Commands::Local(cmd) => local::run(cmd),
        Commands::Admin(cmd) => admin::run(cmd),
        Commands::Serve(args) => serve::run(args),
        Commands::Agent(cmd) => agent::run(cmd),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_verbs_are_rewritten_to_local() {
        for verb in LEGACY_VERBS {
            let got = rewrite_legacy_verbs(vec![
                "transcodarr".into(),
                (*verb).into(),
                "file.mkv".into(),
            ]);
            assert_eq!(
                got,
                vec!["transcodarr", "local", *verb, "file.mkv"],
                "{verb} should be rewritten"
            );
        }
    }

    #[test]
    fn an_explicit_local_is_left_alone() {
        let got = rewrite_legacy_verbs(vec![
            "transcodarr".into(),
            "local".into(),
            "info".into(),
            "file.mkv".into(),
        ]);
        assert_eq!(got, vec!["transcodarr", "local", "info", "file.mkv"]);
    }

    #[test]
    fn flags_and_empty_argv_are_untouched() {
        let got = rewrite_legacy_verbs(vec!["transcodarr".into(), "--help".into()]);
        assert_eq!(got, vec!["transcodarr", "--help"]);
        assert_eq!(rewrite_legacy_verbs(vec!["transcodarr".into()]).len(), 1);
    }
}
