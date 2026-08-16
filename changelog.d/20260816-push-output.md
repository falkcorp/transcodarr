<!-- file: changelog.d/20260816-push-output.md -->
<!-- version: 1.0.0 -->
<!-- guid: b7f4c091-3ad6-42e8-9c15-0e83d7a6b42f -->
<!-- last-edited: 2026-08-16 -->

### Added

#### `PushOutput` installs a streaming agent's output

The second half of the streaming transport. A `TM_STREAM` agent pushes its
finished encode and the server stages it, verifies it and runs the commit
ritual on the agent's behalf — the agent has never been able to see the
destination, so the install has to happen here. With `FetchSource` already
serving bytes, a streaming agent can now complete a job end to end.

The install is the *same* ritual a mount-mode agent runs locally, against the
same ledger, reusing the same `judge_commit`. A second implementation of "may
this agent replace this file" would be a second implementation that can
disagree with the first.

**The intent is not granted here, and granting it would be a bug.** The
orchestrator writes the `commit_intent` row at dispatch, before the assignment
goes out, so the destination is reserved for the length of the encode rather
than from the moment the agent asks. A job that arrived over the wire is always
already granted; `LocalRunner` grants its own only because nothing dispatched
to it. Granting twice would collide on the primary key.

#### A self-asserted epoch is now checked against the registry

`judge_commit` compares the caller's epoch against the *intent*, which is
sufficient on `Connect` — that epoch arrived on a stream the server itself
authenticated. `FetchSource` and `PushOutput` are separate RPCs whose epoch is
asserted by the caller in metadata, and a superseded instance can present the
very epoch its own intent was granted under, satisfying every check that only
compares the two against each other. Only the registry knows an epoch has been
retired. That check is now a shared `require_current_epoch` rather than a
second copy, because two copies of a fence are two fences that can drift.

It runs twice per push: once before the transfer, which only saves bandwidth,
and once immediately before the install, which is the one that actually fences
it. An epoch can be retired while bytes are in flight.

### The source guard, and what it does not cover

A mount-mode runner calls `SourceGuard::observe()` before the encode, so its
guard covers the encode. The server cannot: it is stateless between RPCs and
never watched the file. It builds the guard from the stored `file` row instead —
the scanner is the only writer of `size_bytes`, `mtime_unix` and `inode`
(`record_probe_op` updates twenty columns and none of those three), so the row
is what the job was planned against, which is what `SourceGuard` documents
itself as holding.

That covers the whole planning-to-install window rather than just the encode.
It is not total: a rescan between plan and install refreshes those columns to
match the new contents, so a source that changed *and was rescanned* still
passes. A narrower hole than mount mode's, not the absence of one.

### Refusals

A judged refusal is `PushOutputResponse{accepted: false, reason}` — the server
understood and declined, and the agent needs the reason to decide whether to
retry, re-register or stop. Transport failures stay `Status`: no identity, a
signature mismatch, a chunk that names a different job midway, or a stream that
ends without its `last` chunk. Those are not answers, and the argument
`FetchSource` makes applies here too — a failure that looks like a success is
worse than a failure.

`Sink` guards offsets but not identity, so it would happily append one job's
bytes to another's staging file at the correct offset and produce a file of the
right length and the wrong contents. Every chunk is checked against the job the
first one named. Every refusal after staging opens unlinks the partial.
