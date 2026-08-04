// file: crates/transcodarr-server/src/scanner.rs
// version: 1.0.0
// guid: 6a92c7e0-4b18-4d53-9f26-0a71e58cb3d4
// last-edited: 2026-08-03
//! Discovery: walk a library, record what is there, notice what is gone.
//!
//! Discovery only. The scanner reads directory entries and `stat` results and
//! nothing else — it never opens a media file, never probes, and never decides
//! anything. That separation is what makes a rescan cheap: probe facts are
//! written once and survive every subsequent scan that finds the file
//! unchanged.
//!
//! Two guards shape the design, and both exist because of a specific way the
//! naive version destroys data:
//!
//! - **The mass-missing guard.** An unmounted library is indistinguishable from
//!   every file having been deleted. The scan therefore counts what it *would*
//!   mark missing and refuses to write if the proportion is implausible.
//! - **`min_mtime_age_s`.** A file still being written has a recent mtime.
//!   Recording it races the writer, and enqueueing it means transcoding a
//!   partial file.

use std::path::Path;
use std::time::Instant;

use transcodarr_store::repo::{FileRepo, FileUpsert, LibraryRecord, LibraryRepo};
use transcodarr_store::{ReadPool, WriteLane, Writer};
use walkdir::WalkDir;

use crate::ServerError;

/// Directory names never descended into.
///
/// `.zfs` is the snapshot directory: descending it walks every snapshot of the
/// pool and finds the same media dozens of times over. `work` and `trash` are
/// transcodarr's own staging and retention areas — discovering our own
/// in-progress output as source material would queue it for transcoding.
/// `@eaDir` is Synology's thumbnail store and `lost+found` is fsck's.
pub const DEFAULT_EXCLUDED_DIRS: &[&str] = &[".zfs", "work", "trash", "@eaDir", "lost+found"];

/// Extensions treated as media.
pub const DEFAULT_MEDIA_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "ts", "m2ts", "mts", "wmv", "flv", "webm", "mpg", "mpeg",
    "ogv", "divx",
];

/// How many upserts are in flight before the scanner waits for them.
///
/// The writer coalesces whatever is queued into one transaction, so submitting
/// in batches rather than one at a time is the difference between ~49,600
/// transactions and a few hundred. Bounded rather than unbounded so a scan
/// cannot queue the entire library ahead of a commit-lane write.
const UPSERT_BATCH: usize = 500;

/// Knobs for a scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Lowercase extensions treated as media.
    pub media_extensions: Vec<String>,
    /// Directory names never descended into.
    pub excluded_dirs: Vec<String>,
    /// How deep to walk below the root.
    pub max_depth: usize,
    /// Proportion of a library that may go missing in one scan before the scan
    /// refuses to record it.
    pub mass_missing_percent: f64,
    /// Below this many missing files the proportion is not consulted.
    ///
    /// Without a floor, a library of eight files trips the percentage guard on
    /// a single legitimate deletion.
    pub mass_missing_floor: i64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            media_extensions: DEFAULT_MEDIA_EXTENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            excluded_dirs: DEFAULT_EXCLUDED_DIRS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            max_depth: 32,
            // Ten percent. A library does not lose a tenth of itself in one
            // scan for any benign reason; a half-mounted export loses all of it.
            mass_missing_percent: 10.0,
            mass_missing_floor: 25,
        }
    }
}

/// What one pass of discovery found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    /// The `scan_run` row.
    pub run_id: i64,
    /// The generation allocated to this pass.
    pub generation: i64,
    /// Media files walked.
    pub files_seen: i64,
    /// Files not previously known, including ones returning from `Missing`.
    pub files_new: i64,
    /// Files whose size or mtime moved.
    pub files_changed: i64,
    /// Files marked missing by this pass.
    pub files_missing: i64,
    /// Files skipped as too recently modified to be complete.
    pub files_too_recent: i64,
    /// How long the pass took.
    pub duration_ms: i64,
}

