<!-- file: TESTING.md -->
<!-- version: 1.1.0 -->
<!-- guid: 4d5e6f78-90ab-cdef-0123-456789abcdef -->
<!-- last-edited: 2026-07-31 -->

# Testing Guide for transcodarr

This document describes the comprehensive test and benchmark suite for transcodarr.

## Overview

The test suite consists of:

1. **Integration Tests** - Test the CLI binary end-to-end
2. **Benchmark Tests** - Measure performance characteristics
3. **Test Media Files** - Sample media for testing various scenarios

## Prerequisites

### Required Tools

- **Rust toolchain** (1.76+)
- **ffmpeg** - For actual transcoding operations
- **ffprobe** - For media information extraction
- **Git LFS** - For test media files

Install dependencies:

```bash
# macOS
brew install ffmpeg git-lfs

# Ubuntu/Debian
apt install ffmpeg git-lfs

# Fedora/RHEL
dnf install ffmpeg git-lfs
```

### Build the Binary

Before running tests, build the binary:

```bash
# Debug build (faster compilation)
cargo build

# Release build (required for benchmarks)
cargo build --release
```

## Test Media Files

Test files are located in `testdata/` and tracked via Git LFS:

- `test_color_720p_h264_aac.mp4` - 720p color bars with metadata
- `test_bars_480p_h265_opus.mkv` - 480p SMPTE bars, H265 encoded
- `test_audio_sine_opus.ogg` - Audio-only sine wave
- `test_with_subs_h264_aac.mp4` - Video with embedded subtitles

Generate additional test media:

```bash
python3 scripts/generate_test_media.py
```

## Running Tests

### Quick Test (Fast, No Actual Transcoding)

Run all fast tests (dry-run tests only):

```bash
cargo test
```

### Full Integration Tests

Run all tests including slow actual transcoding tests:

```bash
cargo test -- --ignored --test-threads=1
```

The `--test-threads=1` ensures tests run sequentially to avoid resource contention.

### Individual Test Suites

```bash
# Run only integration tests
cargo test --test integration_tests

# Run specific test
cargo test test_help_command

# Run with verbose output
cargo test -- --nocapture

# Run tests matching pattern
cargo test transcode
```

## Test Categories

### 1. CLI Interface Tests

Test the basic CLI commands work:

- `test_help_command` - Verify help output
- `test_version_command` - Verify version display

### 2. Info Command Tests

Test media information extraction:

- `test_info_command_requires_ffprobe` - Basic info command functionality

### 3. Transcode Dry-Run Tests (Fast)

Test command generation without actual transcoding:

- `test_transcode_dry_run` - Basic transcode dry-run
- `test_transcode_preset_original_h265_dry_run` - Original quality preset
- `test_transcode_preset_tv_h265_fast_dry_run` - TV fast preset
- `test_transcode_preset_movie_quality_dry_run` - Movie quality preset
- `test_invalid_preset_shows_error` - Error handling for invalid presets

### 4. Actual Transcoding Tests (Slow, Ignored by Default)

Test real transcoding operations:

- `test_transcode_actual_execution` - Single file transcode
- `test_batch_actual_execution` - Batch directory transcode

Run with: `cargo test -- --ignored`

### 5. Batch Operation Tests

Test directory processing:

- `test_batch_dry_run` - Batch dry-run without execution
- `test_batch_with_preset_dry_run` - Batch with preset application

## Benchmarks

Benchmarks measure performance characteristics using Criterion.

### Running Benchmarks

```bash
# Run all benchmarks (requires release build)
cargo bench

# Run specific benchmark
cargo bench info_command

# Save baseline for comparison
cargo bench -- --save-baseline initial

# Compare against baseline
cargo bench -- --baseline initial
```

### Benchmark Suites

1. **info_command** - Measures ffprobe execution time per test file
2. **transcode_dry_run** - Measures command generation overhead
3. **preset_parsing** - Measures preset application performance
4. **batch_dry_run** - Measures directory scanning and processing

