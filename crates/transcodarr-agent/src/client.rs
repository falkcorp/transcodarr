// file: crates/transcodarr-agent/src/client.rs
// version: 1.2.0
// guid: 0f5d8c31-97b4-42ae-b6d0-58e19c3a7042
// last-edited: 2026-08-16
//! The agent side of the transport: register, connect, stay connected.
//!
//! The server half of this lives in `transcodarr-server::session`, and the
//! rules it enforces are the reason this module is shaped the way it is.
//!
//! ## `boot_id` is generated once, not once per attempt
//!
//! `fencing_epoch` bumps on a new `boot_id` and only on a new `boot_id`. A
//! client that minted a fresh one per connection attempt would turn every
//! network blip into an epoch bump, and the server would fence work that is
//! running perfectly well behind a transient fault. So the identity comes from
//! [`crate::identity::boot_id`], which is a process-lifetime `OnceLock`, and a
//! reconnect deliberately re-registers under the same one: the server resumes
//! the epoch it already issued, which makes reconnecting cheap and correct.
//!
//! ## The journal is replayed before any work is accepted
//!
//! `Register` carries the outstanding [`crate::journal::IntentJournal`] records
//! as `live_intents`, and the server answers with the subset it has no live
//! ledger entry for. Those are installs this agent was in the middle of that
//! the server cannot account for — the agent must resolve them *before* it
//! takes anything new, or it can be handed the same file again and install over
//! its own half-finished replace.
//!
//! The ordering matters and is easy to get backwards: read the journal, send
//! it, act on the answer, and only then run recovery. Running recovery first
//! clears the records, `live_intents` goes out empty, `unknown_job_ids` comes
//! back empty every time, and the test that checks it passes while proving
//! nothing.
//!
//! ## Identity is metadata, not a message
//!
//! `AgentMessage` carries no identity: every variant of it is a message *about*
//! work. The stream is identified by `x-agent-id` and `x-agent-epoch` set once
//! when it opens. See the module documentation on `session.rs` for why a
//! `Hello` message was not added to the schema.
//!
//! ## What this module does not decide
//!
//! Nothing here interprets a job. Assignments, revokes and drains are handed to
//! a [`Worker`], which is where the [`crate::executor::Executor`] and
//! [`crate::commit::CommitRitual`] live. The seam exists because the transport
//! must be testable against a fake server without media, and because a
//! reconnect must not care what the worker was doing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Notify, mpsc, oneshot};
use tonic::Request;
use tonic::transport::{Channel, Endpoint};

use transcodarr_proto::pb;

use crate::identity;

/// Metadata key carrying the operator-assigned agent name.
const AGENT_ID_KEY: &str = "x-agent-id";

/// Metadata key carrying the epoch the stream authenticated under.
const AGENT_EPOCH_KEY: &str = "x-agent-epoch";

/// How long to wait for a `CommitGrant` before giving up on it.
///
/// Expiring is a *refusal*, never an assumption of permission: the caller is
/// told nothing was granted and leaves the source intact. A commit that waits
/// forever would pin a job in `Committing` across a server restart, which is
/// the state a stalled agent is hardest to distinguish from a working one.
const COMMIT_GRANT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a session must last before its reconnect backoff resets.
///
/// Without this, a server that accepts a stream and immediately closes it turns
/// the backoff into a hot loop — each attempt "succeeds", so each one resets
/// the delay to zero.
const HEALTHY_SESSION: Duration = Duration::from_secs(30);

/// Anything that can go wrong talking to the server.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// The endpoint could not be dialled.
    #[error("dialling {endpoint}: {source}")]
    Dial {
        /// Which endpoint.
        endpoint: String,
        /// Underlying error.
        source: tonic::transport::Error,
    },

    /// The server refused this agent, with a reason.
    ///
    /// Not an error in the transport's sense — the exchange worked, and the
    /// answer was no. It is surfaced as one because an agent that was refused
    /// has nothing useful to do until an operator acts.
    #[error("registration refused: {reason}")]
    Rejected {
        /// What the server said.
        reason: String,
    },

    /// An RPC failed.
    #[error("{what}: {source}")]
    Rpc {
        /// Which call.
        what: &'static str,
        /// Underlying status.
        source: Box<tonic::Status>,
    },

    /// The configured identity cannot be sent as request metadata.
    #[error("{field} is not valid in request metadata: {value}")]
    BadIdentity {
        /// Which field.
        field: &'static str,
        /// What it held.
        value: String,
    },

    /// A streamed file transfer did not complete.
    ///
    /// Distinct from [`ClientError::Rpc`] because the interesting failures here
    /// are not transport faults: a source that will not open, a signature that
    /// does not match, a server that refused the install. Every one of them
    /// must leave the job failed rather than retried blindly against the same
    /// bytes.
    #[error("{what} for {job_id}: {detail}")]
    Transfer {
        /// Which half of the transfer.
        what: &'static str,
        /// The job it belonged to.
        job_id: String,
        /// What went wrong.
        detail: String,
    },
}

