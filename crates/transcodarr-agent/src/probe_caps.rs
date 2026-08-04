// file: crates/transcodarr-agent/src/probe_caps.rs
// version: 1.0.0
// guid: 4b90d216-7e35-42ca-91f8-0537ea16c8d2
// last-edited: 2026-08-03
//! Trial decoding: finding out what the hardware *actually* does.
//!
//! An encoder list is a claim, not a capability. `ffmpeg -decoders` on a Turing
//! card lists AV1 and 10-bit H.264 support that does not work, and the two fail
//! in different ways — which is precisely why one boolean per codec is not
//! enough:
//!
//! - **AV1 on Turing NVDEC fails hard.** Exit 69, roughly a kilobyte of output.
//!   Loud, and easy to catch.
//! - **10-bit H.264 on Turing NVDEC fails soft.** ffmpeg exits **0**, having
//!   silently decoded on the CPU. Nothing is wrong with the output; what is
//!   wrong is that the scheduler now believes this card can take Hi10 work at
//!   GPU speed, and it will queue accordingly.
//!
//! The soft case is the dangerous one, and it is why
//! [`DecoderStatus::VerifiedSoftFallback`] exists as a distinct verdict rather
//! than being folded into "works". A trial that only checks the exit code
//! cannot tell the two apart.

use transcodarr_core::capability::{DecoderCapability, DecoderStatus, DecoderTriple};

/// What one trial decode produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialOutcome {
    /// Process exit code.
    pub exit_code: i32,
    /// Bytes of output written.
    pub output_bytes: u64,
    /// ffmpeg's stderr, which is where the fallback admits itself.
    pub stderr: String,
}

/// Phrases ffmpeg emits when a hardware decoder declines and software takes
/// over.
///
/// Matched case-insensitively against stderr. This is the only signal
/// distinguishing a soft fallback from a genuine hardware decode, because both
/// exit 0 with correct output.
const FALLBACK_MARKERS: &[&str] = &[
    "no decoder surfaces left",
    "hwaccel initialisation returned error",
    "hwaccel initialization returned error",
    "failed setup for format cuda",
    "using auto hwaccel type none",
    "cannot load nvcuvid",
    "decoder does not support",
    "falling back to software decoding",
];

/// How much output a trial must produce to count as having decoded anything.
///
/// The measured AV1 failure writes roughly a kilobyte before giving up, so the
/// threshold sits above it. A trial that "succeeded" with less than this
/// decoded nothing, whatever its exit code says.
pub const MIN_TRIAL_OUTPUT_BYTES: u64 = 8 * 1024;

/// Judge a trial decode.
///
/// Deliberately a pure function of the outcome, so the interesting cases can be
/// tested without a GPU. The measured Turing behaviours are the fixtures.
pub fn classify(outcome: &TrialOutcome) -> (DecoderStatus, String) {
    if outcome.exit_code != 0 {
        return (
            DecoderStatus::VerifiedFail,
            format!(
                "exit {} after {} bytes",
                outcome.exit_code, outcome.output_bytes
            ),
        );
    }

    // Exit 0 with nothing to show for it. The AV1/NVDEC path lands here when
    // ffmpeg is feeling generous about its exit code; treating it as success
    // would advertise a decoder that produces a kilobyte of rubbish.
    if outcome.output_bytes < MIN_TRIAL_OUTPUT_BYTES {
        return (
            DecoderStatus::VerifiedFail,
            format!(
                "exited 0 but wrote only {} bytes; nothing was decoded",
                outcome.output_bytes
            ),
        );
    }

    // The soft failure. Exit 0, correct output, decoded on the CPU. The output
    // is fine; the *capability claim* is not, and a scheduler trusting it will
    // queue Hi10 work to this card at GPU rates.
    let haystack = outcome.stderr.to_ascii_lowercase();
    if let Some(marker) = FALLBACK_MARKERS.iter().find(|m| haystack.contains(*m)) {
        return (
            DecoderStatus::VerifiedSoftFallback,
            format!("decoded on the CPU: stderr contains '{marker}'"),
        );
    }

    (
        DecoderStatus::VerifiedOk,
        format!("decoded {} bytes in hardware", outcome.output_bytes),
    )
}

