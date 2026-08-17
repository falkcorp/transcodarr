// file: crates/transcodarr-agent/src/journal.rs
// version: 1.0.1
// guid: 3a6f81d4-2c95-4e78-b013-9f57ac2e6b80
// last-edited: 2026-08-16
//! The intent journal: what the agent was about to do, written before it did it.
//!
//! The commit ritual moves a file through states no single system call can make
//! atomic. Between retiring the original and installing the replacement there
//! is a moment where the destination path holds nothing. A crash there is
//! survivable *only* if something on disk says what was in progress — otherwise
//! recovery is left inferring intent from the wreckage, and "the destination is
//! missing" is indistinguishable from "the file was deleted on purpose".
//!
//! So every phase is written and **fsynced before** the step it describes, not
//! after. A journal that lags the action is worse than none: it would claim the
//! original is intact at exactly the moment it is not.
//!
//! The file is fsynced, and so is its directory. On most filesystems a
//! `write()` plus `fsync()` on the file leaves the *directory entry* unsynced,
//! so after a power loss the journal can be a file that does not exist.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AgentError;

/// How far the ritual had got.
///
/// Ordered, and the order is the recovery logic: each phase says which side of
/// an irreversible step the agent was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum IntentPhase {
    /// Permission to install has been granted; nothing has moved yet.
    ///
    /// The original is intact. A crash here needs only the temporary file
    /// discarded.
    Granted,
    /// The original has been moved aside to the trash path.
    ///
    /// The destination path may be empty right now. This is the dangerous
    /// window, and it is the one phase where recovery has real work to do.
    Retired,
    /// The replacement is in place at the destination.
    ///
    /// A crash here needs nothing undone; the trash entry is retained for its
    /// retention period as usual.
    Installed,
}

impl IntentPhase {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            IntentPhase::Granted => "Granted",
            IntentPhase::Retired => "Retired",
            IntentPhase::Installed => "Installed",
        }
    }
}

impl std::fmt::Display for IntentPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One journal record: everything recovery needs, with no database available.
///
/// Self-contained on purpose. Recovery runs before the agent has necessarily
/// reconnected to the server, and a record that could only be interpreted
/// alongside a `commit_intent` row would be useless in exactly the situation it
/// exists for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentRecord {
    /// Which job.
    pub job_id: String,
    /// Which attempt.
    pub attempt: i64,
    /// The agent instance that owns this intent.
    pub agent_uid: String,
    /// Which boot of that agent — a resurrected agent must not adopt the
    /// in-flight work of its previous life as though it were still running.
    pub boot_id: String,
    /// Guards against a stale agent acting on a revoked assignment.
    pub fencing_epoch: i64,
    /// Where the staged output is.
    pub temp_path: PathBuf,
    /// Where it is going.
    pub final_path: PathBuf,
    /// Where the original was moved to.
    pub trash_path: PathBuf,
    /// The facts signature the job was planned against.
    pub expected_content_sig: String,
    /// How far the ritual got.
    pub phase: IntentPhase,
}

/// A durable, per-job record of an in-flight install.
#[derive(Debug, Clone)]
pub struct IntentJournal {
    dir: PathBuf,
}

impl IntentJournal {
    /// Open (creating if needed) a journal directory.
    pub fn open(dir: &Path) -> Result<Self, AgentError> {
        fs::create_dir_all(dir).map_err(|e| AgentError::Journal {
            path: dir.display().to_string(),
            source: e,
        })?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// Where this journal lives.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, job_id: &str, attempt: i64) -> PathBuf {
        let safe: String = job_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.{attempt}.intent.json"))
    }

