<!-- file: TODO.md -->
<!-- version: 0.6.5 -->
<!-- guid: 12345678-90ab-cdef-1234-567890abcdef -->

# TODO

## 📥 Inbox

Tasks assembled from `todo.d/` fragments. Add a new task by dropping a fragment
file in `todo.d/` rather than editing this section by hand — see
[`todo.d/README.md`](todo.d/README.md). Checking a task off, or promoting it
into one of the curated sections below, is a normal direct edit.

<!-- todo-insert-here -->

- [ ] **GPU-NVDEC** Teach the plan builder to request a hardware decode, so the
      NVDEC verdicts the agent already measures become load-bearing.
      `build_ffmpeg_argv_raw` (`plan.rs:302`) emits `-i <input>` immediately
      before `-c:v` and appends its `extra` arguments *after* the codec flags.
      `-hwaccel` is an input option and must precede `-i`, so the builder
      cannot express one at all — every `VideoGpu` job software-decodes and
      NVENC-encodes.

      The over-constraint this caused is already fixed: the decode requirement
      now says `Software` with an empty profile, matching the work performed,
      which unblocked `h264 High 4:2:2`, `h264 High 10` and `av1 Main` on the
      Turing node. What remains is the *other* half — making the pipeline live
      up to the original intent at `policy.rs`, that "a hardware encoder
      implies nothing about the decoder, which is the gap Hi10 falls through."

      Needs, in order:

      1. An input-options slot in the argv builder, before `-i`. Everything
         today goes after it, so this is a structural change to the builder
         rather than another `extra`.
      2. `-hwaccel cuda -hwaccel_output_format cuda` on GPU plans, gated on the
         agent's trial verdict for that exact triple rather than on the class —
         `VerifiedSoftFallback` must *not* take the hardware path, because it
         silently decodes on the CPU while looking like a success.
      3. pix_fmt rework. Frames then stay in device memory and `p010le` is a
         system-memory format, so the current `pix_fmt_for` mapping
         (`policy.rs:276`) does not apply unchanged; a `format=cuda` filter or
         an explicit `hwdownload` is needed depending on the filter chain.
      4. Restore a decode requirement that names `Nvdec` and the real profile
         for plans that take the hardware path — the shape reverted here, but
         emitted only when the plan actually asks for `-hwaccel`.

      Worth doing for throughput, not correctness: software decode of 1080p
      h264 feeding NVENC already ran at 248 fps on that card, so this is not
      urgent.

- [ ] **REQ-REFRESH** A job's requirements are written once at creation and no
      command can refresh them, so a policy *code* change that alters emitted
      requirements never reaches jobs that already exist.

      Measured on a database created by the pre-change binary: a `VideoGpu` job
      blocked at `capability` on `Decoder(DecoderTriple { codec: "h264",
      profile: "High 10", bit_depth: Ten, kind: Nvdec })` still reads exactly
      that after `admin evaluate` against a binary that no longer emits
      `Nvdec` at all — `evaluated 0, 0 jobs created`.

      Two independent barriers, either of which alone is enough:

      1. `rules_version` (`policy.rs:327`) is `blake3(serde_json(policy))` — a
         hash of the policy *config*. A code change that alters requirement
         generation leaves it byte-identical, so `needs_eval` never returns the
         file and the evaluator loop exits having looked at nothing.
      2. `evaluate_one` (`evaluator.rs:155`) returns `already_busy` as soon as
         an open job exists for the file, which is *before* `next_job`
         recomputes the spec. So even forcing the file back into the working
         set would not rewrite `requirements_json`.

      There is no recourse: `admin` has no cancel, reset or requeue command
      (`diagnose`, `add-library`, `libraries`, `scan`, `evaluate`, `explain`,
      `run`, `summary`), so an operator's only option is editing SQLite by
      hand. The job blocks forever and `explain` names a requirement no
      currently-installed code can ever emit, which reads as a capability gap
      rather than a stale row.

      Needs, roughly in order:

      1. Decide the refresh rule. Requirements on a `Pending` job are pure
         derived data and safe to rewrite; a `Dispatched`/`Running` job has an
         agent holding a lease against the old set and must not be touched
         mid-flight. Refresh `Pending` only, leave the rest to finish or fail.
      2. Rewrite `requirements_json` *and* `requirements_bucket_key` together —
         dispatch matches on the bucket key, so refreshing one without the
         other is worse than refreshing neither.
      3. A way to force the file back into the working set, since barrier 1
         means the policy hash will not do it. Either an `admin evaluate
         --all` that ignores the recorded `rules_version`, or fold a build
         identifier into `RulesVersion` so a code change invalidates decisions
         the way a config change already does. The second is more honest about
         what the version means but re-evaluates every file on every upgrade.
      4. ~~An `admin jobs cancel <id>` regardless, as the escape hatch for
         every other way a job can become permanently unsatisfiable.~~
         **Done 2026-08-18.** `--force` covers a job an agent holds;
         `Committing` is refused regardless. Items 1–3 are still open, so a
         stale job must still be cancelled by hand rather than refreshed.

      Found while shipping the `Software` decode requirement: that change is
      correct for jobs created after it, and invisible to jobs created before.

