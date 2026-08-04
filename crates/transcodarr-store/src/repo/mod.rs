// file: crates/transcodarr-store/src/repo/mod.rs
// version: 1.4.0
// guid: 7a10c5e4-2b98-4d31-95f7-6e0a48b3d271
// last-edited: 2026-08-04
//! Repositories.
//!
//! Every repository returns domain types — `transcodarr-core` enums and plain
//! records — and never a `rusqlite::Row`. No SQL text escapes this crate. Both
//! rules exist for the same reason: the moment a caller can hand in a fragment
//! of SQL or inspect a raw row, the schema stops being something this crate can
//! change on its own, and "fetch everything and filter in the caller" becomes
//! expressible again.
//!
//! Reads go through [`crate::ReadPool`]; writes are built as [`crate::WriteOp`]
//! values for [`crate::Writer`]. A repository never holds a write connection of
//! its own, so there is exactly one writer no matter how many exist.
//!
//! **Seven of the eleven contracted repositories are implemented here** —
//! [`FileRepo`], [`LibraryRepo`], [`JobRepo`], [`DispatchBlockRepo`],
//! [`CommitIntentRepo`], [`TrashRepo`] and [`AgentRepo`]. `ScheduleRepo`,
//! `ConfigRepo` and `PoolRepo` arrive with the phases that call them. Writing
//! them ahead of a caller would ship untested APIs whose first real user is
//! free to discover they are the wrong shape.

mod agent;
mod commit_intent;
mod dispatch_block;
mod file;
mod job;
mod library;
mod trash;

pub use agent::{AgentRecord, AgentRegistration, AgentRepo, KnownInstance};
pub use commit_intent::{CommitIntent, CommitIntentRepo, NewIntent};
pub use dispatch_block::{DispatchBlock, DispatchBlockRepo};
pub use file::{FileIdentity, FileRecord, FileRepo, FileUpsert};
pub use job::{JobEvent, JobRecord, JobRepo, NewJob};
pub use library::{LibraryRecord, LibraryRepo, ScanRun};
pub use trash::{MIN_GRACE_SECONDS, TrashEntry, TrashRepo};

use crate::StoreError;

/// Turn a stored `TEXT` value into a domain enum, or say which column lied.
///
/// The parsers themselves live in `transcodarr-core` beside their enums, which
/// are `#[non_exhaustive]`: mapping them here would force a wildcard arm, and a
/// wildcard in a state-to-text mapping silently persists a new variant under
/// some other variant's name. This wrapper only supplies the column name that a
/// bare `Option` would have thrown away.
pub(crate) fn parse_enum<T>(
    column: &'static str,
    raw: &str,
    parse: fn(&str) -> Option<T>,
) -> Result<T, StoreError> {
    parse(raw).ok_or_else(|| StoreError::UnknownEnum {
        column,
        value: raw.to_string(),
    })
}

#[cfg(test)]
pub(crate) mod tests_support {
    use crate::db::Db;
    use crate::pool::ReadPool;
    use crate::writer::Writer;
    use tempfile::TempDir;

    /// A migrated database with both a writer and a read pool over it.
    ///
    /// The `TempDir` is held so the database outlives the test; the writer owns
    /// the only write connection, which is also what keeps the WAL `-shm` file
    /// alive for the read-only pool.
    pub(crate) struct Fixture {
        _dir: TempDir,
        pub(crate) writer: Writer,
        pub(crate) pool: ReadPool,
    }

    pub(crate) fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open_unchecked(&path).unwrap();
        let pool = ReadPool::open(&path, 4).unwrap();
        Fixture {
            _dir: dir,
            writer: Writer::start(db),
            pool,
        }
    }

    impl Fixture {
        /// Insert a minimal library so foreign keys on `file` and `job` resolve.
        pub(crate) fn seed_library(&self, id: &str) {
            let rec = super::LibraryRecord {
                id: id.into(),
                name: id.into(),
                root_path: format!("/mnt/{id}"),
                work_dir: format!("/mnt/{id}/work"),
                trash_dir: format!("/mnt/{id}/trash"),
                exclude_globs_json: "[]".into(),
                enabled: true,
                scan_parallelism: 4,
                priority: 0,
                min_mtime_age_s: 300,
            };
            self.writer
                .submit_blocking(
                    crate::writer::WriteLane::Normal,
                    super::LibraryRepo::upsert_op(rec),
                )
                .unwrap();
        }

        /// Apply a write op on the normal lane and unwrap the acknowledgement.
        pub(crate) fn write(&self, op: crate::writer::WriteOp) -> crate::writer::WriteAck {
            self.writer
                .submit_blocking(crate::writer::WriteLane::Normal, op)
                .unwrap()
        }

        /// Apply a write op, returning the error instead of panicking.
        pub(crate) fn try_write(
            &self,
            op: crate::writer::WriteOp,
        ) -> Result<crate::writer::WriteAck, crate::StoreError> {
            self.writer
                .submit_blocking(crate::writer::WriteLane::Normal, op)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use transcodarr_core::facts::SizeBucket;
    use transcodarr_core::file::FileState;
    use transcodarr_core::job::{JobClass, JobState};

    /// Every spelling stored must be readable again. Drift here would make rows
    /// written by one release unreadable by the next, and the `CHECK`
    /// constraints would not catch it — they agree with whichever spelling the
    /// writer used.
    #[test]
    fn every_persisted_enum_round_trips() {
        for s in [
            JobState::Pending,
            JobState::Blocked,
            JobState::Eligible,
            JobState::Assigned,
            JobState::Running,
            JobState::Verifying,
            JobState::Committing,
            JobState::Retrying,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Cancelled,
            JobState::DeadLettered,
            JobState::NeedsOperator,
        ] {
            assert_eq!(
                parse_enum("job.state", s.as_str(), JobState::parse).unwrap(),
                s
            );
        }
        for c in [
            JobClass::Audio,
            JobClass::VideoGpu,
            JobClass::VideoCpu,
            JobClass::Probe,
            JobClass::Verify,
        ] {
            assert_eq!(
                parse_enum("job.class", c.as_str(), JobClass::parse).unwrap(),
                c
            );
        }
        for b in [SizeBucket::Small, SizeBucket::Medium, SizeBucket::Large] {
            assert_eq!(
                parse_enum("size_bucket", b.as_str(), SizeBucket::parse).unwrap(),
                b
            );
        }
        for f in [
            FileState::Discovered,
            FileState::Probing,
            FileState::Probed,
            FileState::ProbeFailed,
            FileState::Evaluated,
            FileState::Processed,
            FileState::Quarantined,
            FileState::Missing,
        ] {
            assert_eq!(
                parse_enum("file.state", f.as_str(), FileState::parse).unwrap(),
                f
            );
        }
    }

    /// A value written by a newer binary is refused, not guessed at, and the
    /// error says which column held it.
    #[test]
    fn an_unrecognised_stored_value_names_its_column() {
        let e = parse_enum("job.state", "Teleporting", JobState::parse).unwrap_err();
        match e {
            StoreError::UnknownEnum { column, value } => {
                assert_eq!(column, "job.state");
                assert_eq!(value, "Teleporting");
            }
            other => panic!("expected UnknownEnum, got {other:?}"),
        }
    }
}
