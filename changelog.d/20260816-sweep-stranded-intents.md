<!-- file: changelog.d/20260816-sweep-stranded-intents.md -->
<!-- version: 1.0.0 -->
<!-- guid: 6f2b8d41-0c93-4e57-b18a-935de4c07621 -->
<!-- last-edited: 2026-08-16 -->

### Fixed

#### A stranded commit intent no longer wedges its file permanently

`idx_commit_intent_live` is `UNIQUE(final_path) WHERE state = 'live'` — keyed on
the **path**, not on `(job_id, attempt)`. A live intent row that nothing resolves
therefore does not merely leak. It blocks every future attempt on that file, a
retry under a fresh attempt number, and any brand-new job for the same path.
Only a resolve frees it.

`CommitIntentRepo::live()` — whose doc comment read "what the reconciler
sweeps" — **had no production callers at all.** It now has one. The reconciler's
periodic pass resolves live intents whose job has reached a terminal state, and
those whose job no longer exists. Startup would have been the wrong trigger for
a condition that arises continuously: a startup-only sweep cannot see an agent
that dies at minute five.

**`NeedsOperator` is excluded, and that exclusion is the safety argument.** It
is where an ambiguous commit lands, and the live intent is what holds the
destination reserved while a human looks at it. Sweeping it would free a path
whose on-disk state nobody has determined, and the next job for that file would
install over it. A live intent on a job still *in flight* is likewise never
swept — the reconciler already escalates that case, because the agent may be
between the two renames right now.

With the ambiguous case carved out, every remaining row belongs to a job whose
outcome was already decided and recorded, so there is nothing to adjudicate and
no file to move — only a ledger row to close. That is why this does not reuse
the agent's `recover_one` decision table.

#### `resolve_op` is guarded on `state = 'live'`

It was `WHERE id = ?1`, unlike `advance_op` directly above it, which has always
been `WHERE id = ?1 AND state = 'live'`. The asymmetry looks unintentional and
it cost two things.

A sweep and a legitimately finishing ritual can reach the same row, and
unguarded both succeed — the sweeper frees a path that is mid-replace, which is
precisely what the unique index exists to prevent. Separately, several callers
resolve best-effort (`release_intent`, `requeue`) and can fire against a row
that is already done; a later "abandoned" would overwrite an earlier
"installed", leaving the audit trail recording the opposite of what happened to
the file.

### Known gap

`LocalRunner` (`admin run`) has the same hole and is not covered: it runs no
orchestrator, so no sweep passes over it. It grants intents directly and a crash
between `grant_op` and `resolve_op` still strands one.
