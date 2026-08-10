// file: crates/transcodarr-agent/src/run.rs
// version: 1.1.0
// guid: c4f7092b-8d31-4e56-a1b0-95d283c74e6f
// last-edited: 2026-08-10
//! Starting an agent: survey, open the work area, connect.
//!
//! The assembly order is the safety argument, and it is the reverse of what is
//! convenient:
//!
//! 1. **Open the work area and the journal first.** Before the network, before
//!    the survey. If a previous process died mid-install, the record of it is on
//!    disk right now, and every later step is only safe once we can see it.
//! 2. **Survey the machine.** Measured, not assumed — see [`crate::survey`].
//! 3. **Then connect.** [`ConnectClient`] replays the journal at `Register` and
//!    resolves what comes back before it opens the stream, so no assignment can
//!    arrive while an unaccounted-for install is still on disk.
//!
//! Doing 3 before 1 would mean an agent accepting work with an unresolved
//! intent underneath it, which is how a file gets installed over its own
//! half-finished replacement.
//!
//! ## The work area is per library, and this takes one
//!
//! `rename(2)` is atomic only within a filesystem, so the work area must sit on
//! the same pool as the destination — decision D14. One agent serving libraries
//! on two pools therefore needs two work areas, which the assignment protocol
//! does not yet express: `JobAssignment` names a `temp_path` the server chose.
//! So this takes a single `--work-dir` and refuses an assignment whose
//! destination is not on it, rather than silently staging somewhere that cannot
//! be installed from.

use std::path::PathBuf;
use std::sync::Arc;

use crate::AgentError;
use crate::client::{ClientConfig, ConnectClient, Shutdown};
use crate::commit::CommitRitual;
use crate::executor::{Executor, ExecutorConfig};
use crate::identity;
use crate::survey::{self, SurveyConfig};
use crate::workarea::WorkArea;
use crate::worker::LocalWorker;

/// Everything needed to run an agent.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The server to connect to, as a URI.
    pub server: String,
    /// The operator-assigned name this agent answers to.
    pub agent_id: String,
    /// Where to stage output. Must be on the destination pool.
    pub work_dir: PathBuf,
    /// The shared secret, when the server requires one.
    pub auth_token: Option<String>,
    /// How to survey this machine.
    pub survey: SurveyConfig,
    /// Which binaries to run.
    pub executor: ExecutorConfig,
}

/// A running agent, and the handle that stops it.
pub struct Running {
    /// Stops the client at the next boundary.
    pub shutdown: Shutdown,
    /// The worker, for inspection.
    pub worker: Arc<LocalWorker>,
}

/// Build the worker and the client, without connecting.
///
/// Separated from [`run`] so a caller can inspect what was surveyed — and what
/// the journal still holds — before anything talks to a server.
pub fn prepare(
    config: &RunConfig,
) -> Result<(Arc<LocalWorker>, ConnectClient<LocalWorker>), AgentError> {
    // The work area and journal first: whatever a previous process left behind
    // is on disk now, and nothing else is safe until it can be read.
    let work = WorkArea::open(
        &config.work_dir,
        &identity::agent_uid(),
        identity::boot_id(),
    )?;
    let journal = work.open_journal()?;
    let outstanding = journal.outstanding()?;
    if !outstanding.is_empty() {
        tracing::warn!(
            count = outstanding.len(),
            "installs were in flight when this agent last stopped; they will be \
             replayed at registration and resolved before any work is accepted"
        );
    }

    let capability = survey::survey(&config.survey)?;
    tracing::info!(
        classes = ?capability.classes,
        encoders = capability.encoders.len(),
        effective_cores = capability.effective_cores,
        mounts = capability.mounts.len(),
        "surveyed this machine"
    );

    let worker = Arc::new(LocalWorker::new(
        Executor::new(config.executor.clone()),
        CommitRitual::new(journal, work.clone()),
        work,
        capability,
    ));

    let mut client_config = ClientConfig::new(config.server.clone(), config.agent_id.clone());
    client_config.auth_token = config.auth_token.clone();

    Ok((
        worker.clone(),
        ConnectClient::new(client_config, worker.clone()),
    ))
}

/// Run an agent until it is told to stop.
///
/// Returns only on shutdown. Every failure in between — the server being down,
/// a refused registration, a dropped stream — is retried with backoff, because
/// an agent that gives up needs an operator to notice and restart it, and the
/// thing it was waiting for is usually a server that came back two minutes
/// later.
pub async fn run(config: RunConfig) -> Result<(), AgentError> {
    let (worker, client) = prepare(&config)?;
    tracing::info!(
        agent = %config.agent_id,
        server = %config.server,
        boot_id = identity::boot_id(),
        work_dir = %config.work_dir.display(),
        "agent starting"
    );
    let _ = worker;
    client.run().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Worker;
    use tempfile::TempDir;

    fn config(dir: &std::path::Path) -> RunConfig {
        RunConfig {
            server: "http://127.0.0.1:1".into(),
            agent_id: "u1".into(),
            work_dir: dir.join("work"),
            auth_token: None,
            survey: SurveyConfig {
                ffmpeg: "ffmpeg".into(),
                ffprobe: "ffprobe".into(),
                work_dir: dir.join("work").display().to_string(),
                mounts: Vec::new(),
                labels: Vec::new(),
                transport: transcodarr_core::capability::TransportMode::Mount,
            },
            executor: ExecutorConfig::default(),
        }
    }

    /// Preparing must not need a server: an agent that could only inspect
    /// itself while connected would be undiagnosable in exactly the situation
    /// where you need to diagnose it.
    #[test]
    fn an_agent_can_be_prepared_without_a_server() {
        let d = TempDir::new().unwrap();
        let (worker, client) = prepare(&config(d.path())).unwrap();
        assert_eq!(client.fencing_epoch(), 0, "no epoch until it registers");
        assert!(worker.running_job_ids().is_empty());
    }

    /// The journal has to be readable before anything else happens, so it is
    /// created with the work area rather than on first use.
    #[test]
    fn preparing_creates_the_work_area_and_journal() {
        let d = TempDir::new().unwrap();
        let cfg = config(d.path());
        let (worker, _) = prepare(&cfg).unwrap();
        assert!(cfg.work_dir.is_dir());
        assert!(worker.journal().dir().is_dir());
        assert!(worker.journal().outstanding().unwrap().is_empty());
    }
}
