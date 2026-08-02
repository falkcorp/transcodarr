// file: crates/transcodarr-core/src/lib.rs
// version: 1.0.0
// guid: 5c1d8b46-9e73-42a0-8f15-3b6a0c27d94e
// last-edited: 2026-08-01
#![deny(unsafe_code)]
#![warn(missing_docs)]
//! Pure domain model for transcodarr: probe facts, policy, capability
//! matching, encode plans, and output validation. No I/O, no async.
//!
//! Everything here is a function of its arguments. Nothing in this crate reads
//! the filesystem, opens a socket, or spawns a process — which is what lets the
//! server and the agent link the *same* matching and validation code rather
//! than maintaining two implementations that can drift apart.

pub mod paths;
pub mod plan;
pub mod preset;
pub mod probe;
pub mod validate;

use thiserror::Error;

/// Errors produced by the pure domain layer.
///
/// These describe *inputs that do not make sense*, never I/O failures — this
/// crate performs no I/O.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoreError {
    /// A preset name was requested that no registry entry matches.
    #[error("unknown preset '{name}'; valid presets are: {valid}")]
    UnknownPreset {
        /// The name that was requested.
        name: String,
        /// Comma-separated list of names and aliases that would have worked.
        valid: String,
    },

    /// A path could not be interpreted (no filename, non-UTF-8, and similar).
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// ffprobe output was not parseable JSON.
    ///
    /// Missing *fields* are not an error — ffprobe legitimately omits
    /// `duration` on some containers and `bits_per_raw_sample` on most.
    #[error("malformed ffprobe output: {0}")]
    MalformedProbe(String),
}
