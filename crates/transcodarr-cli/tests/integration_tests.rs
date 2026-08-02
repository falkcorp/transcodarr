// file: tests/integration_tests.rs
// version: 1.5.0
// guid: 2b3c4d5e-6f78-90ab-cdef-0123456789ab
// last-edited: 2026-07-31

//! Integration tests for transcodarr CLI
//! These tests execute the actual binary and validate behavior

mod common;

use std::fs;
use tempfile::TempDir;

#[test]
fn test_help_command() {
    let output = common::run_transcodarr(&["--help"]).expect("Failed to run transcodarr --help");

    assert!(output.status.success(), "Help command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("transcode"),
        "Help should mention 'transcode' command"
    );
    assert!(
        stdout.contains("batch"),
        "Help should mention 'batch' command"
    );
    assert!(
        stdout.contains("info"),
        "Help should mention 'info' command"
    );
}

#[test]
fn test_batch_same_directory_dry_run() {
    let testdata = common::testdata_dir();

    // Batch with same input and output directory
    let output = common::run_transcodarr(&[
        "batch",
        testdata.to_str().unwrap(),
        testdata.to_str().unwrap(),
        "--preset",
        "tv-h265-fast",
        "--dry-run",
    ])
    .expect("Failed to run batch same-directory dry-run");

    assert!(
        output.status.success(),
        "Batch same-dir dry-run should succeed"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("IN-PLACE"),
        "Should indicate in-place processing"
    );
    assert!(
        stdout.contains("_transcoded"),
        "Should show _transcoded suffix"
    );
    assert!(
        stdout.contains("_transcoded.mkv"),
        "Should create .mkv files with suffix"
    );
}

#[test]
fn test_filename_with_multiple_dots_preserves_stem() {
    use std::io::Write;
    let temp = TempDir::new().expect("temp dir");
    let file_name = "Neon Genesis Evangelion (1995) - s00e003 - Evangelion - 1.11 - You Are (Not) Alone - Bluray-1080p - AV1 10Bit Opus[JA+EN] [EN] - tvdb-70350.mkv";
    let file_path = temp.path().join(file_name);
    // Create an empty file so it is discovered
    let mut f = std::fs::File::create(&file_path).expect("create file");
    f.write_all(b"\n").ok();

    let output = common::run_transcodarr(&[
        "batch",
        temp.path().to_str().unwrap(),
        temp.path().to_str().unwrap(),
        "--dry-run",
    ])
    .expect("run batch");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Ensure the "1.11" part of the stem is preserved, and suffix is appended
    assert!(stdout.contains("1.11"), "stdout: {}", stdout);
    assert!(stdout.contains("_transcoded.mkv"), "stdout: {}", stdout);
}

#[test]
fn test_version_command() {
    let output =
        common::run_transcodarr(&["--version"]).expect("Failed to run transcodarr --version");

    assert!(output.status.success(), "Version command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("transcodarr"),
        "Version should contain program name"
    );
}

