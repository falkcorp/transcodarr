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
