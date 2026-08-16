// file: crates/transcodarr-server/src/serve.rs
// version: 1.1.0
// guid: 9e04c7b3-52d1-4a86-b70f-13c85fa62094
// last-edited: 2026-08-06
//! Running the server: the gRPC endpoint agents connect to.
//!
//! Everything an agent needs on the other end of `ConnectClient` already
//! existed — `AgentSession` serves `Register` and `Connect`, `AgentTable` holds
//! the live streams — and nothing started it. This does.
//!
//! ## The fleet table is passed in, not made here
//!
//! [`AgentSession`] would happily build its own [`AgentTable`], and then the
//! dispatch loop would hold a different one: it would see an empty fleet
//! forever and dispatch nothing, with every part in perfect working order. So
//! the table is created by the caller and handed to both.
//!
//! ## An unset token is not an empty token
//!
//! `--token` absent means no authentication, which is only appropriate on a
//! trusted network and is logged loudly at startup. `--token ""` is a
//! misconfiguration, not a decision to run open, and is refused rather than
//! quietly treated as the former.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tonic::transport::Server;

use transcodarr_proto::pb;
use transcodarr_store::repo::{AgentRepo, CommitIntentRepo, FileRepo, JobRepo, LibraryRepo};

use crate::ServerError;
use crate::capacity::AgentLimits;
use crate::fleet::AgentTable;
use crate::orchestrator::{DEFAULT_TICK, Orchestrator};
use crate::runtime::Runtime;
use crate::session::AgentSession;
use transcodarr_core::policy::Policy;

/// How to run the server.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// Where to listen.
    pub listen: SocketAddr,
    /// The shared secret agents must present, when one is required.
    pub auth_token: Option<String>,
    /// How often the dispatch loop runs.
    pub tick: Duration,
    /// Per-agent concurrency.
    pub limits: AgentLimits,
    /// The policy jobs are re-derived against at dispatch.
    pub policy: Policy,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:7420".parse().expect("a valid default address"),
            auth_token: None,
            tick: DEFAULT_TICK,
            limits: AgentLimits::flat(4, 1),
            policy: transcodarr_core::policy::default_space_saver(),
        }
    }
}

/// Everything the server holds while it runs.
///
/// Returned rather than kept private so the dispatch loop can be attached to
/// the same fleet table the session writes to, and so a test can drive both.
pub struct Serving {
    /// The registry of connected agents.
    pub fleet: AgentTable,
    /// The service, ready to be added to a tonic server.
    pub session: AgentSession,
}

/// Build the agent service over an open store.
pub fn build(runtime: &Runtime, config: &ServeConfig) -> Serving {
    let pool = runtime.pool().clone();
    let fleet = AgentTable::new();
    let session = AgentSession::new(
        AgentRepo::new(pool.clone()),
        CommitIntentRepo::new(pool.clone()),
        JobRepo::new(pool.clone()),
        LibraryRepo::new(pool.clone()),
        FileRepo::new(pool),
        Arc::clone(runtime.writer()),
        config.auth_token.clone(),
    )
    .with_fleet(fleet.clone());

    Serving { fleet, session }
}

/// Serve until the process is asked to stop, running the dispatch loop
/// alongside.
///
/// Both halves share one shutdown signal, and the loop is stopped *with* the
/// server rather than after it. A dispatch pass that ran while the transport
/// was closing would place jobs on agents that can no longer be sent to, and
/// each one would have to time out its lease before anyone noticed.
///
/// Shutdown is graceful: in-flight RPCs finish and every agent's stream ends
/// cleanly rather than being cut. An agent whose stream is cut mid-commit still
/// recovers — that is what the journal is for — but it costs a reconnect and a
/// lease timeout to discover something a clean close says immediately.
pub async fn run(runtime: &Runtime, config: ServeConfig) -> Result<(), ServerError> {
    let serving = build(runtime, &config);

    let orchestrator = Orchestrator::new(
        runtime.pool().clone(),
        Arc::clone(runtime.writer()),
        // The same table the session writes to. Two would each work perfectly
        // and see different fleets.
        serving.fleet.clone(),
        config.policy.clone(),
        config.limits.clone(),
    );

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let tick = config.tick;
    let loop_handle = tokio::spawn(async move {
        let mut stop = stop_rx;
        orchestrator
            .run(tick, async move {
                // `changed()` only resolves on a *change*, so a signal sent
                // before this task was scheduled would be missed; the initial
                // value is checked first.
                while !*stop.borrow() {
                    if stop.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
    });

    let result = serve_with(serving.session, config).await;

    let _ = stop_tx.send(true);
    if let Err(e) = loop_handle.await {
        tracing::warn!(error = %e, "the dispatch loop did not stop cleanly");
    }
    result
}

/// Serve a prepared session. Split out so the dispatch loop can be started
/// against the same fleet table before the server begins accepting.
pub async fn serve_with(session: AgentSession, config: ServeConfig) -> Result<(), ServerError> {
    if config.auth_token.is_none() {
        tracing::warn!(
            "no --token configured: any agent that can reach this port may register. \
             This is only appropriate on a trusted network."
        );
    }

    tracing::info!(listen = %config.listen, "serving the agent protocol");
    Server::builder()
        .add_service(pb::agent_service_server::AgentServiceServer::new(session))
        .serve_with_shutdown(config.listen, shutdown_signal())
        .await
        .map_err(|e| ServerError::Io(std::io::Error::other(e)))?;

    tracing::info!("server stopped");
    Ok(())
}

/// Resolves when the process is asked to stop.
///
/// Both SIGINT and SIGTERM: an operator types the first and a service manager
/// sends the second, and a server that only honours one of them gets killed
/// mid-write by whichever it ignored.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM; only SIGINT will stop this");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received; draining"),
            _ = term.recv() => tracing::info!("SIGTERM received; draining"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("interrupt received; draining");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The session and the dispatch loop must share one table. Two would each
    /// work perfectly and see different fleets — the loop dispatching nothing,
    /// forever, with nothing obviously broken.
    #[test]
    fn the_session_and_the_caller_share_one_fleet_table() {
        let d = TempDir::new().unwrap();
        let rt = Runtime::open_unchecked(&d.path().join("t.db")).unwrap();
        let serving = build(
            &rt,
            &ServeConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                ..ServeConfig::default()
            },
        );

        let _rx = serving.fleet.connect("u1", 1);
        assert_eq!(
            serving.session.fleet().len(),
            1,
            "the session must see what the caller registered"
        );
    }
}
