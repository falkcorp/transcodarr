// file: crates/transcodarr-core/src/failure.rs
// version: 1.0.0
// guid: d05a7b31-49e6-4c28-8f37-b1e6902c4d85
// last-edited: 2026-08-01
//! Classifying why a job failed.
//!
//! The class matters more than the code, because it decides what happens next.
//! Conflating "this node cannot ever do this" with "this node is momentarily
//! busy" is what turns a single GPU node plus a denylist into a dead-lettered
//! queue: one transient NVENC session exhaustion excludes the only capable
//! agent, and every subsequent job fails for want of a node.
//!
//! So: a transient failure **never** excludes an agent and **never**
//! dead-letters. Capability drift does exclude, and is alarmed, because it means
//! the server's model of the fleet is wrong.

use serde::{Deserialize, Serialize};

/// What kind of failure this was, which decides the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FailureClass {
    /// Temporary. Retry on any agent, including this one. Never excludes,
    /// never dead-letters.
    Transient,
    /// The agent cannot do what it advertised. The server's capability model is
    /// wrong — exclude this agent for this requirement and raise an alarm.
    CapabilityDrift,
    /// The job itself is bad. Retrying anywhere will fail the same way.
    Permanent,
}

/// A specific, greppable failure reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FailureCode {
    /// All NVENC encode sessions are in use.
    ///
    /// Distinct on purpose: it sheds a GPU slot and retries, rather than
    /// concluding the card cannot encode.
    EncoderSessionExhausted,
    /// The decoder could not handle this input — the AV1-on-Turing case.
    DecodeUnsupported,
    /// The requested encoder is not present in this ffmpeg build.
    EncoderUnavailable,
    /// The source changed underneath the job.
    SourceChanged,
    /// Out of space.
    DiskFull,
    /// Permission denied reading the source or writing the output.
    PermissionDenied,
    /// The source is damaged.
    CorruptSource,
    /// The process was killed by a signal.
    Killed,
    /// Nothing matched. Treated conservatively.
    Unknown,
}

/// Classify a failed ffmpeg run.
///
/// `stderr_tail` should be the last few KiB. Matching is lowercase-insensitive
/// substring search: ffmpeg's messages are not stable enough for anything
/// stricter, and a missed match degrades to `Unknown`/`Transient`, which
/// retries — the safe direction. Misclassifying a transient fault as permanent
/// dead-letters real work; the reverse merely costs a retry.
pub fn classify_failure(
    exit: i32,
    signal: Option<i32>,
    stderr_tail: &str,
) -> (FailureClass, FailureCode) {
    if signal.is_some() {
        // OOM killer, operator kill, shutdown. Retryable.
        return (FailureClass::Transient, FailureCode::Killed);
    }

    let s = stderr_tail.to_lowercase();

    // Most specific first: session exhaustion looks like a generic NVENC error
    // otherwise, and would be misread as the card lacking the encoder.
    if s.contains("outofmemory") && s.contains("nvenc")
        || s.contains("no free encoding sessions")
        || s.contains("openencodesessionex failed")
    {
        return (
            FailureClass::Transient,
            FailureCode::EncoderSessionExhausted,
        );
    }

    if s.contains("no space left on device") || s.contains("enospc") {
        return (FailureClass::Transient, FailureCode::DiskFull);
    }

    if s.contains("permission denied") {
        return (FailureClass::Transient, FailureCode::PermissionDenied);
    }

    // Hardware decode refusing the input. Measured on Turing with AV1: exit 69
    // and roughly 1 KB of truncated output.
    if s.contains("no decoder surfaces left")
        || s.contains("hwaccel initialisation returned error")
        || s.contains("hwaccel initialization returned error")
        || s.contains("failed setup for format cuda")
        || exit == 69
    {
        return (
            FailureClass::CapabilityDrift,
            FailureCode::DecodeUnsupported,
        );
    }

    if s.contains("unknown encoder") || s.contains("encoder not found") {
        return (
            FailureClass::CapabilityDrift,
            FailureCode::EncoderUnavailable,
        );
    }

    if s.contains("invalid data found when processing input") || s.contains("moov atom not found") {
        return (FailureClass::Permanent, FailureCode::CorruptSource);
    }

    if s.contains("no such file or directory") {
        return (FailureClass::Transient, FailureCode::SourceChanged);
    }

    // Unmatched: retry rather than dead-letter. A wrong guess here costs one
    // retry; guessing Permanent throws away work that would have succeeded.
    (FailureClass::Transient, FailureCode::Unknown)
}

