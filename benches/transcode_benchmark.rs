// file: benches/transcode_benchmark.rs
// version: 1.1.0
// guid: 3c4d5e6f-7890-abcd-ef01-23456789abcd
// last-edited: 2026-07-31

//! Benchmarks for transcodarr operations
//! Run with: cargo bench

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::path::PathBuf;
use std::process::Command;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn testdata_dir() -> PathBuf {
    project_root().join("testdata")
}

fn binary_path() -> PathBuf {
    let mut path = project_root();
    path.push("target");
    path.push("release");
    path.push(if cfg!(windows) {
        "transcodarr.exe"
    } else {
        "transcodarr"
    });
    path
}

fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn list_test_media() -> Vec<PathBuf> {
    let testdata = testdata_dir();
    if !testdata.exists() {
        return vec![];
    }

    std::fs::read_dir(testdata)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|s| s.to_str())
                        .map(|ext| matches!(ext, "mp4" | "mkv" | "avi" | "mov" | "ogg" | "m4a"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn bench_info_command(c: &mut Criterion) {
    if !ffprobe_available() {
        eprintln!("SKIP benchmark: ffprobe not available");
        return;
    }

    let test_files = list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP benchmark: No test media files found");
        return;
    }

    let binary = binary_path();
    if !binary.exists() {
        eprintln!("SKIP benchmark: Binary not found. Run 'cargo build --release' first.");
        return;
    }

    let mut group = c.benchmark_group("info_command");

    for test_file in test_files.iter() {
        let file_name = test_file.file_name().unwrap().to_str().unwrap();

        group.bench_with_input(
            BenchmarkId::from_parameter(file_name),
            test_file,
            |b, path| {
                b.iter(|| {
                    Command::new(&binary)
                        .args(["info", path.to_str().unwrap()])
                        .output()
                        .expect("Failed to run info command")
                });
            },
        );
    }

    group.finish();
}

fn bench_transcode_dry_run(c: &mut Criterion) {
    let test_files = list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP benchmark: No test media files found");
        return;
    }

    let binary = binary_path();
    if !binary.exists() {
        eprintln!("SKIP benchmark: Binary not found. Run 'cargo build --release' first.");
        return;
    }

    let mut group = c.benchmark_group("transcode_dry_run");

    for test_file in test_files.iter() {
        let file_name = test_file.file_name().unwrap().to_str().unwrap();

        group.bench_with_input(
            BenchmarkId::from_parameter(file_name),
            test_file,
            |b, path| {
                b.iter(|| {
                    Command::new(&binary)
                        .args([
                            "transcode",
                            path.to_str().unwrap(),
                            "/tmp/output.mkv",
                            "--dry-run",
                        ])
                        .output()
                        .expect("Failed to run transcode dry-run")
                });
            },
        );
    }

    group.finish();
}

fn bench_preset_parsing(c: &mut Criterion) {
    let test_files = list_test_media();
    if test_files.is_empty() {
        eprintln!("SKIP benchmark: No test media files found");
        return;
    }

    let binary = binary_path();
    if !binary.exists() {
        eprintln!("SKIP benchmark: Binary not found. Run 'cargo build --release' first.");
        return;
    }

    let test_file = &test_files[0];
    let presets = vec!["original-h265", "tv-h265-fast", "movie-quality"];

    let mut group = c.benchmark_group("preset_parsing");

    for preset in presets {
        group.bench_with_input(
            BenchmarkId::from_parameter(preset),
            preset,
            |b, preset_name| {
                b.iter(|| {
                    Command::new(&binary)
                        .args([
                            "transcode",
                            test_file.to_str().unwrap(),
                            "/tmp/output.mkv",
                            "--preset",
                            preset_name,
                            "--dry-run",
                        ])
                        .output()
                        .expect("Failed to run transcode with preset")
                });
            },
        );
    }

    group.finish();
}

fn bench_batch_dry_run(c: &mut Criterion) {
    let testdata = testdata_dir();
    if !testdata.exists() {
        eprintln!("SKIP benchmark: testdata directory not found");
        return;
    }

    let binary = binary_path();
    if !binary.exists() {
        eprintln!("SKIP benchmark: Binary not found. Run 'cargo build --release' first.");
        return;
    }

    c.bench_function("batch_dry_run", |b| {
        b.iter(|| {
            Command::new(&binary)
                .args([
                    "batch",
                    testdata.to_str().unwrap(),
                    "/tmp/output",
                    "--dry-run",
                ])
                .output()
                .expect("Failed to run batch dry-run")
        });
    });
}

criterion_group!(
    benches,
    bench_info_command,
    bench_transcode_dry_run,
    bench_preset_parsing,
    bench_batch_dry_run
);
criterion_main!(benches);