/// Per-library discovery and change detection.
pub struct Scanner {
    files: FileRepo,
    libraries: LibraryRepo,
    writer: std::sync::Arc<Writer>,
    options: ScanOptions,
}

impl Scanner {
    /// Build a scanner over a read pool and the single writer.
    pub fn new(pool: ReadPool, writer: std::sync::Arc<Writer>, options: ScanOptions) -> Self {
        Self {
            files: FileRepo::new(pool.clone()),
            libraries: LibraryRepo::new(pool),
            writer,
            options,
        }
    }

    /// Walk one library and record what changed.
    ///
    /// Returns [`ServerError::MassMissing`] without writing anything if the
    /// walk found implausibly little. The `scan_run` row is still closed, as
    /// `aborted`, with the reason recorded — a scan that stopped early and
    /// looks like a clean one is how a mount failure becomes invisible.
    pub fn scan_library(
        &self,
        library: &LibraryRecord,
        mode: &str,
    ) -> Result<ScanOutcome, ServerError> {
        let started = Instant::now();
        let root = Path::new(&library.root_path);
        if !root.is_dir() {
            return Err(ServerError::LibraryRootUnreadable {
                library_id: library.id.clone(),
                root: library.root_path.clone(),
            });
        }

        // Counted before the walk: it is the denominator the guard compares
        // against, and it must describe the library as it was, not as the walk
        // is leaving it.
        let live_before = self.files.count_live(&library.id)?;

        let run_id = self
            .writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::begin_scan_run_op(library.id.clone(), mode.to_string()),
            )?
            .last_id
            .expect("begin_scan_run reports its row id");
        let generation = self
            .libraries
            .last_scan_run(&library.id)?
            .filter(|r| r.id == run_id)
            .map(|r| r.scan_generation)
            .ok_or_else(|| {
                ServerError::Io(std::io::Error::other(
                    "the scan run just opened could not be read back",
                ))
            })?;

        let walked = self.walk(library, generation);
        let counts = match walked {
            Ok(c) => c,
            Err(e) => {
                self.close_run(
                    run_id,
                    "aborted",
                    Some(format!("walk failed: {e}")),
                    started,
                );
                return Err(e);
            }
        };

        // Counted before anything is marked, so the guard decides on the same
        // information an operator would.
        let would_be_missing = self.files.count_not_seen_in(&library.id, generation)?;
        if let Some(err) = self.mass_missing_check(library, would_be_missing, live_before) {
            let reason = err.to_string();
            self.close_run(run_id, "aborted", Some(reason), started);
            return Err(err);
        }

        if would_be_missing > 0 {
            self.writer.submit_blocking(
                WriteLane::Normal,
                FileRepo::mark_missing_op(library.id.clone(), generation),
            )?;
        }

        let duration_ms = started.elapsed().as_millis() as i64;
        self.writer.submit_blocking(
            WriteLane::Normal,
            LibraryRepo::update_scan_counts_op(
                run_id,
                counts.seen,
                counts.new,
                counts.changed,
                would_be_missing,
                0,
            ),
        )?;
        self.close_run(run_id, "ok", None, started);

