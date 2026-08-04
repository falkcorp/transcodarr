// file: crates/transcodarr-agent/tests/truncated_output.rs
// version: 1.0.0
// guid: 9b2e7f14-0a63-45d8-b719-4c80e5a3d267
// last-edited: 2026-08-03
//! The rule that must never regress: a truncated output is rejected.
//!
//! From production measurement. The Turing NVDEC AV1 path exits 69 having
//! written roughly a kilobyte, and a truncated MKV frequently retains the
//! *source* duration in its container header. A validator that trusts the
//! header, or that consults size first, accepts exactly the outputs that
//! destroyed the media.
//!
//! This test uses real ffmpeg because the thing under test is precisely what
//! real ffmpeg writes into a truncated container. A hand-built fixture would be
//! asserting against my own idea of the failure rather than the failure.

use std::path::Path;
use std::process::Command;

use transcodarr_agent::{Executor, ExecutorConfig};
use transcodarr_core::validate::{SizePolicy, ValidationGate, ValidationSpec};

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn make_media(path: &Path, seconds: u32) {
    let ok = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={seconds}:size=320x240:rate=10"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={seconds}"),
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(ok.success(), "fixture generation failed");
}

/// The tolerance rule from production: asymmetric, and absolutely capped at
/// `min(0.5%, 5s)`. A percentage alone permits a 40-minute loss on a 3-hour
/// film, so the absolute cap is what actually protects the media.
fn spec(source_us: u64, audio: usize, subs: usize) -> ValidationSpec {
    let max_shorter = std::cmp::min(source_us / 200, 5_000_000);
    ValidationSpec {
        source_duration_us: source_us,
        max_shorter_us: max_shorter,
        max_longer_us: 2_000_000,
        expected_audio_streams: audio,
        expected_subtitle_streams: subs,
        source_bytes: 10_000_000,
        size_policy: SizePolicy::RequireSmaller { min_shrink: 0.0 },
    }
}

/// A complete output passes, so the rejection below is meaningful rather than
/// a validator that refuses everything.
#[test]
fn a_complete_output_is_accepted() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not present");
        return;
    }
    let d = tempfile::TempDir::new().unwrap();
    let good = d.path().join("good.mkv");
    make_media(&good, 4);

    let ex = Executor::new(ExecutorConfig::default());
    let measured = ex.last_packet_pts_us(&good).unwrap().expect("a PTS");
    let report = ex.validate(&spec(measured, 1, 0), &good, 0).unwrap();
    assert!(report.passed, "a complete output must pass: {report:?}");
}

/// The measured failure mode. A file truncated to a fraction of its length
/// keeps a plausible header; it must still be rejected, and rejected on
/// duration *before* size is ever consulted.
#[test]
fn a_truncated_output_fails_on_duration_before_size_is_consulted() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not present");
        return;
    }
    let d = tempfile::TempDir::new().unwrap();
    let full = d.path().join("full.mkv");
    make_media(&full, 8);

    let ex = Executor::new(ExecutorConfig::default());
    let source_us = ex.last_packet_pts_us(&full).unwrap().expect("a PTS");

    // Truncate to a fifth: smaller than the source, and short.
    let truncated = d.path().join("truncated.mkv");
    let bytes = std::fs::read(&full).unwrap();
    std::fs::write(&truncated, &bytes[..bytes.len() / 5]).unwrap();

    let report = ex.validate(&spec(source_us, 1, 0), &truncated, 0).unwrap();

    assert!(
        !report.passed,
        "a truncated output must be rejected: {report:?}"
    );
    assert!(
        !report.gates_run.contains(&ValidationGate::Size),
        "size must never be reached -- a truncated file is always smaller, so a \
         size-first gate accepts exactly the outputs that destroyed the media. \
         gates_run={:?}",
        report.gates_run
    );
    assert!(
        matches!(
            report.failed_gate,
            Some(ValidationGate::Duration) | Some(ValidationGate::Probe)
        ),
        "expected Duration or Probe, got {:?}",
        report.failed_gate
    );
}

/// The 1 KB artefact the AV1/NVDEC path actually produces. It is not media at
/// all, and must be rejected without the validator erroring.
#[test]
fn a_kilobyte_of_garbage_is_rejected_not_errored() {
    if !have("ffprobe") {
        eprintln!("skipping: ffprobe not present");
        return;
    }
    let d = tempfile::TempDir::new().unwrap();
    let junk = d.path().join("junk.mkv");
    std::fs::write(&junk, vec![0u8; 1024]).unwrap();

    let ex = Executor::new(ExecutorConfig::default());
    let report = ex
        .validate(&spec(8_000_000, 1, 0), &junk, 0)
        .expect("validation must report, not error");
    assert!(!report.passed);
    assert!(!report.gates_run.contains(&ValidationGate::Size));
}

/// A nonzero exit is rejected first of all, before anything is probed.
#[test]
fn a_failed_exit_code_is_rejected_first() {
    let d = tempfile::TempDir::new().unwrap();
    let out = d.path().join("out.mkv");
    std::fs::write(&out, vec![0u8; 1024]).unwrap();

    let ex = Executor::new(ExecutorConfig::default());
    let report = ex.validate(&spec(8_000_000, 1, 0), &out, 69).unwrap();
    assert!(!report.passed);
    assert_eq!(report.failed_gate, Some(ValidationGate::ExitCode));
    assert_eq!(
        report.gates_run,
        vec![ValidationGate::ExitCode],
        "nothing else should have run"
    );
}
