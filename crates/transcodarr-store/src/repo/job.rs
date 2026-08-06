// file: crates/transcodarr-store/src/repo/job.rs
// version: 1.4.0
// guid: e0947b25-6c31-4fa8-b0d2-58e1a37c92f6
// last-edited: 2026-08-06
//! Jobs: creation, reads, and the compare-and-swap transition.

use rusqlite::{OptionalExtension, Row, params};
use transcodarr_core::facts::SizeBucket;
use transcodarr_core::job::{JobClass, JobState};

use crate::StoreError;
use crate::db::now_unix;
use crate::pool::ReadPool;
use crate::repo::parse_enum;
use crate::writer::WriteOp;

/// A job to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    /// Job identifier, assigned by the caller.
    pub id: String,
    /// The file this job operates on.
    pub file_id: i64,
    /// Owning library.
    pub library_id: String,
    /// What kind of work.
    pub class: JobClass,
    /// Size band.
    pub size_bucket: SizeBucket,
    /// What an agent must provide, as stored JSON.
    pub requirements_json: String,
    /// Precomputed key for eligibility matching. Deliberately excludes paths
    /// and byte thresholds, which are per-job admission checks instead — with
    /// them included the key space explodes and precomputing buys nothing.
    pub requirements_bucket_key: String,
    /// Signature of the facts this job was planned from. An agent aborts if the
    /// source no longer matches it.
    pub expected_content_sig: String,
    /// Rules version that produced it.
    pub rules_version: String,
    /// Dispatch priority.
    pub priority: i64,
    /// The job this one follows, for the two-stage audio-then-video flow.
    pub parent_job_id: Option<String>,
}

/// A stored job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// Job identifier.
    pub id: String,
    /// The file it operates on.
    pub file_id: i64,
    /// Owning library.
    pub library_id: String,
    /// What kind of work.
    pub class: JobClass,
    /// Size band.
    pub size_bucket: SizeBucket,
    /// Where it is in its life.
    pub state: JobState,
    /// Dispatch priority.
    pub priority: i64,
    /// Requirements, as stored JSON.
    pub requirements_json: String,
    /// Eligibility bucket key.
    pub requirements_bucket_key: String,
    /// Facts signature the plan was made against.
    pub expected_content_sig: String,
    /// Rules version that produced it.
    pub rules_version: String,
    /// Which attempt is current.
    pub attempt: i64,
    /// How many attempts are permitted.
    pub max_attempts: i64,
    /// The agent holding it, when one does.
    pub agent_id: Option<String>,
    /// Guards against a resurrected agent acting on a stale assignment.
    pub fencing_epoch: i64,
    /// The job this one follows.
    pub parent_job_id: Option<String>,
    /// Why it ended, when it has.
    pub terminal_reason: Option<String>,
}

const JOB_COLUMNS: &str = "
    id, file_id, library_id, class, size_bucket, state, priority, requirements_json,
    requirements_bucket_key, expected_content_sig, rules_version, attempt, max_attempts,
    agent_id, fencing_epoch, parent_job_id, terminal_reason";

impl JobRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Result<Self, StoreError>> {
        let class = match parse_enum(
            "job.class",
            &row.get::<_, String>("class")?,
            JobClass::parse,
        ) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let size_bucket = match parse_enum(
            "job.size_bucket",
            &row.get::<_, String>("size_bucket")?,
            SizeBucket::parse,
        ) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let state = match parse_enum(
            "job.state",
            &row.get::<_, String>("state")?,
            JobState::parse,
        ) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };

        Ok(Ok(Self {
            id: row.get("id")?,
            file_id: row.get("file_id")?,
            library_id: row.get("library_id")?,
            class,
            size_bucket,
            state,
            priority: row.get("priority")?,
            requirements_json: row.get("requirements_json")?,
            requirements_bucket_key: row.get("requirements_bucket_key")?,
            expected_content_sig: row.get("expected_content_sig")?,
            rules_version: row.get("rules_version")?,
            attempt: row.get("attempt")?,
            max_attempts: row.get("max_attempts")?,
            agent_id: row.get("agent_id")?,
            fencing_epoch: row.get("fencing_epoch")?,
            parent_job_id: row.get("parent_job_id")?,
            terminal_reason: row.get("terminal_reason")?,
        }))
    }
}

