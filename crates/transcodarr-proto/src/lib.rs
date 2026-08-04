// file: crates/transcodarr-proto/src/lib.rs
// version: 1.0.0
// guid: 7c3d90ab-1e58-42f6-b704-8a15de26903c
// last-edited: 2026-08-03
#![deny(unsafe_code)]
#![warn(missing_docs)]
//! The agent protocol: message shapes, the version gate, and conversions.
//!
//! **Codegen is deliberately not wired up yet.** `proto/transcodarr/v1/agent.proto`
//! is the checked-in wire contract, and the types here mirror it — but adding
//! `tonic-build` means adding `protoc` to CI, which is a build-environment
//! change that belongs with the commit that actually needs a transport. Until
//! then the interesting half of this crate is available and testable: the
//! version negotiation, the fencing rule, and the conversions to and from core
//! types.
//!
//! The two rules encoded here are the ones a wire format cannot enforce on its
//! own:
//!
//! - **The version gate is checked at `Register`, not at first use.** An agent
//!   too old to be trusted must be turned away while it is still asking
//!   permission, not discovered halfway through a commit.
//! - **`FencingEpoch` bumps only on a new process instance.** A stream
//!   reconnect resumes the existing epoch (flaw C9). Bumping on reconnect makes
//!   every network blip invalidate work that is still running perfectly well.

pub mod handshake;
pub mod message;

pub use handshake::{AgentIdentity, RegisterOutcome, VersionGate};
pub use message::{CommitPhase, LiveIntent};

use thiserror::Error;

/// The protocol version this build speaks.
pub const PROTO_VERSION: u32 = 1;

/// The oldest protocol version this build will accept from an agent.
pub const MIN_SUPPORTED_PROTO: u32 = 1;

/// Anything that can go wrong interpreting a peer.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtoError {
    /// A field carried a value this build does not understand.
    #[error("{field} holds {value}, which this build does not recognise")]
    Unrecognised {
        /// Which field.
        field: &'static str,
        /// What it held.
        value: String,
    },

    /// A required field was absent.
    #[error("{0} is required but was not set")]
    Missing(&'static str),
}