        Ok(ScanOutcome {
            run_id,
            generation,
            files_seen: counts.seen,
            files_new: counts.new,
            files_changed: counts.changed,
            files_missing: would_be_missing,
            files_too_recent: counts.too_recent,
            duration_ms,
        })
    }

    /// Refuse the sweep when too much of the library vanished at once.
    fn mass_missing_check(
        &self,
        library: &LibraryRecord,
        would_be_missing: i64,
        live_before: i64,
    ) -> Option<ServerError> {
        if live_before <= 0 || would_be_missing <= self.options.mass_missing_floor {
            return None;
        }
        let percent = (would_be_missing as f64 / live_before as f64) * 100.0;
        if percent <= self.options.mass_missing_percent {
            return None;
        }
        Some(ServerError::MassMissing {
            library_id: library.id.clone(),
            would_be_missing,
            live_before,
            percent,
            limit_percent: self.options.mass_missing_percent,
        })
    }

    fn close_run(&self, run_id: i64, status: &str, reason: Option<String>, started: Instant) {
        // Best effort by intent: the scan's own result is what the caller acts
        // on, and failing to close the bookkeeping row must not turn a
        // successful scan into a failed one.
        let _ = self.writer.submit_blocking(
            WriteLane::Normal,
            LibraryRepo::finish_scan_run_op(
                run_id,
                status.to_string(),
                reason,
                started.elapsed().as_millis() as i64,
            ),
        );
    }

    fn walk(&self, library: &LibraryRecord, generation: i64) -> Result<WalkCounts, ServerError> {
        let mut counts = WalkCounts::default();
        let mut pending = Vec::with_capacity(UPSERT_BATCH);
        let now = now_unix();

        let walker = WalkDir::new(&library.root_path)
            .max_depth(self.options.max_depth)
            // Symlinks are not followed. Following them turns a loop into an
            // infinite walk and makes one file discoverable under two paths,
            // which would queue the same bytes for transcoding twice.
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                !self.is_excluded_dir(e)
                    && !(e.file_type().is_dir() && self.is_own_area(library, e.path()))
            });

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                // A directory that vanished mid-walk, or one we cannot read, is
                // not a reason to abandon the library. It is counted as unseen
                // and the mass-missing guard decides whether that matters.
                Err(e) => {
                    tracing::warn!(library = %library.id, error = %e, "skipping unreadable entry");
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if !self.is_media(entry.path()) {
                continue;
            }

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(path = %entry.path().display(), error = %e, "skipping unstatable file");
                    continue;
                }
            };
            let mtime_unix = mtime_secs(&meta);

            // A recent mtime means something may still be writing. Recording it
            // races the writer; enqueueing it transcodes a partial file.
            if now.saturating_sub(mtime_unix) < library.min_mtime_age_s {
                counts.too_recent += 1;
                continue;
            }

            let canonical_path = entry.path().to_string_lossy().to_string();
            let path_hash = transcodarr_core::stable_hash(canonical_path.as_bytes());
            let size_bytes = meta.len() as i64;
            let mtime_ns = mtime_subsec_nanos(&meta);

            match self.files.identity(&library.id, &path_hash)? {
                None => counts.new += 1,
                // A file returning from `Missing` is new work again: its row
                // survived, but nothing has looked at it since it vanished.
                Some(prev) if prev.state == "Missing" => counts.new += 1,
                Some(prev)
                    if prev.size_bytes != size_bytes
                        || prev.mtime_unix != mtime_unix
                        || prev.mtime_ns != mtime_ns =>
                {
                    counts.changed += 1
                }
                Some(_) => {}
            }
            counts.seen += 1;

            pending.push(self.writer.submit(
                WriteLane::Normal,
                FileRepo::upsert_op(FileUpsert {
                    library_id: library.id.clone(),
                    canonical_path,
                    path_hash,
                    size_bytes,
                    mtime_unix,
                    mtime_ns,
                    inode: inode_of(&meta),
                    dev: dev_of(&meta),
                    nlink: nlink_of(&meta),
                    scan_generation: generation,
                }),
            ));

            if pending.len() >= UPSERT_BATCH {
                drain(&mut pending)?;
            }
        }

        drain(&mut pending)?;
        Ok(counts)
    }

    /// Whether a directory is the library's own work or trash area.
    ///
    /// Matched by *path*, not by name. The default name list cannot help here:
    /// an operator who sets `work_dir` to `.transcodarr-work` — or anywhere
    /// else inside the library root — gets a directory that is not called
    /// `work`, and discovery would then walk into it and enqueue transcodarr's
    /// own staged output and retained originals as source material. The staged
    /// output is a *partial* file, so that is not merely wasteful: it is
    /// transcoding a truncated file on purpose.
    fn is_own_area(&self, library: &LibraryRecord, path: &Path) -> bool {
        for dir in [&library.work_dir, &library.trash_dir] {
            if dir.is_empty() {
                continue;
            }
            let dir = Path::new(dir);
            if path == dir || path.starts_with(dir) {
                return true;
            }
        }
        false
    }

    fn is_excluded_dir(&self, entry: &walkdir::DirEntry) -> bool {
        if !entry.file_type().is_dir() {
            return false;
        }
        // The root itself is depth 0 and must never be excluded, however it
        // happens to be named — a library rooted at `/tank/media/work` is an
        // odd choice, but refusing to scan it at all is worse than odd.
        if entry.depth() == 0 {
            return false;
        }
        let name = entry.file_name().to_string_lossy();
        self.options.excluded_dirs.iter().any(|d| d == &name)
    }

    fn is_media(&self, path: &Path) -> bool {
        match path.extension() {
            Some(ext) => {
                let ext = ext.to_string_lossy().to_lowercase();
                self.options.media_extensions.iter().any(|m| m == &ext)
            }
            None => false,
        }
    }
}

