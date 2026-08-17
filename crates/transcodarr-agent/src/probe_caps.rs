// file: crates/transcodarr-agent/src/probe_caps.rs
// version: 2.0.0
// guid: 4b90d216-7e35-42ca-91f8-0537ea16c8d2
// last-edited: 2026-08-16
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
//! cannot tell the two apart. Measured again on the node on 2026-08-16, this
//! time with `High 4:2:2` and `High 4:4:4 Predictive` 8-bit H.264 joining
//! `High 10` as soft fallbacks — all exit 0, all report every frame decoded,
//! all say `Hardware is lacking required capabilities` on stderr and nowhere
//! else.
//!
//! ## Frames, not bytes
//!
//! A trial decodes to `-f null -` and is judged on the frame count ffmpeg
//! reports through `-progress pipe:1`. An earlier version of this module judged
//! on bytes written, which forced every trial to produce a real file: slower,
//! and it put probe output in the same work area the job transport stages
//! into. Frames also say something bytes cannot — a decode that stops at frame
//! three is a failure whose output is a perfectly plausible size.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use transcodarr_core::capability::{DecoderCapability, DecoderKind, DecoderStatus, DecoderTriple};

use crate::probe_samples::TRIAL_SAMPLE_FRAMES;

/// What one trial decode produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialOutcome {
    /// Process exit code, or `None` if it did not exit normally.
    pub exit_code: Option<i32>,
    /// Signal that killed it, where the platform reports one.
    ///
    /// A hardware decoder that faults during initialisation dies here rather
    /// than returning a code, and reading only `exit_code` would score that as
    /// an ordinary failure with no explanation.
    pub signal: Option<i32>,
    /// `-progress pipe:1` output, which carries the frame count.
    pub stdout: String,
    /// ffmpeg's stderr, which is where the fallback admits itself.
    pub stderr: String,
    /// Frames the sample contains, and so the number a full decode reports.
    pub expected_frames: u64,
}

/// Phrases ffmpeg emits when a hardware decoder declines and software takes
/// over.
///
/// Matched case-insensitively against stderr. This is the only signal
/// distinguishing a soft fallback from a genuine hardware decode, because both
/// exit 0, both report every frame, and both produce correct output.
const FALLBACK_MARKERS: &[&str] = &[
    "no decoder surfaces left",
    "hwaccel initialisation returned error",
    "hwaccel initialization returned error",
    "failed setup for format cuda",
    "hardware is lacking required capabilities",
    "using auto hwaccel type none",
    "cannot load nvcuvid",
    "decoder does not support",
    "falling back to software decoding",
];

/// The frame count ffmpeg reported, from `-progress` output.
///
/// `-progress` emits repeated `key=value` blocks; the last `frame=` is the
/// final count. Returns `None` when no count was reported at all, which is
/// itself a failure — a decode that produced no progress produced no frames.
pub fn parse_frames_decoded(progress_stdout: &str) -> Option<u64> {
    progress_stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("frame="))
        .filter_map(|v| v.trim().parse::<u64>().ok())
        .next_back()
}

/// Judge a trial decode.
///
/// Deliberately a pure function of the outcome, so the interesting cases can be
/// tested without a GPU. The measured Turing behaviours are the fixtures.
///
/// Rule order is load-bearing. The fallback check must precede the frame check,
/// because a soft fallback decodes *every* frame — it just does it on the CPU.
/// Testing frames first would score the most dangerous case as a success.
pub fn classify(outcome: &TrialOutcome) -> (DecoderStatus, String) {
    if let Some(sig) = outcome.signal {
        return (
            DecoderStatus::VerifiedFail,
            format!("killed by signal {sig}"),
        );
    }

    if outcome.exit_code != Some(0) {
        let last = outcome
            .stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no stderr");
        return (
            DecoderStatus::VerifiedFail,
            match outcome.exit_code {
                Some(c) => format!("exit {c}: {last}"),
                None => format!("did not exit normally: {last}"),
            },
        );
    }

    // The soft failure. Exit 0, every frame decoded, correct output -- on the
    // CPU. The output is fine; the *capability claim* is not, and a scheduler
    // trusting it will queue Hi10 and 4:2:2 work to this card at GPU rates.
    let haystack = outcome.stderr.to_ascii_lowercase();
    if let Some(marker) = FALLBACK_MARKERS.iter().find(|m| haystack.contains(*m)) {
        return (
            DecoderStatus::VerifiedSoftFallback,
            format!("decoded on the CPU: stderr contains '{marker}'"),
        );
    }

    match parse_frames_decoded(&outcome.stdout) {
        // Not "in hardware": this function is deliberately blind to the decode
        // path, and a software trial reaching here has decoded every frame on
        // the CPU exactly as it was asked to. Only the triple knows which was
        // being tested.
        Some(n) if n >= outcome.expected_frames => {
            (DecoderStatus::VerifiedOk, format!("decoded all {n} frames"))
        }
        Some(n) => (
            DecoderStatus::VerifiedFail,
            format!("decoded {n} of {} frames", outcome.expected_frames),
        ),
        None => (
            DecoderStatus::VerifiedFail,
            "exited 0 but reported no frames; nothing was decoded".to_string(),
        ),
    }
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

/// The `-hwaccel` prefix a decode path needs.
///
/// Empty for software, which is the point: the same trial runs both ways and
/// the only difference is whether the hardware was asked for.
pub fn hwaccel_args_for(kind: DecoderKind) -> &'static [&'static str] {
    match kind {
        DecoderKind::Nvdec => &["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"],
        _ => &[],
    }
}

