- [ ] **TODO-HANDOFF** Correct `docs/design/IMPLEMENTATION-HANDOFF.md`, the
      documented entry point, which is wrong in five verified places. (1) Lines
      761-764 say `ScheduleEngine` is still not wired in and that
      `Orchestrator::tick` never asks it anything — false since PR #69; see
      `orchestrator.rs` lines 113, 142, 153, 212 and 649-659. (2) Both remaining
      "Still outstanding in Phase 3" items are done — `runner.rs:301`
      `CommitIntentRepo::grant_op`, `:330` `resolve_op`, `:340`
      `TrashRepo::retain_op`. (3) Line 455 claims 511 tests while
      `NEXT-SESSION.md` claims 515; assert neither, take the number from a run.
      (4) The PR table stops at #67 — add #69 (retry budget, `Eligible` in the
      queue, `ScheduleEngine` consulted) and #70 (docs lint). (5) The Phase 2 row
      still says probe ingestion was running at handoff; it has finished. A stale
      entry point is worse than a missing one, because it is believed.
