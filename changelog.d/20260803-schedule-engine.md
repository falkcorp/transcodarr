### Added

#### `ScheduleEngine` — when work runs, and how much

Three rules, each preventing something specific:

- **Drain, never cancel.** A window closing means "start nothing new", not "kill
  what is running". Cancelling mid-encode throws away the work and can interrupt
  a commit — the one moment where stopping is genuinely dangerous.
  `should_interrupt_running()` exists so the rule is stated where a caller has
  to read it, rather than being an absence of code.
- **Overrides expire, always.** A temporary limit change that outlives the
  operator's memory of making it is a permanent one, and the usual way a fleet
  ends up mysteriously throttled for months. Expiry *reports* what it removed —
  an override vanishing silently is indistinguishable from one that never
  applied.
- **A zero limit is honoured, not treated as unset.** `0` means "run nothing",
  and reading it as "no opinion" turns a deliberate pause into a full-throttle
  night. `is_paused()` makes that a question the UI can answer, rather than
  showing an idle fleet with no explanation.

Windows wrapping past midnight are handled explicitly. An overnight window
(22:00–06:00) has `end < start`, and the naive `start <= m && m < end` treats
that as empty — silently disabling exactly the windows operators care most
about.

Precedence is baseline → highest-priority covering window → active override.
Most specific last, so an operator acting on information the schedule does not
have always wins.
