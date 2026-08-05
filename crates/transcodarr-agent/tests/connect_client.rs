// file: crates/transcodarr-agent/tests/connect_client.rs
// version: 1.0.0
// guid: 5e73b0a9-2d41-4f68-90c7-1ab4e6d5837f
// last-edited: 2026-08-05
//! The agent side of the transport, against a real gRPC server.
//!
//! The server here is a fake, and deliberately so: `transcodarr-server` links
//! SQLite, and an agent test that depended on it would quietly make the agent
//! untestable on the Windows node it has to run on. What is *not* faked is the
//! transport — a real tonic server on a loopback port, dialled with the
//! generated client, so everything that only appears once messages serialise is
//! exercised.
//!
//! Two tests drive the real [`LocalWorker`] rather than a stub. A suite that
//! only ever ran the stub would be testing the seam instead of the path.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use transcodarr_agent::client::{ClientConfig, ConnectClient, Link, ReconnectPolicy, Worker};
use transcodarr_agent::commit::CommitRitual;
use transcodarr_agent::executor::{Executor, ExecutorConfig};
use transcodarr_agent::journal::{IntentPhase, IntentRecord};
use transcodarr_agent::workarea::WorkArea;
use transcodarr_agent::worker::LocalWorker;
use transcodarr_proto::pb;

// ---------------------------------------------------------------- fake server

/// What the fake server saw and what it was told to answer.
#[derive(Default)]
struct ServerState {
    registrations: Vec<pb::RegisterRequest>,
    inbound: Vec<pb::AgentMessage>,
    stream_metadata: Vec<HashMap<String, String>>,
    /// Answer every replayed intent with "I have no record of this".
    disown_intents: bool,
    /// Whether a `CommitRequest` is granted.
    grant_commits: bool,
    /// The trash path handed out with a grant.
    trash_path: String,
    /// Close the stream once this many messages have arrived.
    close_after: Option<usize>,
    /// Pushed to the agent as soon as the stream opens.
    to_send: Vec<pb::ServerMessage>,
    /// What each `Register` was answered with, so a test can prove which
    /// recovery path did the work.
    answered_unknown: Vec<Vec<String>>,
}

#[derive(Clone, Default)]
struct FakeServer {
    state: Arc<Mutex<ServerState>>,
    epoch: Arc<AtomicUsize>,
}

impl FakeServer {
    fn state(&self) -> std::sync::MutexGuard<'_, ServerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn inbound(&self) -> Vec<pb::AgentMessage> {
        self.state().inbound.clone()
    }

    fn heartbeats(&self) -> Vec<pb::Heartbeat> {
        self.inbound()
            .into_iter()
            .filter_map(|m| match m.body {
                Some(pb::agent_message::Body::Heartbeat(h)) => Some(h),
                _ => None,
            })
            .collect()
    }

    fn reports(&self) -> Vec<pb::CommitReport> {
        self.inbound()
            .into_iter()
            .filter_map(|m| match m.body {
                Some(pb::agent_message::Body::CommitReport(r)) => Some(r),
                _ => None,
            })
            .collect()
    }
}

#[tonic::async_trait]
impl pb::agent_service_server::AgentService for FakeServer {
    async fn register(
        &self,
        request: Request<pb::RegisterRequest>,
    ) -> Result<Response<pb::RegisterResponse>, Status> {
        let req = request.into_inner();
        let mut state = self.state();
        let unknown = if state.disown_intents {
            req.live_intents.iter().map(|i| i.job_id.clone()).collect()
        } else {
            Vec::new()
        };
        state.registrations.push(req);
        state.answered_unknown.push(unknown.clone());
        // The epoch is issued once and resumed on reconnect, exactly as the
        // real server does for an unchanged boot_id.
        let _ = self
            .epoch
            .compare_exchange(0, 7, Ordering::SeqCst, Ordering::SeqCst);

        Ok(Response::new(pb::RegisterResponse {
            accepted: true,
            reject_reason: String::new(),
            server_proto_version: transcodarr_proto::PROTO_VERSION,
            min_supported_proto: transcodarr_proto::MIN_SUPPORTED_PROTO,
            min_agent_version: String::new(),
            server_version: "fake".into(),
            fencing_epoch: self.epoch.load(Ordering::SeqCst) as u64,
            unknown_job_ids: unknown,
        }))
    }

