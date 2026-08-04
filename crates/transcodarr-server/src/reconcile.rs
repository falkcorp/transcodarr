// file: crates/transcodarr-server/src/reconcile.rs
// version: 1.0.0
// guid: f2081ac6-45d9-4e37-b0c8-71e35da96248
// last-edited: 2026-08-03
//! Noticing when reality and the database have drifted apart.
//!
//! Everything else in the system assumes agents report what happened. The
//! reconciler exists because sometimes they do not: a process is killed, a
//! network partitions, a machine reboots. Its job is to decide what that
//! silence means, and the answer is different in each case.
//!
//! The rule throughout: **silence is never taken as success.** A job whose
//! agent stopped answering has an unknown outcome, and every path here either
//! returns it to the queue or escalates it to a human. Nothing is marked
//! `Succeeded` because nobody said otherwise.

use std::collections::HashSet;

use transcodarr_core::job::JobState;

/// How long an agent may be silent before its work is reclaimed.
///
/// Generous relative to the heartbeat interval, because reclaiming work an
/// agent is still doing is worse than waiting: the agent finishes, tries to
/// commit against a revoked epoch, and the job has meanwhile been handed to
/// someone else.
pub const LEASE_GRACE_SECONDS: i64 = 90;

/// What the reconciler decided about one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Return the job to the queue; its agent is gone and nothing was
    /// installed.
    Requeue {
        /// Which job.
        job_id: String,
        /// State it was found in.
        from: JobState,
        /// Why.
        reason: String,
    },
    /// Escalate: the outcome cannot be determined from here.
    ///
    /// Reached when a job was mid-commit. Somewhere between retiring the
    /// original and installing the replacement, only the filesystem knows what
    /// happened, and guessing either way risks either a lost file or a
    /// double-encode over a good one.
    Escalate {
        /// Which job.
        job_id: String,
        /// What is ambiguous.
        detail: String,
    },
    /// Release the capacity a vanished job was holding.
    ReleaseCapacity {
        /// Which job.
        job_id: String,
    },
}

/// A job as the reconciler finds it.
#[derive(Debug, Clone)]
pub struct InFlight {
    /// Job identity.
    pub job_id: String,
    /// Current state.
    pub state: JobState,
    /// The agent that holds it, if any.
    pub agent_id: Option<String>,
    /// When its lease runs out.
    pub lease_expires_unix: Option<i64>,
    /// Whether a live `commit_intent` row exists for it.
    pub has_live_intent: bool,
}

/// Decides what to do about jobs whose agents have gone quiet.
#[derive(Debug, Clone)]
pub struct Reconciler {
    grace_seconds: i64,
}

impl Default for Reconciler {
    fn default() -> Self {
        Self {
            grace_seconds: LEASE_GRACE_SECONDS,
        }
    }
}

impl Reconciler {
    /// A reconciler with the default lease grace.
    pub fn new() -> Self {
        Self::default()
    }

    /// A reconciler with an explicit grace period.
    pub fn with_grace(grace_seconds: i64) -> Self {
        Self { grace_seconds }
    }

    /// Decide what to do about the in-flight set.
    ///
    /// `connected` is the set of agents currently holding a live stream.
    /// `now_unix` is passed rather than read so a pass is deterministic and
    /// testable — a reconciler that consults the clock internally cannot be
    /// driven through its own edge cases.
    pub fn reconcile(
        &self,
        in_flight: &[InFlight],
        connected: &HashSet<String>,
        now_unix: i64,
    ) -> Vec<Action> {
        let mut actions = Vec::new();

        for job in in_flight {
            let agent_present = job
                .agent_id
                .as_ref()
                .map(|a| connected.contains(a))
                .unwrap_or(false);

            // A connected agent whose lease is current is doing its job.
            // Reclaiming work from it is how one file ends up encoded twice.
            let lease_ok = job
                .lease_expires_unix
                .map(|exp| now_unix <= exp + self.grace_seconds)
                .unwrap_or(false);

            if agent_present && lease_ok {
                continue;
            }

            // Mid-commit is the one case that must never be guessed. Somewhere
            // between retiring the original and installing the replacement,
            // only the filesystem knows what happened: requeueing risks
            // encoding over a good replacement, and completing risks calling a
            // lost file a success.
            if job.state == JobState::Committing || job.has_live_intent {
                actions.push(Action::Escalate {
                    job_id: job.job_id.clone(),
                    detail: format!(
                        "agent {} went away mid-commit; the outcome is on disk, not in the database",
                        job.agent_id.as_deref().unwrap_or("(none)")
                    ),
                });
                continue;
            }

            // Everything else was still producing output. Nothing has been
            // installed, so the source is untouched and the work can simply be
            // done again.
            actions.push(Action::ReleaseCapacity {
                job_id: job.job_id.clone(),
            });
            actions.push(Action::Requeue {
                job_id: job.job_id.clone(),
                from: job.state,
                reason: if agent_present {
                    format!(
                        "lease expired at {} and was not renewed",
                        job.lease_expires_unix.unwrap_or(0)
                    )
                } else {
                    format!(
                        "agent {} is no longer connected",
                        job.agent_id.as_deref().unwrap_or("(none)")
                    )
                },
            });
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    fn job(id: &str, state: JobState, agent: Option<&str>, lease: Option<i64>) -> InFlight {
        InFlight {
            job_id: id.into(),
            state,
            agent_id: agent.map(|a| a.to_string()),
            lease_expires_unix: lease,
            has_live_intent: false,
        }
    }

    /// A healthy job is left entirely alone. Reclaiming work from an agent that
    /// is still doing it is how one file gets encoded twice.
    #[test]
    fn a_connected_agent_with_a_current_lease_is_left_alone() {
        let r = Reconciler::new();
        let actions = r.reconcile(
            &[job("j1", JobState::Running, Some("u1"), Some(1000))],
            &connected(&["u1"]),
            900,
        );
        assert!(actions.is_empty());
    }

    /// The grace period is real: an agent a few seconds late is not a dead one.
    #[test]
    fn a_lease_inside_the_grace_period_is_tolerated() {
        let r = Reconciler::with_grace(90);
        let actions = r.reconcile(
            &[job("j1", JobState::Running, Some("u1"), Some(1000))],
            &connected(&["u1"]),
            1050,
        );
        assert!(actions.is_empty(), "50s past a 90s grace must be tolerated");
    }

    #[test]
    fn a_disconnected_agents_work_is_requeued_and_its_capacity_released() {
        let r = Reconciler::new();
        let actions = r.reconcile(
            &[job("j1", JobState::Running, Some("u1"), Some(1000))],
            &connected(&[]),
            900,
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::ReleaseCapacity { .. }))
        );
        match actions.iter().find(|a| matches!(a, Action::Requeue { .. })) {
            Some(Action::Requeue { from, reason, .. }) => {
                assert_eq!(*from, JobState::Running);
                assert!(reason.contains("no longer connected"));
            }
            other => panic!("expected a requeue, got {other:?}"),
        }
    }

