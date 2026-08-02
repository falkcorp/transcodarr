### Changed

#### Phase 1: the repository is now a Cargo workspace

`src/main.rs` is dissolved. Nothing was deleted; everything was relocated:

- `crates/transcodarr-cli/` — the binary, still named `transcodarr`, plus the
  integration tests and benchmarks.
- `crates/transcodarr-core/` — pure domain logic. No tokio, no rusqlite, no
  tonic, no I/O of any kind. `#![deny(unsafe_code)]`, `#![warn(missing_docs)]`.

Three modules landed in core so far: `paths` (output-path derivation), `plan`
(encoder identities, pixel formats, ffmpeg argv construction) and `preset`
(the named quality presets).

#### The `local` subcommand, with the old verbs still working

Commands are now grouped under `transcodarr local <verb>`, ahead of the
`server`, `agent` and `admin` faces that follow. **`transcodarr transcode`,
`batch` and `info` continue to work unchanged** — they are rewritten to
`local <verb>` before argument parsing, so there is one definition of each verb
rather than two that could drift. Top-level `--help` documents the equivalence.

Every flag keeps its name, default and semantics, including `--input-exts` and
the `_transcoded.<ext>` default output.

### Fixed

#### Preset selection no longer relies on a string sentinel

`apply_preset` inferred "the user did not pass `--vcodec`" by testing
`vcodec == "libx264"`. Anyone who explicitly asked for libx264 was
indistinguishable from someone who asked for nothing, so a preset silently
replaced their choice with libx265. Intent is now carried by
`Option<EncoderId>`, and there is a regression test for the case that used to
break.

#### Pixel-format selection is an exhaustive match

`pix_fmt_for(encoder, depth)` cannot fall through a wildcard: libx265 wants
`yuv420p10le` where NVENC wants `p010le`, and the wrong one errors the job. A
new encoder or bit depth now forces a decision at compile time. Bit depth is
never raised — 8-bit stays 8-bit.
