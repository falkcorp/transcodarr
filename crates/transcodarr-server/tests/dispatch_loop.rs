// file: crates/transcodarr-server/tests/dispatch_loop.rs
// version: 1.1.0
// guid: b2d704e9-5c81-4a36-97f0-1e648d3c5b02
// last-edited: 2026-08-16
//! The loop under conditions the single-job proof cannot reach.
//!
//! `end_to_end.rs` runs one job, once, on its first attempt, to success. That
//! path is the one least likely to be wrong. These are the others:
//!
//! - a job the reconciler returns to the queue must be dispatched *again*;
//! - a job that keeps failing must stop, rather than cycle forever;
//! - more jobs than slots must be capped by the ledger, not handed out;
//! - a paused schedule must place nothing at all.
//!
//! No media and no encoding: the agents here are fleet-table entries, and what
//! is under test is the placement decision, not the work. Anything needing
//! ffmpeg belongs in `end_to_end.rs`.

use std::sync::Arc;

use transcodarr_core::capability::{AgentClass, Capability, ContainerId, Platform, TransportMode};
use transcodarr_core::facts::{FileFacts, SizeBucket};
use transcodarr_core::job::{JobClass, JobState};
use transcodarr_core::plan::{BitDepth, EncoderId};
use transcodarr_server::AgentTable;
use transcodarr_server::Runtime;
use transcodarr_server::capacity::AgentLimits;
use transcodarr_server::orchestrator::Orchestrator;
use transcodarr_server::schedule::ScheduleEngine;
use transcodarr_store::WriteLane;
use transcodarr_store::repo::{
    AgentRegistration, AgentRepo, FileRepo, FileUpsert, JobRepo, LibraryRecord, LibraryRepo, NewJob,
};

/// A harness with a store, a fleet table, and a loop over both.
struct Harness {
    runtime: Runtime,
    fleet: AgentTable,
    jobs: JobRepo,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let runtime = Runtime::open_unchecked(&dir.path().join("t.db")).unwrap();
        let jobs = JobRepo::new(runtime.pool().clone());

