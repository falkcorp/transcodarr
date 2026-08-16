<!-- file: changelog.d/20260816-dispatch-block-has-a-writer.md -->
<!-- version: 1.0.0 -->
<!-- guid: 61e1bcf7-15bf-4d5e-9732-42a050127a33 -->
<!-- last-edited: 2026-08-16 -->

### Fixed

#### The `dispatch_block` table has a writer

`dispatch.rs` says outright that "nothing is running and I do not know why" is
the question this table exists to answer, and that it can only answer it if the
dispatcher says why each time it declines. Nothing ever wrote a row. The
dispatcher's reasoning went to a `debug!` line and then nowhere, so the answer
was available only to an operator who thought to restart the server under a
debug filter and wait for the next pass to come round.

The orchestrator now records the stage and the reason for each job it declines,
and clears the record when that job places — a stale block says a job is stuck
when it is running, which is exactly the sort of thing an operator acts on.

#### A tick that stops early still says why it stopped

Two conditions stop the entire queue at once, and both returned from `tick()`
before the dispatcher ever ran, so neither could be reported by a table that
only the dispatcher wrote to:

- **No agent is connected.** The tick returned as soon as the fleet came back
  empty — before it had even read the queue. This is the commonest form of the
  question, and it was the one case with no answer at all.
- **The schedule paused everything.** Recorded as `debug!` and dropped.

Both are now recorded against every waiting job. They are fleet-wide facts
written per job because `explain` is asked about a *file* and has no fleet of
its own to consult.

#### `explain` prints the reason, not just the category

`Explanation::render` read `blocking_stage` and dropped `detail_json` on the
floor. "not dispatching: capability" is true and useless — the requirement that
actually went unmet is the whole answer, and without it the operator's next move
is still to restart the server under a debug filter.

`DispatchBlock::reason` unwraps the stored detail, and `DispatchBlock::detail_for`
wraps a sentence on the way in, so no caller hands prose to a column named
`_json`. A row that is not that shape is shown verbatim rather than dropped: a
reason an operator cannot see costs exactly what a reason nobody recorded does.

### Changed

#### `pause_reason` replaces `is_paused`

`EffectiveLimits::source` records which window or override decided the limits,
"for the operator" — and the caller reduced it to a `bool`. The orchestrator now
carries the source through, so a paused fleet names the rule to edit instead of
leaving it to be found by reading the schedule config. Distinct sources are all
listed, because different agents can be silenced by different rules and naming
one would send the operator to the wrong entry.
