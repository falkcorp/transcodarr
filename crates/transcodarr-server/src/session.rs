// file: crates/transcodarr-server/src/session.rs
// version: 1.0.0
// guid: 5c81a3e7-24b6-4f09-8d15-7a6c03e29b48
// last-edited: 2026-08-04
//! Registration: the server side of the handshake.
//!
//! This is where an agent asks permission, and the only place a `fencing_epoch`
//! is issued. Four gates run in order, and the order is the point — each one
//! costs less than the one after it, and each refusal is cheaper than
//! discovering the same problem once work is in flight:
//!
//! 1. **Token.** Before anything is read from the request body.
//! 2. **Version.** [`VersionGate`] on protocol and agent version. An agent too
//!    old to be trusted is turned away while it is still asking permission —
//!    not discovered halfway through a commit, where the only options left are
//!    bad ones.
//! 3. **Capability.** The document is converted through the proto boundary,
//!    which refuses any value this build does not recognise rather than
//!    defaulting it.
//! 4. **Fencing.** A new process instance takes a new epoch; a stream reconnect
//!    resumes the one it had.
//!
//! **A rejection changes nothing in the database.** It is a clean response
//! carrying a reason an operator can act on, not an error and not a partial
//! write. An agent that was refused must be in exactly the state it was in
//! before it asked, or a rejected registration becomes a way to overwrite a
//! healthy row.
//!
//! `Connect` is not implemented yet and says so. That is deliberate: a stream
//! that accepted assignments without the dispatch loop behind it would hand out
//! work nobody is accounting for, which is worse than no stream at all.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use transcodarr_core::capability::Capability;
use transcodarr_proto::handshake::{AgentIdentity, RegisterOutcome, VersionGate};
use transcodarr_proto::{MIN_SUPPORTED_PROTO, PROTO_VERSION, pb};
use transcodarr_store::repo::{AgentRegistration, AgentRepo, CommitIntentRepo};
use transcodarr_store::writer::{WriteLane, Writer};

/// How long a lease lasts before an agent must have been heard from again.
const LEASE_SECONDS: i64 = 45;

/// The server version reported to agents.
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Serves `Register`, and will serve `Connect`.
#[derive(Clone)]
pub struct AgentSession {
    agents: AgentRepo,
    intents: CommitIntentRepo,
    writer: Arc<Writer>,
    gate: VersionGate,
    /// The shared secret an agent must present, when one is configured.
    ///
    /// `None` means no token is required, which is only appropriate on a
    /// trusted network. It is `None` rather than an empty string on purpose: an
    /// empty configured token and no configured token are different intents,
    /// and conflating them turns a misconfiguration into an open door.
    auth_token: Option<String>,
}

