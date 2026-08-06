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