        runtime
            .writer()
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(LibraryRecord {
                    id: "lib".into(),
                    name: "lib".into(),
                    root_path: dir.path().join("lib").display().to_string(),
                    work_dir: dir.path().join("lib/work").display().to_string(),
                    trash_dir: dir.path().join("lib/trash").display().to_string(),
                    exclude_globs_json: "[]".into(),
                    enabled: true,
                    scan_parallelism: 1,
                    priority: 0,
                    min_mtime_age_s: 0,
                }),
            )
            .unwrap();

        Self {
            runtime,
            fleet: AgentTable::new(),
            jobs,
            _dir: dir,
        }
    }

    fn orchestrator(&self, limits: AgentLimits) -> Orchestrator {
        Orchestrator::new(
            self.runtime.pool().clone(),
            Arc::clone(self.runtime.writer()),
            self.fleet.clone(),
            transcodarr_core::policy::default_space_saver(),
            limits,
        )
    }

    /// Register an agent in the database and connect it to the fleet table.
    ///
    /// Both halves, because they answer different questions: the row says the
    /// fleet knows it, the table says it is reachable right now.
    fn add_agent(
        &self,
        id: &str,
    ) -> tokio::sync::mpsc::Receiver<Result<transcodarr_proto::pb::ServerMessage, tonic::Status>>
    {
        self.register(id, Self::base_capability())
    }

    /// The same agent, but streaming: no mounts, and a work area of its own.
    ///
    /// `workarea` is passed in rather than fixed so a test can register an
    /// agent that advertises none, which is the case the dispatcher has to
    /// refuse.
    fn add_stream_agent(
        &self,
        id: &str,
        workarea: &str,
    ) -> tokio::sync::mpsc::Receiver<Result<transcodarr_proto::pb::ServerMessage, tonic::Status>>
    {
        self.register(
            id,
            Capability {
                transport: TransportMode::Stream,
                platform: Some(Platform::Linux),
                workarea_path: workarea.to_string(),
                ..Self::base_capability()
            },
        )
    }

    fn base_capability() -> Capability {
        Capability {
            classes: vec![AgentClass::Audio, AgentClass::Cpu],
            encoders: vec![EncoderId::Eac3, EncoderId::Aac],
            muxers: vec![ContainerId::Matroska],
            effective_cores: 8.0,
            workarea_free_bytes: 1 << 40,
            ..Default::default()
        }
    }

    fn register(
        &self,
        id: &str,
        capability: Capability,
    ) -> tokio::sync::mpsc::Receiver<Result<transcodarr_proto::pb::ServerMessage, tonic::Status>>
    {
        self.runtime
            .writer()
            .submit_blocking(
                WriteLane::Normal,
                AgentRepo::register_op(AgentRegistration {
                    id: id.into(),
                    agent_uid: format!("uid-{id}"),
                    boot_id: format!("boot-{id}"),
                    hostname: None,
                    platform: None,
                    arch: None,
                    agent_version: "1.0.0".into(),
                    proto_version: 1,
                    ffmpeg_version: None,
                    ffprobe_version: None,
                    driver_version: None,
                    classes_json: serde_json::to_string(&capability.classes).unwrap(),
                    capability_json: serde_json::to_string(&capability).unwrap(),
                    capability_hash: format!("hash-{id}"),
                    effective_cores: 8.0,
                    physical_cores: Some(8),
                    mounts_json: "[]".into(),
                    rename_probe_status: "ok".into(),
                    commit_eligible: true,
                    fencing_epoch: 1,
                }),
            )
            .unwrap();

        self.runtime
            .writer()
            .submit_blocking(
                WriteLane::Normal,
                AgentRepo::heartbeat_op(id.to_string(), 3600),
            )
            .unwrap();

        self.fleet.connect(id, 1)
    }

    /// Queue one job with no requirements, so placement is decided by capacity
    /// rather than by capability. Capability matching is covered end to end.
    fn add_job(&self, id: &str) {
        let path = self._dir.path().join(format!("lib/{id}.mkv"));
        let file_id = self
            .runtime
            .writer()
            .submit_blocking(
                WriteLane::Normal,
                FileRepo::upsert_op(FileUpsert {
                    library_id: "lib".into(),
                    canonical_path: path.display().to_string(),
                    path_hash: format!("h-{id}"),
                    size_bytes: 1_000_000,
                    mtime_unix: 1,
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

        self.runtime
            .writer()
            .submit_blocking(
                WriteLane::Normal,
                FileRepo::record_probe_op(
                    file_id,
                    facts(),
                    format!("sig-{id}"),
                    SizeBucket::Small,
                    "{}".into(),
                    "test".into(),
                ),
            )
            .unwrap();

        self.runtime
            .writer()
            .submit_blocking(
                WriteLane::Normal,
                JobRepo::create_op(NewJob {
                    id: id.into(),
                    file_id,
                    library_id: "lib".into(),
                    class: JobClass::Audio,
                    size_bucket: SizeBucket::Small,
                    priority: 0,
                    requirements_json: "[]".into(),
                    requirements_bucket_key: "audio".into(),
                    expected_content_sig: format!("sig-{id}"),
                    rules_version: "v1".into(),
                    parent_job_id: None,
                }),
            )
            .unwrap();
    }

    fn state(&self, job_id: &str) -> JobState {
        self.jobs.get(job_id).unwrap().state
    }

    fn attempt(&self, job_id: &str) -> i64 {
        self.jobs.get(job_id).unwrap().attempt
    }
}

/// Facts the built-in policy owes an audio pass for: a FLAC track.
fn facts() -> FileFacts {
    FileFacts {
        container: "matroska".into(),
        duration_us: Some(2_000_000),
        size_bytes: 1_000_000,
        bit_rate_bps: Some(100_000),
        video_codec: Some("h264".into()),
        video_profile: Some("High".into()),
        video_bit_depth: Some(BitDepth::Eight),
        video_pix_fmt: Some("yuv420p".into()),
        width: Some(160),
        height: Some(120),
        is_hdr: false,
        is_dovi: false,
        dovi_profile: None,
        has_object_audio: false,
        audio_codecs: vec!["flac".into()],
        audio_track_count: 1,
        subtitle_track_count: 0,
    }
}

/// The reconciler returns work to the queue. Something has to pick it up again.
///
/// The loop read only `Pending`, while requeueing lands a job in `Eligible` —
/// so every job an agent dropped sat there permanently, invisible to every
/// later pass, with the queue looking empty and the file never processed.
#[tokio::test(flavor = "multi_thread")]
async fn a_requeued_job_is_dispatched_again() {
    let h = Harness::new();
    let orchestrator = h.orchestrator(AgentLimits::flat(4, 1));
    let rx = h.add_agent("u1");
    h.add_job("job-1");

    orchestrator.tick().await.unwrap();
    assert_eq!(h.state("job-1"), JobState::Assigned);

    // The agent vanishes: its stream closes and the fleet table forgets it.
    drop(rx);
    h.fleet.disconnect("u1", 1);
    h.runtime
        .writer()
        .submit_blocking(
            WriteLane::Normal,
            AgentRepo::heartbeat_op("u1".to_string(), -10_000),
        )
        .unwrap();

    let outcome = orchestrator.tick().await.unwrap();
    assert_eq!(outcome.requeued, vec!["job-1".to_string()]);
    assert_eq!(h.state("job-1"), JobState::Eligible);

    // It comes back, and the job must go out again.
    let _rx = h.add_agent("u1");
    orchestrator.tick().await.unwrap();
    assert_eq!(
        h.state("job-1"),
        JobState::Assigned,
        "a requeued job must be dispatchable again, not stranded"
    );
    assert_eq!(h.attempt("job-1"), 1, "the retry must count as an attempt");
}

/// A job that keeps losing its agent must stop, not cycle forever.
///
/// Without a retry budget one unlucky file occupies a slot every tick for the
/// life of the server, and the queue behind it never moves.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_keeps_failing_is_eventually_dead_lettered() {
    let h = Harness::new();
    let orchestrator = h.orchestrator(AgentLimits::flat(4, 1));
    h.add_job("job-1");

    for _ in 0..10 {
        let rx = h.add_agent("u1");
        orchestrator.tick().await.unwrap();
        drop(rx);
        h.fleet.disconnect("u1", 1);
        h.runtime
            .writer()
            .submit_blocking(
                WriteLane::Normal,
                AgentRepo::heartbeat_op("u1".to_string(), -10_000),
            )
            .unwrap();
        orchestrator.tick().await.unwrap();

        if h.state("job-1") == JobState::DeadLettered {
            return;
        }
    }
    panic!(
        "job never stopped retrying; it is in {:?} at attempt {}",
        h.state("job-1"),
        h.attempt("job-1")
    );
}

/// The ledger is what stops a node being handed more than it can run. Neither
/// it nor the dispatcher's bucket/admission split had ever run with more than
/// one job or one agent.
#[tokio::test(flavor = "multi_thread")]
async fn placement_is_capped_by_the_per_agent_limit() {
    let h = Harness::new();
    let orchestrator = h.orchestrator(AgentLimits::flat(2, 1));
    let _a = h.add_agent("u1");
    let _b = h.add_agent("u2");
    for i in 0..6 {
        h.add_job(&format!("job-{i}"));
    }

    let outcome = orchestrator.tick().await.unwrap();
    assert_eq!(
        outcome.dispatched.len(),
        4,
        "two agents at two slots each: {outcome:?}"
    );

    // And the placement is spread, not piled onto one agent.
    let jobs = JobRepo::new(h.runtime.pool().clone());
    let mut per_agent = std::collections::HashMap::new();
    for id in &outcome.dispatched {
        let agent = jobs.get(id).unwrap().agent_id.unwrap();
        *per_agent.entry(agent).or_insert(0) += 1;
    }
    assert_eq!(
        per_agent.len(),
        2,
        "both agents should be used: {per_agent:?}"
    );
    for (agent, count) in &per_agent {
        assert!(
            *count <= 2,
            "{agent} was given {count} jobs, over its limit"
        );
    }

    // A second pass places nothing more: every slot is held.
    let again = orchestrator.tick().await.unwrap();
    assert!(
        again.dispatched.is_empty(),
        "the ledger should be full: {again:?}"
    );
}

/// A paused schedule must place nothing at all.
///
/// The engine was built and tested and the loop never asked it anything, so an
/// operator pausing the fleet changed nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_schedule_dispatches_nothing() {
    let h = Harness::new();
    let orchestrator =
        h.orchestrator(AgentLimits::flat(4, 1))
            .with_schedule(ScheduleEngine::paused_until(
                i64::MAX,
                "an operator stopped the fleet",
            ));
    let _rx = h.add_agent("u1");
    h.add_job("job-1");

    let outcome = orchestrator.tick().await.unwrap();
    assert!(
        outcome.dispatched.is_empty(),
        "a paused fleet must place nothing: {outcome:?}"
    );
    assert_eq!(h.state("job-1"), JobState::Pending);
}

