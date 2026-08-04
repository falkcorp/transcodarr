// file: crates/transcodarr-server/src/session.rs
// version: 1.1.0
// guid: 5c81a3e7-24b6-4f09-8d15-7a6c03e29b48
// last-edited: 2026-08-04
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
use transcodarr_store::repo::{AgentRegistration, AgentRepo, CommitIntentRepo, JobRepo};
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
        writer: Arc<Writer>,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            agents,
            intents,
            jobs,
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
            pb::agent_message::Body::Result(r) => {
                // Recorded, not acted on. Completing an assignment is the
                // dispatch loop's job, and there is no dispatch loop yet -- so
                // this build must not pretend a job finished.
                tracing::info!(
                    agent = %agent_id, job = %r.job_id, exit_code = r.exit_code,
                    "job result received, but this build does not yet dispatch work"
                );
                Ok(())
            }
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

        Ok((true, String::new(), intent.final_path))
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
        tracing::info!(agent = %agent_id, job = %rep.job_id, resolution = %rep.resolution, "commit resolved");
        Ok(())
    }
}
