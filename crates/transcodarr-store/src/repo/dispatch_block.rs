// file: crates/transcodarr-store/src/repo/dispatch_block.rs
// version: 1.1.0
// guid: 4c17e5a0-93b6-42d8-8e14-70b9d2f36a58
// last-edited: 2026-08-16
//! Why each queued job did not dispatch last round.
//!
//! Without this table, "nothing is running and I do not know why" is an
//! unanswerable question — the dispatcher's reasoning is gone the moment its
//! loop ends. One row per job, overwritten each round, so the answer is always
//! about the most recent decision rather than an unbounded history nobody
//! reads.

use rusqlite::{OptionalExtension, Row, params};

use crate::StoreError;
use crate::db::now_unix;
use crate::pool::ReadPool;
use crate::writer::WriteOp;

/// The reason one job stayed put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchBlock {
    /// Which job.
    pub job_id: String,
    /// When the dispatcher last considered it.
    pub at_unix: i64,
    /// Which stage of dispatch rejected it — capability match, capacity,
    /// schedule, mount, or admission.
    pub blocking_stage: String,
    /// Stage-specific detail, as stored JSON.
    pub detail_json: Option<String>,
}

impl DispatchBlock {
    /// The detail in the form an operator reads it.
    ///
    /// `detail_json` holds an object so the shape can grow without a
    /// migration; the dispatcher writes `{"reason": "..."}`. A row whose
    /// detail is not that shape is handed back verbatim rather than dropped,
    /// because a reason an operator cannot see costs exactly as much as a
    /// reason nobody recorded — which is the failure this table was built to
    /// prevent.
    pub fn reason(&self) -> Option<String> {
        let raw = self.detail_json.as_deref()?;
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) => match v.get("reason").and_then(|r| r.as_str()) {
                Some(r) => Some(r.to_string()),
                None => Some(raw.to_string()),
            },
            Err(_) => Some(raw.to_string()),
        }
    }

    /// Wrap an operator-facing sentence as the stored detail.
    ///
    /// Paired with [`DispatchBlock::reason`] so callers never hand prose to a
    /// column named `_json`.
    pub fn detail_for(reason: &str) -> String {
        serde_json::json!({ "reason": reason }).to_string()
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            job_id: row.get("job_id")?,
            at_unix: row.get("at_unix")?,
            blocking_stage: row.get("blocking_stage")?,
            detail_json: row.get("detail_json")?,
        })
    }
}

/// Reads and writes over `dispatch_block`.
#[derive(Debug, Clone)]
pub struct DispatchBlockRepo {
    pool: ReadPool,
}

impl DispatchBlockRepo {
    /// Bind to a read pool.
    pub fn new(pool: ReadPool) -> Self {
        Self { pool }
    }

