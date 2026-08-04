// file: crates/transcodarr-server/src/prober.rs
// version: 1.0.0
// guid: 9c58a3f1-e072-4d46-8b19-25f7ac0e6b34
// last-edited: 2026-08-03
//! Probe ingestion: run `ffprobe` over discovered files and store the facts.
//!
//! This is the only part of Phase 2 that opens a media file, and it does so
//! exactly once per file per change. Everything downstream — evaluation,
//! re-evaluation after a policy edit, `admin explain` — runs off the stored
//! result, which is what keeps a policy change a database operation rather than
//! an 85 TB read.
//!
//! Concurrency is bounded on purpose and separately from transcode
//! concurrency. Probing is seek-heavy and the pool is latency-bound: pointing
//! forty-eight threads at it produces forty-eight slow probes rather than
//! forty-eight fast ones, and starves any transcode running alongside.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use transcodarr_core::facts::{self, SizeThresholds};
use transcodarr_core::probe;
use transcodarr_store::repo::{FileRecord, FileRepo};
use transcodarr_store::{ReadPool, WriteLane, Writer};

use crate::ServerError;

/// How many probes run at once by default.
///
/// Four, not one per core. `ffprobe` on a large remux is dominated by seeks
/// against a latency-bound pool, so the useful parallelism is small and the
/// cost of overshooting is paid by every transcode running alongside.
pub const DEFAULT_PROBE_CONCURRENCY: usize = 4;

/// How long a single probe may take before it is abandoned.
///
/// A file on a stalled mount otherwise hangs a worker forever, and four such
/// files stop ingestion entirely with no error to look at.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// How many files are claimed from the queue per pass.
const PROBE_BATCH: u32 = 256;

/// Knobs for probe ingestion.
#[derive(Debug, Clone)]
pub struct ProbeOptions {
    /// Concurrent `ffprobe` invocations.
    pub concurrency: usize,
    /// Per-probe wall-clock limit.
    pub timeout: Duration,
    /// Size-bucket boundaries.
    pub thresholds: SizeThresholds,
    /// The `ffprobe` binary to run.
    pub ffprobe: String,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_PROBE_CONCURRENCY,
            timeout: PROBE_TIMEOUT,
            thresholds: SizeThresholds::default(),
            ffprobe: "ffprobe".to_string(),
        }
    }
}

/// What one ingestion pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// Files probed and stored.
    pub probed: i64,
    /// Files whose probe failed.
    pub failed: i64,
}

/// Runs `ffprobe` over discovered files and stores what it finds.
pub struct Prober {
    files: FileRepo,
    writer: Arc<Writer>,
    options: ProbeOptions,
}

impl Prober {
    /// Build a prober over a read pool and the single writer.
    pub fn new(pool: ReadPool, writer: Arc<Writer>, options: ProbeOptions) -> Self {
        Self {
            files: FileRepo::new(pool),
            writer,
            options,
        }
    }

    /// Probe every `Discovered` file in a library.
    ///
    /// Loops until the queue drains. A pass that probes nothing but still finds
    /// work stops rather than spinning: every outcome, success or failure,
    /// moves a file out of `Discovered`, so no progress means something is
    /// wrong with the store rather than with the files.
    pub fn probe_library(&self, library_id: &str) -> Result<ProbeOutcome, ServerError> {
        let mut total = ProbeOutcome::default();
        loop {
            let batch = self.files.needs_probe(library_id, PROBE_BATCH)?;
            if batch.is_empty() {
                return Ok(total);
            }
            let before = total.probed + total.failed;
            self.probe_batch(&batch, &mut total)?;
            if total.probed + total.failed == before {
                tracing::warn!(
                    library = %library_id,
                    stuck = batch.len(),
                    "probe ingestion made no progress; stopping rather than looping"
                );
                return Ok(total);
            }
        }
    }

