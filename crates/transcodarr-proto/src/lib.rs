// file: crates/transcodarr-proto/src/lib.rs
// version: 1.1.0
// guid: 7c3d90ab-1e58-42f6-b704-8a15de26903c
// last-edited: 2026-08-16
#![deny(unsafe_code)]
#![warn(missing_docs)]
//! The agent protocol: message shapes, the version gate, and conversions.
//!
//! `proto/transcodarr/v1/agent.proto` is the wire contract; [`pb`] is what
//! `tonic-build` makes of it, and [`convert`] is the boundary between those
//! generated types and the domain types in `transcodarr-core`.
//!
//! **The generated types are not used as domain types anywhere.** Every value
//! that crosses the boundary is converted, and every conversion of an enum-like
//! field can fail. A proto3 enum decodes an unknown number to its zero variant
//! rather than to an error, so a peer sending a `DecoderStatus` this build has
//! never heard of would arrive as `DS_UNTESTED` — and untested is a *claim*,
//! not the absence of one. Refusing at the boundary is what keeps a value
//! invented by a newer peer from becoming a domain fact by default.
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

pub mod convert;
pub mod handshake;
pub mod message;
pub mod transfer;

/// The generated types, exactly as `tonic-build` emits them.
///
/// Kept in its own module so the lints this crate holds itself to do not apply
/// to code nobody wrote. `missing_docs` in particular would otherwise force
/// doc comments onto several hundred generated fields, and the noise would bury
/// the documentation on the types that are hand-written.
pub mod pb {
    #![allow(missing_docs)]
    #![allow(clippy::all, clippy::pedantic, clippy::nursery)]

    tonic::include_proto!("transcodarr.v1");
}

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
