// file: crates/transcodarr-agent/src/worker.rs
// version: 1.1.0
// guid: 8c1f37d5-4b0a-49e6-a2f8-13d70b6e5a94
// last-edited: 2026-08-06
//! The real [`Worker`]: an assignment in, an installed file or a reason out.
//!
//! Everything here already existed and was only reachable through `admin run`.
//! This is the same [`Executor`] and the same [`CommitRitual`] driven by the
//! server instead of by a local queue, which is the point — a distributed path
//! that re-implemented either would be a second implementation free to drift
//! from the one that was proven against real media.
//!
//! ## The agent does not compose the command
//!
//! `JobAssignment.argv` is built server-side and run verbatim. An agent that
//! rebuilt it locally could encode to a plan the server never authorised, and
//! nothing would notice until the output was installed. The same goes for the
//! validation spec: it arrives as JSON and is evaluated by
//! `transcodarr-core::validate`, the code the server links too.
//!
//! ## The order is the safety argument
//!
//! 1. Encode into the work area. The destination is untouched throughout.
//! 2. Validate, gates in order — size is never the first thing consulted,
//!    because a truncated file is always smaller.
//! 3. Ask permission.
//! 4. Only then run the ritual, which is the only step that touches the
//!    destination and the only one with a durable journal.
//!
//! A failure anywhere in 1–3 leaves the library exactly as it was.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use transcodarr_core::validate::{ValidationReport, ValidationSpec};
use transcodarr_proto::pb;

use crate::client::{Link, Worker};
use crate::commit::{CommitRequest, CommitRitual, Resolution, SourceGuard};
use crate::executor::Executor;
use crate::journal::IntentRecord;
use crate::workarea::WorkArea;

/// Runs assigned jobs on this machine.
pub struct LocalWorker {
    executor: Executor,
    ritual: CommitRitual,
    work_area: WorkArea,
    capability: pb::Capability,
    running: Mutex<HashSet<String>>,
    /// Jobs the server has revoked since they were handed out.
    ///
    /// Checked before permission is asked for, so a revoke actually stops an
    /// install rather than merely tidying the accounting. The server would
    /// refuse the commit anyway — it has no live intent for a job it revoked —
    /// but relying on that means the agent's own revoke handling does nothing,
    /// and the next reader would believe otherwise.
    revoked: Mutex<HashSet<String>>,
    draining: AtomicBool,
}

impl LocalWorker {
    /// Build a worker over an already-opened work area and ritual.
    ///
    /// The ritual is passed in rather than built here because it owns the
    /// journal, and where that journal lives is a decision with teeth — see
    /// [`WorkArea::open_journal`].
    pub fn new(
        executor: Executor,
        ritual: CommitRitual,
        work_area: WorkArea,
        capability: pb::Capability,
    ) -> Self {
        Self {
            executor,
            ritual,
            work_area,
            capability,
            running: Mutex::new(HashSet::new()),
            revoked: Mutex::new(HashSet::new()),
            draining: AtomicBool::new(false),
        }
    }

    /// Resolve every install left in flight by a previous process.
    ///
    /// Run once at startup, **after** `Register` has been told about them.
    /// Running it first would clear the journal, so `live_intents` would go out
    /// empty and the server would answer that nothing is unaccounted for —
    /// which reads exactly like a clean start.
    pub fn recover(&self) -> Vec<(String, Resolution)> {
        match self.ritual.recover_all() {
            Ok(resolved) => {
                for (job_id, resolution) in &resolved {
                    if resolution.is_resolved() {
                        tracing::warn!(job = %job_id, resolution = %resolution.label(),
                            "resolved an install interrupted by an earlier crash");
                    } else {
                        tracing::error!(job = %job_id, resolution = %resolution.label(),
                            "an interrupted install needs an operator");
                    }
                }
                resolved
            }
            Err(e) => {
                tracing::error!(error = %e, "the journal could not be read; no work will be safe");
                Vec::new()
            }
        }
    }

    /// The journal this worker records intents in.
    ///
    /// Exposed so a caller can inspect what is outstanding without going
    /// through the ritual — the CLI reports it at startup, and a test seeds it.
    pub fn journal(&self) -> &crate::journal::IntentJournal {
        self.ritual.journal()
    }

