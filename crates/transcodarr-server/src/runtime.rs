// file: crates/transcodarr-server/src/runtime.rs
// version: 1.0.0
// guid: b5c1e08d-7f34-42a6-9013-8ae62d5f71bc
// last-edited: 2026-08-03
//! Opening the store, and the operator-facing surface over it.
//!
//! This module exists to settle a layering question the store's own
//! documentation raised: `transcodarr-store` is meant to be linked by
//! `transcodarr-server` and nothing else, but `admin explain` and friends are
//! CLI commands. Rather than make the CLI a second consumer of the database —
//! at which point two crates would need to agree about connection lifetimes,
//! pragmas and the single-writer rule — the CLI calls in here.
//!
//! The practical consequence is that no SQL, no `rusqlite` type and no
//! repository ever appears in `transcodarr-cli`.

use std::path::Path;
use std::sync::Arc;

use transcodarr_store::repo::{LibraryRecord, LibraryRepo};
use transcodarr_store::{Db, ReadPool, WriteLane, Writer};

use crate::ServerError;

/// How many read connections an operator command opens.
const CLI_READ_POOL: u32 = 8;

/// An open store: the single writer, and a pool of readers over it.
///
/// The writer owns the only write connection and the read pool needs it alive
/// for the WAL `-shm` file, so the two are created and dropped together rather
/// than handed out separately.
pub struct Runtime {
    pool: ReadPool,
    writer: Arc<Writer>,
}

impl Runtime {
    /// Open (creating and migrating if needed) the database at `path`.
    ///
    /// Runs the durability probe: a database on storage too slow to fsync makes
    /// the single writer the pacing constraint for the whole system, and it is
    /// better to say so at startup than to be mystified by throughput later.
    pub fn open(path: &Path) -> Result<Self, ServerError> {
        let db = Db::open(path)?;
        let pool = ReadPool::open(path, CLI_READ_POOL)?;
        Ok(Self {
            pool,
            writer: Arc::new(Writer::start(db)),
        })
    }

    /// Open without the durability probe. For tests and throwaway databases.
    pub fn open_unchecked(path: &Path) -> Result<Self, ServerError> {
        let db = Db::open_unchecked(path)?;
        let pool = ReadPool::open(path, CLI_READ_POOL)?;
        Ok(Self {
            pool,
            writer: Arc::new(Writer::start(db)),
        })
    }

    /// The read pool.
    pub fn pool(&self) -> &ReadPool {
        &self.pool
    }

    /// The single writer.
    pub fn writer(&self) -> &Arc<Writer> {
        &self.writer
    }

    /// Every enabled library, or just the one named.
    ///
    /// An unknown id is an error rather than an empty list: "no libraries
    /// matched" and "that library does not exist" send an operator to different
    /// places.
    pub fn libraries(&self, only: Option<&str>) -> Result<Vec<LibraryRecord>, ServerError> {
        let repo = LibraryRepo::new(self.pool.clone());
        match only {
            Some(id) => Ok(vec![repo.get(id)?]),
            None => Ok(repo.list_enabled()?),
        }
    }

    /// Register or update a library.
    #[allow(clippy::too_many_arguments)]
    pub fn add_library(
        &self,
        id: &str,
        name: &str,
        root_path: &str,
        work_dir: &str,
        trash_dir: &str,
        min_mtime_age_s: i64,
    ) -> Result<(), ServerError> {
        self.writer.submit_blocking(
            WriteLane::Normal,
            LibraryRepo::upsert_op(LibraryRecord {
                id: id.to_string(),
                name: name.to_string(),
                root_path: root_path.to_string(),
                work_dir: work_dir.to_string(),
                trash_dir: trash_dir.to_string(),
                exclude_globs_json: "[]".into(),
                enabled: true,
                scan_parallelism: 4,
                priority: 0,
                min_mtime_age_s,
            }),
        )?;
        Ok(())
    }

    /// Clear every recorded decision in a library, so the next evaluation
    /// re-derives all of them.
    ///
    /// Clearing `eval_rules_version` is what puts files back into the working
    /// set — the evaluator has no notion of "again", only of "this decision
    /// predates the current policy". Stored probe facts are untouched, which is
    /// the point: this is the operation that must cost no filesystem I/O.
    pub fn reset_evaluations(&self, library_id: &str) -> Result<u64, ServerError> {
        let id = library_id.to_string();
        let ack = self.writer.submit_blocking(
            WriteLane::Normal,
            transcodarr_store::WriteOp::new(format!("runtime.reset_eval:{id}"), move |c| {
                Ok(c.execute(
                    "UPDATE file SET eval_rules_version = NULL WHERE library_id = ?1",
                    [&id],
                )? as u64)
            }),
        )?;
        Ok(ack.rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_library_can_be_registered_and_listed_without_touching_the_store_directly() {
        let d = TempDir::new().unwrap();
        let rt = Runtime::open_unchecked(&d.path().join("t.db")).unwrap();
        rt.add_library("tv", "Television", "/mnt/tv", "/w", "/t", 300)
            .unwrap();

        let all = rt.libraries(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Television");
        assert_eq!(rt.libraries(Some("tv")).unwrap().len(), 1);
    }

    /// "No libraries matched" and "that library does not exist" send an
    /// operator to different places.
    #[test]
    fn an_unknown_library_is_an_error_not_an_empty_list() {
        let d = TempDir::new().unwrap();
        let rt = Runtime::open_unchecked(&d.path().join("t.db")).unwrap();
        assert!(rt.libraries(Some("nope")).is_err());
        assert!(rt.libraries(None).unwrap().is_empty());
    }

    #[test]
    fn registering_the_same_id_updates_rather_than_duplicating() {
        let d = TempDir::new().unwrap();
        let rt = Runtime::open_unchecked(&d.path().join("t.db")).unwrap();
        rt.add_library("tv", "Old", "/mnt/tv", "/w", "/t", 300)
            .unwrap();
        rt.add_library("tv", "New", "/mnt/tv", "/w", "/t", 900)
            .unwrap();
        let all = rt.libraries(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "New");
        assert_eq!(all[0].min_mtime_age_s, 900);
    }
}