/// One entry in a job's append-only transition ledger.
///
/// Never updated and never deleted for a dead-lettered job — it is the record
/// of what actually happened, which is the only thing left to look at once the
/// job itself is terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEvent {
    /// When, in milliseconds.
    pub at_unix_ms: i64,
    /// The state left. `None` on the opening event, which has no prior state.
    pub from_state: Option<JobState>,
    /// The state entered.
    pub to_state: JobState,
    /// Which attempt was current.
    pub attempt: i64,
    /// Machine-readable reason, when there is one.
    pub reason_code: Option<String>,
    /// Operator-facing detail, when there is one.
    pub detail: Option<String>,
}

/// Reads and writes over `job`, `job_event` and `job_attempt`.
#[derive(Debug, Clone)]
pub struct JobRepo {
    pool: ReadPool,
}

impl JobRepo {
    /// Bind to a read pool.
    pub fn new(pool: ReadPool) -> Self {
        Self { pool }
    }

    /// One job by id.
    pub fn get(&self, id: &str) -> Result<JobRecord, StoreError> {
        let c = self.pool.get()?;
        let found = c
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM job WHERE id = ?1"),
                [id],
                JobRecord::from_row,
            )
            .optional()?;
        match found {
            Some(r) => r,
            None => Err(StoreError::NotFound {
                kind: "job",
                id: id.to_string(),
            }),
        }
    }

    /// The open job for a file, if there is one.
    ///
    /// There can be at most one — `idx_job_open_per_file` makes a second
    /// structurally impossible rather than merely unlikely.
    pub fn open_for_file(&self, file_id: i64) -> Result<Option<JobRecord>, StoreError> {
        let c = self.pool.get()?;
        let found = c
            .query_row(
                &format!(
                    "SELECT {JOB_COLUMNS} FROM job
                     WHERE file_id = ?1
                       AND state NOT IN
                         ('Succeeded','Failed','Cancelled','DeadLettered','NeedsOperator')"
                ),
                [file_id],
                JobRecord::from_row,
            )
            .optional()?;
        found.transpose()
    }

    /// Jobs in a given state, most urgent first.
    pub fn in_state(&self, state: JobState, limit: u32) -> Result<Vec<JobRecord>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM job WHERE state = ?1
             ORDER BY priority DESC, order_key, id LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![state.as_str(), limit], JobRecord::from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    /// How many jobs are in each state, for the queue summary.
    pub fn count_by_state(&self) -> Result<Vec<(JobState, i64)>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare("SELECT state, COUNT(*) FROM job GROUP BY state")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (raw, n) = row?;
            out.push((parse_enum("job.state", &raw, JobState::parse)?, n));
        }
        Ok(out)
    }

    /// Open jobs per state within one library, most numerous first.
    pub fn open_counts_for_library(
        &self,
        library_id: &str,
    ) -> Result<Vec<(JobState, i64)>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(
            "SELECT state, COUNT(*) n FROM job
             WHERE library_id = ?1
               AND state NOT IN
                 ('Succeeded','Failed','Cancelled','DeadLettered','NeedsOperator')
             GROUP BY state ORDER BY n DESC",
        )?;
        let rows = stmt.query_map([library_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (raw, n) = row?;
            out.push((parse_enum("job.state", &raw, JobState::parse)?, n));
        }
        Ok(out)
    }

    /// Create a job in `Pending`, with its opening `job_event`.
    pub fn create_op(job: NewJob) -> WriteOp {
        WriteOp::new(format!("job.create:{}", job.id), move |c| {
            let now = now_unix();
            let rows = c.execute(
                "INSERT INTO job
                   (id, file_id, library_id, class, size_bucket, state, priority,
                    requirements_json, requirements_bucket_key, expected_content_sig,
                    rules_version, parent_job_id, created_unix, updated_unix)
                 VALUES (?1,?2,?3,?4,?5,'Pending',?6,?7,?8,?9,?10,?11,?12,?12)",
                params![
                    job.id,
                    job.file_id,
                    job.library_id,
                    job.class.as_str(),
                    job.size_bucket.as_str(),
                    job.priority,
                    job.requirements_json,
                    job.requirements_bucket_key,
                    job.expected_content_sig,
                    job.rules_version,
                    job.parent_job_id,
                    now,
                ],
            )?;
            c.execute(
                "INSERT INTO job_event (job_id, at_unix_ms, from_state, to_state, reason_code)
                 VALUES (?1, ?2, NULL, 'Pending', 'created')",
                params![job.id, now * 1000],
            )?;
            Ok(rows as u64)
        })
    }

    /// Count another attempt.
    ///
    /// Separate from the transition because the two answer different questions,
    /// and conflating them gets one of them wrong: a job moves through
    /// `Retrying` on its way out of every failure, but only a failure that will
    /// actually be *retried* consumes an attempt. Bumping inside the transition
    /// would charge an attempt to a job on its way to being dead-lettered.
    ///
    /// The attempt number is also what makes a retry's commit intent distinct.
    /// `commit_intent.id` is `job:attempt`, so a re-dispatch that reused the
    /// number would collide with the previous attempt's row on the primary key
    /// and could never be placed at all.
    pub fn bump_attempt_op(job_id: String) -> WriteOp {
        WriteOp::new(format!("job.bump_attempt:{job_id}"), move |c| {
            Ok(c.execute(
                "UPDATE job SET attempt = attempt + 1, updated_unix = ?2 WHERE id = ?1",
                params![job_id, now_unix()],
            )? as u64)
        })
    }

    /// Place a job on an agent: the assignment and the transition, together.
    ///
    /// One op on purpose. A job that reached `Assigned` with no `agent_id`
    /// would be held by nobody and reclaimed by nothing — the reconciler sees
    /// no agent to miss, and the heartbeat check that revokes unrecognised work
    /// compares against a `NULL` that matches no agent. The writer's per-op
    /// `SAVEPOINT` means both land or neither does.
    ///
    /// The epoch is stamped here rather than read later because it is the whole
    /// fence: a commit arriving under a different one must be refused, and that
    /// comparison needs the epoch the job was *given out* under, not whatever
    /// the agent holds by the time it reports.
    pub fn assign_op(job_id: String, agent_id: String, fencing_epoch: i64) -> WriteOp {
        WriteOp::new(format!("job.assign:{job_id}"), move |c| {
            let now = now_unix();
            let rows = c.execute(
                "UPDATE job SET
                   state = 'Assigned',
                   agent_id = ?2,
                   fencing_epoch = ?3,
                   updated_unix = ?4
                 WHERE id = ?1 AND state = 'Eligible'",
                params![job_id, agent_id, fencing_epoch, now],
            )?;

            // Zero rows means it was not `Eligible` when the update ran —
            // another pass placed it, or the reconciler took it back. The
            // caller's decision was made against state that no longer holds.
            if rows == 0 {
                return Err(StoreError::TransitionRaceLost {
                    job_id: job_id.clone(),
                    expected: "Eligible".to_string(),
                });
            }

            c.execute(
                "INSERT INTO job_event
                   (job_id, at_unix_ms, from_state, to_state, attempt, reason_code, detail)
                 VALUES (?1, ?2, 'Eligible', 'Assigned',
                         (SELECT attempt FROM job WHERE id = ?1), 'dispatched', ?3)",
                params![
                    job_id,
                    now * 1000,
                    format!("to {agent_id} at epoch {fencing_epoch}")
                ],
            )?;
            Ok(rows as u64)
        })
    }

    /// Move a job from one state to another, or fail saying why.
    ///
    /// Two guards, and both are needed:
    ///
    /// - [`JobState::can_transition`] rejects edges the state machine forbids.
    ///   That is a domain error, not a silent no-op: a caller asking for
    ///   `Running -> Succeeded` has a bug, and returning `Ok` would hide it.
    /// - The `UPDATE` carries `AND state = ?expected` and requires exactly one
    ///   row. A `SELECT` followed by an `UPDATE` is not a compare-and-swap —
    ///   between the two, a heartbeat timeout can move the job, and the update
    ///   would then overwrite a decision made with better information.
    ///
    /// The `job_event` insert lives in the same op, so the ledger and the row
    /// can never disagree about what happened: the writer's per-op `SAVEPOINT`
    /// rolls back both or neither.
    pub fn transition_op(
        job_id: String,
        from: JobState,
        to: JobState,
        reason_code: Option<String>,
        detail: Option<String>,
    ) -> WriteOp {
        WriteOp::new(format!("job.transition:{job_id}"), move |c| {
            if !JobState::can_transition(from, to) {
                return Err(StoreError::IllegalTransition {
                    job_id: job_id.clone(),
                    from: from.as_str().to_string(),
                    to: to.as_str().to_string(),
                });
            }

            let now = now_unix();
            let rows = c.execute(
                "UPDATE job SET
                   state = ?3,
                   updated_unix = ?4,
                   started_unix = CASE WHEN ?3 = 'Running' AND started_unix IS NULL
                                       THEN ?4 ELSE started_unix END,
                   finished_unix = CASE WHEN ?3 IN
                     ('Succeeded','Failed','Cancelled','DeadLettered','NeedsOperator')
                     THEN ?4 ELSE finished_unix END,
                   terminal_reason = COALESCE(?5, terminal_reason)
                 WHERE id = ?1 AND state = ?2",
                params![job_id, from.as_str(), to.as_str(), now, reason_code],
            )?;

            // Zero rows means the job was not in `from` when the update ran.
            // Somebody else moved it first, and the caller's decision was made
            // against state that no longer holds — re-deciding is theirs.
            if rows == 0 {
                return Err(StoreError::TransitionRaceLost {
                    job_id: job_id.clone(),
                    expected: from.as_str().to_string(),
                });
            }

            c.execute(
                "INSERT INTO job_event
                   (job_id, at_unix_ms, from_state, to_state, attempt, reason_code, detail)
                 VALUES (?1, ?2, ?3, ?4, (SELECT attempt FROM job WHERE id = ?1), ?5, ?6)",
                params![
                    job_id,
                    now * 1000,
                    from.as_str(),
                    to.as_str(),
                    reason_code,
                    detail
                ],
            )?;
            Ok(rows as u64)
        })
    }

    /// The transition ledger for a job, oldest first.
    ///
    /// Returns parsed states rather than the stored spellings: a caller holding
    /// a `String` has to re-derive what it means, and the first thing it will
    /// reach for is a comparison against a literal that nothing keeps in step
    /// with the enum.
    pub fn events(&self, job_id: &str) -> Result<Vec<JobEvent>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(
            "SELECT at_unix_ms, from_state, to_state, attempt, reason_code, detail
             FROM job_event WHERE job_id = ?1 ORDER BY at_unix_ms, id",
        )?;
        let rows = stmt.query_map([job_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (at_unix_ms, from_raw, to_raw, attempt, reason_code, detail) = row?;
            out.push(JobEvent {
                at_unix_ms,
                from_state: from_raw
                    .map(|s| parse_enum("job_event.from_state", &s, JobState::parse))
                    .transpose()?,
                to_state: parse_enum("job_event.to_state", &to_raw, JobState::parse)?,
                attempt,
                reason_code,
                detail,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::file::{FileRepo, FileUpsert};
    use crate::repo::tests_support::{Fixture, fixture};

    fn seed_file(f: &Fixture, name: &str, inode: i64) -> i64 {
        f.write(FileRepo::upsert_op(FileUpsert {
            library_id: "tv".into(),
            canonical_path: format!("/mnt/tv/{name}.mkv"),
            path_hash: format!("h-{name}"),
            size_bytes: 100,
            mtime_unix: 10,
            mtime_ns: 0,
            inode: Some(inode),
            dev: Some(1),
            nlink: 1,
            scan_generation: 1,
        }))
        .last_id
        .unwrap()
    }

    fn seeded() -> (Fixture, JobRepo, i64) {
        let f = fixture();
        f.seed_library("tv");
        let repo = JobRepo::new(f.pool.clone());
        let file_id = seed_file(&f, "a", 1);
        (f, repo, file_id)
    }

    fn new_job(id: &str, file_id: i64, class: JobClass) -> NewJob {
        NewJob {
            id: id.into(),
            file_id,
            library_id: "tv".into(),
            class,
            size_bucket: SizeBucket::Small,
            requirements_json: "[]".into(),
            requirements_bucket_key: "audio/small".into(),
            expected_content_sig: "sig".into(),
            rules_version: "v1".into(),
            priority: 0,
            parent_job_id: None,
        }
    }

    fn to(f: &Fixture, id: &str, from: JobState, state: JobState) {
        f.write(JobRepo::transition_op(id.into(), from, state, None, None));
    }

    #[test]
    fn a_new_job_starts_pending_with_an_opening_event() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        let got = repo.get("j1").unwrap();
        assert_eq!(got.state, JobState::Pending);
        assert_eq!(got.class, JobClass::Audio);
        assert_eq!(got.attempt, 0);
        let events = repo.events("j1").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].from_state, None,
            "the opening event has no prior state"
        );
        assert_eq!(events[0].to_state, JobState::Pending);
    }

    #[test]
    fn the_happy_path_walks_the_state_machine_and_leaves_a_ledger() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        let path = [
            JobState::Pending,
            JobState::Eligible,
            JobState::Assigned,
            JobState::Running,
            JobState::Verifying,
            JobState::Committing,
            JobState::Succeeded,
        ];
        for w in path.windows(2) {
            to(&f, "j1", w[0], w[1]);
        }
        assert_eq!(repo.get("j1").unwrap().state, JobState::Succeeded);
        let events = repo.events("j1").unwrap();
        assert_eq!(events.len(), path.len(), "every transition is recorded");
        assert_eq!(events.last().unwrap().to_state, JobState::Succeeded);
    }

    /// An edge the state machine forbids is an error, not a silent no-op. A
    /// caller asking for `Running -> Succeeded` has skipped validation and the
    /// commit ritual, and returning `Ok` would hide that.
    #[test]
    fn an_illegal_edge_is_refused_and_changes_nothing() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        to(&f, "j1", JobState::Pending, JobState::Eligible);
        to(&f, "j1", JobState::Eligible, JobState::Assigned);
        to(&f, "j1", JobState::Assigned, JobState::Running);

        let e = f
            .try_write(JobRepo::transition_op(
                "j1".into(),
                JobState::Running,
                JobState::Succeeded,
                None,
                None,
            ))
            .unwrap_err();
        assert!(matches!(e, StoreError::IllegalTransition { .. }), "{e:?}");
        assert_eq!(repo.get("j1").unwrap().state, JobState::Running);
    }

    /// The compare-and-swap. A `SELECT` then `UPDATE` would pass the happy-path
    /// test: the point here is that the update itself carries the expected
    /// state, so a job moved in between is not overwritten.
    #[test]
    fn a_transition_from_the_wrong_state_loses_the_race_rather_than_clobbering() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        to(&f, "j1", JobState::Pending, JobState::Eligible);
        to(&f, "j1", JobState::Eligible, JobState::Assigned);

        // Something else cancels it first.
        to(&f, "j1", JobState::Assigned, JobState::Cancelled);

        // The dispatcher, still believing it is Assigned, tries to start it.
        let e = f
            .try_write(JobRepo::transition_op(
                "j1".into(),
                JobState::Assigned,
                JobState::Running,
                None,
                None,
            ))
            .unwrap_err();
        match e {
            StoreError::TransitionRaceLost { job_id, expected } => {
                assert_eq!(job_id, "j1");
                assert_eq!(expected, "Assigned");
            }
            other => panic!("expected TransitionRaceLost, got {other:?}"),
        }
        assert_eq!(
            repo.get("j1").unwrap().state,
            JobState::Cancelled,
            "the cancellation must stand"
        );
    }

    /// A terminal row is immutable. An operator retry inserts a new job with
    /// `parent_job_id` set rather than reanimating this one, so its history
    /// stays intact.
    #[test]
    fn a_terminal_job_cannot_be_moved() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        to(&f, "j1", JobState::Pending, JobState::Cancelled);
        let e = f
            .try_write(JobRepo::transition_op(
                "j1".into(),
                JobState::Cancelled,
                JobState::Eligible,
                None,
                None,
            ))
            .unwrap_err();
        assert!(matches!(e, StoreError::IllegalTransition { .. }), "{e:?}");
        assert_eq!(repo.get("j1").unwrap().state, JobState::Cancelled);
    }

    /// A failed transition must not leave a ledger entry claiming it happened.
    /// Both writes live in one op, so the writer's savepoint rolls back either
    /// both or neither.
    #[test]
    fn a_refused_transition_writes_no_event() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        let before = repo.events("j1").unwrap().len();
        let _ = f.try_write(JobRepo::transition_op(
            "j1".into(),
            JobState::Pending,
            JobState::Running,
            None,
            None,
        ));
        assert_eq!(repo.events("j1").unwrap().len(), before);
    }

    /// Enforced by `idx_job_open_per_file`, not by dispatcher discipline —
    /// double dispatch is structurally impossible rather than merely unlikely.
    #[test]
    fn a_second_open_job_for_one_file_is_refused() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        let e = f
            .try_write(JobRepo::create_op(new_job(
                "j2",
                file_id,
                JobClass::VideoCpu,
            )))
            .unwrap_err();
        assert!(matches!(e, StoreError::Sqlite(_)), "{e:?}");
        assert_eq!(repo.open_for_file(file_id).unwrap().unwrap().id, "j1");
    }

    /// ...but the follow-up video job after a finished audio pass must be
    /// allowed, or the two-stage flow cannot exist at all.
    #[test]
    fn a_followup_job_is_allowed_once_the_first_is_terminal() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        for (from, to_state) in [
            (JobState::Pending, JobState::Eligible),
            (JobState::Eligible, JobState::Assigned),
            (JobState::Assigned, JobState::Running),
            (JobState::Running, JobState::Verifying),
            (JobState::Verifying, JobState::Committing),
            (JobState::Committing, JobState::Succeeded),
        ] {
            to(&f, "j1", from, to_state);
        }

        let mut followup = new_job("j2", file_id, JobClass::VideoCpu);
        followup.parent_job_id = Some("j1".into());
        f.write(JobRepo::create_op(followup));

        let open = repo.open_for_file(file_id).unwrap().unwrap();
        assert_eq!(open.id, "j2");
        assert_eq!(open.parent_job_id.as_deref(), Some("j1"));
    }

    #[test]
    fn a_file_with_no_open_job_reports_none() {
        let (_f, repo, file_id) = seeded();
        assert!(repo.open_for_file(file_id).unwrap().is_none());
    }

    #[test]
    fn jobs_are_listed_highest_priority_first_and_counted_by_state() {
        let f = fixture();
        f.seed_library("tv");
        let repo = JobRepo::new(f.pool.clone());
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let file_id = seed_file(&f, name, i as i64);
            let mut job = new_job(&format!("j-{name}"), file_id, JobClass::Audio);
            job.priority = i as i64;
            f.write(JobRepo::create_op(job));
        }
        to(&f, "j-a", JobState::Pending, JobState::Eligible);

        let counts = repo.count_by_state().unwrap();
        assert_eq!(counts.len(), 2);
        assert_eq!(
            counts
                .iter()
                .find(|(s, _)| *s == JobState::Pending)
                .unwrap()
                .1,
            2
        );

        let pending = repo.in_state(JobState::Pending, 10).unwrap();
        assert_eq!(pending.first().unwrap().id, "j-c");
    }

    #[test]
    fn a_missing_job_reports_not_found() {
        let (_f, repo, _) = seeded();
        let e = repo.get("nope").unwrap_err();
        assert!(
            matches!(e, StoreError::NotFound { kind: "job", .. }),
            "{e:?}"
        );
    }

    #[test]
    fn a_terminal_reason_is_retained() {
        let (f, repo, file_id) = seeded();
        f.write(JobRepo::create_op(new_job("j1", file_id, JobClass::Audio)));
        f.write(JobRepo::transition_op(
            "j1".into(),
            JobState::Pending,
            JobState::Cancelled,
            Some("operator_cancelled".into()),
            Some("library disabled".into()),
        ));
        assert_eq!(
            repo.get("j1").unwrap().terminal_reason.as_deref(),
            Some("operator_cancelled")
        );
    }
}
