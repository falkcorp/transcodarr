// file: crates/transcodarr-agent/tests/stream_transport.rs
// version: 1.0.0
// guid: b3f7d215-8e4a-49c0-97d6-5c218ba0e63f
// last-edited: 2026-08-16
//! `FetchSource` and `PushOutput` from the agent's side, over a real transport.
//!
//! ## Why this fixture exists rather than an extension of the one next door
//!
//! `connect_client.rs`'s `FakeServer` refuses to serve or accept bytes on
//! purpose, and its comment says why: a fake that returned an empty stream
//! would let a streaming test pass while moving nothing. Teaching it to stream
//! would delete that guarantee for every test in that file. This is a second
//! fixture that *does* move bytes, so both properties survive.
//!
//! ## What is real here
//!
//! The transport, the codec, the metadata, and the [`transfer::Sink`] on the
//! receiving end — the same `Sink` the production server stages with. Only the
//! server's database and commit ritual are stubbed, because an agent test that
//! linked `transcodarr-server` would drag SQLite onto the Windows node this
//! agent has to run on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use transcodarr_agent::client::{ClientConfig, ConnectClient, Link, ReconnectPolicy, Worker};
use transcodarr_proto::pb;
use transcodarr_proto::transfer;

/// The epoch this fixture issues, matching the one `connect_client.rs` uses.
const EPOCH: u64 = 7;

// --------------------------------------------------------------- the fixture

/// How the server should behave, and what it saw.
#[derive(Default)]
struct StreamState {
    /// Bytes `FetchSource` serves.
    source: Vec<u8>,
    /// Fail the fetch *inside* the stream after this many data chunks.
    ///
    /// This is how the real server reports a missing source — a `Status` within
    /// the stream, not a transport-level error — and it is the case a reader
    /// can mistake for a clean end.
    fail_fetch_after: Option<usize>,
    /// Send every data chunk but never the terminator, then close cleanly.
    ///
    /// A sender that died mid-file looks exactly like this from the receiving
    /// end, which is why `last` is explicit in the proto.
    drop_terminator: bool,
    /// Metadata seen on each `FetchSource`.
    fetch_metadata: Vec<HashMap<String, String>>,
    /// Metadata seen on each `PushOutput`.
    push_metadata: Vec<HashMap<String, String>>,
    /// Whatever a completed push delivered.
    pushed: Option<Vec<u8>>,
    /// Why the last push was refused, if it was.
    push_error: Option<String>,
    /// Refuse the push before reading a chunk, as a stale-epoch push is.
    refuse_push: bool,
    /// Everything the agent sent up the `Connect` stream.
    inbound: Vec<pb::AgentMessage>,
}

#[derive(Clone, Default)]
struct StreamServer {
    state: Arc<Mutex<StreamState>>,
    /// Where a push is staged. One per fixture.
    staging: Arc<Mutex<Option<std::path::PathBuf>>>,
    assignments: Arc<Mutex<Vec<pb::JobAssignment>>>,
    connected: Arc<AtomicUsize>,
}

impl StreamServer {
    fn state(&self) -> std::sync::MutexGuard<'_, StreamState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Metadata as a plain map, for assertions.
fn metadata_of<T>(request: &Request<T>) -> HashMap<String, String> {
    request
        .metadata()
        .iter()
        .filter_map(|kv| match kv {
            tonic::metadata::KeyAndValueRef::Ascii(k, v) => {
                Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))
            }
            tonic::metadata::KeyAndValueRef::Binary(..) => None,
        })
        .collect()
}

/// The identity gate the real server applies, in miniature.
///
/// Present so an unstamped transfer fails here the way it would in production
/// rather than sailing through a permissive fake. The real `fetch_source` reads
/// identity from exactly this metadata and refuses without it.
// Same allowance the production handlers carry: a `Status` is large and every
// tonic signature in the tree returns one by value.
#[allow(clippy::result_large_err)]
fn require_identity(md: &HashMap<String, String>) -> Result<(), Status> {
    let id = md
        .get("x-agent-id")
        .ok_or_else(|| Status::unauthenticated("no x-agent-id"))?;
    let epoch = md
        .get("x-agent-epoch")
        .ok_or_else(|| Status::unauthenticated("no x-agent-epoch"))?;
    if id.is_empty() {
        return Err(Status::unauthenticated("empty x-agent-id"));
    }
    if epoch != &EPOCH.to_string() {
        return Err(Status::unauthenticated(format!(
            "epoch {epoch} is not the one this agent was issued"
        )));
    }
    Ok(())
}

