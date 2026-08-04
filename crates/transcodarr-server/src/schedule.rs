// file: crates/transcodarr-server/src/schedule.rs
// version: 1.0.0
// guid: 0c67f4b8-2a91-45de-83b0-6e17c9d2045a
// last-edited: 2026-08-03
//! When work is allowed to run, and how much of it.
//!
//! Three rules, each preventing something specific:
//!
//! - **Drain, never cancel.** A window closing means "start nothing new", not
//!   "kill what is running". Cancelling mid-encode throws away an hour of work
//!   and, worse, can interrupt a commit — the one moment where stopping is
//!   genuinely dangerous.
//! - **Overrides expire, always.** A temporary limit change that outlives the
//!   operator's memory of making it is a permanent one, and the usual way a
//!   fleet ends up mysteriously throttled for months.
//! - **A zero limit is honoured, not treated as unset.** `0` means "run
//!   nothing", and quietly reading it as "no opinion" is how a deliberate pause
//!   becomes a full-throttle night.

use std::collections::HashMap;

use transcodarr_core::job::JobClass;

/// Minutes in a day, for window arithmetic.
const MINUTES_PER_DAY: i64 = 24 * 60;

/// A recurring window during which different limits apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleWindow {
    /// Operator-facing name.
    pub name: String,
    /// Whether it is in force at all.
    pub enabled: bool,
    /// Bitmask of weekdays, bit 0 = Monday.
    pub days_mask: u8,
    /// Start, minutes since midnight.
    pub start_minute: i64,
    /// End, minutes since midnight. May be *before* `start_minute`, which means
    /// the window wraps past midnight — the common case for an overnight run.
    pub end_minute: i64,
    /// Higher wins when windows overlap.
    pub priority: i64,
    /// Total slots while this window is in force.
    pub total_slots: Option<u32>,
    /// Per-class slots while this window is in force.
    pub per_class: HashMap<JobClass, u32>,
}

impl ScheduleWindow {
    /// Whether this window covers a given weekday and minute.
    ///
    /// Handles the wrap case explicitly. An overnight window (22:00–06:00) has
    /// `end < start`, and treating that as an empty range — which a naive
    /// `start <= m && m < end` does — silently disables exactly the windows
    /// operators care most about.
    pub fn covers(&self, weekday: u8, minute_of_day: i64) -> bool {
        if !self.enabled || weekday > 6 {
            return false;
        }
        if self.days_mask & (1 << weekday) == 0 {
            return false;
        }
        let m = minute_of_day.rem_euclid(MINUTES_PER_DAY);
        if self.start_minute <= self.end_minute {
            m >= self.start_minute && m < self.end_minute
        } else {
            m >= self.start_minute || m < self.end_minute
        }
    }
}

/// A manual, expiring change to the limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    /// What it applies to: an agent id, or `*` for the fleet.
    pub scope: String,
    /// Which class, or `None` for the total.
    pub class: Option<JobClass>,
    /// The slot count to use.
    pub slots: u32,
    /// When it stops applying. Mandatory by design.
    pub expires_unix: i64,
    /// Who set it and why.
    pub reason: String,
}

impl Override {
    /// Whether this override is still in force.
    pub fn active(&self, now_unix: i64) -> bool {
        now_unix < self.expires_unix
    }

    /// Whether it applies to a given agent.
    pub fn applies_to(&self, agent_id: &str) -> bool {
        self.scope == "*" || self.scope == agent_id
    }
}

/// The limits in force right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveLimits {
    /// Total concurrent jobs.
    pub total_slots: u32,
    /// Per-class limits.
    pub per_class: HashMap<JobClass, u32>,
    /// Which window decided this, for the operator.
    pub source: String,
}

impl EffectiveLimits {
    /// Whether all work is paused.
    ///
    /// A distinct question from "are there slots", because zero is a
    /// deliberate state an operator chose and the UI should say so rather than
    /// showing an idle fleet with no explanation.
    pub fn is_paused(&self) -> bool {
        self.total_slots == 0
    }
}

/// Computes the limits in force from windows, overrides and a baseline.
#[derive(Debug, Clone, Default)]
pub struct ScheduleEngine {
    windows: Vec<ScheduleWindow>,
    overrides: Vec<Override>,
}

impl ScheduleEngine {
    /// An engine with no windows or overrides.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the window set.
    pub fn set_windows(&mut self, windows: Vec<ScheduleWindow>) {
        self.windows = windows;
    }

    /// Replace the override set.
    pub fn set_overrides(&mut self, overrides: Vec<Override>) {
        self.overrides = overrides;
    }

