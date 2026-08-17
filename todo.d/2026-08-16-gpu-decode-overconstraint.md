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