    /// Whether this worker has stopped accepting new assignments.
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    fn hold(&self, job_id: &str) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id.to_string());
    }

    /// Whether this job has been revoked since it was handed out.
    fn is_revoked(&self, job_id: &str) -> bool {
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(job_id)
    }

    fn release(&self, job_id: &str) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(job_id);
    }

    /// Encode, validate, and install one assignment.
    ///
    /// Split out of the trait method so the bookkeeping around it — holding the
    /// job in the running set, releasing it however this ends — cannot be
    /// skipped by an early return.
    async fn execute(&self, a: &pb::JobAssignment, link: &Link) {
        let temp = PathBuf::from(&a.temp_path);
        let final_path = PathBuf::from(&a.final_path);
        let attempt = i64::from(a.attempt);

        // Refused before a byte is written. An install that cannot be atomic is
        // not one to attempt and apologise for.
        if let Err(e) = self.work_area.ensure_same_device(&final_path) {
            tracing::error!(job = %a.job_id, error = %e, "refusing the assignment");
            link.result(failed_result(&a.job_id, attempt, &e.to_string()))
                .await;
            return;
        }

        // Observed before the encode, not after: this is what the ritual
        // compares against to prove the source has not been replaced under us
        // while we were busy.
        let guard = match SourceGuard::observe(&final_path) {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(job = %a.job_id, error = %e, "source unreadable");
                link.result(failed_result(&a.job_id, attempt, &e.to_string()))
                    .await;
                return;
            }
        };

        let execution = match self.encode(a, &temp, link).await {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(job = %a.job_id, error = %e, "encode failed to run");
                let _ = std::fs::remove_file(&temp);
                link.result(failed_result(&a.job_id, attempt, &e.to_string()))
                    .await;
                return;
            }
        };

        let report = self.judge(a, &temp, execution.exit_code);
        link.result(pb::JobResult {
            job_id: a.job_id.clone(),
            attempt: a.attempt,
            exit_code: execution.exit_code,
            signal: execution.signal.unwrap_or(0),
            stderr_tail: execution.stderr_tail.clone(),
            validation_json: serde_json::to_string(&report).unwrap_or_default(),
            output_bytes: execution.output_bytes,
        })
        .await;

        if !report.passed {
            tracing::warn!(job = %a.job_id, detail = %report.detail, "output rejected");
            let _ = std::fs::remove_file(&temp);
            return;
        }

        // A job revoked while it was encoding must not go on to install. The
        // encode itself was left alone -- it writes only into the work area,
        // and killing a child process mid-ritual is how a half-installed file
        // happens -- but this is where it stops.
        if self.is_revoked(&a.job_id) {
            tracing::warn!(job = %a.job_id, "revoked during the encode; not installing");
            let _ = std::fs::remove_file(&temp);
            return;
        }

        // Permission, then install. Never the other way around, and never an
        // install because permission merely did not arrive.
        let Some(grant) = link.request_commit(&a.job_id, attempt).await else {
            tracing::warn!(job = %a.job_id, "no commit grant; leaving the source intact");
            let _ = std::fs::remove_file(&temp);
            return;
        };

        let request = CommitRequest {
            job_id: a.job_id.clone(),
            attempt,
            // The epoch the *link* holds now, not the one the assignment
            // carried. A re-registration between assignment and install moves
            // it, and the server rejects a report bearing the old one.
            fencing_epoch: link.fencing_epoch(),
            temp_path: temp.clone(),
            final_path,
            trash_path: PathBuf::from(&grant.trash_path),
            expected_content_sig: a.expected_content_sig.clone(),
            source_guard: guard,
        };

        let resolution = match self.commit_blocking(request).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(job = %a.job_id, error = %e, "the commit ritual failed");
                // The same snake_case vocabulary `Resolution::label` uses; the
                // ledger stores this verbatim.
                link.report_commit(&a.job_id, attempt, "needs_operator", &e.to_string())
                    .await;
                return;
            }
        };

        tracing::info!(job = %a.job_id, resolution = %resolution.label(), "commit resolved");
        link.report_commit(
            &a.job_id,
            attempt,
            resolution.label(),
            &detail_of(&resolution),
        )
        .await;
    }

    /// Run ffmpeg, forwarding progress up the stream as it goes.
    ///
    /// The encode itself is blocking and long, so it runs on a blocking thread.
    /// Progress crosses back over a channel rather than being sent from inside
    /// the callback: the callback is on that blocking thread and cannot await,
    /// and a bounded `try_send` means a stalled stream drops frames instead of
    /// stalling the encoder.
    async fn encode(
        &self,
        a: &pb::JobAssignment,
        temp: &Path,
        link: &Link,
    ) -> Result<crate::executor::Execution, crate::AgentError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<pb::JobProgress>(8);
        let forward = {
            let (link, job_id) = (link.clone(), a.job_id.clone());
            tokio::spawn(async move {
                while let Some(mut p) = rx.recv().await {
                    p.job_id = job_id.clone();
                    link.progress(p).await;
                }
            })
        };

        let progress_path = temp.with_extension("progress");
        let argv = a.argv.clone();
        let (temp, executor) = (temp.to_path_buf(), self.executor.clone());
        let job_id = a.job_id.clone();

        let result = tokio::task::spawn_blocking(move || {
            executor.run_argv(&argv, &temp, &progress_path, |p| {
                let _ = tx.try_send(pb::JobProgress {
                    job_id: String::new(),
                    out_time_us: p.out_time_us,
                    frames: p.frames,
                    speed: p.speed.clone().unwrap_or_default(),
                    total_size: p.total_size,
                });
            })
        })
        .await;

        forward.abort();
        match result {
            Ok(execution) => execution,
            Err(e) => Err(crate::AgentError::Execute {
                program: format!("encode task for {job_id}"),
                source: std::io::Error::other(e),
            }),
        }
    }

    /// Judge the output against the spec the server sent.
    ///
    /// A spec that will not parse fails the job. It is not defaulted: an empty
    /// spec passes every gate, so guessing here would accept exactly the
    /// outputs nobody checked.
    ///
    /// **The source duration is re-measured here, the same way the output's
    /// is.** The server builds the spec from stored facts, whose duration came
    /// from the container header; validation measures the output's *last packet
    /// PTS*. Those are not the same quantity — a packet's PTS is the
    /// presentation time of the final frame, not the end of the file — so
    /// comparing one against the other invents a shortfall and rejects a
    /// perfectly good encode. Measured on a 2s remux: header 2.000s, output PTS
    /// 1.800s, against a tolerance of min(0.5%, 5s) = 10ms.
    ///
    /// The agent does this rather than the server because the agent is the one
    /// holding the file. Re-probing every source on the orchestrator would put
    /// media I/O back on the machine whose whole job is to stay out of it.
    fn judge(&self, a: &pb::JobAssignment, temp: &Path, exit_code: i32) -> ValidationReport {
        let mut spec: ValidationSpec = match serde_json::from_str(&a.validation_spec_json) {
            Ok(s) => s,
            Err(e) => {
                return ValidationReport {
                    passed: false,
                    failed_gate: None,
                    detail: format!("the validation spec could not be read: {e}"),
                    gates_run: Vec::new(),
                };
            }
        };
        // Only when it can actually be measured. A failed probe leaves the
        // header duration in place, which is the conservative direction: it can
        // reject a good output, never accept a truncated one.
        match self.executor.last_packet_pts_us(Path::new(&a.source_path)) {
            Ok(Some(measured)) => spec.source_duration_us = measured,
            Ok(None) => tracing::warn!(job = %a.job_id,
                "no last-packet PTS for the source; comparing against the header duration"),
            Err(e) => tracing::warn!(job = %a.job_id, error = %e,
                "could not measure the source duration; comparing against the header"),
        }

        match self.executor.validate(&spec, temp, exit_code) {
            Ok(report) => report,
            Err(e) => ValidationReport {
                passed: false,
                failed_gate: None,
                detail: format!("the output could not be judged: {e}"),
                gates_run: Vec::new(),
            },
        }
    }

    /// Run the ritual off the runtime — it is synchronous, fsyncing, and slow.
    ///
    /// `spawn_blocking` rather than `block_in_place`: the latter panics on a
    /// current-thread runtime, which is what `#[tokio::test]` gives you, so the
    /// tests would exercise a path the binary never takes.
    async fn commit_blocking(
        &self,
        request: CommitRequest,
    ) -> Result<Resolution, crate::AgentError> {
        let ritual = self.ritual.clone();
        match tokio::task::spawn_blocking(move || ritual.commit(&request)).await {
            Ok(result) => result,
            Err(e) => Err(crate::AgentError::Commit {
                step: "run the ritual",
                path: String::new(),
                source: std::io::Error::other(e),
            }),
        }
    }
}