    /// Drop overrides that have expired.
    ///
    /// Returns what was removed, so the removal can be logged. An override
    /// vanishing silently is indistinguishable from one that never applied,
    /// and an operator wondering why the limits changed deserves an answer.
    pub fn expire(&mut self, now_unix: i64) -> Vec<Override> {
        let (live, dead): (Vec<_>, Vec<_>) =
            self.overrides.drain(..).partition(|o| o.active(now_unix));
        self.overrides = live;
        dead
    }

    /// The limits for one agent at one moment.
    ///
    /// Precedence is baseline, then the highest-priority covering window, then
    /// any active override — most specific last, so an operator's explicit
    /// instruction always wins over a schedule.
    pub fn effective(
        &self,
        agent_id: &str,
        baseline_total: u32,
        baseline_per_class: &HashMap<JobClass, u32>,
        weekday: u8,
        minute_of_day: i64,
        now_unix: i64,
    ) -> EffectiveLimits {
        let mut limits = EffectiveLimits {
            total_slots: baseline_total,
            per_class: baseline_per_class.clone(),
            source: "baseline".to_string(),
        };

        if let Some(w) = self
            .windows
            .iter()
            .filter(|w| w.covers(weekday, minute_of_day))
            .max_by_key(|w| w.priority)
        {
            if let Some(total) = w.total_slots {
                limits.total_slots = total;
            }
            for (class, slots) in &w.per_class {
                limits.per_class.insert(*class, *slots);
            }
            limits.source = format!("window '{}'", w.name);
        }

        for o in self
            .overrides
            .iter()
            .filter(|o| o.active(now_unix) && o.applies_to(agent_id))
        {
            match o.class {
                Some(class) => {
                    limits.per_class.insert(class, o.slots);
                }
                // Zero is honoured, not treated as unset. It means "run
                // nothing", and reading it as "no opinion" turns a deliberate
                // pause into a full-throttle night.
                None => limits.total_slots = o.slots,
            }
            limits.source = format!("override ({})", o.reason);
        }

        limits
    }

