// file: crates/transcodarr-server/src/session.rs
// version: 1.4.0
// guid: 5c81a3e7-24b6-4f09-8d15-7a6c03e29b48
// last-edited: 2026-08-16
//! Registration and the agent stream: the server side of the transport.
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
//! ## How `Connect` knows who is calling
//!
//! The `AgentMessage` envelope carries no identity — every variant of it is a
//! message *about* work, not about the sender. So the stream is identified by
//! request metadata, `x-agent-id` and `x-agent-epoch`, set once when the stream
//! opens.
//!
//! Adding a `Hello` message to the schema would also work and was not done:
//! `agent.proto` is the reviewed agreement between both ends, and a field
//! serving the transport's convenience does not belong in it. Metadata is the
//! layer this actually lives at.
//!
//! The epoch in that metadata is checked against the stored one and must match
//! exactly. A stream opened under a superseded epoch belongs to a process
//! instance the server has already replaced, and letting it reconnect would
//! hand a revoked instance a live channel.
//!
//! ## What the stream will not do yet
//!
//! No `JobAssignment` is ever sent, because there is no dispatch loop to decide
//! one. Everything here is the half that must be right *before* work can be
//! handed out: fencing on every commit, revoking work the server does not
//! recognise, and lease bookkeeping. An agent connects, is accounted for, and
//! sits idle.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use transcodarr_core::capability::Capability;
use transcodarr_core::job::JobState;
use transcodarr_proto::handshake::{AgentIdentity, RegisterOutcome, VersionGate};
use transcodarr_proto::{MIN_SUPPORTED_PROTO, PROTO_VERSION, pb};
use transcodarr_store::repo::{
    AgentRegistration, AgentRepo, CommitIntentRepo, FileRepo, JobRepo, LibraryRepo,
};

use crate::hardening::{RetryDecision, decide_retry};
use transcodarr_core::failure::FailureClass;
use transcodarr_store::writer::{WriteLane, Writer};

use crate::fleet::AgentTable;

/// How long a lease lasts before an agent must have been heard from again.
const LEASE_SECONDS: i64 = 45;

/// The server version reported to agents.
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Metadata key carrying the operator-assigned agent name on `Connect`.
const AGENT_ID_KEY: &str = "x-agent-id";

/// Metadata key carrying the epoch the stream authenticated under.
const AGENT_EPOCH_KEY: &str = "x-agent-epoch";

/// Job states in which an agent is legitimately holding work.
///
/// Anything an agent claims to be running that is not in one of these is
/// revoked. A survivor of a lost connection must not keep going: the server has
/// already accounted for that slot as free, and two encodes writing the same
/// output is the one outcome the whole ledger exists to prevent.
const HELD_STATES: [JobState; 4] = [
    JobState::Assigned,
    JobState::Running,
    JobState::Verifying,
    JobState::Committing,
];

/// Serves `Register` and `Connect`.
#[derive(Clone)]
pub struct AgentSession {
    agents: AgentRepo,
    intents: CommitIntentRepo,
    jobs: JobRepo,
    libraries: LibraryRepo,
    /// Resolves a job's file to the path streaming reads from.
    files: FileRepo,
    fleet: AgentTable,
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
        jobs: JobRepo,
        libraries: LibraryRepo,
        files: FileRepo,
        writer: Arc<Writer>,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            agents,
            intents,
            jobs,
            libraries,
            files,
            fleet: AgentTable::new(),
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

    /// Share a fleet table with the dispatch loop that will read it.
    pub fn with_fleet(mut self, fleet: AgentTable) -> Self {
        self.fleet = fleet;
        self
    }

