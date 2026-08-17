<!-- file: changelog.d/20260817-gpu-jobs-ask-for-software-decode.md -->
<!-- version: 1.1.0 -->
<!-- guid: 4f2ab8d1-93c7-4e60-b5a2-7c1e0d64f8b3 -->
<!-- last-edited: 2026-08-17 -->

### Changed

#### A GPU job asks for the software decode it actually performs

`Requirement::Decoder` no longer varies by path: every video job asks for
software decode with an empty profile. A job is a GPU job because of
`AgentClass(Gpu)` and `Encoder(HevcNvenc)`, which is where its GPU-ness
genuinely lives.

Asking for `Nvdec` whenever the encoder was a hardware one looked right — the
reasoning, written into the test that asserted it, was that a hardware encoder
implies nothing about the decoder, which is the gap Hi10 falls through. The
reasoning is sound and the requirement was still wrong, because it described a
pipeline that does not exist. `build_ffmpeg_argv_raw` emits `-i <input>`
immediately before `-c:v` and appends its `extra` arguments *after* the codec
flags; `-hwaccel` is an input option and must precede `-i`, so the builder
cannot express one. **Every GPU job software-decodes and NVENC-encodes.**

The requirement was therefore strictly stricter than the work, and fail-closed
matching turns that into refusing jobs the card completes. Measured on a Turing
node: a 10-bit `High 10` source blocked at `capability` —

    no enabled, commit-eligible agent satisfies AgentClass(Gpu)
      + Encoder(HevcNvenc) + Muxer(Matroska)
      + Decoder(DecoderTriple { codec: "h264", profile: "High 10",
                                bit_depth: Ten, kind: Nvdec })

— while that job's exact argv, run by hand on the same node, finished 300
frames of `hevc_nvenc` at 248 fps with the duration preserved. `h264
High 4:2:2` and `av1 Main` were refused the same way, for the same reason. All
three dispatch now.

**This reaches jobs created after the upgrade, and only those.** A job's
requirements are serialised at creation and nothing rewrites them: `admin
evaluate` reports `evaluated 0` because `rules_version` hashes the policy
*config*, which this change does not touch, and `evaluate_one` returns
`already_busy` before it would recompute the spec anyway. A `Pending` job
created by an earlier binary therefore keeps its `Nvdec` requirement and blocks
permanently, naming a requirement no installed code can emit. There is no
cancel or requeue command, so the only recourse is editing the row by hand.
Tracked as `REQ-REFRESH`; it is a pre-existing property of how requirements are
stored, not something introduced here, but this is the first change to make it
bite.

This was only ever reachable once `decoders` stopped being empty: before that
every video job blocked regardless, so an over-constraint was invisible.

NVDEC is still trialled and `survey` still prints per-profile verdicts — the
profile carries a verdict that codec and bit depth do not, and that stays worth
knowing. It becomes dispatch-relevant the moment the plan builder can request a
hardware decode. Until then, requiring it buys nothing and costs real work.
