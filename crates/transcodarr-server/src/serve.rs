// file: crates/transcodarr-server/src/serve.rs
// version: 1.0.0
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

use tonic::transport::Server;

use transcodarr_proto::pb;
use transcodarr_store::repo::{AgentRepo, CommitIntentRepo, JobRepo};

use crate::ServerError;
use crate::fleet::AgentTable;
use crate::runtime::Runtime;
use crate::session::AgentSession;

/// How to run the server.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// Where to listen.
    pub listen: SocketAddr,
    /// The shared secret agents must present, when one is required.
    pub auth_token: Option<String>,
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
        JobRepo::new(pool),
        Arc::clone(runtime.writer()),
        config.auth_token.clone(),
    )
    .with_fleet(fleet.clone());

    Serving { fleet, session }
}

/// Serve until the process is asked to stop.
///
/// Shutdown is graceful on SIGINT: in-flight RPCs finish, and every agent's
/// stream ends cleanly rather than being cut. An agent whose stream is cut
/// mid-commit still recovers — that is what the journal is for — but it costs a
/// reconnect and a lease timeout to discover something a clean close says
/// immediately.
pub async fn run(runtime: &Runtime, config: ServeConfig) -> Result<(), ServerError> {
    let serving = build(runtime, &config);
    serve_with(serving.session, config).await
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
                auth_token: None,
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
