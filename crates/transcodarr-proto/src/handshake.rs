// file: crates/transcodarr-proto/src/handshake.rs
// version: 1.0.0
// guid: 3e08c951-7a26-4bd4-90e7-2f8c14a6b075
// last-edited: 2026-08-03
//! Registration: who an agent is, and whether it may join.
//!
//! Two rules live here that no wire format can enforce on its own.
//!
//! **The version gate is checked at `Register`.** An agent too old to be
//! trusted is turned away while it is still asking permission — not discovered
//! halfway through a commit, where the only options left are bad ones.
//!
//! **`FencingEpoch` bumps only on a new process instance.** A stream reconnect
//! resumes the existing epoch (flaw C9). This is the subtle one: bumping on
//! reconnect looks safer, and is the opposite. Every network blip would
//! invalidate work that is still running perfectly well, and the agent would
//! come back to find its own in-flight job fenced off.

use serde::{Deserialize, Serialize};

/// Who an agent is, across three different lifetimes.
///
/// The three identifiers are not redundant — each answers a different question,
/// and collapsing any two of them breaks a guarantee:
///
/// - `agent_id` is operator-assigned and stable forever (`u1`, `win-rtx2070`).
///   It is what a human types.
/// - `agent_uid` is per *installation*. Reinstalling on the same host produces
///   a new one, which is what stops a fresh install adopting the old one's
///   work area.
/// - `boot_id` is per *process instance*, and is the only thing that bumps the
///   fencing epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
    /// Operator-assigned, stable.
    pub agent_id: String,
    /// Per installation.
    pub agent_uid: String,
    /// Per process instance.
    pub boot_id: String,
    /// Semver, checked against `min_agent_version`.
    pub agent_version: String,
    /// Protocol version this agent speaks.
    pub proto_version: u32,
}

/// What the server decided about a registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Accepted, with the epoch the agent must use on every commit.
    Accepted {
        /// Authoritative fencing epoch.
        fencing_epoch: i64,
        /// Whether this is a new process instance rather than a reconnect.
        new_instance: bool,
    },
    /// Refused, with a reason an operator can act on.
    Rejected {
        /// Why, in words.
        reason: String,
    },
}

impl RegisterOutcome {
    /// Whether the agent may proceed.
    pub fn accepted(&self) -> bool {
        matches!(self, RegisterOutcome::Accepted { .. })
    }
}

/// Decides whether an agent may join, and under which epoch.
#[derive(Debug, Clone)]
pub struct VersionGate {
    /// Oldest protocol version accepted.
    pub min_supported_proto: u32,
    /// Newest this build speaks.
    pub server_proto: u32,
    /// Oldest agent build accepted, as a semver `major.minor.patch`.
    ///
    /// Enforced at registration rather than tolerated, because an agent old
    /// enough to be missing a safety fix should never take work at all — the
    /// alternative is finding out which fix it was missing afterwards.
    pub min_agent_version: (u32, u32, u32),
}

impl Default for VersionGate {
    fn default() -> Self {
        Self {
            min_supported_proto: crate::MIN_SUPPORTED_PROTO,
            server_proto: crate::PROTO_VERSION,
            min_agent_version: (0, 1, 0),
        }
    }
}

impl VersionGate {
    /// Judge a registration.
    ///
    /// `known_boot_id` is the `boot_id` the server last saw for this
    /// `agent_uid`, and `current_epoch` the epoch it is currently holding.
    /// Together they answer the only question that matters here: is this the
    /// same process coming back, or a new one?
    pub fn evaluate(
        &self,
        identity: &AgentIdentity,
        known_boot_id: Option<&str>,
        current_epoch: i64,
    ) -> RegisterOutcome {
        if identity.proto_version < self.min_supported_proto {
            return RegisterOutcome::Rejected {
                reason: format!(
                    "proto_version {} < min_supported {}; upgrade the agent",
                    identity.proto_version, self.min_supported_proto
                ),
            };
        }
        // A newer agent is refused too, and deliberately. It may send messages
        // this build cannot interpret, and a server guessing at a field it does
        // not understand is how a fencing epoch gets ignored.
        if identity.proto_version > self.server_proto {
            return RegisterOutcome::Rejected {
                reason: format!(
                    "proto_version {} > server {}; upgrade the server",
                    identity.proto_version, self.server_proto
                ),
            };
        }

        match parse_semver(&identity.agent_version) {
            None => {
                return RegisterOutcome::Rejected {
                    reason: format!(
                        "agent_version '{}' is not a semver; refusing rather than \
                         assuming it is new enough",
                        identity.agent_version
                    ),
                };
            }
            Some(v) if v < self.min_agent_version => {
                let (a, b, c) = self.min_agent_version;
                return RegisterOutcome::Rejected {
                    reason: format!("agent_version {} < min {a}.{b}.{c}", identity.agent_version),
                };
            }
            Some(_) => {}
        }

        // The fencing rule. A new boot_id is a new process, and only that bumps
        // the epoch. A reconnect resumes: bumping there would invalidate work
        // still running, and the agent would return to find its own in-flight
        // job fenced off.
        let new_instance = known_boot_id != Some(identity.boot_id.as_str());
        RegisterOutcome::Accepted {
            fencing_epoch: if new_instance {
                current_epoch + 1
            } else {
                current_epoch
            },
            new_instance,
        }
    }
}

