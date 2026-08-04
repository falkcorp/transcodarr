### Added

#### `Reconciler` — deciding what silence means

Everything else assumes agents report what happened. The reconciler exists for
when they do not: a killed process, a partition, a reboot.

One rule governs it: **silence is never taken as success.** No path marks a job
`Succeeded` because nobody said otherwise — a property asserted directly across
every in-flight state.

- A **connected agent with a current lease is left alone.** Reclaiming work from
  an agent still doing it is how one file gets encoded twice.
- A **vanished agent's work is requeued and its capacity released first** — a
  slot held by a job on a dead agent is a slot the fleet never gets back.
- A job lost **mid-commit is escalated, never requeued.** Between retiring the
  original and installing the replacement, only the filesystem knows what
  happened: requeueing risks encoding over a good replacement, and completing
  risks calling a lost file a success. A live `commit_intent` triggers the same
  rule even when the job row has not caught up.

The lease grace is generous relative to the heartbeat, because reclaiming work
an agent is still doing is worse than waiting — the agent finishes, tries to
commit against a revoked epoch, and the job has meanwhile gone to someone else.

`now_unix` is passed in rather than read, so a pass is deterministic: a
reconciler that consults the clock internally cannot be driven through its own
edge cases.
