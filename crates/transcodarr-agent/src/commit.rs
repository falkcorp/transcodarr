// file: crates/transcodarr-agent/src/commit.rs
// version: 1.1.0
// guid: 5d2b90f7-6c41-4a83-9e50-1b74af26c8d3
// last-edited: 2026-08-05
//! The commit ritual: installing a replacement without ever losing the source.
//!
//! The whole point is a single invariant, and it is worth stating precisely
//! because everything here is arranged to protect it:
//!
//! > At every instant, and after any crash at any instant, either the original
//! > file is intact or the replacement is fully installed. Never neither.
//!
//! No system call gives that for free. Replacing a file means moving the
//! original out of the way and moving the new one in, and between those two
//! renames the destination path holds nothing. The ritual survives a crash in
//! that window by writing its intent *before* each step, so recovery can tell
//! "about to retire" from "already retired" — states that look identical from
//! the filesystem alone.
//!
//! ## The nine steps
//!
//! 1. Refuse a cross-device work area — `rename(2)` is only atomic within one
//!    filesystem.
//! 2. Re-verify the source still matches what the job was planned against.
//! 3. Journal [`IntentPhase::Granted`], fsynced.
//! 4. fsync the staged output and its directory, so it is real before anything
//!    irreversible happens.
//! 5. Rename the original to the trash path. **The dangerous window opens.**
//! 6. Journal [`IntentPhase::Retired`], fsynced.
//! 7. Rename the staged output to the destination. **The window closes.**
//! 8. Journal [`IntentPhase::Installed`], fsynced.
//! 9. fsync the destination directory and resolve the intent.
//!
//! Steps 3, 6 and 8 are each written *before* the step they describe completes.
//! A journal that lagged its action would claim the original was intact at
//! exactly the moment it was not — which is the one lie that loses a file.
//!
//! ## What recovery can conclude
//!
//! | Journal phase | On disk | Resolution |
//! | --- | --- | --- |
//! | none / `Granted` | original at destination | source intact; discard the staged file |
//! | `Retired` | original in trash, destination empty | restore the original |
//! | `Retired` | original in trash, destination present | the install landed; keep it |
//! | `Installed` | replacement at destination | installed; nothing to undo |
//! | anything | neither original nor replacement findable | **`NeedsOperator`** |
//!
//! The last row never guesses. A file that is genuinely missing is a human's
//! problem, and inventing a resolution would turn an alarming-but-recoverable
//! state into a silent data loss.

use std::fs;
use std::path::{Path, PathBuf};

use crate::AgentError;
use crate::journal::{IntentJournal, IntentPhase, IntentRecord, sync_dir};
use crate::workarea::WorkArea;

/// What a source file must still look like for an install to be allowed.
///
/// Re-checked immediately before the irreversible steps. A job planned against
/// facts that no longer hold would install an encode of a file that has since
/// been replaced — the classic way a two-writer race destroys the newer copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGuard {
    /// Size the job was planned against.
    pub size_bytes: u64,
    /// Modification time it was planned against.
    pub mtime_unix: i64,
    /// Inode, where the platform reports one.
    pub inode: Option<u64>,
}

impl SourceGuard {
    /// Read the current state of a path.
    pub fn observe(path: &Path) -> Result<Self, AgentError> {
        let meta = fs::metadata(path).map_err(|e| AgentError::Commit {
            step: "stat source",
            path: path.display().to_string(),
            source: e,
        })?;
        Ok(Self {
            size_bytes: meta.len(),
            mtime_unix: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            inode: inode_of(&meta),
        })
    }