/// How long to wait between reconnection attempts.
///
/// Exponential, capped, and jittered. The jitter is not decoration: a server
/// restart disconnects every agent at once, and an unjittered backoff has them
/// all return in the same instant — repeatedly, since the retry that fails
/// schedules the next one just as precisely.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// The first delay.
    pub initial: Duration,
    /// The ceiling.
    pub max: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

impl ReconnectPolicy {
    /// The delay before attempt `attempt`, counting from zero.
    ///
    /// Jitter is derived from the agent's `boot_id` rather than from a random
    /// number generator, so it is reproducible in a test and still differs
    /// between agents — which is the only property that matters for spreading a
    /// herd.
    pub fn delay(&self, attempt: u32, boot_id: &str) -> Duration {
        let base = self
            .initial
            .checked_mul(1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX))
            .unwrap_or(self.max)
            .min(self.max);

        // A cheap FNV-1a over the boot id and the attempt number: this only has
        // to spread agents apart, not resist anything.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in boot_id.as_bytes().iter().chain(&attempt.to_le_bytes()) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        // Half the base, plus up to half again: never zero, never over base.
        let half = base / 2;
        half + Duration::from_nanos(
            (u64::try_from(half.as_nanos()).unwrap_or(u64::MAX)).wrapping_mul(h >> 32) >> 32,
        )
    }
}

/// Everything the client needs to introduce itself.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Where the server is, as a URI (`http://host:port`).
    pub endpoint: String,
    /// The operator-assigned name this agent answers to.
    pub agent_id: String,
    /// This installation's identifier.
    pub agent_uid: String,
    /// This build's version.
    pub agent_version: String,
    /// The shared secret, when the server requires one.
    pub auth_token: Option<String>,
    /// How often to heartbeat.
    ///
    /// The server's lease is 45 seconds, so the default of 15 tolerates two
    /// lost heartbeats before the agent is considered gone.
    pub heartbeat: Duration,
    /// How long to wait for the TCP connection itself.
    pub connect_timeout: Duration,
    /// How to back off when the connection drops.
    pub reconnect: ReconnectPolicy,
}

impl ClientConfig {
    /// A configuration for `agent_id` against `endpoint`, with this
    /// installation's identity and the defaults.
    pub fn new(endpoint: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            agent_id: agent_id.into(),
            agent_uid: identity::agent_uid(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            auth_token: None,
            heartbeat: Duration::from_secs(15),
            connect_timeout: Duration::from_secs(10),
            reconnect: ReconnectPolicy::default(),
        }
    }
}

/// What the client hands a [`Worker`] so it can talk back.
///
/// Cloneable and cheap. A worker holds one for the life of a job and it stays
/// valid across the job — but not across a reconnect: sends on a dead stream
/// fail, and a commit awaiting a grant that never arrives is refused rather
/// than assumed.
///
/// It carries two different ways of reaching the server, and the difference
/// matters. `out` is the `Connect` stream, which is already authenticated —
/// the server knows who is on the far end because it accepted the stream. The
/// [`TransportMode::Stream`] transfers are *separate* RPCs on the same channel,
/// so each one has to assert its own identity in metadata; see
/// [`Link::fetch_source`].
#[derive(Clone)]
pub struct Link {
    out: mpsc::Sender<pb::AgentMessage>,
    epoch: Arc<AtomicI64>,
    pending: Pending,
    /// A second handle on the same channel, for the unary/streaming transfer
    /// RPCs. Cloned per call: tonic clients need `&mut self` and cloning one is
    /// a refcount bump, not a new connection.
    rpc: pb::agent_service_client::AgentServiceClient<Channel>,
    /// Needed because a transfer RPC must stamp itself. The `Connect` stream
    /// was stamped once, at the call that opened it, and that stamp does not
    /// travel to any other RPC.
    agent_id: String,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<pb::CommitGrant>>>>;

/// Put an agent's identity into request metadata.
///
/// Free rather than a method because *every* RPC this agent makes needs it and
/// only one of them is made by the client itself. `FetchSource` and
/// `PushOutput` read identity from exactly this metadata and refuse without it,
/// so an unstamped transfer fails with an opaque `Unauthenticated` that names
/// nothing. One implementation, so the two epochs — the one in metadata and the
/// one in the request body — cannot come to disagree.
fn stamp_identity(
    agent_id: &str,
    epoch: i64,
    md: &mut tonic::metadata::MetadataMap,
) -> Result<(), ClientError> {
    let id = agent_id.parse().map_err(|_| ClientError::BadIdentity {
        field: AGENT_ID_KEY,
        value: agent_id.to_string(),
    })?;
    md.insert(AGENT_ID_KEY, id);

    let epoch = epoch
        .to_string()
        .parse()
        .map_err(|_| ClientError::BadIdentity {
            field: AGENT_EPOCH_KEY,
            value: epoch.to_string(),
        })?;
    md.insert(AGENT_EPOCH_KEY, epoch);
    Ok(())
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("fencing_epoch", &self.fencing_epoch())
            .finish()
    }
}

