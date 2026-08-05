// file: crates/transcodarr-agent/src/workarea.rs
// version: 1.1.0
// guid: 7f13c6a2-84be-4d05-9127-6a0e3b58df41
// last-edited: 2026-08-05
//! Where an agent stages output before installing it.
//!
//! Two properties decide the whole design, and both are non-negotiable:
//!
//! - **Same filesystem as the destination.** `rename(2)` is atomic only within
//!   one filesystem. Across a boundary it is `EXDEV`, and the fallback everyone
//!   reaches for — copy then delete — has a window in which neither the source
//!   nor a complete replacement exists. A crash inside that window loses the
//!   file. So a cross-device work area is refused outright rather than papered
//!   over.
//! - **Namespaced per agent instance.** Two agents sharing a work directory
//!   will eventually pick the same temporary name, and the second one to write
//!   silently corrupts the first one's output. The namespace is
//!   `agent_uid`/`boot_id`, so a restarted agent cannot collide with the
//!   leftovers of its own previous life either.
//!
//! **The intent journal is deliberately *outside* that namespace.** Staged
//! output belongs to one process instance and is worthless to the next one;
//! the journal is the opposite. It exists to be read by whoever comes after a
//! crash, and a `boot_id` that changes on restart would hand the successor an
//! empty directory — recovery finding nothing, every time, in exactly the case
//! it was built for. So it lives at `agent_uid`/`.journal`, stable for the
//! life of the installation. See [`WorkArea::open_journal`].
//!
//! This is the concrete form of design decision D14: the work area is colocated
//! on the destination pool, paying roughly double the pool I/O for the encode,
//! in exchange for an install that is a single atomic rename. The alternative —
//! staging on fast local scratch and copying across at the end — moves the same
//! bytes anyway and buys a non-atomic install. The I/O is not saved, only
//! deferred to the least recoverable moment.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::AgentError;
use crate::journal::IntentJournal;

/// The journal directory's name under the installation root.
///
/// Leading dot on purpose: [`sanitise`] maps every character that is not
/// alphanumeric, `-` or `_` to `_`, so no `boot_id` can ever sanitise to this
/// name. Without that guarantee an agent booting as `journal` would place its
/// staged output in the directory holding the records of what it was about to
/// do.
const JOURNAL_DIR: &str = ".journal";

/// A staging directory guaranteed to be on the destination's filesystem.
#[derive(Debug, Clone)]
pub struct WorkArea {
    root: PathBuf,
    install_root: PathBuf,
    agent_uid: String,
    boot_id: String,
}

impl WorkArea {
    /// Prepare a work area under `root` for the given agent instance.
    ///
    /// `root` is created if missing. The caller is expected to have taken it
    /// from `library.work_dir`, which the operator sets on the same pool as the
    /// library — [`WorkArea::ensure_same_device`] is what verifies they did.
    pub fn open(root: &Path, agent_uid: &str, boot_id: &str) -> Result<Self, AgentError> {
        let install_root = root.join(sanitise(agent_uid));
        let scoped = install_root.join(sanitise(boot_id));
        fs::create_dir_all(&scoped).map_err(|e| AgentError::WorkArea {
            path: scoped.display().to_string(),
            source: e,
        })?;
        Ok(Self {
            root: scoped,
            install_root,
            agent_uid: agent_uid.to_string(),
            boot_id: boot_id.to_string(),
        })
    }

    /// The namespaced directory itself.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Where this installation's intent journal lives.
    ///
    /// One level above [`WorkArea::path`], and stable across restarts. See the
    /// module documentation for why it must not be namespaced by `boot_id`.
    pub fn journal_dir(&self) -> PathBuf {
        self.install_root.join(JOURNAL_DIR)
    }

    /// Open this installation's journal, adopting anything an earlier build
    /// left in a per-boot directory.
    ///
    /// The sweep exists because the journal used to live at
    /// `agent_uid`/`boot_id`/`journal`. Records written there describe installs
    /// that were in flight — a `Retired` one means the original is sitting in
    /// the trash and the destination may be empty. Leaving them behind because
    /// the path moved would orphan exactly the records recovery is for, so they
    /// are moved into the stable directory rather than abandoned.
    ///
    /// A record that already exists under the stable name is left alone: the
    /// current location wins over an older copy of the same job.
    pub fn open_journal(&self) -> Result<IntentJournal, AgentError> {
        let stable = self.journal_dir();
        let journal = IntentJournal::open(&stable)?;

        let entries = match fs::read_dir(&self.install_root) {
            Ok(e) => e,
            // Nothing to sweep if the installation root is unreadable; the
            // journal itself opened, which is what the caller needs.
            Err(_) => return Ok(journal),
        };

        for entry in entries.flatten() {
            let legacy = entry.path().join("journal");
            if entry.path() == stable || !legacy.is_dir() {
                continue;
            }
            adopt_legacy_journal(&legacy, &stable)?;
        }

        Ok(journal)
    }