#[tonic::async_trait]
impl Worker for LocalWorker {
    fn capability(&self) -> pb::Capability {
        self.capability.clone()
    }

    fn mounts(&self) -> Vec<pb::Mount> {
        self.capability.mounts.clone()
    }

    /// What the journal says was in flight, in the wire's shape.
    ///
    /// An unreadable journal reports *nothing outstanding*, which is wrong in
    /// the safe direction only by accident — so it is logged at error. The
    /// records are still on disk and recovery will refuse to pass over them.
    fn live_intents(&self) -> Vec<pb::LiveIntent> {
        match self.ritual.journal().outstanding() {
            Ok(records) => records.iter().map(wire_intent).collect(),
            Err(e) => {
                tracing::error!(error = %e, "the journal could not be replayed");
                Vec::new()
            }
        }
    }

    fn running_job_ids(&self) -> Vec<String> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Resolve the installs the server has no record of.
    ///
    /// Each one goes through the ritual's own recovery, which branches on how
    /// far it got: a `Granted` record only needs its staged file discarded, but
    /// a `Retired` one means the original is in the trash and the destination
    /// may be empty — restoring it is the whole job. Treating them alike would
    /// delete media.
    async fn on_unknown_intents(&self, job_ids: Vec<String>) {
        let unknown: HashSet<String> = job_ids.into_iter().collect();
        let records = match self.ritual.journal().outstanding() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "cannot resolve unknown intents: the journal is unreadable");
                return;
            }
        };

        for record in records.iter().filter(|r| unknown.contains(&r.job_id)) {
            match self.ritual.recover_one(record) {
                Ok(resolution) => {
                    tracing::warn!(
                        job = %record.job_id, phase = %record.phase,
                        resolution = %resolution.label(),
                        "the server has no record of this install; resolved locally"
                    );
                    if resolution.is_resolved() {
                        if let Err(e) = self.ritual.journal().clear(&record.job_id, record.attempt)
                        {
                            tracing::error!(job = %record.job_id, error = %e, "could not clear the record");
                        }
                    }
                }
                // Left on disk deliberately: an unresolved record is the only
                // evidence that something needs a human.
                Err(e) => tracing::error!(job = %record.job_id, error = %e,
                    "an unknown intent could not be resolved"),
            }
        }
    }

    /// Resolve whatever the last process left in flight.
    ///
    /// Synchronous inside an async method on purpose: this runs once, before
    /// the stream opens and before any assignment can arrive, so there is
    /// nothing for it to block.
    async fn on_startup(&self) {
        self.recover();
    }

    async fn on_assignment(&self, assignment: pb::JobAssignment, link: Link) {
        if self.is_draining() {
            tracing::warn!(job = %assignment.job_id, "refusing an assignment while draining");
            link.result(failed_result(
                &assignment.job_id,
                i64::from(assignment.attempt),
                "the agent is draining",
            ))
            .await;
            return;
        }

        self.hold(&assignment.job_id);
        self.execute(&assignment, &link).await;
        self.release(&assignment.job_id);
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&assignment.job_id);
    }

    /// Stop a job the server no longer recognises.
    ///
    /// The encode is not killed — it writes only into the work area, where it
    /// can hurt nothing, and tearing down a child process from under the ritual
    /// is how a half-installed file happens. What is stopped is the *install*:
    /// the job is marked revoked, and `execute` checks that before it asks
    /// permission. The claim is dropped too, since the server has already
    /// accounted for the slot as free.
    async fn on_revoke(&self, job_id: String, reason: String) {
        tracing::warn!(job = %job_id, %reason, "revoked; it will not be installed");
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id.clone());
        self.release(&job_id);
    }

    async fn on_drain(&self, drain: pb::Drain) -> Vec<String> {
        self.draining.store(true, Ordering::SeqCst);
        tracing::info!(immediate = drain.immediate, reason = %drain.reason, "draining");
        // Even an immediate drain reports what is running rather than killing
        // it. A job mid-ritual has a journal and a defined recovery; a job
        // killed between two renames has neither.
        self.running_job_ids()
    }
}

