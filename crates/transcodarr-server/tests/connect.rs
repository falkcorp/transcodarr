// file: crates/transcodarr-server/tests/connect.rs
// version: 1.0.0
// guid: 1e5b34d8-7f92-4a06-b3c5-82e17ad9604b
// last-edited: 2026-08-04
//! The `Connect` stream, over a real gRPC channel.
//!
//! Same shape as `register.rs`: a `tonic` server on a loopback port, dialled
//! with the generated client. Nothing is called in-process, because the point
//! is to exercise what only appears once messages actually serialise.
//!
//! The stream is identified by request metadata rather than by a message, since
//! `AgentMessage` carries no identity — see the module documentation on
//! `session.rs` for why that was not added to the schema.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::{Channel, Server};
use tonic::{Request, Streaming};

use transcodarr_core::facts::SizeBucket;
use transcodarr_core::job::JobClass;
use transcodarr_proto::pb;
use transcodarr_server::AgentSession;
use transcodarr_store::repo::{
    AgentRepo, CommitIntentRepo, FileRepo, FileUpsert, JobRepo, LibraryRecord, LibraryRepo,
    NewIntent, NewJob,
};
use transcodarr_store::{Db, ReadPool, WriteLane, Writer};

struct Harness {
    client: pb::agent_service_client::AgentServiceClient<Channel>,
    agents: AgentRepo,
    intents: CommitIntentRepo,
    writer: Arc<Writer>,
    pool: ReadPool,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("t.db");
    let writer = Arc::new(Writer::start(Db::open_unchecked(&path).unwrap()));
    let pool = ReadPool::open(&path, 4).unwrap();

