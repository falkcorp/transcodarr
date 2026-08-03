### Added

#### `ReadPool` and the first four repositories

`transcodarr-store` gains `ReadPool` — an r2d2 pool of read-only connections, so
reads run concurrently alongside the single writer rather than queueing behind
it — plus `LibraryRepo`, `FileRepo`, `JobRepo` and `DispatchBlockRepo`.

Repositories return domain types from `transcodarr-core` and never a
`rusqlite::Row`; no SQL text escapes the crate. `JobRepo::transition` is a real
compare-and-swap: it rejects any edge `JobState::can_transition` forbids, and
its `UPDATE` carries the expected state so a job moved in between is reported as
a lost race rather than overwritten. The `job_event` insert shares the operation,
so the ledger and the row cannot disagree.

Four of the eleven contracted repositories are implemented — the four the Phase 2
milestone exercises. The rest arrive with the phases that call them.

`FileRepo::upsert` keeps stored probe facts across a rescan unless the size or
mtime actually moved, and marks unseen files `Missing` rather than deleting them.

#### Canonical spellings on the persisted core enums

`JobState`, `JobClass`, `SizeBucket` and `DecisionClass` gain `as_str`/`parse`,
and `BitDepth` gains `bits`/`from_bits`. These live in `transcodarr-core` beside
the enums because the enums are `#[non_exhaustive]`: mapping them downstream
would force a wildcard arm, and a wildcard in a state-to-text mapping silently
persists a new variant under an old variant's name.

#### `FileState`

The contracted file lifecycle enum: `Discovered`, `Probing`, `Probed`,
`ProbeFailed`, `Evaluated`, `Processed`, `Quarantined`, `Missing`. Deliberately
separate from `JobState` — dispatching off file state was Tdarr failure mode 7.

### Changed

#### `WriteAck` reports a row id

`WriteAck` gains `last_id`, and `WriteOp::new_with_id` lets an operation report
which row it settled on. The scanner needs the `file.id` an upsert resolved to,
and `last_insert_rowid()` cannot supply it: an upsert taking the `DO UPDATE`
branch inserts nothing, so the connection's rowid still belongs to an earlier
operation. Overloading `rows` to sometimes mean "which one" was rejected as a
bug waiting for the Phase 3 commit ledger.

#### A changed file's decision is invalidated with its signature

`FileRepo::upsert` previously cleared only `content_sig` when a file changed on
disk, leaving `eval_rules_version`, `decision`, `decision_reason` and
`same_decision_streak` in place. A file replaced in-place and re-probed then
still matched its old rules version, so the evaluator skipped it and a decision
computed from facts that no longer existed stood until the rules version moved.
All five now live or die together. Change detection also compares `mtime_ns`, so
a same-second, same-size rewrite is not invisible.
