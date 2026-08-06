// file: crates/transcodarr-server/src/orchestrator.rs
// version: 1.0.0
// guid: 74b2e9c0-3a58-4f16-9d47-0e85b3c21fa6
// last-edited: 2026-08-06
//! The loop: queue in, assignments out, and the ledger kept honest.
//!
//! Every part this drives already existed and had no caller — `Dispatcher`,
//! `CapacityLedger`, `Reconciler`, `ScheduleEngine`. This is the thing that
//! runs them on a tick, and it is deliberately the only place where "what
//! should happen" turns into "an agent was told to do it".
//!
//! ## Rebuild the ledger, do not maintain it
//!
//! Capacity is rebuilt from the database at the top of every tick rather than
//! adjusted as jobs come and go. Incremental accounting is a second source of
//! truth about which agent holds what, and the failure mode is silent: a
//! missed release leaks a slot, the agent looks full, and the fleet quietly
//! runs at less capacity than it has with nothing in any log. Rebuilding costs
//! one indexed query over the in-flight set — tens of rows — and cannot drift.
//!
//! ## The intent is written before the assignment is sent
//!
//! `commit_intent` is the server's record that a particular agent has
//! permission to replace a particular path. It is written at dispatch, not when
//! the agent asks, because the unique index over live intents is what makes it
//! *impossible* to place two jobs against one destination. Writing it only on
//! request would leave that window open for the whole length of an encode.
//!
//! A job that then fails validation must release it, or that path is blocked
//! forever by a job nobody is running. See [`Orchestrator::abandon`].
//!
//! ## What a tick does not do
//!
//! It does not decide policy. The plan and the validation spec are re-derived
//! from stored facts at dispatch time, exactly as `admin run` does, so a policy
//! change takes effect on the next dispatch rather than being frozen into the
//! job row when it was created.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use transcodarr_core::capability::{Capability, Requirements};
use transcodarr_core::job::JobState;
use transcodarr_core::plan::JobPaths;
use transcodarr_core::policy::{self, Policy};
use transcodarr_proto::pb;
use transcodarr_store::repo::{
    AgentRepo, CommitIntentRepo, FileRepo, JobRecord, JobRepo, LibraryRepo, NewIntent,
};
use transcodarr_store::{ReadPool, WriteLane, Writer};

use crate::ServerError;
use crate::capacity::{AgentLimits, CapacityLedger, Grant};
use crate::dispatch::{AgentEntry, Dispatcher, QueuedJob};
use crate::fleet::AgentTable;
use crate::reconcile::{Action, InFlight, Reconciler};

/// States in which a job is legitimately held by an agent.
const HELD_STATES: [JobState; 4] = [
    JobState::Assigned,
    JobState::Running,
    JobState::Verifying,
    JobState::Committing,
];

/// How many pending jobs one pass considers.
///
/// The queue is priority-ordered across every class, so this is a window on the
/// head of it rather than a limit on throughput: what does not fit is
/// considered next tick, in the same order.
const QUEUE_WINDOW: u32 = 512;

