// file: crates/transcodarr-store/src/repo/library.rs
// version: 1.0.0
// guid: 5e83d0b7-41c9-4a26-8f05-b7d2e39c1408
// last-edited: 2026-08-03
//! Libraries and scan-run accounting.

use rusqlite::{Row, params};

use crate::StoreError;
use crate::db::now_unix;
use crate::pool::ReadPool;
use crate::writer::WriteOp;

/// A configured library root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRecord {
    /// Stable identifier.
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// Root directory scanned.
    pub root_path: String,
    /// Where agents stage output.
    pub work_dir: String,
    /// Where replaced originals are retained.
    pub trash_dir: String,
    /// Extra exclusions on top of the built-in list, as stored JSON.
    pub exclude_globs_json: String,
    /// Whether the library participates in scanning and dispatch.
    pub enabled: bool,
    /// How many walker threads discovery may use.
    pub scan_parallelism: i64,
    /// Ordering hint between libraries.
    pub priority: i64,
    /// A file younger than this is skipped: a recent mtime means something may
    /// still be writing it, and enqueueing it races the writer.
    pub min_mtime_age_s: i64,
}

impl LibraryRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            root_path: row.get("root_path")?,
            work_dir: row.get("work_dir")?,
            trash_dir: row.get("trash_dir")?,
            exclude_globs_json: row.get("exclude_globs_json")?,
            enabled: row.get::<_, i64>("enabled")? != 0,
            scan_parallelism: row.get("scan_parallelism")?,
            priority: row.get("priority")?,
            min_mtime_age_s: row.get("min_mtime_age_s")?,
        })
    }
}

/// One pass of discovery over one library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRun {
    /// Row id.
    pub id: i64,
    /// Which library.
    pub library_id: String,
    /// How the scan was triggered.
    pub mode: String,
    /// Monotonically increasing per library; rows not touched by the current
    /// generation are the candidates for "missing".
    pub scan_generation: i64,
    /// `running`, `ok`, or `aborted`.
    pub status: String,
    /// Files walked.
    pub files_seen: i64,
    /// Files inserted.
    pub files_new: i64,
    /// Files whose size or mtime moved.
    pub files_changed: i64,
    /// Files the walk expected and did not find.
    pub files_missing: i64,
    /// Why the run gave up, when it did.
    pub aborted_reason: Option<String>,
}

impl ScanRun {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            library_id: row.get("library_id")?,
            mode: row.get("mode")?,
            scan_generation: row.get("scan_generation")?,
            status: row.get("status")?,
            files_seen: row.get("files_seen")?,
            files_new: row.get("files_new")?,
            files_changed: row.get("files_changed")?,
            files_missing: row.get("files_missing")?,
            aborted_reason: row.get("aborted_reason")?,
        })
    }
}

/// Reads and writes over `library` and `scan_run`.
#[derive(Debug, Clone)]
pub struct LibraryRepo {
    pool: ReadPool,
}

const LIBRARY_COLUMNS: &str = "id, name, root_path, work_dir, trash_dir, exclude_globs_json,
     enabled, scan_parallelism, priority, min_mtime_age_s";

impl LibraryRepo {
    /// Bind to a read pool.
    pub fn new(pool: ReadPool) -> Self {
        Self { pool }
    }

