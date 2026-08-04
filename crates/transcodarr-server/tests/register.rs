// file: crates/transcodarr-server/tests/register.rs
// version: 1.0.0
// guid: 8f27b0d5-63a1-4e94-b8c2-15de70a3968f
// last-edited: 2026-08-04
//! Registration over a real gRPC channel.
//!
//! Every test here starts a `tonic` server on a loopback port, dials it with
//! the generated client, and asserts on what came back over the wire. Nothing
//! is called in-process.
//!
//! That distinction earns its cost. The unit tests already cover the version
//! gate and the repository; what they cannot cover is prost's encoding sitting
//! between them — an enum that decodes to its zero variant, a `u64` epoch that
//! does not survive the trip, a field the conversion silently drops. Those only
//! appear when something actually serialises.
//!
//! Note `AgentServiceClient::new(channel)` rather than a `connect(dst)`
//! constructor: codegen runs with `build_transport(false)`, because the
//! generated constructor would collide with the client method for `rpc
//! Connect`.

use std::sync::Arc;

use tonic::transport::{Channel, Server};

use transcodarr_proto::pb;
use transcodarr_server::AgentSession;
use transcodarr_store::repo::{AgentRepo, CommitIntentRepo};
use transcodarr_store::{Db, ReadPool, Writer};

/// A live server on a loopback port, with a client already dialled.
struct Harness {
    client: pb::agent_service_client::AgentServiceClient<Channel>,
    agents: AgentRepo,
    _dir: tempfile::TempDir,
}

async fn harness(auth_token: Option<&str>) -> Harness {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("t.db");
    let writer = Arc::new(Writer::start(Db::open_unchecked(&path).unwrap()));
    let pool = ReadPool::open(&path, 4).unwrap();

    let session = AgentSession::new(
        AgentRepo::new(pool.clone()),
        CommitIntentRepo::new(pool.clone()),
        writer,
        auth_token.map(str::to_string),
    );

    // Port 0: the OS picks a free one, so tests can run concurrently without
    // agreeing on a port between them.
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
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    };

    Harness {
        client: pb::agent_service_client::AgentServiceClient::new(channel),
        agents: AgentRepo::new(pool),
        _dir: dir,
    }
}

fn identity(boot: &str) -> pb::AgentIdentity {
    pb::AgentIdentity {
        agent_id: "u1".into(),
        agent_uid: "uid-1".into(),
        boot_id: boot.into(),
        agent_version: "1.0.0".into(),
        proto_version: transcodarr_proto::PROTO_VERSION,
    }
}