- [ ] **TODO-TRANSPORT-2** Specify and build the second transport mode — gRPC
      byte streaming — which the design intended and the architecture document
      lost. The owner's stated intent was two modes chosen per node: **direct
      access** with per-node path translation, and **streaming**, where the
      server sends the source bytes, the agent saves them locally, converts, and
      streams the result back, so the node needs to know nothing about the
      server's storage. Only the first exists. Verified 2026-08-10 that
      `distributed-architecture.md` contains **no** mention of upload, download,
      transfer, fetch or byte ranges, and that `Requirement::MountCovers` is
      unconditional — line 123 makes an untranslatable path an ineligibility, and
      even the GPU video example at line 1644 carries `MountCovers`. The proto
      matches: `Register` and a bidirectional `Connect` stream, with no message
      carrying file content. So the code faithfully implements the spec; the spec
      is what dropped the requirement. Fix the document first, then the code.
- [ ] **TODO-TRANSPORT-2-COMMIT** Decide who performs the commit ritual in
      streaming mode, because the agent cannot. An agent that receives bytes and
      returns bytes has no path to the destination, so the nine-step ritual —
      rename original to trash, install replacement, resolve the intent — has to
      move server-side. Note this is **already half-designed under another
      name**: the architecture document specifies that if the WSL2 node fails
      `RenameProbe`, "the GPU agent becomes produce-only and a U0-local agent
      performs commits". Produce-only *is* streaming mode's agent. Reuse that
      shape rather than inventing a second one, and make `MountCovers` a
      requirement only mount-mode nodes must satisfy instead of an unconditional
      one — that is a dispatch-matching change, not merely a file-copying
      feature.
- [ ] **TODO-TRANSPORT-2-UNBLOCKS-GPU** Note when scheduling the above: streaming
      mode is what lets `windows-rtx2070` do real work without a mount. As of
      2026-08-10 all three of its SMB mounts to the server report `Unavailable`
      (`W:` bigdata\books, `X:` bigdata, `Y:` winbackup), so under mount-only
      transport that node is undispatchable no matter how well its encoder works
      — and NVENC there is confirmed working (RTX 2070 SUPER, driver 610.47,
      `hevc_nvenc` encoded a synthetic clip successfully on 2026-08-10). Either
      restore the mounts or build streaming; streaming is the one that also
      removes the node's need to know anything about server paths.
- [ ] **TODO-WIN-SESSION** Run the Windows preflight in the *same logon session
      the agent will run in*, and record which context that is. Windows drive
      mappings are per-user **and per-logon-session**, and an elevated session
      gets a separate set from the interactive one (UAC linked logons).
      Demonstrated 2026-08-10: over SSH the session is `jfg\jdfalk`, `ELEVATED`,
      `net use` shows `W:`/`X:`/`Y:` as `Unavailable`, a direct UNC to
      `\\172.16.2.30\bigdata` returns "Access is denied", and `cmdkey /list`
      shows no stored credential for that host. The same mappings are fine in
      the owner's interactive desktop session.
      **The consequence for this project:** "the mounts work once the software
      starts" holds only if the agent runs in the interactive user session. A
      Windows **service** runs in session 0 with no mapped drives and usually no
      network credentials, and anything launched over SSH gets the separate
      elevated session proven above. So the launch mechanism is a correctness
      requirement, not an ops detail.
      This also invalidates the obvious way to run Phase 0 there: a `RenameProbe`
      executed over SSH-as-admin tests a context the agent never uses, so a pass
      or a fail would both prove nothing about the real one. Decide how the agent
      is launched first, then preflight inside that context. Prefer **UNC paths
      over drive letters** in the mount table regardless — a drive letter is a
      per-session alias, while a UNC path is not, though it still needs
      credentials in whatever session resolves it.

