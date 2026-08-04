// file: crates/transcodarr-store/src/repo/file.rs
// version: 1.3.0
// guid: b6f21e94-70a3-4c85-91d0-4a7e2c38f5b1
// last-edited: 2026-08-03
//! Files: discovery upserts, stored probe facts, and evaluation bookkeeping.

use rusqlite::{OptionalExtension, Row, params};
use transcodarr_core::facts::{FileFacts, SizeBucket};
use transcodarr_core::file::FileState;
use transcodarr_core::plan::BitDepth;
use transcodarr_core::policy::DecisionClass;

use crate::StoreError;
use crate::db::now_unix;
use crate::pool::ReadPool;
use crate::repo::parse_enum;
use crate::writer::WriteOp;

/// What discovery knows about a file: everything obtainable from a `stat`, and
/// nothing that requires opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUpsert {
    /// Owning library.
    pub library_id: String,
    /// Absolute path in canonical (server) form.
    pub canonical_path: String,
    /// Stable hash of the canonical path; the unique key within a library.
    pub path_hash: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Modification time, whole seconds.
    pub mtime_unix: i64,
    /// Sub-second part of the modification time.
    pub mtime_ns: i64,
    /// Inode, where the platform reports one.
    pub inode: Option<i64>,
    /// Device number, where the platform reports one.
    pub dev: Option<i64>,
    /// Link count. A file with `nlink > 1` is shared, and processing it twice
    /// through two paths would encode the same bytes twice.
    pub nlink: i64,
    /// The scan generation that saw it.
    pub scan_generation: i64,
}

/// A stored file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    /// Row id.
    pub id: i64,
    /// Owning library.
    pub library_id: String,
    /// Absolute path in canonical form.
    pub canonical_path: String,
    /// Stable hash of the canonical path.
    pub path_hash: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Modification time, whole seconds.
    pub mtime_unix: i64,
    /// Inode, where known.
    pub inode: Option<i64>,
    /// Device number, where known.
    pub dev: Option<i64>,
    /// Link count.
    pub nlink: i64,
    /// Where the file is in the discover → probe → evaluate cycle.
    pub state: FileState,
    /// Size band, set once probed.
    pub size_bucket: Option<SizeBucket>,
    /// Signature of the facts, used to detect that a source changed under a
    /// job that was planned against it.
    pub content_sig: Option<String>,
    /// Decision-relevant facts, present exactly when the file has been probed.
    ///
    /// `None` rather than a defaulted `FileFacts`: an unprobed file and a file
    /// probed as having no audio tracks must not look alike to the evaluator.
    pub facts: Option<FileFacts>,
    /// The last recorded decision class.
    pub decision: Option<DecisionClass>,
    /// Why, in words an operator can act on.
    pub decision_reason: Option<String>,
    /// Rules version the decision was made under.
    pub eval_rules_version: Option<String>,
    /// How many consecutive evaluations reached the same decision. Guards
    /// against a policy that re-decides the same file forever.
    pub same_decision_streak: i64,
    /// Generation of the scan that last saw it.
    pub scan_generation: i64,
    /// When a scan last saw it.
    pub last_seen_unix: i64,
}

/// Just enough of a stored file to classify a walked one.
///
/// `state` stays a `String` here on purpose: the scanner only compares it
/// against `Missing` to count a returning file as new, and parsing an enum
/// ~49,600 times to answer one equality test is work with no reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    /// Row id.
    pub id: i64,
    /// Size in bytes as last recorded.
    pub size_bytes: i64,
    /// Modification time, whole seconds.
    pub mtime_unix: i64,
    /// Sub-second part of the modification time.
    pub mtime_ns: i64,
    /// Inode, where known.
    pub inode: Option<i64>,
    /// Device number, where known.
    pub dev: Option<i64>,
    /// Stored lifecycle state.
    pub state: String,
}

/// Whether an upsert is looking at a file that actually changed on disk.
///
/// `mtime_ns` is part of the test, not decoration: a same-second, same-size
/// rewrite is still a rewrite, and comparing whole seconds alone would let one
/// through as unchanged.
///
/// Written once and interpolated wherever the upsert needs it, so the branches
/// that invalidate stored knowledge cannot come to disagree about what
/// "changed" means.
const FILE_CHANGED: &str = "(file.size_bytes <> excluded.size_bytes
                             OR file.mtime_unix <> excluded.mtime_unix
                             OR file.mtime_ns <> excluded.mtime_ns)";

