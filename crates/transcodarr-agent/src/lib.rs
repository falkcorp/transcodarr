// file: crates/transcodarr-agent/src/lib.rs
// version: 1.6.0
// guid: b2947c0e-5d81-4f36-a7b0-6e13df852a94
// last-edited: 2026-08-06
#![deny(unsafe_code)]
#![warn(missing_docs)]
//! transcodarr worker agent.
//!
//! Depends on `transcodarr-core` and nothing else internal — never on
//! `transcodarr-store`. An agent must be copyable to the Windows node without
//! dragging SQLite along with it.

pub mod client;
pub mod commit;
pub mod executor;
pub mod identity;
pub mod journal;
pub mod preflight;
pub mod probe_caps;
pub mod probe_samples;
pub mod run;
pub mod survey;
pub mod workarea;
pub mod worker;

/// The wire types, re-exported.
///
/// The public [`client::Worker`] trait already speaks in these, so a caller
/// implementing it would otherwise have to depend on `transcodarr-proto`
/// directly and keep its version in step by hand.
pub use transcodarr_proto::pb;

pub use client::{
    ClientConfig, ClientError, ConnectClient, Link, ReconnectPolicy, Shutdown, Worker,
};
pub use commit::{CommitRequest, CommitRitual, Resolution, SourceGuard};
pub use executor::{Execution, Executor, ExecutorConfig, Progress, ProgressTailer};
pub use identity::{agent_uid, boot_id};
pub use journal::{IntentJournal, IntentPhase, IntentRecord};
pub use probe_caps::{TrialOutcome, capability_for, classify};
pub use run::{RunConfig, prepare, run};
pub use survey::{MountSpec, SurveyConfig, survey};
pub use workarea::WorkArea;
pub use worker::LocalWorker;

use thiserror::Error;

/// Anything that can go wrong in an agent.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AgentError {
    /// The work area could not be prepared or cleaned.
    #[error("work area {path}: {source}")]
    WorkArea {
        /// Which path.
        path: String,
        /// Underlying error.
        source: std::io::Error,
    },

    /// The intent journal could not be read or written.
    ///
    /// Fatal to an install by design: the journal is what makes a crash
    /// survivable, so proceeding without one would mean performing the
    /// irreversible steps with no way to recover from them.
    #[error("intent journal {path}: {source}")]
    Journal {
        /// Which path.
        path: String,
        /// Underlying error.
        source: std::io::Error,
    },

    /// A step of the commit ritual failed against the filesystem.
    #[error("commit ritual, {step} on {path}: {source}")]
    Commit {
        /// Which step.
        step: &'static str,
        /// Which path.
        path: String,
        /// Underlying error.
        source: std::io::Error,
    },

    /// The work area is not on the destination's filesystem.
    ///
    /// A hard refusal rather than a fallback. `rename(2)` is atomic only within
    /// one filesystem; the copy-then-delete alternative has a window in which
    /// neither the original nor a complete replacement exists, and a crash
    /// there loses the file outright.
    #[error(
        "work area {work_area} is not on the same filesystem as {destination}; \
             an atomic install is impossible -- colocate the work area"
    )]
    CrossDeviceWorkArea {
        /// The staging directory.
        work_area: String,
        /// The destination directory.
        destination: String,
    },

    /// A child process could not be started or waited on.
    #[error("running {program}: {source}")]
    Execute {
        /// Which binary.
        program: String,
        /// Underlying error.
        source: std::io::Error,
    },

    /// An output could not be probed.
    #[error("probing {path}: {reason}")]
    Probe {
        /// Which file.
        path: String,
        /// What went wrong.
        reason: String,
    },

    /// The platform cannot report which device a path is on.
    #[error("cannot determine the filesystem of {path}; this agent must be produce-only")]
    DeviceUnknowable {
        /// Which path.
        path: String,
    },
}
