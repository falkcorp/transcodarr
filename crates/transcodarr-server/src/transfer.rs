// file: crates/transcodarr-server/src/transfer.rs
// version: 1.0.0
// guid: 9c1f6b28-4a70-4de3-8f52-6b0d7ae94c11
// last-edited: 2026-08-12
//! Moving file bytes for [`TransportMode::Stream`].
//!
//! A streaming agent never resolves a canonical path. It is handed the source
//! bytes, works on a local copy, and sends the result back for the server to
//! install. This module is only the byte plumbing; the install itself is the
//! ordinary commit ritual, performed server-side because a streaming agent has
//! no path to the destination and could not perform it if it wanted to.
//!
//! ## Why a chunk carries its offset and a signature
//!
//! `offset` is carried rather than implied so a short read is detectable. A
//! receiver that has written 900 MB and is then handed a chunk claiming offset
//! zero knows the stream restarted, instead of appending and producing a
//! corrupt file that happens to be a plausible length.
//!
//! The last chunk carries a blake3 of the whole file, and [`Sink`] refuses the
//! transfer when it does not match. This is the one gate that matters: a
//! truncated transfer is *smaller*, and size is never an accept criterion in
//! this project. Without the hash a half-received encode would install
//! cleanly and silently replace a good original with a broken file.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use transcodarr_proto::pb;

/// Bytes per chunk.
///
/// Well under tonic's 4 MiB default message ceiling, with room for the frame
/// and the other fields: a chunk that trips `max_decoding_message_size` fails
/// the whole transfer, and picking a size near the limit makes that a function
/// of how large the other fields happen to be.
pub const CHUNK_BYTES: usize = 512 * 1024;

/// How many chunks may be buffered ahead of a slow consumer.
const QUEUE_DEPTH: usize = 4;

/// Read `path` and produce its chunks in order.
///
/// The read happens on the blocking pool: these are multi-gigabyte files, and
/// doing that on a runtime worker would stall every other agent's stream.
pub fn source_stream(
    job_id: String,
    attempt: u32,
    path: PathBuf,
) -> ReceiverStream<Result<pb::FileChunk, Status>> {
    let (tx, rx) = mpsc::channel(QUEUE_DEPTH);

    tokio::task::spawn_blocking(move || {
        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                // Blocking send: the receiver is a live gRPC stream, and
                // dropping the error would hand the agent an empty file.
                let _ = tx.blocking_send(Err(Status::not_found(format!(
                    "source {} cannot be opened: {e}",
                    path.display()
                ))));
                return;
            }
        };

        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; CHUNK_BYTES];
        let mut offset: u64 = 0;

        loop {
            let read = match file.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.blocking_send(Err(Status::internal(format!(
                        "reading {} at offset {offset}: {e}",
                        path.display()
                    ))));
                    return;
                }
            };

            if read == 0 {
                // The terminator is its own chunk carrying the signature. An
                // explicit end beats "the stream closed", which is
                // indistinguishable from the sender dying mid-file.
                let _ = tx.blocking_send(Ok(pb::FileChunk {
                    job_id,
                    attempt,
                    offset,
                    data: Vec::new(),
                    last: true,
                    content_sig: hasher.finalize().to_hex().to_string(),
                }));
                return;
            }

            hasher.update(&buf[..read]);
            let chunk = pb::FileChunk {
                job_id: job_id.clone(),
                attempt,
                offset,
                data: buf[..read].to_vec(),
                last: false,
                content_sig: String::new(),
            };
            offset += read as u64;

            if tx.blocking_send(Ok(chunk)).is_err() {
                // The agent hung up. Nothing to report to: stop reading rather
                // than burn I/O on a transfer nobody will receive.
                return;
            }
        }
    });

    ReceiverStream::new(rx)
}

/// Receives chunks into a file, refusing anything it cannot vouch for.
pub struct Sink {
    path: PathBuf,
    file: std::fs::File,
    hasher: blake3::Hasher,
    written: u64,
    finished: bool,
}

