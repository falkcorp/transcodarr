- [ ] **TODO-UNCALLED** Sweep the workspace for built, tested, and uncalled
      surface. Four for four, every component found in that state has been a real
      defect — `LocalWorker::recover()`, the muxer field at the conversion
      boundary, `hardening::decide_retry`, and `ScheduleEngine`. Given that base
      rate this is probably the highest-yield item outstanding. Frame each as a
      check, not an assertion: does anything in the dispatch path read or write a
      `DispatchBlockRepo` row; is `agent_capability_history` ever written when an
      agent's capabilities change; is `decide_retry`'s dead-letter arm reachable
      from the orchestrator or only from tests; does each of the seven existing
      store repositories have a non-test caller; and are the `metrics.rs` name
      constants referenced anywhere that increments them, or only declared. Use
      the Rust LSP `findReferences` rather than grep alone and filter out
      `#[cfg(test)]` modules and `tests/` — a symbol whose only references are
      test files is the finding. Fix what turns up in place rather than filing
      it.
- [ ] **TODO-AGENT-DEP** Make the agent/store separation a check instead of a
      sentence. The handoff states twice (lines 368-372, 766-770) that
      `transcodarr-agent` must never acquire a `transcodarr-store` dependency —
      it has to stay copyable to the Windows node without dragging SQLite along,
      and a shared proto crate makes violating it a one-line accident. Verified
      2026-08-06 that the invariant currently holds (`cargo tree -p
      transcodarr-agent -i transcodarr-store --edges normal` reports no such
      package in the graph, which is the answer to keep seeing) and that
      **nothing in CI enforces it** — no workflow references `cargo tree`. Add
      the check to `ci.yml`, then prove it can fail by temporarily adding the
      dependency before trusting it. A stated invariant with no mechanical check
      is this repository's signature defect class in documentation form.