/// Columns selected for a [`FileRecord`], in one place so the `SELECT` and the
/// row mapping cannot drift apart.
const FILE_COLUMNS: &str = "
    id, library_id, canonical_path, path_hash, size_bytes, mtime_unix, inode, dev, nlink,
    state, size_bucket, content_sig, container, duration_s, bitrate_bps, video_codec,
    video_profile, video_bit_depth, video_pix_fmt, video_width, video_height, is_hdr,
    is_dovi, dovi_profile, has_object_audio, audio_codecs, audio_track_count,
    subtitle_track_count, probe_at_unix, decision, decision_reason, eval_rules_version,
    same_decision_streak, scan_generation, last_seen_unix";

impl FileRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Result<Self, StoreError>> {
        let state = match parse_enum(
            "file.state",
            &row.get::<_, String>("state")?,
            FileState::parse,
        ) {
            Ok(s) => s,
            Err(e) => return Ok(Err(e)),
        };
        let size_bucket = match row.get::<_, Option<String>>("size_bucket")? {
            Some(raw) => match parse_enum("file.size_bucket", &raw, SizeBucket::parse) {
                Ok(b) => Some(b),
                Err(e) => return Ok(Err(e)),
            },
            None => None,
        };
        let decision = match row.get::<_, Option<String>>("decision")? {
            Some(raw) => match parse_enum("file.decision", &raw, DecisionClass::parse) {
                Ok(d) => Some(d),
                Err(e) => return Ok(Err(e)),
            },
            None => None,
        };

        // Facts exist exactly when the file has been probed. Rebuilding them
        // from a partially-populated row would hand the evaluator a defaulted
        // `FileFacts` that is indistinguishable from a real one.
        let facts = if row.get::<_, Option<i64>>("probe_at_unix")?.is_some() {
            Some(facts_from_row(row)?)
        } else {
            None
        };

        Ok(Ok(Self {
            id: row.get("id")?,
            library_id: row.get("library_id")?,
            canonical_path: row.get("canonical_path")?,
            path_hash: row.get("path_hash")?,
            size_bytes: row.get("size_bytes")?,
            mtime_unix: row.get("mtime_unix")?,
            inode: row.get("inode")?,
            dev: row.get("dev")?,
            nlink: row.get("nlink")?,
            state,
            size_bucket,
            content_sig: row.get("content_sig")?,
            facts,
            decision,
            decision_reason: row.get("decision_reason")?,
            eval_rules_version: row.get("eval_rules_version")?,
            same_decision_streak: row.get("same_decision_streak")?,
            scan_generation: row.get("scan_generation")?,
            last_seen_unix: row.get("last_seen_unix")?,
        }))
    }
}

/// Reads and writes over `file`.
#[derive(Debug, Clone)]
pub struct FileRepo {
    pool: ReadPool,
}

impl FileRepo {
    /// Bind to a read pool.
    pub fn new(pool: ReadPool) -> Self {
        Self { pool }
    }

    /// One file by row id.
    pub fn get(&self, id: i64) -> Result<FileRecord, StoreError> {
        let c = self.pool.get()?;
        let found = c
            .query_row(
                &format!("SELECT {FILE_COLUMNS} FROM file WHERE id = ?1"),
                [id],
                FileRecord::from_row,
            )
            .optional()?;
        match found {
            Some(r) => r,
            None => Err(StoreError::NotFound {
                kind: "file",
                id: id.to_string(),
            }),
        }
    }

