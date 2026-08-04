### Added

#### `CapacityLedger` — Phase 4's dispatch accounting

Three properties, each preventing a specific failure:

- **All-or-nothing permits.** A job needing a class slot *and* a large-file slot
  takes both or neither. Taking one and blocking on the second holds capacity
  nobody can use, and two such jobs holding each other's missing half deadlock
  with no timeout to break them. A refused acquisition leaves the ledger
  untouched, which is tested directly.
- **Capacity is released on leaving the admitted set, not on reaching a terminal
  state.** A job going to `Retrying` has stopped using its slot; holding the
  grant until it eventually terminates leaks one slot per retry until the fleet
  is nominally full and completely idle. The admitted set defers to
  `JobState::holds_capacity` rather than being re-derived, so the ledger and the
  state machine cannot drift.
- **Rebuilt from the database before the first dispatch.** A server that
  restarts mid-flight and starts from an empty ledger double-books every agent
  still working, because the jobs holding those slots live in the database, not
  in memory. Jobs whose agent is unknown are *reported* rather than dropped —
  something is running the fleet does not account for, and that is exactly what
  an operator needs told.

The `Large` band is capped separately from the total, because the pool is
latency-bound: 47 concurrent 40–80 GB jobs produced per-file ETAs of 3–34 hours.

Lowering an agent's limit does not evict running work — running work is not made
safe by revoking its slot retroactively; new admissions are refused until usage
falls back under.
