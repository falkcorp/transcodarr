// file: crates/transcodarr-store/src/db.rs
// version: 1.1.0
// guid: 8f5b0c26-3d71-4e94-a15c-72b6e0d38a4f
// last-edited: 2026-08-07
//! Opening the database: pragmas, migrations, and the startup durability probe.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::Connection;

use crate::StoreError;

/// Migrations, embedded at compile time.
///
/// Embedded rather than read from disk so a deployed binary cannot be paired
/// with the wrong migration directory — the schema travels with the code that
/// expects it.
pub const MIGRATIONS: &[(i64, &str, &str)] =
    &[(1, "initial", include_str!("../migrations/0001_initial.sql"))];

/// The pragma block applied to every connection.
///
/// `synchronous = NORMAL` is the baseline; [`crate::WriteLane::Commit`] raises
/// it to `FULL` around commit-ledger writes. That split is deliberate: paying
/// full durability on every job-state tick would make the writer the pacing
/// constraint, while paying it on the replace window is the difference between
/// a recoverable crash and an ambiguous one.
///
/// `wal_autocheckpoint = 0` because the writer checkpoints `PASSIVE` when idle;
/// an automatic checkpoint mid-transaction is a latency spike arriving at the
/// least convenient moment.
const PRAGMAS: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA mmap_size    = 268435456;
PRAGMA wal_autocheckpoint = 0;
";

/// Abort above this fsync p99.
///
/// Duplicated from `transcodarr-agent::preflight` by intent, not oversight:
/// the store must not depend on the agent crate. The layering rule is that an
/// agent stays copyable to the Windows node without SQLite, and inverting it to
/// share one constant would drag the whole store along with it. The number is
/// small and the reasoning is recorded in both places.
pub const FSYNC_ABORT_US: u128 = 100_000;

/// An open database handle.
pub struct Db {
    conn: Connection,
    path: PathBuf,
}

impl Db {
    /// Open (creating if needed), apply pragmas, verify durability, migrate.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_inner(path, Some(FSYNC_ABORT_US))
    }

    /// Open without the durability probe. For tests and for in-memory use,
    /// where measuring fsync latency measures nothing.
    pub fn open_unchecked(path: &Path) -> Result<Self, StoreError> {
        Self::open_inner(path, None)
    }

    /// Open against an explicit fsync ceiling.
    ///
    /// Exists so the probe itself can be tested. No real filesystem can be made
    /// to fsync reliably *slower* than a fixed limit — that is what made the
    /// failures this replaces nondeterministic — but every real filesystem is
    /// slower than zero, and none is slower than `u128::MAX`. Moving the limit
    /// instead of the hardware turns an untestable guard into two deterministic
    /// assertions.
    #[cfg(test)]
    fn open_with_fsync_limit(path: &Path, limit_us: u128) -> Result<Self, StoreError> {
        Self::open_inner(path, Some(limit_us))
    }

    /// `fsync_limit_us` of `None` skips the durability probe entirely; `Some`
    /// runs it and refuses to open above that ceiling.
    fn open_inner(path: &Path, fsync_limit_us: Option<u128>) -> Result<Self, StoreError> {
        if let Some(limit_us) = fsync_limit_us {
            let dir = path.parent().unwrap_or(Path::new("."));
            let p99 = measure_fsync_p99(dir, 200)?;
            if p99 > limit_us {
                return Err(StoreError::DurabilityTooSlow {
                    p99_us: p99,
                    limit_us,
                    path: dir.display().to_string(),
                });
            }
        }

        let conn = Connection::open(path)?;
        conn.execute_batch(PRAGMAS)?;
        verify_pragmas(&conn)?;

        let mut db = Self {
            conn,
            path: path.to_path_buf(),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Borrow the underlying connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutably borrow the underlying connection.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Where this database lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Apply every migration not yet recorded, in order.
    ///
    /// Each runs inside its own transaction together with its `schema_migration`
    /// row, so a migration and the record of it can never disagree.
    pub fn migrate(&mut self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration (
               version      INTEGER PRIMARY KEY,
               name         TEXT NOT NULL,
               checksum     TEXT NOT NULL,
               applied_unix INTEGER NOT NULL
             ) STRICT;",
        )?;

        for (version, name, sql) in MIGRATIONS {
            let checksum = blake3_hex(sql);
            let existing: Option<String> = self
                .conn
                .query_row(
                    "SELECT checksum FROM schema_migration WHERE version = ?1",
                    [version],
                    |r| r.get(0),
                )
                .ok();

            match existing {
                // An applied migration whose text has since changed means the
                // database and the binary disagree about what the schema *is*.
                // Refusing is the only safe answer; silently re-running it
                // would corrupt, and ignoring it would hide the drift.
                Some(prev) if prev != checksum => {
                    return Err(StoreError::MigrationChanged {
                        version: *version,
                        name: (*name).to_string(),
                    });
                }
                Some(_) => continue,
                None => {
                    let tx = self.conn.transaction()?;
                    tx.execute_batch(sql)?;
                    tx.execute(
                        "INSERT INTO schema_migration (version, name, checksum, applied_unix)
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![version, name, checksum, now_unix()],
                    )?;
                    tx.commit()?;
                }
            }
        }
        Ok(())
    }

    /// The highest applied migration version.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migration",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0))
    }
}