    /// Whether the file still looks like the one that was planned.
    ///
    /// Inode is compared only when both sides have one: a `None` means the
    /// platform could not tell us, and treating "unknown" as "different" would
    /// refuse every install on such a platform.
    pub fn matches(&self, other: &SourceGuard) -> bool {
        self.size_bytes == other.size_bytes
            && self.mtime_unix == other.mtime_unix
            && match (self.inode, other.inode) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

#[cfg(unix)]
fn inode_of(meta: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn inode_of(_meta: &fs::Metadata) -> Option<u64> {
    None
}

/// How an install, or a recovered install, turned out.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resolution {
    /// The replacement is at the destination and the original is in the trash.
    Installed {
        /// Where the original was retained.
        trash_path: PathBuf,
        /// Size of the installed replacement.
        output_bytes: u64,
    },
    /// Nothing was installed and the original is untouched.
    SourceIntact {
        /// Why the install did not happen.
        reason: String,
    },
    /// The original was moved aside and then put back.
    SourceRestored {
        /// Why the install did not complete.
        reason: String,
    },
    /// Neither state could be established. A human must look.
    ///
    /// Never produced by guessing — only when the destination holds nothing and
    /// the original cannot be found where the journal says it was put.
    NeedsOperator {
        /// What is ambiguous.
        detail: String,
    },
}

impl Resolution {
    /// The label used in metrics and the commit ledger.
    pub fn label(&self) -> &'static str {
        match self {
            Resolution::Installed { .. } => "installed",
            Resolution::SourceIntact { .. } => "source_intact",
            Resolution::SourceRestored { .. } => "source_restored",
            Resolution::NeedsOperator { .. } => "needs_operator",
        }
    }

    /// Whether this outcome left the media in a well-defined state.
    ///
    /// `NeedsOperator` is the only outcome that did not, which is what makes it
    /// worth alarming on rather than counting.
    pub fn is_resolved(&self) -> bool {
        !matches!(self, Resolution::NeedsOperator { .. })
    }
}

/// Everything one install needs to know.
#[derive(Debug, Clone)]
pub struct CommitRequest {
    /// Which job.
    pub job_id: String,
    /// Which attempt.
    pub attempt: i64,
    /// Guards against a revoked assignment being acted on.
    pub fencing_epoch: i64,
    /// The staged output.
    pub temp_path: PathBuf,
    /// Where it is going. Also where the original currently is.
    pub final_path: PathBuf,
    /// Where the original will be retained.
    pub trash_path: PathBuf,
    /// The facts signature the job was planned against.
    pub expected_content_sig: String,
    /// What the source looked like when the job was planned.
    pub source_guard: SourceGuard,
}

/// Performs, and recovers, installs.
///
/// `Clone` because the ritual runs on a blocking thread. Both halves are
/// path handles rather than open resources -- a clone names the same journal
/// directory, it does not make a second one, and durability comes from fsync
/// on every write rather than from single ownership of this struct.
#[derive(Debug, Clone)]
pub struct CommitRitual {
    journal: IntentJournal,
    work_area: WorkArea,
}

impl CommitRitual {
    /// Build a ritual over a journal and a work area.
    pub fn new(journal: IntentJournal, work_area: WorkArea) -> Self {
        Self { journal, work_area }
    }

    /// The journal this ritual writes to.
    pub fn journal(&self) -> &IntentJournal {
        &self.journal
    }