impl std::fmt::Debug for AgentSession {
    /// Hand-written so the token cannot reach a log through a derived
    /// `Debug`. Everything else here is safe to print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("gate", &self.gate)
            .field("auth_token", &self.auth_token.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl AgentSession {
    /// Build a session over the store.
    pub fn new(
        agents: AgentRepo,
        intents: CommitIntentRepo,
        writer: Arc<Writer>,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            agents,
            intents,
            writer,
            gate: VersionGate::default(),
            auth_token,
        }
    }

    /// Override the version gate, for tests and for a staged rollout.
    pub fn with_gate(mut self, gate: VersionGate) -> Self {
        self.gate = gate;
        self
    }

    /// A refusal, as a clean response that changes nothing.
    fn refuse(reason: String) -> pb::RegisterResponse {
        pb::RegisterResponse {
            accepted: false,
            reject_reason: reason,
            server_proto_version: PROTO_VERSION,
            min_supported_proto: MIN_SUPPORTED_PROTO,
            min_agent_version: String::new(),
            server_version: SERVER_VERSION.to_string(),
            fencing_epoch: 0,
            unknown_job_ids: Vec::new(),
        }
    }

    /// Whether the presented token matches the configured one.
    ///
    /// Compared over the whole length rather than short-circuiting on the first
    /// differing byte. The timing signal from an early return is small, but the
    /// cost of not leaking it is smaller.
    fn token_ok(&self, presented: &str) -> bool {
        let Some(expected) = self.auth_token.as_deref() else {
            return true;
        };
        if expected.len() != presented.len() {
            return false;
        }
        expected
            .bytes()
            .zip(presented.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    /// Whether this agent may install its own output.
    ///
    /// Every mount must have passed the Phase 0 rename probe, not merely one of
    /// them. A node that can rename atomically on one pool and not another
    /// would otherwise be trusted to install everywhere — and the commit ritual
    /// depends on `rename(2)` being atomic on the pool it is actually writing
    /// to. `RP_UNTESTED` grants nothing: absence of a trial is not evidence.
    fn commit_eligible(mounts: &[pb::Mount]) -> bool {
        !mounts.is_empty()
            && mounts.iter().all(|m| {
                pb::RenameProbeStatus::try_from(m.rename_probe)
                    == Ok(pb::RenameProbeStatus::RpAtomicVerified)
            })
    }

    /// Job ids the agent believes it holds that this server has no live record
    /// of.
    ///
    /// The agent replays its fsynced journal before accepting work; anything
    /// here is an install it was in the middle of that the server cannot match
    /// to a live intent. Naming them back is what lets the agent clean up
    /// rather than sit on a staged file forever.
    ///
    /// `tonic::Status` is a large error type and clippy says so. Boxing it here
    /// would only move the cost: the value is returned straight out of an RPC
    /// handler that must produce a `Status` anyway, so the allocation would be
    /// undone one frame later.
    #[allow(clippy::result_large_err)]
    fn unknown_intents(&self, live: &[pb::LiveIntent]) -> Result<Vec<String>, Status> {
        let mut unknown = Vec::new();
        for intent in live {
            let known = self
                .intents
                .get(&intent.job_id)
                .map_err(|e| Status::internal(format!("commit ledger unreadable: {e}")))?;
            let live_for_path = self
                .intents
                .live_for_path(&intent.final_path)
                .map_err(|e| Status::internal(format!("commit ledger unreadable: {e}")))?;
            if known.is_none() && live_for_path.is_none() {
                unknown.push(intent.job_id.clone());
            }
        }
        Ok(unknown)
    }

    /// Apply a write and wait for it without blocking the runtime.
    async fn write(&self, op: transcodarr_store::writer::WriteOp) -> Result<(), Status> {
        let writer = self.writer.clone();
        tokio::task::spawn_blocking(move || writer.submit_blocking(WriteLane::Normal, op))
            .await
            .map_err(|e| Status::internal(format!("write task failed: {e}")))?
            .map_err(|e| Status::internal(format!("write failed: {e}")))?;
        Ok(())
    }
}

#[tonic::async_trait]
impl pb::agent_service_server::AgentService for AgentSession {
    async fn register(
        &self,
        request: Request<pb::RegisterRequest>,
    ) -> Result<Response<pb::RegisterResponse>, Status> {
        let req = request.into_inner();

        if !self.token_ok(&req.auth_token) {
            // Deliberately vague to the caller and specific in the log: an
            // agent with the wrong token learns only that it was wrong.
            tracing::warn!("registration refused: bad auth token");
            return Ok(Response::new(Self::refuse(
                "authentication failed".to_string(),
            )));
        }

        let identity: AgentIdentity = req
            .identity
            .ok_or_else(|| Status::invalid_argument("identity is required"))?
            .try_into()
            .map_err(|e| Status::invalid_argument(format!("identity: {e}")))?;

        // What the server last saw under this name. A changed agent_uid is a
        // reinstall: it answers to a name the fleet knows but is not the same
        // installation, so it must not inherit the previous one's epoch and
        // with it a work area that is not its own.
        let known = self
            .agents
            .instance_of(&identity.agent_id)
            .map_err(|e| Status::internal(format!("agent registry unreadable: {e}")))?;
        let (known_boot_id, current_epoch) = match &known {
            Some(k) if k.agent_uid == identity.agent_uid => (k.boot_id.clone(), k.fencing_epoch),
            Some(k) => (None, k.fencing_epoch),
            None => (None, 0),
        };

        let (fencing_epoch, new_instance) =
            match self
                .gate
                .evaluate(&identity, known_boot_id.as_deref(), current_epoch)
            {
                RegisterOutcome::Rejected { reason } => {
                    tracing::warn!(agent = %identity.agent_id, %reason, "registration refused");
                    return Ok(Response::new(Self::refuse(reason)));
                }
                RegisterOutcome::Accepted {
                    fencing_epoch,
                    new_instance,
                } => (fencing_epoch, new_instance),
            };

        let wire_capability = req
            .capability
            .ok_or_else(|| Status::invalid_argument("capability is required"))?;
        let mounts = wire_capability.mounts.clone();
        let physical_cores = i64::from(wire_capability.physical_cores);
        let ffmpeg_version = wire_capability.ffmpeg_version.clone();
        let ffprobe_version = wire_capability.ffprobe_version.clone();
        let driver_version = wire_capability.nvidia_driver_version.clone();

        // The boundary refuses anything it does not recognise rather than
        // defaulting it. A capability document that cannot be understood is a
        // refusal, not a partially-understood agent.
        let capability: Capability = wire_capability
            .try_into()
            .map_err(|e| Status::invalid_argument(format!("capability: {e}")))?;

        let capability_hash = capability.hash();
        let capability_json = serde_json::to_string(&capability)
            .map_err(|e| Status::internal(format!("capability not serialisable: {e}")))?;
        let classes_json = serde_json::to_string(&capability.classes)
            .map_err(|e| Status::internal(format!("classes not serialisable: {e}")))?;
        let mounts_json = serde_json::to_string(&capability.mounts)
            .map_err(|e| Status::internal(format!("mounts not serialisable: {e}")))?;

        let commit_eligible = Self::commit_eligible(&mounts);
        let rename_probe_status = if commit_eligible { "ok" } else { "untested" };

        let hash_changed = known.is_some()
            && self
                .agents
                .get(&identity.agent_id)
                .map_err(|e| Status::internal(format!("agent registry unreadable: {e}")))?
                .and_then(|a| a.capability_hash)
                // `is_none_or` would read better and is stable only since
                // 1.82; the workspace MSRV is 1.76 and clippy enforces it.
                .map_or(true, |h| h != capability_hash);

        self.write(AgentRepo::register_op(AgentRegistration {
            id: identity.agent_id.clone(),
            agent_uid: identity.agent_uid.clone(),
            boot_id: identity.boot_id.clone(),
            hostname: None,
            platform: (!capability_json.is_empty())
                .then(|| capability.platform.map(|p| format!("{p:?}")))
                .flatten(),
            arch: None,
            agent_version: identity.agent_version.clone(),
            proto_version: i64::from(identity.proto_version),
            ffmpeg_version: (!ffmpeg_version.is_empty()).then_some(ffmpeg_version.clone()),
            ffprobe_version: (!ffprobe_version.is_empty()).then_some(ffprobe_version),
            driver_version: (!driver_version.is_empty()).then_some(driver_version.clone()),
            classes_json,
            capability_json: capability_json.clone(),
            capability_hash: capability_hash.clone(),
            effective_cores: capability.effective_cores,
            physical_cores: (physical_cores > 0).then_some(physical_cores),
            mounts_json,
            rename_probe_status: rename_probe_status.to_string(),
            commit_eligible,
            fencing_epoch,
        }))
        .await?;

        // Appended only when the hash actually moved. An entry per registration
        // would bury the one that matters -- an agent whose ffmpeg was upgraded
        // under it -- beneath a row per reconnect.
        if hash_changed {
            self.write(AgentRepo::record_capability_op(
                identity.agent_id.clone(),
                capability_hash,
                capability_json,
                Some(identity.agent_version.clone()),
                (!ffmpeg_version.is_empty()).then_some(ffmpeg_version),
                (!driver_version.is_empty()).then_some(driver_version),
                "capability hash changed".to_string(),
            ))
            .await?;
        }

        self.write(AgentRepo::heartbeat_op(
            identity.agent_id.clone(),
            LEASE_SECONDS,
        ))
        .await?;

        let unknown_job_ids = self.unknown_intents(&req.live_intents)?;

        tracing::info!(
            agent = %identity.agent_id,
            epoch = fencing_epoch,
            new_instance,
            commit_eligible,
            unknown_intents = unknown_job_ids.len(),
            "agent registered"
        );

        Ok(Response::new(pb::RegisterResponse {
            accepted: true,
            reject_reason: String::new(),
            server_proto_version: PROTO_VERSION,
            min_supported_proto: MIN_SUPPORTED_PROTO,
            min_agent_version: String::new(),
            server_version: SERVER_VERSION.to_string(),
            fencing_epoch: u64::try_from(fencing_epoch).unwrap_or(0),
            unknown_job_ids,
        }))
    }

    type ConnectStream = tokio_stream::wrappers::ReceiverStream<Result<pb::ServerMessage, Status>>;

    /// Not yet implemented, and refused rather than faked.
    ///
    /// A stream that accepted assignments without the dispatch loop behind it
    /// would hand out work nothing is accounting for. An explicit refusal
    /// leaves an agent connected, registered and idle, which is recoverable;
    /// silently accepting would not be.
    async fn connect(
        &self,
        _request: Request<tonic::Streaming<pb::AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        Err(Status::unimplemented(
            "Connect is not served yet; this build accepts Register only",
        ))
    }
}
