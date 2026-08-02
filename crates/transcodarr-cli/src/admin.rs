// file: crates/transcodarr-cli/src/admin.rs
// version: 1.0.0
// guid: 5b8e1a37-c204-4d69-9f52-a03e7b64c81d
// last-edited: 2026-08-02
//! The `admin` subcommand: operator diagnostics.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Subcommand;
use transcodarr_agent::preflight;

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Diagnose this machine's fitness to run transcodarr.
    Diagnose {
        /// Run the environment preflight probes.
        #[arg(long)]
        preflight: bool,
        /// Directory to exercise rename and ZFS probes against.
        #[arg(long, default_value = ".")]
        work_dir: PathBuf,
        /// Directory the database would live in, for the fsync probe.
        #[arg(long)]
        db_dir: Option<PathBuf>,
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

/// Dispatch an `admin` subcommand.
pub fn run(cmd: AdminCommand) -> Result<()> {
    match cmd {
        AdminCommand::Diagnose {
            preflight: run_preflight,
            work_dir,
            db_dir,
            json,
        } => {
            if !run_preflight {
                bail!("nothing to do; pass --preflight");
            }
            let db_dir = db_dir.unwrap_or_else(|| work_dir.clone());
            let report = preflight::run_all(&work_dir, &db_dir);

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render());
                println!();
                println!(
                    "commit eligible: {}",
                    if report.commit_eligible() {
                        "yes"
                    } else {
                        "NO - this machine may produce output but must not install it"
                    }
                );
            }

            // A failed probe is a failed command. Preflight exists to gate
            // deployment, and a gate that always exits 0 gates nothing.
            if report.any_failed() {
                bail!("preflight failed; see the table above");
            }
            Ok(())
        }
    }
}