- [ ] **TODO-SCRIV** Dry-run `changelog-collect.yml` before the first real
      release, because the assembling half of the changelog system has never
      executed in this repository. The workflow fires only on `release:
      published` (or manual `workflow_dispatch`), there has been no release, and
      63 fragments have accumulated since 2026-07-19 — consumed fragments are
      deleted, and none has ever been. The gate half works: `changelog-check.yml`
      greps the PR diff for an added fragment and is exercised on every PR. It is
      the collect half that is untested, which is this repository's signature
      defect shape — a documented mechanism nobody has run. Verified 2026-08-06
      that `scriv collect` itself succeeds (exit 0, all 63 fragments consumed,
      three category headings emitted) but that its **output failed the repo's
      own markdownlint config** on `MD033` (the `<a id='changelog-vX.Y.Z'></a>`
      anchors scriv emits) and `MD022` (that anchor sits flush against the `##`
      version heading). `CHANGELOG.md` is now excluded from lint for that reason.
      Since collect commits with `[skip ci]`, any remaining problem in its output
      lands silently and reddens the next unrelated docs PR instead — so run it
      via `workflow_dispatch` against a scratch tag and read the diff before
      trusting the first real release. Check two things the test run did not
      settle: scriv does not bump `CHANGELOG.md`'s own `version:` header and the
      file has no `last-edited:` line, both of which the repo-wide header rule
      wants.

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

- [x] **TODO-DURABILITY** Fix the flaky `DurabilityTooSlow` gate — `main` does
      not pass `cargo test` on the Mac, and it fails nondeterministically. Two
      tests failed on one run (`foreign_keys_are_enforced_not_merely_declared`,
      p99 109,934 us; `only_one_live_commit_intent_per_final_path`, p99 111,621
      us) and a different one on the next (`only_one_open_job_per_file`). All
      panic identically on an unwrapped `DurabilityTooSlow { limit_us: 100000 }`
      at `crates/transcodarr-store/src/db.rs:244`. The durability probe runs on
      every DB open, so a wall-clock threshold gates every test that opens a
      database, and macOS `/var/folders` fsync p99 sits ~10% over the limit under
      any load. This matters beyond the noise: a suite that fails randomly trains
      everyone to re-run instead of read, the M1 "green triple" exit criterion is
      not actually being met locally, and Linux CI runners hide it. Do **not**
      simply raise the constant — 100 ms is a deliberate production figure and
      the ZFS pool is latency-bound. Prefer making the limit an open-time
      parameter with a production default, so the check stays present everywhere
      rather than absent where most code runs.
- [x] **TODO-PIPEFAIL** Stop piped verification commands reporting success for a
      failed run. `cargo test --workspace 2>&1 | tail -30` returned **exit 0**
      while two tests failed, because a pipeline's status is the last command's
      and `tail` always succeeds. This nearly buried TODO-DURABILITY. It is the
      same shape as the documented "clippy fails open" trap: verify what the
      command concluded, not that the shell returned 0. Audit the verification
      commands in the handoff, `CLAUDE.md` and the workflows for piped
      invocations, and require `set -o pipefail` (or no pipe at all).

- [ ] **TODO-GPU-PREFLIGHT** Run `transcodarr admin diagnose --preflight` on
      `windows-rtx2070` and record the result in `PHASE0-RESULTS.md` beside U0 and
      U1. This is the one piece of Phase 0 never executed, and it is an
      architecture decision rather than a checkbox: architecture document line
      2577 is explicit that if the WSL2 node fails `RenameProbe`, "the
      architecture changes **here, not later** — the GPU agent becomes
      produce-only and a U0-local agent performs commits. Discovering that after
      the dispatcher exists costs weeks." The dispatcher now exists, so this is
      already later than the design wanted, and Phase 5 is entirely about the GPU
      class — this is the last cheap moment. What hangs on the answer:
      `commit_eligible` requires **every** mount to have passed the rename probe,
      not merely one, and `RP_UNTESTED` grants nothing, so an unrun probe means
      the GPU node cannot commit at all and the produce/commit split has to be
      designed in rather than bolted on. Reaching the node may need the owner —
      the access path is not documented the way SSH to U0 and U1 is. If it cannot
      be reached unattended, say so and design Phase 5 under the explicit
      assumption that the GPU agent may be produce-only rather than assuming it
      can commit.

