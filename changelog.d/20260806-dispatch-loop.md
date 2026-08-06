<!-- file: changelog.d/20260806-dispatch-loop.md -->
<!-- version: 1.0.0 -->
<!-- guid: 1f6d84b2-93c5-4a07-8e21-b40d75c9e836 -->
<!-- last-edited: 2026-08-06 -->

### Added

#### The dispatch loop — work flows end to end

`Dispatcher`, `CapacityLedger`, `Reconciler` and `ScheduleEngine` all existed,
were tested, and had no caller. `Orchestrator` runs them on a tick and turns
"what should happen" into "an agent was told to do it". It runs alongside the
gRPC server under one shutdown signal, so a dispatch pass never places work on
agents the closing transport can no longer reach.

- **The ledger is rebuilt from the database every tick, not maintained.**
  Incremental accounting is a second source of truth about which agent holds
  what, and its failure mode is silent: a missed release leaks a slot, the agent
  looks full, and the fleet runs below capacity with nothing in any log.
- **The commit intent is written before the assignment is sent.** The unique
  index over live intents is what makes two jobs against one destination
  impossible; writing it when the agent asks would leave that window open for
  the length of an encode. An assignment that fails to send releases it again.
- **The plan is re-derived at dispatch**, not read back from the job row, so a
  policy change takes effect on the next dispatch rather than being frozen in
  when the job was created.

`AgentSession` now acts on `JobResult` rather than logging it: `Verifying` on a
pass, `Failed` plus a released intent on a rejection. A failed job that kept its
intent would block its own file forever, because the next attempt's intent
collides with the unique index and can never be written.

#### `transcodarr-server/tests/end_to_end.rs`

One job through the whole system over a real gRPC channel — register → connect →
dispatch → encode → result → `RequestCommit` → `ReportCommit` → `Succeeded` —
with real ffmpeg, a real `Executor` and a real `CommitRitual`. FLAC in, EAC3 out,
video copied untouched, original retained. Plus the fence: a superseded epoch
cannot open a stream. Both verified to fail when the behaviour they check is
removed.

### Fixed

#### A commit grant handed back the destination as the trash path

`judge_commit` returned `intent.final_path` as `CommitGrant.trash_path`. The
ritual would then rename the original onto itself — a silent no-op — and install
the replacement over it. **The original was destroyed by the step that exists to
preserve it.** The unit test covering this asserted the wrong path, so it passed.

The grant now derives the trash path from the library's `trash_dir`, and the
test asserts the property (`trash_path != final_path`) rather than a literal.

#### Originals with the same name collided in the trash

`trash_path_for` preserves the path *below the library root* rather than
flattening every original into one directory. Two shows each having an
`S01E01.mkv` is the common case, and a flat trash made the second replacement
overwrite the first original. `admin run` used the flat form too and now shares
the helper.

#### A dispatched encode was validated against the container header

The server builds the validation spec from stored facts, whose duration comes
from the container header; the agent measures the output's *last packet PTS*.
Those are not the same quantity, so every dispatched job failed validation with
a phantom shortfall — measured: header 2.000s, output PTS 1.800s, tolerance
`min(0.5%, 5s)` = 10ms. The agent now re-measures the source the same way it
measures the output, as `admin run` already did. It does this rather than the
server because it is the machine holding the file.