    /// The registry of connected agents.
    pub fn fleet(&self) -> &AgentTable {
        &self.fleet
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
                .live_for_job(&intent.job_id)
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
    // ------------------------------------------------- TM_STREAM transport --
    //
    // `FetchSource` serves bytes. `PushOutput` does not yet, and refuses
    // explicitly rather than returning an accepted-but-ignored push, because
    // that would look like success to a caller and produce a job that reports
    // done having installed nothing.
    //
    // A `TM_MOUNT` agent never reaches either.

    type FetchSourceStream = tokio_stream::wrappers::ReceiverStream<Result<pb::FileChunk, Status>>;

    /// Serve a held job's source bytes to the agent holding it.
    ///
    /// ## Who is asking
    ///
    /// `FetchSourceRequest` carries no `agent_id`, deliberately — the same
    /// argument as `Connect` (see the module docs): identity is a property of
    /// the transport, not a field in the reviewed schema. So the caller is
    /// named by `x-agent-id` metadata, and an unstamped request is refused
    /// rather than served to whoever asked.
    ///
    /// ## Why two epoch checks and not one
    ///
    /// The epoch in the body is the agent's claim, rejected if stale exactly as
    /// a `CommitReport` is. But an epoch that merely matches the registry only
    /// proves the caller is a current instance of *some* agent — so the job row
    /// is checked too, and it must name this caller. Without that second gate,
    /// any live agent that learned a `job_id` could pull another agent's source.
    ///
    /// A refusal is a `Status`, never an empty stream. An empty-but-successful
    /// stream is indistinguishable from a zero-byte file to the receiver, which
    /// would turn "you may not have this" into "this file is empty".
    async fn fetch_source(
        &self,
        request: Request<pb::FetchSourceRequest>,
    ) -> Result<Response<Self::FetchSourceStream>, Status> {
        let (agent_id, stream_epoch) = stream_identity(request.metadata())?;
        let req = request.into_inner();

        let claimed = i64::try_from(req.fencing_epoch)
            .map_err(|_| Status::unauthenticated("fencing_epoch is out of range"))?;

        // The two epochs come from the same place on a sane client. A
        // disagreement means a confused caller, and guessing which one it meant
        // is how a fence gets applied to the wrong instance.
        if claimed != stream_epoch {
            return Err(Status::unauthenticated(format!(
                "epoch {claimed} in the request disagrees with {stream_epoch} in the metadata"
            )));
        }

        let agent = self
            .agents
            .get(&agent_id)
            .map_err(|e| Status::internal(format!("agent registry unreadable: {e}")))?
            .ok_or_else(|| Status::unauthenticated("register before fetching"))?;

        if agent.status == "Quarantined" {
            return Err(Status::permission_denied("agent is quarantined"));
        }
        if claimed != agent.fencing_epoch {
            return Err(Status::unauthenticated(format!(
                "epoch {claimed} is not current ({}); register again",
                agent.fencing_epoch
            )));
        }

        let job = match self.jobs.get(&req.job_id) {
            Ok(j) => j,
            Err(transcodarr_store::StoreError::NotFound { .. }) => {
                return Err(Status::not_found(format!("no job {}", req.job_id)));
            }
            Err(e) => return Err(Status::internal(format!("job lookup failed: {e}"))),
        };

        // The same fence as `on_result`: bytes follow the job's holder, not the
        // caller's claim to be it.
        if job.agent_id.as_deref() != Some(agent_id.as_str()) || job.fencing_epoch != claimed {
            tracing::warn!(
                agent = %agent_id, job = %req.job_id, epoch = claimed,
                held_by = ?job.agent_id, job_epoch = job.fencing_epoch,
                "refusing source bytes for a job this agent does not hold"
            );
            return Err(Status::permission_denied(
                "this job is not held by you at this epoch",
            ));
        }

        // Holding the row is not the same as still holding the work.
        // `transition_op` changes `state` and leaves `agent_id` and
        // `fencing_epoch` exactly as they were, so a job that has since failed
        // or been dead-lettered still names its last holder — and the epoch
        // cannot tell the difference, because only a new `boot_id` bumps it.
        // Without this the agent that just failed a job can keep pulling its
        // source indefinitely.
        if !HELD_STATES.contains(&job.state) {
            tracing::warn!(
                agent = %agent_id, job = %req.job_id, state = ?job.state,
                "refusing source bytes for a job that is no longer live"
            );
            return Err(Status::failed_precondition(format!(
                "job is {:?}, not work you are holding",
                job.state
            )));
        }

        let file = self
            .files
            .get(job.file_id)
            .map_err(|e| Status::internal(format!("file {} unreadable: {e}", job.file_id)))?;

        tracing::info!(
            agent = %agent_id, job = %req.job_id, path = %file.canonical_path,
            "serving source bytes"
        );

        Ok(Response::new(crate::transfer::source_stream(
            req.job_id,
            req.attempt,
            std::path::PathBuf::from(file.canonical_path),
        )))
    }

    async fn push_output(
        &self,
        _request: Request<tonic::Streaming<pb::FileChunk>>,
    ) -> Result<Response<pb::PushOutputResponse>, Status> {
        Err(Status::unimplemented(
            "streaming transport is not built yet: the server cannot accept output \
             bytes or install them. Run this agent with --transport mount, or wait \
             for the PushOutput implementation.",
        ))
    }

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

    /// Hold a bidirectional stream with one registered agent.
    ///
    /// The stream is admitted only if the agent is registered, not
    /// quarantined, and presenting the epoch it currently holds. A stream
    /// opened under a superseded epoch belongs to a process instance the server
    /// has already replaced; letting it in would hand a revoked instance a live
    /// channel.
    async fn connect(
        &self,
        request: Request<tonic::Streaming<pb::AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let (agent_id, claimed_epoch) = stream_identity(request.metadata())?;

        let agent = self
            .agents
            .get(&agent_id)
            .map_err(|e| Status::internal(format!("agent registry unreadable: {e}")))?
            .ok_or_else(|| Status::unauthenticated("register before connecting"))?;

        if agent.status == "Quarantined" {
            return Err(Status::permission_denied("agent is quarantined"));
        }
        if claimed_epoch != agent.fencing_epoch {
            return Err(Status::unauthenticated(format!(
                "epoch {claimed_epoch} is not current ({}); register again",
                agent.fencing_epoch
            )));
        }

        let rx = self.fleet.connect(&agent_id, agent.fencing_epoch);
        self.write(AgentRepo::set_status_op(agent_id.clone(), "Online".into()))
            .await?;

        let session = self.clone();
        let epoch = agent.fencing_epoch;
        let inbound_agent = agent_id.clone();
        tokio::spawn(async move {
            let mut inbound = request.into_inner();
            loop {
                match inbound.message().await {
                    Ok(Some(msg)) => {
                        if let Err(e) = session.handle(&inbound_agent, epoch, msg).await {
                            tracing::warn!(agent = %inbound_agent, error = %e, "stream message failed");
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(agent = %inbound_agent, error = %e, "stream ended in error");
                        break;
                    }
                }
            }

            // Offline, not fenced. A dropped connection does not invalidate work
            // already granted -- that is what the next registration's epoch bump
            // is for -- and fencing here would kill a job running perfectly well
            // behind a network fault.
            session.fleet.disconnect(&inbound_agent, epoch);
            if let Err(e) = session
                .write(AgentRepo::set_status_op(
                    inbound_agent.clone(),
                    "Offline".into(),
                ))
                .await
            {
                tracing::warn!(agent = %inbound_agent, error = %e, "could not mark agent offline");
            }
            tracing::info!(agent = %inbound_agent, "agent disconnected");
        });

        tracing::info!(agent = %agent_id, epoch, "agent connected");
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

/// Whether the agent's validation report says the output passed.
///
/// An unreadable report is a **failure**, never a pass. The report is the only
/// evidence that the output is not the 1 KB truncated artefact the AV1/NVDEC
/// path produces, and treating "cannot tell" as "fine" installs exactly the
/// outputs the gate exists to reject.
fn validation_passed(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("passed").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Where a job goes when its commit resolves.
///
/// Always *from* `Committing`: the grant moved it there, and the state machine
/// has no edge out of `Verifying` to any terminal state. That is not an
/// accident to work around — `Committing` is precisely the window in which the
/// destination may have been touched, and a job that reached a terminal state
/// without passing through it would be one the reconciler never knew to treat
/// as ambiguous.
///
/// `installed` is the only success. Everything else left the source in place —
/// a *safe* outcome, and still a job that did not do what it was dispatched to
/// do, so it fails rather than quietly counting as done. `needs_operator` is
/// neither: nobody knows what is on disk, and the one thing that must not
/// happen is a machine deciding.
fn outcome_of(resolution: &str) -> (JobState, &'static str) {
    let to = match resolution {
        "installed" => JobState::Succeeded,
        "needs_operator" => JobState::NeedsOperator,
        _ => JobState::Failed,
    };
    (to, resolution_code(resolution))
}

/// A stable reason code for the job event ledger.
fn resolution_code(resolution: &str) -> &'static str {
    match resolution {
        "installed" => "installed",
        "source_intact" => "not_installed",
        "source_restored" => "restored",
        "needs_operator" => "ambiguous_commit",
        _ => "unknown_resolution",
    }
}

/// The last 500 characters, for an operator-facing message.
fn tail(s: &str) -> String {
    if s.len() <= 500 {
        return s.to_string();
    }
    s.chars()
        .skip(s.chars().count().saturating_sub(500))
        .collect()
}

/// Read the agent's identity from the stream's metadata.
///
/// See the module documentation for why this is not a message in the schema.
///
/// The `Status` error is large and clippy says so; boxing it would only move
/// the cost, since the value is returned straight out of an RPC handler that
/// must produce a `Status` anyway.
#[allow(clippy::result_large_err)]
fn stream_identity(md: &tonic::metadata::MetadataMap) -> Result<(String, i64), Status> {
    let agent_id = md
        .get(AGENT_ID_KEY)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Status::unauthenticated(format!("{AGENT_ID_KEY} is required")))?
        .to_string();

    let epoch = md
        .get(AGENT_EPOCH_KEY)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| Status::unauthenticated(format!("{AGENT_EPOCH_KEY} must be an integer")))?;

    Ok((agent_id, epoch))
}

impl AgentSession {
    /// Dispatch one inbound message.
    async fn handle(
        &self,
        agent_id: &str,
        epoch: i64,
        msg: pb::AgentMessage,
    ) -> Result<(), Status> {
        let Some(body) = msg.body else {
            return Ok(()); // an empty envelope from a newer peer: nothing to do
        };

        match body {
            pb::agent_message::Body::Heartbeat(hb) => self.on_heartbeat(agent_id, epoch, hb).await,
            pb::agent_message::Body::CommitRequest(req) => {
                self.on_commit_request(agent_id, epoch, req).await
            }
            pb::agent_message::Body::CommitReport(rep) => {
                self.on_commit_report(agent_id, epoch, rep).await
            }
            pb::agent_message::Body::Progress(p) => {
                // Lossy by design: progress is a display concern, and the
                // metrics that consume it arrive in Phase 6.
                tracing::trace!(agent = %agent_id, job = %p.job_id, out_time_us = p.out_time_us, "progress");
                Ok(())
            }
            pb::agent_message::Body::Result(r) => self.on_result(agent_id, epoch, r).await,
            pb::agent_message::Body::DrainAck(ack) => {
                tracing::info!(agent = %agent_id, still_running = ack.still_running.len(), "drain acknowledged");
                Ok(())
            }
        }
    }

    /// A heartbeat: extend the lease, and revoke anything unrecognised.
    ///
    /// The running set is the interesting half. A job this agent claims to be
    /// running that the server does not have assigned to it, under this epoch,
    /// in a state where work is legitimately held, is a survivor of a lost
    /// connection. The server has already accounted for that slot as free, and
    /// two encodes writing one output is what the whole ledger exists to
    /// prevent — so it is revoked rather than adopted.
    async fn on_heartbeat(
        &self,
        agent_id: &str,
        epoch: i64,
        hb: pb::Heartbeat,
    ) -> Result<(), Status> {
        self.fleet.set_running(agent_id, hb.running_job_ids.clone());
        self.write(AgentRepo::heartbeat_op(agent_id.to_string(), LEASE_SECONDS))
            .await?;

        for job_id in &hb.running_job_ids {
            let recognised = match self.jobs.get(job_id) {
                Ok(job) => {
                    job.agent_id.as_deref() == Some(agent_id)
                        && job.fencing_epoch == epoch
                        && HELD_STATES.contains(&job.state)
                }
                // A job the server has never heard of is emphatically not
                // recognised. An unreadable store is a different matter and is
                // reported rather than answered with a revoke.
                Err(transcodarr_store::StoreError::NotFound { .. }) => false,
                Err(e) => return Err(Status::internal(format!("job lookup failed: {e}"))),
            };

            if !recognised {
                tracing::warn!(agent = %agent_id, job = %job_id, "revoking unrecognised running job");
                self.fleet.send(
                    agent_id,
                    pb::ServerMessage {
                        body: Some(pb::server_message::Body::Revoke(pb::Revoke {
                            job_id: job_id.clone(),
                            reason: "the server has no record of this job on this agent".into(),
                        })),
                    },
                );
            }
        }
        Ok(())
    }

    /// An agent reporting how an encode ended.
    ///
    /// This moves the job to `Verifying` or to `Failed`, and it is the only
    /// place that reads the agent's validation verdict. The verdict itself is
    /// produced by `transcodarr-core::validate` on the agent — the same code
    /// this crate links — so it is a re-run of the server's own rules on the
    /// machine that has the file, not a second opinion from a second
    /// implementation.
    ///
    /// **A failure releases the commit intent.** The intent is what reserves
    /// the destination path; a job that failed while holding one blocks its own
    /// file forever, because the next attempt's intent collides with the unique
    /// index over live intents and can never be written.
    async fn on_result(&self, agent_id: &str, epoch: i64, r: pb::JobResult) -> Result<(), Status> {
        let job = match self.jobs.get(&r.job_id) {
            Ok(j) => j,
            Err(transcodarr_store::StoreError::NotFound { .. }) => {
                tracing::warn!(agent = %agent_id, job = %r.job_id, "result for an unknown job");
                return Ok(());
            }
            Err(e) => return Err(Status::internal(format!("job lookup failed: {e}"))),
        };

        // The same fence as everywhere else. A result from an instance the
        // server has already replaced describes work that was revoked, and
        // acting on it would let a superseded agent finish a job that has since
        // been handed to somebody else.
        if job.agent_id.as_deref() != Some(agent_id) || job.fencing_epoch != epoch {
            tracing::warn!(
                agent = %agent_id, job = %r.job_id, epoch,
                held_by = ?job.agent_id, job_epoch = job.fencing_epoch,
                "ignoring a result from an agent or epoch this job is not held by"
            );
            return Ok(());
        }

        let passed = r.exit_code == 0 && validation_passed(&r.validation_json);

        // Assigned -> Running is not a no-op even though the encode is over:
        // the state machine has no Assigned -> Verifying edge, and inventing
        // one would let a job reach Verifying without ever having been recorded
        // as started.
        if job.state == JobState::Assigned {
            self.write(JobRepo::transition_op(
                r.job_id.clone(),
                JobState::Assigned,
                JobState::Running,
                None,
                None,
            ))
            .await?;
        }

        if passed {
            self.write(JobRepo::transition_op(
                r.job_id.clone(),
                JobState::Running,
                JobState::Verifying,
                None,
                None,
            ))
            .await?;
            tracing::info!(agent = %agent_id, job = %r.job_id, bytes = r.output_bytes,
                "encode accepted; awaiting the commit request");
            return Ok(());
        }

        tracing::warn!(
            agent = %agent_id, job = %r.job_id, exit_code = r.exit_code,
            stderr = %tail(&r.stderr_tail), "encode rejected"
        );
        self.release_intent(&r.job_id, job.attempt).await;

        // Through `Retrying`, not straight to `Failed`. A rejected output is
        // usually the file or the plan and sometimes the machine — a full disk,
        // an OOM kill, an ffmpeg that died on a bad sector read. Sending the
        // first rejection to a terminal state throws the job away over what may
        // have been one bad afternoon on one node, and `Failed` cannot be
        // transitioned out of. The dispatch loop owns the budget from there:
        // it picks the job up in `Eligible` and stops when `max_attempts` is
        // spent.
        self.write(JobRepo::transition_op(
            r.job_id.clone(),
            JobState::Running,
            JobState::Retrying,
            Some("validation_failed".into()),
            Some(tail(&r.stderr_tail)),
        ))
        .await?;

        let decision = decide_retry(FailureClass::Transient, job.attempt, job.max_attempts);
        let (to, code) = match decision {
            RetryDecision::RetryAfter { .. } => {
                self.write(JobRepo::bump_attempt_op(r.job_id.clone()))
                    .await?;
                (JobState::Eligible, None)
            }
            RetryDecision::DeadLetter { reason } => {
                tracing::error!(job = %r.job_id, %reason, "no attempts left");
                (
                    JobState::DeadLettered,
                    Some("attempts_exhausted".to_string()),
                )
            }
            RetryDecision::Permanent { reason } => {
                tracing::error!(job = %r.job_id, %reason, "not retryable");
                (JobState::Failed, Some("permanent".to_string()))
            }
        };
        self.write(JobRepo::transition_op(
            r.job_id.clone(),
            JobState::Retrying,
            to,
            code,
            None,
        ))
        .await?;
        Ok(())
    }

    /// Release the destination a job was holding.
    ///
    /// Best-effort and logged rather than fatal: the alternative is refusing to
    /// record that a job failed because the tidy-up failed, which leaves the
    /// job in flight as well as the intent.
    async fn release_intent(&self, job_id: &str, attempt: i64) {
        if let Err(e) = self
            .write(CommitIntentRepo::resolve_op(
                format!("{job_id}:{attempt}"),
                "abandoned".to_string(),
            ))
            .await
        {
            tracing::error!(job = %job_id, error = %e,
                "could not release the commit intent; this destination stays reserved");
        }
    }

    /// An agent asking permission to install.
    ///
    /// Granted only against a live intent the server itself recorded, held by
    /// this agent under the current epoch. Everything else is refused with a
    /// reason: permission to replace a file is not something to infer from the
    /// asking.
    async fn on_commit_request(
        &self,
        agent_id: &str,
        epoch: i64,
        req: pb::CommitRequest,
    ) -> Result<(), Status> {
        let (granted, reason, trash_path) = self.judge_commit(agent_id, epoch, &req)?;
        if !granted {
            tracing::warn!(agent = %agent_id, job = %req.job_id, %reason, "commit refused");
        }

        // Moved before the grant is sent, not after the report comes back.
        // `Committing` is the state the reconciler refuses to guess about, and
        // the window it must cover starts the moment the agent is told it may
        // touch the destination -- not when it tells us it did. There is no
        // Verifying -> Succeeded edge for exactly this reason.
        //
        // **A failure here withdraws the grant.** Permission that could not be
        // written down is permission the server cannot account for: the agent
        // would replace a file the reconciler still believes nobody is
        // touching, and would then be reclaimed mid-ritual by a requeue.
        let (granted, reason, trash_path) = if granted {
            match self
                .write(JobRepo::transition_op(
                    req.job_id.clone(),
                    JobState::Verifying,
                    JobState::Committing,
                    None,
                    None,
                ))
                .await
            {
                Ok(()) => (true, reason, trash_path),
                Err(e) => {
                    tracing::error!(job = %req.job_id, error = %e,
                        "could not record the commit; refusing rather than granting blind");
                    (
                        false,
                        "the server could not record this commit".to_string(),
                        String::new(),
                    )
                }
            }
        } else {
            (granted, reason, trash_path)
        };
        self.fleet.send(
            agent_id,
            pb::ServerMessage {
                body: Some(pb::server_message::Body::CommitGrant(pb::CommitGrant {
                    job_id: req.job_id,
                    granted,
                    reason,
                    trash_path,
                })),
            },
        );
        Ok(())
    }

    /// The decision behind a commit grant, separated so it can be tested
    /// without a stream.
    #[allow(clippy::result_large_err)]
    fn judge_commit(
        &self,
        agent_id: &str,
        epoch: i64,
        req: &pb::CommitRequest,
    ) -> Result<(bool, String, String), Status> {
        if i64::try_from(req.fencing_epoch).unwrap_or(-1) != epoch {
            return Ok((
                false,
                format!(
                    "epoch {} is not the one this stream holds ({epoch})",
                    req.fencing_epoch
                ),
                String::new(),
            ));
        }

        let intent = self
            .intents
            .live_for_job(&req.job_id)
            .map_err(|e| Status::internal(format!("commit ledger unreadable: {e}")))?;

        let Some(intent) = intent else {
            return Ok((
                false,
                "no live commit intent for this job".to_string(),
                String::new(),
            ));
        };

        if intent.state != "live" {
            return Ok((false, format!("intent is {}", intent.state), String::new()));
        }
        if intent.agent_id != agent_id {
            return Ok((
                false,
                format!("intent belongs to {}", intent.agent_id),
                String::new(),
            ));
        }
        if intent.fencing_epoch != epoch {
            return Ok((
                false,
                format!("intent was granted under epoch {}", intent.fencing_epoch),
                String::new(),
            ));
        }

        // Where the original goes -- NOT the destination. Handing back
        // `final_path` here would have the ritual rename the original onto
        // itself (a silent no-op) and then overwrite it with the replacement:
        // the file the trash exists to preserve, destroyed by the step meant to
        // preserve it. The path below the library root is kept, so two shows
        // with the same episode name do not collide in the trash.
        let job = self
            .jobs
            .get(&req.job_id)
            .map_err(|e| Status::internal(format!("job lookup failed: {e}")))?;
        let library = self
            .libraries
            .get(&job.library_id)
            .map_err(|e| Status::internal(format!("library lookup failed: {e}")))?;

        let trash = transcodarr_core::paths::trash_path_for(
            std::path::Path::new(&library.trash_dir),
            std::path::Path::new(&library.root_path),
            std::path::Path::new(&intent.final_path),
        );
        Ok((true, String::new(), trash.display().to_string()))
    }

    /// An agent reporting how a commit ended.
    ///
    /// A report bearing a stale epoch is rejected and **the job is left
    /// untouched**. That is the whole point of the fence: an instance the
    /// server has already replaced must not be able to resolve a ledger entry,
    /// because its view of what happened on disk is exactly what the
    /// replacement was created to stop trusting.
    async fn on_commit_report(
        &self,
        agent_id: &str,
        epoch: i64,
        rep: pb::CommitReport,
    ) -> Result<(), Status> {
        if i64::try_from(rep.fencing_epoch).unwrap_or(-1) != epoch {
            tracing::warn!(
                agent = %agent_id, job = %rep.job_id, reported = rep.fencing_epoch, current = epoch,
                "commit report bearing a stale epoch rejected; the job is left untouched"
            );
            return Ok(());
        }

        let Some(intent) = self
            .intents
            .live_for_job(&rep.job_id)
            .map_err(|e| Status::internal(format!("commit ledger unreadable: {e}")))?
        else {
            tracing::warn!(agent = %agent_id, job = %rep.job_id, "commit report for an unknown intent");
            return Ok(());
        };

        if intent.agent_id != agent_id || intent.fencing_epoch != epoch {
            tracing::warn!(
                agent = %agent_id, job = %rep.job_id,
                "commit report from an agent or epoch the intent was not granted to"
            );
            return Ok(());
        }

        self.write(CommitIntentRepo::resolve_op(
            intent.id,
            rep.resolution.clone(),
        ))
        .await?;

        // The job follows the ledger. Without this it sits in `Verifying`
        // forever: the work is done, the file is installed, and the queue still
        // counts it as in flight until a lease expires and the reconciler
        // requeues an encode that already happened.
        let (to, code) = outcome_of(&rep.resolution);
        if let Err(e) = self
            .write(JobRepo::transition_op(
                rep.job_id.clone(),
                JobState::Committing,
                to,
                Some(code.to_string()),
                Some(rep.detail.clone()),
            ))
            .await
        {
            tracing::error!(job = %rep.job_id, error = %e, to = ?to,
                "the commit resolved but the job could not be moved");
        }

        tracing::info!(agent = %agent_id, job = %rep.job_id, resolution = %rep.resolution,
            state = ?to, "commit resolved");
        Ok(())
    }
}
