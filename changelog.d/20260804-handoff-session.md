### Changed

#### Handoff records what registration landed, and what `Connect` still needs

`AgentRepo` and `Register` are done and served over gRPC; `Connect` returns
`Unimplemented` on purpose. The document now says so, lists what the stream
still needs in dependency order, and records the three registration decisions
worth not relitigating — a rejection changes nothing in the database, a
reinstall takes a new epoch, and `commit_eligible` requires *every* mount to
have passed the rename probe.

It also flags a gap in the Phase 4 milestone: it asserts a
`transcodarr_dispatch_latency_seconds` p99, but `metrics.rs` is names-only with
no exporter until Phase 6, so that measurement will have to be made in-process.
That is not the artifact the milestone text describes, and the pull request that
produces it should say which one was actually made.
