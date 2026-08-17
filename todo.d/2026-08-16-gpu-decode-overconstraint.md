- [ ] **GPU-NVDEC** Reconcile the `VideoGpu` decode requirement with the
      pipeline that actually runs — right now a GPU job requires a `VerifiedOk`
      NVDEC triple but never asks NVDEC to do anything. `build_ffmpeg_argv_raw`
      (`plan.rs:302`) emits `-i <input>` immediately before `-c:v`, and its
      `extra` arguments land *after* the codec flags, so an `-hwaccel` — an
      input option, which must precede `-i` — cannot be expressed by the
      builder at all. Every `VideoGpu` job therefore software-decodes and
      NVENC-encodes.

      Measured on `windows-rtx2070` 2026-08-16: a 10-bit `High 10` source
      blocks at `capability` on
      `Decoder(DecoderTriple { codec: "h264", profile: "High 10", bit_depth: Ten, kind: Nvdec })`,
      while that job's exact pipeline run by hand on the same node succeeds —
      300 frames in and out, duration preserved, `Lavc63.8.101 hevc_nvenc`,
      248 fps. Affected: `h264 High 4:2:2` and `h264 High 10`
      (`VerifiedSoftFallback`) and `av1 Main` (`VerifiedFail`).

      Two defensible resolutions, and they are not equivalent:

      1. **Make the requirement describe the work.** Emit the decoder
         requirement as `kind: Software` for GPU jobs too, leaving GPU-ness to
         `AgentClass(Gpu)` + `Encoder(HevcNvenc)`. Smallest change, unblocks
         Hi10 and 4:2:2 immediately, and demotes the NVDEC verdicts to
         reporting until a hardware decode path exists.
      2. **Make the work match the requirement.** Teach the plan builder to
         place input options before `-i` and emit
         `-hwaccel cuda -hwaccel_output_format cuda` for GPU plans. This is
         what `policy.rs:804` intends ("a hardware encoder implies nothing
         about the decoder, which is the gap Hi10 falls through") and it makes
         the soft-fallback verdicts genuinely load-bearing — but it needs
         pix_fmt rework, since frames then stay in device memory and `p010le`
         is a system-memory format.

      Until one is chosen, the verdict table is **not** dispatch-authoritative
      for the GPU path: it is strictly stricter than the work performed. This
      is pre-existing, not introduced by the trial-decode change — but that
      change is what made it bite, because previously every video job blocked
      regardless and an over-constraint was invisible.