    /// Capacity must be released even though the job is being requeued: a slot
    /// held by a job on a vanished agent is a slot the fleet never gets back.
    #[test]
    fn capacity_is_released_before_the_job_is_requeued() {
        let r = Reconciler::new();
        let actions = r.reconcile(
            &[job("j1", JobState::Assigned, Some("u1"), Some(0))],
            &connected(&[]),
            10_000,
        );
        let release = actions
            .iter()
            .position(|a| matches!(a, Action::ReleaseCapacity { .. }));
        let requeue = actions
            .iter()
            .position(|a| matches!(a, Action::Requeue { .. }));
        assert!(release.is_some() && requeue.is_some());
        assert!(release < requeue, "the slot must be freed first");
    }

    /// The case that must never be guessed. Between retiring the original and
    /// installing the replacement, only the filesystem knows what happened.
    #[test]
    fn a_job_lost_mid_commit_is_escalated_never_requeued() {
        let r = Reconciler::new();
        let actions = r.reconcile(
            &[job("j1", JobState::Committing, Some("u1"), Some(0))],
            &connected(&[]),
            10_000,
        );
        assert!(
            actions.iter().all(|a| !matches!(a, Action::Requeue { .. })),
            "requeueing risks encoding over a good replacement"
        );
        assert!(matches!(actions[0], Action::Escalate { .. }));
    }

    /// A live intent means a commit was in progress even if the job row has not
    /// caught up, so the same rule applies.
    #[test]
    fn a_live_commit_intent_escalates_whatever_the_job_state_says() {
        let r = Reconciler::new();
        let mut j = job("j1", JobState::Running, Some("u1"), Some(0));
        j.has_live_intent = true;
        let actions = r.reconcile(&[j], &connected(&[]), 10_000);
        assert!(matches!(actions[0], Action::Escalate { .. }));
        assert!(actions.iter().all(|a| !matches!(a, Action::Requeue { .. })));
    }

    /// Silence is never success. Nothing here may conclude a job finished
    /// simply because nobody said otherwise.
    #[test]
    fn no_path_marks_a_job_succeeded() {
        let r = Reconciler::new();
        let states = [
            JobState::Assigned,
            JobState::Running,
            JobState::Verifying,
            JobState::Committing,
        ];
        for state in states {
            for actions in [
                r.reconcile(
                    &[job("j", state, Some("u1"), Some(0))],
                    &connected(&[]),
                    9999,
                ),
                r.reconcile(
                    &[job("j", state, Some("u1"), Some(0))],
                    &connected(&["u1"]),
                    9999,
                ),
            ] {
                for a in &actions {
                    assert!(
                        matches!(
                            a,
                            Action::Requeue { .. }
                                | Action::Escalate { .. }
                                | Action::ReleaseCapacity { .. }
                        ),
                        "{state:?} produced {a:?}"
                    );
                }
            }
        }
    }

    /// A connected agent whose lease lapsed anyway is still reclaimed — the
    /// stream being open is not evidence that work is progressing.
    #[test]
    fn a_connected_agent_with_an_expired_lease_still_loses_the_job() {
        let r = Reconciler::with_grace(30);
        let actions = r.reconcile(
            &[job("j1", JobState::Running, Some("u1"), Some(1000))],
            &connected(&["u1"]),
            2000,
        );
        match actions.iter().find(|a| matches!(a, Action::Requeue { .. })) {
            Some(Action::Requeue { reason, .. }) => assert!(reason.contains("lease expired")),
            other => panic!("expected a requeue, got {other:?}"),
        }
    }

    /// A job with no agent at all is orphaned and must be reclaimed rather than
    /// left holding a slot forever.
    #[test]
    fn a_job_with_no_agent_is_reclaimed() {
        let r = Reconciler::new();
        let actions = r.reconcile(
            &[job("j1", JobState::Assigned, None, None)],
            &connected(&[]),
            0,
        );
        assert!(actions.iter().any(|a| matches!(a, Action::Requeue { .. })));
    }

    #[test]
    fn an_empty_fleet_produces_no_actions() {
        let r = Reconciler::new();
        assert!(r.reconcile(&[], &connected(&["u1"]), 0).is_empty());
    }
}