impl Link {
    /// The epoch this agent currently holds.
    ///
    /// Read at the moment it is used rather than cached by the caller: a
    /// re-registration can move it, and a message bearing the old one is
    /// rejected with the job left untouched.
    pub fn fencing_epoch(&self) -> i64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Put this agent's identity on a transfer RPC.
    ///
    /// Read at the moment of the call, like every other use of the epoch: a
    /// re-registration between an assignment and its fetch moves it, and the
    /// server compares against what the registry holds now.
    fn stamp(&self, md: &mut tonic::metadata::MetadataMap) -> Result<(), ClientError> {
        stamp_identity(&self.agent_id, self.fencing_epoch(), md)
    }

    /// Send one message, returning whether the stream took it.
    async fn send(&self, body: pb::agent_message::Body) -> bool {
        self.out
            .send(pb::AgentMessage { body: Some(body) })
            .await
            .is_ok()
    }

    /// Report progress. Lossy by design — a dropped progress frame costs a
    /// display update, and blocking an encode to deliver one would cost more.
    pub async fn progress(&self, progress: pb::JobProgress) {
        let _ = self.send(pb::agent_message::Body::Progress(progress)).await;
    }

    /// Report how an encode ended.
    pub async fn result(&self, result: pb::JobResult) -> bool {
        self.send(pb::agent_message::Body::Result(result)).await
    }

    /// Ask permission to install, and wait for the answer.
    ///
    /// `None` means no permission was given — refused, or the stream died, or
    /// the answer did not arrive within [`COMMIT_GRANT_TIMEOUT`]. Every one of
    /// those is a refusal, and the caller must leave the source intact.
    /// Permission to replace a file is never inferred from silence.
    pub async fn request_commit(&self, job_id: &str, attempt: i64) -> Option<pb::CommitGrant> {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            // A second request for one job supersedes the first; the earlier
            // waiter sees its sender dropped and reads that as a refusal.
            pending.insert(job_id.to_string(), tx);
        }

        let sent = self
            .send(pb::agent_message::Body::CommitRequest(pb::CommitRequest {
                job_id: job_id.to_string(),
                attempt: u32::try_from(attempt).unwrap_or(0),
                fencing_epoch: u64::try_from(self.fencing_epoch()).unwrap_or(0),
            }))
            .await;
        if !sent {
            self.forget(job_id);
            return None;
        }