/// Confirm the pragmas actually took.
///
/// Setting a pragma is a request, not a guarantee — `journal_mode = WAL` fails
/// silently on some filesystems and leaves you in `delete` mode, where the
/// concurrency assumptions behind a single writer plus a read pool simply do
/// not hold. Asking afterwards is the only way to know.
fn verify_pragmas(conn: &Connection) -> Result<(), StoreError> {
    let journal: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    if !journal.eq_ignore_ascii_case("wal") && !journal.eq_ignore_ascii_case("memory") {
        return Err(StoreError::PragmaRejected {
            pragma: "journal_mode".into(),
            wanted: "wal".into(),
            got: journal,
        });
    }
    let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    if fk != 1 {
        return Err(StoreError::PragmaRejected {
            pragma: "foreign_keys".into(),
            wanted: "1".into(),
            got: fk.to_string(),
        });
    }
    Ok(())
}

/// Measure fsync p99 in microseconds on `dir`.
fn measure_fsync_p99(dir: &Path, iterations: usize) -> Result<u128, StoreError> {
    let path = dir.join(".transcodarr-fsync-probe");
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    let mut samples = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let buf = format!("{i:016}");
        let t = Instant::now();
        f.write_all(buf.as_bytes())?;
        f.sync_data()?;
        samples.push(t.elapsed().as_micros());
    }
    drop(f);
    let _ = std::fs::remove_file(&path);
    samples.sort_unstable();
    Ok(samples[(samples.len() * 99 / 100).min(samples.len() - 1)])
}

