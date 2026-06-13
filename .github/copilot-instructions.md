<!-- file: .github/copilot-instructions.md -->
<!-- version: 2.4.0 -->
<!-- guid: 4d5e6f7a-8b9c-0d1e-2f3a-4b5c6d7e8f9a -->
<!-- last-edited: 2026-06-13 -->

# transcoderr — Additional Context

Org-wide coding standards (file headers, language rules, commit format) are at
**https://github.com/falkcorp/.github** and apply automatically to this repo.

For full project context: **CLAUDE.md** at the repo root.

## Project overview

Rust-based media transcoder CLI using ffmpeg/ffprobe. Wraps ffmpeg/ffprobe to
transcode media files while preserving metadata. Single binary (`transcoderr`),
entry point at `src/main.rs`.

## Key directories

| Path | Purpose |
|------|---------|
| `src/` | Rust source (single `main.rs` binary) |
| `tests/` | Integration tests |
| `benches/` | Criterion benchmarks |
| `testdata/` | Sample media files for tests |
| `scripts/` | Helper scripts |

## Critical constraints

- Requires `ffmpeg` and `ffprobe` on `PATH` at runtime — do not shell out to any other media tools.
- Optional `json` feature gate enables `serde`/`serde_json` for JSON output mode.
- Minimum Rust edition: 2021.
