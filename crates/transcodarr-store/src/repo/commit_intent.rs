// file: crates/transcodarr-store/src/repo/commit_intent.rs
// version: 1.1.0
// guid: 6e10a4d3-92c7-4b58-81f0-3a5d76e29b41
// last-edited: 2026-08-04
//! The server-side commit ledger.
//!
//! The agent's `IntentJournal` survives a crash of the *agent*. This table
//! survives a crash of the connection: without it, a `JobResult` lost in flight
//! after a successful replace makes the next attempt re-encode a file that has
//! already been replaced — reading the new file as though it were the original.
//!
//! `idx_commit_intent_live` makes two live intents on one final path
//! structurally impossible rather than merely unlikely. Two agents mid-replace
//! on the same path is not a race to be detected and logged; it is an insert
//! that fails.

use rusqlite::{OptionalExtension, Row, params};

use crate::StoreError;
use crate::db::now_unix;
use crate::pool::ReadPool;
use crate::writer::WriteOp;

/// A live or resolved commit intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIntent {
    /// Intent identifier.
    pub id: String,
    /// Which job.
    pub job_id: String,
    /// Which attempt.
    pub attempt: i64,
    /// The agent holding it.
    pub agent_id: String,
    /// That agent's instance identity.
    pub agent_uid: String,
    /// Guards against a resurrected agent acting on a revoked grant.
    pub fencing_epoch: i64,
    /// The source being replaced.
    pub source_path: String,
    /// Where the output is staged.
    pub temp_path: String,
    /// Where it is going.
    pub final_path: String,
    /// How far the ritual got: `Granted`, `Retired` or `Installed`.
    pub phase: String,
    /// `live` or `resolved`.
    pub state: String,
    /// How it ended, once it has.
    pub resolution: Option<String>,
}

const INTENT_COLUMNS: &str = "
    id, job_id, attempt, agent_id, agent_uid, fencing_epoch, source_path, temp_path,
    final_path, phase, state, resolution";

impl CommitIntent {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            job_id: row.get("job_id")?,
            attempt: row.get("attempt")?,
            agent_id: row.get("agent_id")?,
            agent_uid: row.get("agent_uid")?,
            fencing_epoch: row.get("fencing_epoch")?,
            source_path: row.get("source_path")?,
            temp_path: row.get("temp_path")?,
            final_path: row.get("final_path")?,
            phase: row.get("phase")?,
            state: row.get("state")?,
            resolution: row.get("resolution")?,
        })
    }
}

/// What an agent needs granted before it may install.
#[derive(Debug, Clone)]
pub struct NewIntent {
    /// Intent identifier.
    pub id: String,
    /// Which job.
    pub job_id: String,
    /// Which attempt.
    pub attempt: i64,
    /// The agent asking.
    pub agent_id: String,
    /// That agent's instance identity.
    pub agent_uid: String,
    /// The grant's fencing epoch.
    pub fencing_epoch: i64,
    /// The source being replaced.
    pub source_path: String,
    /// Where the output is staged.
    pub temp_path: String,
    /// Where it is going.
    pub final_path: String,
    /// The facts signature the job was planned against.
    pub expected_content_sig: String,
}

/// Reads and writes over `commit_intent`.
#[derive(Debug, Clone)]
pub struct CommitIntentRepo {
    pool: ReadPool,
}

impl CommitIntentRepo {
    /// Bind to a read pool.
    pub fn new(pool: ReadPool) -> Self {
        Self { pool }
    }