    /// The minimum needed to decide whether a walked file is new, changed, or
    /// already known.
    ///
    /// Deliberately not a full [`FileRecord`]: this is the only per-file lookup
    /// in a scan, so on the real libraries it runs ~49,600 times. Reconstructing
    /// facts and parsing three enums for each of them, only to discard the
    /// result for every unchanged file, is the difference between a scan that
    /// finishes and one that is the bottleneck.
    pub fn identity(
        &self,
        library_id: &str,
        path_hash: &str,
    ) -> Result<Option<FileIdentity>, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            "SELECT id, size_bytes, mtime_unix, mtime_ns, inode, dev, state
             FROM file WHERE library_id = ?1 AND path_hash = ?2",
            params![library_id, path_hash],
            |r| {
                Ok(FileIdentity {
                    id: r.get(0)?,
                    size_bytes: r.get(1)?,
                    mtime_unix: r.get(2)?,
                    mtime_ns: r.get(3)?,
                    inode: r.get(4)?,
                    dev: r.get(5)?,
                    state: r.get::<_, String>(6)?,
                })
            },
        )
        .optional()?)
    }

    /// One file by its path within a library.
    pub fn get_by_path_hash(
        &self,
        library_id: &str,
        path_hash: &str,
    ) -> Result<Option<FileRecord>, StoreError> {
        let c = self.pool.get()?;
        let found = c
            .query_row(
                &format!(
                    "SELECT {FILE_COLUMNS} FROM file WHERE library_id = ?1 AND path_hash = ?2"
                ),
                params![library_id, path_hash],
                FileRecord::from_row,
            )
            .optional()?;
        found.transpose()
    }

    /// Files awaiting a probe, oldest first.
    pub fn needs_probe(&self, library_id: &str, limit: u32) -> Result<Vec<FileRecord>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM file
             WHERE library_id = ?1 AND state = 'Discovered'
             ORDER BY id LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![library_id, limit], FileRecord::from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    /// Files whose decision predates `rules_version`, in evaluator batches.
    ///
    /// This is the query [`idx_file_needs_eval`](../../migrations) exists for.
    /// `IS NOT` rather than `<>` on purpose: a file that has never been
    /// evaluated has `eval_rules_version IS NULL`, and `NULL <> 'v1'` is `NULL`,
    /// which is not true — so `<>` would silently skip every unevaluated file,
    /// which is precisely the set that most needs evaluating.
    pub fn needs_eval(
        &self,
        library_id: &str,
        rules_version: &str,
        limit: u32,
    ) -> Result<Vec<FileRecord>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM file
             WHERE library_id = ?1
               AND state IN ('Probed','Evaluated')
               AND eval_rules_version IS NOT ?2
             ORDER BY id LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![library_id, rules_version, limit], |r| {
            FileRecord::from_row(r)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    /// How many live files a scan generation did not touch.
    ///
    /// The mass-missing guard reads this before marking anything: an unmounted
    /// library looks exactly like every file having been deleted, and the only
    /// difference visible from here is the proportion.
    pub fn count_not_seen_in(
        &self,
        library_id: &str,
        scan_generation: i64,
    ) -> Result<i64, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            "SELECT COUNT(*) FROM file
             WHERE library_id = ?1 AND scan_generation < ?2 AND state <> 'Missing'",
            params![library_id, scan_generation],
            |r| r.get(0),
        )?)
    }

    /// Per-decision file counts and byte totals for a library.
    ///
    /// Aggregated in SQL. The Phase 2 claim is that this answers "what needs
    /// transcoding across 85 TB" in under a second over ~49,600 rows; fetching
    /// every row across the crate boundary to sum it in the caller would make
    /// that false for no benefit — and is exactly the pattern the no-SQL-escapes
    /// rule exists to prevent.
    pub fn decision_totals(
        &self,
        library_id: &str,
    ) -> Result<Vec<(Option<String>, i64, i64)>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(
            "SELECT decision, COUNT(*), COALESCE(SUM(size_bytes), 0) FROM file
             WHERE library_id = ?1 AND state <> 'Missing'
             GROUP BY decision ORDER BY 3 DESC",
        )?;
        let rows = stmt.query_map([library_id], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Live file count and total bytes for a library.
    pub fn totals(&self, library_id: &str) -> Result<(i64, i64), StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM file
             WHERE library_id = ?1 AND state <> 'Missing'",
            [library_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// How many files sit in each lifecycle state.
    pub fn state_counts(&self, library_id: &str) -> Result<Vec<(FileState, i64)>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt =
            c.prepare("SELECT state, COUNT(*) FROM file WHERE library_id = ?1 GROUP BY state")?;
        let rows = stmt.query_map([library_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (raw, n) = row?;
            out.push((parse_enum("file.state", &raw, FileState::parse)?, n));
        }
        Ok(out)
    }

    /// Total live files in a library.
    pub fn count_live(&self, library_id: &str) -> Result<i64, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            "SELECT COUNT(*) FROM file WHERE library_id = ?1 AND state <> 'Missing'",
            [library_id],
            |r| r.get(0),
        )?)
    }

    /// Insert a discovered file, or refresh what a `stat` can see.
    ///
    /// Idempotent, and deliberately conservative: stored probe facts survive an
    /// upsert unless the size or mtime actually moved. Discarding them on every
    /// scan would re-probe 49,600 files nightly for no new information, and a
    /// rescan is not evidence that a file changed.
    ///
    /// Reports the row id it settled on, which the caller needs to attach
    /// streams and jobs — `last_insert_rowid()` cannot supply it, because the
    /// `DO UPDATE` branch inserts nothing.
    pub fn upsert_op(f: FileUpsert) -> WriteOp {
        WriteOp::new_with_id(format!("file.upsert:{}", f.path_hash), move |c| {
            let now = now_unix();
            let id: i64 = c.query_row(
                &format!(
                    "INSERT INTO file
                   (library_id, canonical_path, path_hash, size_bytes, mtime_unix, mtime_ns,
                    inode, dev, nlink, state, first_seen_unix, last_seen_unix, scan_generation)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'Discovered',?10,?10,?11)
                 ON CONFLICT(library_id, path_hash) DO UPDATE SET
                   canonical_path  = excluded.canonical_path,
                   size_bytes      = excluded.size_bytes,
                   mtime_unix      = excluded.mtime_unix,
                   mtime_ns        = excluded.mtime_ns,
                   inode           = excluded.inode,
                   dev             = excluded.dev,
                   nlink           = excluded.nlink,
                   last_seen_unix  = excluded.last_seen_unix,
                   scan_generation = excluded.scan_generation,
                   -- Only a real change invalidates what we know. A file that
                   -- came back after being marked Missing is rediscovered.
                   state = CASE
                     WHEN {CHANGED} THEN 'Discovered'
                     WHEN file.state = 'Missing' THEN 'Discovered'
                     ELSE file.state
                   END,
                   -- Everything describing a decision dies together with the
                   -- signature. Clearing only `content_sig` leaves a changed,
                   -- re-probed file matching its old `eval_rules_version`, so
                   -- the evaluator skips it and a decision computed from facts
                   -- that no longer exist stands until the rules version moves.
                   content_sig = CASE
                     WHEN {CHANGED} THEN NULL ELSE file.content_sig END,
                   eval_rules_version = CASE
                     WHEN {CHANGED} THEN NULL ELSE file.eval_rules_version END,
                   decision = CASE
                     WHEN {CHANGED} THEN NULL ELSE file.decision END,
                   decision_reason = CASE
                     WHEN {CHANGED} THEN NULL ELSE file.decision_reason END,
                   same_decision_streak = CASE
                     WHEN {CHANGED} THEN 0 ELSE file.same_decision_streak END
                 RETURNING id",
                    CHANGED = FILE_CHANGED
                ),
                params![
                    f.library_id,
                    f.canonical_path,
                    f.path_hash,
                    f.size_bytes,
                    f.mtime_unix,
                    f.mtime_ns,
                    f.inode,
                    f.dev,
                    f.nlink,
                    now,
                    f.scan_generation,
                ],
                |r| r.get(0),
            )?;
            Ok((1, id))
        })
    }

    /// Mark everything a scan generation did not see as missing.
    ///
    /// Rows are never deleted. A file that reappears is rediscovered by the
    /// next upsert with its probe facts intact, and a mistaken sweep is
    /// therefore recoverable rather than a re-probe of the whole library.
    pub fn mark_missing_op(library_id: String, scan_generation: i64) -> WriteOp {
        WriteOp::new(format!("file.mark_missing:{library_id}"), move |c| {
            Ok(c.execute(
                "UPDATE file SET state = 'Missing'
                 WHERE library_id = ?1 AND scan_generation < ?2 AND state <> 'Missing'",
                params![library_id, scan_generation],
            )? as u64)
        })
    }

    /// Store probe results against a file.
    pub fn record_probe_op(
        file_id: i64,
        facts: FileFacts,
        content_sig: String,
        size_bucket: SizeBucket,
        probe_json: String,
        tool_version: String,
    ) -> WriteOp {
        WriteOp::new(format!("file.record_probe:{file_id}"), move |c| {
            Ok(c.execute(
                "UPDATE file SET
                   state = 'Probed',
                   container = ?2,
                   duration_s = ?3,
                   bitrate_bps = ?4,
                   video_codec = ?5,
                   video_profile = ?6,
                   video_bit_depth = ?7,
                   video_pix_fmt = ?8,
                   video_width = ?9,
                   video_height = ?10,
                   is_hdr = ?11,
                   is_dovi = ?12,
                   dovi_profile = ?13,
                   has_object_audio = ?14,
                   audio_codecs = ?15,
                   audio_track_count = ?16,
                   subtitle_track_count = ?17,
                   content_sig = ?18,
                   size_bucket = ?19,
                   probe_json = ?20,
                   probe_tool_version = ?21,
                   probe_at_unix = ?22
                 WHERE id = ?1",
                params![
                    file_id,
                    facts.container,
                    facts.duration_us.map(|us| us as f64 / 1_000_000.0),
                    facts.bit_rate_bps.map(|b| b as i64),
                    facts.video_codec,
                    facts.video_profile,
                    facts.video_bit_depth.map(|d| i64::from(d.bits())),
                    facts.video_pix_fmt,
                    facts.width.map(i64::from),
                    facts.height.map(i64::from),
                    i64::from(facts.is_hdr),
                    i64::from(facts.is_dovi),
                    facts.dovi_profile.map(i64::from),
                    i64::from(facts.has_object_audio),
                    facts.audio_codecs.join(","),
                    facts.audio_track_count as i64,
                    facts.subtitle_track_count as i64,
                    content_sig,
                    size_bucket.as_str(),
                    probe_json,
                    tool_version,
                    now_unix(),
                ],
            )? as u64)
        })
    }

    /// Record that a probe failed.
    ///
    /// A distinct state rather than leaving the file `Discovered`: an
    /// unprobeable file must not be retried forever as though it were merely
    /// new, and it must not be mistaken for a file with no work to do.
    pub fn record_probe_failure_op(file_id: i64, reason: String) -> WriteOp {
        WriteOp::new(format!("file.probe_failed:{file_id}"), move |c| {
            Ok(c.execute(
                "UPDATE file SET state = 'ProbeFailed', decision_reason = ?2 WHERE id = ?1",
                params![file_id, reason],
            )? as u64)
        })
    }

    /// Record a policy decision against stored facts.
    ///
    /// `same_decision_streak` advances only when the decision is unchanged, and
    /// resets otherwise — a policy that keeps re-deciding the same file the
    /// same way is visible in that counter before it becomes visible as load.
    pub fn record_decision_op(
        file_id: i64,
        decision: DecisionClass,
        reason: String,
        rules_version: String,
    ) -> WriteOp {
        WriteOp::new(format!("file.record_decision:{file_id}"), move |c| {
            Ok(c.execute(
                "UPDATE file SET
                   state = 'Evaluated',
                   decision = ?2,
                   decision_reason = ?3,
                   eval_rules_version = ?4,
                   eval_at_unix = ?5,
                   same_decision_streak =
                     CASE WHEN file.decision IS ?2 THEN file.same_decision_streak + 1 ELSE 0 END
                 WHERE id = ?1",
                params![
                    file_id,
                    decision.as_str(),
                    reason,
                    rules_version,
                    now_unix(),
                ],
            )? as u64)
        })
    }

    /// Move a file to an explicit state.
    pub fn set_state_op(file_id: i64, state: FileState) -> WriteOp {
        WriteOp::new(format!("file.set_state:{file_id}"), move |c| {
            Ok(c.execute(
                "UPDATE file SET state = ?2 WHERE id = ?1",
                params![file_id, state.as_str()],
            )? as u64)
        })
    }
}