        match tokio::time::timeout(COMMIT_GRANT_TIMEOUT, rx).await {
            Ok(Ok(grant)) if grant.granted => Some(grant),
            Ok(Ok(grant)) => {
                tracing::warn!(job = %job_id, reason = %grant.reason, "commit refused");
                None
            }
            Ok(Err(_)) => {
                tracing::warn!(job = %job_id, "stream ended before the commit was answered");
                None
            }
            Err(_) => {
                self.forget(job_id);
                tracing::warn!(job = %job_id, "no commit grant arrived; treating as refused");
                None
            }
        }
    }

    /// Say how a commit ended.
    ///
    /// Carries the *current* epoch. A report bearing a stale one is rejected
    /// and the job left untouched — that is the fence working, not a bug to
    /// route around by caching the epoch the job started under.
    pub async fn report_commit(
        &self,
        job_id: &str,
        attempt: i64,
        resolution: &str,
        detail: &str,
    ) -> bool {
        self.send(pb::agent_message::Body::CommitReport(pb::CommitReport {
            job_id: job_id.to_string(),
            attempt: u32::try_from(attempt).unwrap_or(0),
            fencing_epoch: u64::try_from(self.fencing_epoch()).unwrap_or(0),
            resolution: resolution.to_string(),
            detail: detail.to_string(),
        }))
        .await
    }

    /// Pull a job's source down into `dest`.
    ///
    /// Returns how many bytes landed. Only for [`TransportMode::Stream`]: a
    /// mount agent opens the source directly and never calls this.
    ///
    /// **A stream error is fatal, never end-of-stream.** The server reports a
    /// missing source as a `Status` *inside* the stream, so a reader that
    /// treats any stop as completion turns "there is no source" into a
    /// zero-byte fetch — and a blake3 of nothing verifies against a signature
    /// computed over nothing. [`tonic::Streaming::message`] is used rather than
    /// a `while let Some(Ok(..))` loop precisely because it makes the two cases
    /// different types instead of two arms one can forget to write.
    ///
    /// A failed fetch removes whatever it had written. A partial source left
    /// behind would be picked up by the encode as if it were whole.
    pub async fn fetch_source(
        &self,
        job_id: &str,
        attempt: i64,
        dest: &std::path::Path,
    ) -> Result<u64, ClientError> {
        let result = self.fetch_into(job_id, attempt, dest).await;
        if result.is_err() {
            let _ = std::fs::remove_file(dest);
        }
        result
    }

    async fn fetch_into(
        &self,
        job_id: &str,
        attempt: i64,
        dest: &std::path::Path,
    ) -> Result<u64, ClientError> {
        let fail = |detail: String| ClientError::Transfer {
            what: "fetch_source",
            job_id: job_id.to_string(),
            detail,
        };

        let mut request = Request::new(pb::FetchSourceRequest {
            job_id: job_id.to_string(),
            attempt: u32::try_from(attempt).unwrap_or(0),
            fencing_epoch: u64::try_from(self.fencing_epoch()).unwrap_or(0),
        });
        self.stamp(request.metadata_mut())?;

        let mut rpc = self.rpc.clone();
        let mut chunks = rpc
            .fetch_source(request)
            .await
            .map_err(|e| ClientError::Rpc {
                what: "fetch_source",
                source: Box::new(e),
            })?
            .into_inner();

        let mut sink = transcodarr_proto::transfer::Sink::create(dest)
            .map_err(|e| fail(format!("cannot open {} to write: {e}", dest.display())))?;

        loop {
            // `?` on this is the whole point: an in-stream `Status` leaves here
            // as an error, where `Ok(None)` falls through to the truncation
            // check below. They must not share an arm.
            let chunk = chunks
                .message()
                .await
                .map_err(|e| fail(format!("the source stream failed: {e}")))?;

            let Some(chunk) = chunk else {
                return Err(fail(format!(
                    "the stream ended after {} bytes without a final chunk; a transfer that \
                     stopped is not a transfer that finished",
                    sink.written()
                )));
            };

            if sink.accept(&chunk).map_err(fail)? {
                return Ok(sink.written());
            }
        }
    }

    /// Push a finished encode back for the server to install.
    ///
    /// The server performs the commit ritual itself, so what comes back is the
    /// *resolution*, not permission to act: a streaming agent has no path to
    /// the destination and could not install anything if it tried. A refusal
    /// here is a clean answer with a reason, and the source on the server is
    /// untouched.
    ///
    /// Opening the file is the caller's first failure point on purpose. An
    /// output that is not there must fail before an RPC starts, rather than
    /// becoming a transfer that opens and then quietly stops.
    pub async fn push_output(
        &self,
        job_id: &str,
        attempt: i64,
        source: &std::path::Path,
    ) -> Result<pb::PushOutputResponse, ClientError> {
        let fail = |detail: String| ClientError::Transfer {
            what: "push_output",
            job_id: job_id.to_string(),
            detail,
        };

        let file = std::fs::File::open(source)
            .map_err(|e| fail(format!("cannot read {} to push: {e}", source.display())))?;

        let stream = transcodarr_proto::transfer::output_stream(
            job_id.to_string(),
            u32::try_from(attempt).unwrap_or(0),
            file,
        );

        let mut request = Request::new(stream);
        self.stamp(request.metadata_mut())?;

        let mut rpc = self.rpc.clone();
        let response = rpc
            .push_output(request)
            .await
            .map_err(|e| ClientError::Rpc {
                what: "push_output",
                source: Box::new(e),
            })?
            .into_inner();

        Ok(response)
    }

    fn forget(&self, job_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(job_id);
    }
}