/// One journal record, in the wire's shape.
fn wire_intent(record: &IntentRecord) -> pb::LiveIntent {
    pb::LiveIntent {
        job_id: record.job_id.clone(),
        attempt: u32::try_from(record.attempt).unwrap_or(0),
        fencing_epoch: u64::try_from(record.fencing_epoch).unwrap_or(0),
        // Lowercase, which is the wire spelling `CommitPhase` parses. The
        // stored SQL values are capitalised and both are accepted, but the
        // schema's comment names these.
        phase: record.phase.as_str().to_ascii_lowercase(),
        temp_path: record.temp_path.display().to_string(),
        final_path: record.final_path.display().to_string(),
        trash_path: record.trash_path.display().to_string(),
    }
}

/// A result for a job that never produced an output.
fn failed_result(job_id: &str, attempt: i64, detail: &str) -> pb::JobResult {
    pb::JobResult {
        job_id: job_id.to_string(),
        attempt: u32::try_from(attempt).unwrap_or(0),
        // Not zero: zero is what a successful ffmpeg exits with, and a failure
        // that reports success is the one mistake this whole path exists to
        // avoid.
        exit_code: -1,
        signal: 0,
        stderr_tail: detail.to_string(),
        validation_json: String::new(),
        output_bytes: 0,
    }
}

/// The operator-facing half of a resolution.
fn detail_of(resolution: &Resolution) -> String {
    match resolution {
        Resolution::Installed { output_bytes, .. } => format!("installed, {output_bytes} bytes"),
        Resolution::SourceIntact { reason } | Resolution::SourceRestored { reason } => {
            reason.clone()
        }
        Resolution::NeedsOperator { detail } => detail.clone(),
    }
}