    /// Which agent instance owns this area.
    pub fn agent_uid(&self) -> &str {
        &self.agent_uid
    }

    /// Which boot of that agent.
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// Refuse unless this work area and `final_path` share a filesystem.
    ///
    /// Checked against the destination's *parent directory*, because the
    /// destination itself may not exist yet — and checking the file would then
    /// silently fall back to checking nothing.
    pub fn ensure_same_device(&self, final_path: &Path) -> Result<(), AgentError> {
        let dest_dir = final_path.parent().unwrap_or(Path::new("/"));
        let ours = device_of(&self.root)?;
        let theirs = device_of(dest_dir)?;
        if ours != theirs {
            return Err(AgentError::CrossDeviceWorkArea {
                work_area: self.root.display().to_string(),
                destination: dest_dir.display().to_string(),
            });
        }
        Ok(())
    }

    /// A temporary path for one job attempt.
    ///
    /// The extension is preserved because ffmpeg picks its muxer from it: a
    /// temporary file named `.tmp` would be muxed as whatever ffmpeg guesses,
    /// which is not necessarily what the plan asked for.
    pub fn temp_path(&self, job_id: &str, attempt: i64, final_path: &Path) -> PathBuf {
        let ext = final_path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_else(|| "mkv".to_string());
        self.root
            .join(format!("{}.{attempt}.partial.{ext}", sanitise(job_id)))
    }

    /// Remove anything left behind by a previous attempt of this job.
    ///
    /// Leftovers are stale by definition: a temporary file that survived means
    /// the attempt that wrote it did not install it.
    pub fn clear(&self, job_id: &str, attempt: i64, final_path: &Path) -> Result<(), AgentError> {
        let p = self.temp_path(job_id, attempt, final_path);
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AgentError::WorkArea {
                path: p.display().to_string(),
                source: e,
            }),
        }
    }
}

