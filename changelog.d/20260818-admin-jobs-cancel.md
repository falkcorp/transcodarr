<!-- file: changelog.d/20260818-admin-jobs-cancel.md -->
<!-- version: 1.0.0 -->
<!-- guid: 3d7c4e91-58ab-4f26-9c03-e1b58a2d7f40 -->
<!-- last-edited: 2026-08-18 -->

### Added

#### `admin jobs cancel` — an operator can end a job that can never finish

```
transcodarr admin jobs cancel <id> [--reason <text>] [--force]
```

Until now a job that had become permanently unsatisfiable could not be cleared
at all. `admin` had no cancel, reset or requeue, so the only recourse was
editing SQLite by hand — and `explain` would go on naming a requirement no
installed code can emit, which reads as a capability gap rather than a stale
row. That is the shape `REQ-REFRESH` describes, and this is the escape hatch it
asks for; the refresh itself is still open.

The job id comes from `admin explain <path>`, which already prints it.

**`Cancelled` was a fully modelled state that nothing ever wrote.** It is in
`JobState`, `can_transition` admits `(_, Cancelled)` from anywhere non-terminal,
`is_terminal` includes it, `transition_op` stamps `finished_unix` for it and
`metrics` counts it. Every downstream consumer was already built and waiting for
a caller — the fifth instance of the documented-tested-and-never-called pattern
this repository keeps turning up.

Which is why this adds so little. Three things that look like they need handling
here are already covered, and the code says so at each site rather than
duplicating them:

- **Capacity.** The ledger is rebuilt from the database every tick
  (`Orchestrator::tick` → `rebuild_capacity`), and `rebuild` skips any state
  where `!occupies_slot`. The slot frees itself on the next pass. Releasing it
  from the CLI would touch that process's ledger, not the running server's.
- **The commit intent.** `sweep_stranded_intents` resolves live intents whose
  job is terminal and not `NeedsOperator`. A cancelled job is exactly that.
- **An agent still holding the job.** `on_heartbeat` revokes any running job
  whose state is not in `HELD_STATES`, and the agent sweeps its work area on
  every exit path. So `--force` needed no new protocol message.

### The two refusals

**A job an agent is holding needs `--force`.** The stated need — a job blocked
forever on an unsatisfiable requirement — is never in flight, so interrupting
live work is the deliberate case, not the default.

**A job that is `Committing` is refused even under `--force`.** That is the
window between the commit ritual's two renames, which is the ambiguity
`NeedsOperator` exists to record. Cancelling there races a rename on real files,
and the intent sweep would then free a destination whose on-disk state nobody
has determined — the next job for that file would install over it. Wait for the
ritual to land, or for it to escalate.

Both guards were mutation-tested: removing either turns exactly one test red,
and no other.

The transition records `operator_cancelled` as its reason code and the
`--reason` text as the event detail. A terminal row with no recorded why is the
same legibility failure `explain` had before it learned to print
`DispatchBlock::reason`.

`--force` stops the *install*, not ffmpeg: the agent checks the revoke after its
encode finishes, so a forced cancel lets the remaining encode run and then
discards it. Safe, and not instant.