    /// Write a record and make it durable before returning.
    ///
    /// Written to a temporary file and renamed into place, so a crash during
    /// the write leaves the *previous* record rather than a truncated one. A
    /// half-written journal is worse than a stale one: recovery would parse
    /// garbage and have no phase at all.
    pub fn record(&self, rec: &IntentRecord) -> Result<(), AgentError> {
        let final_path = self.path_for(&rec.job_id, rec.attempt);
        let tmp = final_path.with_extension("writing");

        let body = serde_json::to_vec_pretty(rec).map_err(|e| AgentError::Journal {
            path: final_path.display().to_string(),
            source: std::io::Error::other(e),
        })?;

        let write = || -> std::io::Result<()> {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&body)?;
            // The data, then the metadata. Without this the rename below can
            // publish a file whose contents are still in page cache.
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &final_path)?;
            // ...and the directory entry, or after a power loss the journal is
            // a file that does not exist.
            sync_dir(&self.dir)?;
            Ok(())
        };
        write().map_err(|e| AgentError::Journal {
            path: final_path.display().to_string(),
            source: e,
        })
    }

    /// Read one record back, if it is there.
    pub fn read(&self, job_id: &str, attempt: i64) -> Result<Option<IntentRecord>, AgentError> {
        let p = self.path_for(job_id, attempt);
        match fs::read(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AgentError::Journal {
                path: p.display().to_string(),
                source: e,
            }),
            Ok(bytes) => {
                serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(|e| AgentError::Journal {
                        path: p.display().to_string(),
                        source: std::io::Error::other(e),
                    })
            }
        }
    }

    /// Every record still on disk.
    ///
    /// What recovery iterates at startup. An unparseable record is *reported*,
    /// never skipped: a journal we cannot read describes an install we cannot
    /// reason about, and silently ignoring it is how a half-installed file
    /// becomes permanent.
    pub fn outstanding(&self) -> Result<Vec<IntentRecord>, AgentError> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.dir).map_err(|e| AgentError::Journal {
            path: self.dir.display().to_string(),
            source: e,
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| AgentError::Journal {
                path: self.dir.display().to_string(),
                source: e,
            })?;
            let path = entry.path();
            if path.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }
            let bytes = fs::read(&path).map_err(|e| AgentError::Journal {
                path: path.display().to_string(),
                source: e,
            })?;
            let rec: IntentRecord =
                serde_json::from_slice(&bytes).map_err(|e| AgentError::Journal {
                    path: path.display().to_string(),
                    source: std::io::Error::other(e),
                })?;
            out.push(rec);
        }
        out.sort_by(|a, b| (&a.job_id, a.attempt).cmp(&(&b.job_id, b.attempt)));
        Ok(out)
    }

    /// Drop a record once its install is fully resolved.
    pub fn clear(&self, job_id: &str, attempt: i64) -> Result<(), AgentError> {
        let p = self.path_for(job_id, attempt);
        match fs::remove_file(&p) {
            Ok(()) => sync_dir(&self.dir).map_err(|e| AgentError::Journal {
                path: self.dir.display().to_string(),
                source: e,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AgentError::Journal {
                path: p.display().to_string(),
                source: e,
            }),
        }
    }
}