- [ ] **TODO-TDARR** Re-check live Tdarr, which is still the production system
      while transcodarr is built. The handoff's snapshot is from 2026-08-01 — five
      days stale, and it describes a scheduled `at` job that has long since fired.
      This is the owner's actual media pipeline, so a regression here is a real
      outage rather than a test failure. Re-check, all on the server: queue depth
      and lifetime reclaim against the snapshot of 6,575 files queued (anime
      5,209, TV 780, movies 586) and 2,634 GB reclaimed; and `queueSortType`,
      which regressed to `sortSizeSmallest` once and was set back to `noSort` —
      the single biggest dispatch win, because the single-threaded server re-sorts
      the entire queue on every dispatch request. It has regressed before, so
      confirm rather than assume. Decide whether to revert `UNI_WORKERS` in
      `~/ai/tdarr/tdarr-ensure-node.py`, which the 2026-08-01 `at` job raised from
      4 to 16 for an away period that has ended — ask the owner rather than
      assuming, since 16 slots is a noise and thermal decision, not a technical
      one. Note that `tdarr-ensure-node.py` re-arms worker counts every 3 minutes
      via cron, so any change made live through the Tdarr API is silently undone
      unless that file is edited too, and those scripts are not in git.
- [ ] **TODO-TDARR-PARKED** Decide what to do about the work parked in live Tdarr,
      all four items deliberately left unfixed on 2026-08-01 in favour of a
      throughput bump. Flow `hqhevcnvenc2` has both branches of its `gpugate`
      `customFunction` returning `outputNumber: 1`, so a CPU worker falls straight
      into the video encoder; the CPU branch must return `outputNumber: 2` wired
      to a requeue terminal, and **3,558 anime video files are parked behind
      this**. `windows-rtx2070` has 3 GPU slots idle because the audio-only flow's
      `cpugate` requires `workerType == CPU`, so GPU slots take work and bail. And
      242 files sit in `Transcode error` (anime 125, TV 88, movies 29). Worth
      cross-checking against TODO-PROBE: transcodarr's own probe run just failed
      on 2,271 files, and if those sets overlap they are one problem with two
      symptoms — Tdarr's errors would be a free labelled sample of what
      transcodarr cannot read.

- [ ] **TODO-P2-MILESTONE** Close out the Phase 2 milestone, now that the probe
      run has finished — `awaiting probe 0` on all three libraries. Two
      assertions remain, one command each. The `admin summary` decision/GiB
      breakdown is already captured: anime 17,825 files / 16,083.0 GiB with 6,916
      Pending jobs; movies 2,432 / 37,232.5 GiB with 803; tv 29,343 / 29,498.8
      GiB with 15,388 — 49,600 files, 82,814 GiB and **23,107 open Pending
      jobs**. Record that in the milestone. Then run `admin evaluate --force` for
      the "re-derive every decision with zero filesystem I/O" claim, and confirm
      the decisions come out identical to the stored ones rather than treating
      the absence of an error as proof. Note the scale this reveals: 23,107
      queued jobs against a fleet that has run exactly one job end to end, and
      nearly 3.5x the depth live Tdarr is currently carrying. `admin config
      validate --diff` stays deferred to Phase 6 — no configuration format exists
      to validate, and inventing one now means guessing at settings Phases 4-6
      define.