    /// Why one job is not dispatching.
    pub fn get(&self, job_id: &str) -> Result<Option<DispatchBlock>, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            "SELECT job_id, at_unix, blocking_stage, detail_json
             FROM dispatch_block WHERE job_id = ?1",
            [job_id],
            DispatchBlock::from_row,
        )
        .optional()?)
    }

    /// How many jobs each stage is holding up, most first.
    ///
    /// The shape an operator actually asks for: not "why is this job stuck" but
    /// "what is stopping the queue".
    pub fn count_by_stage(&self) -> Result<Vec<(String, i64)>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(
            "SELECT blocking_stage, COUNT(*) n FROM dispatch_block
             GROUP BY blocking_stage ORDER BY n DESC, blocking_stage",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record, or replace, why a job did not dispatch.
    pub fn upsert_op(
        job_id: String,
        blocking_stage: String,
        detail_json: Option<String>,
    ) -> WriteOp {
        WriteOp::new(format!("dispatch_block.upsert:{job_id}"), move |c| {
            Ok(c.execute(
                "INSERT INTO dispatch_block (job_id, at_unix, blocking_stage, detail_json)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(job_id) DO UPDATE SET
                   at_unix = excluded.at_unix,
                   blocking_stage = excluded.blocking_stage,
                   detail_json = excluded.detail_json",
                params![job_id, now_unix(), blocking_stage, detail_json],
            )? as u64)
        })
    }

    /// Clear the record for a job that has since dispatched.
    ///
    /// A stale block is worse than none: it says a job is stuck when it is
    /// running, which is the sort of thing an operator acts on.
    pub fn clear_op(job_id: String) -> WriteOp {
        WriteOp::new(format!("dispatch_block.clear:{job_id}"), move |c| {
            Ok(c.execute("DELETE FROM dispatch_block WHERE job_id = ?1", [&job_id])? as u64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::file::{FileRepo, FileUpsert};
    use crate::repo::job::{JobRepo, NewJob};
    use crate::repo::tests_support::{Fixture, fixture};
    use transcodarr_core::facts::SizeBucket;
    use transcodarr_core::job::JobClass;

    fn seeded(jobs: &[&str]) -> (Fixture, DispatchBlockRepo) {
        let f = fixture();
        f.seed_library("tv");
        for (i, id) in jobs.iter().enumerate() {
            let file_id = f
                .write(FileRepo::upsert_op(FileUpsert {
                    library_id: "tv".into(),
                    canonical_path: format!("/mnt/tv/{id}.mkv"),
                    path_hash: format!("h-{id}"),
                    size_bytes: 100,
                    mtime_unix: 10,
                    mtime_ns: 0,
                    inode: Some(i as i64),
                    dev: Some(1),
                    nlink: 1,
                    scan_generation: 1,
                }))
                .last_id
                .unwrap();
            f.write(JobRepo::create_op(NewJob {
                id: (*id).into(),
                file_id,
                library_id: "tv".into(),
                class: JobClass::Audio,
                size_bucket: SizeBucket::Small,
                requirements_json: "[]".into(),
                requirements_bucket_key: "audio/small".into(),
                expected_content_sig: "sig".into(),
                rules_version: "v1".into(),
                priority: 0,
                parent_job_id: None,
            }));
        }
        let repo = DispatchBlockRepo::new(f.pool.clone());
        (f, repo)
    }

    #[test]
    fn a_block_records_which_stage_refused_the_job() {
        let (f, repo) = seeded(&["j1"]);
        f.write(DispatchBlockRepo::upsert_op(
            "j1".into(),
            "capability".into(),
            Some(r#"{"unmet":"nvenc_hevc_10bit"}"#.into()),
        ));
        let got = repo.get("j1").unwrap().unwrap();
        assert_eq!(got.blocking_stage, "capability");
        assert!(got.detail_json.unwrap().contains("nvenc_hevc_10bit"));
    }

    /// One row per job, overwritten each round. An unbounded history nobody
    /// reads is not the same as an answer to "why is this stuck now".
    #[test]
    fn a_later_round_replaces_the_earlier_reason() {
        let (f, repo) = seeded(&["j1"]);
        f.write(DispatchBlockRepo::upsert_op(
            "j1".into(),
            "capability".into(),
            None,
        ));
        f.write(DispatchBlockRepo::upsert_op(
            "j1".into(),
            "capacity".into(),
            None,
        ));
        assert_eq!(repo.get("j1").unwrap().unwrap().blocking_stage, "capacity");
        assert_eq!(repo.count_by_stage().unwrap().len(), 1);
    }

    /// The shape an operator actually asks for: not "why is this job stuck" but
    /// "what is stopping the queue".
    #[test]
    fn stages_are_counted_most_blocking_first() {
        let (f, repo) = seeded(&["j1", "j2", "j3"]);
        for (job, stage) in [("j1", "capacity"), ("j2", "capacity"), ("j3", "capability")] {
            f.write(DispatchBlockRepo::upsert_op(job.into(), stage.into(), None));
        }
        let counts = repo.count_by_stage().unwrap();
        assert_eq!(counts[0], ("capacity".to_string(), 2));
        assert_eq!(counts[1], ("capability".to_string(), 1));
    }

    /// A stale block says a job is stuck when it is running — the sort of thing
    /// an operator acts on.
    #[test]
    fn clearing_a_block_removes_it() {
        let (f, repo) = seeded(&["j1"]);
        f.write(DispatchBlockRepo::upsert_op(
            "j1".into(),
            "capacity".into(),
            None,
        ));
        f.write(DispatchBlockRepo::clear_op("j1".into()));
        assert!(repo.get("j1").unwrap().is_none());
        assert!(repo.count_by_stage().unwrap().is_empty());
    }

    #[test]
    fn a_reason_survives_the_round_trip_through_the_column() {
        let (f, repo) = seeded(&["j1"]);
        let prose = "no enabled, commit-eligible agent satisfies decoder(h264/High/Eight, nvdec)";
        f.write(DispatchBlockRepo::upsert_op(
            "j1".into(),
            "capability".into(),
            Some(DispatchBlock::detail_for(prose)),
        ));
        assert_eq!(
            repo.get("j1").unwrap().unwrap().reason().as_deref(),
            Some(prose)
        );
    }

    /// Rows written before the `{"reason": ...}` convention, or by hand at a
    /// `sqlite3` prompt, still have to render. A detail an operator cannot see
    /// costs as much as one nobody recorded.
    #[test]
    fn a_detail_that_is_not_the_expected_shape_is_shown_verbatim() {
        let (f, repo) = seeded(&["j1", "j2"]);
        f.write(DispatchBlockRepo::upsert_op(
            "j1".into(),
            "capability".into(),
            Some("plain prose, not JSON at all".into()),
        ));
        f.write(DispatchBlockRepo::upsert_op(
            "j2".into(),
            "capability".into(),
            Some(r#"{"unmet":"nvenc_hevc_10bit"}"#.into()),
        ));
        assert_eq!(
            repo.get("j1").unwrap().unwrap().reason().as_deref(),
            Some("plain prose, not JSON at all")
        );
        assert_eq!(
            repo.get("j2").unwrap().unwrap().reason().as_deref(),
            Some(r#"{"unmet":"nvenc_hevc_10bit"}"#)
        );
    }

    #[test]
    fn an_unblocked_job_has_no_row() {
        let (_f, repo) = seeded(&["j1"]);
        assert!(repo.get("j1").unwrap().is_none());
    }

    /// The row is scoped to the job's lifetime by a cascading foreign key, so a
    /// deleted job cannot leave an orphan reason behind.
    #[test]
    fn a_block_for_an_unknown_job_is_refused() {
        let (f, _repo) = seeded(&[]);
        let e = f
            .try_write(DispatchBlockRepo::upsert_op(
                "ghost".into(),
                "capacity".into(),
                None,
            ))
            .unwrap_err();
        assert!(matches!(e, StoreError::Sqlite(_)), "{e:?}");
    }
}