    /// Whether a running job should be allowed to finish.
    ///
    /// Always yes. A window closing means "start nothing new", never "kill what
    /// is running" — cancelling mid-encode throws away the work and can
    /// interrupt a commit, which is the one moment where stopping is genuinely
    /// dangerous. This function exists so the rule is stated somewhere a caller
    /// has to read, rather than being an absence of code.
    pub fn should_interrupt_running(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_DAYS: u8 = 0b0111_1111;

    fn window(name: &str, start: i64, end: i64, total: Option<u32>, prio: i64) -> ScheduleWindow {
        ScheduleWindow {
            name: name.into(),
            enabled: true,
            days_mask: ALL_DAYS,
            start_minute: start,
            end_minute: end,
            priority: prio,
            total_slots: total,
            per_class: HashMap::new(),
        }
    }

    fn engine(windows: Vec<ScheduleWindow>) -> ScheduleEngine {
        let mut e = ScheduleEngine::new();
        e.set_windows(windows);
        e
    }

    fn effective(e: &ScheduleEngine, minute: i64, now: i64) -> EffectiveLimits {
        e.effective("u1", 4, &HashMap::new(), 0, minute, now)
    }

    #[test]
    fn with_no_windows_the_baseline_applies() {
        let l = effective(&engine(vec![]), 600, 0);
        assert_eq!(l.total_slots, 4);
        assert_eq!(l.source, "baseline");
    }

    #[test]
    fn a_covering_window_replaces_the_baseline() {
        let e = engine(vec![window("nightly", 60, 300, Some(16), 0)]);
        assert_eq!(effective(&e, 120, 0).total_slots, 16);
        assert_eq!(effective(&e, 400, 0).total_slots, 4, "outside the window");
    }

    /// The overnight case. An operator's most valuable window wraps midnight,
    /// and a naive `start <= m && m < end` silently disables exactly those.
    #[test]
    fn a_window_wrapping_past_midnight_is_covered_correctly() {
        let w = window("overnight", 22 * 60, 6 * 60, Some(16), 0);
        assert!(w.covers(0, 23 * 60), "23:00 is inside 22:00-06:00");
        assert!(w.covers(0, 2 * 60), "02:00 is inside");
        assert!(w.covers(0, 22 * 60), "the start minute is inclusive");
        assert!(!w.covers(0, 6 * 60), "the end minute is exclusive");
        assert!(!w.covers(0, 12 * 60), "midday is outside");
    }

    #[test]
    fn a_disabled_window_covers_nothing() {
        let mut w = window("off", 0, 1440, Some(16), 0);
        w.enabled = false;
        assert!(!w.covers(0, 600));
    }

    #[test]
    fn a_window_only_applies_on_its_days() {
        let mut w = window("weekend", 0, 1440, Some(16), 0);
        w.days_mask = 0b0110_0000; // Saturday and Sunday
        assert!(w.covers(5, 600));
        assert!(w.covers(6, 600));
        assert!(!w.covers(2, 600));
    }

    #[test]
    fn the_highest_priority_window_wins_an_overlap() {
        let e = engine(vec![
            window("broad", 0, 1440, Some(8), 0),
            window("quiet-hours", 0, 480, Some(2), 10),
        ]);
        assert_eq!(effective(&e, 100, 0).total_slots, 2);
        assert_eq!(effective(&e, 600, 0).total_slots, 8);
    }

    /// An operator's explicit instruction beats a schedule. The schedule is a
    /// default; the override is someone acting on information the schedule
    /// does not have.
    #[test]
    fn an_override_beats_a_window() {
        let mut e = engine(vec![window("nightly", 0, 1440, Some(16), 0)]);
        e.set_overrides(vec![Override {
            scope: "*".into(),
            class: None,
            slots: 1,
            expires_unix: 1000,
            reason: "pool is thrashing".into(),
        }]);
        let l = effective(&e, 600, 500);
        assert_eq!(l.total_slots, 1);
        assert!(l.source.contains("pool is thrashing"));
    }

    /// The rule that keeps a temporary change temporary. A limit change that
    /// outlives the operator's memory of making it is a permanent one.
    #[test]
    fn an_expired_override_stops_applying() {
        let mut e = engine(vec![]);
        e.set_overrides(vec![Override {
            scope: "*".into(),
            class: None,
            slots: 1,
            expires_unix: 1000,
            reason: "temporary".into(),
        }]);
        assert_eq!(effective(&e, 600, 999).total_slots, 1);
        assert_eq!(
            effective(&e, 600, 1000).total_slots,
            4,
            "at the expiry instant it is already gone"
        );
    }

    /// Zero means "run nothing". Reading it as "unset" turns a deliberate pause
    /// into a full-throttle night.
    #[test]
    fn a_zero_slot_override_pauses_rather_than_being_ignored() {
        let mut e = engine(vec![window("nightly", 0, 1440, Some(16), 0)]);
        e.set_overrides(vec![Override {
            scope: "*".into(),
            class: None,
            slots: 0,
            expires_unix: 1000,
            reason: "maintenance".into(),
        }]);
        let l = effective(&e, 600, 0);
        assert_eq!(l.total_slots, 0);
        assert!(l.is_paused(), "zero must read as a deliberate pause");
    }

    /// ...and a zero window limit likewise.
    #[test]
    fn a_zero_slot_window_pauses_too() {
        let e = engine(vec![window("blackout", 0, 1440, Some(0), 5)]);
        assert!(effective(&e, 600, 0).is_paused());
    }

    #[test]
    fn an_override_scoped_to_another_agent_does_not_apply() {
        let mut e = engine(vec![]);
        e.set_overrides(vec![Override {
            scope: "u2".into(),
            class: None,
            slots: 1,
            expires_unix: 1000,
            reason: "u2 only".into(),
        }]);
        assert_eq!(effective(&e, 600, 0).total_slots, 4);
    }

    #[test]
    fn a_per_class_override_leaves_the_total_alone() {
        let mut e = engine(vec![]);
        e.set_overrides(vec![Override {
            scope: "*".into(),
            class: Some(JobClass::VideoGpu),
            slots: 0,
            expires_unix: 1000,
            reason: "gpu maintenance".into(),
        }]);
        let l = effective(&e, 600, 0);
        assert_eq!(l.total_slots, 4, "the total is untouched");
        assert_eq!(l.per_class.get(&JobClass::VideoGpu), Some(&0));
        assert!(!l.is_paused(), "only one class is paused");
    }

    /// An override vanishing silently is indistinguishable from one that never
    /// applied. An operator wondering why the limits changed deserves an
    /// answer.
    #[test]
    fn expiring_reports_what_it_removed() {
        let mut e = engine(vec![]);
        e.set_overrides(vec![
            Override {
                scope: "*".into(),
                class: None,
                slots: 1,
                expires_unix: 100,
                reason: "old".into(),
            },
            Override {
                scope: "*".into(),
                class: None,
                slots: 2,
                expires_unix: 10_000,
                reason: "still valid".into(),
            },
        ]);
        let removed = e.expire(500);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].reason, "old");
        assert_eq!(effective(&e, 600, 500).total_slots, 2);
    }

    /// Drain, never cancel. Cancelling mid-encode throws away the work and can
    /// interrupt a commit -- the one moment where stopping is dangerous.
    #[test]
    fn a_closing_window_never_interrupts_running_work() {
        assert!(!engine(vec![]).should_interrupt_running());
    }

    /// A window with no opinion on the total leaves the baseline in place
    /// rather than zeroing it.
    #[test]
    fn a_window_without_a_total_does_not_zero_the_baseline() {
        let e = engine(vec![window("class-only", 0, 1440, None, 0)]);
        assert_eq!(effective(&e, 600, 0).total_slots, 4);
    }
}