    /// Probe one batch across `concurrency` worker threads.
    fn probe_batch(
        &self,
        batch: &[FileRecord],
        total: &mut ProbeOutcome,
    ) -> Result<(), ServerError> {
        let next = AtomicUsize::new(0);
        let probed = AtomicUsize::new(0);
        let failed = AtomicUsize::new(0);

        // Scoped threads so the batch can be borrowed rather than cloned into
        // each worker, and so every worker is joined before the counters are
        // read -- a detached worker still writing after the pass "finished"
        // would make the reported counts a lie.
        std::thread::scope(|scope| {
            for _ in 0..self.options.concurrency.max(1) {
                scope.spawn(|| {
                    loop {
                        let idx = next.fetch_add(1, Ordering::Relaxed);
                        let Some(file) = batch.get(idx) else { return };
                        match self.probe_one(file) {
                            Ok(()) => {
                                probed.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
        });

        total.probed += probed.load(Ordering::Relaxed) as i64;
        total.failed += failed.load(Ordering::Relaxed) as i64;
        Ok(())
    }

    fn probe_one(&self, file: &FileRecord) -> Result<(), ServerError> {
        let path = Path::new(&file.canonical_path);
        match self.run_ffprobe(path) {
            Ok(raw) => match probe::parse_ffprobe_json(&raw) {
                Ok(parsed) => {
                    let derived = facts::derive_facts(&parsed, file.size_bytes.max(0) as u64);
                    let sig = facts::content_sig(&derived).0;
                    let bucket = facts::size_bucket_for(
                        file.size_bytes.max(0) as u64,
                        &self.options.thresholds,
                    );
                    self.writer.submit_blocking(
                        WriteLane::Normal,
                        FileRepo::record_probe_op(
                            file.id,
                            derived,
                            sig,
                            bucket,
                            raw,
                            self.options.ffprobe.clone(),
                        ),
                    )?;
                    Ok(())
                }
                // Unparseable output is a probe failure, not a parse bug to
                // panic on: a truncated or non-media file legitimately produces
                // one, and the file must be marked rather than retried forever.
                Err(e) => self.record_failure(file, format!("unparseable ffprobe output: {e}")),
            },
            Err(e) => self.record_failure(file, e),
        }
    }

    fn record_failure(&self, file: &FileRecord, reason: String) -> Result<(), ServerError> {
        tracing::warn!(path = %file.canonical_path, reason = %reason, "probe failed");
        self.writer.submit_blocking(
            WriteLane::Normal,
            FileRepo::record_probe_failure_op(file.id, truncate(&reason, 500)),
        )?;
        // The file is marked, so the pass made progress. Reporting this as an
        // error would abandon the rest of the batch over one bad file.
        Err(ServerError::ProbeFailed {
            path: file.canonical_path.clone(),
            reason,
        })
    }

    /// Run `ffprobe` and return its JSON, or a reason it did not.
    ///
    /// Arguments are passed as argv, never through a shell: a filename holding
    /// a quote or a semicolon is ordinary in a media library and must not be
    /// able to become a command.
    fn run_ffprobe(&self, path: &Path) -> Result<String, String> {
        let mut child = Command::new(&self.options.ffprobe)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", self.options.ffprobe))?;

        let deadline = std::time::Instant::now() + self.options.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    // A stalled mount otherwise holds a worker forever, and
                    // four such files stop ingestion with nothing to look at.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "ffprobe exceeded {}s",
                        self.options.timeout.as_secs()
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(e) => return Err(format!("waiting on ffprobe: {e}")),
            }
        }

        let out = child
            .wait_with_output()
            .map_err(|e| format!("collecting ffprobe output: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "ffprobe exited {}: {}",
                out.status.code().unwrap_or(-1),
                truncate(&String::from_utf8_lossy(&out.stderr), 300)
            ));
        }
        String::from_utf8(out.stdout).map_err(|e| format!("ffprobe output was not UTF-8: {e}"))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Truncate on a character boundary; a media path is frequently non-ASCII
    // and slicing mid-codepoint would panic on exactly those files.
    let end = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|i| *i <= max)
        .last()
        .unwrap_or(0);
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use transcodarr_core::file::FileState;
    use transcodarr_store::repo::{FileUpsert, LibraryRecord, LibraryRepo};
    use transcodarr_store::{Db, ReadPool, Writer};

    struct Harness {
        _dir: TempDir,
        bin_dir: TempDir,
        pool: ReadPool,
        writer: Arc<Writer>,
    }

    /// A stand-in for `ffprobe`. Real ffmpeg is not assumed present in CI, and
    /// what is under test is the ingestion loop -- claiming, concurrency,
    /// failure marking, timeout -- not ffmpeg's own parsing.
    fn fake_ffprobe(dir: &Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p.to_string_lossy().to_string()
    }

    const GOOD_JSON: &str = r#"{
      "format": {"format_name": "matroska,webm", "duration": "1500.0", "bit_rate": "8000000"},
      "streams": [
        {"index":0,"codec_type":"video","codec_name":"hevc","profile":"Main 10",
         "pix_fmt":"yuv420p10le","width":1920,"height":1080},
        {"index":1,"codec_type":"audio","codec_name":"truehd","channels":8},
        {"index":2,"codec_type":"subtitle","codec_name":"subrip"}
      ]
    }"#;

    fn harness() -> Harness {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open_unchecked(&path).unwrap();
        let pool = ReadPool::open(&path, 4).unwrap();
        let h = Harness {
            _dir: dir,
            bin_dir: TempDir::new().unwrap(),
            pool,
            writer: Arc::new(Writer::start(db)),
        };
        h.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(LibraryRecord {
                    id: "tv".into(),
                    name: "tv".into(),
                    root_path: "/mnt/tv".into(),
                    work_dir: "/w".into(),
                    trash_dir: "/t".into(),
                    exclude_globs_json: "[]".into(),
                    enabled: true,
                    scan_parallelism: 4,
                    priority: 0,
                    min_mtime_age_s: 300,
                }),
            )
            .unwrap();
        h
    }

    impl Harness {
        fn add_file(&self, name: &str) -> i64 {
            self.writer
                .submit_blocking(
                    WriteLane::Normal,
                    FileRepo::upsert_op(FileUpsert {
                        library_id: "tv".into(),
                        canonical_path: format!("/mnt/tv/{name}"),
                        path_hash: transcodarr_core::stable_hash(name.as_bytes()),
                        size_bytes: 1_000_000_000,
                        mtime_unix: 1000,
                        mtime_ns: 0,
                        inode: Some(1),
                        dev: Some(1),
                        nlink: 1,
                        scan_generation: 1,
                    }),
                )
                .unwrap()
                .last_id
                .unwrap()
        }

        fn prober(&self, script: &str) -> Prober {
            let ffprobe = fake_ffprobe(self.bin_dir.path(), "ffprobe-stub", script);
            Prober::new(
                self.pool.clone(),
                Arc::clone(&self.writer),
                ProbeOptions {
                    concurrency: 3,
                    timeout: Duration::from_secs(5),
                    thresholds: SizeThresholds::default(),
                    ffprobe,
                },
            )
        }

        fn files(&self) -> FileRepo {
            FileRepo::new(self.pool.clone())
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_successful_probe_stores_facts_and_moves_the_file_on() {
        let h = harness();
        let id = h.add_file("a.mkv");
        let out = h
            .prober(&format!("cat <<'EOF'\n{GOOD_JSON}\nEOF"))
            .probe_library("tv")
            .unwrap();

        assert_eq!(out.probed, 1);
        assert_eq!(out.failed, 0);

        let rec = h.files().get(id).unwrap();
        assert_eq!(rec.state, FileState::Probed);
        let facts = rec.facts.expect("facts must be stored");
        assert_eq!(facts.video_codec.as_deref(), Some("hevc"));
        assert_eq!(facts.audio_codecs, vec!["truehd"]);
        assert_eq!(facts.subtitle_track_count, 1);
        assert!(rec.content_sig.is_some(), "a signature must be recorded");
    }

    /// Size comes from the row, not from ffprobe's `format.size`. Discovery is
    /// the single authority for it, and a probe reporting otherwise would make
    /// the size bucket disagree with the file it describes.
    #[cfg(unix)]
    #[test]
    fn stored_size_comes_from_discovery_not_from_the_probe() {
        let h = harness();
        let id = h.add_file("a.mkv");
        h.prober(&format!("cat <<'EOF'\n{GOOD_JSON}\nEOF"))
            .probe_library("tv")
            .unwrap();
        let rec = h.files().get(id).unwrap();
        assert_eq!(rec.size_bytes, 1_000_000_000);
        assert_eq!(rec.facts.unwrap().size_bytes, 1_000_000_000);
    }

    /// A probe failure must mark the file. Leaving it `Discovered` would mean
    /// re-probing the same unreadable file on every pass, forever.
    #[cfg(unix)]
    #[test]
    fn a_failing_probe_marks_the_file_rather_than_retrying_forever() {
        let h = harness();
        let id = h.add_file("broken.mkv");
        let out = h
            .prober("echo 'moov atom not found' >&2; exit 1")
            .probe_library("tv")
            .unwrap();

        assert_eq!(out.failed, 1);
        assert_eq!(out.probed, 0);
        let rec = h.files().get(id).unwrap();
        assert_eq!(rec.state, FileState::ProbeFailed);
        assert!(rec.decision_reason.unwrap().contains("ffprobe exited 1"));
        assert!(
            h.files().needs_probe("tv", 100).unwrap().is_empty(),
            "a failed file must leave the probe queue"
        );
    }

    /// Unparseable output is a probe failure, not a panic. A non-media file
    /// with a media extension legitimately produces one.
    #[cfg(unix)]
    #[test]
    fn unparseable_output_is_a_failure_not_a_crash() {
        let h = harness();
        let id = h.add_file("notmedia.mkv");
        let out = h
            .prober("echo 'this is not json'")
            .probe_library("tv")
            .unwrap();
        assert_eq!(out.failed, 1);
        assert_eq!(h.files().get(id).unwrap().state, FileState::ProbeFailed);
    }

    /// One bad file must not abandon the batch. Returning early on the first
    /// failure would leave the rest of the library unprobed.
    #[cfg(unix)]
    #[test]
    fn one_bad_file_does_not_stop_the_others() {
        let h = harness();
        for i in 0..6 {
            h.add_file(&format!("f{i}.mkv"));
        }
        // Fail only on f3.
        let script =
            format!("case \"$*\" in *f3.mkv*) exit 1 ;; esac\ncat <<'EOF'\n{GOOD_JSON}\nEOF");
        let out = h.prober(&script).probe_library("tv").unwrap();
        assert_eq!(out.probed, 5);
        assert_eq!(out.failed, 1);
        assert!(h.files().needs_probe("tv", 100).unwrap().is_empty());
    }

    /// A stalled mount otherwise holds a worker forever, and enough of them
    /// stop ingestion with no error to look at.
    #[cfg(unix)]
    #[test]
    fn a_hanging_probe_is_abandoned_rather_than_held_forever() {
        let h = harness();
        let id = h.add_file("stalled.mkv");
        let ffprobe = fake_ffprobe(h.bin_dir.path(), "hang", "sleep 30");
        let prober = Prober::new(
            h.pool.clone(),
            Arc::clone(&h.writer),
            ProbeOptions {
                concurrency: 1,
                timeout: Duration::from_millis(300),
                thresholds: SizeThresholds::default(),
                ffprobe,
            },
        );

        let started = std::time::Instant::now();
        let out = prober.probe_library("tv").unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "must not hang");
        assert_eq!(out.failed, 1);
        assert!(
            h.files()
                .get(id)
                .unwrap()
                .decision_reason
                .unwrap()
                .contains("exceeded"),
            "the timeout must be recorded as the reason"
        );
    }

    /// Every file, once. A second pass finds nothing because success and
    /// failure both move a file out of `Discovered`.
    #[cfg(unix)]
    #[test]
    fn a_second_pass_finds_nothing_left_to_probe() {
        let h = harness();
        for i in 0..4 {
            h.add_file(&format!("f{i}.mkv"));
        }
        let p = h.prober(&format!("cat <<'EOF'\n{GOOD_JSON}\nEOF"));
        assert_eq!(p.probe_library("tv").unwrap().probed, 4);
        assert_eq!(p.probe_library("tv").unwrap(), ProbeOutcome::default());
    }

    /// A filename holding shell metacharacters is ordinary in a media library.
    /// Passed as argv it is inert; interpolated into a shell it is a command.
    #[cfg(unix)]
    #[test]
    fn a_filename_with_shell_metacharacters_is_not_interpreted() {
        let h = harness();
        let id = h.add_file("Trick; rm -rf $HOME 'quoted'.mkv");
        let out = h
            .prober(&format!("cat <<'EOF'\n{GOOD_JSON}\nEOF"))
            .probe_library("tv")
            .unwrap();
        assert_eq!(out.probed, 1, "the name must be passed through untouched");
        assert_eq!(h.files().get(id).unwrap().state, FileState::Probed);
    }

    /// A missing binary is an operator problem, and every file failing with the
    /// same reason is what says so.
    #[cfg(unix)]
    #[test]
    fn a_missing_ffprobe_binary_fails_every_file_with_a_clear_reason() {
        let h = harness();
        let id = h.add_file("a.mkv");
        let prober = Prober::new(
            h.pool.clone(),
            Arc::clone(&h.writer),
            ProbeOptions {
                ffprobe: "/nonexistent/ffprobe".into(),
                ..ProbeOptions::default()
            },
        );
        let out = prober.probe_library("tv").unwrap();
        assert_eq!(out.failed, 1);
        assert!(
            h.files()
                .get(id)
                .unwrap()
                .decision_reason
                .unwrap()
                .contains("could not run"),
            "the reason must name the binary"
        );
    }
}
