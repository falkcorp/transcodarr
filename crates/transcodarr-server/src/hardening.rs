// file: crates/transcodarr-server/src/hardening.rs
// version: 1.0.0
// guid: 2d5f81c9-6034-47ea-b8d1-9f04e7263a85
// last-edited: 2026-08-03
//! Retry, backoff, dead-lettering, and taking a bad agent out of rotation.
//!
//! The failure this prevents is a loop that looks like progress. A job that
//! fails, retries immediately, fails again and repeats will saturate the fleet
//! while completing nothing — and because every attempt *starts* successfully,
//! the queue looks busy the whole time.
//!
//! Three rules:
//!
//! - **A permanent failure is never retried.** Re-running ffmpeg over a file
//!   with no video stream produces the same error, more slowly.
//! - **Retries back off, and stop.** Exhausted retries dead-letter rather than
//!   failing, because a dead-lettered job is retained for inspection and never
//!   auto-retried — the difference between "we gave up" and "we forgot".
//! - **An agent that fails everything is quarantined.** N consecutive failures
//!   with zero successes means the agent is the problem, and continuing to feed
//!   it converts one broken machine into a fleet-wide outage (flaw B17).

use std::collections::HashMap;

use transcodarr_core::failure::FailureClass;

/// Consecutive failures, with no success between, before an agent is taken out
/// of rotation.
///
/// Five, not three: a genuinely bad agent reaches five quickly, while three is
/// close enough to normal flakiness that a healthy node gets quarantined during
/// an unrelated storage blip.
pub const QUARANTINE_AFTER_CONSECUTIVE_FAILURES: u32 = 5;

/// Base delay for the first retry.
pub const RETRY_BASE_SECONDS: i64 = 30;

/// Ceiling on the backoff.
///
/// An hour. Beyond that a job is effectively abandoned but still nominally
/// queued, which is the worst of both — it neither runs nor shows up as needing
/// attention.
pub const RETRY_MAX_SECONDS: i64 = 3600;

/// What to do about a failed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDecision {
    /// Try again, not before this many seconds from now.
    RetryAfter {
        /// Delay in seconds.
        seconds: i64,
        /// Which attempt this will be.
        next_attempt: i64,
    },
    /// Stop trying, and retain the job for inspection.
    ///
    /// Distinct from failing: a dead-lettered job is never auto-retried and is
    /// kept with its full history, which is the difference between "we gave up
    /// and here is why" and "we forgot".
    DeadLetter {
        /// Why it stopped.
        reason: String,
    },
    /// Stop trying; retrying cannot help.
    Permanent {
        /// Why.
        reason: String,
    },
}

/// Decide what to do after an attempt failed.
///
/// `attempt` is the attempt that just failed, counting from zero.
pub fn decide_retry(class: FailureClass, attempt: i64, max_attempts: i64) -> RetryDecision {
    // Retrying a permanent failure produces the same error, more slowly, and
    // burns a slot doing it.
    //
    // `CapabilityDrift` is deliberately *not* permanent: the job is fine, the
    // server's model of that agent is wrong. It retries elsewhere while the
    // agent is excluded for that requirement — treating it as permanent would
    // throw away a perfectly good job over one lying agent.
    if class == FailureClass::Permanent {
        return RetryDecision::Permanent {
            reason: "the job itself is bad; retrying anywhere fails the same way".to_string(),
        };
    }

    let next_attempt = attempt + 1;
    if next_attempt >= max_attempts {
        return RetryDecision::DeadLetter {
            reason: format!("{max_attempts} attempts exhausted; last failure was {class:?}"),
        };
    }

    // Exponential, capped. Doubling without a ceiling reaches delays longer
    // than anyone waits, and the job is then abandoned in all but name.
    let seconds = RETRY_BASE_SECONDS
        .saturating_mul(1i64.checked_shl(attempt.clamp(0, 16) as u32).unwrap_or(1))
        .min(RETRY_MAX_SECONDS);

    RetryDecision::RetryAfter {
        seconds,
        next_attempt,
    }
}

/// Per-agent health, for deciding when one has become the problem.
#[derive(Debug, Default, Clone)]
pub struct AgentHealth {
    consecutive_failures: HashMap<String, u32>,
    quarantined: HashMap<String, String>,
}