/// Parse `major.minor.patch`, ignoring any pre-release or build suffix.
fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(proto: u32, version: &str, boot: &str) -> AgentIdentity {
        AgentIdentity {
            agent_id: "u1".into(),
            agent_uid: "uid-1".into(),
            boot_id: boot.into(),
            agent_version: version.into(),
            proto_version: proto,
        }
    }

    fn gate() -> VersionGate {
        VersionGate {
            min_supported_proto: 1,
            server_proto: 2,
            min_agent_version: (1, 2, 0),
        }
    }

    #[test]
    fn a_current_agent_is_accepted() {
        let out = gate().evaluate(&identity(2, "1.3.0", "boot-a"), None, 0);
        assert!(out.accepted(), "{out:?}");
    }

    /// The gate is at registration, not at first use. An agent missing a safety
    /// fix must never take work at all.
    #[test]
    fn an_agent_below_the_minimum_version_is_turned_away_at_registration() {
        let out = gate().evaluate(&identity(2, "1.1.9", "boot-a"), None, 0);
        match out {
            RegisterOutcome::Rejected { reason } => assert!(reason.contains("min 1.2.0")),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn an_agent_speaking_too_old_a_protocol_is_refused() {
        let g = VersionGate {
            min_supported_proto: 2,
            ..gate()
        };
        let out = g.evaluate(&identity(1, "9.9.9", "boot-a"), None, 0);
        assert!(!out.accepted());
    }

    /// A newer agent is refused too. It may send fields this build cannot
    /// interpret, and a server guessing at one is how a fencing epoch gets
    /// silently ignored.
    #[test]
    fn an_agent_newer_than_the_server_is_also_refused() {
        let out = gate().evaluate(&identity(99, "1.3.0", "boot-a"), None, 0);
        match out {
            RegisterOutcome::Rejected { reason } => assert!(reason.contains("upgrade the server")),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    /// An unparseable version is refused rather than assumed new enough --
    /// assuming would make the gate trivially bypassable by sending garbage.
    #[test]
    fn an_unparseable_agent_version_is_refused() {
        for v in ["", "latest", "1.2.3.4", "v1.2.3"] {
            let out = gate().evaluate(&identity(2, v, "boot-a"), None, 0);
            assert!(!out.accepted(), "version '{v}' should be refused");
        }
    }

    #[test]
    fn a_prerelease_suffix_does_not_defeat_the_gate() {
        let out = gate().evaluate(&identity(2, "1.3.0-rc1", "boot-a"), None, 0);
        assert!(out.accepted(), "{out:?}");
    }

    /// A new process instance bumps the epoch, so anything the previous
    /// instance was doing can no longer commit.
    #[test]
    fn a_new_process_instance_bumps_the_fencing_epoch() {
        let out = gate().evaluate(&identity(2, "1.3.0", "boot-b"), Some("boot-a"), 7);
        match out {
            RegisterOutcome::Accepted {
                fencing_epoch,
                new_instance,
            } => {
                assert_eq!(fencing_epoch, 8);
                assert!(new_instance);
            }
            other => panic!("expected acceptance, got {other:?}"),
        }
    }

    /// The subtle one. Bumping on reconnect looks safer and is the opposite:
    /// every network blip would invalidate work still running, and the agent
    /// would come back to find its own in-flight job fenced off.
    #[test]
    fn a_stream_reconnect_resumes_the_existing_epoch() {
        let out = gate().evaluate(&identity(2, "1.3.0", "boot-a"), Some("boot-a"), 7);
        match out {
            RegisterOutcome::Accepted {
                fencing_epoch,
                new_instance,
            } => {
                assert_eq!(fencing_epoch, 7, "a reconnect must not fence its own work");
                assert!(!new_instance);
            }
            other => panic!("expected acceptance, got {other:?}"),
        }
    }

    /// A first-ever registration has no known boot id, so it is a new instance.
    #[test]
    fn a_first_registration_is_a_new_instance() {
        let out = gate().evaluate(&identity(2, "1.3.0", "boot-a"), None, 0);
        assert_eq!(
            out,
            RegisterOutcome::Accepted {
                fencing_epoch: 1,
                new_instance: true
            }
        );
    }

    /// The epoch is monotonic across restarts, so an old instance's grant can
    /// never become valid again.
    #[test]
    fn the_epoch_never_goes_backwards_across_restarts() {
        let g = gate();
        let mut epoch = 0;
        for (i, boot) in ["b1", "b2", "b3"].iter().enumerate() {
            let prev = if i == 0 { None } else { Some("older") };
            match g.evaluate(&identity(2, "1.3.0", boot), prev, epoch) {
                RegisterOutcome::Accepted { fencing_epoch, .. } => {
                    assert!(fencing_epoch > epoch);
                    epoch = fencing_epoch;
                }
                other => panic!("{other:?}"),
            }
        }
    }
}
