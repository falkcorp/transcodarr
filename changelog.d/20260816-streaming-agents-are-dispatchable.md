<!-- file: changelog.d/20260816-streaming-agents-are-dispatchable.md -->
<!-- version: 1.0.0 -->
<!-- guid: 9d31f7a8-4b60-42e5-8c19-06fa2be5d73c -->
<!-- last-edited: 2026-08-16 -->

### Fixed

#### A streaming agent could never be dispatched to

`commit_eligible` required `!mounts.is_empty()`. A `TM_STREAM` agent advertises
**no mounts by design** — `agent.proto` says so outright — so it was permanently
ineligible, and `Dispatcher::place` skips an ineligible agent as a candidate
outright. Streaming was unreachable in production: the server would register the
agent, report `blocked=1` every tick, and never hand it a thing.

The rename that must be atomic under streaming is between the *server's* work
directory and the library, because the server performs the install in
`push_output`. That is a property of the server's filesystem and nothing the
agent can attest to in either direction. The mount rule is unchanged for mount
agents, so the exemption is scoped to the transport rather than being a hole in
the Phase 0 probe.

`rename_probe_status` still reports what the *mounts* proved, so a streaming
agent stores `untested` while being commit eligible. That is not a
contradiction: it has no mounts to have probed.

**Found by running the two real binaries against each other.** 573 tests passed
over it, because every dispatch test registers agents through a harness that
sets `commit_eligible: true` directly — bypassing the rule. A fixture that
asserts the precondition it exists to exercise cannot fail on it.

#### ffmpeg's `-progress` sink is cleaned up

`encode` created `<temp>.progress` and nothing removed it. A successful
mount-mode install *renames* the temp file away and a failure removes it;
neither ever knew about the sibling. One 200-byte file per job, forever, in a
work directory that otherwise looks empty. Pre-existing in mount mode; noticed
because a streaming work area is swept and this was what was left in it.