impl AgentHealth {
    /// A fresh tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful job.
    ///
    /// Resets the counter completely. The rule is *consecutive* failures: an
    /// agent that fails four times and then succeeds is flaky, not broken, and
    /// quarantining it would shrink the fleet over normal noise.
    pub fn record_success(&mut self, agent_id: &str) {
        self.consecutive_failures.remove(agent_id);
    }

    /// Record a failed job, and say whether the agent should now be
    /// quarantined.
    ///
    /// An agent failing everything handed to it is the problem, and continuing
    /// to feed it converts one broken machine into a fleet-wide outage: every
    /// job routes to the agent with free slots, which is precisely the one that
    /// finishes nothing.
    pub fn record_failure(&mut self, agent_id: &str, detail: &str) -> bool {
        let n = {
            let entry = self
                .consecutive_failures
                .entry(agent_id.to_string())
                .or_insert(0);
            *entry += 1;
            *entry
        };
        if n >= QUARANTINE_AFTER_CONSECUTIVE_FAILURES && !self.is_quarantined(agent_id) {
            self.quarantined.insert(
                agent_id.to_string(),
                format!("{n} consecutive failures with no success; last: {detail}"),
            );
            return true;
        }
        false
    }

    /// Whether an agent is currently out of rotation.
    pub fn is_quarantined(&self, agent_id: &str) -> bool {
        self.quarantined.contains_key(agent_id)
    }

    /// Why an agent was quarantined.
    pub fn quarantine_reason(&self, agent_id: &str) -> Option<&str> {
        self.quarantined.get(agent_id).map(|s| s.as_str())
    }

    /// How many consecutive failures an agent has accumulated.
    pub fn consecutive_failures(&self, agent_id: &str) -> u32 {
        self.consecutive_failures
            .get(agent_id)
            .copied()
            .unwrap_or(0)
    }

    /// Return an agent to rotation.
    ///
    /// Operator-driven only, and deliberately so. Automatic recovery on a timer
    /// re-enters the loop that caused the quarantine: the agent comes back,
    /// fails five more jobs, and leaves again, having burned five slots each
    /// cycle. A human clearing it means someone has looked (flaw C8).
    pub fn clear_quarantine(&mut self, agent_id: &str) {
        self.quarantined.remove(agent_id);
        self.consecutive_failures.remove(agent_id);
    }

