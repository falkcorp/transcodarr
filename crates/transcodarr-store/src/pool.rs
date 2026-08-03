// file: crates/transcodarr-store/src/pool.rs
// version: 1.0.0
// guid: c48e07b3-95d1-4f27-8a60-3e29b7d14c05
// last-edited: 2026-08-03
//! The read pool.
//!
//! SQLite in WAL mode allows any number of concurrent readers alongside the one
//! writer, so reads do not go through [`crate::Writer`] at all — routing them
//! through it would serialise the API behind the scan.
//!
//! Three things about read-only connections are easy to get wrong, and each is
//! handled here rather than discovered in production:
//!
//! - **`journal_mode` cannot be set on a read-only connection.** The pragma
//!   block is therefore a deliberate *subset* of [`crate::db`]'s, not a copy of
//!   it. Applying the full block would fail, and failing silently would leave
//!   the pool without `busy_timeout`.
//! - **A read-only connection cannot create the database.** The pool must be
//!   built after [`crate::Db::open`] has created and migrated the file, which
//!   is why [`ReadPool::open`] takes a path that is expected to exist and says
//!   so if it does not, rather than conjuring an empty database.
//! - **WAL needs its `-shm` file.** A read-only connection cannot create one,
//!   so opening a WAL database with no writer present can fail outright. In
//!   this process the writer owns the database for the pool's whole lifetime,
//!   which is exactly the arrangement that makes it safe.

use std::path::{Path, PathBuf};
use std::time::Duration;

use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;

use crate::StoreError;

/// Pragmas applied to every read connection.
///
/// A subset of the writer's block on purpose — see the module documentation.
/// `query_only` is belt and braces on top of `SQLITE_OPEN_READ_ONLY`: a repo
/// that grows a stray `UPDATE` fails loudly here instead of racing the writer.
const READ_PRAGMAS: &str = "
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
PRAGMA mmap_size    = 268435456;
PRAGMA query_only   = ON;
";

/// How long a caller waits for a free connection before giving up.
///
/// Finite rather than unbounded: a read pool exhausted by a slow query should
/// surface as an error an operator can see, not as an API that hangs forever.
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// A pool of read-only connections.
#[derive(Clone)]
pub struct ReadPool {
    pool: Pool<SqliteConnectionManager>,
    path: PathBuf,
}

impl ReadPool {
    /// Open a pool of `size` read-only connections against an existing database.
    ///
    /// The database must already exist and be migrated; a read-only connection
    /// can do neither.
    pub fn open(path: &Path, size: u32) -> Result<Self, StoreError> {
        if !path.exists() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "{} does not exist; the read pool cannot create or migrate it",
                    path.display()
                ),
            )));
        }

        let manager = SqliteConnectionManager::file(path)
            .with_flags(
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_init(|c| c.execute_batch(READ_PRAGMAS));

        let pool = Pool::builder()
            .max_size(size.max(1))
            .connection_timeout(CHECKOUT_TIMEOUT)
            .build(manager)
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("read pool: {e}"))))?;

        Ok(Self {
            pool,
            path: path.to_path_buf(),
        })
    }

    /// Check out a connection.
    pub fn get(&self) -> Result<PooledConnection<SqliteConnectionManager>, StoreError> {
        self.pool
            .get()
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("read pool: {e}"))))
    }

    /// Which database this pool reads.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many connections the pool may hold.
    pub fn size(&self) -> u32 {
        self.pool.max_size()
    }
}

impl std::fmt::Debug for ReadPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadPool")
            .field("path", &self.path)
            .field("size", &self.pool.max_size())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use tempfile::TempDir;

    fn pooled() -> (TempDir, Db, ReadPool) {
        let d = TempDir::new().unwrap();
        let p = d.path().join("t.db");
        let db = Db::open_unchecked(&p).unwrap();
        let pool = ReadPool::open(&p, 4).unwrap();
        (d, db, pool)
    }

    #[test]
    fn a_pooled_connection_can_read_the_migrated_schema() {
        let (_d, _db, pool) = pooled();
        let c = pool.get().unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM schema_migration", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// The pool is a read pool. A repo that grows a stray write must fail here
    /// rather than race the single writer.
    #[test]
    fn a_write_through_the_read_pool_is_refused() {
        let (_d, _db, pool) = pooled();
        let c = pool.get().unwrap();
        let e = c.execute(
            "INSERT INTO storage_pool (id,name,dataset,kind) VALUES ('p','n','d','k')",
            [],
        );
        assert!(e.is_err(), "the read pool must not accept writes");
    }

    /// A read-only connection cannot create a database. Conjuring an empty one
    /// would give every query a plausible, wrong, empty answer.
    #[test]
    fn opening_a_missing_database_fails_rather_than_creating_it() {
        let d = TempDir::new().unwrap();
        let e = ReadPool::open(&d.path().join("nope.db"), 2);
        assert!(matches!(e, Err(StoreError::Io(_))), "must not create");
    }

    #[test]
    fn several_connections_can_read_at_once() {
        let (_d, _db, pool) = pooled();
        let a = pool.get().unwrap();
        let b = pool.get().unwrap();
        for c in [&a, &b] {
            let n: i64 = c
                .query_row("SELECT COUNT(*) FROM library", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0);
        }
        assert_eq!(pool.size(), 4);
    }

    /// Readers must see a committed write without reopening anything — that is
    /// the whole point of WAL plus a separate read pool.
    #[test]
    fn a_reader_sees_a_committed_write() {
        let (_d, db, pool) = pooled();
        db.conn()
            .execute(
                "INSERT INTO storage_pool (id,name,dataset,kind) VALUES ('p','n','d','k')",
                [],
            )
            .unwrap();
        let c = pool.get().unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM storage_pool", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
