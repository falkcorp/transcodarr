// file: crates/transcodarr-server/src/metrics.rs
// version: 1.0.0
// guid: 6f2c08e5-4b71-49a3-90d8-15ea736c204b
// last-edited: 2026-08-03
//! Metric names, as constants rather than string literals at call sites.
//!
//! A metric is an interface. Once a dashboard or an alert rule references
//! `transcodarr_jobs_total`, a typo at one call site does not fail anything —
//! it produces a second, near-identical series that nobody is watching, and the
//! alert stays quiet through exactly the incident it was written for.
//!
//! So the names live here once. A rename is then a compile error at every
//! emitter, which is the only way a rename can be safe.
//!
//! Label *values* that come from an enum are rendered through that enum's
//! `as_str`, never formatted ad hoc, for the same reason: `{:?}` on a state
//! produces `NeedsOperator` today and something else the moment someone adds a
//! `#[serde(rename)]`.

use transcodarr_core::job::{JobClass, JobState};

/// Jobs by terminal outcome.
pub const JOBS_TOTAL: &str = "transcodarr_jobs_total";
/// Time from dispatch to terminal state.
pub const JOB_DURATION_SECONDS: &str = "transcodarr_job_duration_seconds";
/// Jobs currently in each state.
pub const JOBS_IN_STATE: &str = "transcodarr_jobs_in_state";
/// Files discovered by a scan.
pub const SCAN_FILES_SEEN: &str = "transcodarr_scan_files_seen_total";
/// How long a scan took.
pub const SCAN_DURATION_SECONDS: &str = "transcodarr_scan_duration_seconds";
/// Scans abandoned by the mass-missing guard.
///
/// Worth alerting on rather than counting: it fires when a library looks
/// deleted, which is almost always a mount problem an operator must fix.
pub const SCAN_ABORTED_TOTAL: &str = "transcodarr_scan_aborted_total";
/// How long a full policy evaluation pass took.
///
/// The Phase 2 claim lives here: a policy edit must re-derive every decision in
/// seconds with no filesystem I/O, and this is what proves it still does.
pub const POLICY_EVAL_DURATION_SECONDS: &str = "transcodarr_policy_eval_duration_seconds";
/// Files probed.
pub const PROBE_TOTAL: &str = "transcodarr_probe_total";
/// Outputs rejected, by the gate that rejected them.
pub const VALIDATION_FAILED_TOTAL: &str = "transcodarr_validation_failed_total";
/// Commit intents resolved after an interruption, by resolution.
///
/// Every arm of the crash matrix reports here. A nonzero `needs_operator` is
/// the one that must page someone.
pub const COMMIT_INTENT_RECOVERED_TOTAL: &str = "transcodarr_commit_intent_recovered_total";
/// Installs by resolution.
pub const COMMIT_TOTAL: &str = "transcodarr_commit_total";
/// Bytes reclaimed, measured from ZFS accounting.
///
/// Never from file sizes: deleting a 40 GB original reclaims nothing while a
/// snapshot still references its blocks, and a size-derived figure reports
/// progress that did not happen.
pub const BYTES_RECLAIMED_TOTAL: &str = "transcodarr_bytes_reclaimed_total";
/// Slots in use per agent.
pub const AGENT_SLOTS_IN_USE: &str = "transcodarr_agent_slots_in_use";
/// Agents by status.
pub const AGENTS_CONNECTED: &str = "transcodarr_agents_connected";
/// Jobs an agent refused after re-validating the requirements.
///
/// Alarmed as a server bug, never absorbed as a routine retry: it means the
/// server's model of that agent is stale, and the next dispatch will make the
/// same mistake.
pub const AGENT_REJECTIONS_TOTAL: &str = "transcodarr_agent_rejections_total";
/// Jobs that did not dispatch, by blocking stage.
pub const DISPATCH_BLOCKED: &str = "transcodarr_dispatch_blocked";
/// How long a dispatch pass took.
pub const DISPATCH_DURATION_SECONDS: &str = "transcodarr_dispatch_duration_seconds";
/// Writer queue depth, by lane.
pub const WRITER_QUEUE_DEPTH: &str = "transcodarr_writer_queue_depth";
/// Write operations that failed enough times to be reported as poisoned.
pub const WRITER_POISONED: &str = "transcodarr_writer_poisoned";
/// Trash entries retained, and their bytes.
pub const TRASH_RETAINED: &str = "transcodarr_trash_retained";

