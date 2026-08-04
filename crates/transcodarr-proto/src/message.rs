// file: crates/transcodarr-proto/src/message.rs
// version: 1.0.0
// guid: 91b4e7d2-05a3-4c68-8f19-6d270ea35b14
// last-edited: 2026-08-03
//! Message payloads shared by both ends, and their conversions to core types.

use serde::{Deserialize, Serialize};

use crate::ProtoError;

/// How far a commit had got when it was reported.
///
/// Mirrors the agent's `IntentPhase`. The two are separate types on purpose:
/// this one is a wire value that an older or newer peer may set to something
/// unexpected, and collapsing them would let an unrecognised wire value become
/// a valid domain state by assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CommitPhase {
    /// Permission granted; nothing has moved.
    Granted,
    /// The original has been moved aside.
    Retired,
    /// The replacement is in place.
    Installed,
}

impl CommitPhase {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CommitPhase::Granted => "granted",
            CommitPhase::Retired => "retired",
            CommitPhase::Installed => "installed",
        }
    }

    /// Parse a wire spelling.
    ///
    /// Case-insensitive, because the proto enum names (`granted`) and the
    /// stored SQL values (`Granted`) differ in case and both appear in
    /// practice. Refusing one of them over capitalisation would be a protocol
    /// break for no reason.
    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "granted" => CommitPhase::Granted,
            "retired" => CommitPhase::Retired,
            "installed" => CommitPhase::Installed,
            other => {
                return Err(ProtoError::Unrecognised {
                    field: "phase",
                    value: other.to_string(),
                });
            }
        })
    }
}

/// An install the agent was in the middle of when it last stopped.
///
/// Replayed at `Register` from the fsynced `IntentJournal`, before the agent
/// accepts any work. Sending these first is what lets the server resolve an
/// interrupted commit against its own ledger rather than discovering the
/// conflict when a second agent is handed the same file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveIntent {
    /// Which job.
    pub job_id: String,
    /// Which attempt.
    pub attempt: i64,
    /// The epoch under which it was granted.
    pub fencing_epoch: i64,
    /// How far it got.
    pub phase: CommitPhase,
    /// Where the staged output is.
    pub temp_path: String,
    /// Where it was going.
    pub final_path: String,
    /// Where the original was moved to.
    pub trash_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_round_trip_through_their_wire_spelling() {
        for p in [
            CommitPhase::Granted,
            CommitPhase::Retired,
            CommitPhase::Installed,
        ] {
            assert_eq!(CommitPhase::parse(p.as_str()).unwrap(), p);
        }
    }

    /// The proto enum names are lowercase and the stored SQL values are
    /// capitalised; both appear in practice, and refusing one over
    /// capitalisation would be a protocol break for no reason.
    #[test]
    fn parsing_accepts_either_capitalisation() {
        assert_eq!(CommitPhase::parse("Retired").unwrap(), CommitPhase::Retired);
        assert_eq!(CommitPhase::parse("retired").unwrap(), CommitPhase::Retired);
        assert_eq!(CommitPhase::parse("RETIRED").unwrap(), CommitPhase::Retired);
    }

    /// A value from a newer peer is refused, not guessed at. Guessing would let
    /// an unknown commit state become a known one by assignment.
    #[test]
    fn an_unrecognised_phase_is_refused() {
        let e = CommitPhase::parse("teleported").unwrap_err();
        assert!(matches!(e, ProtoError::Unrecognised { field: "phase", .. }));
    }

    #[test]
    fn a_live_intent_survives_serialisation() {
        let i = LiveIntent {
            job_id: "j1".into(),
            attempt: 2,
            fencing_epoch: 7,
            phase: CommitPhase::Retired,
            temp_path: "/w/j1.partial.mkv".into(),
            final_path: "/mnt/tv/a.mkv".into(),
            trash_path: "/t/a.mkv".into(),
        };
        let back: LiveIntent = serde_json::from_str(&serde_json::to_string(&i).unwrap()).unwrap();
        assert_eq!(back, i);
    }
}
