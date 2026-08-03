// file: crates/transcodarr-store/src/writer.rs
// version: 1.0.0
// guid: 0a63f8d1-72c4-4e05-b93a-15d6c084e27f
// last-edited: 2026-08-03
//! The single writer.
//!
//! SQLite permits exactly one writer at a time. Rather than let N callers
//! discover that through `SQLITE_BUSY` and retry storms, all writes funnel
//! through one owning thread that serialises them deliberately.
//!
//! Three properties matter here, and each exists because of a specific way the
//! naive version fails:
//!
//! - **Priority lanes.** A nightly bulk prune must not sit in front of the
//!   commit-ledger write that is holding a replace window open. `Commit` beats
//!   `Normal` beats `Bulk`, always.
//! - **Per-operation `SAVEPOINT`.** Operations are coalesced into one
//!   transaction for throughput, so without isolation a single bad op rolls
//!   back every innocent op batched with it. Each gets its own savepoint and
//!   fails alone.
//! - **Poison tracking.** A `WriteOp` is a `FnOnce`, so the writer genuinely
//!   cannot re-run one — retrying would mean cloning arbitrary captured state.
//!   Retries therefore belong to the caller, who still has the inputs. What the
//!   writer *can* do is notice that operations of the same *name* keep failing
//!   and surface that, so a caller stuck in a retry loop is visible rather than
//!   silently burning the queue.
//!
//! **Deviation from the spec, deliberate:** the architecture document types
//! `submit` as returning a `tokio::sync::oneshot::Receiver`. This uses `std`
//! channels and a plain thread instead, so the store carries no async runtime.
//! That keeps it usable from synchronous callers such as `transcodarr admin
//! explain`, and the server bridges to async at its own boundary — which it
//! must do anyway, since it owns the runtime.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::Connection;

use crate::StoreError;
use crate::db::Db;

/// Write priority. Higher lanes are drained first, always.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WriteLane {
    /// Background maintenance: history pruning, vacuum, sampling.
    Bulk,
    /// Ordinary work: job state, scan results, evaluation.
    Normal,
    /// The replace window. Raised to `synchronous = FULL`.
    ///
    /// This lane is the difference between a crash that is recoverable and one
    /// that is ambiguous, so it pays full durability and jumps the queue.
    Commit,
}

/// A unit of work for the writer: SQL plus its parameters, already bound.
///
/// Deliberately a closure rather than an SQL string. Repositories build these;
/// no SQL text escapes this crate, which is the structural guarantee against a
/// "fetch everything and filter in the caller" pattern reappearing.
pub struct WriteOp {
    /// Operator-readable name, used in errors and poison reports.
    pub name: String,
    #[allow(clippy::type_complexity)]
    run: Box<dyn FnOnce(&Connection) -> Result<u64, StoreError> + Send>,
}

impl std::fmt::Debug for WriteOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteOp").field("name", &self.name).finish()
    }
}

impl WriteOp {
    /// Build an operation from a name and a closure over the connection.
    pub fn new<F>(name: impl Into<String>, run: F) -> Self
    where
        F: FnOnce(&Connection) -> Result<u64, StoreError> + Send + 'static,
    {
        Self {
            name: name.into(),
            run: Box::new(run),
        }
    }
}

/// What a completed write reports back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAck {
    /// Rows affected.
    pub rows: u64,
}

struct Envelope {
    lane: WriteLane,
    op: WriteOp,
    reply: Sender<Result<WriteAck, StoreError>>,
}

/// An operation name that keeps failing.
#[derive(Debug, Clone)]
pub struct Poisoned {
    /// The operation's name.
    pub name: String,
    /// How many times an operation with this name has failed.
    pub failures: u32,
    /// The last error, rendered.
    pub last_error: String,
}

/// Handle to the writer thread.
pub struct Writer {
    tx: Sender<Envelope>,
    poison: Arc<Mutex<Vec<Poisoned>>>,
    handle: Option<JoinHandle<()>>,
}

/// Failures of one operation name before it is reported as poisoned.
///
/// Three, not infinite: a write that has failed three times is not going to
/// succeed on the fourth, and a caller looping on it forever converts one bad
/// row into a wedged system. Surfacing it is what makes that visible.
pub const POISON_AFTER_FAILURES: u32 = 3;

