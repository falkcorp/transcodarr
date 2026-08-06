- [ ] **TODO-PHASE6** Phase 6 — observability, schedules, UI. Coarse by design; a
      multi-session unit covering a metrics subsystem, a scheduler and a web UI.
      Decompose when it starts. Milestone (architecture document line 2662):
      unplug the GPU node mid-run, and `/api/v1/diagnose` returns the correct
      first blocking stage with evidence and a suggested action, with
      `transcodarr admin diagnose` rendering it over SSH and no browser. This
      phase carries two debts from earlier ones. First, the metrics exporter:
      `metrics.rs` is names-only — 217 lines of constants — so until this lands
      every latency claim in the project is measured in-process against a local
      clock, including the Phase 4 p99. When the exporter exists, re-assert that
      p99 against a real Prometheus histogram and correct the record. Second,
      `admin config validate --diff`, specified for Phase 2 but deferred because
      it needs a configuration file format that does not exist; building it
      earlier meant guessing at the schedule and dispatch settings this phase
      defines. Build the config subsystem here, then the validator. Schedules are
      partly done — `ScheduleEngine` exists and has been consulted by the
      orchestrator since PR #69 — so what is missing is the operator-facing side:
      defining windows and pause overrides somewhere other than code. The owner
      already runs Prometheus alert rules and an exporter inventory under
      `~/ai/monitoring/prometheus/` on the server; read those before inventing
      metric plumbing. Blocked on Phase 5.
