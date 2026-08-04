// file: crates/transcodarr-server/src/runner.rs
// version: 1.0.0
// guid: 2c94ea70-58d1-4b36-9f82-0a7e14b6d539
// last-edited: 2026-08-03
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

use transcodarr_agent::{
    CommitRequest, CommitRitual, Executor, ExecutorConfig, IntentJournal, Resolution, SourceGuard,
    WorkArea,
};
use transcodarr_core::job::JobState;
use transcodarr_core::plan::JobPaths;
use transcodarr_core::policy::{self, Policy};
use transcodarr_store::repo::{FileRepo, JobRepo, LibraryRecord};
use transcodarr_store::{ReadPool, WriteLane, Writer};

use crate::ServerError;

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
    ) -> Result<RunOutcome, ServerError> {
        let work = WorkArea::open(
            std::path::Path::new(&library.work_dir),
            &agent_uid(),
            &boot_id(),
        )?;
        let journal = IntentJournal::open(&work.path().join("journal"))?;
        let ritual = CommitRitual::new(journal, work.clone());

        for (job_id, resolution) in ritual.recover_all()? {
            tracing::warn!(job = %job_id, resolution = %resolution.label(),
                "resolved an install interrupted by an earlier crash");
        }

        let mut out = RunOutcome::default();
        for job in self.jobs.in_state(JobState::Pending, limit)? {
            if job.library_id != library.id {
                continue;
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
        let trash = std::path::Path::new(&library.trash_dir).join(
            source
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| job.id.clone()),
        );
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

/// A stable identity for this agent installation.
///
/// Falls back to the hostname, then to a constant. It only has to be stable
/// across restarts and distinct between machines — it namespaces a work area,
/// it does not authenticate anything.
fn agent_uid() -> String {
    std::env::var("TRANSCODARR_AGENT_UID")
        .ok()
        .or_else(hostname)
        .unwrap_or_else(|| "local".to_string())
}

/// An identity for *this run* of the agent.
///
/// Distinct per process, so a restarted agent cannot mistake the leftovers of
/// its own previous life for work still in flight.
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("pid-{}", std::process::id()))
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