    /// One intent by id.
    pub fn get(&self, id: &str) -> Result<Option<CommitIntent>, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            &format!("SELECT {INTENT_COLUMNS} FROM commit_intent WHERE id = ?1"),
            [id],
            CommitIntent::from_row,
        )
        .optional()?)
    }

    /// The live intent for a job, if there is one.
    ///
    /// Distinct from [`CommitIntentRepo::get`], which takes the *intent* id.
    /// The session only ever knows a job id — an agent asking permission names
    /// the job it is working on, not a ledger row it has never seen — and
    /// passing one to the other silently answers "no intent" for every job,
    /// which reads as a correct refusal right up until a legitimate commit is
    /// refused too.
    pub fn live_for_job(&self, job_id: &str) -> Result<Option<CommitIntent>, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            &format!(
                "SELECT {INTENT_COLUMNS} FROM commit_intent
                 WHERE job_id = ?1 AND state = 'live'"
            ),
            [job_id],
            CommitIntent::from_row,
        )
        .optional()?)
    }

    /// The live intent on a final path, if there is one.
    pub fn live_for_path(&self, final_path: &str) -> Result<Option<CommitIntent>, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            &format!(
                "SELECT {INTENT_COLUMNS} FROM commit_intent
                 WHERE final_path = ?1 AND state = 'live'"
            ),
            [final_path],
            CommitIntent::from_row,
        )
        .optional()?)
    }

    /// Every intent still live.
    ///
    /// What the reconciler sweeps: a live intent whose agent has gone away is
    /// the ambiguity the whole ledger exists to make visible.
    pub fn live(&self) -> Result<Vec<CommitIntent>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {INTENT_COLUMNS} FROM commit_intent WHERE state = 'live'
             ORDER BY created_unix_ms"
        ))?;
        let rows = stmt.query_map([], CommitIntent::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Grant an intent.
    ///
    /// Fails if another live intent already holds the final path — that is
    /// `idx_commit_intent_live` doing its job, and a failed insert is the
    /// correct answer rather than something to detect afterwards.
    pub fn grant_op(intent: NewIntent) -> WriteOp {
        WriteOp::new(format!("commit_intent.grant:{}", intent.id), move |c| {
            let now = now_unix() * 1000;
            Ok(c.execute(
                "INSERT INTO commit_intent
                   (id, job_id, attempt, agent_id, agent_uid, fencing_epoch, source_path,
                    temp_path, final_path, expected_content_sig, phase, state,
                    created_unix_ms, updated_unix_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'Granted','live',?11,?11)",
                params![
                    intent.id,
                    intent.job_id,
                    intent.attempt,
                    intent.agent_id,
                    intent.agent_uid,
                    intent.fencing_epoch,
                    intent.source_path,
                    intent.temp_path,
                    intent.final_path,
                    intent.expected_content_sig,
                    now,
                ],
            )? as u64)
        })
    }

    /// Advance an intent's phase as the agent reports progress.
    pub fn advance_op(id: String, phase: String) -> WriteOp {
        WriteOp::new(format!("commit_intent.advance:{id}"), move |c| {
            Ok(c.execute(
                "UPDATE commit_intent SET phase = ?2, updated_unix_ms = ?3
                 WHERE id = ?1 AND state = 'live'",
                params![id, phase, now_unix() * 1000],
            )? as u64)
        })
    }

    /// Resolve an intent, freeing the final path for a future one.
    ///
    /// Rows are never deleted here: they are the audit trail for what happened
    /// to a file, retained long after the job itself is pruned.
    pub fn resolve_op(id: String, resolution: String) -> WriteOp {
        WriteOp::new(format!("commit_intent.resolve:{id}"), move |c| {
            let now = now_unix() * 1000;
            Ok(c.execute(
                "UPDATE commit_intent
                 SET state = 'resolved', resolution = ?2, resolved_unix_ms = ?3,
                     updated_unix_ms = ?3
                 WHERE id = ?1",
                params![id, resolution, now],
            )? as u64)
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

    fn seeded() -> (Fixture, CommitIntentRepo) {
        let f = fixture();
        f.seed_library("tv");
        let file_id = f
            .write(FileRepo::upsert_op(FileUpsert {
                library_id: "tv".into(),
                canonical_path: "/mnt/tv/a.mkv".into(),
                path_hash: "h-a".into(),
                size_bytes: 100,
                mtime_unix: 10,
                mtime_ns: 0,
                inode: Some(1),
                dev: Some(1),
                nlink: 1,
                scan_generation: 1,
            }))
            .last_id
            .unwrap();
        f.write(JobRepo::create_op(NewJob {
            id: "job-1".into(),
            file_id,
            library_id: "tv".into(),
            class: JobClass::Audio,
            size_bucket: SizeBucket::Small,
            requirements_json: "[]".into(),
            requirements_bucket_key: "k".into(),
            expected_content_sig: "sig".into(),
            rules_version: "v1".into(),
            priority: 0,
            parent_job_id: None,
        }));
        let repo = CommitIntentRepo::new(f.pool.clone());
        (f, repo)
    }

    fn intent(id: &str, path: &str) -> NewIntent {
        NewIntent {
            id: id.into(),
            job_id: "job-1".into(),
            attempt: 0,
            agent_id: "agent-a".into(),
            agent_uid: "uid-a".into(),
            fencing_epoch: 1,
            source_path: path.into(),
            temp_path: "/w/tmp.mkv".into(),
            final_path: path.into(),
            expected_content_sig: "sig".into(),
        }
    }

    #[test]
    fn a_granted_intent_is_live_and_findable_by_path() {
        let (f, repo) = seeded();
        f.write(CommitIntentRepo::grant_op(intent("i1", "/mnt/tv/a.mkv")));

        let got = repo.live_for_path("/mnt/tv/a.mkv").unwrap().unwrap();
        assert_eq!(got.id, "i1");
        assert_eq!(got.phase, "Granted");
        assert_eq!(got.state, "live");
        assert_eq!(repo.live().unwrap().len(), 1);
    }

    /// Two agents mid-replace on one path is not a race to detect and log; it
    /// is an insert that fails.
    #[test]
    fn a_second_live_intent_on_one_path_is_structurally_impossible() {
        let (f, repo) = seeded();
        f.write(CommitIntentRepo::grant_op(intent("i1", "/mnt/tv/a.mkv")));

        let e = f
            .try_write(CommitIntentRepo::grant_op(intent("i2", "/mnt/tv/a.mkv")))
            .unwrap_err();
        assert!(matches!(e, StoreError::Sqlite(_)), "{e:?}");
        assert_eq!(repo.live().unwrap().len(), 1);
    }

    /// ...but once resolved, the path is free again. Otherwise a retry could
    /// never install.
    #[test]
    fn resolving_frees_the_path_for_a_later_intent() {
        let (f, repo) = seeded();
        f.write(CommitIntentRepo::grant_op(intent("i1", "/mnt/tv/a.mkv")));
        f.write(CommitIntentRepo::resolve_op(
            "i1".into(),
            "installed".into(),
        ));

        assert!(repo.live_for_path("/mnt/tv/a.mkv").unwrap().is_none());
        f.write(CommitIntentRepo::grant_op(intent("i2", "/mnt/tv/a.mkv")));
        assert_eq!(
            repo.live_for_path("/mnt/tv/a.mkv").unwrap().unwrap().id,
            "i2"
        );
    }

    /// The row is the audit trail for what happened to a file, retained long
    /// after the job is pruned. Resolving must not delete it.
    #[test]
    fn a_resolved_intent_is_retained_not_deleted() {
        let (f, repo) = seeded();
        f.write(CommitIntentRepo::grant_op(intent("i1", "/mnt/tv/a.mkv")));
        f.write(CommitIntentRepo::resolve_op(
            "i1".into(),
            "installed".into(),
        ));

        let got = repo.get("i1").unwrap().unwrap();
        assert_eq!(got.state, "resolved");
        assert_eq!(got.resolution.as_deref(), Some("installed"));
    }

    #[test]
    fn phases_advance_while_the_intent_is_live() {
        let (f, repo) = seeded();
        f.write(CommitIntentRepo::grant_op(intent("i1", "/mnt/tv/a.mkv")));
        for phase in ["Retired", "Installed"] {
            f.write(CommitIntentRepo::advance_op("i1".into(), phase.into()));
            assert_eq!(repo.get("i1").unwrap().unwrap().phase, phase);
        }
    }

    /// A resolved intent is history. Advancing it would rewrite the record of
    /// what actually happened.
    #[test]
    fn a_resolved_intent_cannot_be_advanced() {
        let (f, repo) = seeded();
        f.write(CommitIntentRepo::grant_op(intent("i1", "/mnt/tv/a.mkv")));
        f.write(CommitIntentRepo::resolve_op(
            "i1".into(),
            "installed".into(),
        ));

        let ack = f.write(CommitIntentRepo::advance_op("i1".into(), "Retired".into()));
        assert_eq!(ack.rows, 0, "no live row to advance");
        assert_eq!(repo.get("i1").unwrap().unwrap().phase, "Granted");
    }

    /// A live intent whose agent has gone away is the ambiguity the ledger
    /// exists to surface; the reconciler sweeps exactly this list.
    #[test]
    fn live_lists_only_unresolved_intents() {
        let (f, repo) = seeded();
        f.write(CommitIntentRepo::grant_op(intent("i1", "/mnt/tv/a.mkv")));
        f.write(CommitIntentRepo::resolve_op(
            "i1".into(),
            "installed".into(),
        ));
        f.write(CommitIntentRepo::grant_op(intent("i2", "/mnt/tv/a.mkv")));

        let live = repo.live().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, "i2");
    }

    #[test]
    fn an_unknown_intent_is_none() {
        let (_f, repo) = seeded();
        assert!(repo.get("nope").unwrap().is_none());
    }
}