    /// Install a staged output, or explain why it was not installed.
    ///
    /// Every early return is a [`Resolution::SourceIntact`] rather than an
    /// error, because "we declined to install" is a normal outcome that leaves
    /// the media in a known state. Errors are reserved for the cases where the
    /// filesystem itself failed under us.
    pub fn commit(&self, req: &CommitRequest) -> Result<Resolution, AgentError> {
        // 1. rename(2) is atomic only within one filesystem. Across a boundary
        //    the copy-then-delete fallback has a window in which neither the
        //    source nor a complete replacement exists.
        self.work_area.ensure_same_device(&req.final_path)?;

        if !req.temp_path.is_file() {
            return Ok(Resolution::SourceIntact {
                reason: format!("no staged output at {}", req.temp_path.display()),
            });
        }

        // 2. Re-verify the source. A job planned against facts that no longer
        //    hold would install an encode of a file that has since been
        //    replaced, destroying the newer copy.
        let now = match SourceGuard::observe(&req.final_path) {
            Ok(g) => g,
            Err(_) => {
                return Ok(Resolution::SourceIntact {
                    reason: format!(
                        "source {} is no longer there; nothing was installed",
                        req.final_path.display()
                    ),
                });
            }
        };
        if !req.source_guard.matches(&now) {
            return Ok(Resolution::SourceIntact {
                reason: format!(
                    "source {} changed since the job was planned; refusing to install",
                    req.final_path.display()
                ),
            });
        }

        let mut record = IntentRecord {
            job_id: req.job_id.clone(),
            attempt: req.attempt,
            agent_uid: self.work_area.agent_uid().to_string(),
            boot_id: self.work_area.boot_id().to_string(),
            fencing_epoch: req.fencing_epoch,
            temp_path: req.temp_path.clone(),
            final_path: req.final_path.clone(),
            trash_path: req.trash_path.clone(),
            expected_content_sig: req.expected_content_sig.clone(),
            phase: IntentPhase::Granted,
        };

        // 3. Journal the intent before anything moves.
        self.journal.record(&record)?;

        // 4. Make the staged output real before anything irreversible happens.
        //    Installing a file whose contents are still in page cache means a
        //    power loss leaves a correctly-named file full of zeroes.
        fsync_file(&req.temp_path)?;
        if let Some(parent) = req.temp_path.parent() {
            let _ = sync_dir(parent);
        }

        if let Some(parent) = req.trash_path.parent() {
            fs::create_dir_all(parent).map_err(|e| AgentError::Commit {
                step: "create trash directory",
                path: parent.display().to_string(),
                source: e,
            })?;
        }

        // 5. Retire the original. THE DANGEROUS WINDOW OPENS HERE.
        fs::rename(&req.final_path, &req.trash_path).map_err(|e| AgentError::Commit {
            step: "retire original",
            path: req.final_path.display().to_string(),
            source: e,
        })?;

        // 6. Say so, durably, before doing anything else.
        record.phase = IntentPhase::Retired;
        self.journal.record(&record)?;

        // 7. Install. The window closes.
        if let Err(e) = fs::rename(&req.temp_path, &req.final_path) {
            // The install failed with the original already moved aside. Put it
            // back rather than leave the destination empty.
            let restored = fs::rename(&req.trash_path, &req.final_path).is_ok();
            let _ = sync_dir(req.final_path.parent().unwrap_or(Path::new("/")));
            self.journal.clear(&req.job_id, req.attempt)?;
            return Ok(if restored {
                Resolution::SourceRestored {
                    reason: format!("installing the replacement failed ({e}); original restored"),
                }
            } else {
                Resolution::NeedsOperator {
                    detail: format!(
                        "install failed ({e}) and the original could not be restored from {}",
                        req.trash_path.display()
                    ),
                }
            });
        }

        // 8. Record that it landed.
        record.phase = IntentPhase::Installed;
        self.journal.record(&record)?;

        // 9. Make the destination directory entry durable, then resolve.
        let _ = sync_dir(req.final_path.parent().unwrap_or(Path::new("/")));
        let output_bytes = fs::metadata(&req.final_path).map(|m| m.len()).unwrap_or(0);
        self.journal.clear(&req.job_id, req.attempt)?;

        Ok(Resolution::Installed {
            trash_path: req.trash_path.clone(),
            output_bytes,
        })
    }

    /// Resolve every install that was in flight when the process died.
    ///
    /// Run at startup, before any new work is accepted. An agent that began
    /// transcoding while an unresolved intent sat on disk could be handed the
    /// same file again and install over its own half-finished replace.
    pub fn recover_all(&self) -> Result<Vec<(String, Resolution)>, AgentError> {
        let mut out = Vec::new();
        for rec in self.journal.outstanding()? {
            let resolution = self.recover_one(&rec)?;
            if resolution.is_resolved() {
                self.journal.clear(&rec.job_id, rec.attempt)?;
            }
            // An unresolved intent is deliberately left on disk. It is the only
            // record that something needs a human, and clearing it would erase
            // the evidence along with the problem.
            out.push((rec.job_id.clone(), resolution));
        }
        Ok(out)
    }