/// What the transport hands its work to.
///
/// Implemented by [`crate::worker::LocalWorker`] for a real agent, and by a
/// stub in tests. Nothing in this trait knows about gRPC: it receives decoded
/// messages and answers through a [`Link`].
#[tonic::async_trait]
pub trait Worker: Send + Sync + 'static {
    /// What this agent can do, as sent at `Register`.
    fn capability(&self) -> pb::Capability;

    /// Installs that were in flight when this agent last stopped.
    ///
    /// Read from the fsynced journal **before** recovery runs, or the records
    /// are already cleared and the server is told there is nothing outstanding.
    fn live_intents(&self) -> Vec<pb::LiveIntent>;

    /// Jobs this agent believes it is running.
    fn running_job_ids(&self) -> Vec<String>;

    /// The mounts to re-stat on each heartbeat. A mount can vanish under a
    /// running agent, and the dispatcher budgets against these.
    fn mounts(&self) -> Vec<pb::Mount> {
        Vec::new()
    }

    /// This machine's load, for the operator's benefit.
    fn load_average(&self) -> f64 {
        0.0
    }

    /// Resolve the intents the server has no record of.
    ///
    /// Called after `Register` and before any assignment is accepted. What to
    /// do depends on how far each one got, and a uniform "clean up" here
    /// deletes media — see [`crate::worker::LocalWorker`].
    async fn on_unknown_intents(&self, job_ids: Vec<String>);

    /// Resolve whatever else was in flight when this process last stopped.
    ///
    /// Called **once**, on the first connection, after `on_unknown_intents` and
    /// before the stream opens. `on_unknown_intents` only covers the records the
    /// server disowns; the ordinary crash — where the ledger row and the journal
    /// record both exist — is this one. Without it a `Retired` record sits on
    /// disk with the destination empty while the agent cheerfully takes new
    /// work.
    ///
    /// Once, not per reconnect, and that is not an optimisation: a reconnect can
    /// happen while a commit is between its two renames, and recovery running
    /// then would restore the original out from under the ritual performing it.
    async fn on_startup(&self) {}

    /// Run one assignment. Must not block: the caller spawns it, and the
    /// inbound loop keeps reading while it runs.
    async fn on_assignment(&self, assignment: pb::JobAssignment, link: Link);

    /// Stop work the server no longer recognises.
    async fn on_revoke(&self, job_id: String, reason: String);

    /// Stop taking work; return what is still running.
    async fn on_drain(&self, drain: pb::Drain) -> Vec<String>;

    /// Adopt new runtime limits.
    async fn on_config(&self, _config: pb::RuntimeConfig) {}
}

/// Asks the client to stop at the next boundary.
#[derive(Clone, Debug)]
pub struct Shutdown {
    stopped: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Shutdown {
    /// Stop the client. Idempotent, and safe from any task.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Resolves once stopped — **including when it was already stopped before
    /// this was called**.
    ///
    /// Awaiting `notify.notified()` directly is the wrong thing and the reason
    /// this method exists. `notify_waiters` wakes only the tasks already parked
    /// at the instant it runs and stores no permit, so the signal is lost
    /// whenever [`Shutdown::stop`] lands while this task is somewhere else —
    /// mid-recovery, mid-register, mid-dispatch. The task then parks on a
    /// notification that has already been and gone. That is not a slow
    /// shutdown, it is a permanent one: it cost a CI job 26 hours parked on a
    /// condvar at zero CPU.
    ///
    /// The ordering argument: `stop` stores the flag *before* it notifies, and
    /// this registers as a waiter *before* it reads the flag. So either the
    /// read below sees `true`, or the notify that follows it is guaranteed to
    /// reach an already-registered waiter. There is no interleaving in which
    /// both are missed.
    async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        // Registers interest without awaiting. This line is the fix; moving it
        // below the check reopens the race.
        notified.as_mut().enable();
        if self.is_stopped() {
            return;
        }
        notified.await;
    }
}

