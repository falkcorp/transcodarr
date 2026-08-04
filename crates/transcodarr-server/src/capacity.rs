// file: crates/transcodarr-server/src/capacity.rs
// version: 1.0.0
// guid: c74e1b05-9d38-4a26-8f71-25e08c3b6d94
// last-edited: 2026-08-03
//! Who is allowed to run what, and how many at once.
//!
//! Three properties, each with a specific failure it prevents:
//!
//! - **Permits are acquired all-or-nothing.** A job needing a GPU slot *and* a
//!   large-file slot must take both or neither. Taking one and blocking on the
//!   second holds capacity nobody can use, and two such jobs holding each
//!   other's missing half is a deadlock with no timeout to break it.
//! - **Capacity is released when a job leaves the admitted set, not when it
//!   reaches a terminal state.** A job going to `Retrying` has stopped using
//!   its slot. Holding the grant until it eventually terminates leaks a slot
//!   per retry, and a fleet that retries enough deadlocks with every agent
//!   nominally full and nothing running.
//! - **The ledger is rebuilt from the database before the first dispatch.** A
//!   server that restarts mid-flight and starts from zero will cheerfully
//!   double-book every agent that is still working, because the jobs holding
//!   those slots are in the database, not in memory.

use std::collections::HashMap;

use transcodarr_core::facts::SizeBucket;
use transcodarr_core::job::{JobClass, JobState};

/// What an agent can run at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLimits {
    /// Total concurrent jobs.
    pub total_slots: u32,
    /// Concurrent jobs per class.
    pub per_class: HashMap<JobClass, u32>,
    /// Concurrent jobs in the `Large` size band.
    ///
    /// Its own cap because the pool is latency-bound: 47 concurrent 40-80 GB
    /// jobs produced per-file ETAs of 3-34 hours. Large files starve everything
    /// else unless they are limited separately from the total.
    pub large_slots: u32,
}

impl AgentLimits {
    /// A simple limit with no per-class distinction.
    pub fn flat(total: u32, large: u32) -> Self {
        Self {
            total_slots: total,
            per_class: HashMap::new(),
            large_slots: large,
        }
    }

    fn class_limit(&self, class: JobClass) -> u32 {
        self.per_class.get(&class).copied().unwrap_or(u32::MAX)
    }
}

/// What one job is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// Which class it occupies.
    pub class: JobClass,
    /// Which size band.
    pub size_bucket: SizeBucket,
}

/// Why a job could not be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// No agent by that id.
    UnknownAgent,
    /// The agent is at its total limit.
    TotalSlots,
    /// The agent is at its limit for that class.
    ClassSlots,
    /// The agent is at its large-file limit.
    LargeSlots,
    /// That job already holds a grant here.
    AlreadyHeld,
}

impl Refusal {
    /// The label recorded on a `dispatch_block` row.
    pub fn label(self) -> &'static str {
        match self {
            Refusal::UnknownAgent => "unknown_agent",
            Refusal::TotalSlots => "capacity_total",
            Refusal::ClassSlots => "capacity_class",
            Refusal::LargeSlots => "capacity_large",
            Refusal::AlreadyHeld => "already_held",
        }
    }
}

/// Whether a job in this state is occupying a slot.
///
/// Defers to [`JobState::holds_capacity`] rather than re-deriving the set, so
/// the ledger and the state machine cannot come to disagree about what
/// "running" means — which is exactly how a slot leak starts.
pub fn occupies_slot(state: JobState) -> bool {
    state.holds_capacity()
}

/// Live capacity accounting for the fleet.
#[derive(Debug, Default)]
pub struct CapacityLedger {
    limits: HashMap<String, AgentLimits>,
    /// agent -> job -> what it holds.
    held: HashMap<String, HashMap<String, Grant>>,
}

