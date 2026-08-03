// file: crates/transcodarr-core/src/file.rs
// version: 1.0.0
// guid: 2f9c4a71-58d3-4e06-b7a2-9c1e0d5b83f4
// last-edited: 2026-08-03
//! Where a file is in the discover → probe → evaluate → process cycle.
//!
//! Deliberately separate from [`crate::job::JobState`]. Tdarr's failure mode 7
//! was dispatching off file state, which conflates "what do we know about this
//! file" with "what work is in flight for it" — the two drift the moment a job
//! fails and the file is still perfectly well understood.

use serde::{Deserialize, Serialize};

/// What is known about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FileState {
    /// Seen by a scan; nothing read from it yet.
    Discovered,
    /// A probe is in flight.
    Probing,
    /// Probed; facts are stored and current.
    Probed,
    /// The probe failed. Retried, but never silently treated as "no work".
    ProbeFailed,
    /// A policy decision has been recorded against the stored facts.
    Evaluated,
    /// Work completed and installed.
    Processed,
    /// Held back from all work until an operator intervenes.
    Quarantined,
    /// A scan expected it and it was not there.
    Missing,
}

impl FileState {
    /// The canonical spelling, which is also the value stored in SQLite.
    ///
    /// One spelling, used everywhere: the database `CHECK` constraint, the API
    /// and the logs all agree by construction rather than by convention.
    pub fn as_str(self) -> &'static str {
        match self {
            FileState::Discovered => "Discovered",
            FileState::Probing => "Probing",
            FileState::Probed => "Probed",
            FileState::ProbeFailed => "ProbeFailed",
            FileState::Evaluated => "Evaluated",
            FileState::Processed => "Processed",
            FileState::Quarantined => "Quarantined",
            FileState::Missing => "Missing",
        }
    }

    /// Parse the canonical spelling. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Discovered" => FileState::Discovered,
            "Probing" => FileState::Probing,
            "Probed" => FileState::Probed,
            "ProbeFailed" => FileState::ProbeFailed,
            "Evaluated" => FileState::Evaluated,
            "Processed" => FileState::Processed,
            "Quarantined" => FileState::Quarantined,
            "Missing" => FileState::Missing,
            _ => return None,
        })
    }

    /// Whether stored probe facts for this file are usable by the evaluator.
    ///
    /// `Processed` is included on purpose: a file that has had an audio pass
    /// installed is re-probed and re-evaluated, because the video pass may
    /// still be owed. Excluding it is how a two-stage pipeline silently stops
    /// after stage one.
    pub fn has_usable_facts(self) -> bool {
        matches!(
            self,
            FileState::Probed | FileState::Evaluated | FileState::Processed
        )
    }
}

impl std::fmt::Display for FileState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored spelling is the contract; a round trip failure would mean a
    /// row written by one version is unreadable by the next.
    #[test]
    fn every_state_round_trips_through_its_stored_spelling() {
        for s in [
            FileState::Discovered,
            FileState::Probing,
            FileState::Probed,
            FileState::ProbeFailed,
            FileState::Evaluated,
            FileState::Processed,
            FileState::Quarantined,
            FileState::Missing,
        ] {
            assert_eq!(FileState::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn an_unknown_spelling_is_rejected_rather_than_defaulted() {
        assert_eq!(FileState::parse("Probed "), None);
        assert_eq!(FileState::parse("probed"), None);
        assert_eq!(FileState::parse(""), None);
    }

    /// A processed file is re-evaluated: the audio pass being installed does
    /// not mean the video pass is not still owed.
    #[test]
    fn processed_files_remain_evaluable() {
        assert!(FileState::Processed.has_usable_facts());
        assert!(FileState::Probed.has_usable_facts());
        assert!(!FileState::Discovered.has_usable_facts());
        assert!(!FileState::ProbeFailed.has_usable_facts());
        assert!(!FileState::Quarantined.has_usable_facts());
    }
}