### Benchmark Reports

After running benchmarks, view HTML reports:

```bash
open target/criterion/report/index.html
```

## Test Coverage

### What's Tested

✅ CLI argument parsing and validation
✅ Help and version output
✅ Info command with ffprobe integration
✅ Transcode command with all presets
✅ Batch directory processing
✅ Dry-run functionality (no actual execution)
✅ Preset application and merging
✅ Error handling for invalid inputs
✅ Output file creation and validation

### What's Not Tested (Yet)

⚠️ Metadata preservation verification
⚠️ Audio/video quality validation
⚠️ Resume capability (not implemented)
⚠️ Progress reporting (not implemented)
⚠️ Hardware acceleration (not implemented)
⚠️ Parallel batch processing (not implemented)

## Continuous Integration

Tests run automatically on GitHub Actions for:

- Every push to main branch
- Every pull request
- Multiple Rust versions (stable, beta)

CI runs:

1. `cargo fmt --check` - Code formatting
2. `cargo clippy` - Linting
3. `cargo build` - Compilation
4. `cargo test` - Fast tests only (no `--ignored`)

Slow tests and benchmarks are NOT run in CI to keep workflow times reasonable.

## Writing New Tests

### Integration Test Template

```rust
#[test]
fn test_my_new_feature() {
    // Arrange
    let test_file = &common::list_test_media()[0];
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // Act
    let output = common::run_transcodarr(&[
        "my-command",
        test_file.to_str().unwrap()
    ]).expect("Failed to run command");

    // Assert
    assert!(output.status.success(), "Command should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("expected-text"));
}
```

### Slow Test Template

Mark tests that perform actual transcoding as ignored:

```rust
#[test]
#[ignore] // Slow test - run with: cargo test -- --ignored
fn test_slow_operation() {
    if !common::ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not available");
        return;
    }

    // ... test implementation
}
```

### Benchmark Template

```rust
fn bench_my_operation(c: &mut Criterion) {
    c.bench_function("operation_name", |b| {
        b.iter(|| {
            // Code to benchmark
        });
    });
}
```

## Troubleshooting

### Tests Fail with "ffmpeg not found"

Install ffmpeg and ensure it's on PATH:

```bash
which ffmpeg
which ffprobe
```

### Tests Fail with "No test media files found"

Pull test media from Git LFS:

```bash
git lfs install
git lfs pull
```

### Benchmarks Show "Binary not found"

Build in release mode first:

```bash
cargo build --release
```

### Tests Timeout

Increase timeout for slow tests:

```bash
RUST_TEST_TIME_UNIT=60000 cargo test -- --ignored
```

## Performance Expectations

Typical benchmark results on modern hardware:

- **info_command**: 20-50ms per file (ffprobe overhead)
- **transcode_dry_run**: <1ms (pure CLI overhead)
- **preset_parsing**: <1ms (argument parsing)
- **batch_dry_run**: 5-20ms (directory scanning)

Actual transcoding performance depends on:

- Input file size and format
- Target codec and quality settings
- CPU performance and thread count
- Hardware acceleration availability

## Best Practices

1. **Run fast tests frequently** during development
2. **Run full tests** before committing
3. **Run benchmarks** when optimizing performance
4. **Add tests** for new features before implementation
5. **Mark slow tests** with `#[ignore]` attribute
6. **Use descriptive test names** that explain what's tested
7. **Test error cases** not just happy paths
8. **Clean up temp files** using TempDir for automatic cleanup

## Future Testing Improvements

- [ ] Add property-based testing with proptest
- [ ] Add mutation testing with cargo-mutants
- [ ] Add code coverage reporting with tarpaulin
- [ ] Add fuzz testing for CLI parsing
- [ ] Add performance regression detection
- [ ] Add visual regression tests for quality
- [ ] Add integration tests with real TV show/movie samples
- [ ] Add parallel test execution optimization