#[tonic::async_trait]
impl pb::agent_service_server::AgentService for StreamServer {
    type FetchSourceStream = tokio_stream::wrappers::ReceiverStream<Result<pb::FileChunk, Status>>;

    async fn fetch_source(
        &self,
        request: Request<pb::FetchSourceRequest>,
    ) -> Result<Response<Self::FetchSourceStream>, Status> {
        let md = metadata_of(&request);
        self.state().fetch_metadata.push(md.clone());
        require_identity(&md)?;

        let req = request.into_inner();
        let (source, fail_after, drop_terminator) = {
            let state = self.state();
            (
                state.source.clone(),
                state.fail_fetch_after,
                state.drop_terminator,
            )
        };

        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut hasher = blake3::Hasher::new();
            let mut offset = 0u64;
            let mut sent = 0usize;

            for slice in source.chunks(transfer::CHUNK_BYTES) {
                if fail_after == Some(sent) {
                    // The shape that matters: an error *inside* the stream,
                    // after some good bytes have already landed.
                    let _ = tx
                        .send(Err(Status::not_found("the source vanished mid-read")))
                        .await;
                    return;
                }
                hasher.update(slice);
                let _ = tx
                    .send(Ok(pb::FileChunk {
                        job_id: req.job_id.clone(),
                        attempt: req.attempt,
                        offset,
                        data: slice.to_vec(),
                        last: false,
                        content_sig: String::new(),
                    }))
                    .await;
                offset += slice.len() as u64;
                sent += 1;
            }

            if fail_after == Some(sent) {
                let _ = tx
                    .send(Err(Status::not_found("the source vanished mid-read")))
                    .await;
                return;
            }

            if drop_terminator {
                // Dropping `tx` ends the stream with no error and no `last`.
                return;
            }

            let _ = tx
                .send(Ok(pb::FileChunk {
                    job_id: req.job_id,
                    attempt: req.attempt,
                    offset,
                    data: Vec::new(),
                    last: true,
                    content_sig: hasher.finalize().to_hex().to_string(),
                }))
                .await;
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn push_output(
        &self,
        request: Request<tonic::Streaming<pb::FileChunk>>,
    ) -> Result<Response<pb::PushOutputResponse>, Status> {
        let md = metadata_of(&request);
        self.state().push_metadata.push(md.clone());
        require_identity(&md)?;

        if self.state().refuse_push {
            return Ok(Response::new(pb::PushOutputResponse {
                accepted: false,
                reason: "the epoch this push claims has been retired".into(),
                bytes_received: 0,
            }));
        }

        let staged = self
            .staging
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("the fixture must be given a staging path");

        // The production `Sink`, not a re-implementation: the offset gap check
        // and the blake3 gate are the things under test on this side.
        let mut sink = transfer::Sink::create(&staged)
            .map_err(|e| Status::internal(format!("cannot stage: {e}")))?;
        let mut chunks = request.into_inner();

        loop {
            match chunks.message().await? {
                Some(chunk) => match sink.accept(&chunk) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(e) => {
                        self.state().push_error = Some(e.clone());
                        return Ok(Response::new(pb::PushOutputResponse {
                            accepted: false,
                            reason: e,
                            bytes_received: 0,
                        }));
                    }
                },
                None => {
                    let e = "the stream ended without a final chunk".to_string();
                    self.state().push_error = Some(e.clone());
                    return Ok(Response::new(pb::PushOutputResponse {
                        accepted: false,
                        reason: e,
                        bytes_received: sink.written(),
                    }));
                }
            }
        }

        let bytes = std::fs::read(&staged).unwrap_or_default();
        let received = sink.written();
        self.state().pushed = Some(bytes);
        Ok(Response::new(pb::PushOutputResponse {
            accepted: true,
            reason: String::new(),
            bytes_received: received,
        }))
    }

    async fn register(
        &self,
        _request: Request<pb::RegisterRequest>,
    ) -> Result<Response<pb::RegisterResponse>, Status> {
        Ok(Response::new(pb::RegisterResponse {
            accepted: true,
            server_proto_version: transcodarr_proto::PROTO_VERSION,
            min_supported_proto: transcodarr_proto::MIN_SUPPORTED_PROTO,
            fencing_epoch: EPOCH,
            ..Default::default()
        }))
    }