impl Sink {
    /// Open `path` for a new transfer, truncating any previous attempt.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            file: std::fs::File::create(path)?,
            hasher: blake3::Hasher::new(),
            written: 0,
            finished: false,
        })
    }

    /// Bytes accepted so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Where the bytes are landing.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept one chunk. Returns `true` once the transfer is complete.
    ///
    /// Every rejection here leaves a partial file on disk on purpose — the
    /// caller removes it. Returning an error while pretending the file is gone
    /// would be a worse lie than the partial file is a mess.
    pub fn accept(&mut self, chunk: &pb::FileChunk) -> Result<bool, String> {
        if self.finished {
            return Err("a chunk arrived after the transfer was declared complete".into());
        }
        // The gap check. Out-of-order or restarted streams are the failure this
        // exists to catch, and appending regardless is how you get a file of
        // the right length and the wrong contents.
        if chunk.offset != self.written {
            return Err(format!(
                "chunk claims offset {} but {} bytes have been written; the stream restarted or lost a chunk",
                chunk.offset, self.written
            ));
        }

        if !chunk.data.is_empty() {
            self.file
                .write_all(&chunk.data)
                .map_err(|e| format!("writing at offset {}: {e}", self.written))?;
            self.hasher.update(&chunk.data);
            self.written += chunk.data.len() as u64;
        }

        if !chunk.last {
            return Ok(false);
        }

        self.file
            .flush()
            .map_err(|e| format!("flushing {}: {e}", self.path.display()))?;
        self.file
            .sync_all()
            .map_err(|e| format!("syncing {}: {e}", self.path.display()))?;

        let actual = self.hasher.finalize().to_hex().to_string();
        if actual != chunk.content_sig {
            return Err(format!(
                "content signature mismatch after {} bytes: sender said {}, received bytes hash to {}",
                self.written, chunk.content_sig, actual
            ));
        }

        self.finished = true;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(offset: u64, data: &[u8], last: bool, sig: &str) -> pb::FileChunk {
        pb::FileChunk {
            job_id: "job-1".into(),
            attempt: 0,
            offset,
            data: data.to_vec(),
            last,
            content_sig: sig.into(),
        }
    }

    fn sig_of(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    #[test]
    fn a_whole_transfer_lands_and_verifies() {
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("out.mkv");
        let mut sink = Sink::create(&dest).unwrap();

        assert!(!sink.accept(&chunk(0, b"hello ", false, "")).unwrap());
        assert!(!sink.accept(&chunk(6, b"world", false, "")).unwrap());
        assert!(
            sink.accept(&chunk(11, b"", true, &sig_of(b"hello world")))
                .unwrap()
        );

        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
        assert_eq!(sink.written(), 11);
    }

    /// The failure the signature exists for. A truncated transfer produces a
    /// *smaller* file, so length proves nothing — this is the same rule as the
    /// encode side, where a truncated output is never accepted on size.
    #[test]
    fn a_truncated_transfer_is_refused_rather_than_installed() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut sink = Sink::create(&dir.path().join("out.mkv")).unwrap();

        sink.accept(&chunk(0, b"hello ", false, "")).unwrap();
        // The sender died and something closed the transfer early: the bytes
        // present are a clean prefix, and only the hash reveals it.
        let err = sink
            .accept(&chunk(6, b"", true, &sig_of(b"hello world")))
            .unwrap_err();

        assert!(err.contains("content signature mismatch"), "{err}");
    }

    /// A restarted stream must not be appended to the bytes already written.
    #[test]
    fn a_chunk_at_the_wrong_offset_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut sink = Sink::create(&dir.path().join("out.mkv")).unwrap();

        sink.accept(&chunk(0, b"hello ", false, "")).unwrap();
        let err = sink.accept(&chunk(0, b"hello ", false, "")).unwrap_err();

        assert!(err.contains("claims offset 0"), "{err}");
    }

    #[tokio::test]
    async fn a_file_round_trips_through_the_stream_and_the_sink() {
        use tokio_stream::StreamExt;

        let dir = tempfile::TempDir::new().unwrap();
        // Deliberately larger than one chunk, and not a multiple of it: an
        // off-by-one in the final partial read is the bug this shape catches.
        let body: Vec<u8> = (0..(CHUNK_BYTES * 2 + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        let src = dir.path().join("in.mkv");
        std::fs::write(&src, &body).unwrap();

        let mut stream = source_stream("job-1".into(), 0, src);
        let dest = dir.path().join("out.mkv");
        let mut sink = Sink::create(&dest).unwrap();

        let mut done = false;
        let mut chunks = 0;
        while let Some(next) = stream.next().await {
            chunks += 1;
            done = sink.accept(&next.unwrap()).unwrap();
        }

        assert!(
            done,
            "the stream must terminate with an explicit last chunk"
        );
        assert!(chunks >= 4, "expected several chunks, got {chunks}");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[tokio::test]
    async fn a_missing_source_reports_rather_than_streaming_nothing() {
        use tokio_stream::StreamExt;

        let dir = tempfile::TempDir::new().unwrap();
        let mut stream = source_stream("job-1".into(), 0, dir.path().join("absent.mkv"));

        let first = stream.next().await.expect("an error, not an empty stream");
        let status = first.expect_err("a missing source must not look like an empty file");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }
}