/// Reconstruct facts from stored columns.
///
/// Free function rather than a method so the column list and the reconstruction
/// sit next to each other in one file and are reviewed together.
fn facts_from_row(row: &Row<'_>) -> rusqlite::Result<FileFacts> {
    let audio_codecs: Option<String> = row.get("audio_codecs")?;
    let bit_depth: Option<i64> = row.get("video_bit_depth")?;
    Ok(FileFacts {
        container: row
            .get::<_, Option<String>>("container")?
            .unwrap_or_default(),
        duration_us: row
            .get::<_, Option<f64>>("duration_s")?
            .map(|s| (s * 1_000_000.0) as u64),
        size_bytes: row.get::<_, i64>("size_bytes")? as u64,
        bit_rate_bps: row.get::<_, Option<i64>>("bitrate_bps")?.map(|b| b as u64),
        video_codec: row.get("video_codec")?,
        video_profile: row.get("video_profile")?,
        video_bit_depth: bit_depth.and_then(|b| BitDepth::from_bits(b as u8)),
        video_pix_fmt: row.get("video_pix_fmt")?,
        width: row.get::<_, Option<i64>>("video_width")?.map(|v| v as u32),
        height: row.get::<_, Option<i64>>("video_height")?.map(|v| v as u32),
        is_hdr: row.get::<_, i64>("is_hdr")? != 0,
        is_dovi: row.get::<_, i64>("is_dovi")? != 0,
        dovi_profile: row.get::<_, Option<i64>>("dovi_profile")?.map(|v| v as u8),
        has_object_audio: row.get::<_, i64>("has_object_audio")? != 0,
        audio_codecs: audio_codecs
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        audio_track_count: row.get::<_, i64>("audio_track_count")? as usize,
        subtitle_track_count: row.get::<_, i64>("subtitle_track_count")? as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests_support::{Fixture, fixture};

    fn upsert(path: &str, size: i64, mtime: i64, generation: i64) -> FileUpsert {
        FileUpsert {
            library_id: "tv".into(),
            canonical_path: format!("/mnt/tv/{path}"),
            path_hash: transcodarr_core::stable_hash(path.as_bytes()),
            size_bytes: size,
            mtime_unix: mtime,
            mtime_ns: 0,
            inode: Some(42),
            dev: Some(7),
            nlink: 1,
            scan_generation: generation,
        }
    }

    /// `size_bytes` is threaded through because the store keeps exactly one
    /// authority for it — the `file.size_bytes` column, maintained by
    /// discovery. `record_probe_op` deliberately does not write it, so
    /// reconstructed facts carry the row's size rather than whatever a stale
    /// probe struct happened to hold.
    fn probed_facts(size_bytes: u64) -> FileFacts {
        FileFacts {
            container: "matroska".into(),
            duration_us: Some(1_500_000_000),
            size_bytes,
            bit_rate_bps: Some(5_000_000),
            video_codec: Some("hevc".into()),
            video_profile: Some("Main 10".into()),
            video_bit_depth: Some(BitDepth::Ten),
            video_pix_fmt: Some("yuv420p10le".into()),
            width: Some(1920),
            height: Some(1080),
            is_hdr: false,
            is_dovi: false,
            dovi_profile: None,
            has_object_audio: false,
            audio_codecs: vec!["truehd".into(), "ac3".into()],
            audio_track_count: 2,
            subtitle_track_count: 3,
        }
    }

    fn seeded() -> (Fixture, FileRepo) {
        let f = fixture();
        f.seed_library("tv");
        let repo = FileRepo::new(f.pool.clone());
        (f, repo)
    }

    #[test]
    fn a_discovered_file_round_trips_and_reports_its_id() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .expect("upsert must report the row id it settled on");
        let got = repo.get(id).unwrap();
        assert_eq!(got.canonical_path, "/mnt/tv/a.mkv");
        assert_eq!(got.state, FileState::Discovered);
        assert_eq!(got.inode, Some(42));
        assert!(got.facts.is_none(), "an unprobed file has no facts");
    }

    /// The upsert is keyed on the path, so rescanning must update rather than
    /// duplicate — and must report the same row id both times.
    #[test]
    fn rescanning_updates_the_same_row() {
        let (f, repo) = seeded();
        let first = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id;
        let second = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 2)))
            .last_id;
        assert_eq!(first, second);
        assert_eq!(repo.count_live("tv").unwrap(), 1);
        assert_eq!(repo.get(second.unwrap()).unwrap().scan_generation, 2);
    }

    /// Probe facts are expensive. A rescan that found nothing changed is not
    /// evidence that a file changed, and discarding facts on every scan would
    /// re-probe the whole library nightly for no new information.
    #[test]
    fn an_unchanged_rescan_keeps_stored_probe_facts() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::record_probe_op(
            id,
            probed_facts(100),
            "sig-1".into(),
            SizeBucket::Small,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));
        f.write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 2)));

        let got = repo.get(id).unwrap();
        assert_eq!(got.state, FileState::Probed);
        assert_eq!(got.content_sig.as_deref(), Some("sig-1"));
        assert_eq!(got.facts.unwrap(), probed_facts(100));
    }

    /// ...but a file whose size or mtime moved is a different file as far as
    /// any stored plan is concerned, and its signature must not survive.
    #[test]
    fn a_changed_file_is_rediscovered_and_loses_its_signature() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::record_probe_op(
            id,
            probed_facts(100),
            "sig-1".into(),
            SizeBucket::Small,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));
        f.write(FileRepo::upsert_op(upsert("a.mkv", 200, 99, 2)));

        let got = repo.get(id).unwrap();
        assert_eq!(got.state, FileState::Discovered);
        assert_eq!(
            got.content_sig, None,
            "a stale signature would let a job run against the wrong bytes"
        );
    }

    /// Every field must survive the round trip. Bit depth especially: losing
    /// 10-bit would mean planning an 8-bit encode, which is the upconversion
    /// that must never happen.
    #[test]
    fn probe_facts_round_trip_including_bit_depth() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 1024, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::record_probe_op(
            id,
            probed_facts(1024),
            "sig".into(),
            SizeBucket::Medium,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));
        let got = repo.get(id).unwrap();
        let facts = got.facts.unwrap();
        assert_eq!(facts.video_bit_depth, Some(BitDepth::Ten));
        assert_eq!(facts.audio_codecs, vec!["truehd", "ac3"]);
        assert_eq!(facts.subtitle_track_count, 3);
        assert_eq!(facts, probed_facts(1024));
        assert_eq!(got.size_bucket, Some(SizeBucket::Medium));
    }

    /// `eval_rules_version IS NULL` must be picked up. Written with `<>` this
    /// returns NULL rather than true, and every never-evaluated file — the set
    /// that most needs evaluating — is silently skipped.
    #[test]
    fn never_evaluated_files_are_in_the_evaluator_batch() {
        let (f, repo) = seeded();
        for (i, name) in ["a.mkv", "b.mkv"].iter().enumerate() {
            let id = f
                .write(FileRepo::upsert_op(upsert(name, 100, 10, 1)))
                .last_id
                .unwrap();
            f.write(FileRepo::record_probe_op(
                id,
                probed_facts(100),
                format!("sig-{i}"),
                SizeBucket::Small,
                "{}".into(),
                "ffprobe 7.0".into(),
            ));
        }
        let batch = repo.needs_eval("tv", "v1", 1000).unwrap();
        assert_eq!(batch.len(), 2, "unevaluated files must not be skipped");
    }

    #[test]
    fn a_file_evaluated_under_the_current_rules_leaves_the_batch() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::record_probe_op(
            id,
            probed_facts(100),
            "sig".into(),
            SizeBucket::Small,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));
        f.write(FileRepo::record_decision_op(
            id,
            DecisionClass::Audio,
            "lossless audio".into(),
            "v1".into(),
        ));

        assert!(repo.needs_eval("tv", "v1", 1000).unwrap().is_empty());
        assert_eq!(
            repo.needs_eval("tv", "v2", 1000).unwrap().len(),
            1,
            "a new rules version must bring every file back"
        );
    }

    /// The streak counter is how a policy that re-decides the same file forever
    /// becomes visible before it becomes visible as load.
    #[test]
    fn the_same_decision_streak_advances_and_resets() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::record_probe_op(
            id,
            probed_facts(100),
            "sig".into(),
            SizeBucket::Small,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));

        for expected in [0, 1, 2] {
            f.write(FileRepo::record_decision_op(
                id,
                DecisionClass::Audio,
                "same".into(),
                "v1".into(),
            ));
            assert_eq!(repo.get(id).unwrap().same_decision_streak, expected);
        }

        f.write(FileRepo::record_decision_op(
            id,
            DecisionClass::Video,
            "changed".into(),
            "v2".into(),
        ));
        let got = repo.get(id).unwrap();
        assert_eq!(got.same_decision_streak, 0);
        assert_eq!(got.decision, Some(DecisionClass::Video));
    }

    /// Rows are never deleted. A file that comes back is rediscovered with its
    /// facts intact, so a mistaken sweep costs nothing to undo.
    #[test]
    fn a_missing_file_is_marked_not_deleted_and_can_return() {
        let (f, repo) = seeded();
        let stays = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        let goes = f
            .write(FileRepo::upsert_op(upsert("b.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::record_probe_op(
            goes,
            probed_facts(100),
            "sig-b".into(),
            SizeBucket::Small,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));

        // A second scan sees only a.mkv.
        f.write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 2)));
        assert_eq!(repo.count_not_seen_in("tv", 2).unwrap(), 1);
        f.write(FileRepo::mark_missing_op("tv".into(), 2));

        assert_eq!(repo.get(goes).unwrap().state, FileState::Missing);
        assert_eq!(repo.get(stays).unwrap().state, FileState::Discovered);
        assert_eq!(repo.count_live("tv").unwrap(), 1);

        // ...and the third scan finds it again, facts intact.
        f.write(FileRepo::upsert_op(upsert("b.mkv", 100, 10, 3)));
        let back = repo.get(goes).unwrap();
        assert_eq!(back.state, FileState::Discovered);
        assert_eq!(
            back.facts,
            Some(probed_facts(100)),
            "probe data must survive"
        );
    }

    /// The stale-decision hole. A file that changed on disk and was re-probed
    /// carries brand-new facts and a decision computed from the old ones — so
    /// it must return to the evaluator even though the rules version has not
    /// moved. Without this, an in-place replacement by Sonarr or Radarr leaves
    /// a decision that nothing will ever revisit.
    #[test]
    fn a_changed_and_reprobed_file_returns_to_the_evaluator() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::record_probe_op(
            id,
            probed_facts(100),
            "sig-1".into(),
            SizeBucket::Small,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));
        f.write(FileRepo::record_decision_op(
            id,
            DecisionClass::Audio,
            "lossless audio".into(),
            "v1".into(),
        ));
        assert!(repo.needs_eval("tv", "v1", 100).unwrap().is_empty());

        // The file is replaced on disk, then re-probed.
        f.write(FileRepo::upsert_op(upsert("a.mkv", 555, 99, 2)));
        f.write(FileRepo::record_probe_op(
            id,
            probed_facts(555),
            "sig-2".into(),
            SizeBucket::Small,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));

        let batch = repo.needs_eval("tv", "v1", 100).unwrap();
        assert_eq!(
            batch.len(),
            1,
            "new facts must invalidate the decision made from the old ones"
        );
        let got = repo.get(id).unwrap();
        assert_eq!(got.decision, None, "the stale decision must be cleared");
        assert_eq!(got.same_decision_streak, 0);
    }

    /// A same-second, same-size rewrite is still a rewrite. `mtime_ns` is
    /// collected precisely so this case is not invisible.
    #[test]
    fn a_sub_second_modification_counts_as_a_change() {
        let (f, repo) = seeded();
        let mut first = upsert("a.mkv", 100, 10, 1);
        first.mtime_ns = 1;
        let id = f.write(FileRepo::upsert_op(first)).last_id.unwrap();
        f.write(FileRepo::record_probe_op(
            id,
            probed_facts(100),
            "sig-1".into(),
            SizeBucket::Small,
            "{}".into(),
            "ffprobe 7.0".into(),
        ));

        let mut again = upsert("a.mkv", 100, 10, 2);
        again.mtime_ns = 999_000_000;
        f.write(FileRepo::upsert_op(again));

        let got = repo.get(id).unwrap();
        assert_eq!(got.state, FileState::Discovered);
        assert_eq!(got.content_sig, None);
    }

    /// The mass-missing guard's input. An unmounted library looks exactly like
    /// every file having been deleted; the proportion is the only difference
    /// visible from the database.
    #[test]
    fn count_not_seen_in_reports_the_whole_library_when_nothing_was_seen() {
        let (f, repo) = seeded();
        for name in ["a.mkv", "b.mkv", "c.mkv"] {
            f.write(FileRepo::upsert_op(upsert(name, 100, 10, 1)));
        }
        assert_eq!(repo.count_not_seen_in("tv", 2).unwrap(), 3);
        assert_eq!(repo.count_live("tv").unwrap(), 3);
    }

    #[test]
    fn a_probe_failure_is_its_own_state_not_a_silent_no_work() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::record_probe_failure_op(
            id,
            "moov atom not found".into(),
        ));
        let got = repo.get(id).unwrap();
        assert_eq!(got.state, FileState::ProbeFailed);
        assert!(
            !repo
                .needs_probe("tv", 100)
                .unwrap()
                .iter()
                .any(|x| x.id == id)
        );
        assert!(repo.needs_eval("tv", "v1", 100).unwrap().is_empty());
        assert_eq!(got.decision_reason.as_deref(), Some("moov atom not found"));
    }

    #[test]
    fn discovered_files_are_queued_for_probing() {
        let (f, repo) = seeded();
        f.write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)));
        f.write(FileRepo::upsert_op(upsert("b.mkv", 100, 10, 1)));
        assert_eq!(repo.needs_probe("tv", 100).unwrap().len(), 2);
        assert_eq!(
            repo.needs_probe("tv", 1).unwrap().len(),
            1,
            "the limit binds"
        );
    }

    #[test]
    fn a_missing_file_id_reports_not_found() {
        let (_f, repo) = seeded();
        let e = repo.get(9999).unwrap_err();
        assert!(
            matches!(e, StoreError::NotFound { kind: "file", .. }),
            "{e:?}"
        );
    }

    #[test]
    fn lookup_by_path_hash_finds_the_row_or_says_it_is_absent() {
        let (f, repo) = seeded();
        let up = upsert("a.mkv", 100, 10, 1);
        let hash = up.path_hash.clone();
        f.write(FileRepo::upsert_op(up));
        assert!(repo.get_by_path_hash("tv", &hash).unwrap().is_some());
        assert!(repo.get_by_path_hash("tv", "nope").unwrap().is_none());
    }

    #[test]
    fn set_state_moves_a_file_explicitly() {
        let (f, repo) = seeded();
        let id = f
            .write(FileRepo::upsert_op(upsert("a.mkv", 100, 10, 1)))
            .last_id
            .unwrap();
        f.write(FileRepo::set_state_op(id, FileState::Quarantined));
        assert_eq!(repo.get(id).unwrap().state, FileState::Quarantined);
    }
}
