// file: crates/transcodarr-agent/src/identity.rs
// version: 1.0.0
// guid: 6ba2e947-1c05-4d38-8f62-7e05b1a34cd9
// last-edited: 2026-08-05
//! Who this agent is, and which run of it this is.
//!
//! Three identifiers, and the distinction between them is the fencing rule:
//!
//! - `agent_id` is operator-assigned and stable (`u1`, `win-rtx2070`). It is
//!   configuration, so it is not derived here.
//! - `agent_uid` is per *installation*. A reinstall under the same name is a
//!   different agent, and must not inherit a work area that is not its own.
//! - `boot_id` is per *process instance*, and is the only thing that bumps
//!   `fencing_epoch`. A stream reconnect resumes the epoch it already had.
//!
//! ## Why `boot_id` is not the kernel's
//!
//! `/proc/sys/kernel/random/boot_id` is tempting and wrong. It changes when the
//! *machine* reboots, not when the agent process restarts — so an agent that
//! crashed and came back would present the identifier of the instance that
//! crashed, the server would resume its epoch, and nothing would be fenced.
//! That is precisely the case the fence exists for: the new process cannot know
//! what its predecessor had in flight, and a survivor of the old one may still
//! be writing.
//!
//! It also fails differently by platform. The read succeeds on Linux and fails
//! on macOS and Windows, so the same code fenced correctly on a laptop and not
//! on the machine that matters.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// This process's `boot_id`, generated once and reused for the process's life.
///
/// **Reused across reconnects on purpose.** Generating a fresh one per
/// connection attempt would turn every network blip into an epoch bump, fencing
/// work that is running perfectly well behind a transient fault.
pub fn boot_id() -> &'static str {
    static BOOT_ID: OnceLock<String> = OnceLock::new();
    BOOT_ID.get_or_init(|| {
        // pid and start time together: pid alone is reused by the OS, and a
        // timestamp alone collides between two agents started in the same
        // nanosecond on one host. Neither is a UUID and neither needs to be —
        // this only has to distinguish one process from its own predecessor.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{:x}-{:x}", std::process::id(), nanos)
    })
}

/// This installation's `agent_uid`.
///
/// Taken from `TRANSCODARR_AGENT_UID` when set, so an operator can pin it, and
/// otherwise from the hostname. This identifies an installation, not a person:
/// it namespaces a work area and it does not authenticate anything.
pub fn agent_uid() -> String {
    std::env::var("TRANSCODARR_AGENT_UID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(hostname)
        .unwrap_or_else(|| "local".to_string())
}

/// The machine's hostname, if it can be read without a syscall crate.
fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the `OnceLock`: a reconnect must present the same
    /// identifier, or the server bumps the epoch and fences live work.
    #[test]
    fn the_boot_id_is_stable_within_a_process() {
        assert_eq!(boot_id(), boot_id());
        assert!(!boot_id().is_empty());
    }

    /// It must also not be the kernel's, which survives a process restart. If
    /// this ever starts matching, the fence has quietly stopped working.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_boot_id_is_not_the_kernels() {
        if let Ok(kernel) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
            assert_ne!(boot_id(), kernel.trim());
        }
    }

    #[test]
    fn an_agent_uid_is_never_empty() {
        assert!(!agent_uid().is_empty());
    }
}