/// Pull the assignment the server just sent, or say what arrived instead.
fn assignment_from(
    rx: &mut tokio::sync::mpsc::Receiver<
        Result<transcodarr_proto::pb::ServerMessage, tonic::Status>,
    >,
) -> transcodarr_proto::pb::JobAssignment {
    let msg = rx.try_recv().expect("no message was dispatched");
    match msg.expect("the stream carried an error").body {
        Some(transcodarr_proto::pb::server_message::Body::Assignment(a)) => a,
        other => panic!("expected an assignment, got {other:?}"),
    }
}

/// A streaming agent cannot see the library, so nothing it is handed may name
/// it — not `source_path`, not `temp_path`, and not `argv`, which it execs
/// verbatim.
///
/// The assertion is on the *absence* of the library root rather than only on
/// the presence of the work area: an argv that pointed at both would satisfy a
/// positive check and still fail on the agent.
#[tokio::test(flavor = "multi_thread")]
async fn a_streaming_agent_is_never_handed_a_library_path() {
    let h = Harness::new();
    let orchestrator = h.orchestrator(AgentLimits::flat(4, 1));
    let mut rx = h.add_stream_agent("win-rtx2070", "/agent/work");
    h.add_job("job-1");

    orchestrator.tick().await.unwrap();
    assert_eq!(h.state("job-1"), JobState::Assigned);

    let a = assignment_from(&mut rx);
    let library_root = h._dir.path().join("lib").display().to_string();

    assert_eq!(a.source_path, "/agent/work/job-1.0.src.mkv");
    assert_eq!(a.temp_path, "/agent/work/job-1.0.partial.mkv");
    for arg in &a.argv {
        assert!(
            !arg.contains(&library_root),
            "argv names the library, which this agent cannot open: {arg}"
        );
    }
    assert!(
        a.argv.iter().any(|x| x == &a.source_path),
        "argv must read the file the agent was told to fetch; argv = {:?}",
        a.argv
    );
    assert!(
        a.argv.iter().any(|x| x == &a.temp_path),
        "argv must write where the agent will push from; argv = {:?}",
        a.argv
    );

    // The canonical path is still the server's business: it is what gets
    // replaced at install time, and the ledger is where it belongs.
    assert!(
        a.final_path.contains(&library_root),
        "the server still installs to the library"
    );
}

