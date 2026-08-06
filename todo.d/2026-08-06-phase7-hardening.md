- [ ] **TODO-PHASE7** Phase 7 — hardening. Coarse by design; decompose when it
      starts. Milestone (architecture document line 2670): a rolling upgrade of
      both agents under full load with zero job losses and zero temp files left
      behind. This is where the fencing machinery built in Phase 4 finally gets
      its real test, under the epoch rules already settled and not to be
      relitigated — one `boot_id` per process reused across every reconnect,
      because a fresh one per attempt turns every network blip into an epoch
      bump; a stale epoch can neither open a stream nor resolve a commit; and
      `AgentTable` keeps one connection per agent, newest wins, with `disconnect`
      epoch-guarded so a slow teardown cannot evict the replacement that just
      arrived. Fold in here the test-infrastructure items that survive the
      workspace split and belong to hardening rather than to a phase: code
      coverage (tarpaulin), property-based testing (proptest), mutation testing
      (cargo-mutants), fuzzing the CLI parser, and performance regression
      detection in CI. Mutation testing deserves particular weight in this
      repository, whose recurring defect is a check that cannot fail — a crash
      matrix that could not fail, a lint that had never run, a repository lookup
      that could only return `None`, and a load-bearing test built with
      `requirements_json: "[]"`. `cargo-mutants` is the mechanical form of the
      question this project keeps having to ask by hand. Blocked on Phase 6.
