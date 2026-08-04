// file: crates/transcodarr-agent/src/workarea.rs
// version: 1.0.0
// guid: 7f13c6a2-84be-4d05-9127-6a0e3b58df41
// last-edited: 2026-08-03
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

/// A staging directory guaranteed to be on the destination's filesystem.
#[derive(Debug, Clone)]
pub struct WorkArea {
    root: PathBuf,
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
        let scoped = root.join(sanitise(agent_uid)).join(sanitise(boot_id));
        fs::create_dir_all(&scoped).map_err(|e| AgentError::WorkArea {
            path: scoped.display().to_string(),
            source: e,
        })?;
        Ok(Self {
            root: scoped,
            agent_uid: agent_uid.to_string(),
            boot_id: boot_id.to_string(),
        })
    }

    /// The namespaced directory itself.
    pub fn path(&self) -> &Path {
        &self.root
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
