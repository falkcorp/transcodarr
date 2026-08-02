// file: crates/transcodarr-core/src/job.rs
// version: 1.0.0
// guid: 9e17b3c0-6d42-4a85-b2f9-0c73e5a18ff6
// last-edited: 2026-08-02
//! Job identity and the state machine.
//!
//! Transitions are checked rather than assumed. Every state change in the store
//! goes through [`JobState::can_transition`], so an impossible edge is a
//! rejected write instead of a row that quietly contradicts its own history.

use serde::{Deserialize, Serialize};

use crate::capability::Requirements;
use crate::facts::SizeBucket;

/// What kind of work a job performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum JobClass {
    /// Audio-only remux or re-encode; video is copied.
    Audio,
    /// Video encode on a GPU.
    VideoGpu,
    /// Video encode on CPU.
    VideoCpu,
    /// Probe only.
    Probe,
    /// Verification pass.
    Verify,
}

/// Where a job is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum JobState {
    /// Created, not yet considered.
    Pending,
    /// Considered and currently unroutable — no agent can satisfy it.
    Blocked,
    /// Ready to dispatch.
    Eligible,
    /// Handed to an agent, not yet started.
    Assigned,
    /// ffmpeg is running.
    Running,
    /// Output is being validated.
    Verifying,
    /// The replace ritual is underway.
    Committing,
    /// Failed transiently; will be retried.
    Retrying,
    /// Done.
    Succeeded,
    /// Failed, retries exhausted or permanent.
    Failed,
    /// Cancelled by an operator.
    Cancelled,
    /// Retries exhausted; retained for inspection, never auto-retried.
    DeadLettered,
    /// Ambiguous outcome a human must resolve. Never auto-resolved.
    NeedsOperator,
}

impl JobState {
    /// Whether this state is terminal.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Succeeded
                | JobState::Failed
                | JobState::Cancelled
                | JobState::DeadLettered
                | JobState::NeedsOperator
        )
    }

    /// Whether a job in this state holds capacity on an agent.
    ///
    /// Capacity is released when a job *leaves this set* — not when it reaches
    /// a terminal state. A job going to `Retrying` has stopped using its slot,
    /// and holding the grant until it eventually terminates leaks capacity
    /// until the fleet deadlocks.
    pub fn holds_capacity(self) -> bool {
        matches!(
            self,
            JobState::Assigned | JobState::Running | JobState::Verifying | JobState::Committing
        )
    }

    /// Whether `from -> to` is a legal transition.
    pub fn can_transition(from: JobState, to: JobState) -> bool {
        use JobState::*;
        if from.is_terminal() {
            // Terminal rows are immutable. An operator retry inserts a *new*
            // job with parent_job_id set rather than reanimating this one.
            return false;
        }
        match (from, to) {
            // Cancellation can interrupt anything not yet terminal.
            (_, Cancelled) => true,
            // Anything in flight can hit an ambiguous commit.
            (Committing, NeedsOperator) => true,

            (Pending, Blocked | Eligible) => true,
            // Routability is re-evaluated as the fleet changes, in both
            // directions: a new agent unblocks work, a departing one blocks it.
            (Blocked, Eligible) => true,
            (Eligible, Blocked) => true,
            (Eligible, Assigned) => true,
            (Assigned, Running) => true,
            // An agent may reject an assignment it re-validates as unsuitable.
            (Assigned, Retrying | Failed) => true,
            (Running, Verifying | Retrying | Failed) => true,
            (Verifying, Committing | Retrying | Failed) => true,
            (Committing, Succeeded | Retrying | Failed) => true,
            (Retrying, Eligible | Blocked | DeadLettered | Failed) => true,
            _ => false,
        }
    }
}

/// A unit of work, before it is persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    /// What kind of work.
    pub class: JobClass,
    /// Size band, used to partition queues.
    pub size_bucket: SizeBucket,
    /// What an agent must provide to run this.
    pub requirements: Requirements,
    /// Signature of the facts this job was planned from. The agent aborts if
    /// the source no longer matches.
    pub expected_content_sig: String,
}

#[cfg(test)]
mod tests {
    use super::JobState::*;
    use super::*;

    #[test]
    fn the_happy_path_is_legal_end_to_end() {
        let path = [
            Pending, Eligible, Assigned, Running, Verifying, Committing, Succeeded,
        ];
        for w in path.windows(2) {
            assert!(
                JobState::can_transition(w[0], w[1]),
                "{:?} -> {:?} should be legal",
                w[0],
                w[1]
            );
        }
    }

    /// Terminal rows are immutable; an operator retry inserts a new job rather
    /// than reanimating the old one, so its history stays intact.
    #[test]
    fn terminal_states_are_immutable() {
        for t in [Succeeded, Failed, Cancelled, DeadLettered, NeedsOperator] {
            assert!(t.is_terminal());
            for to in [Eligible, Running, Pending, Assigned] {
                assert!(
                    !JobState::can_transition(t, to),
                    "{t:?} -> {to:?} must be rejected"
                );
            }
        }
    }

    /// Both directions matter: a new agent unblocks work, and an agent leaving
    /// blocks work that was previously routable.
    #[test]
    fn routability_can_change_in_both_directions() {
        assert!(JobState::can_transition(Blocked, Eligible));
        assert!(JobState::can_transition(Eligible, Blocked));
    }

    /// Capacity is released on leaving the admitted set, not on reaching a
    /// terminal state. Holding a grant through `Retrying` leaks slots until the
    /// fleet deadlocks.
    #[test]
    fn retrying_does_not_hold_capacity() {
        assert!(Running.holds_capacity());
        assert!(Committing.holds_capacity());
        assert!(!Retrying.holds_capacity());
        assert!(!Eligible.holds_capacity());
        assert!(!Succeeded.holds_capacity());
    }

    #[test]
    fn an_ambiguous_commit_goes_to_needs_operator_not_a_guess() {
        assert!(JobState::can_transition(Committing, NeedsOperator));
        assert!(NeedsOperator.is_terminal());
    }

    #[test]
    fn cancellation_interrupts_anything_not_yet_terminal() {
        for s in [
            Pending, Blocked, Eligible, Assigned, Running, Verifying, Retrying,
        ] {
            assert!(JobState::can_transition(s, Cancelled), "{s:?}");
        }
    }

    #[test]
    fn nonsense_transitions_are_rejected() {
        assert!(!JobState::can_transition(Pending, Running));
        assert!(!JobState::can_transition(Pending, Succeeded));
        assert!(!JobState::can_transition(Eligible, Committing));
        assert!(!JobState::can_transition(Running, Succeeded));
    }

    #[test]
    fn only_retrying_can_dead_letter() {
        assert!(JobState::can_transition(Retrying, DeadLettered));
        assert!(!JobState::can_transition(Running, DeadLettered));
        assert!(!JobState::can_transition(Eligible, DeadLettered));
    }
}
