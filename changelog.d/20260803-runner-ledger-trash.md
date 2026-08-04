### Changed

#### `LocalRunner` now writes the commit ledger and retains originals

`CommitIntentRepo` and `TrashRepo` existed and were tested but had no caller.
They are now wired into the install path:

- A `commit_intent` row is granted on the **commit lane** *before* the ritual
  touches anything, and resolved afterwards whatever the outcome — so a final
  path is never left permanently locked by a live intent nobody will finish. A
  refused grant is a refusal to proceed, not a warning to log past:
  `idx_commit_intent_live` failing means another agent already holds the path.
- A `trash_entry` is recorded only *after* the original really has been moved.
  Writing it first would leave a row pointing at a file that was never moved,
  and the reaper would later try to delete the live original.

Default retention is 7 days, brought forward under pool pressure but never below
the grace floor.

A dry run grants no intent at all — granting one for work that will never happen
leaves the path locked.