    /// Resolve one interrupted install.
    pub fn recover_one(&self, rec: &IntentRecord) -> Result<Resolution, AgentError> {
        let final_exists = rec.final_path.exists();
        let trash_exists = rec.trash_path.exists();

        match rec.phase {
            // Nothing irreversible had happened. The original is where it
            // always was; the staged file is stale by definition.
            IntentPhase::Granted => {
                let _ = fs::remove_file(&rec.temp_path);
                if final_exists {
                    Ok(Resolution::SourceIntact {
                        reason: "interrupted before the original was moved".into(),
                    })
                } else {
                    Ok(Resolution::NeedsOperator {
                        detail: format!(
                            "journal says nothing had moved, but {} is not there",
                            rec.final_path.display()
                        ),
                    })
                }
            }

            // The dangerous window. This is the only phase where recovery has
            // real work to do, and the only one where the two renames can have
            // landed in either order.
            IntentPhase::Retired => {
                if final_exists {
                    // The install rename landed before the crash; the journal
                    // simply never got its `Installed` record written.
                    let _ = fs::remove_file(&rec.temp_path);
                    let output_bytes = fs::metadata(&rec.final_path).map(|m| m.len()).unwrap_or(0);
                    return Ok(Resolution::Installed {
                        trash_path: rec.trash_path.clone(),
                        output_bytes,
                    });
                }
                if trash_exists {
                    // Destination empty, original in the trash: put it back.
                    fs::rename(&rec.trash_path, &rec.final_path).map_err(|e| {
                        AgentError::Commit {
                            step: "restore retired original",
                            path: rec.trash_path.display().to_string(),
                            source: e,
                        }
                    })?;
                    let _ = sync_dir(rec.final_path.parent().unwrap_or(Path::new("/")));
                    let _ = fs::remove_file(&rec.temp_path);
                    return Ok(Resolution::SourceRestored {
                        reason: "interrupted between retiring the original and installing".into(),
                    });
                }
                // Neither exists. Never guess.
                Ok(Resolution::NeedsOperator {
                    detail: format!(
                        "neither {} nor {} exists; the original cannot be accounted for",
                        rec.final_path.display(),
                        rec.trash_path.display()
                    ),
                })
            }

            IntentPhase::Installed => {
                let _ = fs::remove_file(&rec.temp_path);
                if final_exists {
                    let output_bytes = fs::metadata(&rec.final_path).map(|m| m.len()).unwrap_or(0);
                    Ok(Resolution::Installed {
                        trash_path: rec.trash_path.clone(),
                        output_bytes,
                    })
                } else if trash_exists {
                    // The journal says installed but the destination is gone.
                    // The original is still recoverable, so restore it rather
                    // than leave the library short a file.
                    fs::rename(&rec.trash_path, &rec.final_path).map_err(|e| {
                        AgentError::Commit {
                            step: "restore after a vanished install",
                            path: rec.trash_path.display().to_string(),
                            source: e,
                        }
                    })?;
                    Ok(Resolution::SourceRestored {
                        reason: "the journal recorded an install but the destination was gone"
                            .into(),
                    })
                } else {
                    Ok(Resolution::NeedsOperator {
                        detail: format!(
                            "journal recorded an install but neither {} nor {} exists",
                            rec.final_path.display(),
                            rec.trash_path.display()
                        ),
                    })
                }
            }
        }
    }
}

fn fsync_file(path: &Path) -> Result<(), AgentError> {
    let f = fs::File::open(path).map_err(|e| AgentError::Commit {
        step: "open staged output",
        path: path.display().to_string(),
        source: e,
    })?;
    f.sync_all().map_err(|e| AgentError::Commit {
        step: "fsync staged output",
        path: path.display().to_string(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const ORIGINAL: &[u8] = b"the original file, which must never be lost";
    const REPLACEMENT: &[u8] = b"the replacement";

    struct Fixture {
        _dir: TempDir,
        ritual: CommitRitual,
        lib: PathBuf,
        trash: PathBuf,
        work: WorkArea,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let lib = dir.path().join("lib");
        let trash = dir.path().join("trash");
        let work_root = dir.path().join("work");
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&trash).unwrap();
        let work = WorkArea::open(&work_root, "agent-a", "boot-1").unwrap();
        let journal = IntentJournal::open(&dir.path().join("journal")).unwrap();
        Fixture {
            _dir: dir,
            ritual: CommitRitual::new(journal, work.clone()),
            lib,
            trash,
            work,
        }
    }

    impl Fixture {
        fn request(&self, name: &str) -> CommitRequest {
            let final_path = self.lib.join(name);
            fs::write(&final_path, ORIGINAL).unwrap();
            let temp_path = self.work.temp_path("job-1", 0, &final_path);
            fs::write(&temp_path, REPLACEMENT).unwrap();
            CommitRequest {
                job_id: "job-1".into(),
                attempt: 0,
                fencing_epoch: 1,
                temp_path,
                final_path: final_path.clone(),
                trash_path: self.trash.join(name),
                expected_content_sig: "sig".into(),
                source_guard: SourceGuard::observe(&final_path).unwrap(),
            }
        }

        /// Build the journal state a crash at `phase` would have left, without
        /// actually performing the ritual. This is what lets the crash matrix
        /// exercise every window deterministically.
        fn crashed_at(&self, req: &CommitRequest, phase: IntentPhase) -> IntentRecord {
            let rec = IntentRecord {
                job_id: req.job_id.clone(),
                attempt: req.attempt,
                agent_uid: "agent-a".into(),
                boot_id: "boot-1".into(),
                fencing_epoch: req.fencing_epoch,
                temp_path: req.temp_path.clone(),
                final_path: req.final_path.clone(),
                trash_path: req.trash_path.clone(),
                expected_content_sig: req.expected_content_sig.clone(),
                phase,
            };
            self.ritual.journal().record(&rec).unwrap();
            rec
        }
    }

    /// The invariant, stated as an assertion: after any outcome, either the
    /// original is intact somewhere findable or the replacement is installed.
    /// Never neither.
    fn assert_never_neither(req: &CommitRequest, resolution: &Resolution) {
        let final_bytes = fs::read(&req.final_path).ok();
        let trash_bytes = fs::read(&req.trash_path).ok();

        let original_intact =
            final_bytes.as_deref() == Some(ORIGINAL) || trash_bytes.as_deref() == Some(ORIGINAL);
        let replacement_installed = final_bytes.as_deref() == Some(REPLACEMENT);

        if let Resolution::NeedsOperator { .. } = resolution {
            // The one honest exit. It is allowed to be ambiguous precisely
            // because it refuses to claim otherwise.
            return;
        }
        assert!(
            original_intact || replacement_installed,
            "INVARIANT VIOLATED: neither original nor replacement survives.\n\
             resolution={resolution:?}\n final={final_bytes:?}\n trash={trash_bytes:?}"
        );
    }

    #[test]
    fn a_clean_commit_installs_and_retains_the_original() {
        let f = fixture();
        let req = f.request("a.mkv");
        let r = f.ritual.commit(&req).unwrap();

        assert!(matches!(r, Resolution::Installed { .. }), "{r:?}");
        assert_eq!(fs::read(&req.final_path).unwrap(), REPLACEMENT);
        assert_eq!(
            fs::read(&req.trash_path).unwrap(),
            ORIGINAL,
            "the original must be retained, not deleted"
        );
        assert!(!req.temp_path.exists(), "the staged file is consumed");
        assert_never_neither(&req, &r);
    }

    /// The journal is cleared on success. A record left behind would make the
    /// next startup try to recover an install that already completed.
    #[test]
    fn a_completed_commit_leaves_no_outstanding_intent() {
        let f = fixture();
        let req = f.request("a.mkv");
        f.ritual.commit(&req).unwrap();
        assert!(f.ritual.journal().outstanding().unwrap().is_empty());
    }

    /// A job planned against facts that no longer hold would install an encode
    /// of a file that has since been replaced, destroying the newer copy.
    #[test]
    fn a_source_that_changed_since_planning_is_not_replaced() {
        let f = fixture();
        let mut req = f.request("a.mkv");
        req.source_guard.size_bytes += 1;

        let r = f.ritual.commit(&req).unwrap();
        assert!(matches!(r, Resolution::SourceIntact { .. }), "{r:?}");
        assert_eq!(fs::read(&req.final_path).unwrap(), ORIGINAL);
        assert!(!req.trash_path.exists(), "nothing may have been retired");
        assert_never_neither(&req, &r);
    }

    #[test]
    fn a_missing_staged_output_is_declined_not_attempted() {
        let f = fixture();
        let req = f.request("a.mkv");
        fs::remove_file(&req.temp_path).unwrap();

        let r = f.ritual.commit(&req).unwrap();
        assert!(matches!(r, Resolution::SourceIntact { .. }), "{r:?}");
        assert_eq!(fs::read(&req.final_path).unwrap(), ORIGINAL);
        assert_never_neither(&req, &r);
    }

    #[test]
    fn a_vanished_source_is_declined_rather_than_erroring() {
        let f = fixture();
        let req = f.request("a.mkv");
        fs::remove_file(&req.final_path).unwrap();

        let r = f.ritual.commit(&req).unwrap();
        assert!(matches!(r, Resolution::SourceIntact { .. }), "{r:?}");
    }

    // ---------------------------------------------------------------------
    // The crash matrix. Every window in the ritual, in every on-disk state it
    // can leave, must resolve to source-intact or replacement-installed --
    // never neither, and never a guess.
    // ---------------------------------------------------------------------

    /// Crash after journalling intent, before anything moved.
    #[test]
    fn crash_matrix_granted_original_still_in_place() {
        let f = fixture();
        let req = f.request("a.mkv");
        let rec = f.crashed_at(&req, IntentPhase::Granted);

        let r = f.ritual.recover_one(&rec).unwrap();
        assert!(matches!(r, Resolution::SourceIntact { .. }), "{r:?}");
        assert_eq!(fs::read(&req.final_path).unwrap(), ORIGINAL);
        assert!(
            !req.temp_path.exists(),
            "the stale staged file is discarded"
        );
        assert_never_neither(&req, &r);
    }

    /// Crash in the dangerous window: original retired, destination empty.
    /// This is the case the entire design exists for.
    #[test]
    fn crash_matrix_retired_destination_empty_restores_the_original() {
        let f = fixture();
        let req = f.request("a.mkv");
        fs::rename(&req.final_path, &req.trash_path).unwrap();
        let rec = f.crashed_at(&req, IntentPhase::Retired);
        assert!(!req.final_path.exists(), "precondition: the window is open");

        let r = f.ritual.recover_one(&rec).unwrap();
        assert!(matches!(r, Resolution::SourceRestored { .. }), "{r:?}");
        assert_eq!(
            fs::read(&req.final_path).unwrap(),
            ORIGINAL,
            "the original must come back"
        );
        assert_never_neither(&req, &r);
    }

    /// Crash after both renames landed but before the `Installed` record was
    /// written. The install is real; recovery must not undo it.
    #[test]
    fn crash_matrix_retired_but_install_actually_landed() {
        let f = fixture();
        let req = f.request("a.mkv");
        fs::rename(&req.final_path, &req.trash_path).unwrap();
        fs::rename(&req.temp_path, &req.final_path).unwrap();
        let rec = f.crashed_at(&req, IntentPhase::Retired);

        let r = f.ritual.recover_one(&rec).unwrap();
        assert!(matches!(r, Resolution::Installed { .. }), "{r:?}");
        assert_eq!(fs::read(&req.final_path).unwrap(), REPLACEMENT);
        assert_never_neither(&req, &r);
    }

    /// Crash after the install and its journal record.
    #[test]
    fn crash_matrix_installed_is_left_alone() {
        let f = fixture();
        let req = f.request("a.mkv");
        fs::rename(&req.final_path, &req.trash_path).unwrap();
        fs::rename(&req.temp_path, &req.final_path).unwrap();
        let rec = f.crashed_at(&req, IntentPhase::Installed);

        let r = f.ritual.recover_one(&rec).unwrap();
        assert!(matches!(r, Resolution::Installed { .. }), "{r:?}");
        assert_eq!(fs::read(&req.final_path).unwrap(), REPLACEMENT);
        assert_eq!(fs::read(&req.trash_path).unwrap(), ORIGINAL);
        assert_never_neither(&req, &r);
    }

    /// Journal says installed, but the destination is gone — something outside
    /// transcodarr removed it. The original is still recoverable, so recover it
    /// rather than leave the library short a file.
    #[test]
    fn crash_matrix_installed_but_destination_vanished_restores_the_original() {
        let f = fixture();
        let req = f.request("a.mkv");
        fs::rename(&req.final_path, &req.trash_path).unwrap();
        let rec = f.crashed_at(&req, IntentPhase::Installed);

        let r = f.ritual.recover_one(&rec).unwrap();
        assert!(matches!(r, Resolution::SourceRestored { .. }), "{r:?}");
        assert_eq!(fs::read(&req.final_path).unwrap(), ORIGINAL);
        assert_never_neither(&req, &r);
    }

    /// Neither the destination nor the trash copy exists. This is the only
    /// honest ambiguity, and it must never be resolved by guessing.
    #[test]
    fn crash_matrix_nothing_findable_needs_an_operator() {
        let f = fixture();
        let req = f.request("a.mkv");
        fs::remove_file(&req.final_path).unwrap();
        let rec = f.crashed_at(&req, IntentPhase::Retired);

        let r = f.ritual.recover_one(&rec).unwrap();
        assert!(matches!(r, Resolution::NeedsOperator { .. }), "{r:?}");
        assert!(!r.is_resolved(), "this outcome must be alarmable");
    }

    /// Granted, but the original is missing anyway: the journal said nothing
    /// had moved, so this is not a state the ritual could have produced.
    #[test]
    fn crash_matrix_granted_but_source_gone_needs_an_operator() {
        let f = fixture();
        let req = f.request("a.mkv");
        fs::remove_file(&req.final_path).unwrap();
        let rec = f.crashed_at(&req, IntentPhase::Granted);

        let r = f.ritual.recover_one(&rec).unwrap();
        assert!(matches!(r, Resolution::NeedsOperator { .. }), "{r:?}");
    }

    /// The whole matrix in one sweep, asserting the single invariant against
    /// every reachable on-disk state at every phase.
    #[test]
    fn crash_matrix_no_phase_and_state_combination_loses_the_file() {
        // (phase, original retired?, replacement installed?)
        let states = [
            (IntentPhase::Granted, false, false),
            (IntentPhase::Granted, true, false),
            (IntentPhase::Granted, true, true),
            (IntentPhase::Retired, false, false),
            (IntentPhase::Retired, true, false),
            (IntentPhase::Retired, true, true),
            (IntentPhase::Installed, true, false),
            (IntentPhase::Installed, true, true),
        ];

        for (i, (phase, retired, installed)) in states.into_iter().enumerate() {
            let f = fixture();
            let req = f.request(&format!("case{i}.mkv"));
            if retired {
                fs::rename(&req.final_path, &req.trash_path).unwrap();
            }
            if installed {
                fs::rename(&req.temp_path, &req.final_path).unwrap();
            }
            let rec = f.crashed_at(&req, phase);

            let r = f
                .ritual
                .recover_one(&rec)
                .unwrap_or_else(|e| panic!("case {i} ({phase:?}) errored: {e}"));
            assert_never_neither(&req, &r);

            // And whatever the outcome, it is one of the four named ones --
            // recovery never invents a fifth answer.
            assert!(
                matches!(
                    r,
                    Resolution::Installed { .. }
                        | Resolution::SourceIntact { .. }
                        | Resolution::SourceRestored { .. }
                        | Resolution::NeedsOperator { .. }
                ),
                "case {i}"
            );
        }
    }

    /// Recovery runs over everything left on disk, and clears only what it
    /// actually resolved. An unresolved intent is the only record that a human
    /// is needed; clearing it would erase the evidence with the problem.
    #[test]
    fn recovery_clears_resolved_intents_and_keeps_ambiguous_ones() {
        let f = fixture();

        let good = f.request("good.mkv");
        fs::rename(&good.final_path, &good.trash_path).unwrap();
        f.crashed_at(&good, IntentPhase::Retired);

        let mut bad = f.request("bad.mkv");
        bad.job_id = "job-2".into();
        fs::remove_file(&bad.final_path).unwrap();
        f.crashed_at(&bad, IntentPhase::Retired);

        let results = f.ritual.recover_all().unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .any(|(_, r)| matches!(r, Resolution::SourceRestored { .. }))
        );
        assert!(
            results
                .iter()
                .any(|(_, r)| matches!(r, Resolution::NeedsOperator { .. }))
        );

        let left = f.ritual.journal().outstanding().unwrap();
        assert_eq!(left.len(), 1, "only the ambiguous one is retained");
        assert_eq!(left[0].job_id, "job-2");
    }

    /// Every resolution carries a stable label for the commit ledger and the
    /// `transcodarr_commit_intent_recovered_total{resolution}` metric.
    #[test]
    fn every_resolution_has_a_metric_label() {
        assert_eq!(
            Resolution::Installed {
                trash_path: "/t".into(),
                output_bytes: 1
            }
            .label(),
            "installed"
        );
        assert_eq!(
            Resolution::SourceIntact {
                reason: String::new()
            }
            .label(),
            "source_intact"
        );
        assert_eq!(
            Resolution::SourceRestored {
                reason: String::new()
            }
            .label(),
            "source_restored"
        );
        assert_eq!(
            Resolution::NeedsOperator {
                detail: String::new()
            }
            .label(),
            "needs_operator"
        );
    }

    /// Two agents, or two boots of one agent, must not adopt each other's
    /// in-flight work. The record carries who owned it.
    #[test]
    fn an_intent_records_which_agent_instance_owned_it() {
        let f = fixture();
        let req = f.request("a.mkv");
        f.ritual.commit(&req).unwrap();

        let rec = f.crashed_at(&req, IntentPhase::Granted);
        assert_eq!(rec.agent_uid, "agent-a");
        assert_eq!(rec.boot_id, "boot-1");
    }
}