/// Registers, connects, and keeps connected.
pub struct ConnectClient<W: Worker> {
    config: ClientConfig,
    worker: Arc<W>,
    epoch: Arc<AtomicI64>,
    pending: Pending,
    shutdown: Shutdown,
    /// Whether startup recovery has run. Once per process, never per
    /// reconnect — see [`Worker::on_startup`].
    recovered: Arc<AtomicBool>,
}

impl<W: Worker> ConnectClient<W> {
    /// Build a client over a worker.
    pub fn new(config: ClientConfig, worker: Arc<W>) -> Self {
        Self {
            config,
            worker,
            epoch: Arc::new(AtomicI64::new(0)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Shutdown {
                stopped: Arc::new(AtomicBool::new(false)),
                notify: Arc::new(Notify::new()),
            },
            recovered: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A handle that stops this client.
    pub fn shutdown(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// The epoch currently held, or zero before the first registration.
    pub fn fencing_epoch(&self) -> i64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Register, connect, and reconnect until told to stop.
    ///
    /// Returns only on shutdown. Every failure is retried with backoff,
    /// including a refusal — an agent turned away for its version will be
    /// accepted once it is upgraded, and giving up would mean an operator has
    /// to restart every agent by hand after a rollout.
    pub async fn run(&self) {
        let mut attempt = 0u32;
        while !self.shutdown.is_stopped() {
            let started = Instant::now();
            match self.session().await {
                Ok(()) => tracing::info!("stream closed"),
                Err(e) => tracing::warn!(error = %e, "session ended"),
            }
            if self.shutdown.is_stopped() {
                break;
            }

            // Only a session that actually lasted counts as healthy. A server
            // that accepts and immediately closes would otherwise reset the
            // backoff on every attempt and produce a hot loop.
            attempt = if started.elapsed() >= HEALTHY_SESSION {
                0
            } else {
                attempt.saturating_add(1)
            };

            let delay = self.config.reconnect.delay(attempt, identity::boot_id());
            tracing::info!(?delay, attempt, "reconnecting");
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.shutdown.cancelled() => break,
            }
        }
        tracing::info!("agent client stopped");
    }

    /// One connection, from dial to stream close.
    async fn session(&self) -> Result<(), ClientError> {
        let mut client = self.dial().await?;

        // Read the journal *before* anything is resolved: `live_intents` is how
        // the server learns what this agent was in the middle of.
        let live_intents = self.worker.live_intents();
        let response = self.register(&mut client, live_intents).await?;

        self.epoch.store(
            i64::try_from(response.fencing_epoch).unwrap_or(0),
            Ordering::SeqCst,
        );
        tracing::info!(
            epoch = self.fencing_epoch(),
            server = %response.server_version,
            unknown_intents = response.unknown_job_ids.len(),
            "registered"
        );

        // Acted on before the stream opens, so no assignment can arrive while
        // an unaccounted-for install is still on disk.
        if !response.unknown_job_ids.is_empty() {
            self.worker
                .on_unknown_intents(response.unknown_job_ids)
                .await;
        }

        // Then everything else the journal still holds -- the ordinary crash,
        // where the server does have a live intent and so named nothing. Once
        // per process: a reconnect can land while a commit is between its two
        // renames, and recovery running then would restore the original out
        // from under the ritual installing over it.
        if !self.recovered.swap(true, Ordering::SeqCst) {
            self.worker.on_startup().await;
        }

        let (out, out_rx) = mpsc::channel::<pb::AgentMessage>(64);
        let link = Link {
            out: out.clone(),
            epoch: self.epoch.clone(),
            pending: self.pending.clone(),
            // The same channel the stream runs on. Cloning the client shares
            // the connection; it does not dial a second one.
            rpc: client.clone(),
            agent_id: self.config.agent_id.clone(),
        };

        let mut request = Request::new(tokio_stream::wrappers::ReceiverStream::new(out_rx));
        self.stamp(request.metadata_mut())?;
        let mut inbound = client
            .connect(request)
            .await
            .map_err(|e| ClientError::Rpc {
                what: "connect",
                source: Box::new(e),
            })?
            .into_inner();

        let heartbeat = tokio::spawn(heartbeat_loop(
            self.worker.clone(),
            out,
            self.config.heartbeat,
        ));
        // Aborted however this scope is left, including on an early return:
        // a heartbeat task outliving its stream would hold a sender open and
        // keep a dead session's channel from closing.
        let _guard = AbortOnDrop(heartbeat);

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return Ok(()),
                message = inbound.message() => {
                    match message {
                        Ok(Some(msg)) => self.dispatch(msg, &link),
                        Ok(None) => return Ok(()),
                        Err(e) => return Err(ClientError::Rpc {
                            what: "stream",
                            source: Box::new(e),
                        }),
                    }
                }
            }
        }
    }

    /// Dial the server.
    async fn dial(
        &self,
    ) -> Result<pb::agent_service_client::AgentServiceClient<Channel>, ClientError> {
        let channel = Endpoint::from_shared(self.config.endpoint.clone())
            .map_err(|e| ClientError::Dial {
                endpoint: self.config.endpoint.clone(),
                source: e,
            })?
            .connect_timeout(self.config.connect_timeout)
            .connect()
            .await
            .map_err(|e| ClientError::Dial {
                endpoint: self.config.endpoint.clone(),
                source: e,
            })?;
        // `AgentServiceClient::new(channel)`, not a `connect(dst)` constructor:
        // the generated client is built with `build_transport(false)` because
        // that constructor collides with the method generated for `rpc
        // Connect`.
        Ok(pb::agent_service_client::AgentServiceClient::new(channel))
    }

    /// Introduce this agent and replay its journal.
    async fn register(
        &self,
        client: &mut pb::agent_service_client::AgentServiceClient<Channel>,
        live_intents: Vec<pb::LiveIntent>,
    ) -> Result<pb::RegisterResponse, ClientError> {
        let response = client
            .register(pb::RegisterRequest {
                identity: Some(pb::AgentIdentity {
                    agent_id: self.config.agent_id.clone(),
                    agent_uid: self.config.agent_uid.clone(),
                    // The same value on every attempt. See the module
                    // documentation: a fresh one here fences live work.
                    boot_id: identity::boot_id().to_string(),
                    agent_version: self.config.agent_version.clone(),
                    proto_version: transcodarr_proto::PROTO_VERSION,
                }),
                capability: Some(self.worker.capability()),
                auth_token: self.config.auth_token.clone().unwrap_or_default(),
                live_intents,
            })
            .await
            .map_err(|e| ClientError::Rpc {
                what: "register",
                source: Box::new(e),
            })?
            .into_inner();

        if !response.accepted {
            return Err(ClientError::Rejected {
                reason: response.reject_reason,
            });
        }
        Ok(response)
    }

    /// Put this agent's identity on the stream's request.
    fn stamp(&self, md: &mut tonic::metadata::MetadataMap) -> Result<(), ClientError> {
        stamp_identity(&self.config.agent_id, self.fencing_epoch(), md)
    }

    /// Route one inbound message.
    ///
    /// Everything that could take time is spawned. The inbound loop must keep
    /// reading: a `Revoke` that arrives while an assignment is being set up is
    /// exactly the message that must not wait behind it.
    fn dispatch(&self, msg: pb::ServerMessage, link: &Link) {
        let Some(body) = msg.body else {
            return; // an empty envelope from a newer peer
        };

        match body {
            pb::server_message::Body::Assignment(a) => {
                let (worker, link) = (self.worker.clone(), link.clone());
                tracing::info!(job = %a.job_id, attempt = a.attempt, "assignment received");
                tokio::spawn(async move { worker.on_assignment(a, link).await });
            }
            pb::server_message::Body::CommitGrant(grant) => {
                let waiter = self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&grant.job_id);
                match waiter {
                    Some(tx) => {
                        let _ = tx.send(grant);
                    }
                    // Late, or for a job this instance is not waiting on. Not
                    // acted on: a grant is permission to install something
                    // specific, and nothing here knows what that was.
                    None => tracing::warn!(job = %grant.job_id, "unmatched commit grant ignored"),
                }
            }
            pb::server_message::Body::Revoke(r) => {
                let worker = self.worker.clone();
                tracing::warn!(job = %r.job_id, reason = %r.reason, "revoked");
                tokio::spawn(async move { worker.on_revoke(r.job_id, r.reason).await });
            }
            pb::server_message::Body::Drain(d) => {
                let (worker, link) = (self.worker.clone(), link.clone());
                tracing::info!(immediate = d.immediate, reason = %d.reason, "drain requested");
                tokio::spawn(async move {
                    let still_running = worker.on_drain(d).await;
                    link.send(pb::agent_message::Body::DrainAck(pb::DrainAck {
                        still_running,
                    }))
                    .await;
                });
            }
            pb::server_message::Body::Config(c) => {
                let worker = self.worker.clone();
                tokio::spawn(async move { worker.on_config(c).await });
            }
        }
    }
}

