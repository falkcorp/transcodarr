### Added

#### `transcodarr-store`: schema, migrations and durability checks

Phase 2, first increment. The crate owns SQLite and nothing else; only
`transcodarr-server` will link it, so an agent stays copyable to the Windows
node without dragging a database engine along.

All 21 contract tables ship as one embedded migration — embedded rather than
read from disk so a deployed binary cannot be paired with the wrong migration
directory. Every table is `STRICT`: SQLite's default affinity will happily store
the string `Running` in an INTEGER column, and a job state machine a typo can
corrupt is not a state machine.

Two invariants are enforced by the database rather than by application
discipline, and both have tests proving the negative case fails and the
legitimate follow-up still succeeds:

- **One open job per file** — `idx_job_open_per_file`, a partial unique index
  over non-terminal states. Double dispatch is impossible, not merely unlikely.
  A follow-up job after a terminal one is still allowed, which is what makes the
  audio-then-video two-stage flow work.
- **One live commit intent per final path** — `idx_commit_intent_live`. Two
  agents can never be mid-replace on the same file.

`Db::open` applies the pragma block and then **verifies it took**. Setting a
pragma is a request, not a guarantee: `journal_mode = WAL` fails silently on
some filesystems and leaves the connection in `delete` mode, where the
concurrency assumptions behind a single writer plus a read pool do not hold.

A migration whose text has changed since it was applied is refused rather than
re-run or ignored — the database and the binary disagreeing about the schema is
not something to paper over.
