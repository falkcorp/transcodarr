<!-- file: changelog.d/20260806-retry-budget-and-schedule.md -->
<!-- version: 1.0.0 -->
<!-- guid: 5c0b73a8-49e1-4d27-b6f3-81a25cd0e694 -->
<!-- last-edited: 2026-08-06 -->

### Fixed

#### Requeued jobs were stranded forever

The loop drew its queue from `Pending` only, while every requeue lands a job in
`Eligible` — the state machine has no edge back to `Pending`. So every job whose
agent went away sat there permanently, invisible to each later pass, with the
queue looking empty and the file never processed. A single-job proof that runs
once, on its first attempt, to success cannot see this.

#### Nothing enforced a retry budget

`hardening::decide_retry` existed, was tested, and had no caller — the third
component in this repository found in that state, after `LocalWorker::recover()`
and the dropped muxers. `job.attempt` was never incremented and `max_attempts`
was never read, so a job that kept losing its agent would cycle forever,
occupying a slot every tick while the queue behind it never moved.

The attempt is now incremented **only when the job will actually be tried
again** — a job on its way to being dead-lettered does not consume one. That
number is also what makes a retry's commit intent distinct: `commit_intent.id`
is `job:attempt`, so a re-dispatch that reused it would collide with the
previous attempt's row on the primary key and could never be placed at all.

#### A rejected encode killed the job outright

`on_result` sent the first validation failure straight to `Failed`, which is
terminal and cannot be transitioned out of. A rejected output is usually the
file or the plan and sometimes the machine — a full disk, an OOM kill, an ffmpeg
that died on a bad sector read — and throwing the job away over one bad
afternoon on one node is not recoverable. It now goes through `Retrying` under
the same budget.

#### The reconciler escalated everything instead of retrying

`InFlight::has_live_intent` was computed as "a row exists", but since intents are
written at *dispatch*, that is true for every job in flight. The reconciler read
it as "may be mid-commit" and escalated every job whose agent merely went
offline. It is now only true for a job in `Committing`, which is the actual
danger window — the grant is never issued before that transition succeeds.

Relatedly, a commit whose `Committing` transition fails is now **refused rather
than granted**: permission the server cannot record is permission it cannot
account for, and the agent would replace a file the reconciler still believes
nobody is touching.

#### `ScheduleEngine` was never consulted

Built, tested, and never asked anything — so an operator pausing the fleet
changed nothing. `Orchestrator::tick` now asks it before placing work, and
`ScheduleEngine::paused_until` expresses a fleet-wide pause. A window closing
means "start nothing new", never "stop what is running".

### Added

#### `transcodarr-server/tests/dispatch_loop.rs`

The conditions the single-job proof cannot reach: a requeued job dispatched
again, a job that keeps failing eventually dead-lettered, six jobs across two
agents capped by the ledger, and a paused schedule placing nothing. The
dispatcher's bucket/admission split and the capacity ledger had never run with
more than one job or one agent.