- [ ] **TODO-P3-MILESTONE** Run the Phase 3 200-file milestone with a
      `file_stream` diff harness — the last genuinely outstanding Phase 3 item
      (architecture document line 2634, proof 2): 200 real files transcoded end
      to end on U1 with byte-exact track preservation verified by diffing input
      and output `file_stream` rows. Ten files have run and the diff harness does
      not exist. Those ten do not count: they executed on the pre-#41 binary, so
      their validations compared header duration to header duration — consistent
      and therefore safe, but not the intended last-packet-PTS guard. The other
      two Phase 3 items the handoff still lists are done and should not be
      carried forward (see TODO-HANDOFF). The diff must catch the production
      rules that were expensive to learn: every audio **and** subtitle stream
      preserved, because a bare `-c:a eac3` silently drops all but the default
      track; bit depth preserved and never upconverted from 8-bit; HDR and Dolby
      Vision video never re-encoded; lossless (TrueHD/DTS/FLAC/PCM/MLP) and Opus
      converted to EAC3 640k while aac/ac3/eac3/mp3 are left alone. Size is never
      an accept criterion — a truncated file is always smaller — so duration
      compares against last-packet PTS, not the container header, and the
      duration gate runs before the size gate.

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

- [ ] **TODO-PHASE5** Phase 5 — GPU class, capability probing, emergent
      two-stage. Deliberately coarse: this is a multi-session unit, and the
      handoff's scope note is explicit that attempting a phase in one pass
      produces half-built subsystems, which is worse than none. Decompose it into
      real tasks when it starts. Milestone (architecture document line 2654),
      verified live: AV1 and Hi10 H.264 files are **never** dispatched to
      `windows-rtx2070` with an NVDEC requirement and
      `transcodarr_agent_rejections_total` stays 0 across a 500-file run; and a
      TrueHD 10-bit HEVC file runs audio-on-U1 then video-on-GPU with no phase
      column anywhere in the schema, finishing at exactly one job per stage, with
      `idx_job_open_per_file` proving no double-dispatch. The two-stage behaviour
      must **emerge** from capability matching rather than be encoded as a
      pipeline. Measured hardware limits that must shape the design, because
      rediscovering them is expensive: Turing NVDEC cannot decode AV1 at all
      (ffmpeg exit 69, ~1 KB truncated output, hard failure) and cannot decode
      10-bit H.264 (silent soft fallback to software), so hardware decode is
      gated per codec and never enabled globally; NVENC on the RTX 2070 reaches
      aggregate 71 / 101 / 117 fps for 1 / 2 / 3 sessions, with the encoder ASIC
      pinned at 75-100% while the GPU cores idle near 20%, so beyond about three
      sessions there is nothing to gain; and NVENC wants `p010le` for 10-bit
      where libx265 wants `yuv420p10le`, with the wrong one erroring the job.
      There is real demand waiting — 20,305 files carry a Video decision across
      the three libraries, and live Tdarr has 3,558 anime video files parked
      behind a broken GPU flow. Blocked on the Phase 4 milestone, and read
      TODO-GPU-PREFLIGHT's result first: if that node cannot commit, the
      produce-only split changes this design.

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

- [ ] **TODO-PROBE** Investigate the 2,271 files that failed to probe — 1,105
      anime, 316 movies, 850 tv, totalling roughly 11.9 TiB. The handoff
      predicted 8, so the rate is not "normal" and was never checked. The
      failures skew enormously large: movies averages ~29 GiB per failed file.
      Worse than the count is where they land — `(not evaluated)`, not
      `Quarantined`, so they are invisible to the dispatcher rather than refused
      once with a reason. Rejecting with a reason instead of silent invisibility
      is a founding requirement of this project and the specific Tdarr behaviour
      it exists to replace. Pull the failing paths and recorded reasons out of
      `~/tc/tc.db` on the server, group by reason and container, probe a handful
      by hand with `ffprobe`, and decide whether this is a prober defect or
      genuinely unreadable media. Either way make the outcome explicit. Rule out
      first: probe timeout at `--probe-concurrency 32` under load (large files
      take longest), ZFS latency on 40-80 GB files, and the `-read_intervals`
      class of bug that PR #41 already fixed once in `last_packet_pts_us`.

