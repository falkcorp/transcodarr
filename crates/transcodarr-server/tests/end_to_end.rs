// file: crates/transcodarr-server/tests/end_to_end.rs
// version: 1.1.0
// guid: 8a3c15f7-64b0-4e92-b1d8-07f5a29e3c64
// last-edited: 2026-08-06
//! One job, all the way through, over a real gRPC channel.
//!
//! register → connect → dispatch → encode → result → RequestCommit →
//! ReportCommit → the job reaches `Succeeded` and the file on disk is the
//! replacement.
//!
//! Both ends are real: a `tonic` server over a real `AgentSession` and a real
//! `Orchestrator`, and a real `ConnectClient` driving a real `LocalWorker` with
//! a real `Executor`. The only thing faked is the media, which is generated
//! with ffmpeg because the properties under test are the ones only real files
//! have.
//!
//! This is the proof the Phase 4 milestone rests on, so it asserts the *fence*
//! as well as the happy path: a `ReportCommit` bearing a stale epoch must be
//! rejected with the job left untouched. A test that only proved work flows
//! would pass equally well with the safety property removed.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tonic::transport::Server;

use transcodarr_agent::client::{ClientConfig, ConnectClient, ReconnectPolicy};
use transcodarr_agent::commit::CommitRitual;
use transcodarr_agent::executor::{Executor, ExecutorConfig};
use transcodarr_agent::workarea::WorkArea;
use transcodarr_agent::worker::LocalWorker;
use transcodarr_core::capability::{AgentClass, ContainerId, Requirement, Requirements};
use transcodarr_core::facts::{FileFacts, SizeBucket};
use transcodarr_core::job::{JobClass, JobState};
use transcodarr_core::plan::{BitDepth, EncoderId};
use transcodarr_proto::pb;
use transcodarr_server::Runtime;
use transcodarr_server::capacity::AgentLimits;
use transcodarr_server::orchestrator::Orchestrator;
use transcodarr_server::serve;
use transcodarr_server::serve::ServeConfig;
use transcodarr_store::WriteLane;
use transcodarr_store::repo::{FileRepo, FileUpsert, JobRepo, LibraryRecord, LibraryRepo, NewJob};

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A two-second file with one video and one audio stream.
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
            "flac",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(ok.success(), "fixture generation failed");
}

