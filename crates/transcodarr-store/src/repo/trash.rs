// file: crates/transcodarr-store/src/repo/trash.rs
// version: 1.0.0
// guid: 0f47cb92-3d86-4e15-a970-52c81b6e304f
// last-edited: 2026-08-03
//! Retained originals, and when they may be reaped.
//!
//! Originals are retained rather than deleted, so a bad decision is
//! recoverable. That only holds if reaping is conservative, which means two
//! rules that are easy to get wrong:
//!
//! - **A minimum grace period, always.** Pool pressure is a reason to reap
//!   *sooner*, never a reason to reap immediately. An operator who notices a
//!   bad transcode an hour later must still be able to undo it, and a pool that
//!   filled because of a runaway job would otherwise delete the very originals
//!   that job destroyed.
//! - **Reclaim is measured from ZFS accounting, never from file sizes.**
//!   Deleting a 40 GB original reclaims nothing while a snapshot still
//!   references its blocks. Summing `size_bytes` of reaped rows produces a
//!   number that looks like progress and is not — the pool stays exactly as
//!   full, and the operator is told 40 GB was freed.

use rusqlite::{Row, params};

use crate::StoreError;
use crate::db::now_unix;
use crate::pool::ReadPool;
use crate::writer::WriteOp;

/// The shortest a retained original is ever kept.
///
/// Six hours. Long enough to survive a night's unattended run being noticed the
/// next morning; short enough that pool pressure has somewhere to go.
pub const MIN_GRACE_SECONDS: i64 = 6 * 3600;

/// A retained original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    /// Row id.
    pub id: i64,
    /// The file it belonged to, while that row still exists.
    pub file_id: Option<i64>,
    /// The job that replaced it.
    pub job_id: Option<String>,
    /// Where it used to live.
    pub original_path: String,
    /// Where it is now.
    pub trash_path: String,
    /// Its size.
    pub size_bytes: i64,
    /// When it was retained.
    pub at_unix: i64,
    /// The earliest it may be reaped.
    pub purge_after_unix: i64,
    /// When it was put back, if it was.
    pub restored_unix: Option<i64>,
}

impl TrashEntry {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            file_id: row.get("file_id")?,
            job_id: row.get("job_id")?,
            original_path: row.get("original_path")?,
            trash_path: row.get("trash_path")?,
            size_bytes: row.get("size_bytes")?,
            at_unix: row.get("at_unix")?,
            purge_after_unix: row.get("purge_after_unix")?,
            restored_unix: row.get("restored_unix")?,
        })
    }
}

const TRASH_COLUMNS: &str = "
    id, file_id, job_id, original_path, trash_path, size_bytes, at_unix,
    purge_after_unix, restored_unix";

/// Reads and writes over `trash_entry`.
#[derive(Debug, Clone)]
pub struct TrashRepo {
    pool: ReadPool,
}

impl TrashRepo {
    /// Bind to a read pool.
    pub fn new(pool: ReadPool) -> Self {
        Self { pool }
    }