fn capability(rename_probe: pb::RenameProbeStatus) -> pb::Capability {
    pb::Capability {
        platform: "linux".into(),
        effective_cores: 38.0,
        physical_cores: 48,
        ffmpeg_version: "7.1".into(),
        classes: vec!["audio".into()],
        encoders: vec!["eac3".into(), "copy".into()],
        mounts: vec![pb::Mount {
            local_path: "/media".into(),
            canonical_prefix: "/mnt/media".into(),
            writable: true,
            rename_probe: rename_probe as i32,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn request(boot: &str, token: &str) -> pb::RegisterRequest {
    pb::RegisterRequest {
        identity: Some(identity(boot)),
        capability: Some(capability(pb::RenameProbeStatus::RpAtomicVerified)),
        auth_token: token.into(),
        live_intents: Vec::new(),
    }
}

#[tokio::test]
async fn an_agent_registers_and_is_told_its_epoch() {
    let mut h = harness(None).await;
    let res = h
        .client
        .register(request("boot-a", ""))
        .await
        .unwrap()
        .into_inner();

    assert!(res.accepted, "{}", res.reject_reason);
    assert_eq!(res.fencing_epoch, 1, "a first registration starts at 1");
    assert_eq!(res.server_proto_version, transcodarr_proto::PROTO_VERSION);

    let stored = h.agents.get("u1").unwrap().unwrap();
    assert_eq!(stored.fencing_epoch, 1);
    assert_eq!(stored.status, "Online");
    assert!(stored.commit_eligible, "the rename probe passed");
}

/// The rule that a wire format cannot enforce. Bumping on reconnect looks safer
/// and is the opposite: every network blip would invalidate work still running,
/// and the agent would come back to find its own in-flight job fenced off.
#[tokio::test]
async fn a_reconnect_resumes_its_epoch_and_a_restart_bumps_it() {
    let mut h = harness(None).await;

    let first = h
        .client
        .register(request("boot-a", ""))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.fencing_epoch, 1);

    // Same process instance coming back: same epoch.
    let again = h
        .client
        .register(request("boot-a", ""))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        again.fencing_epoch, 1,
        "a reconnect must not fence its own work"
    );

    // A new process instance: everything the old one held is now stale.
    let restarted = h
        .client
        .register(request("boot-b", ""))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(restarted.fencing_epoch, 2);
}

/// A reinstall answers to a name the fleet knows but is not the same
/// installation, so it must not inherit the previous one's epoch — and with it
/// a work area that is not its own.
#[tokio::test]
async fn a_reinstall_under_the_same_name_takes_a_new_epoch() {
    let mut h = harness(None).await;
    h.client.register(request("boot-a", "")).await.unwrap();

    let mut reinstalled = request("boot-a", "");
    reinstalled.identity.as_mut().unwrap().agent_uid = "uid-2".into();
    let res = h.client.register(reinstalled).await.unwrap().into_inner();

    assert!(res.accepted);
    assert_eq!(
        res.fencing_epoch, 2,
        "a new installation must not resume the previous one's epoch"
    );
}

/// The gate is at registration, not at first use: an agent missing a safety fix
/// must never take work at all.
#[tokio::test]
async fn an_agent_speaking_the_wrong_protocol_is_refused_cleanly() {
    let mut h = harness(None).await;
    let mut req = request("boot-a", "");
    req.identity.as_mut().unwrap().proto_version = 99;

    let res = h.client.register(req).await.unwrap().into_inner();
    assert!(!res.accepted);
    assert!(res.reject_reason.contains("upgrade the server"), "{res:?}");

    // The refusal is a clean response, not an error, and it wrote nothing.
    assert!(
        h.agents.get("u1").unwrap().is_none(),
        "a rejected registration must not create a row"
    );
}

/// A rejection must leave a healthy row exactly as it was, or being refused
/// becomes a way to overwrite one.
#[tokio::test]
async fn a_refusal_does_not_disturb_an_existing_registration() {
    let mut h = harness(None).await;
    h.client.register(request("boot-a", "")).await.unwrap();
    let before = h.agents.get("u1").unwrap().unwrap();

    let mut bad = request("boot-b", "");
    bad.identity.as_mut().unwrap().agent_version = "not-a-version".into();
    let res = h.client.register(bad).await.unwrap().into_inner();
    assert!(!res.accepted);

    assert_eq!(
        h.agents.get("u1").unwrap().unwrap(),
        before,
        "a refused registration must change nothing"
    );
}

#[tokio::test]
async fn a_bad_token_is_refused_and_a_good_one_is_not() {
    let mut h = harness(Some("s3cret")).await;

    let refused = h
        .client
        .register(request("boot-a", "wrong"))
        .await
        .unwrap()
        .into_inner();
    assert!(!refused.accepted);
    assert_eq!(refused.reject_reason, "authentication failed");
    assert!(h.agents.get("u1").unwrap().is_none());

    let accepted = h
        .client
        .register(request("boot-a", "s3cret"))
        .await
        .unwrap()
        .into_inner();
    assert!(accepted.accepted, "{}", accepted.reject_reason);
}

/// `commit_eligible` is the Phase 0 rename probe reaching the scheduler. A node
/// that cannot rename over an open destination may produce output but must
/// never install it, and `RP_UNTESTED` grants nothing — absence of a trial is
/// not evidence of success.
#[tokio::test]
async fn an_unproven_rename_probe_does_not_grant_commit_eligibility() {
    let mut h = harness(None).await;
    let mut req = request("boot-a", "");
    req.capability = Some(capability(pb::RenameProbeStatus::RpUntested));

    let res = h.client.register(req).await.unwrap().into_inner();
    assert!(
        res.accepted,
        "an unproven probe is not a refusal to register"
    );
    assert!(
        !h.agents.get("u1").unwrap().unwrap().commit_eligible,
        "untested must never satisfy commit eligibility"
    );
}

/// The conversion boundary refuses what it does not recognise rather than
/// defaulting it, and that refusal has to survive the wire as a refusal.
#[tokio::test]
async fn a_capability_naming_an_unknown_class_is_refused() {
    let mut h = harness(None).await;
    let mut req = request("boot-a", "");
    req.capability.as_mut().unwrap().classes = vec!["quantum".into()];

    let err = h.client.register(req).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("classes"), "{}", err.message());
}

/// An install the agent was in the middle of that this server has no live
/// record of. Naming it back is what lets the agent clean up rather than sit on
/// a staged file forever.
#[tokio::test]
async fn an_intent_the_server_never_granted_is_named_back_to_the_agent() {
    let mut h = harness(None).await;
    let mut req = request("boot-a", "");
    req.live_intents = vec![pb::LiveIntent {
        job_id: "ghost-job".into(),
        attempt: 1,
        fencing_epoch: 1,
        phase: "granted".into(),
        temp_path: "/w/ghost.partial.mkv".into(),
        final_path: "/mnt/media/ghost.mkv".into(),
        trash_path: String::new(),
    }];

    let res = h.client.register(req).await.unwrap().into_inner();
    assert!(res.accepted);
    assert_eq!(res.unknown_job_ids, vec!["ghost-job".to_string()]);
}

/// Refused rather than faked. A stream that accepted assignments with no
/// dispatch loop behind it would hand out work nothing is accounting for.
#[tokio::test]
async fn connect_is_refused_explicitly_until_it_is_served() {
    let mut h = harness(None).await;
    let err = h.client.connect(tokio_stream::empty()).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}