/// How often the loop runs when no interval is given.
pub const DEFAULT_TICK: Duration = Duration::from_secs(5);

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickOutcome {
    /// Jobs placed on an agent.
    pub dispatched: Vec<String>,
    /// Jobs that stayed put, with the stage that refused them.
    pub blocked: Vec<(String, &'static str)>,
    /// Jobs the reconciler returned to the queue.
    pub requeued: Vec<String>,
    /// Jobs the reconciler could not decide.
    pub escalated: Vec<String>,
}

/// Runs the dispatch loop.
pub struct Orchestrator {
    files: FileRepo,
    jobs: JobRepo,
    agents: AgentRepo,
    libraries: LibraryRepo,
    intents: CommitIntentRepo,
    writer: Arc<Writer>,
    fleet: AgentTable,
    dispatcher: Mutex<Dispatcher>,
    ledger: Mutex<CapacityLedger>,
    reconciler: Reconciler,
    policy: Policy,
    limits: AgentLimits,
}

impl Orchestrator {
    /// Build a loop over the store and a fleet table.
    ///
    /// The fleet table must be the one the [`crate::session::AgentSession`]
    /// holds. Two tables each work perfectly and see different fleets, and the
    /// symptom is a loop that dispatches nothing while everything looks healthy.
    pub fn new(
        pool: ReadPool,
        writer: Arc<Writer>,
        fleet: AgentTable,
        policy: Policy,
        limits: AgentLimits,
    ) -> Self {
        Self {
            files: FileRepo::new(pool.clone()),
            jobs: JobRepo::new(pool.clone()),
            agents: AgentRepo::new(pool.clone()),
            libraries: LibraryRepo::new(pool.clone()),
            intents: CommitIntentRepo::new(pool),
            writer,
            fleet,
            dispatcher: Mutex::new(Dispatcher::new()),
            ledger: Mutex::new(CapacityLedger::new()),
            reconciler: Reconciler::new(),
            policy,
            limits,
        }
    }

    /// Run until the future resolves.
    pub async fn run(&self, tick: Duration, shutdown: impl std::future::Future<Output = ()>) {
        tokio::pin!(shutdown);
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = ticker.tick() => {
                    match self.tick().await {
                        Ok(outcome) if outcome != TickOutcome::default() => {
                            tracing::info!(
                                dispatched = outcome.dispatched.len(),
                                blocked = outcome.blocked.len(),
                                requeued = outcome.requeued.len(),
                                escalated = outcome.escalated.len(),
                                "dispatch pass"
                            );
                        }
                        Ok(_) => {}
                        // A failed pass is logged and the loop continues. The
                        // next tick re-reads everything from the database, so a
                        // transient store error costs five seconds rather than
                        // stopping dispatch until somebody notices.
                        Err(e) => tracing::error!(error = %e, "dispatch pass failed"),
                    }
                }
            }
        }
        tracing::info!("dispatch loop stopped");
    }

    /// One pass: reconcile, then dispatch.
    ///
    /// Reconciliation first, and the order matters. It returns work whose agent
    /// has gone to the queue, and doing it before dispatch means that work is
    /// placed this tick rather than sitting for another interval — but more
    /// importantly, it frees the capacity those jobs were holding before the
    /// ledger is rebuilt from it.
    pub async fn tick(&self) -> Result<TickOutcome, ServerError> {
        let mut outcome = TickOutcome::default();
        self.reconcile(&mut outcome)?;

        let agents = self.fleet_view()?;
        if agents.is_empty() {
            return Ok(outcome);
        }
        self.rebuild_capacity(&agents)?;

        let pending = self.jobs.in_state(JobState::Pending, QUEUE_WINDOW)?;
        if pending.is_empty() {
            return Ok(outcome);
        }

        let queued: Vec<QueuedJob> = pending.iter().filter_map(|j| self.queued(j)).collect();
        let round = {
            let mut dispatcher = self.dispatcher.lock().unwrap_or_else(|e| e.into_inner());
            let mut ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
            dispatcher.set_agents(agents.clone());
            dispatcher.dispatch(&queued, &mut ledger)
        };

        for blocked in &round.blocked {
            outcome
                .blocked
                .push((blocked.job_id.clone(), blocked.stage));
            tracing::debug!(job = %blocked.job_id, stage = blocked.stage, detail = %blocked.detail,
                "job did not dispatch");
        }

        for assignment in &round.assignments {
            let Some(job) = pending.iter().find(|j| j.id == assignment.job_id) else {
                continue;
            };
            match self.place(job, &assignment.agent_id).await {
                Ok(()) => outcome.dispatched.push(job.id.clone()),
                Err(e) => {
                    // The permit is handed back, or a job that failed to leave
                    // the building holds a slot until the process restarts.
                    self.ledger
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .release(&job.id);
                    tracing::warn!(job = %job.id, agent = %assignment.agent_id, error = %e,
                        "could not place a job; its slot was returned");
                }
            }
        }

        Ok(outcome)
    }

    /// Who is connected, registered, and willing.
    ///
    /// Connectedness comes from the fleet table and everything else from the
    /// database, because the two answer different questions: a row survives a
    /// reboot and a stream does not. Dispatching to an agent whose stream
    /// closed a second ago sends the assignment nowhere while the slot stays
    /// counted.
    fn fleet_view(&self) -> Result<Vec<AgentEntry>, ServerError> {
        let connected: HashSet<String> = self
            .fleet
            .connected()
            .into_iter()
            .map(|c| c.agent_id)
            .collect();

        let mut entries = Vec::new();
        for agent in self.agents.dispatchable()? {
            if !connected.contains(&agent.id) {
                continue;
            }
            let capability: Capability = match serde_json::from_str(&agent.capability_json) {
                Ok(c) => c,
                // Refused, not defaulted. An unreadable capability document
                // means we do not know what this agent can do, and an empty one
                // would silently make it eligible for nothing while looking
                // healthy — or, worse, for anything with no requirements.
                Err(e) => {
                    tracing::warn!(agent = %agent.id, error = %e,
                        "capability document unreadable; not dispatching to this agent");
                    continue;
                }
            };
            let workarea_free_bytes = capability.workarea_free_bytes;
            entries.push(AgentEntry {
                id: agent.id,
                capability,
                commit_eligible: agent.commit_eligible,
                accepting: agent.admin_state == "Enabled",
                workarea_free_bytes,
            });
        }
        Ok(entries)
    }

    /// Rebuild the capacity ledger from what the database says is in flight.
    fn rebuild_capacity(&self, agents: &[AgentEntry]) -> Result<(), ServerError> {
        let mut in_flight = Vec::new();
        for state in HELD_STATES {
            for job in self.jobs.in_state(state, QUEUE_WINDOW)? {
                if let Some(agent_id) = job.agent_id.clone() {
                    in_flight.push((
                        agent_id,
                        job.id,
                        Grant {
                            class: job.class,
                            size_bucket: job.size_bucket,
                        },
                        job.state,
                    ));
                }
            }
        }

        let mut ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        for agent in agents {
            ledger.set_limits(&agent.id, self.limits.clone());
        }
        let orphaned = ledger.rebuild(in_flight);
        if !orphaned.is_empty() {
            // Held by an agent with no limits configured, which means an agent
            // that is not connected. The reconciler is what decides their fate;
            // saying so here is what makes that visible rather than mysterious.
            tracing::debug!(
                count = orphaned.len(),
                "in-flight jobs held by agents that are not connected"
            );
        }
        Ok(())
    }

    /// A stored job in the shape the dispatcher wants.
    fn queued(&self, job: &JobRecord) -> Option<QueuedJob> {
        let requirements: Requirements = match serde_json::from_str(&job.requirements_json) {
            Ok(r) => r,
            // Skipped rather than dispatched with no requirements: a job whose
            // requirements cannot be read would otherwise match every agent,
            // which is the one interpretation guaranteed to be wrong.
            Err(e) => {
                tracing::warn!(job = %job.id, error = %e, "requirements unreadable; not dispatching");
                return None;
            }
        };
        Some(QueuedJob {
            id: job.id.clone(),
            class: job.class,
            size_bucket: job.size_bucket,
            requirements,
            bucket_key: job.requirements_bucket_key.clone(),
            excluded_agents: Vec::new(),
        })
    }

    /// Hand one job to one agent.
    async fn place(&self, job: &JobRecord, agent_id: &str) -> Result<(), ServerError> {
        let file = self.files.get(job.file_id)?;
        let library = self.libraries.get(&job.library_id)?;
        let Some(facts) = file.facts.clone() else {
            return Err(ServerError::ProbeFailed {
                path: file.canonical_path.clone(),
                reason: "no stored facts".into(),
            });
        };

        // Re-derived, not read back. The stored decision may predate the
        // current policy, and encoding to a plan nobody would make today is how
        // a reverted policy change keeps taking effect.
        let decision = policy::evaluate(&facts, &self.policy);
        let Some(plan) = policy::encode_plan_for(&decision, &facts) else {
            return Err(ServerError::ProbeFailed {
                path: file.canonical_path.clone(),
                reason: format!("current policy owes no work: {}", decision.reason),
            });
        };

        let epoch = self
            .fleet
            .get(agent_id)
            .map(|c| c.fencing_epoch)
            .ok_or_else(|| ServerError::ProbeFailed {
                path: file.canonical_path.clone(),
                reason: format!("{agent_id} disconnected before the assignment was built"),
            })?;

        let source = std::path::PathBuf::from(&file.canonical_path);
        let temp = temp_path_for(&library.work_dir, agent_id, &job.id, job.attempt, &source);
        let paths = JobPaths {
            input: source.clone(),
            output: temp.clone(),
        };
        let argv = transcodarr_core::plan::build_ffmpeg_argv(&plan, &paths);
        let spec = policy::validation_spec_for(&facts, &decision);

        // The ledger row goes in before the assignment goes out. The unique
        // index over live intents is what makes two jobs against one
        // destination impossible; writing it when the agent asks instead would
        // leave that window open for the length of an encode.
        let intent_id = format!("{}:{}", job.id, job.attempt);
        self.writer.submit_blocking(
            WriteLane::Commit,
            CommitIntentRepo::grant_op(NewIntent {
                id: intent_id,
                job_id: job.id.clone(),
                attempt: job.attempt,
                agent_id: agent_id.to_string(),
                agent_uid: agent_id.to_string(),
                fencing_epoch: epoch,
                source_path: file.canonical_path.clone(),
                temp_path: temp.display().to_string(),
                final_path: file.canonical_path.clone(),
                expected_content_sig: job.expected_content_sig.clone(),
            }),
        )?;

        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::transition_op(
                job.id.clone(),
                JobState::Pending,
                JobState::Eligible,
                None,
                None,
            ),
        )?;
        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::assign_op(job.id.clone(), agent_id.to_string(), epoch),
        )?;

        let assignment = pb::JobAssignment {
            job_id: job.id.clone(),
            attempt: u32::try_from(job.attempt).unwrap_or(0),
            fencing_epoch: u64::try_from(epoch).unwrap_or(0),
            source_path: file.canonical_path.clone(),
            final_path: file.canonical_path.clone(),
            temp_path: temp.display().to_string(),
            argv,
            validation_spec_json: serde_json::to_string(&spec).unwrap_or_default(),
            expected_content_sig: job.expected_content_sig.clone(),
        };

        if !self.fleet.send(
            agent_id,
            pb::ServerMessage {
                body: Some(pb::server_message::Body::Assignment(assignment)),
            },
        ) {
            // It never left. The job is put back and the intent released, or
            // this destination is blocked by an assignment nobody received.
            self.abandon(&job.id, job.attempt, "the agent became unreachable")?;
            return Err(ServerError::ProbeFailed {
                path: file.canonical_path,
                reason: format!("{agent_id} was unreachable when the assignment was sent"),
            });
        }

        tracing::info!(job = %job.id, agent = %agent_id, epoch, class = ?job.class, "dispatched");
        Ok(())
    }

    /// Put a job back and release the destination it was holding.
    ///
    /// Both halves, always. A job returned to the queue whose intent stayed
    /// live blocks its own destination forever: the next dispatch writes a
    /// second live intent for that path, the unique index refuses it, and the
    /// file is unprocessable until somebody reads the database by hand.
    fn abandon(&self, job_id: &str, attempt: i64, reason: &str) -> Result<(), ServerError> {
        self.writer.submit_blocking(
            WriteLane::Commit,
            CommitIntentRepo::resolve_op(format!("{job_id}:{attempt}"), "abandoned".to_string()),
        )?;
        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::transition_op(
                job_id.to_string(),
                JobState::Assigned,
                JobState::Retrying,
                Some("dispatch_failed".into()),
                Some(reason.to_string()),
            ),
        )?;
        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::transition_op(
                job_id.to_string(),
                JobState::Retrying,
                JobState::Eligible,
                None,
                None,
            ),
        )?;
        Ok(())
    }

    /// Decide what to do about work whose agent has gone quiet.
    fn reconcile(&self, outcome: &mut TickOutcome) -> Result<(), ServerError> {
        let connected: HashSet<String> = self
            .fleet
            .connected()
            .into_iter()
            .map(|c| c.agent_id)
            .collect();

        let mut in_flight = Vec::new();
        for state in HELD_STATES {
            for job in self.jobs.in_state(state, QUEUE_WINDOW)? {
                let has_live_intent = self
                    .intents
                    .live_for_job(&job.id)
                    .map(|i| i.is_some())
                    .unwrap_or(false);
                in_flight.push((
                    job.state,
                    InFlight {
                        job_id: job.id.clone(),
                        state: job.state,
                        agent_id: job.agent_id.clone(),
                        lease_expires_unix: self.lease_of(job.agent_id.as_deref()),
                        has_live_intent,
                    },
                ));
            }
        }

        let flat: Vec<InFlight> = in_flight.iter().map(|(_, f)| f.clone()).collect();
        for action in self.reconciler.reconcile(&flat, &connected, now_unix()) {
            match action {
                Action::Requeue {
                    job_id,
                    from,
                    reason,
                } => {
                    if let Err(e) = self.requeue(&job_id, from, &reason) {
                        tracing::warn!(job = %job_id, error = %e, "could not requeue");
                        continue;
                    }
                    outcome.requeued.push(job_id);
                }
                Action::Escalate { job_id, detail } => {
                    // Never guessed. Somewhere between retiring the original
                    // and installing the replacement, only the filesystem knows
                    // what happened.
                    tracing::error!(job = %job_id, %detail, "escalating an ambiguous commit");
                    let _ = self.writer.submit_blocking(
                        WriteLane::Normal,
                        JobRepo::transition_op(
                            job_id.clone(),
                            JobState::Committing,
                            JobState::NeedsOperator,
                            Some("ambiguous_commit".into()),
                            Some(detail),
                        ),
                    );
                    outcome.escalated.push(job_id);
                }
                Action::ReleaseCapacity { job_id } => {
                    self.ledger
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .release(&job_id);
                }
            }
        }
        Ok(())
    }

    /// Return one job to the queue, releasing what it held.
    fn requeue(&self, job_id: &str, from: JobState, reason: &str) -> Result<(), ServerError> {
        let attempt = self.jobs.get(job_id).map(|j| j.attempt).unwrap_or(0);
        self.writer.submit_blocking(
            WriteLane::Commit,
            CommitIntentRepo::resolve_op(format!("{job_id}:{attempt}"), "abandoned".to_string()),
        )?;
        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::transition_op(
                job_id.to_string(),
                from,
                JobState::Retrying,
                Some("agent_lost".into()),
                Some(reason.to_string()),
            ),
        )?;
        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::transition_op(
                job_id.to_string(),
                JobState::Retrying,
                JobState::Eligible,
                None,
                None,
            ),
        )?;
        self.ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release(job_id);
        Ok(())
    }

    /// When an agent's lease runs out, if it has one.
    fn lease_of(&self, agent_id: Option<&str>) -> Option<i64> {
        let id = agent_id?;
        self.agents.get(id).ok().flatten()?.lease_expires_unix
    }
}