    /// Entries eligible for reaping right now, oldest first.
    ///
    /// The grace floor is applied in SQL rather than trusted from the stored
    /// `purge_after_unix`, so a row written with a bad retention — a
    /// misconfiguration, or an older binary — still cannot be reaped early.
    pub fn reapable(&self, limit: u32) -> Result<Vec<TrashEntry>, StoreError> {
        let now = now_unix();
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {TRASH_COLUMNS} FROM trash_entry
             WHERE restored_unix IS NULL
               AND purge_after_unix <= ?1
               AND at_unix <= ?2
             ORDER BY at_unix LIMIT ?3"
        ))?;
        let rows = stmt.query_map(
            params![now, now - MIN_GRACE_SECONDS, limit],
            TrashEntry::from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Everything still retained.
    pub fn retained(&self, limit: u32) -> Result<Vec<TrashEntry>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {TRASH_COLUMNS} FROM trash_entry
             WHERE restored_unix IS NULL ORDER BY at_unix DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit], TrashEntry::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The entry for a path, if one is retained.
    pub fn for_original_path(&self, path: &str) -> Result<Option<TrashEntry>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {TRASH_COLUMNS} FROM trash_entry
             WHERE original_path = ?1 AND restored_unix IS NULL
             ORDER BY at_unix DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query_map([path], TrashEntry::from_row)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// How much is retained, in rows and bytes.
    ///
    /// The byte figure is what is *held*, not what deleting it would free —
    /// see the module documentation. Reclaim comes from `pool_reclaim_sample`.
    pub fn retained_totals(&self) -> Result<(i64, i64), StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM trash_entry
             WHERE restored_unix IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Record a retained original.
    ///
    /// `retention_seconds` is clamped up to [`MIN_GRACE_SECONDS`]: a
    /// configuration asking for a shorter retention is asking for originals to
    /// be unrecoverable, and the floor is not negotiable.
    pub fn retain_op(
        file_id: Option<i64>,
        job_id: Option<String>,
        original_path: String,
        trash_path: String,
        size_bytes: i64,
        retention_seconds: i64,
    ) -> WriteOp {
        WriteOp::new_with_id(format!("trash.retain:{original_path}"), move |c| {
            let now = now_unix();
            let retention = retention_seconds.max(MIN_GRACE_SECONDS);
            let rows = c.execute(
                "INSERT INTO trash_entry
                   (file_id, job_id, original_path, trash_path, size_bytes, at_unix,
                    purge_after_unix)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    file_id,
                    job_id,
                    original_path,
                    trash_path,
                    size_bytes,
                    now,
                    now + retention,
                ],
            )?;
            Ok((rows as u64, c.last_insert_rowid()))
        })
    }

    /// Mark an entry as reaped.
    ///
    /// The row is deleted only after the file is gone, so a crash between the
    /// two leaves a row pointing at a missing file — recoverable — rather than
    /// a file nothing knows about.
    pub fn reap_op(id: i64) -> WriteOp {
        WriteOp::new(format!("trash.reap:{id}"), move |c| {
            Ok(c.execute("DELETE FROM trash_entry WHERE id = ?1", [id])? as u64)
        })
    }

    /// Mark an entry as restored.
    pub fn restore_op(id: i64) -> WriteOp {
        WriteOp::new(format!("trash.restore:{id}"), move |c| {
            Ok(c.execute(
                "UPDATE trash_entry SET restored_unix = ?2 WHERE id = ?1",
                params![id, now_unix()],
            )? as u64)
        })
    }

    /// Bring a retention forward under pool pressure.
    ///
    /// Never below the floor. Pressure is a reason to reap sooner, never a
    /// reason to reap immediately — and a pool that filled because of a runaway
    /// job would otherwise delete the very originals that job destroyed.
    pub fn hasten_op(seconds_earlier: i64) -> WriteOp {
        WriteOp::new("trash.hasten", move |c| {
            // The floor is measured from when the entry was retained, not from
            // now. Clamping to "a moment in the future" instead would make
            // hastening incapable of ever bringing anything due, which is a
            // guard that silently does nothing -- worse than no guard, because
            // it looks like one.
            Ok(c.execute(
                "UPDATE trash_entry
                 SET purge_after_unix =
                       MAX(at_unix + ?2, purge_after_unix - ?1)
                 WHERE restored_unix IS NULL",
                params![seconds_earlier.max(0), MIN_GRACE_SECONDS],
            )? as u64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests_support::{Fixture, fixture};

    fn seeded() -> (Fixture, TrashRepo) {
        let f = fixture();
        f.seed_library("tv");
        let repo = TrashRepo::new(f.pool.clone());
        (f, repo)
    }

    /// Backdate an entry so the grace floor can be exercised without sleeping.
    fn backdate(f: &Fixture, id: i64, seconds: i64) {
        f.write(crate::writer::WriteOp::new("test.backdate", move |c| {
            Ok(c.execute(
                "UPDATE trash_entry SET at_unix = at_unix - ?2,
                        purge_after_unix = purge_after_unix - ?2 WHERE id = ?1",
                params![id, seconds],
            )? as u64)
        }));
    }

    fn retain(f: &Fixture, path: &str, retention: i64) -> i64 {
        f.write(TrashRepo::retain_op(
            None,
            None,
            path.to_string(),
            format!("/t/{path}"),
            40_000_000_000,
            retention,
        ))
        .last_id
        .unwrap()
    }

    #[test]
    fn a_retained_original_is_findable_and_counted() {
        let (f, repo) = seeded();
        retain(&f, "a.mkv", 7 * 86400);
        let got = repo.for_original_path("a.mkv").unwrap().unwrap();
        assert_eq!(got.size_bytes, 40_000_000_000);
        assert_eq!(repo.retained_totals().unwrap(), (1, 40_000_000_000));
    }

    /// Nothing is reapable while the grace period holds, whatever the
    /// configured retention says.
    #[test]
    fn a_fresh_entry_is_never_reapable() {
        let (f, repo) = seeded();
        retain(&f, "a.mkv", 0);
        assert!(
            repo.reapable(10).unwrap().is_empty(),
            "the grace floor must hold even at zero retention"
        );
    }

    /// A configuration asking for a shorter retention is asking for originals
    /// to be unrecoverable. The floor is not negotiable.
    #[test]
    fn a_retention_below_the_floor_is_raised_to_it() {
        let (f, repo) = seeded();
        let id = retain(&f, "a.mkv", 60);
        let got = repo.for_original_path("a.mkv").unwrap().unwrap();
        assert_eq!(got.id, id);
        assert!(
            got.purge_after_unix - got.at_unix >= MIN_GRACE_SECONDS,
            "retention was {}",
            got.purge_after_unix - got.at_unix
        );
    }

    #[test]
    fn an_entry_past_its_retention_and_the_floor_is_reapable() {
        let (f, repo) = seeded();
        let id = retain(&f, "a.mkv", MIN_GRACE_SECONDS);
        backdate(&f, id, MIN_GRACE_SECONDS + 60);

        let reapable = repo.reapable(10).unwrap();
        assert_eq!(reapable.len(), 1);
        assert_eq!(reapable[0].id, id);
    }

    #[test]
    fn reaping_removes_the_row() {
        let (f, repo) = seeded();
        let id = retain(&f, "a.mkv", MIN_GRACE_SECONDS);
        backdate(&f, id, MIN_GRACE_SECONDS + 60);
        f.write(TrashRepo::reap_op(id));
        assert!(repo.reapable(10).unwrap().is_empty());
        assert_eq!(repo.retained_totals().unwrap().0, 0);
    }

    /// A restored original is no longer retained and must never be reaped.
    #[test]
    fn a_restored_entry_leaves_the_reap_queue() {
        let (f, repo) = seeded();
        let id = retain(&f, "a.mkv", MIN_GRACE_SECONDS);
        backdate(&f, id, MIN_GRACE_SECONDS + 60);
        f.write(TrashRepo::restore_op(id));

        assert!(repo.reapable(10).unwrap().is_empty());
        assert!(repo.for_original_path("a.mkv").unwrap().is_none());
        assert_eq!(repo.retained_totals().unwrap().0, 0);
    }

    /// Pool pressure reaps sooner, never immediately. A pool that filled
    /// because of a runaway job would otherwise delete the very originals that
    /// job destroyed.
    #[test]
    fn hastening_under_pressure_never_reaps_below_the_floor() {
        let (f, repo) = seeded();
        retain(&f, "a.mkv", 30 * 86400);
        f.write(TrashRepo::hasten_op(365 * 86400));

        assert!(
            repo.reapable(10).unwrap().is_empty(),
            "a fresh entry must survive any amount of hastening"
        );
        let got = repo.for_original_path("a.mkv").unwrap().unwrap();
        assert!(
            got.purge_after_unix > now_unix(),
            "purge time must stay future"
        );
    }

    /// ...but hastening does move an old entry forward, which is the point.
    #[test]
    fn hastening_brings_an_old_entry_forward() {
        let (f, repo) = seeded();
        let id = retain(&f, "a.mkv", 30 * 86400);
        backdate(&f, id, MIN_GRACE_SECONDS + 60);
        assert!(repo.reapable(10).unwrap().is_empty(), "not yet due");

        f.write(TrashRepo::hasten_op(30 * 86400));
        assert_eq!(repo.reapable(10).unwrap().len(), 1, "now due");
    }

    #[test]
    fn the_reap_limit_binds() {
        let (f, repo) = seeded();
        for i in 0..5 {
            let id = retain(&f, &format!("f{i}.mkv"), MIN_GRACE_SECONDS);
            backdate(&f, id, MIN_GRACE_SECONDS + 60);
        }
        assert_eq!(repo.reapable(2).unwrap().len(), 2);
        assert_eq!(repo.retained(10).unwrap().len(), 5);
    }

    #[test]
    fn an_unknown_path_has_no_entry() {
        let (_f, repo) = seeded();
        assert!(repo.for_original_path("nope.mkv").unwrap().is_none());
    }
}
