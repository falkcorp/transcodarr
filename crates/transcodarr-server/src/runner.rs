// file: crates/transcodarr-server/src/runner.rs
// version: 1.2.0
// guid: 2c94ea70-58d1-4b36-9f82-0a7e14b6d539
// last-edited: 2026-08-06
//! Single-node execution: take a job, encode it, validate it, install it.
//!
//! No dispatcher, no agents, no gRPC. The agent runs in-process, which is what
//! makes Phase 3 a risk-retirement exercise rather than a distributed-systems
//! one: the commit ritual is exercised against real media before any machinery
//! exists that could hide a bug in it.
//!
//! The order is deliberate and is the whole safety argument:
//!
//! 1. Encode into the work area — the destination is untouched throughout.
//! 2. Validate the output, gates in order, size never reached first.
//! 3. Only then run the commit ritual.
//!
//! A failure at step 1 or 2 leaves the library exactly as it was. Only step 3
//! touches the destination, and it is the one part with a durable journal.

use std::sync::Arc;

use transcodarr_agent::identity::{agent_uid, boot_id};
use transcodarr_agent::{
    CommitRequest, CommitRitual, Executor, ExecutorConfig, Resolution, SourceGuard, WorkArea,
};
use transcodarr_core::job::JobState;
use transcodarr_core::plan::JobPaths;
use transcodarr_core::policy::{self, Policy};
use transcodarr_store::repo::{
    CommitIntentRepo, FileRepo, JobRepo, LibraryRecord, NewIntent, TrashRepo,
};
use transcodarr_store::{ReadPool, WriteLane, Writer};

use crate::ServerError;

/// How long a replaced original is retained by default.
///
/// Seven days. Long enough that a bad policy change is noticed and undone from
/// the trash rather than from backups; short enough that a library's worth of
/// originals does not accumulate indefinitely. Pool pressure can bring it
/// forward, but never below `MIN_GRACE_SECONDS`.
pub const DEFAULT_RETENTION_SECONDS: i64 = 7 * 24 * 3600;

/// How one job turned out.
#[derive(Debug, Clone)]
pub struct JobOutcome {
    /// Which job.
    pub job_id: String,
    /// The file it operated on.
    pub path: String,
    /// How the install resolved, if it got that far.
    pub resolution: Option<Resolution>,
    /// Why it did not, if it did not.
    pub rejected: Option<String>,
    /// Bytes saved. Negative when an audio pass legitimately grew the file.
    pub bytes_delta: i64,
}

/// What a run did.
#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    /// Jobs attempted.
    pub attempted: i64,
    /// Jobs whose output was installed.
    pub installed: i64,
    /// Jobs whose output was rejected by validation.
    pub rejected: i64,
    /// Jobs that failed to run at all.
    pub failed: i64,
    /// Per-job detail.
    pub jobs: Vec<JobOutcome>,
}

/// Runs jobs on this machine, start to finish.
pub struct LocalRunner {
    files: FileRepo,
    jobs: JobRepo,
    writer: Arc<Writer>,
    executor: Executor,
}

impl LocalRunner {
    /// Build a runner.
    pub fn new(pool: ReadPool, writer: Arc<Writer>, config: ExecutorConfig) -> Self {
        Self {
            files: FileRepo::new(pool.clone()),
            jobs: JobRepo::new(pool),
            writer,
            executor: Executor::new(config),
        }
    }

