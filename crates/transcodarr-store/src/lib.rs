// file: crates/transcodarr-store/src/lib.rs
// version: 1.0.0
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

pub use db::Db;

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
