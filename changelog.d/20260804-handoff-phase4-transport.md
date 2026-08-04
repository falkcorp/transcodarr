### Changed

#### Handoff document brought up to date, and told about CI

`IMPLEMENTATION-HANDOFF.md` opens with what a reader most needs to know: no CI
had ever executed in this repository until 2026-08-04, which means any
"verified" claim made before that date was verified on one laptop and never
independently reproduced.

The phase table said Phase 4 was not started. In fact `CapacityLedger`,
`Dispatcher`, `Reconciler`, `ScheduleEngine`, retry/dead-lettering/quarantine
and the proto semantics had all landed after the document's previous edit —
they are simply connected to nothing, which the table now says instead. The
Phase 4 section lists what actually remains (`AgentRepo`, `AgentSession`,
`ConnectClient`, the `serve` loop and the milestone) in dependency order, and
records the three codegen facts that would otherwise cost an hour each to
rediscover: where `protoc` comes from, why `build_transport(false)` is
load-bearing, and why the buf against-reference needs `subdir`.

The "first action for the next session" section had been two phases stale.
