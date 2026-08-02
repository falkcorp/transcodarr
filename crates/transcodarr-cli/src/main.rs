// file: crates/transcodarr-cli/src/main.rs
// version: 1.0.0
// guid: 0f9e8d7c-6b5a-4c3d-2e1f-0a9b8c7d6e5f
// last-edited: 2026-08-01
//! `transcodarr` command-line entry point.
//!
//! The binary is one executable with several faces — `local` today, with
//! `server`, `agent` and `admin` to follow. Only `local` exists so far; the
//! grouping is introduced now so the surface is stable before the orchestrator
//! lands behind it.

mod local;

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

fn main() -> Result<()> {
    let cli = Cli::parse_from(rewrite_legacy_verbs(std::env::args().collect()));
    match cli.command {
        Commands::Local(cmd) => local::run(cmd),
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