impl FailureClass {
    /// Whether this class should exclude the agent from similar work.
    ///
    /// Only capability drift does. This is the single most important property
    /// in this module: a transient fault that excluded its agent would, on a
    /// fleet with one GPU, take the whole class of work offline.
    pub fn excludes_agent(self) -> bool {
        matches!(self, FailureClass::CapabilityDrift)
    }

    /// Whether exhausting retries should dead-letter the job.
    pub fn may_dead_letter(self) -> bool {
        !matches!(self, FailureClass::Transient)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvenc_session_exhaustion_is_transient_not_a_missing_encoder() {
        let (class, code) = classify_failure(
            1,
            None,
            "[hevc_nvenc @ 0x55] OpenEncodeSessionEx failed: out of memory (10)",
        );
        assert_eq!(code, FailureCode::EncoderSessionExhausted);
        assert_eq!(class, FailureClass::Transient);
        assert!(
            !class.excludes_agent(),
            "a busy card must not be excluded -- with one GPU that stops all video work"
        );
        assert!(!class.may_dead_letter());
    }

    #[test]
    fn av1_on_turing_is_capability_drift() {
        // The measured signature: exit 69.
        let (class, code) = classify_failure(69, None, "Failed setup for format cuda");
        assert_eq!(code, FailureCode::DecodeUnsupported);
        assert_eq!(class, FailureClass::CapabilityDrift);
        assert!(
            class.excludes_agent(),
            "the server's model said this node could decode it; it cannot"
        );
    }

    #[test]
    fn a_missing_encoder_is_drift_because_the_model_claimed_it() {
        let (class, code) = classify_failure(1, None, "Unknown encoder 'hevc_nvenc'");
        assert_eq!(code, FailureCode::EncoderUnavailable);
        assert_eq!(class, FailureClass::CapabilityDrift);
    }

    #[test]
    fn signals_are_transient() {
        let (class, code) = classify_failure(0, Some(9), "");
        assert_eq!(code, FailureCode::Killed);
        assert_eq!(class, FailureClass::Transient);
    }

    #[test]
    fn disk_full_is_transient_not_permanent() {
        let (class, code) = classify_failure(
            1,
            None,
            "av_interleaved_write_frame(): No space left on device",
        );
        assert_eq!(code, FailureCode::DiskFull);
        assert_eq!(class, FailureClass::Transient);
    }

    #[test]
    fn a_corrupt_source_is_permanent() {
        let (class, code) = classify_failure(1, None, "Invalid data found when processing input");
        assert_eq!(code, FailureCode::CorruptSource);
        assert_eq!(class, FailureClass::Permanent);
        assert!(class.may_dead_letter());
    }

    /// The safe default. An unrecognised failure retries rather than being
    /// thrown away — a wrong guess costs a retry, the opposite loses work.
    #[test]
    fn unrecognised_failures_retry_rather_than_dead_letter() {
        let (class, code) = classify_failure(1, None, "something nobody has seen before");
        assert_eq!(code, FailureCode::Unknown);
        assert_eq!(class, FailureClass::Transient);
        assert!(!class.may_dead_letter());
    }

    #[test]
    fn matching_is_case_insensitive() {
        let (_, code) = classify_failure(1, None, "NO SPACE LEFT ON DEVICE");
        assert_eq!(code, FailureCode::DiskFull);
    }

    /// Session exhaustion must win over the generic NVENC-ish text, or it would
    /// be read as the card lacking the encoder and exclude the only GPU node.
    #[test]
    fn session_exhaustion_is_matched_before_generic_encoder_errors() {
        let (class, code) = classify_failure(
            1,
            None,
            "[hevc_nvenc] No free encoding sessions; Unknown encoder fallback",
        );
        assert_eq!(code, FailureCode::EncoderSessionExhausted);
        assert_eq!(class, FailureClass::Transient);
    }
}
