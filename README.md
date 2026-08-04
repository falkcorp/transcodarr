<!-- file: README.md -->
<!-- version: 0.9.1 -->
<!-- guid: 0a1b2c3d-4e5f-6789-abcd-ef0123456789 -->
<!-- last-edited: 2026-08-04 -->

# transcodarr

 A Rust CLI that wraps ffmpeg/ffprobe to transcode media while preserving metadata. Designed for batch conversion of TV shows and movies to modern codecs (h265/aac).

## Features

- `info`: show media info via ffprobe (optionally JSON)
- `transcode`: transcode while preserving metadata (map_metadata, movflags)
- `batch`: process entire directories recursively with h265 encoding
- Presets: `original-h265` (high-quality h265 + AAC 256k), `tv-h265-fast` (faster encode for TV), `movie-quality` (higher quality for films with AAC 320k)
- Sensible defaults with override flags for codecs and extra args

## Requirements

- Rust (1.76+ recommended)
- ffmpeg and ffprobe available on PATH
- Git LFS (for cloning test media): `brew install git-lfs` or `apt install git-lfs`

## Install / Build

```bash
cargo build
```

## Usage

```bash
# Show help
cargo run -- --help

# Show media info (JSON)
cargo run -- info testdata/test_color_720p_h264_aac.mp4 --json

# Transcode single file (h265+aac, preserve metadata)
cargo run -- transcode input.mp4 output.mkv --vcodec libx265 --acodec aac

# Transcode with implicit output (safe default)
# When output is omitted, transcodarr writes next to the input as `<name>_transcoded.mkv`
cargo run -- transcode input.mp4 --preset original-h265 --dry-run

# Use preset for original quality (h265+aac 256k, CRF 18, preset slow)
cargo run -- transcode input.mp4 output.mkv --preset original-h265

# Dry-run a single transcode with a preset (no execution)
cargo run -- transcode input.mp4 output.mkv --preset original-h265 --dry-run

# Batch convert TV show directory to h265+aac
cargo run -- batch /path/to/tv-shows /path/to/output --vcodec libx265 --acodec aac --ext mkv

# Batch in-place (same directory) - adds '_transcoded' suffix to prevent overwrite
cargo run -- batch /path/to/tv-shows /path/to/tv-shows --preset tv-h265-fast --dry-run

# Batch with preset (original quality -> h265+aac 256k)
cargo run -- batch /path/to/tv-shows /path/to/output --preset original-h265 --ext mkv

# Batch with TV fast preset (h265+aac 160k, CRF 22, preset medium)
cargo run -- batch /path/to/tv-shows /path/to/output --preset tv-h265-fast --ext mkv --dry-run

# Batch with movie-quality preset (h265+aac 320k, CRF 16, preset slow)
cargo run -- batch /path/to/movies /path/to/output --preset movie-quality --ext mkv

# Advanced: custom CRF and preset
cargo run -- transcode input.mp4 output.mp4 --vcodec libx265 --acodec aac --extra -crf 28 -preset medium
```

## Git LFS Setup

Test media files are tracked via Git LFS. After cloning:

```bash
git lfs install
git lfs pull
```

## Generate test media

Use the helper script (writes files to `testdata/`):

```bash
python3 scripts/generate_test_media.py
```

Generated examples:

- `test_color_720p_h264_aac.mp4` (3s, color pattern, metadata)
- `test_bars_480p_h265_aac.mkv` (3s, SMPTE bars)
- `test_audio_sine_aac.m4a` (3s, audio-only)
- `test_with_subs_h264_aac.mp4` (3s, embedded subtitles)

## Testing

Comprehensive test and benchmark suite available. See [TESTING.md](TESTING.md) for details.

Quick test:

```bash
# Run fast tests
cargo test

# Run all tests including slow transcoding tests
cargo test -- --ignored

# Run benchmarks
cargo bench
```

## Roadmap

- [x] Basic transcode with metadata preservation
- [x] Git LFS setup for test media
- [x] Batch processing for directories
- [x] Dry-run mode to preview ffmpeg commands
- [x] Preset: original-h265 (h265+aac 256k, CRF 18, slow)
- [x] Additional presets (tv-h265-fast, movie-quality)
- [x] Integration tests and benchmarks
- [ ] Progress reporting and ETA
- [ ] Resume capability for interrupted batches
- [ ] Extended metadata (cover art, chapters)
- [ ] Hardware acceleration support
