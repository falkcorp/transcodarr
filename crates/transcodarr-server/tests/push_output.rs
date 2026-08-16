// file: crates/transcodarr-server/tests/push_output.rs
// version: 1.0.0
// guid: 4d21f6b8-9c07-4e35-a1d2-6b8f0e7a3c94
// last-edited: 2026-08-16
//! `PushOutput`: the server installing on a streaming agent's behalf.
//!
//! Its own file rather than more of `connect.rs`, which already carries three
//! RPCs and a module doc that describes only one of them.
//!
//! These tests use a **real library on disk** — a real source file, a real
//! work area, a real trash directory — because the thing under test is an
//! irreversible filesystem operation. A test that mocked the install would
//! verify the bookkeeping around a step it never performed, which is the
//! failure mode the commit ritual exists to prevent in the first place.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tonic::transport::{Channel, Server};
use tonic::{Request, Status};

use transcodarr_core::facts::SizeBucket;
use transcodarr_core::job::{JobClass, JobState};
use transcodarr_proto::pb;
use transcodarr_server::AgentSession;
use transcodarr_store::repo::{
    AgentRepo, CommitIntentRepo, FileRepo, FileUpsert, JobRepo, LibraryRecord, LibraryRepo,
    NewIntent, NewJob,
};
use transcodarr_store::{Db, ReadPool, WriteLane, Writer};

/// The bytes a seeded source file holds before anything is installed over it.
const SOURCE_BYTES: &[u8] = b"the original file, which must survive in the trash";
/// What the agent claims to have encoded.
const OUTPUT_BYTES: &[u8] = b"a smaller re-encode";
/// The attempt a freshly created job is on.
///
/// Zero, not one. Every refusal test below would otherwise trip the attempt
/// gate instead of the gate it is named for, and pass while proving nothing.
const ATTEMPT: u32 = 0;

struct Harness {
    client: pb::agent_service_client::AgentServiceClient<Channel>,
    intents: CommitIntentRepo,
    jobs: JobRepo,
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
        intents: CommitIntentRepo::new(pool.clone()),
        jobs: JobRepo::new(pool.clone()),
        writer,
        pool,
        dir,
    }
}

impl Harness {
    fn root(&self) -> PathBuf {
        self.dir.path().join("lib")
    }
    fn work_dir(&self) -> PathBuf {
        self.root().join(".work")
    }
    fn trash_dir(&self) -> PathBuf {
        self.root().join(".trash")
    }
    fn source_of(&self, job_id: &str) -> PathBuf {
        self.root().join(format!("{job_id}.mkv"))
    }