/// argv for one trial decode.
///
/// `-f null -` discards the frames: what is being measured is whether the
/// decoder produced them, not what they look like. A vector, never a shell
/// string.
pub fn decode_argv(triple: &DecoderTriple, sample: &Path) -> Vec<String> {
    let mut argv: Vec<String> = ["-hide_banner", "-nostdin"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    argv.extend(hwaccel_args_for(triple.kind).iter().map(|s| s.to_string()));
    argv.push("-i".into());
    argv.push(sample.to_string_lossy().into_owned());
    argv.extend(
        [
            "-map",
            "0:v:0",
            "-f",
            "null",
            "-",
            "-progress",
            "pipe:1",
            "-v",
            "error",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    argv
}

/// Run one trial decode and record what it proves.
///
/// A timeout is a failure, not an error to propagate: a hung NVDEC
/// initialisation must never hold up registration, and "this decode does not
/// finish" is a perfectly good reason not to send the card that work.
pub fn run_one(
    ffmpeg: &str,
    triple: &DecoderTriple,
    sample: &Path,
    timeout: Duration,
) -> DecoderCapability {
    let argv = decode_argv(triple, sample);
    let child = Command::new(ffmpeg)
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return DecoderCapability {
                triple: triple.clone(),
                status: DecoderStatus::VerifiedFail,
                evidence: format!("could not run {ffmpeg}: {e}"),
            };
        }
    };

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return DecoderCapability {
                    triple: triple.clone(),
                    status: DecoderStatus::VerifiedFail,
                    evidence: format!("timed out after {}s", timeout.as_secs()),
                };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                return DecoderCapability {
                    triple: triple.clone(),
                    status: DecoderStatus::VerifiedFail,
                    evidence: format!("could not wait on ffmpeg: {e}"),
                };
            }
        }
    }

    match child.wait_with_output() {
        Ok(out) => capability_for(
            triple.clone(),
            &TrialOutcome {
                exit_code: out.status.code(),
                signal: signal_of(&out.status),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                expected_frames: TRIAL_SAMPLE_FRAMES,
            },
        ),
        Err(e) => DecoderCapability {
            triple: triple.clone(),
            status: DecoderStatus::VerifiedFail,
            evidence: format!("could not collect ffmpeg output: {e}"),
        },
    }
}