#[test]
fn test_info_command_requires_ffprobe() {
    if !common::ffprobe_available() {
        eprintln!("SKIP: ffprobe not available");
        return;
    }

    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    let test_file = &test_files[0];
    let output = common::run_transcodarr(&["info", test_file.to_str().unwrap()])
        .expect("Failed to run info command");

    assert!(output.status.success(), "Info command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "Info should produce output");
}

#[test]
fn test_info_all_media_files_dry() {
    if !common::ffprobe_available() {
        eprintln!("SKIP: ffprobe not available");
        return;
    }

    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    for test_file in test_files.iter() {
        let output = common::run_transcodarr(&["info", test_file.to_str().unwrap()])
            .expect("Failed to run info command");
        assert!(
            output.status.success(),
            "Info command should succeed for {:?}",
            test_file
        );
    }
}

#[test]
fn test_transcode_dry_run() {
    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    let test_file = &test_files[0];
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_path = temp_dir.path().join("output.mkv");

    let output = common::run_transcodarr(&[
        "transcode",
        test_file.to_str().unwrap(),
        output_path.to_str().unwrap(),
        "--dry-run",
    ])
    .expect("Failed to run transcode dry-run");

    assert!(output.status.success(), "Dry-run should succeed");
    assert!(
        !output_path.exists(),
        "Dry-run should not create output file"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[DRY RUN]"),
        "Dry-run should indicate it's a dry run"
    );
    assert!(
        stdout.contains("ffmpeg"),
        "Dry-run should show ffmpeg command"
    );
}

#[test]
fn transcode_dry_run_default_output_suffix_and_mkv() {
    let test_file = common::testdata_dir().join("test_bars_480p_h265_aac.mkv");
    // Omit output argument; expect default `<stem>_transcoded.mkv` next to input
    let output = common::run_transcodarr(&["transcode", test_file.to_str().unwrap(), "--dry-run"])
        .expect("Failed to run transcode dry-run with default output");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Default output should be in same dir and use _transcoded.mkv suffix
    let stem = test_file.file_stem().unwrap().to_string_lossy();
    let parent = test_file.parent().unwrap();
    let expected_name = format!("{}_transcoded.mkv", stem);
    let expected_path = parent.join(expected_name);
    assert!(
        stdout.contains(expected_path.to_str().unwrap()),
        "stdout was: {}",
        stdout
    );
}

#[test]
fn test_transcode_dry_run_all_formats_to_multiple_exts() {
    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let exts = ["mkv", "mp4"];

    for test_file in test_files.iter() {
        for ext in exts.iter() {
            let out = temp_dir.path().join(format!("output.{}", ext));
            let output = common::run_transcodarr(&[
                "transcode",
                test_file.to_str().unwrap(),
                out.to_str().unwrap(),
                "--dry-run",
            ])
            .expect("Failed to run transcode dry-run");
            assert!(
                output.status.success(),
                "Dry-run should succeed for {:?} -> .{}",
                test_file,
                ext
            );
        }
    }
}

#[test]
fn test_transcode_preset_original_h265_dry_run() {
    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    let test_file = &test_files[0];
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_path = temp_dir.path().join("output.mkv");

    let output = common::run_transcodarr(&[
        "transcode",
        test_file.to_str().unwrap(),
        output_path.to_str().unwrap(),
        "--preset",
        "original-h265",
        "--dry-run",
    ])
    .expect("Failed to run transcode with preset");

    assert!(output.status.success(), "Preset dry-run should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("libx265"), "Preset should use libx265");
    assert!(stdout.contains("aac"), "Preset should use AAC");
    assert!(stdout.contains("-crf"), "Preset should include CRF");
    assert!(
        stdout.contains("-b:a"),
        "Preset should include audio bitrate"
    );
}

#[test]
fn test_transcode_preset_tv_h265_fast_dry_run() {
    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    let test_file = &test_files[0];
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_path = temp_dir.path().join("output.mkv");

    let output = common::run_transcodarr(&[
        "transcode",
        test_file.to_str().unwrap(),
        output_path.to_str().unwrap(),
        "--preset",
        "tv-h265-fast",
        "--dry-run",
    ])
    .expect("Failed to run transcode with preset");

    assert!(output.status.success(), "Preset dry-run should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("libx265"), "TV preset should use libx265");
    assert!(
        stdout.contains("160k"),
        "TV preset should use 160k audio bitrate"
    );
}

#[test]
fn test_transcode_preset_movie_quality_dry_run() {
    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    let test_file = &test_files[0];
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_path = temp_dir.path().join("output.mkv");

    let output = common::run_transcodarr(&[
        "transcode",
        test_file.to_str().unwrap(),
        output_path.to_str().unwrap(),
        "--preset",
        "movie-quality",
        "--dry-run",
    ])
    .expect("Failed to run transcode with preset");

    assert!(output.status.success(), "Preset dry-run should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("libx265"),
        "Movie preset should use libx265"
    );
    assert!(
        stdout.contains("320k"),
        "Movie preset should use 320k audio bitrate"
    );
}

#[test]
#[ignore] // Slow test - run with: cargo test -- --ignored
fn test_transcode_actual_execution() {
    if !common::ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not available");
        return;
    }

    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    // Use the smallest test file
    let test_file = &test_files[0];
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_path = temp_dir.path().join("output.mkv");

    let output = common::run_transcodarr(&[
        "transcode",
        test_file.to_str().unwrap(),
        output_path.to_str().unwrap(),
        "--vcodec",
        "libx264",
        "--acodec",
        "aac",
    ])
    .expect("Failed to run transcode");

    if !output.status.success() {
        eprintln!("STDERR: {}", String::from_utf8_lossy(&output.stderr));
    }

    assert!(output.status.success(), "Transcode should succeed");
    assert!(
        common::file_exists_and_valid(&output_path),
        "Output file should exist and be valid"
    );
}

#[test]
fn test_batch_dry_run() {
    let testdata_dir = common::testdata_dir();
    if !testdata_dir.exists() {
        eprintln!("SKIP: testdata directory not found");
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_dir = temp_dir.path().join("output");

    let output = common::run_transcodarr(&[
        "batch",
        testdata_dir.to_str().unwrap(),
        output_dir.to_str().unwrap(),
        "--dry-run",
    ])
    .expect("Failed to run batch dry-run");

    assert!(output.status.success(), "Batch dry-run should succeed");
    assert!(
        !output_dir.exists(),
        "Batch dry-run should not create output directory"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[DRY RUN]") || stdout.contains("Would process"),
        "Batch dry-run should indicate dry-run mode"
    );
}

#[test]
fn test_batch_with_preset_dry_run() {
    let testdata_dir = common::testdata_dir();
    if !testdata_dir.exists() {
        eprintln!("SKIP: testdata directory not found");
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_dir = temp_dir.path().join("output");

    let output = common::run_transcodarr(&[
        "batch",
        testdata_dir.to_str().unwrap(),
        output_dir.to_str().unwrap(),
        "--preset",
        "tv-h265-fast",
        "--dry-run",
    ])
    .expect("Failed to run batch with preset");

    assert!(
        output.status.success(),
        "Batch preset dry-run should succeed"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("libx265") || stdout.contains("[DRY RUN]"),
        "Batch should use preset or show dry-run"
    );
}

#[test]
fn test_batch_dry_run_with_input_exts() {
    let testdata_dir = common::testdata_dir();
    if !testdata_dir.exists() {
        eprintln!("SKIP: testdata directory not found");
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_dir = temp_dir.path().join("output");

    let output = common::run_transcodarr(&[
        "batch",
        testdata_dir.to_str().unwrap(),
        output_dir.to_str().unwrap(),
        "--input-exts",
        "mp4,mkv,ogg,m4a",
        "--dry-run",
    ])
    .expect("Failed to run batch dry-run with input-exts");

    assert!(output.status.success(), "Batch dry-run should succeed");
}

#[test]
#[ignore] // Slow test - run with: cargo test -- --ignored
fn test_batch_actual_execution() {
    if !common::ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not available");
        return;
    }

    let testdata_dir = common::testdata_dir();
    if !testdata_dir.exists() || common::list_test_media().is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_dir = temp_dir.path().join("output");

    let output = common::run_transcodarr(&[
        "batch",
        testdata_dir.to_str().unwrap(),
        output_dir.to_str().unwrap(),
        "--vcodec",
        "libx264",
        "--acodec",
        "aac",
        "--ext",
        "mp4",
    ])
    .expect("Failed to run batch");

    if !output.status.success() {
        eprintln!("STDERR: {}", String::from_utf8_lossy(&output.stderr));
    }

    assert!(output.status.success(), "Batch should succeed");
    assert!(output_dir.exists(), "Batch should create output directory");

    // Check that at least one file was created
    let output_files: Vec<_> = fs::read_dir(&output_dir)
        .expect("Failed to read output dir")
        .filter_map(|e| e.ok())
        .collect();

    assert!(!output_files.is_empty(), "Batch should create output files");
}

#[test]
fn test_invalid_preset_shows_error() {
    let test_files = common::list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP: No test media files found");
        return;
    }

    let test_file = &test_files[0];
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_path = temp_dir.path().join("output.mkv");

    let output = common::run_transcodarr(&[
        "transcode",
        test_file.to_str().unwrap(),
        output_path.to_str().unwrap(),
        "--preset",
        "invalid-preset-name",
        "--dry-run",
    ])
    .expect("Failed to run transcode with invalid preset");

    // Should either fail or show warning
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Either error or warning about unknown preset
    assert!(
        !output.status.success()
            || stderr.contains("unknown")
            || stderr.contains("invalid")
            || stdout.contains("unknown")
            || stdout.contains("invalid"),
        "Invalid preset should produce error or warning"
    );
}
