// file: crates/transcodarr-cli/src/serve.rs
// version: 1.1.0
// guid: b71e3d95-04c8-42fa-9e63-8d51a072c4b1
// last-edited: 2026-08-06
//! `transcodarr serve` — run the orchestrator.
//!
//! Argument parsing and nothing else. The wiring lives in
//! `transcodarr-server::serve`, so that no SQL, no `rusqlite` type and no
//! repository appears in this crate — the same layering rule that keeps
//! `admin` calling `Runtime` rather than the store.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;

use transcodarr_server::Runtime;
use transcodarr_server::capacity::AgentLimits;
use transcodarr_server::serve::{self, ServeConfig};

/// Run the orchestrator: serve the agent protocol.
#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Database file.
    #[arg(long, default_value = "transcodarr.db")]
    pub db: PathBuf,

    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:7420")]
    pub listen: SocketAddr,

    /// Shared secret agents must present.
    ///
    /// Omit to run without authentication, which is only appropriate on a
    /// trusted network and is warned about at startup.
    #[arg(long, env = "TRANSCODARR_TOKEN")]
    pub token: Option<String>,

    /// How many jobs one agent may run at once.
    #[arg(long, default_value_t = 4)]
    pub slots: u32,

    /// How many of those may be large files.
    ///
    /// Capped separately because the pool is latency-bound: 47 concurrent
    /// 40-80 GB jobs produced per-file ETAs of 3 to 34 hours.
    #[arg(long, default_value_t = 1)]
    pub large_slots: u32,

    /// Seconds between dispatch passes.
    #[arg(long, default_value_t = 5)]
    pub tick_seconds: u64,
}

/// Run the server until it is signalled to stop.
pub fn run(args: ServeArgs) -> Result<()> {
    // An explicitly empty token is a misconfiguration, not a decision to run
    // open: an operator who typed `--token ""` or whose environment expanded to
    // nothing meant to set one. Treating it as "no authentication" would open
    // the port precisely when someone believed they had closed it.
    if args.token.as_deref() == Some("") {
        bail!("--token was set to an empty string; omit it entirely to run without a token");
    }

    let runtime = Runtime::open(&args.db)
        .with_context(|| format!("opening the database at {}", args.db.display()))?;

    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;

    tokio.block_on(serve::run(
        &runtime,
        ServeConfig {
            listen: args.listen,
            auth_token: args.token,
            tick: Duration::from_secs(args.tick_seconds.max(1)),
            limits: AgentLimits::flat(args.slots, args.large_slots),
            ..ServeConfig::default()
        },
    ))?;
    Ok(())
}