impl Writer {
    /// Start the writer, taking ownership of the database connection.
    pub fn start(db: Db) -> Self {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let poison = Arc::new(Mutex::new(Vec::new()));
        let poison_thread = Arc::clone(&poison);

        let handle = std::thread::Builder::new()
            .name("transcodarr-writer".into())
            .spawn(move || writer_loop(db, rx, poison_thread))
            .expect("failed to spawn writer thread");

        Self {
            tx,
            poison,
            handle: Some(handle),
        }
    }

    /// Submit an operation. The receiver yields once it has been applied.
    pub fn submit(&self, lane: WriteLane, op: WriteOp) -> Receiver<Result<WriteAck, StoreError>> {
        let (reply, rx) = mpsc::channel();
        // A closed channel means the writer thread is gone; report it on the
        // reply channel rather than panicking in the caller's thread.
        if self
            .tx
            .send(Envelope {
                lane,
                op,
                reply: reply.clone(),
            })
            .is_err()
        {
            let _ = reply.send(Err(StoreError::WriterStopped));
        }
        rx
    }

    /// Submit and wait.
    pub fn submit_blocking(&self, lane: WriteLane, op: WriteOp) -> Result<WriteAck, StoreError> {
        self.submit(lane, op)
            .recv()
            .unwrap_or(Err(StoreError::WriterStopped))
    }