/// The device number a path lives on.
#[cfg(unix)]
pub fn device_of(path: &Path) -> Result<u64, AgentError> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).map_err(|e| AgentError::WorkArea {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(meta.dev())
}

/// On a platform without device numbers the check cannot be performed.
///
/// Returning a constant would make [`WorkArea::ensure_same_device`] pass
/// unconditionally, which is worse than useless: it would report that a
/// cross-device install is safe. The Windows node is produce-only for exactly
/// this class of reason, decided by the Phase 0 `RenameProbe`.
#[cfg(not(unix))]
pub fn device_of(path: &Path) -> Result<u64, AgentError> {
    let _ = fs::metadata(path).map_err(|e| AgentError::WorkArea {
        path: path.display().to_string(),
        source: e,
    })?;
    Err(AgentError::DeviceUnknowable {
        path: path.display().to_string(),
    })
}

/// Move every journal record out of a per-boot directory into the stable one.
///
/// The move is a `rename(2)` within one filesystem, so a record is either at
/// the old name or the new one and never at neither. The source directory is
/// removed only if it ends up empty — a leftover that could not be adopted is
/// left where it is rather than deleted, because an install record is the only
/// evidence that an install was in progress.
fn adopt_legacy_journal(legacy: &Path, stable: &Path) -> Result<(), AgentError> {
    let entries = fs::read_dir(legacy).map_err(|e| AgentError::Journal {
        path: legacy.display().to_string(),
        source: e,
    })?;

    for entry in entries.flatten() {
        let from = entry.path();
        let Some(name) = from.file_name() else {
            continue;
        };
        if from.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let to = stable.join(name);
        if to.exists() {
            tracing_note(&from, &to);
            continue;
        }
        fs::rename(&from, &to).map_err(|e| AgentError::Journal {
            path: from.display().to_string(),
            source: e,
        })?;
    }

    crate::journal::sync_dir(stable).map_err(|e| AgentError::Journal {
        path: stable.display().to_string(),
        source: e,
    })?;
    // Best-effort: a directory that still holds something stays.
    let _ = fs::remove_dir(legacy);
    Ok(())
}

/// Record that an adopted name was already taken, without pulling `tracing`
/// into this crate.
///
/// The agent stays dependency-light so it remains copyable to the Windows node;
/// this is the only place that wants a log line, and stderr is enough for a
/// condition an operator will see once in the life of an installation.
fn tracing_note(from: &Path, to: &Path) {
    eprintln!(
        "transcodarr-agent: leaving {} in place; {} already exists",
        from.display(),
        to.display()
    );
}

/// Make a string safe to use as one path component.
///
/// An agent id or job id arriving with a slash or `..` in it must not be able
/// to place a temporary file outside the work area.
fn sanitise(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn the_work_area_is_namespaced_by_agent_and_boot() {
        let d = TempDir::new().unwrap();
        let a = WorkArea::open(d.path(), "agent-1", "boot-a").unwrap();
        let b = WorkArea::open(d.path(), "agent-1", "boot-b").unwrap();
        let c = WorkArea::open(d.path(), "agent-2", "boot-a").unwrap();
        assert_ne!(a.path(), b.path(), "a restart must not reuse its own area");
        assert_ne!(a.path(), c.path(), "two agents must not share an area");
        assert!(a.path().is_dir());
    }

    /// A job id carrying a slash or `..` must not escape the work area — it
    /// arrives from the server, and a path traversal here writes anywhere the
    /// agent can reach.
    #[test]
    fn identifiers_cannot_escape_the_work_area() {
        let d = TempDir::new().unwrap();
        let w = WorkArea::open(d.path(), "../../etc", "boot/../..").unwrap();
        assert!(w.path().starts_with(d.path()), "{:?}", w.path());

        let temp = w.temp_path("../../../etc/passwd", 0, Path::new("/mnt/tv/a.mkv"));
        assert!(temp.starts_with(d.path()), "{temp:?}");
        assert!(!temp.to_string_lossy().contains(".."));
    }

    /// ffmpeg picks its muxer from the extension. A temporary file named
    /// `.tmp` would be muxed as whatever ffmpeg guesses.
    #[test]
    fn the_temporary_path_keeps_the_destination_extension() {
        let d = TempDir::new().unwrap();
        let w = WorkArea::open(d.path(), "a", "b").unwrap();
        let t = w.temp_path("job1", 2, Path::new("/mnt/tv/show.mkv"));
        assert_eq!(t.extension().unwrap(), "mkv");
        assert!(t.to_string_lossy().contains("job1"));
        assert!(t.to_string_lossy().contains(".2."));
    }

    #[test]
    fn two_attempts_of_one_job_do_not_share_a_temporary_path() {
        let d = TempDir::new().unwrap();
        let w = WorkArea::open(d.path(), "a", "b").unwrap();
        assert_ne!(
            w.temp_path("job1", 0, Path::new("/x/a.mkv")),
            w.temp_path("job1", 1, Path::new("/x/a.mkv"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_work_area_on_the_destination_filesystem_is_accepted() {
        let d = TempDir::new().unwrap();
        let lib = d.path().join("lib");
        fs::create_dir_all(&lib).unwrap();
        let w = WorkArea::open(&d.path().join("work"), "a", "b").unwrap();
        w.ensure_same_device(&lib.join("out.mkv")).unwrap();
    }

    /// `rename(2)` is atomic only within a filesystem. Across one it is EXDEV,
    /// and the copy-then-delete fallback has a window where neither the source
    /// nor a complete replacement exists.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_cross_device_work_area_is_refused() {
        // /dev/shm is a different filesystem from a temp dir on the root fs on
        // every Linux box this will run on. Skipped rather than failed if not.
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() {
            return;
        }
        let d = TempDir::new().unwrap();
        if device_of(shm).unwrap() == device_of(d.path()).unwrap() {
            return;
        }
        let w = WorkArea::open(&shm.join("transcodarr-test"), "a", "b").unwrap();
        let err = w.ensure_same_device(&d.path().join("out.mkv")).unwrap_err();
        assert!(
            matches!(err, AgentError::CrossDeviceWorkArea { .. }),
            "{err:?}"
        );
        let _ = fs::remove_dir_all(shm.join("transcodarr-test"));
    }

    #[test]
    fn clearing_a_missing_leftover_is_not_an_error() {
        let d = TempDir::new().unwrap();
        let w = WorkArea::open(d.path(), "a", "b").unwrap();
        w.clear("job1", 0, Path::new("/x/a.mkv")).unwrap();
    }

    use crate::journal::{IntentPhase, IntentRecord};

    fn record(job_id: &str) -> IntentRecord {
        IntentRecord {
            job_id: job_id.into(),
            attempt: 0,
            agent_uid: "agent-1".into(),
            boot_id: "boot-a".into(),
            fencing_epoch: 1,
            temp_path: "/w/j.partial.mkv".into(),
            final_path: "/mnt/tv/a.mkv".into(),
            trash_path: "/t/a.mkv".into(),
            expected_content_sig: "sig".into(),
            phase: IntentPhase::Retired,
        }
    }

    /// The reason the journal is not namespaced by `boot_id`. A restart takes a
    /// new one, and a journal that moved with it would hand the successor an
    /// empty directory — recovery finding nothing, every time, in exactly the
    /// case it exists for.
    #[test]
    fn the_journal_survives_a_restart() {
        let d = TempDir::new().unwrap();
        let before = WorkArea::open(d.path(), "agent-1", "boot-a").unwrap();
        before
            .open_journal()
            .unwrap()
            .record(&record("job-1"))
            .unwrap();

        let after = WorkArea::open(d.path(), "agent-1", "boot-b").unwrap();
        assert_ne!(before.path(), after.path(), "a restart is a new work area");
        assert_eq!(
            before.journal_dir(),
            after.journal_dir(),
            "but not a new journal"
        );

        let outstanding = after.open_journal().unwrap().outstanding().unwrap();
        assert_eq!(outstanding.len(), 1, "the successor must see the record");
        assert_eq!(outstanding[0].job_id, "job-1");
    }

    /// Two installations still keep their journals apart: the record says which
    /// `agent_uid` owns the intent, and adopting another installation's would
    /// mean acting on a work area that is not ours.
    #[test]
    fn two_installations_do_not_share_a_journal() {
        let d = TempDir::new().unwrap();
        let a = WorkArea::open(d.path(), "agent-1", "boot-a").unwrap();
        let b = WorkArea::open(d.path(), "agent-2", "boot-a").unwrap();
        a.open_journal().unwrap().record(&record("job-1")).unwrap();
        assert_ne!(a.journal_dir(), b.journal_dir());
        assert!(b.open_journal().unwrap().outstanding().unwrap().is_empty());
    }

    /// Records written by the build that kept the journal under the boot
    /// directory describe installs that were in flight. A `Retired` one means
    /// the original is in the trash and the destination may be empty, so they
    /// are adopted rather than orphaned by the path change.
    #[test]
    fn a_journal_left_by_an_older_build_is_adopted() {
        let d = TempDir::new().unwrap();
        let old = WorkArea::open(d.path(), "agent-1", "boot-old").unwrap();

        // Exactly where the previous build put it.
        let legacy = old.path().join("journal");
        IntentJournal::open(&legacy)
            .unwrap()
            .record(&record("job-old"))
            .unwrap();

        let now = WorkArea::open(d.path(), "agent-1", "boot-new").unwrap();
        let outstanding = now.open_journal().unwrap().outstanding().unwrap();
        assert_eq!(outstanding.len(), 1, "the old record must not be orphaned");
        assert_eq!(outstanding[0].job_id, "job-old");
        assert!(!legacy.exists(), "the emptied directory should be gone");
    }

    /// The current record wins. An older copy of the same job is left where it
    /// is rather than overwriting what the live journal says.
    #[test]
    fn adopting_never_overwrites_a_current_record() {
        let d = TempDir::new().unwrap();
        let w = WorkArea::open(d.path(), "agent-1", "boot-new").unwrap();

        let mut current = record("job-1");
        current.phase = IntentPhase::Installed;
        w.open_journal().unwrap().record(&current).unwrap();

        let old = WorkArea::open(d.path(), "agent-1", "boot-old").unwrap();
        IntentJournal::open(&old.path().join("journal"))
            .unwrap()
            .record(&record("job-1"))
            .unwrap();

        let outstanding = w.open_journal().unwrap().outstanding().unwrap();
        assert_eq!(outstanding.len(), 1);
        assert_eq!(
            outstanding[0].phase,
            IntentPhase::Installed,
            "the live record must not be replaced by the stale one"
        );
    }

    /// No `boot_id` may sanitise to the journal's name, or an agent booting
    /// under it would stage output into the directory recording what it was
    /// about to do.
    #[test]
    fn no_boot_id_can_collide_with_the_journal_directory() {
        let d = TempDir::new().unwrap();
        for boot in ["journal", ".journal", "..journal", "-journal"] {
            let w = WorkArea::open(d.path(), "agent-1", boot).unwrap();
            assert_ne!(w.path(), w.journal_dir(), "boot_id {boot}");
        }
    }

    /// A temporary file that survived means the attempt that wrote it did not
    /// install it, so it is stale by definition.
    #[test]
    fn clearing_removes_a_leftover() {
        let d = TempDir::new().unwrap();
        let w = WorkArea::open(d.path(), "a", "b").unwrap();
        let p = w.temp_path("job1", 0, Path::new("/x/a.mkv"));
        fs::write(&p, b"leftover").unwrap();
        assert!(p.exists());
        w.clear("job1", 0, Path::new("/x/a.mkv")).unwrap();
        assert!(!p.exists());
    }
}