    type ConnectStream = tokio_stream::wrappers::ReceiverStream<Result<pb::ServerMessage, Status>>;

    async fn connect(
        &self,
        request: Request<tonic::Streaming<pb::AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let metadata: HashMap<String, String> = request
            .metadata()
            .iter()
            .filter_map(|kv| match kv {
                tonic::metadata::KeyAndValueRef::Ascii(k, v) => {
                    Some((k.to_string(), v.to_str().ok()?.to_string()))
                }
                tonic::metadata::KeyAndValueRef::Binary(..) => None,
            })
            .collect();

        let (tx, rx) = mpsc::channel(16);
        {
            let mut state = self.state();
            state.stream_metadata.push(metadata);
            for msg in state.to_send.drain(..) {
                let _ = tx.try_send(Ok(msg));
            }
        }

        let this = self.clone();
        tokio::spawn(async move {
            let mut inbound = request.into_inner();
            while let Ok(Some(msg)) = inbound.message().await {
                let (grant, trash, close_after, seen) = {
                    let mut state = this.state();
                    state.inbound.push(msg.clone());
                    (
                        state.grant_commits,
                        state.trash_path.clone(),
                        state.close_after,
                        state.inbound.len(),
                    )
                };

                if let Some(pb::agent_message::Body::CommitRequest(req)) = msg.body {
                    let _ = tx
                        .send(Ok(pb::ServerMessage {
                            body: Some(pb::server_message::Body::CommitGrant(pb::CommitGrant {
                                job_id: req.job_id,
                                granted: grant,
                                reason: if grant {
                                    String::new()
                                } else {
                                    "the fake server refuses".into()
                                },
                                trash_path: trash,
                            })),
                        }))
                        .await;
                }

                if close_after.is_some_and(|n| seen >= n) {
                    return; // dropping tx closes the stream
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

/// Start the fake on a loopback port; returns it and its endpoint.
async fn serve() -> (FakeServer, String) {
    let fake = FakeServer::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let service = fake.clone();
    tokio::spawn(async move {
        Server::builder()
            .add_service(pb::agent_service_server::AgentServiceServer::new(service))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
    });

    (fake, format!("http://{addr}"))
}

/// Poll until `check` holds, or fail the test.
///
/// Bounded rather than unbounded: a hang here is a real failure, and a test
/// that waits forever reports it as a timeout with no message.
async fn until(what: &str, check: impl Fn() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

fn config(endpoint: &str) -> ClientConfig {
    let mut c = ClientConfig::new(endpoint, "u1");
    c.heartbeat = Duration::from_millis(50);
    c.reconnect = ReconnectPolicy {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(50),
    };
    c
}

// ------------------------------------------------------------------ the stub

/// A worker that records what it was asked and does nothing.
#[derive(Default)]
struct StubWorker {
    live_intents: Vec<pb::LiveIntent>,
    running: Vec<String>,
    unknown_seen: Mutex<Vec<String>>,
    revoked: Mutex<Vec<String>>,
    drained: Mutex<bool>,
}

#[tonic::async_trait]
impl Worker for StubWorker {
    fn capability(&self) -> pb::Capability {
        pb::Capability {
            platform: "linux".into(),
            effective_cores: 4.0,
            classes: vec!["audio".into()],
            ..Default::default()
        }
    }

    fn live_intents(&self) -> Vec<pb::LiveIntent> {
        self.live_intents.clone()
    }

    fn running_job_ids(&self) -> Vec<String> {
        self.running.clone()
    }

    async fn on_unknown_intents(&self, job_ids: Vec<String>) {
        self.unknown_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(job_ids);
    }

    async fn on_assignment(&self, _assignment: pb::JobAssignment, _link: Link) {}

    async fn on_revoke(&self, job_id: String, _reason: String) {
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(job_id);
    }

    async fn on_drain(&self, _drain: pb::Drain) -> Vec<String> {
        *self.drained.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.running.clone()
    }
}

// ------------------------------------------------------------ transport tests

/// The stream carries no identity, so the metadata is the only thing that says
/// who is calling. It must also carry the epoch actually issued.
#[tokio::test(flavor = "multi_thread")]
async fn the_stream_is_identified_by_metadata() {
    let (fake, endpoint) = serve().await;
    let client = ConnectClient::new(config(&endpoint), Arc::new(StubWorker::default()));
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("the stream to open", || {
        !fake.state().stream_metadata.is_empty()
    })
    .await;

    let md = fake.state().stream_metadata[0].clone();
    assert_eq!(md.get("x-agent-id").map(String::as_str), Some("u1"));
    assert_eq!(md.get("x-agent-epoch").map(String::as_str), Some("7"));

    shutdown.stop();
    run.await.unwrap();
}

/// The rule the whole fence rests on: a reconnect must present the *same*
/// `boot_id`, or the server treats each network blip as a new process instance
/// and fences work that is running perfectly well.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_presents_the_same_boot_id() {
    let (fake, endpoint) = serve().await;
    fake.state().close_after = Some(1);

    let client = ConnectClient::new(config(&endpoint), Arc::new(StubWorker::default()));
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("a second registration", || {
        fake.state().registrations.len() >= 2
    })
    .await;

    let ids: Vec<_> = fake
        .state()
        .registrations
        .iter()
        .map(|r| r.identity.clone().unwrap())
        .collect();
    assert_eq!(
        ids[0].boot_id, ids[1].boot_id,
        "a reconnect must not mint a new boot_id"
    );
    assert!(!ids[0].boot_id.is_empty());
    assert_eq!(ids[0].agent_uid, ids[1].agent_uid);
    assert_eq!(ids[0].agent_id, "u1");

    shutdown.stop();
    run.await.unwrap();
}

/// The journal goes out with `Register`, and what comes back is acted on before
/// the stream opens — no assignment can arrive while an unaccounted-for install
/// is still on disk.
#[tokio::test(flavor = "multi_thread")]
async fn the_journal_is_replayed_and_the_answer_acted_on() {
    let (fake, endpoint) = serve().await;
    fake.state().disown_intents = true;

    let worker = Arc::new(StubWorker {
        live_intents: vec![pb::LiveIntent {
            job_id: "job-1".into(),
            attempt: 0,
            fencing_epoch: 7,
            phase: "retired".into(),
            temp_path: "/w/job-1.partial.mkv".into(),
            final_path: "/mnt/tv/a.mkv".into(),
            trash_path: "/t/a.mkv".into(),
        }],
        ..Default::default()
    });
    let client = ConnectClient::new(config(&endpoint), worker.clone());
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("the unknown intent to be handled", || {
        !worker
            .unknown_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    })
    .await;

    let replayed = fake.state().registrations[0].live_intents.clone();
    assert_eq!(replayed.len(), 1, "the journal must reach the server");
    assert_eq!(replayed[0].job_id, "job-1");
    let seen = worker.unknown_seen.lock().unwrap().clone();
    assert_eq!(seen, vec!["job-1".to_string()]);

    shutdown.stop();
    run.await.unwrap();
}

/// The running set is the load-bearing half of a heartbeat: the server revokes
/// anything in it that it does not recognise.
#[tokio::test(flavor = "multi_thread")]
async fn a_heartbeat_carries_the_running_set() {
    let (fake, endpoint) = serve().await;
    let worker = Arc::new(StubWorker {
        running: vec!["job-7".into()],
        ..Default::default()
    });
    let client = ConnectClient::new(config(&endpoint), worker);
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("a heartbeat", || !fake.heartbeats().is_empty()).await;
    assert_eq!(fake.heartbeats()[0].running_job_ids, vec!["job-7"]);
    assert!(fake.heartbeats()[0].at_unix_ms > 0);

    shutdown.stop();
    run.await.unwrap();
}

/// A `Revoke` must reach the worker while it is busy — the inbound loop cannot
/// be blocked behind whatever it is doing.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoke_reaches_the_worker() {
    let (fake, endpoint) = serve().await;
    fake.state().to_send = vec![pb::ServerMessage {
        body: Some(pb::server_message::Body::Revoke(pb::Revoke {
            job_id: "job-9".into(),
            reason: "no record".into(),
        })),
    }];

    let worker = Arc::new(StubWorker::default());
    let client = ConnectClient::new(config(&endpoint), worker.clone());
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("the revoke", || {
        !worker
            .revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    })
    .await;
    let revoked = worker.revoked.lock().unwrap().clone();
    assert_eq!(revoked, vec!["job-9".to_string()]);

    shutdown.stop();
    run.await.unwrap();
}

/// A drain is acknowledged with what is still running, not with silence.
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_is_acknowledged() {
    let (fake, endpoint) = serve().await;
    fake.state().to_send = vec![pb::ServerMessage {
        body: Some(pb::server_message::Body::Drain(pb::Drain {
            immediate: false,
            reason: "maintenance".into(),
        })),
    }];

    let worker = Arc::new(StubWorker {
        running: vec!["job-3".into()],
        ..Default::default()
    });
    let client = ConnectClient::new(config(&endpoint), worker);
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("the drain ack", || {
        fake.inbound().iter().any(|m| {
            matches!(
                m.body,
                Some(pb::agent_message::Body::DrainAck(ref a)) if a.still_running == ["job-3"]
            )
        })
    })
    .await;

    shutdown.stop();
    run.await.unwrap();
}

// ------------------------------------------------- the real worker, real disk

fn capability() -> pb::Capability {
    pb::Capability {
        platform: "linux".into(),
        effective_cores: 4.0,
        classes: vec!["audio".into()],
        ..Default::default()
    }
}

fn local_worker(root: &Path) -> (Arc<LocalWorker>, WorkArea) {
    let work = WorkArea::open(&root.join("work"), "uid-1", "boot-a").unwrap();
    let ritual = CommitRitual::new(work.open_journal().unwrap(), work.clone());
    let worker = LocalWorker::new(
        Executor::new(ExecutorConfig::default()),
        ritual,
        work.clone(),
        capability(),
    );
    (Arc::new(worker), work)
}

/// The dangerous case, end to end and on real files.
///
/// The agent crashed between retiring the original and installing the
/// replacement, so the destination is empty and the original is in the trash.
/// The server has no live intent for it. Discarding — the obvious reading of
/// "the server does not know about this" — would lose the file outright; the
/// original has to come back.
#[tokio::test(flavor = "multi_thread")]
async fn a_retired_intent_the_server_disowns_is_restored() {
    let dir = tempfile::TempDir::new().unwrap();
    let (worker, work) = local_worker(dir.path());

    let final_path = dir.path().join("lib/a.mkv");
    let trash_path = dir.path().join("trash/a.mkv");
    std::fs::create_dir_all(final_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(trash_path.parent().unwrap()).unwrap();
    // The original, where a crash between the two renames leaves it.
    std::fs::write(&trash_path, b"the original").unwrap();

    worker
        .journal()
        .record(&IntentRecord {
            job_id: "job-1".into(),
            attempt: 0,
            agent_uid: "uid-1".into(),
            boot_id: "boot-previous".into(),
            fencing_epoch: 6,
            temp_path: work.temp_path("job-1", 0, &final_path),
            final_path: final_path.clone(),
            trash_path: trash_path.clone(),
            expected_content_sig: "sig".into(),
            phase: IntentPhase::Retired,
        })
        .unwrap();

    let (fake, endpoint) = serve().await;
    fake.state().disown_intents = true;

    let client = ConnectClient::new(config(&endpoint), worker);
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("the original to come back", || final_path.is_file()).await;
    assert_eq!(std::fs::read(&final_path).unwrap(), b"the original");
    assert!(!trash_path.exists(), "it was moved back, not copied");

    // The record is cleared only because it resolved. An unresolved one stays,
    // being the only evidence that something needs a human.
    assert_eq!(
        fake.state().registrations[0].live_intents.len(),
        1,
        "the record must have been replayed before it was resolved"
    );

    shutdown.stop();
    run.await.unwrap();
}

/// The ordinary crash, and the case `on_unknown_intents` does **not** cover.
///
/// The server has a live intent for this job, so it names nothing unknown —
/// there is no answer to act on. Recovery still has to run, or a `Retired`
/// record sits on disk with the destination empty while the agent cheerfully
/// takes new work. This is the test that fails when nothing calls it.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_the_server_still_knows_about_is_recovered_at_startup() {
    let dir = tempfile::TempDir::new().unwrap();
    let (worker, work) = local_worker(dir.path());

    let final_path = dir.path().join("lib/a.mkv");
    let trash_path = dir.path().join("trash/a.mkv");
    std::fs::create_dir_all(final_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(trash_path.parent().unwrap()).unwrap();
    std::fs::write(&trash_path, b"the original").unwrap();

    worker
        .journal()
        .record(&IntentRecord {
            job_id: "job-1".into(),
            attempt: 0,
            agent_uid: "uid-1".into(),
            boot_id: "boot-previous".into(),
            fencing_epoch: 6,
            temp_path: work.temp_path("job-1", 0, &final_path),
            final_path: final_path.clone(),
            trash_path: trash_path.clone(),
            expected_content_sig: "sig".into(),
            phase: IntentPhase::Retired,
        })
        .unwrap();

    let (fake, endpoint) = serve().await;
    // The difference from the test above: the server knows about it, so
    // `unknown_job_ids` comes back empty and nothing is disowned.
    fake.state().disown_intents = false;

    let client = ConnectClient::new(config(&endpoint), worker);
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("the original to come back", || final_path.is_file()).await;
    assert_eq!(std::fs::read(&final_path).unwrap(), b"the original");
    let answered = fake.state().answered_unknown[0].clone();
    assert!(
        answered.is_empty(),
        "the server must have named nothing, or this proves the other path: {answered:?}"
    );
    let replayed = fake.state().registrations[0].live_intents.clone();
    assert_eq!(replayed.len(), 1, "the record was still replayed");

    shutdown.stop();
    run.await.unwrap();
}

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn make_media(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let ok = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(ok.success(), "fixture generation failed");
}

/// One assignment, all the way through: encode, validate, ask, install, report.
///
/// Real ffmpeg, the real executor, the real ritual. The argv is what the server
/// sent and is run verbatim — an agent that rebuilt it could encode to a plan
/// the server never authorised.
#[tokio::test(flavor = "multi_thread")]
async fn an_assignment_runs_end_to_end() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg and ffprobe are needed for this test");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let final_path = dir.path().join("lib/a.mkv");
    make_media(&final_path);
    let original = std::fs::read(&final_path).unwrap();

    let (worker, work) = local_worker(dir.path());
    let temp = work.temp_path("job-1", 0, &final_path);
    let trash_path = dir.path().join("trash/a.mkv");

    let (fake, endpoint) = serve().await;
    {
        let mut state = fake.state();
        state.grant_commits = true;
        state.trash_path = trash_path.display().to_string();
        state.to_send = vec![pb::ServerMessage {
            body: Some(pb::server_message::Body::Assignment(pb::JobAssignment {
                job_id: "job-1".into(),
                attempt: 0,
                fencing_epoch: 7,
                source_path: final_path.display().to_string(),
                final_path: final_path.display().to_string(),
                temp_path: temp.display().to_string(),
                // A remux: every stream copied. Built server-side, run verbatim.
                argv: vec![
                    "-v".into(),
                    "error".into(),
                    "-y".into(),
                    "-i".into(),
                    final_path.display().to_string(),
                    "-map".into(),
                    "0".into(),
                    "-c".into(),
                    "copy".into(),
                    temp.display().to_string(),
                ],
                validation_spec_json: serde_json::json!({
                    "source_duration_us": 2_000_000u64,
                    "max_shorter_us": 100_000u64,
                    "max_longer_us": 2_000_000u64,
                    "expected_audio_streams": 1,
                    "expected_subtitle_streams": 0,
                    "source_bytes": original.len(),
                    "size_policy": "MayGrow",
                })
                .to_string(),
                expected_content_sig: "sig".into(),
            })),
        }];
    }

    let client = ConnectClient::new(config(&endpoint), worker);
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    until("the commit report", || !fake.reports().is_empty()).await;

    let report = fake.reports()[0].clone();
    assert_eq!(report.job_id, "job-1");
    assert_eq!(report.resolution, "installed", "{}", report.detail);
    assert_eq!(
        report.fencing_epoch, 7,
        "the report must carry the current epoch"
    );

    // The replacement is in place, the original is retained, and the staged
    // file is gone.
    assert!(final_path.is_file());
    assert_ne!(
        std::fs::read(&final_path).unwrap(),
        original,
        "the destination should hold the remux, not the original"
    );
    assert_eq!(std::fs::read(&trash_path).unwrap(), original);
    assert!(!temp.exists(), "the staged file should have been renamed");

    shutdown.stop();
    run.await.unwrap();
}

/// A refusal is not permission. When the server says no, the source must be
/// exactly where it was — this is the case where getting it wrong destroys a
/// file the server had good reason to protect.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_commit_leaves_the_source_intact() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg and ffprobe are needed for this test");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let final_path = dir.path().join("lib/a.mkv");
    make_media(&final_path);
    let original = std::fs::read(&final_path).unwrap();

    let (worker, work) = local_worker(dir.path());
    let temp = work.temp_path("job-1", 0, &final_path);

    let (fake, endpoint) = serve().await;
    {
        let mut state = fake.state();
        state.grant_commits = false; // the only difference from the test above
        state.trash_path = dir.path().join("trash/a.mkv").display().to_string();
        state.to_send = vec![pb::ServerMessage {
            body: Some(pb::server_message::Body::Assignment(pb::JobAssignment {
                job_id: "job-1".into(),
                attempt: 0,
                fencing_epoch: 7,
                source_path: final_path.display().to_string(),
                final_path: final_path.display().to_string(),
                temp_path: temp.display().to_string(),
                argv: vec![
                    "-v".into(),
                    "error".into(),
                    "-y".into(),
                    "-i".into(),
                    final_path.display().to_string(),
                    "-map".into(),
                    "0".into(),
                    "-c".into(),
                    "copy".into(),
                    temp.display().to_string(),
                ],
                validation_spec_json: serde_json::json!({
                    "source_duration_us": 2_000_000u64,
                    "max_shorter_us": 100_000u64,
                    "max_longer_us": 2_000_000u64,
                    "expected_audio_streams": 1,
                    "expected_subtitle_streams": 0,
                    "source_bytes": original.len(),
                    "size_policy": "MayGrow",
                })
                .to_string(),
                expected_content_sig: "sig".into(),
            })),
        }];
    }

    let client = ConnectClient::new(config(&endpoint), worker);
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    // The request is what proves the refusal was actually exercised: without
    // it, "the source is intact" would also pass if nothing had run at all.
    until("the commit request", || {
        fake.inbound()
            .iter()
            .any(|m| matches!(m.body, Some(pb::agent_message::Body::CommitRequest(_))))
    })
    .await;
    until("the staged file to be discarded", || !temp.exists()).await;

    assert_eq!(
        std::fs::read(&final_path).unwrap(),
        original,
        "a refusal must leave the source exactly as it was"
    );
    assert!(fake.reports().is_empty(), "nothing was committed to report");

    shutdown.stop();
    run.await.unwrap();
}
