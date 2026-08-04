### Added

#### `CommitIntentRepo` — the server-side commit ledger

The agent's `IntentJournal` survives a crash of the *agent*; this table survives
a crash of the *connection*. Without it, a `JobResult` lost in flight after a
successful replace makes the next attempt re-encode a file that has already been
replaced — reading the new file as though it were the original.

`idx_commit_intent_live` makes two live intents on one final path structurally
impossible. Two agents mid-replace on the same path is not a race to detect and
log; it is an insert that fails. Resolved rows are retained, never deleted —
they are the audit trail for what happened to a file, long after the job is
pruned.

#### `TrashRepo` — retention with a non-negotiable floor

Two rules that are easy to get wrong, both enforced here:

- **A minimum grace period, always.** Pool pressure is a reason to reap
  *sooner*, never immediately. A pool that filled because of a runaway job would
  otherwise delete the very originals that job destroyed. A configured retention
  below the floor is raised to it, and the floor is re-applied in SQL at read
  time so a row written by a misconfigured or older binary still cannot be
  reaped early.
- **Reclaim is measured from ZFS accounting, never from file sizes.** Deleting a
  40 GB original reclaims nothing while a snapshot still references its blocks.
  `retained_totals` is documented as what is *held*, not what freeing it would
  reclaim; that number comes from `pool_reclaim_sample`.

The hasten-under-pressure floor is measured from when an entry was retained, not
from now. Clamping to "a moment in the future" instead — which is what I wrote
first — makes hastening incapable of ever bringing anything due: a guard that
silently does nothing, which is worse than no guard because it looks like one.