    /// Run up to `limit` pending jobs for a library.
    ///
    /// Recovery runs first, before any new work is accepted. An agent that
    /// started encoding while an unresolved intent sat on disk could be handed
    /// the same file again and install over its own half-finished replace.
    pub fn run_library(
        &self,
        library: &LibraryRecord,
        policy: &Policy,
        limit: u32,
        dry_run: bool,
        only_class: Option<transcodarr_core::job::JobClass>,
    ) -> Result<RunOutcome, ServerError> {
        let work = WorkArea::open(
            std::path::Path::new(&library.work_dir),
            &agent_uid(),
            boot_id(),
        )?;
        // Not `work.path()`: the journal is stable per installation, so a
        // restart can still find what the previous instance was doing. See the
        // module documentation on `WorkArea`.
        let journal = work.open_journal()?;
        let ritual = CommitRitual::new(journal, work.clone());

        for (job_id, resolution) in ritual.recover_all()? {
            tracing::warn!(job = %job_id, resolution = %resolution.label(),
                "resolved an install interrupted by an earlier crash");
        }

        let mut out = RunOutcome::default();
        // Over-fetch, then filter: the queue is priority-ordered across every
        // class, so asking for `limit` rows and then filtering would return
        // almost nothing whenever the head of the queue is a class the caller
        // did not ask for.
        let pool = self
            .jobs
            .in_state(JobState::Pending, limit.saturating_mul(50).max(limit))?;
        for job in pool.into_iter().take_while(|_| true) {
            if job.library_id != library.id {
                continue;
            }
            if let Some(want) = only_class {
                if job.class != want {
                    continue;
                }
            }
            if out.attempted >= limit as i64 {
                break;
            }
            out.attempted += 1;
            match self.run_one(&job, library, policy, &work, &ritual, dry_run) {
                Ok(o) => {
                    match &o.resolution {
                        Some(Resolution::Installed { .. }) => out.installed += 1,
                        _ if o.rejected.is_some() => out.rejected += 1,
                        _ => {}
                    }
                    out.jobs.push(o);
                }
                Err(e) => {
                    out.failed += 1;
                    tracing::error!(job = %job.id, error = %e, "job failed");
                    out.jobs.push(JobOutcome {
                        job_id: job.id.clone(),
                        path: String::new(),
                        resolution: None,
                        rejected: Some(e.to_string()),
                        bytes_delta: 0,
                    });
                }
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_one(
        &self,
        job: &transcodarr_store::repo::JobRecord,
        library: &LibraryRecord,
        policy: &Policy,
        work: &WorkArea,
        ritual: &CommitRitual,
        dry_run: bool,
    ) -> Result<JobOutcome, ServerError> {
        let file = self.files.get(job.file_id)?;
        let source = std::path::PathBuf::from(&file.canonical_path);

        let Some(facts) = file.facts.clone() else {
            return Ok(JobOutcome {
                job_id: job.id.clone(),
                path: file.canonical_path.clone(),
                resolution: None,
                rejected: Some("no stored facts".into()),
                bytes_delta: 0,
            });
        };

        // The decision is re-derived rather than read back. The stored one may
        // predate the current policy, and encoding to a plan nobody would make
        // today is how a reverted policy change keeps taking effect.
        let decision = policy::evaluate(&facts, policy);
        let Some(plan) = policy::encode_plan_for(&decision, &facts) else {
            return Ok(JobOutcome {
                job_id: job.id.clone(),
                path: file.canonical_path.clone(),
                resolution: None,
                rejected: Some(format!("current policy owes no work: {}", decision.reason)),
                bytes_delta: 0,
            });
        };

        let temp = work.temp_path(&job.id, job.attempt, &source);
        let paths = JobPaths {
            input: source.clone(),
            output: temp.clone(),
        };

        if dry_run {
            return Ok(JobOutcome {
                job_id: job.id.clone(),
                path: file.canonical_path.clone(),
                resolution: None,
                rejected: Some(format!(
                    "dry run: {}",
                    transcodarr_core::plan::format_command(
                        "ffmpeg",
                        &self.executor.argv_for(&plan, &paths)
                    )
                )),
                bytes_delta: 0,
            });
        }

        // Refuse before encoding, not after. Discovering the work area is on
        // the wrong filesystem *after* an hour of transcoding wastes the hour.
        work.ensure_same_device(&source)?;
        work.clear(&job.id, job.attempt, &source)?;

        let guard = SourceGuard::observe(&source)?;
        self.transition(&job.id, JobState::Pending, JobState::Eligible)?;
        self.transition(&job.id, JobState::Eligible, JobState::Assigned)?;
        self.transition(&job.id, JobState::Assigned, JobState::Running)?;

        let progress = work.path().join(format!("{}.progress", job.attempt));
        let exec = self
            .executor
            .run(&plan, &paths, &progress, |_| {})
            .map_err(ServerError::Agent)?;

        self.transition(&job.id, JobState::Running, JobState::Verifying)?;

        // The source duration is re-measured the same way the output's is --
        // from the last packet PTS -- because comparing a *header* duration
        // against a *packet* duration is not comparing like with like. A
        // packet's PTS is the presentation time of the last frame, not the end
        // of the file, so it sits one frame short of the header value. Measured
        // on a real 5s remux: header 5.000s, source PTS 4.900s, output PTS
        // 4.900s. Against the header that is a phantom 0.1s shortfall and a
        // perfectly good output is rejected; against the source PTS it is exact.
        let mut spec = policy::validation_spec_for(&facts, &decision);
        if let Ok(Some(measured)) = self.executor.last_packet_pts_us(&source) {
            spec.source_duration_us = measured;
        }
        let report = self
            .executor
            .validate(&spec, &temp, exec.exit_code)
            .map_err(ServerError::Agent)?;

        if !report.passed {
            // The destination was never touched, so there is nothing to undo.
            let _ = std::fs::remove_file(&temp);
            self.transition(&job.id, JobState::Verifying, JobState::Failed)?;
            return Ok(JobOutcome {
                job_id: job.id.clone(),
                path: file.canonical_path.clone(),
                resolution: None,
                rejected: Some(format!(
                    "validation failed at {:?}: {}",
                    report.failed_gate, report.detail
                )),
                bytes_delta: 0,
            });
        }

        self.transition(&job.id, JobState::Verifying, JobState::Committing)?;
        // The path below the library root is preserved, so two shows with the
        // same episode name do not collide in the trash and silently destroy
        // one another's originals.
        let trash = transcodarr_core::paths::trash_path_for(
            std::path::Path::new(&library.trash_dir),
            std::path::Path::new(&library.root_path),
            &source,
        );
        // The server-side ledger is written *before* the ritual touches
        // anything. The agent's journal survives a crash of the agent; this row
        // survives a crash of the connection -- without it, a result lost in
        // flight after a successful replace makes the next attempt re-encode a
        // file that has already been replaced.
        //
        // A refused grant is a refusal to proceed, not a warning to log past:
        // idx_commit_intent_live failing means another agent already holds this
        // path, and installing anyway is the double-replace the index exists to
        // prevent.
        let intent_id = format!("{}:{}", job.id, job.attempt);
        self.writer.submit_blocking(
            WriteLane::Commit,
            CommitIntentRepo::grant_op(NewIntent {
                id: intent_id.clone(),
                job_id: job.id.clone(),
                attempt: job.attempt,
                agent_id: agent_uid(),
                agent_uid: agent_uid(),
                fencing_epoch: job.fencing_epoch,
                source_path: file.canonical_path.clone(),
                temp_path: temp.to_string_lossy().to_string(),
                final_path: file.canonical_path.clone(),
                expected_content_sig: job.expected_content_sig.clone(),
            }),
        )?;

        let resolution = ritual.commit(&CommitRequest {
            job_id: job.id.clone(),
            attempt: job.attempt,
            fencing_epoch: job.fencing_epoch,
            temp_path: temp.clone(),
            final_path: source.clone(),
            trash_path: trash,
            expected_content_sig: job.expected_content_sig.clone(),
            source_guard: guard,
        })?;

        // Resolve the ledger row whatever happened, so the final path is not
        // left permanently locked by a live intent nobody will ever finish.
        self.writer.submit_blocking(
            WriteLane::Commit,
            CommitIntentRepo::resolve_op(intent_id, resolution.label().to_string()),
        )?;

        // Record the retained original only once it really is retained.
        // Writing the row first would leave a trash_entry pointing at a file
        // that was never moved, and the reaper would later try to delete the
        // live original.
        if let Resolution::Installed { trash_path, .. } = &resolution {
            self.writer.submit_blocking(
                WriteLane::Normal,
                TrashRepo::retain_op(
                    Some(file.id),
                    Some(job.id.clone()),
                    file.canonical_path.clone(),
                    trash_path.to_string_lossy().to_string(),
                    file.size_bytes,
                    DEFAULT_RETENTION_SECONDS,
                ),
            )?;
        }

        let bytes_delta = match &resolution {
            Resolution::Installed { output_bytes, .. } => file.size_bytes - *output_bytes as i64,
            _ => 0,
        };

        match &resolution {
            Resolution::Installed { .. } => {
                self.transition(&job.id, JobState::Committing, JobState::Succeeded)?;
            }
            Resolution::NeedsOperator { .. } => {
                self.transition(&job.id, JobState::Committing, JobState::NeedsOperator)?;
            }
            _ => {
                self.transition(&job.id, JobState::Committing, JobState::Failed)?;
            }
        }

        Ok(JobOutcome {
            job_id: job.id.clone(),
            path: file.canonical_path.clone(),
            resolution: Some(resolution),
            rejected: None,
            bytes_delta,
        })
    }

    fn transition(&self, job_id: &str, from: JobState, to: JobState) -> Result<(), ServerError> {
        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::transition_op(job_id.to_string(), from, to, None, None),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use transcodarr_core::facts::FileFacts;
    use transcodarr_store::repo::{FileUpsert, LibraryRepo, TrashRepo};
    use transcodarr_store::{Db, ReadPool, Writer};

    struct Harness {
        _dir: TempDir,
        root: TempDir,
        pool: ReadPool,
        writer: Arc<Writer>,
    }

    fn harness() -> Harness {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open_unchecked(&path).unwrap();
        let pool = ReadPool::open(&path, 4).unwrap();
        Harness {
            _dir: dir,
            root: TempDir::new().unwrap(),
            pool,
            writer: Arc::new(Writer::start(db)),
        }
    }

    impl Harness {
        fn library(&self) -> LibraryRecord {
            let lib = LibraryRecord {
                id: "tv".into(),
                name: "tv".into(),
                root_path: self.root.path().join("lib").to_string_lossy().to_string(),
                work_dir: self.root.path().join("work").to_string_lossy().to_string(),
                trash_dir: self.root.path().join("trash").to_string_lossy().to_string(),
                exclude_globs_json: "[]".into(),
                enabled: true,
                scan_parallelism: 4,
                priority: 0,
                min_mtime_age_s: 0,
            };
            std::fs::create_dir_all(&lib.root_path).unwrap();
            std::fs::create_dir_all(&lib.trash_dir).unwrap();
            self.writer
                .submit_blocking(WriteLane::Normal, LibraryRepo::upsert_op(lib.clone()))
                .unwrap();
            lib
        }

        /// A file with stored facts that owe an audio pass, and a real byte on
        /// disk so the ritual has something to move.
        fn seed_job(&self, lib: &LibraryRecord, name: &str) -> i64 {
            let path = std::path::Path::new(&lib.root_path).join(name);
            std::fs::write(&path, b"original bytes").unwrap();
            let id = self
                .writer
                .submit_blocking(
                    WriteLane::Normal,
                    FileRepo::upsert_op(FileUpsert {
                        library_id: "tv".into(),
                        canonical_path: path.to_string_lossy().to_string(),
                        path_hash: transcodarr_core::stable_hash(name.as_bytes()),
                        size_bytes: 14,
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
                .unwrap();
            let facts = FileFacts {
                container: "matroska".into(),
                duration_us: Some(60_000_000),
                size_bytes: 14,
                audio_codecs: vec!["truehd".into()],
                audio_track_count: 1,
                ..FileFacts::default()
            };
            let sig = transcodarr_core::facts::content_sig(&facts).0;
            self.writer
                .submit_blocking(
                    WriteLane::Normal,
                    FileRepo::record_probe_op(
                        id,
                        facts,
                        sig,
                        transcodarr_core::facts::SizeBucket::Small,
                        "{}".into(),
                        "ffprobe".into(),
                    ),
                )
                .unwrap();
            id
        }

        fn runner(&self) -> LocalRunner {
            LocalRunner::new(
                self.pool.clone(),
                Arc::clone(&self.writer),
                // A binary that cannot exist, so no encode is ever attempted.
                // What is under test is the bookkeeping around the ritual.
                ExecutorConfig {
                    ffmpeg: "/nonexistent/ffmpeg".into(),
                    ffprobe: "/nonexistent/ffprobe".into(),
                    timeout: None,
                },
            )
        }
    }

    /// A dry run must not create a ledger row. Granting an intent for work that
    /// will never happen leaves the final path locked by a live intent nobody
    /// resolves.
    #[test]
    fn a_dry_run_grants_no_commit_intent() {
        let h = harness();
        let lib = h.library();
        h.seed_job(&lib, "a.mkv");
        crate::Evaluator::new(
            h.pool.clone(),
            Arc::clone(&h.writer),
            transcodarr_core::facts::SizeThresholds::default(),
        )
        .evaluate_library("tv", &policy::default_space_saver())
        .unwrap();

        let out = h
            .runner()
            .run_library(&lib, &policy::default_space_saver(), 5, true, None)
            .unwrap();
        assert_eq!(out.attempted, 1);

        let intents = transcodarr_store::repo::CommitIntentRepo::new(h.pool.clone());
        assert!(
            intents.live().unwrap().is_empty(),
            "a dry run must leave no live intent"
        );
    }

    /// An encode that never ran leaves the library untouched and the ledger
    /// clean -- nothing was granted, so nothing needs resolving.
    #[test]
    fn a_failed_encode_leaves_no_live_intent_and_no_trash_entry() {
        let h = harness();
        let lib = h.library();
        let file_id = h.seed_job(&lib, "a.mkv");
        crate::Evaluator::new(
            h.pool.clone(),
            Arc::clone(&h.writer),
            transcodarr_core::facts::SizeThresholds::default(),
        )
        .evaluate_library("tv", &policy::default_space_saver())
        .unwrap();

        let out = h
            .runner()
            .run_library(&lib, &policy::default_space_saver(), 5, false, None)
            .unwrap();
        assert_eq!(out.attempted, 1);
        assert_eq!(out.installed, 0);

        let intents = transcodarr_store::repo::CommitIntentRepo::new(h.pool.clone());
        assert!(
            intents.live().unwrap().is_empty(),
            "a failed encode must not leave the path locked"
        );
        assert_eq!(
            TrashRepo::new(h.pool.clone()).retained_totals().unwrap().0,
            0,
            "nothing was replaced, so nothing may be retained"
        );

        // The original is exactly where it was.
        let rec = FileRepo::new(h.pool.clone()).get(file_id).unwrap();
        assert!(std::path::Path::new(&rec.canonical_path).exists());
        assert_eq!(
            std::fs::read(&rec.canonical_path).unwrap(),
            b"original bytes"
        );
    }
}