    type ConnectStream = tokio_stream::wrappers::ReceiverStream<Result<pb::ServerMessage, Status>>;

    async fn connect(
        &self,
        request: Request<tonic::Streaming<pb::AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let (tx, rx) = mpsc::channel(16);
        // Cloned out before any await: a `MutexGuard` held across one makes
        // this future non-`Send`, which tonic requires.
        let assignments = self
            .assignments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for a in assignments {
            let _ = tx
                .send(Ok(pb::ServerMessage {
                    body: Some(pb::server_message::Body::Assignment(a)),
                }))
                .await;
        }
        self.connected.fetch_add(1, Ordering::SeqCst);

        let mut stream = request.into_inner();
        let state = self.state.clone();
        tokio::spawn(async move {
            // `tx` is moved in and held for the life of the inbound loop. Let
            // it drop at the end of this method instead and the server stream
            // closes immediately, the client reconnects, and every assignment
            // is delivered again — which reads as the agent doing the job
            // three times rather than as a fixture that hung up.
            let _keep_open = tx;
            while let Ok(Some(msg)) = stream.message().await {
                state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .inbound
                    .push(msg);
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

async fn serve() -> (StreamServer, String) {
    let fake = StreamServer::default();
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

// ------------------------------------------------------------ the test worker

/// A worker whose only job is to hand its [`Link`] back to the test.
///
/// The link comes from the real client, built at the real call site, so a
/// transfer here exercises the same handle `LocalWorker` is given.
struct LinkCatcher {
    tx: Mutex<Option<tokio::sync::oneshot::Sender<Link>>>,
}

#[tonic::async_trait]
impl Worker for LinkCatcher {
    fn capability(&self) -> pb::Capability {
        pb::Capability {
            platform: "linux".into(),
            effective_cores: 4.0,
            classes: vec!["audio".into()],
            transport: pb::TransportMode::TmStream as i32,
            workarea_path: "/tmp/work".into(),
            ..Default::default()
        }
    }

    fn live_intents(&self) -> Vec<pb::LiveIntent> {
        Vec::new()
    }

    fn running_job_ids(&self) -> Vec<String> {
        Vec::new()
    }

    async fn on_unknown_intents(&self, _job_ids: Vec<String>) {}

    async fn on_assignment(&self, _assignment: pb::JobAssignment, link: Link) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(link);
        }
    }

    async fn on_revoke(&self, _job_id: String, _reason: String) {}

    async fn on_drain(&self, _drain: pb::Drain) -> Vec<String> {
        Vec::new()
    }
}

/// Bring up a server and a connected agent, and return the `Link` the agent's
/// worker was handed.
async fn linked(fake: &StreamServer, endpoint: &str) -> (Link, transcodarr_agent::Shutdown) {
    fake.assignments
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(pb::JobAssignment {
            job_id: "j1".into(),
            attempt: 0,
            ..Default::default()
        });

    let (tx, rx) = tokio::sync::oneshot::channel();
    let worker = Arc::new(LinkCatcher {
        tx: Mutex::new(Some(tx)),
    });

    let mut config = ClientConfig::new(endpoint, "u1");
    config.heartbeat = Duration::from_millis(500);
    config.reconnect = ReconnectPolicy {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(50),
    };

    let client = ConnectClient::new(config, worker);
    let shutdown = client.shutdown();
    tokio::spawn(async move { client.run().await });

    let link = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("the assignment never arrived")
        .expect("the worker never handed the link back");
    (link, shutdown)
}

/// A body big enough to cross the chunk boundary, so ordering and offsets are
/// actually exercised. A single-chunk transfer passes even if the offset
/// bookkeeping is wrong, because there is only ever one offset and it is zero.
fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

// ------------------------------------------------------------------ the tests

#[tokio::test(flavor = "multi_thread")]
async fn a_fetched_source_arrives_whole_and_in_order() {
    let (fake, endpoint) = serve().await;
    let expected = body(transfer::CHUNK_BYTES * 2 + 1234);
    fake.state().source = expected.clone();

    let (link, shutdown) = linked(&fake, &endpoint).await;
    let dir = tempfile::TempDir::new().unwrap();
    let dest = dir.path().join("j1.0.src.mkv");

    let bytes = link.fetch_source("j1", 0, &dest).await.unwrap();

    assert_eq!(bytes, expected.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    shutdown.stop();
}

/// The trap this test exists for: the server reports a missing source as a
/// `Status` *inside* the stream. A reader that treats a stream error as the end
/// of the stream turns that into a short, well-formed file — and because the
/// signature never arrives, nothing else would catch it either.
#[tokio::test(flavor = "multi_thread")]
async fn an_error_inside_the_stream_fails_the_fetch_rather_than_ending_it() {
    let (fake, endpoint) = serve().await;
    fake.state().source = body(transfer::CHUNK_BYTES * 3);
    // Two good chunks land, then the error. The bytes on disk at that point are
    // a clean prefix, which is exactly what makes this failure survivable.
    fake.state().fail_fetch_after = Some(2);

    let (link, shutdown) = linked(&fake, &endpoint).await;
    let dir = tempfile::TempDir::new().unwrap();
    let dest = dir.path().join("j1.0.src.mkv");

    let err = link
        .fetch_source("j1", 0, &dest)
        .await
        .expect_err("a fetch that hit an in-stream error must not report success");

    assert!(
        err.to_string().contains("the source stream failed"),
        "the error must name the stream failure, not a later symptom: {err}"
    );
    assert!(
        !dest.exists(),
        "a failed fetch must remove what it wrote; a partial source would be \
         encoded as though it were whole"
    );
    shutdown.stop();
}

/// A stream that simply stops is indistinguishable from a sender that died.
/// Without a final chunk there is no signature, so nothing downstream can tell
/// a complete transfer from a truncated one — it has to be refused here, and
/// refused loudly rather than accepted as a short file.
#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_that_ends_without_a_terminator_is_refused() {
    let (fake, endpoint) = serve().await;
    fake.state().source = body(transfer::CHUNK_BYTES * 2);
    fake.state().drop_terminator = true;

    let (link, shutdown) = linked(&fake, &endpoint).await;
    let dir = tempfile::TempDir::new().unwrap();
    let dest = dir.path().join("j1.0.src.mkv");

    let err = link
        .fetch_source("j1", 0, &dest)
        .await
        .expect_err("a stream that stopped is not a stream that finished");

    assert!(
        err.to_string().contains("without a final chunk"),
        "the error must say the transfer never completed: {err}"
    );
    assert!(
        !dest.exists(),
        "the bytes that did arrive are a clean prefix and must not be left \
         behind looking like a whole source"
    );
    shutdown.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_carries_this_agent_s_identity() {
    let (fake, endpoint) = serve().await;
    fake.state().source = body(64);

    let (link, shutdown) = linked(&fake, &endpoint).await;
    let dir = tempfile::TempDir::new().unwrap();
    link.fetch_source("j1", 0, &dir.path().join("s.mkv"))
        .await
        .unwrap();

    let md = fake.state().fetch_metadata[0].clone();
    assert_eq!(
        md.get("x-agent-id").map(String::as_str),
        Some("u1"),
        "an unstamped fetch is refused with an opaque Unauthenticated, which \
         names nothing an operator can act on"
    );
    assert_eq!(
        md.get("x-agent-epoch").map(String::as_str),
        Some("7"),
        "the epoch must be the one the server issued at Register, not the one \
         the assignment happened to carry"
    );
    shutdown.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pushed_output_arrives_whole_and_verifies() {
    let (fake, endpoint) = serve().await;
    let dir = tempfile::TempDir::new().unwrap();
    let staged = dir.path().join("staged.mkv");
    *fake.staging.lock().unwrap() = Some(staged.clone());

    let expected = body(transfer::CHUNK_BYTES * 2 + 77);
    let out = dir.path().join("j1.0.partial.mkv");
    std::fs::write(&out, &expected).unwrap();

    let (link, shutdown) = linked(&fake, &endpoint).await;
    let response = link.push_output("j1", 0, &out).await.unwrap();

    assert!(response.accepted, "refused: {}", response.reason);
    assert_eq!(response.bytes_received, expected.len() as u64);
    assert_eq!(
        fake.state().pushed.clone().unwrap(),
        expected,
        "the bytes that landed must be the bytes that were sent; the receiving \
         Sink verifies a whole-file blake3 before any of this is usable"
    );
    shutdown.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_carries_this_agent_s_identity() {
    let (fake, endpoint) = serve().await;
    let dir = tempfile::TempDir::new().unwrap();
    *fake.staging.lock().unwrap() = Some(dir.path().join("staged.mkv"));
    let out = dir.path().join("out.mkv");
    std::fs::write(&out, body(32)).unwrap();

    let (link, shutdown) = linked(&fake, &endpoint).await;
    link.push_output("j1", 0, &out).await.unwrap();

    let md = fake.state().push_metadata[0].clone();
    assert_eq!(md.get("x-agent-id").map(String::as_str), Some("u1"));
    assert_eq!(md.get("x-agent-epoch").map(String::as_str), Some("7"));
    shutdown.stop();
}

/// An output that is not on disk must fail before an RPC starts. A transfer
/// that opens and then stops looks, from the server's side, like a network
/// fault rather than a missing file.
#[tokio::test(flavor = "multi_thread")]
async fn pushing_an_output_that_is_not_there_fails_before_the_rpc() {
    let (fake, endpoint) = serve().await;
    let dir = tempfile::TempDir::new().unwrap();
    *fake.staging.lock().unwrap() = Some(dir.path().join("staged.mkv"));

    let (link, shutdown) = linked(&fake, &endpoint).await;
    let err = link
        .push_output("j1", 0, &dir.path().join("nothing-here.mkv"))
        .await
        .expect_err("a missing output must not be pushed");

    assert!(err.to_string().contains("cannot read"), "{err}");
    assert!(
        fake.state().push_metadata.is_empty(),
        "the RPC must never have been made"
    );
    shutdown.stop();
}

/// A refusal is an answer, not a fault. The agent must read the reason rather
/// than treating a refused install as a transport error to retry.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_push_comes_back_as_a_reason_not_an_error() {
    let (fake, endpoint) = serve().await;
    let dir = tempfile::TempDir::new().unwrap();
    *fake.staging.lock().unwrap() = Some(dir.path().join("staged.mkv"));
    fake.state().refuse_push = true;
    let out = dir.path().join("out.mkv");
    std::fs::write(&out, body(32)).unwrap();

    let (link, shutdown) = linked(&fake, &endpoint).await;
    let response = link.push_output("j1", 0, &out).await.unwrap();

    assert!(!response.accepted);
    assert!(response.reason.contains("retired"), "{}", response.reason);
    shutdown.stop();
}

// ------------------------------------------------- the whole path, real bytes

fn have(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn make_media(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let ok = std::process::Command::new("ffmpeg")
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
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "could not build the test media");
}

/// Duration in microseconds, straight from ffprobe.
fn duration_us(path: &std::path::Path) -> u64 {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .expect("ffprobe");
    let text = String::from_utf8_lossy(&out.stdout);
    let seconds: f64 = text.trim().parse().unwrap_or(0.0);
    (seconds * 1_000_000.0) as u64
}

/// The whole streaming path on real media: fetch, encode, push, install.
///
/// **`final_path` deliberately names a directory that does not exist on this
/// machine.** Under `TM_STREAM` it is an identifier in the server's namespace,
/// and the agent must never stat it. If this worker took the mount path by
/// mistake, `ensure_same_device` and `SourceGuard::observe` both touch that
/// path and the job would fail here rather than quietly passing — which is
/// what would happen if the test pointed it at a file that happens to exist
/// locally, as it would on a single-machine fixture that was not thinking
/// about it.
#[tokio::test(flavor = "multi_thread")]
async fn a_streaming_agent_fetches_encodes_and_pushes_real_media() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg and ffprobe are needed for this test");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();

    // The server's side: a real file the agent has no path to.
    let library = dir.path().join("server-library/a.mkv");
    make_media(&library);
    let source_bytes = std::fs::read(&library).unwrap();
    let staged = dir.path().join("server-staging/j1.0.partial.mkv");
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();

    let (fake, endpoint) = serve().await;
    fake.state().source = source_bytes.clone();
    *fake.staging.lock().unwrap() = Some(staged.clone());

    // The agent's side: both ends of the job inside its own work area, exactly
    // as `core::plan::agent_job_paths` composes them.
    let work_root = dir.path().join("agent-work");
    let work = transcodarr_agent::workarea::WorkArea::open(&work_root, "uid-1", "boot-a").unwrap();
    let agent_source = work_root.join("j1.0.src.mkv");
    let agent_temp = work_root.join("j1.0.partial.mkv");

    let capability = pb::Capability {
        platform: "linux".into(),
        effective_cores: 4.0,
        classes: vec!["audio".into()],
        transport: pb::TransportMode::TmStream as i32,
        workarea_path: work_root.display().to_string(),
        ..Default::default()
    };

    let ritual = transcodarr_agent::commit::CommitRitual::new(work.open_journal().unwrap(), work);
    let worker = Arc::new(transcodarr_agent::worker::LocalWorker::new(
        transcodarr_agent::executor::Executor::new(
            transcodarr_agent::executor::ExecutorConfig::default(),
        ),
        ritual,
        transcodarr_agent::workarea::WorkArea::open(&work_root, "uid-1", "boot-a").unwrap(),
        capability,
    ));

    // A path in a namespace this agent does not have. See the doc comment.
    let final_path = "/no-such-library-on-this-machine/a.mkv";

    fake.assignments.lock().unwrap().push(pb::JobAssignment {
        job_id: "j1".into(),
        attempt: 0,
        fencing_epoch: EPOCH,
        source_path: agent_source.display().to_string(),
        final_path: final_path.into(),
        temp_path: agent_temp.display().to_string(),
        // A remux, run verbatim, over the agent-local paths the server chose.
        argv: vec![
            "-v".into(),
            "error".into(),
            "-y".into(),
            "-i".into(),
            agent_source.display().to_string(),
            "-map".into(),
            "0".into(),
            "-c".into(),
            "copy".into(),
            agent_temp.display().to_string(),
        ],
        validation_spec_json: serde_json::json!({
            "source_duration_us": 2_000_000u64,
            "max_shorter_us": 100_000u64,
            "max_longer_us": 2_000_000u64,
            "expected_audio_streams": 1,
            "expected_subtitle_streams": 0,
            "source_bytes": source_bytes.len(),
            "size_policy": "MayGrow",
        })
        .to_string(),
        expected_content_sig: "sig".into(),
    });

    let mut config = ClientConfig::new(&endpoint, "u1");
    config.heartbeat = Duration::from_millis(500);
    config.reconnect = ReconnectPolicy {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(50),
    };
    let client = ConnectClient::new(config, worker);
    let shutdown = client.shutdown();
    let run = tokio::spawn(async move { client.run().await });

    // Bounded: a hang here is a real failure, and an unbounded wait reports it
    // as a timeout with nothing to read.
    let mut pushed = None;
    for _ in 0..600 {
        if let Some(bytes) = fake.state().pushed.clone() {
            pushed = Some(bytes);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pushed = pushed.unwrap_or_else(|| {
        panic!(
            "nothing was ever pushed; push refusal was {:?}",
            fake.state().push_error
        )
    });

    // What landed on the server is a real, complete media file — not merely
    // some bytes of the right length. A truncated transfer is always *smaller*,
    // so the probe is the check that means something.
    assert_eq!(
        std::fs::read(&staged).unwrap(),
        pushed,
        "the staged file and the verified bytes must be the same thing"
    );
    let landed = duration_us(&staged);
    assert!(
        landed > 1_900_000 && landed < 2_100_000,
        "the installed output should be the ~2s remux, got {landed}us"
    );

    // The server owns the install under streaming, so the agent must not have
    // asked permission or reported a resolution. Either would be a second
    // verdict on one outcome.
    let inbound = fake.state().inbound.clone();
    let results: Vec<_> = inbound
        .iter()
        .filter_map(|m| match &m.body {
            Some(pb::agent_message::Body::Result(r)) => Some(r.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "exactly one JobResult");
    assert_eq!(results[0].exit_code, 0, "{}", results[0].stderr_tail);

    assert!(
        !inbound.iter().any(|m| matches!(
            m.body,
            Some(pb::agent_message::Body::CommitRequest(_))
                | Some(pb::agent_message::Body::CommitReport(_))
        )),
        "a streaming agent installs nothing, so it must neither ask permission \
         nor report a resolution; the push is the whole exchange"
    );

    // Both ends swept. A fetched source is a whole copy of the original, and
    // one left behind per job fills the work area on a machine that looks idle.
    assert!(
        !agent_source.exists(),
        "the fetched source was not cleaned up"
    );
    assert!(!agent_temp.exists(), "the encode output was not cleaned up");

    // Never touched, because it was never reachable.
    assert!(!std::path::Path::new(final_path).exists());

    shutdown.stop();
    let _ = run.await;
}
