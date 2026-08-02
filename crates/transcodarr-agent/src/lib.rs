// file: crates/transcodarr-agent/src/lib.rs
// version: 1.0.0
// guid: b2947c0e-5d81-4f36-a7b0-6e13df852a94
// last-edited: 2026-08-02
#![deny(unsafe_code)]
#![warn(missing_docs)]
//! transcodarr worker agent.
//!
//! Depends on `transcodarr-core` and nothing else internal — never on
//! `transcodarr-store`. An agent must be copyable to the Windows node without
//! dragging SQLite along with it.

pub mod preflight;