/// A mount agent must be unaffected. This is the mode that already works, and
/// the translation seam is the kind of change that silently breaks it.
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_agent_is_still_handed_the_canonical_path() {
    let h = Harness::new();
    let orchestrator = h.orchestrator(AgentLimits::flat(4, 1));
    let mut rx = h.add_agent("u1");
    h.add_job("job-1");

    orchestrator.tick().await.unwrap();
    let a = assignment_from(&mut rx);

    assert_eq!(
        a.source_path, a.final_path,
        "a mount agent reads the file it will replace"
    );
    assert!(a.argv.iter().any(|x| x == &a.source_path));
}

/// An agent too old to advertise a work area must not be dispatched to.
///
/// Defaulting would join to `/job-1.0.src.mkv` -- the filesystem root -- and
/// the job would fail on the agent, in ffmpeg, three steps from the cause.
#[tokio::test(flavor = "multi_thread")]
async fn a_streaming_agent_with_no_work_area_is_not_dispatched_to() {
    let h = Harness::new();
    let orchestrator = h.orchestrator(AgentLimits::flat(4, 1));
    let mut rx = h.add_stream_agent("too-old", "");
    h.add_job("job-1");

    orchestrator.tick().await.unwrap();

    assert!(
        rx.try_recv().is_err(),
        "a job was dispatched to an agent that cannot be given a usable path"
    );
    assert_ne!(
        h.state("job-1"),
        JobState::Assigned,
        "the job must stay queued for an agent that can run it"
    );
}