    let session = AgentSession::new(
        AgentRepo::new(pool.clone()),
        CommitIntentRepo::new(pool.clone()),
        JobRepo::new(pool.clone()),
        writer.clone(),
        None,
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .add_service(pb::agent_service_server::AgentServiceServer::new(session))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    let channel = loop {
        match Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
        {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };

    Harness {
        client: pb::agent_service_client::AgentServiceClient::new(channel),
        agents: AgentRepo::new(pool.clone()),
        intents: CommitIntentRepo::new(pool.clone()),
        writer,
        pool,
        _dir: dir,
    }
}

impl Harness {
    /// Register `u1`, returning the epoch it was issued.
    async fn register(&mut self) -> i64 {
        let res = self
            .client
            .register(pb::RegisterRequest {
                identity: Some(pb::AgentIdentity {
                    agent_id: "u1".into(),
                    agent_uid: "uid-1".into(),
                    boot_id: "boot-a".into(),
                    agent_version: "1.0.0".into(),
                    proto_version: transcodarr_proto::PROTO_VERSION,
                }),
                capability: Some(pb::Capability {
                    platform: "linux".into(),
                    effective_cores: 38.0,
                    classes: vec!["audio".into()],
                    ..Default::default()
                }),
                auth_token: String::new(),
                live_intents: Vec::new(),
            })
            .await
            .unwrap()
            .into_inner();
        assert!(res.accepted, "{}", res.reject_reason);
        i64::try_from(res.fencing_epoch).unwrap()
    }

    /// Open a stream as `u1` under `epoch`.
    async fn open(
        &mut self,
        epoch: i64,
    ) -> Result<(mpsc::Sender<pb::AgentMessage>, Streaming<pb::ServerMessage>), tonic::Status> {
        let (tx, rx) = mpsc::channel(8);
        let mut req = Request::new(tokio_stream::wrappers::ReceiverStream::new(rx));
        req.metadata_mut()
            .insert("x-agent-id", "u1".parse().unwrap());
        req.metadata_mut()
            .insert("x-agent-epoch", epoch.to_string().parse().unwrap());
        let stream = self.client.connect(req).await?.into_inner();
        Ok((tx, stream))
    }

    /// Seed a library, a file and a job so an intent has something to hang on.
    fn seed_job(&self, job_id: &str) {
        self.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(LibraryRecord {
                    id: "tv".into(),
                    name: "tv".into(),
                    root_path: "/mnt/tv".into(),
                    work_dir: "/mnt/tv/work".into(),
                    trash_dir: "/mnt/tv/trash".into(),
                    exclude_globs_json: "[]".into(),
                    enabled: true,
                    scan_parallelism: 4,
                    priority: 0,
                    min_mtime_age_s: 300,
                }),
            )
            .unwrap();

        let file_id = self
            .writer
            .submit_blocking(
                WriteLane::Normal,
                FileRepo::upsert_op(FileUpsert {
                    library_id: "tv".into(),
                    canonical_path: format!("/mnt/tv/{job_id}.mkv"),
                    path_hash: format!("h-{job_id}"),
                    size_bytes: 100,
                    mtime_unix: 10,
                    mtime_ns: 0,
                    inode: Some(1),
                    dev: Some(1),
                    nlink: 1,
                    scan_generation: 1,
                }),
            )
            .unwrap()
            .last_id
            .unwrap();

        self.writer
            .submit_blocking(
                WriteLane::Normal,
                JobRepo::create_op(NewJob {
                    id: job_id.into(),
                    file_id,
                    library_id: "tv".into(),
                    class: JobClass::Audio,
                    size_bucket: SizeBucket::Small,
                    requirements_json: "[]".into(),
                    requirements_bucket_key: "audio".into(),
                    expected_content_sig: "sig".into(),
                    rules_version: "1".into(),
                    priority: 0,
                    parent_job_id: None,
                }),
            )
            .unwrap();
    }

    /// Grant a live commit intent for a seeded job.
    fn seed_intent(&self, job_id: &str, agent_id: &str, epoch: i64) {
        self.writer
            .submit_blocking(
                WriteLane::Normal,
                CommitIntentRepo::grant_op(NewIntent {
                    id: format!("intent-{job_id}"),
                    job_id: job_id.into(),
                    attempt: 1,
                    agent_id: agent_id.into(),
                    agent_uid: "uid-1".into(),
                    fencing_epoch: epoch,
                    source_path: format!("/mnt/tv/{job_id}.mkv"),
                    temp_path: format!("/mnt/tv/work/{job_id}.partial.mkv"),
                    final_path: format!("/mnt/tv/{job_id}.mkv"),
                    expected_content_sig: "sig".into(),
                }),
            )
            .unwrap();
    }
}

/// Wait for one server message, failing rather than hanging forever.
async fn next(stream: &mut Streaming<pb::ServerMessage>) -> pb::ServerMessage {
    tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("timed out waiting for a server message")
        .expect("stream error")
        .expect("stream ended")
}

fn heartbeat(running: &[&str]) -> pb::AgentMessage {
    pb::AgentMessage {
        body: Some(pb::agent_message::Body::Heartbeat(pb::Heartbeat {
            at_unix_ms: 0,
            running_job_ids: running.iter().map(|s| (*s).to_string()).collect(),
            mounts: Vec::new(),
            load_average: 1.0,
        })),
    }
}

#[tokio::test]
async fn an_unregistered_agent_cannot_open_a_stream() {
    let mut h = harness().await;
    let err = h.open(1).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

/// A stream opened under a superseded epoch belongs to a process instance the
/// server has already replaced. Letting it in would hand a revoked instance a
/// live channel.
#[tokio::test]
async fn a_stream_bearing_a_stale_epoch_is_refused() {
    let mut h = harness().await;
    let epoch = h.register().await;

    let err = h.open(epoch - 1).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert!(err.message().contains("not current"), "{}", err.message());
}

#[tokio::test]
async fn a_registered_agent_holds_a_stream_and_its_lease_is_extended() {
    let mut h = harness().await;
    let epoch = h.register().await;
    let (tx, _stream) = h.open(epoch).await.unwrap();

    tx.send(heartbeat(&[])).await.unwrap();

    // The heartbeat is handled asynchronously; wait for the lease to move.
    let mut extended = false;
    for _ in 0..50 {
        let a = h.agents.get("u1").unwrap().unwrap();
        if a.lease_expires_unix.is_some() && a.status == "Online" {
            extended = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(extended, "the heartbeat should have extended the lease");
}

/// A survivor of a lost connection must not keep going. The server has already
/// accounted for that slot as free, and two encodes writing one output is what
/// the whole ledger exists to prevent.
#[tokio::test]
async fn a_running_job_the_server_does_not_recognise_is_revoked() {
    let mut h = harness().await;
    let epoch = h.register().await;
    let (tx, mut stream) = h.open(epoch).await.unwrap();

    tx.send(heartbeat(&["ghost-job"])).await.unwrap();

    match next(&mut stream).await.body.unwrap() {
        pb::server_message::Body::Revoke(r) => assert_eq!(r.job_id, "ghost-job"),
        other => panic!("expected a revoke, got {other:?}"),
    }
}

/// Permission to replace a file is not something to infer from the asking.
#[tokio::test]
async fn a_commit_request_with_no_intent_behind_it_is_refused() {
    let mut h = harness().await;
    let epoch = h.register().await;
    let (tx, mut stream) = h.open(epoch).await.unwrap();

    tx.send(pb::AgentMessage {
        body: Some(pb::agent_message::Body::CommitRequest(pb::CommitRequest {
            job_id: "j1".into(),
            attempt: 1,
            fencing_epoch: u64::try_from(epoch).unwrap(),
        })),
    })
    .await
    .unwrap();

    match next(&mut stream).await.body.unwrap() {
        pb::server_message::Body::CommitGrant(g) => {
            assert!(!g.granted);
            assert!(g.reason.contains("no live commit intent"), "{}", g.reason);
        }
        other => panic!("expected a commit grant, got {other:?}"),
    }
}

#[tokio::test]
async fn a_commit_request_against_a_live_intent_is_granted() {
    let mut h = harness().await;
    let epoch = h.register().await;
    h.seed_job("j1");
    h.seed_intent("j1", "u1", epoch);

    let (tx, mut stream) = h.open(epoch).await.unwrap();
    tx.send(pb::AgentMessage {
        body: Some(pb::agent_message::Body::CommitRequest(pb::CommitRequest {
            job_id: "j1".into(),
            attempt: 1,
            fencing_epoch: u64::try_from(epoch).unwrap(),
        })),
    })
    .await
    .unwrap();

    match next(&mut stream).await.body.unwrap() {
        pb::server_message::Body::CommitGrant(g) => {
            assert!(g.granted, "{}", g.reason);
            assert_eq!(g.trash_path, "/mnt/tv/j1.mkv");
        }
        other => panic!("expected a commit grant, got {other:?}"),
    }
}

/// The fence, exercised over the wire. An instance the server has already
/// replaced must not be able to resolve a ledger entry: its view of what
/// happened on disk is exactly what the replacement was created to stop
/// trusting.
#[tokio::test]
async fn a_commit_report_bearing_a_stale_epoch_leaves_the_intent_untouched() {
    let mut h = harness().await;
    let epoch = h.register().await;
    h.seed_job("j1");
    h.seed_intent("j1", "u1", epoch);

    let (tx, _stream) = h.open(epoch).await.unwrap();
    tx.send(pb::AgentMessage {
        body: Some(pb::agent_message::Body::CommitReport(pb::CommitReport {
            job_id: "j1".into(),
            attempt: 1,
            // One epoch behind: a revoked instance reporting success.
            fencing_epoch: u64::try_from(epoch - 1).unwrap(),
            resolution: "installed".into(),
            detail: String::new(),
        })),
    })
    .await
    .unwrap();

    // Give the server long enough to have acted on it, had it been going to.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let intent = h.intents.get("intent-j1").unwrap().unwrap();
    assert_eq!(intent.state, "live", "a stale report must resolve nothing");
    assert!(intent.resolution.is_none());
}

/// The same report under the current epoch does resolve it, so the test above
/// cannot be passing merely because nothing works.
#[tokio::test]
async fn a_commit_report_under_the_current_epoch_resolves_the_intent() {
    let mut h = harness().await;
    let epoch = h.register().await;
    h.seed_job("j1");
    h.seed_intent("j1", "u1", epoch);

    let (tx, _stream) = h.open(epoch).await.unwrap();
    tx.send(pb::AgentMessage {
        body: Some(pb::agent_message::Body::CommitReport(pb::CommitReport {
            job_id: "j1".into(),
            attempt: 1,
            fencing_epoch: u64::try_from(epoch).unwrap(),
            resolution: "installed".into(),
            detail: String::new(),
        })),
    })
    .await
    .unwrap();

    let mut resolved = false;
    for _ in 0..50 {
        let intent = h.intents.get("intent-j1").unwrap().unwrap();
        if intent.state == "resolved" {
            assert_eq!(intent.resolution.as_deref(), Some("installed"));
            resolved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(resolved, "a current-epoch report must resolve the intent");
}

#[tokio::test]
async fn a_quarantined_agent_is_not_allowed_to_connect() {
    let mut h = harness().await;
    let epoch = h.register().await;
    h.writer
        .submit_blocking(
            WriteLane::Normal,
            AgentRepo::quarantine_op("u1".into(), "failed five validations".into()),
        )
        .unwrap();

    let err = h.open(epoch).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    let _ = &h.pool; // the pool is held so the database outlives the server
}

/// Offline, not fenced. A dropped connection does not invalidate work already
/// granted; fencing here would kill a job running perfectly well behind a
/// network fault.
#[tokio::test]
async fn a_closed_stream_marks_the_agent_offline_without_touching_its_epoch() {
    let mut h = harness().await;
    let epoch = h.register().await;
    let (tx, stream) = h.open(epoch).await.unwrap();

    drop(tx);
    drop(stream);

    let mut offline = false;
    for _ in 0..50 {
        let a = h.agents.get("u1").unwrap().unwrap();
        if a.status == "Offline" {
            assert_eq!(a.fencing_epoch, epoch, "disconnect must not fence");
            offline = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(offline, "a closed stream should mark the agent offline");
}