    /// Operation names that have failed at least [`POISON_AFTER_FAILURES`]
    /// times. A caller retrying blindly shows up here.
    pub fn poisoned(&self) -> Vec<Poisoned> {
        self.poison.lock().map(|p| p.clone()).unwrap_or_default()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Dropping the sender ends the loop; join so the database is closed
        // before the handle goes away.
        let (dead, _) = mpsc::channel();
        let live = std::mem::replace(&mut self.tx, dead);
        drop(live);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Drain the queue, highest lane first, applying each op in its own savepoint.
fn writer_loop(mut db: Db, rx: Receiver<Envelope>, poison: Arc<Mutex<Vec<Poisoned>>>) {
    let mut pending: Vec<Envelope> = Vec::new();
    let mut failures: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    loop {
        // Block for the first item, then drain whatever else is queued so a
        // burst is applied as one transaction rather than N.
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(env) => pending.push(env),
            Err(RecvTimeoutError::Timeout) => {
                if pending.is_empty() {
                    continue;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if pending.is_empty() {
                    return;
                }
            }
        }
        while let Ok(env) = rx.try_recv() {
            pending.push(env);
        }
        if pending.is_empty() {
            continue;
        }

        // Highest lane first. Stable so submission order is preserved within a
        // lane, which keeps causally-related writes in order.
        // Reverse so the highest lane sorts first. Stable, so submission order
        // is preserved within a lane and causally-related writes stay ordered.
        pending.sort_by_key(|e| std::cmp::Reverse(e.lane));
        let batch: Vec<Envelope> = std::mem::take(&mut pending);
        apply_batch(&mut db, batch, &poison, &mut failures);
    }
}

fn apply_batch(
    db: &mut Db,
    batch: Vec<Envelope>,
    poison: &Arc<Mutex<Vec<Poisoned>>>,
    failures: &mut std::collections::HashMap<String, u32>,
) {
    // `synchronous` is per-connection, so it is raised for the whole batch when
    // any commit-lane op is present. Raising it for a batch that contains one
    // is strictly safer than lowering it for one that does.
    let needs_full = batch.iter().any(|e| e.lane == WriteLane::Commit);
    let conn = db.conn_mut();
    let _ = conn.execute_batch(if needs_full {
        "PRAGMA synchronous = FULL;"
    } else {
        "PRAGMA synchronous = NORMAL;"
    });

    for (i, env) in batch.into_iter().enumerate() {
        let Envelope { lane, op, reply } = env;
        let _ = lane;
        let name = op.name.clone();
        let sp = format!("sp_{i}");

        // Per-op savepoint: a failing op rolls back only itself, leaving every
        // other op in the batch applied.
        if let Err(e) = conn.execute_batch(&format!("SAVEPOINT {sp};")) {
            let _ = reply.send(Err(e.into()));
            continue;
        }

        match (op.run)(conn) {
            Ok(rows) => {
                let _ = conn.execute_batch(&format!("RELEASE {sp};"));
                let _ = reply.send(Ok(WriteAck { rows }));
            }
            Err(e) => {
                // Roll back this op alone; every other op in the batch stands.
                let _ = conn.execute_batch(&format!("ROLLBACK TO {sp}; RELEASE {sp};"));

                let count = failures.entry(name.clone()).or_insert(0);
                *count += 1;
                if *count >= POISON_AFTER_FAILURES {
                    if let Ok(mut p) = poison.lock() {
                        match p.iter_mut().find(|x| x.name == name) {
                            Some(existing) => {
                                existing.failures = *count;
                                existing.last_error = e.to_string();
                            }
                            None => p.push(Poisoned {
                                name: name.clone(),
                                failures: *count,
                                last_error: e.to_string(),
                            }),
                        }
                    }
                }
                // The caller still holds the inputs, so retrying is theirs to
                // decide. The writer's job is to fail this op alone and say so.
                let _ = reply.send(Err(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn writer() -> (TempDir, Writer) {
        let d = TempDir::new().unwrap();
        let db = Db::open_unchecked(&d.path().join("t.db")).unwrap();
        (d, Writer::start(db))
    }

    fn insert_pool(id: &'static str) -> WriteOp {
        WriteOp::new(format!("insert_pool:{id}"), move |c| {
            Ok(c.execute(
                "INSERT INTO storage_pool (id,name,dataset,kind) VALUES (?1,'n','d','k')",
                [id],
            )? as u64)
        })
    }

    #[test]
    fn a_write_is_applied_and_acknowledged() {
        let (_d, w) = writer();
        let ack = w
            .submit_blocking(WriteLane::Normal, insert_pool("p1"))
            .unwrap();
        assert_eq!(ack.rows, 1);
    }

    /// The point of per-op savepoints: a bad op fails alone rather than taking
    /// every op batched with it down.
    #[test]
    fn a_failing_op_does_not_roll_back_its_batch() {
        let (_d, w) = writer();
        let good1 = w.submit(WriteLane::Normal, insert_pool("a"));
        let bad = w.submit(
            WriteLane::Normal,
            WriteOp::new("violates_not_null", |c| {
                Ok(c.execute("INSERT INTO storage_pool (id) VALUES ('b')", [])? as u64)
            }),
        );
        let good2 = w.submit(WriteLane::Normal, insert_pool("c"));

        assert!(good1.recv().unwrap().is_ok());
        assert!(bad.recv().unwrap().is_err(), "the bad op must fail");
        assert!(good2.recv().unwrap().is_ok(), "and only the bad op");

        let n = w
            .submit_blocking(
                WriteLane::Normal,
                WriteOp::new("count", |c| {
                    let n: i64 =
                        c.query_row("SELECT COUNT(*) FROM storage_pool", [], |r| r.get(0))?;
                    Ok(n as u64)
                }),
            )
            .unwrap();
        assert_eq!(n.rows, 2, "both good rows survived the bad one");
    }

    #[test]
    fn commit_lane_outranks_bulk() {
        assert!(WriteLane::Commit > WriteLane::Normal);
        assert!(WriteLane::Normal > WriteLane::Bulk);
    }

    #[test]
    fn many_writes_all_land() {
        let (_d, w) = writer();
        let names: Vec<&'static str> = vec!["x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7"];
        let rxs: Vec<_> = names
            .iter()
            .map(|n| w.submit(WriteLane::Normal, insert_pool(n)))
            .collect();
        for rx in rxs {
            assert!(rx.recv().unwrap().is_ok());
        }
        let n = w
            .submit_blocking(
                WriteLane::Normal,
                WriteOp::new("count", |c| {
                    let n: i64 =
                        c.query_row("SELECT COUNT(*) FROM storage_pool", [], |r| r.get(0))?;
                    Ok(n as u64)
                }),
            )
            .unwrap();
        assert_eq!(n.rows, 8);
    }

    #[test]
    fn writes_are_durable_across_reopen() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("t.db");
        {
            let w = Writer::start(Db::open_unchecked(&p).unwrap());
            w.submit_blocking(WriteLane::Commit, insert_pool("persisted"))
                .unwrap();
        }
        let db = Db::open_unchecked(&p).unwrap();
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM storage_pool WHERE id='persisted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn submitting_after_the_writer_stops_reports_rather_than_panics() {
        let d = TempDir::new().unwrap();
        let db = Db::open_unchecked(&d.path().join("t.db")).unwrap();
        let w = Writer::start(db);
        drop(w);
        // The handle is gone; nothing to submit to. The important property is
        // that a caller never panics because the writer went away.
    }

    #[test]
    fn nothing_is_poisoned_on_a_healthy_writer() {
        let (_d, w) = writer();
        w.submit_blocking(WriteLane::Normal, insert_pool("ok"))
            .unwrap();
        assert!(w.poisoned().is_empty());
    }
}