/// Wait for a batch of upserts, surfacing the first failure.
fn drain(
    pending: &mut Vec<
        std::sync::mpsc::Receiver<
            Result<transcodarr_store::WriteAck, transcodarr_store::StoreError>,
        >,
    >,
) -> Result<(), ServerError> {
    let mut first_error = None;
    for rx in pending.drain(..) {
        match rx.recv() {
            Ok(Ok(_)) => {}
            // Every acknowledgement is drained before returning: leaving
            // receivers unread would let the writer block on a full reply
            // channel while the scanner is already unwinding.
            Ok(Err(e)) if first_error.is_none() => first_error = Some(ServerError::Store(e)),
            Ok(Err(_)) => {}
            Err(_) if first_error.is_none() => {
                first_error = Some(ServerError::Store(
                    transcodarr_store::StoreError::WriterStopped,
                ));
            }
            Err(_) => {}
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct WalkCounts {
    seen: i64,
    new: i64,
    changed: i64,
    too_recent: i64,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime_subsec_nanos(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| i64::from(d.subsec_nanos()))
        .unwrap_or(0)
}

/// Identity is `(dev, inode)`, not path: a moved file is the same file, and a
/// hardlinked one must not be processed twice through two names.
#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino() as i64)
}

#[cfg(unix)]
fn dev_of(meta: &std::fs::Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.dev() as i64)
}

#[cfg(unix)]
fn nlink_of(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() as i64
}

/// On platforms without stable inode numbers the columns stay `NULL` rather
/// than carrying a fabricated value. A missing identity is recoverable; a wrong
/// one silently merges two files into one row.
#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> Option<i64> {
    None
}

#[cfg(not(unix))]
fn dev_of(_meta: &std::fs::Metadata) -> Option<i64> {
    None
}