/// Where an agent should stage this job's output.
///
/// Inside the library's own work directory, which the operator sets on the same
/// pool as the library — that colocation is decision D14, and it is what makes
/// the install a single atomic `rename(2)` rather than a copy across a
/// filesystem boundary with a window where the file exists nowhere.
///
/// Named with the agent in it so two agents cannot pick the same path, and with
/// the destination's extension because ffmpeg chooses its muxer from it.
fn temp_path_for(
    work_dir: &str,
    agent_id: &str,
    job_id: &str,
    attempt: i64,
    final_path: &std::path::Path,
) -> std::path::PathBuf {
    let ext = final_path
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "mkv".to_string());
    std::path::Path::new(work_dir).join(format!(
        "{}.{}.{attempt}.partial.{ext}",
        sanitise(agent_id),
        sanitise(job_id)
    ))
}

/// Seconds since the epoch.
///
/// Passed into the reconciler rather than read inside it, so a pass is
/// deterministic and can be driven through its own edge cases in a test.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Make a string safe as one path component.
fn sanitise(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two agents must never pick the same staging path, or the second one to
    /// write silently corrupts the first one's output.
    #[test]
    fn two_agents_get_different_temporary_paths() {
        let final_path = std::path::Path::new("/mnt/tv/show.mkv");
        let a = temp_path_for("/mnt/tv/work", "u1", "job-1", 0, final_path);
        let b = temp_path_for("/mnt/tv/work", "u2", "job-1", 0, final_path);
        assert_ne!(a, b);
    }

    /// ffmpeg picks its muxer from the extension; a `.tmp` would be muxed as
    /// whatever it guessed rather than what the plan asked for.
    #[test]
    fn the_temporary_path_keeps_the_destination_extension() {
        let p = temp_path_for(
            "/w",
            "u1",
            "job-1",
            2,
            std::path::Path::new("/mnt/tv/a.mkv"),
        );
        assert_eq!(p.extension().unwrap(), "mkv");
        assert!(p.starts_with("/w"));
        assert!(p.to_string_lossy().contains(".2."));
    }

    /// An identifier arriving with a slash must not place a file outside the
    /// work directory. Job and agent ids come from the database, but the
    /// database is not a trust boundary for path construction.
    #[test]
    fn identifiers_cannot_escape_the_work_directory() {
        let p = temp_path_for(
            "/w",
            "../../etc",
            "../../passwd",
            0,
            std::path::Path::new("/mnt/tv/a.mkv"),
        );
        assert!(p.starts_with("/w"), "{p:?}");
        assert!(!p.to_string_lossy().contains(".."));
    }

    /// Two attempts of one job must not share a path either: a leftover from
    /// the first would be mistaken for the second's output.
    #[test]
    fn two_attempts_do_not_share_a_temporary_path() {
        let f = std::path::Path::new("/mnt/tv/a.mkv");
        assert_ne!(
            temp_path_for("/w", "u1", "job-1", 0, f),
            temp_path_for("/w", "u1", "job-1", 1, f)
        );
    }
}