fn blake3_hex(s: &str) -> String {
    // Reuse core's dependency rather than adding blake3 here directly; the
    // hash only has to be stable, not cryptographically meaningful.
    transcodarr_core::stable_hash(s.as_bytes())
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_temp() -> (TempDir, Db) {
        let d = TempDir::new().unwrap();
        let db = Db::open_unchecked(&d.path().join("t.db")).unwrap();
        (d, db)
    }

    #[test]
    fn open_applies_migrations_and_records_them() {
        let (_d, db) = open_temp();
        assert_eq!(db.schema_version().unwrap(), 1);
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM schema_migration", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn migrations_are_idempotent() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("t.db");
        {
            Db::open_unchecked(&p).unwrap();
        }
        let db = Db::open_unchecked(&p).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
    }

    #[test]
    fn every_contract_table_exists() {
        let (_d, db) = open_temp();
        for t in [
            "schema_migration",
            "storage_pool",
            "pool_reclaim_sample",
            "library",
            "file",
            "file_stream",
            "file_skip_marker",
            "job",
            "job_event",
            "job_attempt",
            "commit_intent",
            "agent",
            "agent_mount",
            "agent_capability_history",
            "agent_capability_override",
            "dispatch_block",
            "config_revision",
            "schedule_window",
            "schedule_override",
            "scan_run",
            "trash_entry",
        ] {
            let n: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {t} is missing");
        }
    }

    #[test]
    fn wal_and_foreign_keys_are_actually_on() {
        let (_d, db) = open_temp();
        let j: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert!(j.eq_ignore_ascii_case("wal"));
        let fk: i64 = db
            .conn()
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    /// STRICT tables are the point: without them SQLite stores 'Running' in an
    /// INTEGER column happily, and a state machine that a typo can corrupt is
    /// not a state machine.
    #[test]
    fn strict_typing_rejects_a_wrong_type() {
        let (_d, db) = open_temp();
        let e = db.conn().execute(
            "INSERT INTO storage_pool (id, name, dataset, kind, reserve_bytes)
             VALUES ('p', 'n', 'd', 'k', 'not-a-number')",
            [],
        );
        assert!(
            e.is_err(),
            "STRICT must reject a text value in an INTEGER column"
        );
    }

    #[test]
    fn check_constraints_reject_an_unknown_job_state() {
        let (_d, db) = open_temp();
        db.conn()
            .execute_batch(
                "INSERT INTO storage_pool (id,name,dataset,kind) VALUES ('p','n','d','k');
                 INSERT INTO library (id,name,root_path,work_dir,trash_dir,created_unix,updated_unix)
                   VALUES ('l','tv','/mnt/tv','/w','/t',0,0);
                 INSERT INTO file (id,library_id,canonical_path,path_hash,size_bytes,mtime_unix,
                                   first_seen_unix,last_seen_unix)
                   VALUES (1,'l','/mnt/tv/a.mkv','h',1,0,0,0);",
            )
            .unwrap();
        let e = db.conn().execute(
            "INSERT INTO job (id,file_id,library_id,class,size_bucket,state,requirements_json,
                              requirements_bucket_key,expected_content_sig,rules_version,
                              created_unix,updated_unix)
             VALUES ('j',1,'l','Audio','Small','Bogus','[]','k','s','v',0,0)",
            [],
        );
        assert!(e.is_err(), "an unknown job state must be rejected by CHECK");
    }

    /// At most one open job per file, enforced by the database rather than by
    /// dispatcher discipline.
    #[test]
    fn only_one_open_job_per_file_is_possible() {
        let (_d, db) = open_temp();
        db.conn()
            .execute_batch(
                "INSERT INTO library (id,name,root_path,work_dir,trash_dir,created_unix,updated_unix)
                   VALUES ('l','tv','/mnt/tv','/w','/t',0,0);
                 INSERT INTO file (id,library_id,canonical_path,path_hash,size_bytes,mtime_unix,
                                   first_seen_unix,last_seen_unix)
                   VALUES (1,'l','/mnt/tv/a.mkv','h',1,0,0,0);
                 INSERT INTO job (id,file_id,library_id,class,size_bucket,state,requirements_json,
                                  requirements_bucket_key,expected_content_sig,rules_version,
                                  created_unix,updated_unix)
                   VALUES ('j1',1,'l','Audio','Small','Eligible','[]','k','s','v',0,0);",
            )
            .unwrap();

        let second = db.conn().execute(
            "INSERT INTO job (id,file_id,library_id,class,size_bucket,state,requirements_json,
                              requirements_bucket_key,expected_content_sig,rules_version,
                              created_unix,updated_unix)
             VALUES ('j2',1,'l','VideoCpu','Small','Eligible','[]','k','s','v',0,0)",
            [],
        );
        assert!(
            second.is_err(),
            "double-dispatch must be structurally impossible"
        );

        // ...but once the first job is terminal, a follow-up is allowed. That is
        // how the audio-then-video two-stage flow works at all.
        db.conn()
            .execute("UPDATE job SET state='Succeeded' WHERE id='j1'", [])
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO job (id,file_id,library_id,class,size_bucket,state,requirements_json,
                                  requirements_bucket_key,expected_content_sig,rules_version,
                                  created_unix,updated_unix)
                 VALUES ('j3',1,'l','VideoCpu','Small','Eligible','[]','k','s','v',0,0)",
                [],
            )
            .expect("a follow-up job after a terminal one must be allowed");
    }

    /// Two agents can never be mid-replace on the same final path.
    #[test]
    fn only_one_live_commit_intent_per_final_path() {
        let (_d, db) = open_temp();
        db.conn()
            .execute_batch(
                "INSERT INTO library (id,name,root_path,work_dir,trash_dir,created_unix,updated_unix)
                   VALUES ('l','tv','/mnt/tv','/w','/t',0,0);
                 INSERT INTO file (id,library_id,canonical_path,path_hash,size_bytes,mtime_unix,
                                   first_seen_unix,last_seen_unix)
                   VALUES (1,'l','/mnt/tv/a.mkv','h',1,0,0,0);
                 INSERT INTO job (id,file_id,library_id,class,size_bucket,state,requirements_json,
                                  requirements_bucket_key,expected_content_sig,rules_version,
                                  created_unix,updated_unix)
                   VALUES ('j1',1,'l','Audio','Small','Running','[]','k','s','v',0,0);
                 INSERT INTO commit_intent (id,job_id,attempt,agent_id,agent_uid,fencing_epoch,
                                            source_path,temp_path,final_path,expected_content_sig,
                                            created_unix_ms,updated_unix_ms)
                   VALUES ('i1','j1',0,'a','u',0,'/s','/tmp/x','/mnt/tv/a.mkv','s',0,0);",
            )
            .unwrap();

        let second = db.conn().execute(
            "INSERT INTO commit_intent (id,job_id,attempt,agent_id,agent_uid,fencing_epoch,
                                        source_path,temp_path,final_path,expected_content_sig,
                                        created_unix_ms,updated_unix_ms)
             VALUES ('i2','j1',1,'b','u2',0,'/s','/tmp/y','/mnt/tv/a.mkv','s',0,0)",
            [],
        );
        assert!(
            second.is_err(),
            "a second live intent on one path must be impossible"
        );

        db.conn()
            .execute(
                "UPDATE commit_intent SET state='resolved' WHERE id='i1'",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO commit_intent (id,job_id,attempt,agent_id,agent_uid,fencing_epoch,
                                            source_path,temp_path,final_path,expected_content_sig,
                                            created_unix_ms,updated_unix_ms)
                 VALUES ('i3','j1',1,'b','u2',0,'/s','/tmp/y','/mnt/tv/a.mkv','s',0,0)",
                [],
            )
            .expect("once resolved, a new intent on the same path is fine");
    }

    // The durability probe had no test of its own. Until now the only thing
    // exercising it was its own intermittent failure on macOS, where
    // /var/folders fsync p99 sits just over the 100 ms ceiling under load — a
    // different test failing on each run, none of them about durability. That
    // is not coverage; it is a guard nobody had ever watched succeed *or* fail
    // on purpose.
    //
    // These two are a pair on purpose. The refusal alone would still pass if
    // `open_inner` had been broken to reject everything, so the acceptance case
    // is what proves the refusal means something.

    #[test]
    fn the_durability_probe_refuses_a_filesystem_slower_than_its_ceiling() {
        let d = TempDir::new().unwrap();
        let err = match Db::open_with_fsync_limit(&d.path().join("t.db"), 0) {
            Ok(_) => panic!("a zero-microsecond ceiling must refuse every real filesystem"),
            Err(e) => e,
        };
        assert!(
            matches!(
                err,
                StoreError::DurabilityTooSlow {
                    limit_us: 0,
                    p99_us,
                    ..
                } if p99_us > 0
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn the_durability_probe_accepts_a_filesystem_within_its_ceiling() {
        let d = TempDir::new().unwrap();
        let db = Db::open_with_fsync_limit(&d.path().join("t.db"), u128::MAX)
            .expect("an unbounded ceiling must accept any filesystem");
        assert_eq!(db.schema_version().unwrap(), 1);
    }

    #[test]
    fn a_changed_migration_is_refused_rather_than_reapplied() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("t.db");
        {
            Db::open_unchecked(&p).unwrap();
        }
        // Simulate the binary's migration text having changed since it ran.
        {
            let c = Connection::open(&p).unwrap();
            c.execute(
                "UPDATE schema_migration SET checksum='different' WHERE version=1",
                [],
            )
            .unwrap();
        }
        let err = match Db::open_unchecked(&p) {
            Ok(_) => panic!("a changed migration must not be accepted"),
            Err(e) => e,
        };
        assert!(
            matches!(err, StoreError::MigrationChanged { version: 1, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn foreign_keys_are_enforced_not_merely_declared() {
        let (_d, db) = open_temp();
        let e = db.conn().execute(
            "INSERT INTO file (library_id,canonical_path,path_hash,size_bytes,mtime_unix,
                               first_seen_unix,last_seen_unix)
             VALUES ('no-such-library','/x','h',1,0,0,0)",
            [],
        );
        assert!(e.is_err(), "a dangling library_id must be rejected");
    }
}