    /// Every quarantined agent, with its reason.
    pub fn quarantined(&self) -> Vec<(&str, &str)> {
        self.quarantined
            .iter()
            .map(|(a, r)| (a.as_str(), r.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Retrying a permanent failure produces the same error, more slowly, while
    /// holding a slot.
    #[test]
    fn a_permanent_failure_is_never_retried() {
        assert!(matches!(
            decide_retry(FailureClass::Permanent, 0, 3),
            RetryDecision::Permanent { .. }
        ));
    }

    /// Capability drift is not the job's fault. The server's model of that
    /// agent is wrong, so the job retries elsewhere while the agent is excluded
    /// -- treating it as permanent throws away a good job over a lying agent.
    #[test]
    fn capability_drift_retries_rather_than_discarding_the_job() {
        assert!(matches!(
            decide_retry(FailureClass::CapabilityDrift, 0, 3),
            RetryDecision::RetryAfter { .. }
        ));
    }

    #[test]
    fn a_transient_failure_is_retried_with_a_delay() {
        match decide_retry(FailureClass::Transient, 0, 3) {
            RetryDecision::RetryAfter {
                seconds,
                next_attempt,
            } => {
                assert_eq!(seconds, RETRY_BASE_SECONDS);
                assert_eq!(next_attempt, 1);
            }
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    /// Backoff grows, so a persistent problem is not hammered at a fixed rate.
    #[test]
    fn backoff_grows_with_each_attempt() {
        let mut last = 0;
        for attempt in 0..4 {
            match decide_retry(FailureClass::Transient, attempt, 100) {
                RetryDecision::RetryAfter { seconds, .. } => {
                    assert!(seconds > last, "attempt {attempt} did not back off further");
                    last = seconds;
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// ...but is capped. Beyond an hour a job neither runs nor shows up as
    /// needing attention, which is the worst of both.
    #[test]
    fn backoff_is_capped() {
        match decide_retry(FailureClass::Transient, 30, 100) {
            RetryDecision::RetryAfter { seconds, .. } => {
                assert_eq!(seconds, RETRY_MAX_SECONDS);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Exhausted retries dead-letter rather than failing. A dead-lettered job
    /// is retained and never auto-retried -- "we gave up and here is why"
    /// rather than "we forgot".
    #[test]
    fn exhausted_retries_dead_letter_rather_than_failing() {
        match decide_retry(FailureClass::Transient, 2, 3) {
            RetryDecision::DeadLetter { reason } => {
                assert!(reason.contains("exhausted"));
            }
            other => panic!("expected a dead letter, got {other:?}"),
        }
    }

    #[test]
    fn a_single_attempt_limit_dead_letters_immediately() {
        assert!(matches!(
            decide_retry(FailureClass::Transient, 0, 1),
            RetryDecision::DeadLetter { .. }
        ));
    }

    /// An agent failing everything is the problem. Continuing to feed it turns
    /// one broken machine into a fleet-wide outage, because every job routes to
    /// the agent with free slots -- which is precisely the one finishing
    /// nothing.
    #[test]
    fn an_agent_failing_everything_is_quarantined() {
        let mut h = AgentHealth::new();
        for i in 1..QUARANTINE_AFTER_CONSECUTIVE_FAILURES {
            assert!(!h.record_failure("u1", "boom"), "too early at {i}");
            assert!(!h.is_quarantined("u1"));
        }
        assert!(h.record_failure("u1", "boom"), "the threshold must fire");
        assert!(h.is_quarantined("u1"));
        assert!(h.quarantine_reason("u1").unwrap().contains("consecutive"));
    }

    /// Consecutive is the operative word. An agent that fails four times and
    /// then succeeds is flaky, not broken, and quarantining it shrinks the
    /// fleet over normal noise.
    #[test]
    fn a_success_resets_the_failure_streak() {
        let mut h = AgentHealth::new();
        for _ in 0..(QUARANTINE_AFTER_CONSECUTIVE_FAILURES - 1) {
            h.record_failure("u1", "boom");
        }
        h.record_success("u1");
        assert_eq!(h.consecutive_failures("u1"), 0);
        assert!(!h.record_failure("u1", "boom"), "the streak restarted");
        assert!(!h.is_quarantined("u1"));
    }

    /// One agent's failures must not take out another. Fleet-wide quarantine
    /// from a per-agent fault is the outage this guard exists to prevent.
    #[test]
    fn quarantine_is_per_agent() {
        let mut h = AgentHealth::new();
        for _ in 0..QUARANTINE_AFTER_CONSECUTIVE_FAILURES {
            h.record_failure("u1", "boom");
        }
        assert!(h.is_quarantined("u1"));
        assert!(!h.is_quarantined("u2"));
        assert_eq!(h.quarantined().len(), 1);
    }

    /// Automatic recovery on a timer re-enters the loop that caused the
    /// quarantine: back, five more failures, out again, five slots burned each
    /// cycle. A human clearing it means someone has actually looked.
    #[test]
    fn a_quarantine_clears_only_when_an_operator_clears_it() {
        let mut h = AgentHealth::new();
        for _ in 0..QUARANTINE_AFTER_CONSECUTIVE_FAILURES {
            h.record_failure("u1", "boom");
        }
        assert!(h.is_quarantined("u1"));

        // More failures do not un-quarantine, and do not re-report.
        assert!(!h.record_failure("u1", "boom"), "already quarantined");
        assert!(h.is_quarantined("u1"));

        h.clear_quarantine("u1");
        assert!(!h.is_quarantined("u1"));
        assert_eq!(h.consecutive_failures("u1"), 0, "the streak resets too");
    }

    /// The reason is carried, because "agent quarantined" without one sends an
    /// operator hunting through logs for which failure mattered.
    #[test]
    fn the_quarantine_reason_names_the_last_failure() {
        let mut h = AgentHealth::new();
        for _ in 0..QUARANTINE_AFTER_CONSECUTIVE_FAILURES {
            h.record_failure("u1", "ffmpeg exited 69");
        }
        assert!(
            h.quarantine_reason("u1").unwrap().contains("exited 69"),
            "reason: {:?}",
            h.quarantine_reason("u1")
        );
    }

    #[test]
    fn a_healthy_agent_is_never_quarantined() {
        let mut h = AgentHealth::new();
        for _ in 0..50 {
            h.record_success("u1");
        }
        assert!(!h.is_quarantined("u1"));
        assert!(h.quarantined().is_empty());
    }
}