/// Record a trial's verdict against its triple.
pub fn capability_for(triple: DecoderTriple, outcome: &TrialOutcome) -> DecoderCapability {
    let (status, evidence) = classify(outcome);
    DecoderCapability {
        triple,
        status,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use transcodarr_core::capability::DecoderKind;
    use transcodarr_core::plan::BitDepth;

    fn triple(codec: &str, depth: BitDepth) -> DecoderTriple {
        DecoderTriple {
            codec: codec.into(),
            profile: "High".into(),
            bit_depth: depth,
            kind: DecoderKind::Nvdec,
        }
    }

    fn ok_trial() -> TrialOutcome {
        TrialOutcome {
            exit_code: 0,
            output_bytes: 512 * 1024,
            stderr: "frame= 120 fps=240".into(),
        }
    }

    #[test]
    fn a_clean_hardware_decode_is_verified_ok() {
        let (status, _) = classify(&ok_trial());
        assert_eq!(status, DecoderStatus::VerifiedOk);
    }

    /// The measured Turing AV1 failure: exit 69, roughly a kilobyte of output.
    #[test]
    fn the_turing_av1_hard_failure_is_verified_fail() {
        let (status, evidence) = classify(&TrialOutcome {
            exit_code: 69,
            output_bytes: 1024,
            stderr: "Failed setup for format cuda".into(),
        });
        assert_eq!(status, DecoderStatus::VerifiedFail);
        assert!(evidence.contains("exit 69"));
    }

    /// The dangerous one. Exit 0, correct output, decoded on the CPU. Folding
    /// this into "works" tells the scheduler this card takes Hi10 at GPU speed,
    /// and it queues accordingly.
    #[test]
    fn the_turing_hi10_soft_fallback_is_its_own_verdict() {
        let (status, evidence) = classify(&TrialOutcome {
            exit_code: 0,
            output_bytes: 512 * 1024,
            stderr: "Failed setup for format cuda: hwaccel initialisation returned error\n\
                     frame= 120 fps=12"
                .into(),
        });
        assert_eq!(
            status,
            DecoderStatus::VerifiedSoftFallback,
            "a soft fallback must never be reported as a hardware decode"
        );
        assert!(evidence.contains("CPU"));
    }

    /// ...and the core rule that makes the distinction matter: a soft fallback
    /// does not satisfy a hardware decode requirement.
    #[test]
    fn a_soft_fallback_never_satisfies_a_hardware_requirement() {
        assert!(!DecoderStatus::VerifiedSoftFallback.satisfies_hardware_requirement());
        assert!(DecoderStatus::VerifiedOk.satisfies_hardware_requirement());
        assert!(!DecoderStatus::VerifiedFail.satisfies_hardware_requirement());
        assert!(!DecoderStatus::Untested.satisfies_hardware_requirement());
    }

    /// Exit 0 with nothing written decoded nothing, whatever the exit code
    /// claims. The AV1 path lands here when ffmpeg is generous about exiting.
    #[test]
    fn exiting_zero_with_no_output_is_still_a_failure() {
        let (status, evidence) = classify(&TrialOutcome {
            exit_code: 0,
            output_bytes: 1024,
            stderr: String::new(),
        });
        assert_eq!(status, DecoderStatus::VerifiedFail);
        assert!(evidence.contains("nothing was decoded"));
    }

    /// The marker match must not be defeated by capitalisation -- ffmpeg's
    /// wording varies across builds and locales.
    #[test]
    fn fallback_markers_match_regardless_of_case() {
        for stderr in [
            "HWACCEL INITIALISATION RETURNED ERROR",
            "Cannot load nvcuvid",
            "No decoder surfaces left",
            "falling back to software decoding",
        ] {
            let (status, _) = classify(&TrialOutcome {
                stderr: stderr.into(),
                ..ok_trial()
            });
            assert_eq!(
                status,
                DecoderStatus::VerifiedSoftFallback,
                "stderr {stderr:?} should read as a fallback"
            );
        }
    }

    /// Ordinary progress chatter must not be mistaken for a fallback, or every
    /// working decoder would be demoted.
    #[test]
    fn ordinary_stderr_is_not_mistaken_for_a_fallback() {
        for stderr in [
            "frame= 240 fps=480 q=-1.0 size=  2048kB",
            "[hevc @ 0x55] Reinit context to 1920x1080",
            "",
        ] {
            let (status, _) = classify(&TrialOutcome {
                stderr: stderr.into(),
                ..ok_trial()
            });
            assert_eq!(status, DecoderStatus::VerifiedOk, "stderr {stderr:?}");
        }
    }

    #[test]
    fn a_capability_carries_its_triple_and_its_evidence() {
        let cap = capability_for(triple("h264", BitDepth::Ten), &ok_trial());
        assert_eq!(cap.triple.codec, "h264");
        assert_eq!(cap.triple.bit_depth, BitDepth::Ten);
        assert_eq!(cap.status, DecoderStatus::VerifiedOk);
        assert!(
            !cap.evidence.is_empty(),
            "a verdict without evidence cannot be argued with"
        );
    }

    /// Eight-bit and ten-bit are separate triples: the whole Turing Hi10 lesson
    /// is that one can work while the other silently does not.
    #[test]
    fn bit_depth_is_part_of_the_triple() {
        let eight = capability_for(triple("h264", BitDepth::Eight), &ok_trial());
        let ten = capability_for(
            triple("h264", BitDepth::Ten),
            &TrialOutcome {
                stderr: "hwaccel initialisation returned error".into(),
                ..ok_trial()
            },
        );
        assert_eq!(eight.status, DecoderStatus::VerifiedOk);
        assert_eq!(ten.status, DecoderStatus::VerifiedSoftFallback);
        assert_ne!(eight.triple, ten.triple);
    }
}