/// fsync a directory, so renames and unlinks within it survive a power loss.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    // Opening a directory read-only and syncing it is the portable-enough way
    // to flush its entries; on Windows this is not meaningful, and the agent
    // there is produce-only for related reasons.
    //
    // `File` is named in full rather than imported at the top of the module,
    // because this is its only use and it sits behind `cfg(unix)`. Imported
    // unconditionally it is an unused import on the Windows target — which CI
    // never builds, being Linux-only, so the warning was reachable solely by
    // the cross-compile that produces the agent this crate exists to ship.
    #[cfg(unix)]
    {
        let f = std::fs::File::open(dir)?;
        f.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn record(phase: IntentPhase) -> IntentRecord {
        IntentRecord {
            job_id: "job-1".into(),
            attempt: 0,
            agent_uid: "agent-a".into(),
            boot_id: "boot-1".into(),
            fencing_epoch: 3,
            temp_path: "/w/job-1.0.partial.mkv".into(),
            final_path: "/mnt/tv/a.mkv".into(),
            trash_path: "/t/a.mkv".into(),
            expected_content_sig: "sig".into(),
            phase,
        }
    }

    #[test]
    fn a_record_round_trips() {
        let d = TempDir::new().unwrap();
        let j = IntentJournal::open(d.path()).unwrap();
        let rec = record(IntentPhase::Granted);
        j.record(&rec).unwrap();
        assert_eq!(j.read("job-1", 0).unwrap(), Some(rec));
    }

    #[test]
    fn a_missing_record_is_none_not_an_error() {
        let d = TempDir::new().unwrap();
        let j = IntentJournal::open(d.path()).unwrap();
        assert_eq!(j.read("nope", 0).unwrap(), None);
    }

    /// Phases advance in place. Recovery reads the latest, so an overwrite that
    /// left both would make "how far did it get" ambiguous.
    #[test]
    fn advancing_a_phase_replaces_the_record() {
        let d = TempDir::new().unwrap();
        let j = IntentJournal::open(d.path()).unwrap();
        j.record(&record(IntentPhase::Granted)).unwrap();
        j.record(&record(IntentPhase::Retired)).unwrap();
        j.record(&record(IntentPhase::Installed)).unwrap();

        assert_eq!(
            j.read("job-1", 0).unwrap().unwrap().phase,
            IntentPhase::Installed
        );
        assert_eq!(j.outstanding().unwrap().len(), 1);
    }

    #[test]
    fn outstanding_lists_every_live_record() {
        let d = TempDir::new().unwrap();
        let j = IntentJournal::open(d.path()).unwrap();
        for i in 0..3 {
            let mut r = record(IntentPhase::Granted);
            r.job_id = format!("job-{i}");
            j.record(&r).unwrap();
        }
        assert_eq!(j.outstanding().unwrap().len(), 3);

        j.clear("job-1", 0).unwrap();
        let left: Vec<_> = j
            .outstanding()
            .unwrap()
            .into_iter()
            .map(|r| r.job_id)
            .collect();
        assert_eq!(left, vec!["job-0", "job-2"]);
    }

    /// A journal we cannot read describes an install we cannot reason about.
    /// Skipping it silently is how a half-installed file becomes permanent.
    #[test]
    fn an_unreadable_record_is_reported_not_skipped() {
        let d = TempDir::new().unwrap();
        let j = IntentJournal::open(d.path()).unwrap();
        j.record(&record(IntentPhase::Retired)).unwrap();
        fs::write(d.path().join("garbage.json"), b"{not json").unwrap();
        assert!(j.outstanding().is_err(), "must not silently ignore it");
    }

    /// A crash mid-write must leave the previous record, not a truncated one:
    /// recovery would otherwise parse garbage and have no phase at all.
    #[test]
    fn a_partial_write_never_replaces_a_good_record() {
        let d = TempDir::new().unwrap();
        let j = IntentJournal::open(d.path()).unwrap();
        j.record(&record(IntentPhase::Retired)).unwrap();

        // The staging name the writer uses; a crash leaves this behind.
        fs::write(d.path().join("job-1.0.intent.writing"), b"{truncated").unwrap();

        assert_eq!(
            j.read("job-1", 0).unwrap().unwrap().phase,
            IntentPhase::Retired,
            "the good record must still be readable"
        );
        assert_eq!(
            j.outstanding().unwrap().len(),
            1,
            "the staging file must not be mistaken for a record"
        );
    }

    #[test]
    fn clearing_a_missing_record_is_not_an_error() {
        let d = TempDir::new().unwrap();
        let j = IntentJournal::open(d.path()).unwrap();
        j.clear("nope", 0).unwrap();
    }

    /// The phase ordering *is* the recovery logic, so it must not drift.
    #[test]
    fn phases_are_ordered_by_how_far_the_ritual_got() {
        assert!(IntentPhase::Granted < IntentPhase::Retired);
        assert!(IntentPhase::Retired < IntentPhase::Installed);
    }
}