    /// Register an agent, returning the epoch it was issued.
    ///
    /// `boot_id` is the only thing that bumps an epoch, so registering twice
    /// with different ones is how a test manufactures a superseded instance.
    async fn register_as(&mut self, agent_id: &str, boot_id: &str) -> i64 {
        let res = self
            .client
            .register(pb::RegisterRequest {
                identity: Some(pb::AgentIdentity {
                    agent_id: agent_id.into(),
                    agent_uid: agent_id.into(),
                    boot_id: boot_id.into(),
                    agent_version: "1.0.0".into(),
                    proto_version: transcodarr_proto::PROTO_VERSION,
                }),
                capability: Some(pb::Capability {
                    platform: "linux".into(),
                    effective_cores: 38.0,
                    classes: vec!["audio".into()],
                    transport: pb::TransportMode::TmStream as i32,
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

    /// Seed a job that is genuinely installable: real directories, a real
    /// source file, and a file row carrying that file's actual stat facts.
    ///
    /// The stat facts matter. The server builds its `SourceGuard` from the
    /// stored row, so a row that disagreed with the disk would make every
    /// install refuse itself and every test here pass for the wrong reason.
    fn seed_installable_job(&self, job_id: &str, agent_id: &str, epoch: i64) {
        std::fs::create_dir_all(self.root()).unwrap();
        std::fs::create_dir_all(self.work_dir()).unwrap();
        std::fs::create_dir_all(self.trash_dir()).unwrap();

        let source = self.source_of(job_id);
        std::fs::write(&source, SOURCE_BYTES).unwrap();
        let facts = stat_facts(&source);

        self.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(LibraryRecord {
                    id: "tv".into(),
                    name: "tv".into(),
                    root_path: self.root().display().to_string(),
                    work_dir: self.work_dir().display().to_string(),
                    trash_dir: self.trash_dir().display().to_string(),
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
                    canonical_path: source.display().to_string(),
                    path_hash: format!("h-{job_id}"),
                    size_bytes: facts.0,
                    mtime_unix: facts.1,
                    mtime_ns: 0,
                    inode: facts.2,
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

        self.advance_to_verifying(job_id, agent_id, epoch);
        self.grant_intent(job_id, agent_id, epoch, &source);
    }

    /// Walk a job to `Verifying` — where a streaming agent pushes from, exactly
    /// as a mount-mode agent asks to commit from.
    fn advance_to_verifying(&self, job_id: &str, agent_id: &str, epoch: i64) {
        let step = |from, to| {
            self.writer
                .submit_blocking(
                    WriteLane::Normal,
                    JobRepo::transition_op(job_id.into(), from, to, None, None),
                )
                .unwrap();
        };
        step(JobState::Pending, JobState::Eligible);
        self.writer
            .submit_blocking(
                WriteLane::Normal,
                JobRepo::assign_op(job_id.into(), agent_id.into(), epoch),
            )
            .unwrap();
        step(JobState::Assigned, JobState::Running);
        step(JobState::Running, JobState::Verifying);
    }

    /// Write the ledger row the orchestrator writes at dispatch.
    ///
    /// Not something `PushOutput` does: the row goes in before the assignment
    /// goes out, so a pushed job always arrives already granted.
    fn grant_intent(&self, job_id: &str, agent_id: &str, epoch: i64, source: &Path) {
        self.writer
            .submit_blocking(
                WriteLane::Commit,
                CommitIntentRepo::grant_op(NewIntent {
                    id: format!("{job_id}:{ATTEMPT}"),
                    job_id: job_id.into(),
                    attempt: i64::from(ATTEMPT),
                    agent_id: agent_id.into(),
                    agent_uid: agent_id.into(),
                    fencing_epoch: epoch,
                    source_path: source.display().to_string(),
                    temp_path: self
                        .work_dir()
                        .join(format!("{job_id}.tmp"))
                        .display()
                        .to_string(),
                    final_path: source.display().to_string(),
                    expected_content_sig: "sig".into(),
                }),
            )
            .unwrap();
    }

    /// Push `bytes` as the output of `job_id`.
    ///
    /// `sig_override` lets a test send a signature that does not describe the
    /// bytes, which is the one failure a receiver must never wave through.
    async fn push(
        &mut self,
        job_id: &str,
        attempt: u32,
        bytes: &[u8],
        identity: Option<(&str, i64)>,
        sig_override: Option<&str>,
    ) -> Result<pb::PushOutputResponse, Status> {
        let sig = sig_override
            .map(str::to_string)
            .unwrap_or_else(|| blake3::hash(bytes).to_hex().to_string());
        let chunks = vec![pb::FileChunk {
            job_id: job_id.into(),
            attempt,
            offset: 0,
            data: bytes.to_vec(),
            last: true,
            content_sig: sig,
        }];
        self.push_chunks(chunks, identity).await
    }

    /// Push a caller-built chunk sequence, for the cases a well-formed transfer
    /// cannot express.
    async fn push_chunks(
        &mut self,
        chunks: Vec<pb::FileChunk>,
        identity: Option<(&str, i64)>,
    ) -> Result<pb::PushOutputResponse, Status> {
        let mut req = Request::new(tokio_stream::iter(chunks));
        if let Some((agent_id, epoch)) = identity {
            req.metadata_mut()
                .insert("x-agent-id", agent_id.parse().unwrap());
            req.metadata_mut()
                .insert("x-agent-epoch", epoch.to_string().parse().unwrap());
        }
        self.client.push_output(req).await.map(|r| r.into_inner())
    }

    fn job_state(&self, job_id: &str) -> JobState {
        self.jobs.get(job_id).unwrap().state
    }

    /// The ledger row's `state` and the resolution recorded against it.
    ///
    /// Not `phase`: that is the ritual's high-water mark and `resolve_op` does
    /// not touch it. Asserting on `phase` would be asserting that the ritual
    /// reached a step, not that the server wrote down how it ended.
    fn intent_state(&self, job_id: &str) -> Option<(String, Option<String>)> {
        self.intents
            .get(&format!("{job_id}:{ATTEMPT}"))
            .unwrap()
            .map(|i| (i.state, i.resolution))
    }

    /// Every file left under the work area, so a test can prove that a refused
    /// push staged nothing — or cleaned up after itself.
    fn staged_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in walkdir_all(&self.work_dir()) {
            if entry.is_file() {
                out.push(entry);
            }
        }
        out
    }
}

/// Size, mtime and inode as `SourceGuard` reads them.
fn stat_facts(path: &Path) -> (i64, i64, Option<i64>) {
    let meta = std::fs::metadata(path).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        Some(meta.ino() as i64)
    };
    #[cfg(not(unix))]
    let inode = None;
    (meta.len() as i64, mtime, inode)
}

fn walkdir_all(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walkdir_all(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// Assert that a push did not install, however the server chose to say so.
///
/// A judged refusal is `accepted: false`; a malformed or corrupt transfer is a
/// `Status`. Both are legitimate ways to say "you may not install this". The
/// one answer that is never acceptable is an accepted push, so that is what
/// this rules out — leaving each test's filesystem assertions to prove that
/// nothing was actually touched.
fn assert_not_installed(res: Result<pb::PushOutputResponse, Status>, why: &str) {
    if let Ok(r) = res {
        assert!(!r.accepted, "{why}");
    }
}

/// The whole point: bytes arrive over the wire and the library changes.
///
/// Asserted end to end rather than by inspecting the ledger, because a commit
/// that updated every row and moved no file would satisfy any weaker check.
#[tokio::test]
async fn a_pushed_output_replaces_the_source_and_keeps_the_original() {
    let mut h = harness().await;
    let epoch = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", epoch);

    let res = h
        .push("j1", ATTEMPT, OUTPUT_BYTES, Some(("u1", epoch)), None)
        .await
        .expect("a held job at a live epoch may install");
    assert!(res.accepted, "{}", res.reason);
    assert_eq!(res.bytes_received, OUTPUT_BYTES.len() as u64);

    assert_eq!(
        std::fs::read(h.source_of("j1")).unwrap(),
        OUTPUT_BYTES,
        "the destination must hold the pushed bytes"
    );
    let trashed = h.trash_dir().join("j1.mkv");
    assert_eq!(
        std::fs::read(&trashed).unwrap(),
        SOURCE_BYTES,
        "the original must be retained, not destroyed"
    );

    assert_eq!(h.job_state("j1"), JobState::Succeeded);
    assert_eq!(
        h.intent_state("j1"),
        Some(("resolved".into(), Some("installed".into()))),
        "the ledger row must not stay live, and must say how it ended"
    );
}

/// A superseded instance must not be able to install.
///
/// Re-registering under a new `boot_id` is what makes the first epoch stale,
/// and it is stale in the registry *and* on the intent — so this cannot pass
/// by tripping only one of the two checks.
#[tokio::test]
async fn a_push_from_a_stale_epoch_installs_nothing() {
    let mut h = harness().await;
    let stale = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", stale);
    let fresh = h.register_as("u1", "boot-b").await;
    assert_ne!(stale, fresh, "a new boot must bump the epoch");

    let res = h
        .push("j1", ATTEMPT, OUTPUT_BYTES, Some(("u1", stale)), None)
        .await;
    assert_not_installed(res, "a stale epoch must not install");

    assert_eq!(
        std::fs::read(h.source_of("j1")).unwrap(),
        SOURCE_BYTES,
        "the destination must be untouched"
    );
}

/// Holding a live epoch proves you are *an* agent, not that you hold *this*
/// job. Without the intent check any live agent that learned a job id could
/// overwrite another agent's destination.
#[tokio::test]
async fn a_push_from_an_agent_that_does_not_hold_the_job_installs_nothing() {
    let mut h = harness().await;
    let e1 = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", e1);
    let e2 = h.register_as("u2", "boot-b").await;

    let res = h
        .push("j1", ATTEMPT, OUTPUT_BYTES, Some(("u2", e2)), None)
        .await;
    assert_not_installed(res, "u2 does not hold j1");

    assert_eq!(std::fs::read(h.source_of("j1")).unwrap(), SOURCE_BYTES);
}

/// Unlike `FetchSource`, the attempt is load-bearing here: staging and the
/// ledger row are both keyed on `(job_id, attempt)`. A mislabelled push must be
/// refused *before* a byte is written, so it cannot leave a staging file that
/// belongs to no attempt.
#[tokio::test]
async fn a_push_for_the_wrong_attempt_stages_nothing() {
    let mut h = harness().await;
    let epoch = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", epoch);

    let res = h
        .push("j1", 7, OUTPUT_BYTES, Some(("u1", epoch)), None)
        .await;
    assert_not_installed(res, "attempt 7 is not the live attempt");

    assert_eq!(std::fs::read(h.source_of("j1")).unwrap(), SOURCE_BYTES);
    assert!(
        h.staged_files().is_empty(),
        "a refused attempt must not have opened a staging file: {:?}",
        h.staged_files()
    );
}

/// A truncated or corrupted transfer must never be installed, and must never
/// be reported as an accepted one.
#[tokio::test]
async fn a_push_whose_bytes_do_not_match_their_signature_installs_nothing() {
    let mut h = harness().await;
    let epoch = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", epoch);

    let lie = blake3::hash(b"different bytes entirely")
        .to_hex()
        .to_string();
    let res = h
        .push("j1", ATTEMPT, OUTPUT_BYTES, Some(("u1", epoch)), Some(&lie))
        .await;
    assert_not_installed(res, "a signature mismatch must never be accepted");

    assert_eq!(
        std::fs::read(h.source_of("j1")).unwrap(),
        SOURCE_BYTES,
        "corrupt bytes must not reach the destination"
    );
    assert!(
        h.staged_files().is_empty(),
        "the partial transfer must be cleaned up: {:?}",
        h.staged_files()
    );
}

/// Identity lives in metadata, exactly as it does for `Connect` and
/// `FetchSource`. An unstamped push is refused rather than attributed.
#[tokio::test]
async fn a_push_without_identity_metadata_is_refused() {
    let mut h = harness().await;
    let epoch = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", epoch);

    let err = h
        .push("j1", ATTEMPT, OUTPUT_BYTES, None, None)
        .await
        .expect_err("an unidentified push must not be served");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err}");

    assert_eq!(std::fs::read(h.source_of("j1")).unwrap(), SOURCE_BYTES);
}

/// The ledger is the authority on whether a destination may be touched. Once
/// the intent is resolved the reservation is gone, and a late push must not
/// resurrect it.
#[tokio::test]
async fn a_push_against_a_resolved_intent_installs_nothing() {
    let mut h = harness().await;
    let epoch = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", epoch);
    h.writer
        .submit_blocking(
            WriteLane::Commit,
            CommitIntentRepo::resolve_op(format!("j1:{ATTEMPT}"), "abandoned".into()),
        )
        .unwrap();

    let res = h
        .push("j1", ATTEMPT, OUTPUT_BYTES, Some(("u1", epoch)), None)
        .await;
    assert_not_installed(res, "a resolved intent grants nothing");

    assert_eq!(std::fs::read(h.source_of("j1")).unwrap(), SOURCE_BYTES);
}

/// The guard the whole streaming design turns on.
///
/// The server never watched this file — it cannot `observe()` a source the way
/// an in-process runner does, because it is stateless between RPCs. It builds
/// the guard from the scan facts the plan was made against instead, which is
/// what `SourceGuard` documents itself as holding. A source edited after the
/// plan was made must not be replaced by an encode of its previous contents.
#[tokio::test]
async fn a_source_that_changed_since_it_was_planned_is_not_overwritten() {
    let mut h = harness().await;
    let epoch = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", epoch);

    // Somebody replaced the episode after the job was planned.
    let replaced = b"a different, longer file that somebody else put here";
    std::fs::write(h.source_of("j1"), replaced).unwrap();

    let res = h
        .push("j1", ATTEMPT, OUTPUT_BYTES, Some(("u1", epoch)), None)
        .await;
    assert_not_installed(res, "a changed source must not be replaced");

    assert_eq!(
        std::fs::read(h.source_of("j1")).unwrap(),
        replaced,
        "the newer file must survive"
    );
    assert_ne!(
        h.job_state("j1"),
        JobState::Succeeded,
        "a refused install is not a success"
    );
}

/// `Sink` guards offsets, not identity. A stream that changes which job it is
/// talking about halfway through is a broken client, and continuing would
/// append one job's bytes to another's staging file.
#[tokio::test]
async fn a_stream_that_changes_job_midway_is_refused() {
    let mut h = harness().await;
    let epoch = h.register_as("u1", "boot-a").await;
    h.seed_installable_job("j1", "u1", epoch);
    h.seed_installable_job("j2", "u1", epoch);

    let res = h
        .push_chunks(
            vec![
                pb::FileChunk {
                    job_id: "j1".into(),
                    attempt: ATTEMPT,
                    offset: 0,
                    data: b"first half".to_vec(),
                    last: false,
                    content_sig: String::new(),
                },
                pb::FileChunk {
                    job_id: "j2".into(),
                    attempt: ATTEMPT,
                    offset: 10,
                    data: b"second half".to_vec(),
                    last: true,
                    content_sig: blake3::hash(b"first halfsecond half").to_hex().to_string(),
                },
            ],
            Some(("u1", epoch)),
        )
        .await;
    assert_not_installed(res, "a stream must name one job throughout");

    assert_eq!(std::fs::read(h.source_of("j1")).unwrap(), SOURCE_BYTES);
    assert_eq!(std::fs::read(h.source_of("j2")).unwrap(), SOURCE_BYTES);
}

/// A push naming a job that does not exist is an error, not an empty success.
#[tokio::test]
async fn a_push_for_an_unknown_job_is_refused() {
    let mut h = harness().await;
    let epoch = h.register_as("u1", "boot-a").await;

    let res = h
        .push("nope", ATTEMPT, OUTPUT_BYTES, Some(("u1", epoch)), None)
        .await;
    match res {
        Ok(r) => assert!(!r.accepted, "an unknown job must not be accepted"),
        Err(e) => assert_eq!(e.code(), tonic::Code::NotFound, "{e}"),
    }
    let _ = &h.pool;
}
