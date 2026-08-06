- [ ] **TODO-DI1** Render the DI-1 maximal-matching invariant as a CI-checked
      artifact — first of the three Phase 4 milestone items (architecture
      document line 2644). Required: a table enumerating every `DispatchEvent`
      against every conjunct of "a free slot coexists with unmatched eligible
      work", with a test per cell. The dispatcher implements the invariant;
      nothing renders it into a form a reviewer can diff. Confirmed 2026-08-06
      that `DI-1` appears only under `docs/` and nowhere in `crates/`, so this
      does not exist yet. Generate the table from the code or the test names into
      a checked-in artifact and have CI fail when the generated output differs
      from the committed one, the way a snapshot test does — a hand-maintained
      table drifts and then lies. Before trusting it, sabotage one cell's
      handling in the dispatcher and confirm the check goes red.
- [ ] **TODO-LOADTEST** Build the `FakeAgent` load test — second Phase 4
      milestone item. Many synthetic agents against one server on a loopback
      port, to find where the dispatch loop's assumptions break before the real
      fleet does. Confirmed 2026-08-06 that no `FakeAgent` exists anywhere in
      `crates/`; `transcodarr-server/tests/end_to_end.rs` is the shape to grow,
      and `transcodarr-agent/tests/connect_client.rs` does the mirror-image trick
      with a fake server. The milestone asserts 50k synthetic files with a full
      library scan running concurrently, `transcodarr_dispatch_latency_seconds`
      p99 <= 100 ms (R65/R66), and `transcodarr_agent_slots_idle_with_eligible_work`
      held at 0. Fifty thousand is the right order of magnitude: the real corpus
      is 49,600 files carrying 23,107 open Pending jobs. Build the synthetic jobs
      with the requirements a real `Evaluator` attaches — including
      `Muxer(Matroska)` — never `requirements_json: "[]"`, which is exactly why
      the existing end-to-end test stayed green for months while capability
      matching was completely broken. State in the PR body, rather than leaving a
      later reader to assume otherwise, that `metrics.rs` is names-only with no
      exporter until Phase 6, so the p99 is measured in-process against the same
      clock and is **not** the Prometheus histogram the milestone text describes.
      Blocked on TODO-DURABILITY: the durability probe fires on every DB open, so
      a load test that opens databases under load trips it constantly and its
      results are unreadable.
- [ ] **TODO-U1** Sustain 24 concurrent audio jobs on U1 (`172.16.2.35`,
      `unimatrixone-cpu`, 48 cores, root SSH from the server) — the third and
      final Phase 4 milestone item, and the only part that needs real hardware.
      Audio class only; the GPU node is Phase 5. Sizing, from production
      measurement: audio-only remux runs ~1.8-2.2 cores per job, so 24 slots
      demand roughly the whole box. This is a much lighter proposition than the
      24 *video* jobs that produced load 127 on 2026-07-30 at ~4.5 cores each.
      Tdarr is still in production alongside and `tdarr-ensure-node.py` re-arms
      worker counts every 3 minutes via cron, so any live change through the
      Tdarr API is silently undone unless that file is edited too — decide and
      state whether Tdarr is throttled for the run or left alone. `CPUQuota=3800%`
      is the hard backstop regardless of slot count. Do not declare this on a
      green suite: run it, read the logs, and inspect the installed files. Blocked
      on TODO-DI1 and TODO-LOADTEST — find the breaking points on loopback before
      spending real hardware and real media on them.
