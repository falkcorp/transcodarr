// file: crates/transcodarr-server/src/lib.rs
// version: 1.5.0
// guid: 8b40e7c2-19d5-46fa-b03e-7c2a815d94f6
// last-edited: 2026-08-03
#![deny(unsafe_code)]
#![warn(missing_docs)]
//! transcodarr orchestration.
//!
//! Owns discovery, evaluation and — from Phase 4 — dispatch. It is the only
//! crate that links `transcodarr-store`: an agent must stay copyable to the
//! Windows node without dragging SQLite along with it.
//!
//! Nothing here runs ffmpeg. Deciding what should happen and making it happen
//! are separate jobs held by separate processes, which is what lets a decision
//! be re-derived from stored facts without touching a byte of media.

pub mod capacity;
pub mod dispatch;
pub mod evaluator;
pub mod explain;
pub mod prober;
pub mod runner;
pub mod runtime;
pub mod scanner;
pub mod summary;

pub use capacity::{AgentLimits, CapacityLedger, Grant, Refusal};
pub use dispatch::{AgentEntry, Assignment, Blocked, DispatchRound, Dispatcher, QueuedJob};
pub use evaluator::{EvalOutcome, Evaluator};
pub use explain::{Explainer, Explanation};
pub use prober::{ProbeOptions, ProbeOutcome, Prober};
pub use runner::{JobOutcome, LocalRunner, RunOutcome};
pub use runtime::Runtime;
pub use scanner::{ScanOptions, ScanOutcome, Scanner};
pub use summary::{LibrarySummary, summarize};
pub use transcodarr_agent::ExecutorConfig;
pub use transcodarr_store::repo::LibraryRecord;

use thiserror::Error;

/// Anything that can go wrong orchestrating.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    /// The store said no.
    #[error("store: {0}")]
    Store(#[from] transcodarr_store::StoreError),

    /// Filesystem error during a walk.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A library root is not a directory that can be walked.
    ///
    /// Separated from a plain I/O error because the operator action differs: a
    /// missing root is a mount problem, not a permissions problem, and saying
    /// so is the difference between a five-minute fix and an hour of guessing.
    #[error("library {library_id}: root {root} is not a readable directory")]
    LibraryRootUnreadable {
        /// Which library.
        library_id: String,
        /// The path that could not be walked.
        root: String,
    },

    /// The agent reported a failure.
    #[error("agent: {0}")]
    Agent(#[from] transcodarr_agent::AgentError),

    /// No stored file matches a path.
    ///
    /// Distinct from "no work owed": a path nobody has scanned is a different
    /// question, and answering it as though the file were settled would send an
    /// operator looking at the policy instead of at the library configuration.
    #[error("no scanned file at {path}; is it inside an enabled library, and has a scan run?")]
    UnknownPath {
        /// The path that matched nothing.
        path: String,
    },

    /// A single file could not be probed.
    ///
    /// Carried as an error so one bad file is visible, but the file is marked
    /// `ProbeFailed` before it is returned: ingestion counts it as progress and
    /// moves on rather than abandoning the rest of the batch.
    #[error("probe of {path} failed: {reason}")]
    ProbeFailed {
        /// Which file.
        path: String,
        /// What went wrong.
        reason: String,
    },

    /// A scan stopped rather than record an implausible number of deletions.
    ///
    /// An unmounted library is indistinguishable from every file having been
    /// deleted, except by proportion. Marking 49,600 files missing because a
    /// mount was not ready is recoverable, but it destroys every open job and
    /// takes a full re-probe to undo.
    #[error(
        "scan of {library_id} aborted: {would_be_missing} of {live_before} files \
             would be marked missing ({percent:.1}%), above the {limit_percent:.1}% \
             limit -- check the mount before rescanning"
    )]
    MassMissing {
        /// Which library.
        library_id: String,
        /// How many rows the scan did not see.
        would_be_missing: i64,
        /// How many live rows existed before it ran.
        live_before: i64,
        /// The proportion that represents.
        percent: f64,
        /// The configured ceiling.
        limit_percent: f64,
    },
}
