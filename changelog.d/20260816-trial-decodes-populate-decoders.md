<!-- file: changelog.d/20260816-trial-decodes-populate-decoders.md -->
<!-- version: 1.1.0 -->
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
codec and bit depth as profiles that work, so the profile has to stay part of
the key.

### Changed

#### Software decode requirements no longer name a profile

`Requirement::Decoder` is emitted with an empty profile on the software path and
the file's real profile on the hardware path. Software decode does not vary by
profile — ffmpeg either has the decoder compiled in or it does not — whereas
NVDEC's verdict does. Keying both on the profile meant any profile absent from
the candidate list blocked the *CPU* path too; `Main` H.264, which is what much
real library media carries, was one such.

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

### Fixed

#### `journal.rs` no longer imports `File` on a target that cannot use it

`std::fs::File` is named there exactly once, behind `cfg(unix)`. Imported
unconditionally it was an unused import on the Windows target — which CI never
builds, being Linux-only, so the warning was reachable only by the
cross-compile that produces the agent this crate exists to ship.

### Verified

Both video paths were run end to end after the change, server on the Mac and
`--transport stream`:

| path | job | source → output | frames | encoder in the bitstream |
|---|---|---|---|---|
| GPU (`windows-rtx2070`) | `VideoGpu` | h264 `High` 20.2 Mbps → hevc `Main` 2.4 Mbps | 627 → 627 | `Lavc63.8.101 hevc_nvenc` |
| CPU (Mac) | `VideoCpu` | av1 `Main` 1.2 Mbps → hevc `Main` 0.7 Mbps | 240 → 240 | `Lavc62.28.102 libx265` |

Frame counts, not sizes: a decode that stops early still writes a smaller,
structurally valid file, so size cannot distinguish success from truncation.

The AV1 case is the one that discriminates. That agent reports av1 NVDEC as
`VerifiedFail` (exit 69) and carries no `av1`/`Main` entry at all — only
`av1`/*any profile*/8-bit/software. Matching is exact equality and fail-closed,
so the job could dispatch only because the software requirement was emitted
with an empty profile. With the profile populated it blocks, which is what the
same job did before the change:

    no enabled, commit-eligible agent satisfies AgentClass(Cpu)
      + Encoder(Libx265) + Muxer(Matroska)
      + Decoder(DecoderTriple { codec: "av1", profile: "Main", ... })