/// Heartbeat until the stream closes.
///
/// The running set is the load-bearing half: the server revokes anything in it
/// that it does not recognise, which is how a survivor of a lost connection is
/// stopped rather than left to install over live work.
async fn heartbeat_loop<W: Worker>(
    worker: Arc<W>,
    out: mpsc::Sender<pb::AgentMessage>,
    every: Duration,
) {
    let mut ticker = tokio::time::interval(every);
    // The first tick fires immediately, which is what we want: the server has
    // just admitted the stream and the running set is the first thing it needs.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let beat = pb::Heartbeat {
            at_unix_ms: u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
            )
            .unwrap_or(0),
            running_job_ids: worker.running_job_ids(),
            mounts: worker.mounts(),
            load_average: worker.load_average(),
        };
        if out
            .send(pb::AgentMessage {
                body: Some(pb::agent_message::Body::Heartbeat(beat)),
            })
            .await
            .is_err()
        {
            return; // the stream is gone
        }
    }
}

/// Aborts a task when it goes out of scope.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_grows_and_is_capped() {
        let p = ReconnectPolicy::default();
        let boot = "boot-a";
        assert!(p.delay(0, boot) <= p.initial);
        assert!(p.delay(3, boot) > p.delay(0, boot));
        for attempt in 0..40 {
            assert!(p.delay(attempt, boot) <= p.max, "attempt {attempt}");
        }
    }

    /// A delay of zero would busy-loop against a server that is down.
    #[test]
    fn the_backoff_is_never_zero() {
        let p = ReconnectPolicy::default();
        for attempt in 0..40 {
            assert!(p.delay(attempt, "boot-a") > Duration::ZERO);
        }
    }

    /// A server restart disconnects every agent at once. Without jitter they
    /// all come back in the same instant, repeatedly.
    #[test]
    fn two_agents_do_not_reconnect_in_lockstep() {
        let p = ReconnectPolicy::default();
        let differ = (0..8).any(|a| p.delay(a, "boot-a") != p.delay(a, "boot-b"));
        assert!(differ, "the jitter must depend on the agent");
    }

    /// A huge attempt count must not overflow into a tiny delay — that would
    /// turn the longest outage into the busiest retry loop.
    #[test]
    fn an_extreme_attempt_count_stays_capped() {
        let p = ReconnectPolicy::default();
        assert!(p.delay(u32::MAX, "boot-a") <= p.max);
        assert!(p.delay(u32::MAX, "boot-a") > Duration::ZERO);
    }

    fn test_shutdown() -> Shutdown {
        Shutdown {
            stopped: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// The lost wakeup, which is the failure that hung a CI job for 26 hours.
    ///
    /// `stop` runs while nothing is parked yet — exactly what happens when it
    /// lands mid-recovery, mid-register or mid-dispatch. `notify_waiters`
    /// reaches nobody and leaves no permit behind, so a waiter arriving
    /// afterwards has only the flag to go on.
    ///
    /// Note the timeout. The bug this guards against *hangs* rather than
    /// fails, so an assertion alone would reproduce the disease instead of
    /// reporting it: without the wrapper, a regression parks this test forever
    /// and the suite dies with no failing test to point at.
    #[tokio::test]
    async fn a_stop_that_precedes_the_waiter_is_still_seen() {
        let shutdown = test_shutdown();
        shutdown.stop();
        tokio::time::timeout(Duration::from_secs(5), shutdown.cancelled())
            .await
            .expect("cancelled() must resolve when stop() has already happened");
    }

    /// The other direction, kept because the obvious fix breaks it: a
    /// `cancelled` that only polled the flag once and never registered would
    /// satisfy the test above and leave an already-parked waiter asleep.
    #[tokio::test]
    async fn a_waiter_already_parked_is_still_woken() {
        let shutdown = test_shutdown();
        let waiter = shutdown.clone();
        let parked = tokio::spawn(async move { waiter.cancelled().await });
        // Single-threaded runtime: this hands control to the spawned task and
        // does not come back until it has parked.
        tokio::task::yield_now().await;

        shutdown.stop();
        tokio::time::timeout(Duration::from_secs(5), parked)
            .await
            .expect("a waiter parked before stop() must be woken by it")
            .expect("the waiting task must not panic");
    }
}