impl CapacityLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or update an agent's limits.
    ///
    /// Lowering a limit below what is already held does **not** evict anything.
    /// Running work is not made safe by revoking its slot retroactively; the
    /// ledger simply refuses new admissions until usage falls back under.
    pub fn set_limits(&mut self, agent_id: &str, limits: AgentLimits) {
        self.limits.insert(agent_id.to_string(), limits);
        self.held.entry(agent_id.to_string()).or_default();
    }

    /// Forget an agent entirely.
    ///
    /// Its grants go with it: an agent that is gone is not holding anything,
    /// and keeping its rows would permanently shrink the fleet.
    pub fn remove_agent(&mut self, agent_id: &str) {
        self.limits.remove(agent_id);
        self.held.remove(agent_id);
    }

    /// How many jobs an agent is currently holding.
    pub fn in_flight(&self, agent_id: &str) -> u32 {
        self.held.get(agent_id).map(|m| m.len() as u32).unwrap_or(0)
    }

    /// How many of them are in the `Large` band.
    pub fn large_in_flight(&self, agent_id: &str) -> u32 {
        self.held
            .get(agent_id)
            .map(|m| {
                m.values()
                    .filter(|g| g.size_bucket == SizeBucket::Large)
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// How many of them are of a given class.
    pub fn class_in_flight(&self, agent_id: &str, class: JobClass) -> u32 {
        self.held
            .get(agent_id)
            .map(|m| m.values().filter(|g| g.class == class).count() as u32)
            .unwrap_or(0)
    }

    /// Whether a job is holding a grant anywhere.
    pub fn holder_of(&self, job_id: &str) -> Option<&str> {
        self.held
            .iter()
            .find(|(_, jobs)| jobs.contains_key(job_id))
            .map(|(agent, _)| agent.as_str())
    }

    /// Take every permit a job needs, or none of them.
    ///
    /// All-or-nothing is the point. Taking the total slot and then failing on
    /// the large-file slot would hold capacity nobody can use, and two jobs
    /// each holding the other's missing half deadlock with no timeout to break
    /// them.
    pub fn try_acquire(
        &mut self,
        agent_id: &str,
        job_id: &str,
        grant: Grant,
    ) -> Result<(), Refusal> {
        let Some(limits) = self.limits.get(agent_id) else {
            return Err(Refusal::UnknownAgent);
        };
        if self.holder_of(job_id).is_some() {
            return Err(Refusal::AlreadyHeld);
        }
        if self.in_flight(agent_id) >= limits.total_slots {
            return Err(Refusal::TotalSlots);
        }
        if self.class_in_flight(agent_id, grant.class) >= limits.class_limit(grant.class) {
            return Err(Refusal::ClassSlots);
        }
        if grant.size_bucket == SizeBucket::Large
            && self.large_in_flight(agent_id) >= limits.large_slots
        {
            return Err(Refusal::LargeSlots);
        }

        // Every check has passed, so the whole set is taken at once.
        self.held
            .entry(agent_id.to_string())
            .or_default()
            .insert(job_id.to_string(), grant);
        Ok(())
    }

    /// Release whatever a job holds, wherever it holds it.
    ///
    /// Idempotent: releasing a job that holds nothing is a no-op, because the
    /// caller is often a state transition that may fire more than once.
    pub fn release(&mut self, job_id: &str) -> bool {
        for jobs in self.held.values_mut() {
            if jobs.remove(job_id).is_some() {
                return true;
            }
        }
        false
    }

    /// Apply a state transition's effect on capacity.
    ///
    /// The rule that prevents the deadlock: capacity is released when a job
    /// *leaves the admitted set*, not when it reaches a terminal state. A job
    /// moving to `Retrying` has stopped using its slot, and holding the grant
    /// until it eventually terminates leaks one slot per retry.
    pub fn on_transition(&mut self, job_id: &str, from: JobState, to: JobState) {
        if occupies_slot(from) && !occupies_slot(to) {
            self.release(job_id);
        }
    }

    /// Rebuild from the jobs the database says are in flight.
    ///
    /// Run before the first dispatch pass. A server that restarts and starts
    /// from an empty ledger will double-book every agent still working, because
    /// the jobs holding those slots live in the database, not in memory.
    ///
    /// Jobs whose agent is unknown are skipped and returned, rather than
    /// silently dropped: they are the ones an operator needs to see, since
    /// something is running that the fleet does not account for.
    pub fn rebuild(
        &mut self,
        in_flight: impl IntoIterator<Item = (String, String, Grant, JobState)>,
    ) -> Vec<String> {
        for jobs in self.held.values_mut() {
            jobs.clear();
        }
        let mut orphaned = Vec::new();
        for (agent_id, job_id, grant, state) in in_flight {
            if !occupies_slot(state) {
                continue;
            }
            match self.held.get_mut(&agent_id) {
                Some(jobs) => {
                    jobs.insert(job_id, grant);
                }
                None => orphaned.push(job_id),
            }
        }
        orphaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(bucket: SizeBucket) -> Grant {
        Grant {
            class: JobClass::Audio,
            size_bucket: bucket,
        }
    }

    fn ledger() -> CapacityLedger {
        let mut l = CapacityLedger::new();
        l.set_limits("u1", AgentLimits::flat(4, 1));
        l
    }

    #[test]
    fn permits_are_taken_up_to_the_limit_and_no_further() {
        let mut l = ledger();
        for i in 0..4 {
            l.try_acquire("u1", &format!("j{i}"), audio(SizeBucket::Small))
                .unwrap();
        }
        assert_eq!(l.in_flight("u1"), 4);
        assert_eq!(
            l.try_acquire("u1", "j5", audio(SizeBucket::Small)),
            Err(Refusal::TotalSlots)
        );
    }

    /// The large band has its own cap because the pool is latency-bound: 47
    /// concurrent 40-80 GB jobs produced per-file ETAs of 3-34 hours.
    #[test]
    fn large_files_are_capped_separately_from_the_total() {
        let mut l = ledger();
        l.try_acquire("u1", "big", audio(SizeBucket::Large))
            .unwrap();
        assert_eq!(
            l.try_acquire("u1", "big2", audio(SizeBucket::Large)),
            Err(Refusal::LargeSlots),
            "a second large job must be refused even though 3 total slots remain"
        );
        // ...but a small job still fits.
        l.try_acquire("u1", "small", audio(SizeBucket::Small))
            .unwrap();
        assert_eq!(l.in_flight("u1"), 2);
    }

    /// A refused acquisition must leave the ledger untouched. Taking the total
    /// slot and then failing on the large-file slot would hold capacity nobody
    /// can use.
    #[test]
    fn a_refused_acquisition_holds_nothing() {
        let mut l = ledger();
        l.try_acquire("u1", "big", audio(SizeBucket::Large))
            .unwrap();
        let before = l.in_flight("u1");

        assert!(
            l.try_acquire("u1", "big2", audio(SizeBucket::Large))
                .is_err()
        );
        assert_eq!(l.in_flight("u1"), before, "no partial hold may remain");
        assert!(l.holder_of("big2").is_none());
    }

    #[test]
    fn per_class_limits_bind_independently() {
        let mut l = CapacityLedger::new();
        let mut per_class = HashMap::new();
        per_class.insert(JobClass::VideoGpu, 1);
        l.set_limits(
            "gpu",
            AgentLimits {
                total_slots: 8,
                per_class,
                large_slots: 8,
            },
        );

        let gpu = Grant {
            class: JobClass::VideoGpu,
            size_bucket: SizeBucket::Small,
        };
        l.try_acquire("gpu", "g1", gpu).unwrap();
        assert_eq!(l.try_acquire("gpu", "g2", gpu), Err(Refusal::ClassSlots));
        // A different class is unaffected.
        l.try_acquire("gpu", "a1", audio(SizeBucket::Small))
            .unwrap();
    }

    /// The deadlock this exists to prevent. A job going to `Retrying` has
    /// stopped using its slot; holding the grant until it eventually terminates
    /// leaks one slot per retry until the fleet is nominally full and idle.
    #[test]
    fn retrying_releases_capacity_rather_than_leaking_it() {
        let mut l = ledger();
        l.try_acquire("u1", "j1", audio(SizeBucket::Small)).unwrap();
        assert_eq!(l.in_flight("u1"), 1);

        l.on_transition("j1", JobState::Running, JobState::Retrying);
        assert_eq!(l.in_flight("u1"), 0, "a retrying job is not using its slot");
    }

    #[test]
    fn reaching_a_terminal_state_also_releases() {
        let mut l = ledger();
        l.try_acquire("u1", "j1", audio(SizeBucket::Small)).unwrap();
        l.on_transition("j1", JobState::Committing, JobState::Succeeded);
        assert_eq!(l.in_flight("u1"), 0);
    }

    /// Moving *within* the admitted set changes nothing. Releasing on every
    /// transition would free a slot the job is still using.
    #[test]
    fn moving_within_the_admitted_set_holds_the_slot() {
        let mut l = ledger();
        l.try_acquire("u1", "j1", audio(SizeBucket::Small)).unwrap();
        for (from, to) in [
            (JobState::Assigned, JobState::Running),
            (JobState::Running, JobState::Verifying),
            (JobState::Verifying, JobState::Committing),
        ] {
            l.on_transition("j1", from, to);
            assert_eq!(
                l.in_flight("u1"),
                1,
                "{from:?} -> {to:?} must keep the slot"
            );
        }
    }

    /// A transition that never held capacity must not release someone else's.
    #[test]
    fn a_transition_outside_the_admitted_set_releases_nothing() {
        let mut l = ledger();
        l.try_acquire("u1", "j1", audio(SizeBucket::Small)).unwrap();
        l.on_transition("j1", JobState::Pending, JobState::Eligible);
        assert_eq!(l.in_flight("u1"), 1);
    }

    #[test]
    fn a_job_cannot_hold_two_grants() {
        let mut l = ledger();
        l.try_acquire("u1", "j1", audio(SizeBucket::Small)).unwrap();
        assert_eq!(
            l.try_acquire("u1", "j1", audio(SizeBucket::Small)),
            Err(Refusal::AlreadyHeld)
        );
        assert_eq!(l.in_flight("u1"), 1);
    }

    #[test]
    fn releasing_is_idempotent() {
        let mut l = ledger();
        l.try_acquire("u1", "j1", audio(SizeBucket::Small)).unwrap();
        assert!(l.release("j1"));
        assert!(!l.release("j1"), "a second release is a no-op, not a panic");
        assert!(!l.release("never-existed"));
    }

    #[test]
    fn an_unknown_agent_is_refused_by_name() {
        let mut l = ledger();
        assert_eq!(
            l.try_acquire("nope", "j1", audio(SizeBucket::Small)),
            Err(Refusal::UnknownAgent)
        );
    }

    /// An agent that is gone is not holding anything. Keeping its rows would
    /// permanently shrink the fleet.
    #[test]
    fn removing_an_agent_frees_what_it_held() {
        let mut l = ledger();
        l.try_acquire("u1", "j1", audio(SizeBucket::Small)).unwrap();
        l.remove_agent("u1");
        assert_eq!(l.in_flight("u1"), 0);
        assert!(l.holder_of("j1").is_none());
    }

    /// The restart case. Starting from an empty ledger double-books every agent
    /// still working, because the jobs holding those slots are in the database.
    #[test]
    fn rebuilding_from_the_database_restores_in_flight_grants() {
        let mut l = ledger();
        let orphans = l.rebuild(vec![
            (
                "u1".into(),
                "j1".into(),
                audio(SizeBucket::Small),
                JobState::Running,
            ),
            (
                "u1".into(),
                "j2".into(),
                audio(SizeBucket::Large),
                JobState::Committing,
            ),
            // Not in flight: must not consume a slot.
            (
                "u1".into(),
                "j3".into(),
                audio(SizeBucket::Small),
                JobState::Succeeded,
            ),
        ]);
        assert!(orphans.is_empty());
        assert_eq!(l.in_flight("u1"), 2);
        assert_eq!(l.large_in_flight("u1"), 1);
        assert_eq!(
            l.try_acquire("u1", "j4", audio(SizeBucket::Large)),
            Err(Refusal::LargeSlots),
            "the rebuilt large grant must still bind"
        );
    }

    /// A job running on an agent the fleet does not know about is exactly what
    /// an operator needs told. Dropping it silently is how a mystery starts.
    #[test]
    fn rebuilding_reports_jobs_whose_agent_is_unknown() {
        let mut l = ledger();
        let orphans = l.rebuild(vec![(
            "ghost".into(),
            "j9".into(),
            audio(SizeBucket::Small),
            JobState::Running,
        )]);
        assert_eq!(orphans, vec!["j9"]);
        assert_eq!(l.in_flight("u1"), 0);
    }

    #[test]
    fn rebuilding_replaces_rather_than_accumulates() {
        let mut l = ledger();
        l.try_acquire("u1", "stale", audio(SizeBucket::Small))
            .unwrap();
        l.rebuild(vec![(
            "u1".into(),
            "fresh".into(),
            audio(SizeBucket::Small),
            JobState::Running,
        )]);
        assert_eq!(l.in_flight("u1"), 1);
        assert!(l.holder_of("stale").is_none());
        assert_eq!(l.holder_of("fresh"), Some("u1"));
    }

    /// Running work is not made safe by revoking its slot retroactively. The
    /// ledger refuses new admissions until usage falls back under.
    #[test]
    fn lowering_a_limit_does_not_evict_running_work() {
        let mut l = ledger();
        for i in 0..4 {
            l.try_acquire("u1", &format!("j{i}"), audio(SizeBucket::Small))
                .unwrap();
        }
        l.set_limits("u1", AgentLimits::flat(2, 1));
        assert_eq!(l.in_flight("u1"), 4, "running work is untouched");
        assert_eq!(
            l.try_acquire("u1", "j9", audio(SizeBucket::Small)),
            Err(Refusal::TotalSlots)
        );
    }

    /// The ledger defers to the state machine for what counts as occupying a
    /// slot, so the two cannot drift.
    #[test]
    fn the_admitted_set_matches_the_state_machine() {
        for s in [
            JobState::Assigned,
            JobState::Running,
            JobState::Verifying,
            JobState::Committing,
        ] {
            assert!(occupies_slot(s), "{s:?} must occupy a slot");
        }
        for s in [
            JobState::Pending,
            JobState::Eligible,
            JobState::Blocked,
            JobState::Retrying,
            JobState::Succeeded,
            JobState::Failed,
        ] {
            assert!(!occupies_slot(s), "{s:?} must not occupy a slot");
        }
    }
}
