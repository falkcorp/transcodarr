// file: crates/transcodarr-store/src/lib.rs
// version: 1.1.0
// guid: 6d2a90fb-1c47-4358-b0e9-a7f43c8152d6
// last-edited: 2026-08-03
#![deny(unsafe_code)]
#![warn(missing_docs)]
//! transcodarr persistence.
//!
//! Owns SQLite and nothing else. Only `transcodarr-server` links this crate —
//! an agent must stay copyable to the Windows node without dragging a database
//! engine along with it.

pub mod db;
pub mod pool;
pub mod repo;
pub mod writer;

pub use db::Db;
pub use pool::ReadPool;
pub use repo::{DispatchBlockRepo, FileRepo, JobRepo, LibraryRepo};
pub use writer::{WriteAck, WriteLane, WriteOp, Writer};

use thiserror::Error;

/// Anything that can go wrong talking to the store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// SQLite said no.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Filesystem error while opening or probing.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A pragma was set but did not take effect.
    ///
    /// Setting a pragma is a request, not a guarantee: `journal_mode = WAL`
    /// fails silently on some filesystems and leaves the connection in `delete`
    /// mode, where the concurrency assumptions behind a single writer plus a
    /// read pool do not hold.
    #[error("pragma {pragma} wanted {wanted} but got {got}")]
    PragmaRejected {
        /// Which pragma.
        pragma: String,
        /// What was asked for.
        wanted: String,
        /// What the database reported afterwards.
        got: String,
    },

    /// An already-applied migration's text has changed since it ran.
    ///
    /// Refusing is the only safe response: re-running it would corrupt, and
    /// ignoring it would let the binary and the database disagree about what
    /// the schema is.
    #[error(
        "migration {version} ({name}) has changed since it was applied; \
             the database and this binary disagree about the schema"
    )]
    MigrationChanged {
        /// Migration version.
        version: i64,
        /// Migration name.
        name: String,
    },

    /// The writer thread is no longer running.
    #[error("writer stopped")]
    WriterStopped,

    /// A row that was required was not there.
    #[error("no {kind} with id {id}")]
    NotFound {
        /// What was being looked for.
        kind: &'static str,
        /// The identifier that found nothing.
        id: String,
    },

    /// A stored value is not one this binary understands.
    ///
    /// The `CHECK` constraints make this unreachable for rows written by a
    /// matching binary, so it means a newer binary wrote the row — and guessing
    /// is worse than refusing.
    #[error("column {column} holds {value}, which this binary does not recognise")]
    UnknownEnum {
        /// Which column.
        column: &'static str,
        /// What it held.
        value: String,
    },

    /// A job transition the state machine forbids.
    #[error("job {job_id}: {from} -> {to} is not a legal transition")]
    IllegalTransition {
        /// Which job.
        job_id: String,
        /// State it was expected to be in.
        from: String,
        /// State that was requested.
        to: String,
    },

    /// A compare-and-swap transition lost its race.
    ///
    /// The job was not in the expected state when the `UPDATE` ran, so somebody
    /// else moved it first. Reporting this rather than retrying is deliberate:
    /// the caller's decision was made against state that no longer holds, and
    /// re-deciding is its job, not the store's.
    #[error("job {job_id} was no longer in state {expected}")]
    TransitionRaceLost {
        /// Which job.
        job_id: String,
        /// The state the caller believed it was in.
        expected: String,
    },

    /// Measured fsync latency is too high for a single-writer design.
    #[error(
        "fsync p99 {p99_us}us on {path} exceeds the {limit_us}us limit; \
             the single writer would be the bottleneck -- move the database"
    )]
    DurabilityTooSlow {
        /// Measured p99, microseconds.
        p99_us: u128,
        /// Configured limit, microseconds.
        limit_us: u128,
        /// Directory probed.
        path: String,
    },
}
