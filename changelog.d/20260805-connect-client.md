<!-- file: changelog.d/20260805-connect-client.md -->
<!-- version: 1.0.0 -->
<!-- guid: 7a41e6b8-0c95-4d23-81fe-3b60d7a2495c -->
<!-- last-edited: 2026-08-05 -->

### Added

#### `ConnectClient` — the agent side of the transport

The server has served `Register` and the `Connect` stream since PR #59; nothing
on the agent knew how to talk to it. It does now: register, replay the journal,
open the stream, heartbeat, and reconnect with backoff.

Four things it gets right on purpose, each of which is a way to lose media if
inverted:

- **One `boot_id` per process, reused across every reconnect.** `fencing_epoch`
  bumps on a new `boot_id` and only on a new one, so minting a fresh one per
  attempt would turn every network blip into an epoch bump and fence work that
  is running perfectly well. There is a test asserting two registrations across
  a dropped stream carry the same one.
- **The journal is replayed before anything is accepted.** `live_intents` goes
  out with `Register`, and the `unknown_job_ids` that come back are resolved
  *before* the stream opens. The ordering is the easy thing to get backwards:
  recovering first clears the records, so the replay goes out empty, the answer
  comes back empty, and the test that checks it passes while proving nothing.
- **Each unknown intent is resolved by how far it got.** A `Granted` record only
  needs its staged file discarded; a `Retired` one means the original is in the
  trash and the destination may be empty, and restoring it is the whole job. A
  uniform clean-up would delete media. Covered by a test that crashes an agent
  mid-ritual on real files and asserts the original comes back.
- **A refusal is not permission.** No `CommitGrant`, a refused one, a dead
  stream, or a grant that never arrives are all the same answer: nothing is
  installed and the source is left exactly as it was.
- **Startup recovery runs once, and it runs.** `on_unknown_intents` only covers
  the records the server *disowns*; the ordinary crash — where the ledger row
  and the journal record both exist — is `Worker::on_startup`, called after the
  replay and before the stream opens. Once per process rather than per
  reconnect, because a reconnect can land while a commit is between its two
  renames, and recovery running then would restore the original out from under
  the ritual installing over it.

A `Revoke` now stops the install, not just the accounting: the job is marked and
`execute` checks that before asking permission. The server would refuse the
commit anyway, having no live intent for a job it revoked — but leaning on that
means the agent's own revoke handling does nothing while appearing to.

#### `LocalWorker`

The same `Executor` and `CommitRitual` that `admin run` uses, driven by the
server instead of by a local queue. `JobAssignment.argv` is run verbatim — an
agent that rebuilt the command locally could encode to a plan the server never
authorised, and nothing would notice until the output was installed.

### Changed

- `Executor::run_argv` runs a command composed elsewhere; `Executor::run` is now
  a wrapper that builds one from a plan. One implementation under both entry
  points, so the dispatched path cannot drift from the one proven on real media.
- `Executor` and `CommitRitual` are `Clone`, so an encode and an install can run
  on a blocking thread. Both hold path handles rather than open resources.
- `transcodarr-agent` gains `tokio`, `tonic`, `tokio-stream`, `tracing` and
  `transcodarr-proto`. It still does not link `transcodarr-store`, and
  `cargo tree -p transcodarr-agent -i transcodarr-store` reports no such package
  in the graph at all.
