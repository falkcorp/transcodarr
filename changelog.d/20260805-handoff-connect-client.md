<!-- file: changelog.d/20260805-handoff-connect-client.md -->
<!-- version: 1.0.0 -->
<!-- guid: 2f8b5c40-6d19-4e73-a05f-9c14b7e03d68 -->
<!-- last-edited: 2026-08-05 -->

### Changed

#### Handoff: the agent speaks now, and two defects it uncovered

`IMPLEMENTATION-HANDOFF.md` said `ConnectClient` did not exist. It does, along
with `LocalWorker`, so the Phase 4 table, the crate table, the PR list and the
next-session instruction all moved. The next action is `serve` and
`agent connect`, then the dispatch loop — `Dispatcher`, `CapacityLedger`,
`Reconciler` and `ScheduleEngine` all exist and none of them has a caller.

Also recorded: the four client rules worth not relitigating, and the two defects
found in what the client was built on — a journal that could never be recovered,
and a `boot_id` that was the machine's rather than the process's. Both were in
shipped, tested code; neither had a test that could fail.