/// Every metric this build emits.
///
/// Exists so a test can assert the naming convention over the whole set at
/// once. A convention checked only by review drifts the first time someone is
/// in a hurry.
pub const ALL_METRICS: &[&str] = &[
    JOBS_TOTAL,
    JOB_DURATION_SECONDS,
    JOBS_IN_STATE,
    SCAN_FILES_SEEN,
    SCAN_DURATION_SECONDS,
    SCAN_ABORTED_TOTAL,
    POLICY_EVAL_DURATION_SECONDS,
    PROBE_TOTAL,
    VALIDATION_FAILED_TOTAL,
    COMMIT_INTENT_RECOVERED_TOTAL,
    COMMIT_TOTAL,
    BYTES_RECLAIMED_TOTAL,
    AGENT_SLOTS_IN_USE,
    AGENTS_CONNECTED,
    AGENT_REJECTIONS_TOTAL,
    DISPATCH_BLOCKED,
    DISPATCH_DURATION_SECONDS,
    WRITER_QUEUE_DEPTH,
    WRITER_POISONED,
    TRASH_RETAINED,
];

/// The label value for a job state.
///
/// Goes through `as_str` rather than `{:?}`, so a series name cannot change
/// because someone added a derive.
pub fn state_label(state: JobState) -> &'static str {
    state.as_str()
}

/// The label value for a job class.
pub fn class_label(class: JobClass) -> &'static str {
    class.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Prometheus convention, and the reason dashboards written against one
    /// deployment work against another.
    #[test]
    fn every_metric_is_prefixed_and_lowercase() {
        for m in ALL_METRICS {
            assert!(m.starts_with("transcodarr_"), "{m} lacks the prefix");
            assert_eq!(*m, m.to_ascii_lowercase(), "{m} is not lowercase");
            assert!(
                !m.contains(' ') && !m.contains('-'),
                "{m} has a character Prometheus will not accept"
            );
        }
    }

    /// A duplicate name means two different things reporting into one series,
    /// which is worse than either being missing: the numbers add up and mean
    /// nothing.
    #[test]
    fn no_two_metrics_share_a_name() {
        let unique: HashSet<_> = ALL_METRICS.iter().collect();
        assert_eq!(
            unique.len(),
            ALL_METRICS.len(),
            "a duplicated metric name silently merges two series"
        );
    }

    /// Counters end `_total`, durations end `_seconds`. An alert rule written
    /// against `_total` on something that turns out to be a gauge is wrong in a
    /// way nobody notices until it matters.
    #[test]
    fn counters_and_durations_follow_the_suffix_convention() {
        for m in ALL_METRICS {
            if m.contains("duration") {
                assert!(
                    m.ends_with("_seconds"),
                    "{m} is a duration and must end _seconds"
                );
            }
        }
        assert!(JOBS_TOTAL.ends_with("_total"));
        assert!(COMMIT_INTENT_RECOVERED_TOTAL.ends_with("_total"));
        assert!(BYTES_RECLAIMED_TOTAL.ends_with("_total"));
    }

    /// Label values come from the enums, so a series cannot be renamed by
    /// someone adding a derive.
    #[test]
    fn label_values_come_from_the_domain_enums() {
        assert_eq!(state_label(JobState::NeedsOperator), "NeedsOperator");
        assert_eq!(class_label(JobClass::VideoGpu), "VideoGpu");
    }

    /// Every state has a label, so a queue-depth gauge cannot silently omit
    /// one -- and the state nobody thought to include is invariably the one
    /// that matters during an incident.
    #[test]
    fn every_job_state_has_a_label() {
        for s in [
            JobState::Pending,
            JobState::Blocked,
            JobState::Eligible,
            JobState::Assigned,
            JobState::Running,
            JobState::Verifying,
            JobState::Committing,
            JobState::Retrying,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Cancelled,
            JobState::DeadLettered,
            JobState::NeedsOperator,
        ] {
            assert!(!state_label(s).is_empty(), "{s:?} has no label");
        }
    }

    /// The metrics that must exist for the documented alerts to be writable at
    /// all. Named individually so deleting one is a test failure rather than a
    /// silently broken alert rule.
    #[test]
    fn the_alertable_metrics_exist() {
        for m in [
            COMMIT_INTENT_RECOVERED_TOTAL,
            SCAN_ABORTED_TOTAL,
            AGENT_REJECTIONS_TOTAL,
            WRITER_POISONED,
        ] {
            assert!(ALL_METRICS.contains(&m), "{m} is not registered");
        }
    }
}
