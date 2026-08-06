<!-- file: changelog.d/20260806-muxers-cross-the-boundary.md -->
<!-- version: 1.0.0 -->
<!-- guid: 9b57c204-1e83-4d6a-95f0-3c72a8146db9 -->
<!-- last-edited: 2026-08-06 -->

### Fixed

#### Muxers were dropped at the conversion boundary, so nothing could dispatch

`TryFrom<pb::Capability> for Capability` set `muxers: Vec::new()`. Every agent
therefore registered advertising no muxers at all, and every job the evaluator
creates carries a `Muxer(Matroska)` requirement — so no agent satisfied any job.
A fleet would connect, report healthy, log a dispatch pass every tick, and place
nothing, forever.

The reverse direction had the same hole: `TryFrom<Capability> for pb::Capability`
never populated the field, so the round trip lost it twice.

**Found by running it, not by testing it.** A real library was scanned, a real
server and agent started, and every pass logged `blocked=1` with
`no enabled, commit-eligible agent satisfies AgentClass(Cpu) + Encoder(Eac3) +
Muxer(Matroska)`.

The end-to-end test could not have caught this: it created its job with
`requirements_json: "[]"`, so it never exercised capability matching at all. It
now carries the requirements a real evaluator attaches, and fails when the
muxers are dropped again.

Unknown muxers are skipped rather than refused, matching how encoders are
already handled — an ffmpeg build lists hundreds this scheduler has no opinion
about, and one unfamiliar name should not cost a working node its place in the
fleet.