/// The signal that killed a process, where the platform reports one.
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// Trial every triple, in order, one at a time.
///
/// Sequential on purpose. Concurrent NVDEC probes contend for the same fixed
/// number of decoder sessions on the ASIC, and a trial that failed because
/// another trial held the last session is a false verdict that then gets cached
/// and believed.
pub fn run_all(
    ffmpeg: &str,
    triples: &[(DecoderTriple, std::path::PathBuf)],
    timeout: Duration,
) -> Vec<DecoderCapability> {
    triples
        .iter()
        .map(|(triple, sample)| {
            let cap = run_one(ffmpeg, triple, sample, timeout);
            tracing::debug!(
                codec = %cap.triple.codec,
                profile = %cap.triple.profile,
                depth = cap.triple.bit_depth.bits(),
                kind = ?cap.triple.kind,
                status = ?cap.status,
                evidence = %cap.evidence,
                "trial decode"
            );
            cap
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
            exit_code: Some(0),
            signal: None,
            stdout: "frame=10\nfps=240\nprogress=end\n".into(),
            stderr: String::new(),
            expected_frames: 10,
        }
    }

    #[test]
    fn a_clean_hardware_decode_is_verified_ok() {
        let (status, _) = classify(&ok_trial());
        assert_eq!(status, DecoderStatus::VerifiedOk);
    }

    /// The measured Turing AV1 failure: exit 69.
    #[test]
    fn the_turing_av1_hard_failure_is_verified_fail() {
        let (status, evidence) = classify(&TrialOutcome {
            exit_code: Some(69),
            stderr: "Failed setup for format cuda".into(),
            ..ok_trial()
        });
        assert_eq!(status, DecoderStatus::VerifiedFail);
        assert!(evidence.contains("exit 69"), "{evidence}");
    }

    /// The dangerous one. Exit 0, every frame decoded, correct output, on the
    /// CPU. Folding this into "works" tells the scheduler this card takes Hi10
    /// at GPU speed, and it queues accordingly.
    #[test]
    fn the_turing_hi10_soft_fallback_is_its_own_verdict() {
        let (status, evidence) = classify(&TrialOutcome {
            stderr: "Failed setup for format cuda: hwaccel initialisation returned error".into(),
            ..ok_trial()
        });
        assert_eq!(
            status,
            DecoderStatus::VerifiedSoftFallback,
            "a soft fallback must never be reported as a hardware decode"
        );
        assert!(evidence.contains("CPU"), "{evidence}");
    }

    /// Measured on the node on 2026-08-16 for `High 4:2:2` and
    /// `High 4:4:4 Predictive` 8-bit H.264 -- the wording NVDEC uses when the
    /// chroma format is the thing it cannot do.
    #[test]
    fn the_measured_chroma_fallback_wording_is_recognised() {
        let (status, _) = classify(&TrialOutcome {
            stderr: "[h264 @ 0000023b] Hardware is lacking required capabilities".into(),
            ..ok_trial()
        });
        assert_eq!(status, DecoderStatus::VerifiedSoftFallback);
    }

    /// Rule order. A soft fallback decodes every frame, so a frame check that
    /// ran first would score the most dangerous case as a success.
    #[test]
    fn a_soft_fallback_outranks_a_complete_frame_count() {
        let (status, _) = classify(&TrialOutcome {
            stdout: "frame=10\nprogress=end\n".into(),
            stderr: "hwaccel initialisation returned error".into(),
            ..ok_trial()
        });
        assert_eq!(status, DecoderStatus::VerifiedSoftFallback);
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

    /// A decode that stops early produced output of an entirely plausible size.
    /// Only the frame count says it did not finish.
    #[test]
    fn a_short_decode_is_a_failure() {
        let (status, evidence) = classify(&TrialOutcome {
            stdout: "frame=3\nprogress=end\n".into(),
            ..ok_trial()
        });
        assert_eq!(status, DecoderStatus::VerifiedFail);
        assert!(evidence.contains("3 of 10"), "{evidence}");
    }

    #[test]
    fn exiting_zero_with_no_frames_reported_is_still_a_failure() {
        let (status, evidence) = classify(&TrialOutcome {
            stdout: String::new(),
            ..ok_trial()
        });
        assert_eq!(status, DecoderStatus::VerifiedFail);
        assert!(evidence.contains("nothing was decoded"), "{evidence}");
    }

    /// A hardware decoder that faults during initialisation dies on a signal
    /// and never returns a code at all.
    #[test]
    fn a_process_killed_by_a_signal_is_a_failure_that_says_so() {
        let (status, evidence) = classify(&TrialOutcome {
            exit_code: None,
            signal: Some(11),
            ..ok_trial()
        });
        assert_eq!(status, DecoderStatus::VerifiedFail);
        assert!(evidence.contains("signal 11"), "{evidence}");
    }

    #[test]
    fn the_last_frame_count_wins() {
        assert_eq!(
            parse_frames_decoded("frame=1\nfps=0\nframe=7\nframe=10\nprogress=end\n"),
            Some(10)
        );
        assert_eq!(parse_frames_decoded("fps=0\nprogress=end\n"), None);
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

    #[test]
    fn a_hardware_trial_asks_for_the_hardware_and_a_software_one_does_not() {
        let mut t = triple("h264", BitDepth::Eight);
        let argv = decode_argv(&t, Path::new("/s/x.mkv"));
        let i = argv.iter().position(|a| a == "-hwaccel").expect("nvdec");
        assert_eq!(argv[i + 1], "cuda");

        t.kind = DecoderKind::Software;
        assert!(
            !decode_argv(&t, Path::new("/s/x.mkv")).contains(&"-hwaccel".to_string()),
            "a software trial that asks for cuda measures the wrong thing"
        );
    }

    /// The frame count only exists because `-progress` was asked for, and the
    /// sample path must survive spaces without a shell to re-split it.
    #[test]
    fn a_trial_requests_progress_and_passes_its_path_whole() {
        let argv = decode_argv(&triple("h264", BitDepth::Eight), Path::new("/a dir/x.mkv"));
        let i = argv.iter().position(|a| a == "-progress").unwrap();
        assert_eq!(argv[i + 1], "pipe:1");
        assert!(argv.contains(&"/a dir/x.mkv".to_string()));
        let f = argv.iter().position(|a| a == "-f").unwrap();
        assert_eq!(argv[f + 1], "null");
    }
}
