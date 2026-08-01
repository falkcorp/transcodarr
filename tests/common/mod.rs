// file: tests/common/mod.rs
// version: 1.2.0
// guid: 1a2b3c4d-5e6f-7890-abcd-ef1234567890
// last-edited: 2026-07-31

//! Common test utilities and helpers for integration tests

// This module is compiled into each test binary separately, so any helper not
// used by *that particular* binary reads as dead code. The helpers are part of
// the shared toolkit, not leftovers, so the lint is suppressed for the module
// rather than by deleting them.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Get the path to the project root directory
pub fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Get the path to the testdata directory
pub fn testdata_dir() -> PathBuf {
    project_root().join("testdata")
}

/// Get the path to the transcodarr binary
pub fn binary_path() -> PathBuf {
    let mut path = project_root();
    path.push("target");
    path.push(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    });
    path.push(if cfg!(windows) {
        "transcodarr.exe"
    } else {
        "transcodarr"
    });
    path
}

/// Run the transcodarr binary with given arguments
pub fn run_transcodarr(args: &[&str]) -> Result<std::process::Output, std::io::Error> {
    Command::new(binary_path()).args(args).output()
}

/// Check if ffmpeg is available on PATH
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check if ffprobe is available on PATH
pub fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get information about a media file using ffprobe
pub fn get_media_info(path: &Path) -> Result<String, std::io::Error> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            path.to_str().unwrap(),
        ])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Check if a file exists and has non-zero size
pub fn file_exists_and_valid(path: &Path) -> bool {
    path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false)
}

/// List all test media files in testdata directory
pub fn list_test_media() -> Vec<PathBuf> {
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

/// Cleanup temporary test output files
pub fn cleanup_temp_files(prefix: &str) -> std::io::Result<()> {
    let temp_dir = std::env::temp_dir();
    if let Ok(entries) = std::fs::read_dir(temp_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(prefix) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
    Ok(())
}
