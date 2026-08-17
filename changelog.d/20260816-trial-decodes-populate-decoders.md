<!-- file: changelog.d/20260816-trial-decodes-populate-decoders.md -->
<!-- version: 1.2.0 -->
<!-- guid: bc57d73f-fa30-4536-bb8a-521980160fb8 -->
<!-- last-edited: 2026-08-16 -->

### Added

#### Agents now trial-decode, so `decoders` is no longer empty

An agent generates ten-frame `lavfi` clips at registration, decodes each one
both ways, and reports what actually happened. Until now `Capability.decoders`
was always `Vec::new()`, which meant every hardware-decode requirement went
unmet and no agent could be given video work at all.

Measured on `windows-rtx2070` (Turing) on 2026-08-16, the card disagrees with
`ffmpeg -decoders` four times over, and not in a pattern any single flag
captures:

| triple | verdict |
|---|---|
| `h264` / `High`, `Main`, `Constrained Baseline` / 8-bit | hardware |
| `h264` / `High 4:2:2` / 8-bit | **silent CPU fallback** |
| `h264` / `High 10` / 10-bit | **silent CPU fallback** |
| `hevc` / `Main 10` / 10-bit | hardware |
| `av1` / `Main` / 8-bit | hard fail, exit 69 |

10-bit HEVC decodes in hardware while 10-bit H.264 does not, so "10-bit is
unsupported" is wrong in both directions; and `High 4:2:2` fails at the *same*
codec and bit depth as profiles that work, so NVDEC's verdict genuinely varies
by profile and the triple has to carry one.

Both video paths were then run end to end, server on the Mac and `--transport
stream`, judged on **frame counts rather than file size** — a decode that stops
early still writes a smaller, structurally valid file:

| path | job | source → output | frames | encoder in the bitstream |
|---|---|---|---|---|
| GPU (`windows-rtx2070`) | `VideoGpu` | h264 `High` 20.2 Mbps → hevc `Main` 2.4 Mbps | 627 → 627 | `Lavc63.8.101 hevc_nvenc` |
| CPU (Mac) | `VideoCpu` | av1 `Main` 1.2 Mbps → hevc `Main` 0.7 Mbps | 240 → 240 | `Lavc62.28.102 libx265` |

### Changed

#### Software decode requirements no longer name a profile

`Requirement::Decoder` is emitted with an empty profile on the software path and
the file's real profile on the hardware path. Software decode does not vary by
profile — ffmpeg either has the decoder compiled in or it does not — whereas
NVDEC's verdict does. Keying both on the profile meant any profile absent from
the candidate list blocked the *CPU* path too; `Main` H.264, which is what much
real library media carries, was one such.

The AV1 case is what proves the asymmetry works. The GPU agent reports av1
NVDEC as `VerifiedFail` (exit 69) and advertises no `av1`/`Main` entry at all —
only `av1`/*any profile*/8-bit/software. Matching is exact equality and
fail-closed, so that job could dispatch only because the software requirement
carried an empty profile.

#### Trial decodes are judged on frames rather than bytes written

A trial now runs to `-f null -` and reads its frame count from
`-progress pipe:1`. The previous rule counted output bytes, which forced every
trial to write a real file into the same work area the job transport stages
into, and could not tell a decode that stopped at frame three from one that
finished. Fallback detection still takes precedence over the frame count,
because a soft fallback decodes every frame — on the CPU.

#### `survey` prints the decode verdicts

A card that soft-falls-back is identical to a working one everywhere else in
that output.

#### Known limitation: a `VideoGpu` job requires NVDEC but does not use it

`build_ffmpeg_argv_raw` (`plan.rs:302`) goes from `-i <input>` straight to
`-c:v`, and its `extra` arguments are appended *after* the codec flags —
`-hwaccel` is an input option and must precede `-i`, so the builder cannot
express one at all. Every `VideoGpu` job therefore **decodes in software and
encodes on NVENC**, while `policy.rs` requires a `VerifiedOk` NVDEC triple for
it.

That requirement is deliberate — the test at `policy.rs:804` says "a hardware
encoder implies nothing about the decoder, which is the gap Hi10 falls
through" — but it describes a full NVDEC→NVENC pipeline the plan builder never
grew. Populating `decoders` is what makes the divergence bite: previously every
video job blocked regardless, so an over-constraint was invisible.

Measured, not inferred. A 10-bit `High 10` source blocks:

    no enabled, commit-eligible agent satisfies AgentClass(Gpu)
      + Encoder(HevcNvenc) + Muxer(Matroska)
      + Decoder(DecoderTriple { codec: "h264", profile: "High 10",
                                bit_depth: Ten, kind: Nvdec })

while that job's exact pipeline, run by hand on the same node, succeeds — 300
frames in and out, duration preserved, `Lavc63.8.101 hevc_nvenc`, 248 fps. The
three triples affected are `h264 High 4:2:2` and `h264 High 10` (both
`VerifiedSoftFallback`) and `av1 Main` (`VerifiedFail`).

Until this is resolved the verdict table is **not** dispatch-authoritative for
the GPU path: it is stricter than the work performed.

### Fixed

#### `journal.rs` no longer imports `File` on a target that cannot use it

`std::fs::File` is named there exactly once, behind `cfg(unix)`. Imported
unconditionally it was an unused import on the Windows target — which CI never
builds, being Linux-only, so the warning was reachable only by the
cross-compile that produces the agent this crate exists to ship.