    /// One library by id.
    pub fn get(&self, id: &str) -> Result<LibraryRecord, StoreError> {
        let c = self.pool.get()?;
        c.query_row(
            &format!("SELECT {LIBRARY_COLUMNS} FROM library WHERE id = ?1"),
            [id],
            LibraryRecord::from_row,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound {
                kind: "library",
                id: id.to_string(),
            },
            other => other.into(),
        })
    }

    /// Every enabled library, in dispatch priority order.
    pub fn list_enabled(&self) -> Result<Vec<LibraryRecord>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {LIBRARY_COLUMNS} FROM library WHERE enabled = 1
             ORDER BY priority DESC, id"
        ))?;
        let rows = stmt.query_map([], LibraryRecord::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The most recent scan run for a library, if any.
    pub fn last_scan_run(&self, library_id: &str) -> Result<Option<ScanRun>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(
            "SELECT id, library_id, mode, scan_generation, status, files_seen, files_new,
                    files_changed, files_missing, aborted_reason
             FROM scan_run WHERE library_id = ?1
             ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([library_id], ScanRun::from_row)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Insert or replace a library definition.
    pub fn upsert_op(rec: LibraryRecord) -> WriteOp {
        WriteOp::new(format!("library.upsert:{}", rec.id), move |c| {
            let now = now_unix();
            Ok(c.execute(
                "INSERT INTO library
                   (id, name, root_path, work_dir, trash_dir, exclude_globs_json, enabled,
                    scan_parallelism, priority, min_mtime_age_s, created_unix, updated_unix)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name,
                   root_path = excluded.root_path,
                   work_dir = excluded.work_dir,
                   trash_dir = excluded.trash_dir,
                   exclude_globs_json = excluded.exclude_globs_json,
                   enabled = excluded.enabled,
                   scan_parallelism = excluded.scan_parallelism,
                   priority = excluded.priority,
                   min_mtime_age_s = excluded.min_mtime_age_s,
                   updated_unix = excluded.updated_unix",
                params![
                    rec.id,
                    rec.name,
                    rec.root_path,
                    rec.work_dir,
                    rec.trash_dir,
                    rec.exclude_globs_json,
                    i64::from(rec.enabled),
                    rec.scan_parallelism,
                    rec.priority,
                    rec.min_mtime_age_s,
                    now,
                ],
            )? as u64)
        })
    }

    /// Open a scan run, allocating the next generation for the library.
    ///
    /// The generation is allocated inside the write op rather than read first
    /// and passed in: reading it through the pool and writing it through the
    /// writer are two different points in time, and two scans started together
    /// would otherwise share a generation and each mark the other's files
    /// missing.
    pub fn begin_scan_run_op(library_id: String, mode: String) -> WriteOp {
        WriteOp::new_with_id(format!("scan_run.begin:{library_id}"), move |c| {
            let generation: i64 = c.query_row(
                "SELECT COALESCE(MAX(scan_generation), 0) + 1 FROM scan_run WHERE library_id = ?1",
                [&library_id],
                |r| r.get(0),
            )?;
            let rows = c.execute(
                "INSERT INTO scan_run (library_id, mode, started_unix, status, scan_generation)
                 VALUES (?1, ?2, ?3, 'running', ?4)",
                params![library_id, mode, now_unix(), generation],
            )?;
            Ok((rows as u64, c.last_insert_rowid()))
        })
    }

    /// Record counters against an open run.
    pub fn update_scan_counts_op(
        run_id: i64,
        seen: i64,
        new: i64,
        changed: i64,
        missing: i64,
        probe_errors: i64,
    ) -> WriteOp {
        WriteOp::new(format!("scan_run.counts:{run_id}"), move |c| {
            Ok(c.execute(
                "UPDATE scan_run SET files_seen = ?2, files_new = ?3, files_changed = ?4,
                        files_missing = ?5, probe_errors = ?6
                 WHERE id = ?1",
                params![run_id, seen, new, changed, missing, probe_errors],
            )? as u64)
        })
    }

    /// Close a scan run.
    ///
    /// `aborted_reason` is `Some` exactly when the run gave up — most usefully
    /// when the mass-missing guard fired, where recording *why* is the whole
    /// point of not having silently marked thousands of files missing.
    pub fn finish_scan_run_op(
        run_id: i64,
        status: String,
        aborted_reason: Option<String>,
        duration_ms: i64,
    ) -> WriteOp {
        WriteOp::new(format!("scan_run.finish:{run_id}"), move |c| {
            Ok(c.execute(
                "UPDATE scan_run SET finished_unix = ?2, status = ?3, aborted_reason = ?4,
                        duration_ms = ?5
                 WHERE id = ?1",
                params![run_id, now_unix(), status, aborted_reason, duration_ms],
            )? as u64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests_support::fixture;
    use crate::writer::WriteLane;

    fn library(id: &str, enabled: bool, priority: i64) -> LibraryRecord {
        LibraryRecord {
            id: id.into(),
            name: id.into(),
            root_path: format!("/mnt/{id}"),
            work_dir: format!("/mnt/{id}/work"),
            trash_dir: format!("/mnt/{id}/trash"),
            exclude_globs_json: "[]".into(),
            enabled,
            scan_parallelism: 4,
            priority,
            min_mtime_age_s: 300,
        }
    }

    #[test]
    fn a_library_round_trips_through_the_store() {
        let f = fixture();
        let want = library("tv", true, 5);
        f.writer
            .submit_blocking(WriteLane::Normal, LibraryRepo::upsert_op(want.clone()))
            .unwrap();
        let repo = LibraryRepo::new(f.pool.clone());
        assert_eq!(repo.get("tv").unwrap(), want);
    }

    #[test]
    fn upsert_updates_rather_than_duplicating() {
        let f = fixture();
        let repo = LibraryRepo::new(f.pool.clone());
        f.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(library("tv", true, 1)),
            )
            .unwrap();
        let mut changed = library("tv", true, 1);
        changed.min_mtime_age_s = 900;
        f.writer
            .submit_blocking(WriteLane::Normal, LibraryRepo::upsert_op(changed))
            .unwrap();
        assert_eq!(repo.list_enabled().unwrap().len(), 1);
        assert_eq!(repo.get("tv").unwrap().min_mtime_age_s, 900);
    }

    #[test]
    fn disabled_libraries_are_not_listed_and_priority_orders_the_rest() {
        let f = fixture();
        for lib in [
            library("anime", true, 1),
            library("tv", true, 9),
            library("old", false, 100),
        ] {
            f.writer
                .submit_blocking(WriteLane::Normal, LibraryRepo::upsert_op(lib))
                .unwrap();
        }
        let repo = LibraryRepo::new(f.pool.clone());
        let ids: Vec<_> = repo
            .list_enabled()
            .unwrap()
            .into_iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids, vec!["tv", "anime"]);
    }

    #[test]
    fn a_missing_library_reports_not_found_rather_than_an_sqlite_error() {
        let f = fixture();
        let repo = LibraryRepo::new(f.pool.clone());
        let e = repo.get("nope").unwrap_err();
        assert!(
            matches!(
                e,
                StoreError::NotFound {
                    kind: "library",
                    ..
                }
            ),
            "{e:?}"
        );
    }

    /// Generations must advance. Two runs sharing one would each conclude the
    /// other's files were missing.
    #[test]
    fn each_scan_run_gets_a_fresh_generation() {
        let f = fixture();
        f.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(library("tv", true, 0)),
            )
            .unwrap();
        let first = f
            .writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::begin_scan_run_op("tv".into(), "full".into()),
            )
            .unwrap();
        let second = f
            .writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::begin_scan_run_op("tv".into(), "full".into()),
            )
            .unwrap();
        assert_ne!(first.last_id, second.last_id);

        let repo = LibraryRepo::new(f.pool.clone());
        let last = repo.last_scan_run("tv").unwrap().unwrap();
        assert_eq!(last.scan_generation, 2);
        assert_eq!(last.status, "running");
    }

    /// An abort must say why. A run that stopped early and looks like a clean
    /// one is how a mass-missing event becomes invisible.
    #[test]
    fn an_aborted_run_records_its_reason() {
        let f = fixture();
        f.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(library("tv", true, 0)),
            )
            .unwrap();
        let run = f
            .writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::begin_scan_run_op("tv".into(), "full".into()),
            )
            .unwrap()
            .last_id
            .unwrap();
        f.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::update_scan_counts_op(run, 10, 3, 1, 9000, 0),
            )
            .unwrap();
        f.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::finish_scan_run_op(
                    run,
                    "aborted".into(),
                    Some("mass-missing guard: 9000 of 10 files".into()),
                    120,
                ),
            )
            .unwrap();

        let repo = LibraryRepo::new(f.pool.clone());
        let got = repo.last_scan_run("tv").unwrap().unwrap();
        assert_eq!(got.status, "aborted");
        assert_eq!(got.files_missing, 9000);
        assert!(got.aborted_reason.unwrap().contains("mass-missing"));
    }

    #[test]
    fn a_library_with_no_runs_reports_none() {
        let f = fixture();
        f.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(library("tv", true, 0)),
            )
            .unwrap();
        let repo = LibraryRepo::new(f.pool.clone());
        assert!(repo.last_scan_run("tv").unwrap().is_none());
    }
}
