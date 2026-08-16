<!-- file: changelog.d/20260816-fetch-source.md -->
<!-- version: 1.0.0 -->
<!-- guid: 8bea84a7-588e-4b23-8108-2a3ae2cc6151 -->
<!-- last-edited: 2026-08-16 -->

### Added

#### `FetchSource` serves source bytes to a streaming agent

The first half of the streaming transport moves real bytes. A `TM_STREAM` agent
can pull the source for a job it holds; `PushOutput` still refuses, so nothing
installs yet.

Two gates decide whether bytes leave the server, and the second is the one worth
describing. The epoch in the request is checked against the registry, which
proves only that the caller is a current instance of *some* agent — so the job
row is checked too, and it must name this caller at this epoch. Without that,
any live agent that learned a `job_id` could pull another agent's source. The
caller is named by `x-agent-id` metadata rather than a new proto field, matching
the argument `Connect` already makes: identity belongs to the transport, not to
the reviewed schema.

A refusal is a `Status`, never an empty stream. An empty-but-successful stream
is indistinguishable from a zero-byte file at the receiver, which would quietly
turn "you may not have this" into "this file is empty" — and the receiving side
would then verify a blake3 of nothing and be satisfied.