- [ ] **TODO-RECONCILE** Reconcile the curated sections of `TODO.md` against the
      Phase 0-7 plan. Those sections still describe the single-binary transcoder
      that the workspace split retired, and they are stale in a way that
      misleads: a reader takes them as the project's backlog. Retire the items
      obsoleted by the split into six crates and the distributed design — "embed
      static ffmpeg build", "add preset (web-optimized)", "progress reporting
      with ETA during batch conversion", "resume capability for interrupted
      batches" and "quality comparison reports" all describe `transcodarr local`
      rather than the orchestrator. Map the survivors onto phases instead of
      leaving them loose: hardware acceleration (VAAPI, NVENC, VideoToolbox) is
      Phase 5; "parallel processing for batch operations" is already the
      dispatcher, so delete it; proptest, cargo-mutants, fuzzing, tarpaulin and
      performance regression detection are Phase 7 (see TODO-PHASE7); "verify
      metadata preservation in tests" is the Phase 3 `file_stream` diff harness;
      and "quality validation tests" are already the validation gates — confirm
      and delete. Retirements and removals are normal direct edits of `TODO.md`,
      since the fragment system is add-only. While editing, fix the file's own
      header: it carries the placeholder guid
      `12345678-90ab-cdef-1234-567890abcdef` and has no `last-edited:` line at
      all.

- [ ] **TODO-GUIDS** Clear the small known-defect backlog in one PR — each item
      is currently a small lie in the repository. The guid
      `a1b2c3d4-e5f6-7890-abcd-ef1234567890` is shared by `.editorconfig`,
      `.github/dependabot.yml` and `.github/workflows/ci.yml`; a guid that
      identifies three files identifies none. `TODO.md` carries the placeholder
      `12345678-90ab-cdef-1234-567890abcdef` and no `last-edited:` line (overlaps
      TODO-RECONCILE — do it wherever lands first).
      `docs/design/distributed-architecture.md` still has `<new uuid>`
      placeholders at lines 169 and 214 inside example `Cargo.toml` blocks; known
      and harmless, but they have been known for a while. And
      `task-inventory.json` carries no file header because JSON cannot hold
      comments — either accept that permanently and say so where the header rule
      is stated, or add a sibling `task-inventory.header.md`, because right now
      it is an unexplained exception.
- [ ] **TODO-INVENTORY** Mark `docs/design/task-inventory.json` clearly as
      reference-only at the top of the file, or audit it. All 414 tasks were
      produced by a workflow interrupted before its verification pass, were never
      reconciled against the architecture document, and may be missing whole
      subsystems. The Phase 0-7 plan supersedes it. Its sheer size makes it look
      authoritative, which is precisely the risk — it must never be handed to
      implementer agents as-is.
- [ ] **TODO-DIRNAME** Rename the local clone from
      `~/repos/github.com/jdfalk/transcoderr` to match the repository and crate
      name `transcodarr`. Purely cosmetic and the owner's convenience. **Confirm
      before touching it** — it will break every absolute path in the handoff, in
      shell history, and in any running background job.

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

## Completed

- [x] Basic transcode command with metadata preservation
- [x] Git LFS setup for test media
- [x] Batch processing for recursive directory conversion
- [x] Dry-run mode for batch operations
- [x] GitHub Actions CI for lint, build, and basic smoke test
- [x] Add presets (original-h265, tv-h265-fast, movie-quality)
- [x] Comprehensive integration test suite with test media
- [x] Benchmark suite using Criterion
- [x] Test utilities and helpers (common module)
- [x] Testing documentation (TESTING.md)

## In Progress

- [ ] Test CLI functionality (run with actual test files)
- [ ] Progress reporting with ETA during batch conversion
- [ ] Resume capability for interrupted batches (skip already converted files)

## Planned

### High Priority

- [ ] Embed static ffmpeg build from <https://github.com/jdfalk/FFmpeg-Builds>
- [ ] Add preset (web-optimized)
- [ ] Verify metadata preservation in tests (compare input vs output metadata)
- [ ] Add quality validation tests (ensure transcoded files play correctly)

### Medium Priority

- [ ] Extend metadata preservation options (cover art, chapters)
- [ ] Hardware acceleration support (VAAPI, NVENC, VideoToolbox)
- [ ] Parallel processing for batch operations
- [ ] Quality comparison reports (original vs. transcoded file sizes)
- [ ] Add code coverage reporting (tarpaulin)

### Low Priority

- [ ] Property-based testing with proptest
- [ ] Mutation testing with cargo-mutants
- [ ] Fuzz testing for CLI parsing
- [ ] Performance regression detection in CI