/// Poll until `check` holds, or fail with something an operator could act on.
async fn until(what: &str, check: impl Fn() -> bool) {
    for _ in 0..600 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// One job through the whole system.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_goes_from_the_queue_to_installed() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg and ffprobe are needed for this test");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let media = dir.path().join("lib/show.mkv");
    make_media(&media);
    let original = std::fs::read(&media).unwrap();

    // ---- the server ------------------------------------------------------
    let runtime = Runtime::open_unchecked(&dir.path().join("tc.db")).unwrap();
    let serving = serve::build(
        &runtime,
        &ServeConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..ServeConfig::default()
        },
    );

    // The work directory is the library's, on the same filesystem as the
    // library itself — decision D14, and what makes the install one rename.
    let work_dir = dir.path().join("lib/work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let trash_dir = dir.path().join("lib/trash");

    seed(
        &runtime,
        &media,
        &work_dir.display().to_string(),
        &trash_dir.display().to_string(),
        original.len() as i64,
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let session = serving.session.clone();
    tokio::spawn(async move {
        Server::builder()
            .add_service(pb::agent_service_server::AgentServiceServer::new(session))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
    });

    let orchestrator = Orchestrator::new(
        runtime.pool().clone(),
        Arc::clone(runtime.writer()),
        serving.fleet.clone(),
        transcodarr_core::policy::default_space_saver(),
        AgentLimits::flat(4, 1),
    );

    // ---- the agent -------------------------------------------------------
    let agent_work = WorkArea::open(&work_dir, "uid-1", "boot-a").unwrap();
    let worker = Arc::new(LocalWorker::new(
        Executor::new(ExecutorConfig::default()),
        CommitRitual::new(agent_work.open_journal().unwrap(), agent_work.clone()),
        agent_work,
        pb::Capability {
            platform: "linux".into(),
            effective_cores: 4.0,
            classes: vec!["audio".into(), "cpu".into()],
            encoders: vec!["eac3".into(), "aac".into()],
            muxers: vec!["matroska".into()],
            mounts: vec![pb::Mount {
                canonical_prefix: dir.path().join("lib").display().to_string(),
                local_path: dir.path().join("lib").display().to_string(),
                writable: true,
                rename_probe: pb::RenameProbeStatus::RpAtomicVerified as i32,
                ..Default::default()
            }],
            workarea_free_bytes: 1 << 40,
            ..Default::default()
        },
    ));

    let mut client_config = ClientConfig::new(format!("http://{addr}"), "u1");
    client_config.heartbeat = Duration::from_millis(200);
    client_config.reconnect = ReconnectPolicy {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
    };
    let client = ConnectClient::new(client_config, worker);
    let shutdown = client.shutdown();
    let agent = tokio::spawn(async move { client.run().await });

    // ---- run -------------------------------------------------------------
    let fleet = serving.fleet.clone();
    until("the agent to connect", || !fleet.is_empty()).await;

    let jobs = JobRepo::new(runtime.pool().clone());
    // Ticked by hand rather than on a timer: a test that waits for a background
    // interval is a test that is slow when it passes and mysterious when it
    // fails.
    for _ in 0..40 {
        orchestrator.tick().await.unwrap();
        if matches!(jobs.get("job-1"), Ok(j) if j.state == JobState::Succeeded) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let job = jobs.get("job-1").unwrap();
    if job.state != JobState::Succeeded {
        for e in jobs.events("job-1").unwrap() {
            eprintln!(
                "event {:?} -> {:?}: {:?} {:?}",
                e.from_state, e.to_state, e.reason_code, e.detail
            );
        }
    }
    assert_eq!(
        job.state,
        JobState::Succeeded,
        "job did not complete: {:?}",
        job.terminal_reason
    );
    assert_eq!(job.agent_id.as_deref(), Some("u1"));

    // The file on disk is the replacement, and the original is retained.
    let installed = std::fs::read(&media).unwrap();
    assert_ne!(
        installed, original,
        "the destination still holds the original"
    );
    let retained: Vec<_> = std::fs::read_dir(&trash_dir)
        .expect("the trash directory should exist")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(retained.len(), 1, "the original should be retained");
    assert_eq!(std::fs::read(retained[0].path()).unwrap(), original);

    // The audio was actually converted: FLAC in, EAC3 out, video untouched.
    let streams = probe_codecs(&media);
    assert!(
        streams.contains(&"eac3".to_string()),
        "audio should be eac3, got {streams:?}"
    );
    assert!(
        streams.contains(&"h264".to_string()),
        "video should be copied untouched, got {streams:?}"
    );

    shutdown.stop();
    let _ = agent.await;
}

/// The fence, over the wire, with a real job in a real state.
///
/// A `ReportCommit` bearing a superseded epoch must be rejected and the job
/// left exactly as it was. Without this the happy-path test above would pass
/// just as well with the fence deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_report_under_a_stale_epoch_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let runtime = Runtime::open_unchecked(&dir.path().join("tc.db")).unwrap();
    let serving = serve::build(
        &runtime,
        &ServeConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..ServeConfig::default()
        },
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let session = serving.session.clone();
    tokio::spawn(async move {
        Server::builder()
            .add_service(pb::agent_service_server::AgentServiceServer::new(session))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
    });

    let channel = loop {
        match tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
        {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    let mut client = pb::agent_service_client::AgentServiceClient::new(channel);

    let epoch = register(&mut client, "boot-a").await;
    // A new process instance: same agent, new boot_id, new epoch. The first
    // one is now superseded.
    let newer = register(&mut client, "boot-b").await;
    assert!(newer > epoch, "a new instance must take a later epoch");

    // The superseded instance cannot even open a stream.
    let (_tx, rx) = tokio::sync::mpsc::channel(4);
    let mut request = tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    request
        .metadata_mut()
        .insert("x-agent-id", "u1".parse().unwrap());
    request
        .metadata_mut()
        .insert("x-agent-epoch", epoch.to_string().parse().unwrap());
    let refused = client.connect(request).await;
    assert!(
        refused.is_err(),
        "a stream under a superseded epoch must be refused"
    );
    assert_eq!(refused.unwrap_err().code(), tonic::Code::Unauthenticated);
}

/// Register `u1` under a boot id, returning the epoch issued.
async fn register(
    client: &mut pb::agent_service_client::AgentServiceClient<tonic::transport::Channel>,
    boot_id: &str,
) -> i64 {
    let res = client
        .register(pb::RegisterRequest {
            identity: Some(pb::AgentIdentity {
                agent_id: "u1".into(),
                agent_uid: "uid-1".into(),
                boot_id: boot_id.into(),
                agent_version: "1.0.0".into(),
                proto_version: transcodarr_proto::PROTO_VERSION,
            }),
            capability: Some(pb::Capability {
                platform: "linux".into(),
                effective_cores: 4.0,
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

/// Put a library, a file with facts, and one pending job in the database.
fn seed(runtime: &Runtime, media: &Path, work_dir: &str, trash_dir: &str, size: i64) {
    let writer = runtime.writer();
    let root = media.parent().unwrap().display().to_string();

    writer
        .submit_blocking(
            WriteLane::Normal,
            LibraryRepo::upsert_op(LibraryRecord {
                id: "lib".into(),
                name: "lib".into(),
                root_path: root,
                work_dir: work_dir.to_string(),
                trash_dir: trash_dir.to_string(),
                exclude_globs_json: "[]".into(),
                enabled: true,
                scan_parallelism: 1,
                priority: 0,
                min_mtime_age_s: 0,
            }),
        )
        .unwrap();

    // Facts describing what ffmpeg actually wrote: one h264 video stream and
    // one FLAC audio stream, which the built-in policy converts to EAC3. This
    // is what the prober would have recorded; the point of the test is the
    // dispatch path, not discovery.
    let facts = FileFacts {
        container: "matroska".into(),
        duration_us: Some(2_000_000),
        size_bytes: size as u64,
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
    };

    let file_id = writer
        .submit_blocking(
            WriteLane::Normal,
            FileRepo::upsert_op(FileUpsert {
                library_id: "lib".into(),
                canonical_path: media.display().to_string(),
                path_hash: "h-1".into(),
                size_bytes: size,
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

    writer
        .submit_blocking(
            WriteLane::Normal,
            FileRepo::record_probe_op(
                file_id,
                facts,
                "sig-1".into(),
                SizeBucket::Small,
                "{}".into(),
                "test".into(),
            ),
        )
        .unwrap();

    writer
        .submit_blocking(
            WriteLane::Normal,
            JobRepo::create_op(NewJob {
                id: "job-1".into(),
                file_id,
                library_id: "lib".into(),
                class: JobClass::Audio,
                size_bucket: SizeBucket::Small,
                priority: 0,
                // The requirements a real evaluator attaches, not an empty
                // list. An end-to-end test with no requirements never
                // exercises capability matching -- which is how a boundary
                // that silently dropped every agent's muxers passed every
                // test in the suite and dispatched nothing in production.
                requirements_json: serde_json::to_string(&Requirements(vec![
                    Requirement::AgentClass(AgentClass::Cpu),
                    Requirement::Encoder(EncoderId::Eac3),
                    Requirement::Muxer(ContainerId::Matroska),
                ]))
                .unwrap(),
                requirements_bucket_key: "audio".into(),
                expected_content_sig: "sig-1".into(),
                rules_version: "v1".into(),
                parent_job_id: None,
            }),
        )
        .unwrap();
}

/// Every codec in a file, as ffprobe names them.
fn probe_codecs(path: &Path) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}