#[cfg(not(unix))]
fn nlink_of(_meta: &std::fs::Metadata) -> i64 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;
    use transcodarr_store::{Db, ReadPool, Writer};

    struct Harness {
        _db_dir: TempDir,
        root: TempDir,
        pool: ReadPool,
        writer: Arc<Writer>,
    }

    fn harness() -> Harness {
        let db_dir = TempDir::new().unwrap();
        let path = db_dir.path().join("t.db");
        let db = Db::open_unchecked(&path).unwrap();
        let pool = ReadPool::open(&path, 4).unwrap();
        Harness {
            _db_dir: db_dir,
            root: TempDir::new().unwrap(),
            pool,
            writer: Arc::new(Writer::start(db)),
        }
    }

    impl Harness {
        /// A library whose `min_mtime_age_s` is 0, so fixtures written moments
        /// ago are not skipped as still-being-written.
        fn library(&self) -> LibraryRecord {
            LibraryRecord {
                id: "tv".into(),
                name: "tv".into(),
                root_path: self.root.path().to_string_lossy().to_string(),
                work_dir: "/w".into(),
                trash_dir: "/t".into(),
                exclude_globs_json: "[]".into(),
                enabled: true,
                scan_parallelism: 4,
                priority: 0,
                min_mtime_age_s: 0,
            }
        }

        fn install_library(&self) -> LibraryRecord {
            let lib = self.library();
            self.writer
                .submit_blocking(WriteLane::Normal, LibraryRepo::upsert_op(lib.clone()))
                .unwrap();
            lib
        }

        fn touch(&self, rel: &str, bytes: usize) {
            let p = self.root.path().join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, vec![b'x'; bytes]).unwrap();
        }

        fn scanner(&self, options: ScanOptions) -> Scanner {
            Scanner::new(self.pool.clone(), Arc::clone(&self.writer), options)
        }

        fn files(&self) -> FileRepo {
            FileRepo::new(self.pool.clone())
        }
    }

    #[test]
    fn a_first_scan_discovers_every_media_file() {
        let h = harness();
        let lib = h.install_library();
        h.touch("a.mkv", 10);
        h.touch("season 1/b.mp4", 20);
        h.touch("season 1/nested/c.MKV", 30);

        let out = h
            .scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap();
        assert_eq!(out.files_seen, 3);
        assert_eq!(out.files_new, 3);
        assert_eq!(out.files_changed, 0);
        assert_eq!(out.files_missing, 0);
        assert_eq!(h.files().count_live("tv").unwrap(), 3);
    }

    /// Non-media is not the scanner's business. Recording an `.nfo` or a
    /// subtitle sidecar as a file would put it in the probe queue.
    #[test]
    fn non_media_files_are_ignored() {
        let h = harness();
        let lib = h.install_library();
        h.touch("a.mkv", 10);
        h.touch("a.nfo", 1);
        h.touch("a.srt", 1);
        h.touch("poster.jpg", 1);
        h.touch("no_extension", 1);

        let out = h
            .scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap();
        assert_eq!(out.files_seen, 1);
    }

    /// `.zfs` holds a copy of the library in every snapshot; `work` and `trash`
    /// hold transcodarr's own output. Descending either finds the same media
    /// many times over, or queues our own in-progress output as source.
    #[test]
    fn excluded_directories_are_never_descended() {
        let h = harness();
        let lib = h.install_library();
        h.touch("real.mkv", 10);
        h.touch(".zfs/snapshot/daily/real.mkv", 10);
        h.touch("work/in-progress.mkv", 10);
        h.touch("trash/deleted.mkv", 10);
        h.touch("@eaDir/thumb.mkv", 10);
        h.touch("lost+found/orphan.mkv", 10);

        let out = h
            .scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap();
        assert_eq!(out.files_seen, 1, "only the real file is media to us");
    }

    /// The default name list cannot save an operator who sets `work_dir` to
    /// something not called `work`. Discovery would then enqueue transcodarr's
    /// own staged output -- which is a *partial* file -- as source material.
    #[test]
    fn the_libraries_own_work_and_trash_areas_are_never_scanned() {
        let h = harness();
        let mut lib = h.library();
        lib.work_dir = h
            .root
            .path()
            .join(".transcodarr-work")
            .to_string_lossy()
            .to_string();
        lib.trash_dir = h
            .root
            .path()
            .join(".transcodarr-trash")
            .to_string_lossy()
            .to_string();
        h.writer
            .submit_blocking(WriteLane::Normal, LibraryRepo::upsert_op(lib.clone()))
            .unwrap();

        h.touch("real.mkv", 10);
        h.touch(".transcodarr-work/job1.0.partial.mkv", 10);
        h.touch(".transcodarr-trash/replaced.mkv", 10);

        let out = h
            .scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap();
        assert_eq!(
            out.files_seen, 1,
            "only the real file; our own staged output and retained originals must be invisible"
        );
    }

    /// A work area outside the library root is the normal case and must not
    /// affect the walk at all.
    #[test]
    fn a_work_area_outside_the_library_changes_nothing() {
        let h = harness();
        let lib = h.install_library();
        h.touch("a.mkv", 10);
        let out = h
            .scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap();
        assert_eq!(out.files_seen, 1);
    }

    /// A file still being written has a recent mtime. Recording it races the
    /// writer, and enqueueing it transcodes a partial file.
    #[test]
    fn a_recently_modified_file_is_left_alone() {
        let h = harness();
        let mut lib = h.install_library();
        lib.min_mtime_age_s = 3600;
        h.touch("just-written.mkv", 10);

        let out = h
            .scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap();
        assert_eq!(out.files_seen, 0);
        assert_eq!(out.files_too_recent, 1);
        assert_eq!(h.files().count_live("tv").unwrap(), 0);
    }

    /// Scanning twice over an unchanged library must record nothing new. If it
    /// did, every scan would invalidate every decision.
    #[test]
    fn a_rescan_of_an_unchanged_library_reports_no_changes() {
        let h = harness();
        let lib = h.install_library();
        h.touch("a.mkv", 10);
        h.touch("b.mkv", 10);
        let s = h.scanner(ScanOptions::default());

        s.scan_library(&lib, "full").unwrap();
        let second = s.scan_library(&lib, "full").unwrap();
        assert_eq!(second.files_seen, 2);
        assert_eq!(second.files_new, 0);
        assert_eq!(second.files_changed, 0);
        assert_eq!(second.files_missing, 0);
        assert_eq!(h.files().count_live("tv").unwrap(), 2);
    }

    #[test]
    fn a_resized_file_is_reported_as_changed() {
        let h = harness();
        let lib = h.install_library();
        h.touch("a.mkv", 10);
        let s = h.scanner(ScanOptions::default());
        s.scan_library(&lib, "full").unwrap();

        h.touch("a.mkv", 4096);
        let second = s.scan_library(&lib, "full").unwrap();
        assert_eq!(second.files_changed, 1);
        assert_eq!(second.files_new, 0);
    }

    /// Generations must advance, or the second scan marks the first scan's
    /// files missing.
    #[test]
    fn each_scan_takes_the_next_generation() {
        let h = harness();
        let lib = h.install_library();
        h.touch("a.mkv", 10);
        let s = h.scanner(ScanOptions::default());
        let first = s.scan_library(&lib, "full").unwrap();
        let second = s.scan_library(&lib, "full").unwrap();
        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 2);
        assert_ne!(first.run_id, second.run_id);
    }

    /// A deletion below the floor is recorded normally — the guard exists for
    /// mount failures, not for ordinary housekeeping.
    #[test]
    fn a_small_number_of_deletions_is_recorded() {
        let h = harness();
        let lib = h.install_library();
        for i in 0..10 {
            h.touch(&format!("f{i}.mkv"), 10);
        }
        let s = h.scanner(ScanOptions::default());
        s.scan_library(&lib, "full").unwrap();

        fs::remove_file(h.root.path().join("f0.mkv")).unwrap();
        let second = s.scan_library(&lib, "full").unwrap();
        assert_eq!(second.files_missing, 1);
        assert_eq!(h.files().count_live("tv").unwrap(), 9);
    }

    /// The guard. An unmounted library is indistinguishable from every file
    /// having been deleted, and marking 49,600 files missing because a mount
    /// was not ready destroys every open job and costs a full re-probe to undo.
    #[test]
    fn an_empty_library_root_aborts_rather_than_marking_everything_missing() {
        let h = harness();
        let lib = h.install_library();
        for i in 0..40 {
            h.touch(&format!("f{i}.mkv"), 10);
        }
        let s = h.scanner(ScanOptions::default());
        s.scan_library(&lib, "full").unwrap();

        // The mount goes away: the root is still a directory, but empty.
        for i in 0..40 {
            fs::remove_file(h.root.path().join(format!("f{i}.mkv"))).unwrap();
        }
        let err = s.scan_library(&lib, "full").unwrap_err();
        match err {
            ServerError::MassMissing {
                would_be_missing,
                live_before,
                ..
            } => {
                assert_eq!(would_be_missing, 40);
                assert_eq!(live_before, 40);
            }
            other => panic!("expected MassMissing, got {other:?}"),
        }

        assert_eq!(
            h.files().count_live("tv").unwrap(),
            40,
            "nothing may be marked missing when the guard fires"
        );
    }

    /// An aborted run must say why. A run that stopped early and looks like a
    /// clean one is how a mount failure becomes invisible.
    #[test]
    fn an_aborted_scan_records_its_reason_on_the_run() {
        let h = harness();
        let lib = h.install_library();
        for i in 0..40 {
            h.touch(&format!("f{i}.mkv"), 10);
        }
        let s = h.scanner(ScanOptions::default());
        s.scan_library(&lib, "full").unwrap();
        for i in 0..40 {
            fs::remove_file(h.root.path().join(format!("f{i}.mkv"))).unwrap();
        }
        let _ = s.scan_library(&lib, "full");

        let run = LibraryRepo::new(h.pool.clone())
            .last_scan_run("tv")
            .unwrap()
            .unwrap();
        assert_eq!(run.status, "aborted");
        assert!(
            run.aborted_reason
                .unwrap()
                .contains("would be marked missing"),
            "the reason must name the guard"
        );
    }

    /// A file that comes back is new work again: its row survived with its
    /// probe facts, but nothing has looked at it since it vanished.
    #[test]
    fn a_returning_file_counts_as_new_again() {
        let h = harness();
        let lib = h.install_library();
        for i in 0..10 {
            h.touch(&format!("f{i}.mkv"), 10);
        }
        let s = h.scanner(ScanOptions::default());
        s.scan_library(&lib, "full").unwrap();

        fs::remove_file(h.root.path().join("f0.mkv")).unwrap();
        s.scan_library(&lib, "full").unwrap();
        assert_eq!(h.files().count_live("tv").unwrap(), 9);

        h.touch("f0.mkv", 10);
        let third = s.scan_library(&lib, "full").unwrap();
        assert_eq!(third.files_new, 1);
        assert_eq!(h.files().count_live("tv").unwrap(), 10);
    }

    /// A missing root is a mount problem, not a permissions problem, and the
    /// operator action differs.
    #[test]
    fn a_missing_library_root_is_named_as_such() {
        let h = harness();
        let mut lib = h.install_library();
        lib.root_path = "/no/such/library/root".into();
        let err = h
            .scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap_err();
        assert!(
            matches!(err, ServerError::LibraryRootUnreadable { .. }),
            "{err:?}"
        );
    }

    /// The guard must not fire on the very first scan of a library, where every
    /// row is new and there is nothing to lose.
    #[test]
    fn a_first_scan_of_an_empty_library_is_not_a_mass_missing_event() {
        let h = harness();
        let lib = h.install_library();
        let out = h
            .scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap();
        assert_eq!(out.files_seen, 0);
        assert_eq!(out.files_missing, 0);
    }

    /// Identity is `(dev, inode)`. Without it a moved file is a new file and a
    /// hardlink is two files.
    #[cfg(unix)]
    #[test]
    fn discovery_records_filesystem_identity() {
        let h = harness();
        let lib = h.install_library();
        h.touch("a.mkv", 10);
        h.scanner(ScanOptions::default())
            .scan_library(&lib, "full")
            .unwrap();

        let rec = h
            .files()
            .get_by_path_hash(
                "tv",
                &transcodarr_core::stable_hash(
                    h.root.path().join("a.mkv").to_string_lossy().as_bytes(),
                ),
            )
            .unwrap()
            .unwrap();
        assert!(rec.inode.is_some(), "inode must be recorded");
        assert!(rec.dev.is_some(), "device must be recorded");
        assert_eq!(rec.nlink, 1);
    }
}
