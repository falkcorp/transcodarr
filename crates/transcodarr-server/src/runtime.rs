// file: crates/transcodarr-server/src/runtime.rs
// version: 1.1.0
// guid: b5c1e08d-7f34-42a6-9013-8ae62d5f71bc
// last-edited: 2026-08-18
//! Opening the store, and the operator-facing surface over it.
//!
//! This module exists to settle a layering question the store's own
//! documentation raised: `transcodarr-store` is meant to be linked by
//! `transcodarr-server` and nothing else, but `admin explain` and friends are
//! CLI commands. Rather than make the CLI a second consumer of the database —
//! at which point two crates would need to agree about connection lifetimes,
//! pragmas and the single-writer rule — the CLI calls in here.
//!
//! The practical consequence is that no SQL, no `rusqlite` type and no
//! repository ever appears in `transcodarr-cli`.

use std::path::Path;
use std::sync::Arc;

use transcodarr_core::job::JobState;
use transcodarr_store::repo::{JobRepo, LibraryRecord, LibraryRepo};
use transcodarr_store::{Db, ReadPool, WriteLane, Writer};

use crate::ServerError;

/// How many read connections an operator command opens.
const CLI_READ_POOL: u32 = 8;

/// An open store: the single writer, and a pool of readers over it.
///
/// The writer owns the only write connection and the read pool needs it alive
/// for the WAL `-shm` file, so the two are created and dropped together rather
/// than handed out separately.
pub struct Runtime {
    pool: ReadPool,
    writer: Arc<Writer>,
}

impl Runtime {
    /// Open (creating and migrating if needed) the database at `path`.
    ///
    /// Runs the durability probe: a database on storage too slow to fsync makes
    /// the single writer the pacing constraint for the whole system, and it is
    /// better to say so at startup than to be mystified by throughput later.
    pub fn open(path: &Path) -> Result<Self, ServerError> {
        let db = Db::open(path)?;
        let pool = ReadPool::open(path, CLI_READ_POOL)?;
        Ok(Self {
            pool,
            writer: Arc::new(Writer::start(db)),
        })
    }

    /// Open without the durability probe. For tests and throwaway databases.
    pub fn open_unchecked(path: &Path) -> Result<Self, ServerError> {
        let db = Db::open_unchecked(path)?;
        let pool = ReadPool::open(path, CLI_READ_POOL)?;
        Ok(Self {
            pool,
            writer: Arc::new(Writer::start(db)),
        })
    }

    /// The read pool.
    pub fn pool(&self) -> &ReadPool {
        &self.pool
    }

    /// The single writer.
    pub fn writer(&self) -> &Arc<Writer> {
        &self.writer
    }

    /// Every enabled library, or just the one named.
    ///
    /// An unknown id is an error rather than an empty list: "no libraries
    /// matched" and "that library does not exist" send an operator to different
    /// places.
    pub fn libraries(&self, only: Option<&str>) -> Result<Vec<LibraryRecord>, ServerError> {
        let repo = LibraryRepo::new(self.pool.clone());
        match only {
            Some(id) => Ok(vec![repo.get(id)?]),
            None => Ok(repo.list_enabled()?),
        }
    }

    /// Register or update a library.
    #[allow(clippy::too_many_arguments)]
    pub fn add_library(
        &self,
        id: &str,
        name: &str,
        root_path: &str,
        work_dir: &str,
        trash_dir: &str,
        min_mtime_age_s: i64,
    ) -> Result<(), ServerError> {
        self.writer.submit_blocking(
            WriteLane::Normal,
            LibraryRepo::upsert_op(LibraryRecord {
                id: id.to_string(),
                name: name.to_string(),
                root_path: root_path.to_string(),
                work_dir: work_dir.to_string(),
                trash_dir: trash_dir.to_string(),
                exclude_globs_json: "[]".into(),
                enabled: true,
                scan_parallelism: 4,
                priority: 0,
                min_mtime_age_s,
            }),
        )?;
        Ok(())
    }

