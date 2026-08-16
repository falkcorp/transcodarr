// file: crates/transcodarr-server/tests/connect.rs
// version: 1.3.1
// guid: 1e5b34d8-7f92-4a06-b3c5-82e17ad9604b
// last-edited: 2026-08-16
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
use transcodarr_core::job::{JobClass, JobState};
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
    dir: tempfile::TempDir,
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
        LibraryRepo::new(pool.clone()),
        FileRepo::new(pool.clone()),
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
        dir,
    }
}

impl Harness {
    /// Register `u1`, returning the epoch it was issued.
    async fn register(&mut self) -> i64 {
        self.register_as("u1", "uid-1", "boot-a").await
    }

    /// Register any agent. A second one is needed to prove that bytes follow
    /// the job's holder — and `job.agent_id` is a foreign key, so the other
    /// agent has to genuinely exist rather than merely be named.
    /// `boot_id` is the only thing that bumps an epoch, so a second call with a
    /// fresh one is how a test manufactures a genuinely superseded instance.
    async fn register_as(&mut self, agent_id: &str, agent_uid: &str, boot_id: &str) -> i64 {
        let res = self
            .client
            .register(pb::RegisterRequest {
                identity: Some(pb::AgentIdentity {
                    agent_id: agent_id.into(),
                    agent_uid: agent_uid.into(),
                    boot_id: boot_id.into(),
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

    /// Walk a seeded job to `Verifying`, where an agent asks to commit from.
    ///
    /// The real path, not a shortcut: the server only grants a commit for a job
    /// it can move to `Committing`, because permission it cannot record is
    /// permission it cannot account for.
    fn advance_to_verifying(&self, job_id: &str, agent_id: &str, epoch: i64) {
        for (from, to) in [
            (JobState::Pending, JobState::Eligible),
            (JobState::Running, JobState::Verifying),
        ] {
            if from == JobState::Pending {
                self.writer
                    .submit_blocking(
                        WriteLane::Normal,
                        JobRepo::transition_op(job_id.into(), from, to, None, None),
                    )
                    .unwrap();
                self.writer
                    .submit_blocking(
                        WriteLane::Normal,
                        JobRepo::assign_op(job_id.into(), agent_id.into(), epoch),
                    )
                    .unwrap();
                self.writer
                    .submit_blocking(
                        WriteLane::Normal,
                        JobRepo::transition_op(
                            job_id.into(),
                            JobState::Assigned,
                            JobState::Running,
                            None,
                            None,
                        ),
                    )
                    .unwrap();
            } else {
                self.writer
                    .submit_blocking(
                        WriteLane::Normal,
                        JobRepo::transition_op(job_id.into(), from, to, None, None),
                    )
                    .unwrap();
            }
        }
    }

    /// Seed a job whose source is a real file, held by `agent_id` at `epoch`.
    ///
    /// `seed_job` points the row at `/mnt/tv/...`, which is fine for every test
    /// that never opens the file. Streaming reads the bytes, so the row has to
    /// name something that exists — otherwise the handler fails on the open and
    /// the test proves nothing about fencing.
    fn seed_streamable_job(&self, job_id: &str, agent_id: &str, epoch: i64, body: &[u8]) {
        let source = self.dir.path().join(format!("{job_id}.mkv"));
        std::fs::write(&source, body).unwrap();

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
                    canonical_path: source.to_string_lossy().to_string(),
                    path_hash: format!("h-{job_id}"),
                    size_bytes: body.len() as i64,
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

        // Pending -> Eligible -> Assigned -> Running, the real path. A job an
        // agent is fetching source for is one it has been handed.
        self.writer
            .submit_blocking(
                WriteLane::Normal,
                JobRepo::transition_op(
                    job_id.into(),
                    JobState::Pending,
                    JobState::Eligible,
                    None,
                    None,
                ),
            )
            .unwrap();
        self.writer
            .submit_blocking(
                WriteLane::Normal,
                JobRepo::assign_op(job_id.into(), agent_id.into(), epoch),
            )
            .unwrap();
        self.writer
            .submit_blocking(
                WriteLane::Normal,
                JobRepo::transition_op(
                    job_id.into(),
                    JobState::Assigned,
                    JobState::Running,
                    None,
                    None,
                ),
            )
            .unwrap();
    }

    /// Call `FetchSource`, optionally stamping the identity metadata.
    ///
    /// `identity: None` is the unidentified caller, which must be refused —
    /// `FetchSourceRequest` carries no `agent_id`, deliberately, so metadata is
    /// the only thing that says who is asking.
    async fn fetch(
        &mut self,
        job_id: &str,
        fencing_epoch: u64,
        identity: Option<(&str, i64)>,
    ) -> Result<tonic::Response<Streaming<pb::FileChunk>>, tonic::Status> {
        let mut req = Request::new(pb::FetchSourceRequest {
            job_id: job_id.into(),
            attempt: 1,
            fencing_epoch,
        });
        if let Some((id, ep)) = identity {
            req.metadata_mut().insert("x-agent-id", id.parse().unwrap());
            req.metadata_mut()
                .insert("x-agent-epoch", ep.to_string().parse().unwrap());
        }
        self.client.fetch_source(req).await
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
    h.advance_to_verifying("j1", "u1", epoch);
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
            // Where the original goes, under the library's trash directory --
            // emphatically *not* the destination. Handing back the final path
            // has the ritual rename the original onto itself and then overwrite
            // it, destroying the copy the trash exists to preserve. This test
            // asserted exactly that for two PRs.
            assert_eq!(g.trash_path, "/mnt/tv/trash/j1.mkv");
            assert_ne!(
                g.trash_path, "/mnt/tv/j1.mkv",
                "the trash path must never be the destination"
            );
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
    h.advance_to_verifying("j1", "u1", epoch);
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
    h.advance_to_verifying("j1", "u1", epoch);
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

// ------------------------------------------------------------ FetchSource --

/// Collect a whole fetch, so a refusal and an empty-but-successful stream
/// cannot be confused.
///
/// A handler that returned `Ok` with no chunks would satisfy any assertion
/// phrased as "this must not succeed in delivering bytes". Folding the call
/// error and the in-stream error into one `Result` means the refusal tests can
/// demand an actual error, which an empty stream does not produce.
async fn collect(
    res: Result<tonic::Response<Streaming<pb::FileChunk>>, tonic::Status>,
) -> Result<Vec<u8>, tonic::Status> {
    let mut stream = res?.into_inner();
    let mut out = Vec::new();
    while let Some(chunk) = stream.message().await? {
        out.extend_from_slice(&chunk.data);
    }
    Ok(out)
}

#[tokio::test]
async fn a_held_job_streams_its_source_bytes() {
    let mut h = harness().await;
    let epoch = h.register().await;
    // Larger than one chunk and not a multiple of it: the partial final read is
    // where an off-by-one would hide.
    let body: Vec<u8> = (0..(transcodarr_proto::transfer::CHUNK_BYTES + 4321))
        .map(|i| (i % 251) as u8)
        .collect();
    h.seed_streamable_job("j-fetch", "u1", epoch, &body);

    let got = collect(h.fetch("j-fetch", epoch as u64, Some(("u1", epoch))).await)
        .await
        .expect("a held job must serve its source");

    assert_eq!(got, body, "the bytes delivered must be the bytes on disk");
}

/// The signature is the gate that matters, so prove it is actually checked
/// end to end rather than trusting that `transfer` computed one.
#[tokio::test]
async fn the_delivered_stream_verifies_against_its_own_signature() {
    let mut h = harness().await;
    let epoch = h.register().await;
    let body: Vec<u8> = (0..40_000).map(|i| (i % 97) as u8).collect();
    h.seed_streamable_job("j-sig", "u1", epoch, &body);

    let dest = h.dir.path().join("received.mkv");
    let mut sink = transcodarr_proto::transfer::Sink::create(&dest).unwrap();
    let mut stream = h
        .fetch("j-sig", epoch as u64, Some(("u1", epoch)))
        .await
        .unwrap()
        .into_inner();

    let mut done = false;
    while let Some(chunk) = stream.message().await.unwrap() {
        done = sink.accept(&chunk).expect("every chunk must be acceptable");
    }

    assert!(done, "the stream must end with an explicit last chunk");
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[tokio::test]
async fn a_stale_epoch_is_served_no_bytes() {
    let mut h = harness().await;
    let epoch = h.register().await;
    h.seed_streamable_job("j-stale", "u1", epoch, b"secret bytes");

    // A restart under a new boot_id supersedes the instance holding `epoch`.
    // The old instance stamps and claims that same stale epoch consistently —
    // which is the realistic shape, and the one that actually exercises the
    // check against the registry rather than the two claims against each other.
    let fresh = h.register_as("u1", "uid-1", "boot-restarted").await;
    assert_ne!(fresh, epoch, "a new boot_id must take a new epoch");

    let err = collect(h.fetch("j-stale", epoch as u64, Some(("u1", epoch))).await)
        .await
        .expect_err("a fenced-out agent must not pull bytes for work it lost");

    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err}");
}

/// The two epochs come from one place on a real client, so a disagreement is a
/// confused caller — and picking one to believe is how a fence lands on the
/// wrong instance.
#[tokio::test]
async fn a_fetch_whose_two_epochs_disagree_is_refused() {
    let mut h = harness().await;
    let epoch = h.register().await;
    h.seed_streamable_job("j-mixed", "u1", epoch, b"mixed signals");

    let err = collect(
        h.fetch("j-mixed", (epoch + 1) as u64, Some(("u1", epoch)))
            .await,
    )
    .await
    .expect_err("a request that contradicts its own metadata must be refused");

    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err}");
}

#[tokio::test]
async fn an_agent_that_does_not_hold_the_job_is_served_no_bytes() {
    let mut h = harness().await;
    let epoch = h.register().await;
    // Held by somebody else entirely.
    let other = h.register_as("u2", "uid-2", "boot-b").await;
    h.seed_streamable_job("j-other", "u2", other, b"not yours");

    let err = collect(h.fetch("j-other", epoch as u64, Some(("u1", epoch))).await)
        .await
        .expect_err("bytes must follow the job's holder, not the caller's claim");

    assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err}");
}

#[tokio::test]
async fn a_fetch_without_identity_metadata_is_refused() {
    let mut h = harness().await;
    let epoch = h.register().await;
    h.seed_streamable_job("j-anon", "u1", epoch, b"anonymous");

    let err = collect(h.fetch("j-anon", epoch as u64, None).await)
        .await
        .expect_err("an unidentified caller must not be served");

    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err}");
}

/// Holding the row is not holding the work.
///
/// `transition_op` leaves `agent_id` and `fencing_epoch` untouched, so a failed
/// job still names its last holder — and the epoch cannot tell the difference,
/// since only a new `boot_id` bumps it. Nothing but the state says this work is
/// over.
#[tokio::test]
async fn a_job_that_has_left_a_held_state_is_served_no_bytes() {
    let mut h = harness().await;
    let epoch = h.register().await;
    h.seed_streamable_job("j-dead", "u1", epoch, b"finished with");

    h.writer
        .submit_blocking(
            WriteLane::Normal,
            JobRepo::transition_op(
                "j-dead".into(),
                JobState::Running,
                JobState::Failed,
                None,
                None,
            ),
        )
        .unwrap();

    let err = collect(h.fetch("j-dead", epoch as u64, Some(("u1", epoch))).await)
        .await
        .expect_err("a failed job must not keep serving its source");

    assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err}");
}

#[tokio::test]
async fn a_fetch_for_an_unknown_job_is_refused() {
    let mut h = harness().await;
    let epoch = h.register().await;

    let err = collect(
        h.fetch("no-such-job", epoch as u64, Some(("u1", epoch)))
            .await,
    )
    .await
    .expect_err("an unknown job has no source to serve");

    assert_eq!(err.code(), tonic::Code::NotFound, "{err}");
}