    /// Clear every recorded decision in a library, so the next evaluation
    /// re-derives all of them.
    ///
    /// Clearing `eval_rules_version` is what puts files back into the working
    /// set — the evaluator has no notion of "again", only of "this decision
    /// predates the current policy". Stored probe facts are untouched, which is
    /// the point: this is the operation that must cost no filesystem I/O.
    pub fn reset_evaluations(&self, library_id: &str) -> Result<u64, ServerError> {
        let id = library_id.to_string();
        let ack = self.writer.submit_blocking(
            WriteLane::Normal,
            transcodarr_store::WriteOp::new(format!("runtime.reset_eval:{id}"), move |c| {
                Ok(c.execute(
                    "UPDATE file SET eval_rules_version = NULL WHERE library_id = ?1",
                    [&id],
                )? as u64)
            }),
        )?;
        Ok(ack.rows)
    }

    /// Cancel a job, at an operator's request.
    ///
    /// This is the escape hatch for a job that has become permanently
    /// unsatisfiable — most often one whose stored requirements no longer
    /// match anything the current code can emit, which nothing else can clear.
    ///
    /// ## Three things this deliberately does not do
    ///
    /// Each looks like it needs handling here and is already covered. Adding
    /// any of them would be a no-op that reads as though it were load-bearing.
    ///
    /// - **Release the capacity slot.** The ledger is rebuilt from the
    ///   database on every tick (`Orchestrator::tick` -> `rebuild_capacity`),
    ///   and `CapacityLedger::rebuild` skips any state where
    ///   `!occupies_slot`. A cancelled job stops holding its slot on the next
    ///   pass. Releasing it from here would in any case touch this process's
    ///   ledger, not the running server's.
    /// - **Resolve the commit intent.** `sweep_stranded_intents` resolves
    ///   live intents whose job is terminal and not `NeedsOperator`. A
    ///   cancelled job is exactly that, so the row is closed for us.
    /// - **Tell the agent.** `on_heartbeat` revokes any running job whose
    ///   state is not in `HELD_STATES`, and the agent sweeps its work area on
    ///   every exit path. So `force` needs no new protocol message — though
    ///   note the agent checks the revoke only after its encode finishes, so a
    ///   forced cancel stops the *install*, not the ffmpeg process.
    ///
    /// ## Why `Committing` is refused even under `force`
    ///
    /// It is the window between the commit ritual's two renames, which is the
    /// ambiguity `NeedsOperator` exists to record. Cancelling there races a
    /// rename on real files, and sweeping the intent afterwards would free a
    /// destination whose on-disk state nobody has determined — the next job
    /// for that file would install over it.
    ///
    /// Returns the state the job was cancelled from.
    pub fn cancel_job(
        &self,
        job_id: &str,
        reason: Option<&str>,
        force: bool,
    ) -> Result<JobState, ServerError> {
        let from = JobRepo::new(self.pool.clone()).get(job_id)?.state;

        let refuse = |hint: &str| {
            Err(ServerError::CancelRefused {
                job_id: job_id.to_string(),
                state: from.as_str().to_string(),
                hint: hint.to_string(),
            })
        };

        if from.is_terminal() {
            return refuse(
                "it has already finished; terminal rows are immutable, and a retry \
                 inserts a new job rather than reanimating this one",
            );
        }
        if from == JobState::Committing {
            return refuse(
                "the commit ritual is mid-rename -- wait for it to land, or for it \
                 to resolve as NeedsOperator if the outcome is ambiguous",
            );
        }
        if from.holds_capacity() && !force {
            return refuse("an agent is holding it; pass --force to cancel anyway");
        }

        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::transition_op(
                job_id.to_string(),
                from,
                JobState::Cancelled,
                // The code is what a machine reads back out of `terminal_reason`;
                // the operator's note is the detail on the event. Passing `None`
                // for the code would leave a terminal row with no recorded why.
                Some("operator_cancelled".to_string()),
                reason.map(str::to_string),
            ),
        )?;
        Ok(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use transcodarr_core::facts::SizeBucket;
    use transcodarr_core::job::JobClass;
    use transcodarr_store::repo::{FileUpsert, NewJob};

    /// A runtime with one library, one file and one job, left in `state`.
    ///
    /// The job is walked into `state` through `transition_op` rather than
    /// inserted there, so the fixture cannot assert a state the state machine
    /// would refuse. A harness that writes the row directly would happily set
    /// up a job in a shape production can never produce.
    fn with_job(state: JobState) -> (TempDir, Runtime, JobRepo) {
        let d = TempDir::new().unwrap();
        let rt = Runtime::open_unchecked(&d.path().join("t.db")).unwrap();
        rt.add_library("tv", "Television", "/mnt/tv", "/w", "/t", 300)
            .unwrap();

        let path = "/mnt/tv/show.mkv";
        let file_id = rt
            .writer
            .submit_blocking(
                WriteLane::Normal,
                transcodarr_store::repo::FileRepo::upsert_op(FileUpsert {
                    library_id: "tv".into(),
                    canonical_path: path.into(),
                    path_hash: transcodarr_core::stable_hash(path.as_bytes()),
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
            .expect("the file insert returns its row id");

        rt.writer
            .submit_blocking(
                WriteLane::Normal,
                JobRepo::create_op(NewJob {
                    id: "j1".into(),
                    file_id,
                    library_id: "tv".into(),
                    class: JobClass::VideoGpu,
                    size_bucket: SizeBucket::Large,
                    requirements_json: "[]".into(),
                    requirements_bucket_key: "k".into(),
                    expected_content_sig: "sig".into(),
                    rules_version: "v".into(),
                    priority: 0,
                    parent_job_id: None,
                }),
            )
            .unwrap();

        // Walk the legal path to whichever state the test wants.
        let route: &[JobState] = match state {
            JobState::Pending => &[],
            JobState::Blocked => &[JobState::Blocked],
            JobState::Eligible => &[JobState::Eligible],
            JobState::Retrying => &[
                JobState::Eligible,
                JobState::Assigned,
                JobState::Running,
                JobState::Retrying,
            ],
            JobState::Assigned => &[JobState::Eligible, JobState::Assigned],
            JobState::Running => &[JobState::Eligible, JobState::Assigned, JobState::Running],
            JobState::Verifying => &[
                JobState::Eligible,
                JobState::Assigned,
                JobState::Running,
                JobState::Verifying,
            ],
            JobState::Committing => &[
                JobState::Eligible,
                JobState::Assigned,
                JobState::Running,
                JobState::Verifying,
                JobState::Committing,
            ],
            JobState::Succeeded => &[
                JobState::Eligible,
                JobState::Assigned,
                JobState::Running,
                JobState::Verifying,
                JobState::Committing,
                JobState::Succeeded,
            ],
            other => panic!("no route to {other:?}"),
        };
        let mut from = JobState::Pending;
        for to in route {
            rt.writer
                .submit_blocking(
                    WriteLane::Normal,
                    JobRepo::transition_op("j1".into(), from, *to, None, None),
                )
                .unwrap();
            from = *to;
        }

        let jobs = JobRepo::new(rt.pool.clone());
        assert_eq!(jobs.get("j1").unwrap().state, state, "fixture setup");
        (d, rt, jobs)
    }

    #[test]
    fn a_queued_job_is_cancelled_and_records_both_the_code_and_the_operators_note() {
        for state in [
            JobState::Pending,
            JobState::Blocked,
            JobState::Eligible,
            JobState::Retrying,
        ] {
            let (_d, rt, jobs) = with_job(state);
            let from = rt
                .cancel_job("j1", Some("source is a duplicate"), false)
                .unwrap();
            assert_eq!(from, state);

            let job = jobs.get("j1").unwrap();
            assert_eq!(job.state, JobState::Cancelled, "from {state:?}");
            // The code is what a machine reads back; without it the row is a
            // terminal job with no recorded why.
            assert_eq!(job.terminal_reason.as_deref(), Some("operator_cancelled"));

            let last = jobs.events("j1").unwrap().pop().unwrap();
            assert_eq!(last.to_state, JobState::Cancelled);
            assert_eq!(last.detail.as_deref(), Some("source is a duplicate"));
        }
    }

    /// Terminal rows are immutable; a retry inserts a new job with
    /// `parent_job_id` rather than reanimating this one.
    #[test]
    fn a_finished_job_cannot_be_cancelled() {
        let (_d, rt, jobs) = with_job(JobState::Succeeded);
        let e = rt.cancel_job("j1", None, true).unwrap_err();
        assert!(matches!(e, ServerError::CancelRefused { .. }), "{e:?}");
        assert_eq!(jobs.get("j1").unwrap().state, JobState::Succeeded);
    }

    /// Deleting the `force` guard must fail this test.
    #[test]
    fn a_job_an_agent_is_holding_is_refused_without_force() {
        for state in [JobState::Assigned, JobState::Running, JobState::Verifying] {
            let (_d, rt, jobs) = with_job(state);
            let e = rt.cancel_job("j1", None, false).unwrap_err();
            assert!(
                matches!(e, ServerError::CancelRefused { .. }),
                "{state:?}: {e:?}"
            );
            assert_eq!(jobs.get("j1").unwrap().state, state);
        }
    }

    #[test]
    fn force_cancels_a_job_an_agent_is_holding() {
        for state in [JobState::Assigned, JobState::Running, JobState::Verifying] {
            let (_d, rt, jobs) = with_job(state);
            assert_eq!(rt.cancel_job("j1", None, true).unwrap(), state);
            assert_eq!(jobs.get("j1").unwrap().state, JobState::Cancelled);
        }
    }

    /// Deleting the `Committing` carve-out must fail this test.
    ///
    /// That state is the window between the commit ritual's two renames, which
    /// is exactly the ambiguity `NeedsOperator` exists to record. Cancelling
    /// there races a rename on real files and frees a destination whose
    /// on-disk state nobody has determined.
    #[test]
    fn a_committing_job_is_refused_even_under_force() {
        let (_d, rt, jobs) = with_job(JobState::Committing);
        let e = rt.cancel_job("j1", None, true).unwrap_err();
        assert!(matches!(e, ServerError::CancelRefused { .. }), "{e:?}");
        assert_eq!(jobs.get("j1").unwrap().state, JobState::Committing);
    }

    #[test]
    fn an_unknown_job_is_an_error_rather_than_a_silent_success() {
        let (_d, rt, _jobs) = with_job(JobState::Pending);
        assert!(rt.cancel_job("nope", None, true).is_err());
    }

    #[test]
    fn a_library_can_be_registered_and_listed_without_touching_the_store_directly() {
        let d = TempDir::new().unwrap();
        let rt = Runtime::open_unchecked(&d.path().join("t.db")).unwrap();
        rt.add_library("tv", "Television", "/mnt/tv", "/w", "/t", 300)
            .unwrap();

        let all = rt.libraries(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Television");
        assert_eq!(rt.libraries(Some("tv")).unwrap().len(), 1);
    }

    /// "No libraries matched" and "that library does not exist" send an
    /// operator to different places.
    #[test]
    fn an_unknown_library_is_an_error_not_an_empty_list() {
        let d = TempDir::new().unwrap();
        let rt = Runtime::open_unchecked(&d.path().join("t.db")).unwrap();
        assert!(rt.libraries(Some("nope")).is_err());
        assert!(rt.libraries(None).unwrap().is_empty());
    }

    #[test]
    fn registering_the_same_id_updates_rather_than_duplicating() {
        let d = TempDir::new().unwrap();
        let rt = Runtime::open_unchecked(&d.path().join("t.db")).unwrap();
        rt.add_library("tv", "Old", "/mnt/tv", "/w", "/t", 300)
            .unwrap();
        rt.add_library("tv", "New", "/mnt/tv", "/w", "/t", 900)
            .unwrap();
        let all = rt.libraries(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "New");
        assert_eq!(all[0].min_mtime_age_s, 900);
    }
}
