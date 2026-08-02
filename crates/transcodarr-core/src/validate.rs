// file: crates/transcodarr-core/src/validate.rs
// version: 1.0.0
// guid: 6a209e4b-8c31-4f75-bd60-93e1a7c05284
// last-edited: 2026-08-01
//! Output validation.
//!
//! **Size is not an accept criterion.** The measured AV1/NVDEC hard failure on
//! Turing produces ffmpeg exit 69 and roughly a 1 KB output — and a truncated
//! file is *always* smaller than its source. A size-first gate therefore accepts
//! exactly the outputs that destroyed the media it was supposed to shrink.
//!
//! Gates run in a fixed order and the first failure is terminal, so `Size` is
//! never even consulted for an output that failed `Duration`. That ordering is
//! the safety property; it is asserted by tests, not left to convention.

use serde::{Deserialize, Serialize};

use crate::probe::{MediaProbe, StreamKind};

/// The checks applied to a transcode output, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ValidationGate {
    /// ffmpeg exited non-zero, or on a signal.
    ExitCode,
    /// The output could not be probed at all — not a valid media file.
    Probe,
    /// Output duration does not match the source within tolerance.
    Duration,
    /// Audio or subtitle streams went missing.
    Streams,
    /// The output failed its size policy.
    Size,
}

impl ValidationGate {
    /// Gates in execution order. `Size` is deliberately last.
    pub const ORDER: &'static [ValidationGate] = &[
        ValidationGate::ExitCode,
        ValidationGate::Probe,
        ValidationGate::Duration,
        ValidationGate::Streams,
        ValidationGate::Size,
    ];
}

/// What the output is allowed to do to the file size.
// No `Eq`: `min_shrink` is an f64 and floats have no total equality.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SizePolicy {
    /// The output must be smaller than the source by at least `min_shrink`.
    RequireSmaller {
        /// Fraction of the source size that must be saved, e.g. `0.05` for 5%.
        min_shrink: f64,
    },
    /// The output may be larger. Used for audio stages: re-encoding Opus to
    /// EAC3 640k legitimately grows the file, and rejecting that would strand
    /// the video stage that was supposed to follow it.
    MayGrow,
}

/// Everything needed to judge one output, computed by the planner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationSpec {
    /// Source duration in microseconds, measured at plan time.
    pub source_duration_us: u64,
    /// How much shorter the output may be. The caller computes this as
    /// `min(0.5% of source, 5s)` — a percentage alone permits a 40-minute loss
    /// on a 3-hour film, so the absolute cap is what actually protects you.
    pub max_shorter_us: u64,
    /// How much longer the output may be. Small; encoders round up, they do not
    /// invent minutes.
    pub max_longer_us: u64,
    /// Audio streams the source had, all of which must survive.
    pub expected_audio_streams: usize,
    /// Subtitle streams the source had, all of which must survive.
    pub expected_subtitle_streams: usize,
    /// Source size in bytes.
    pub source_bytes: u64,
    /// What the output is allowed to do to the size.
    pub size_policy: SizePolicy,
}

/// The verdict on one output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// Whether every gate that ran passed.
    pub passed: bool,
    /// The gate that rejected the output, if any.
    pub failed_gate: Option<ValidationGate>,
    /// Human-readable reason, safe to show an operator.
    pub detail: String,
    /// Gates actually evaluated, in order. Short-circuits on first failure —
    /// so this is the evidence that `Size` was never reached.
    pub gates_run: Vec<ValidationGate>,
}

impl ValidationReport {
    fn fail(gates_run: Vec<ValidationGate>, gate: ValidationGate, detail: String) -> Self {
        Self {
            passed: false,
            failed_gate: Some(gate),
            detail,
            gates_run,
        }
    }
}

/// Judge a transcode output.
///
/// `probe` is the *output's* ffprobe result. Its `duration_us` must have been
/// measured from the last packet PTS rather than the container header: a
/// truncated MKV frequently retains the source duration in its header, so a
/// header-derived duration would sail through the gate that exists to catch
/// exactly this.
pub fn validate_output(
    spec: &ValidationSpec,
    probe: &MediaProbe,
    exit_code: i32,
    out_bytes: u64,
) -> ValidationReport {
    let mut run = Vec::new();

    // 1. Exit code.
    run.push(ValidationGate::ExitCode);
    if exit_code != 0 {
        return ValidationReport::fail(
            run,
            ValidationGate::ExitCode,
            format!("ffmpeg exited {exit_code}"),
        );
    }

    // 2. Probeable at all. An output with no duration and no streams is not a
    //    media file, whatever its size says.
    run.push(ValidationGate::Probe);
    let Some(out_duration) = probe.duration_us else {
        return ValidationReport::fail(
            run,
            ValidationGate::Probe,
            "output has no readable duration".to_string(),
        );
    };
    if probe.streams.is_empty() {
        return ValidationReport::fail(
            run,
            ValidationGate::Probe,
            "output contains no streams".to_string(),
        );
    }

    // 3. Duration. Asymmetric: being short means content was lost, being long
    //    by a hair is encoder rounding.
    run.push(ValidationGate::Duration);
    if out_duration < spec.source_duration_us {
        let missing = spec.source_duration_us - out_duration;
        if missing > spec.max_shorter_us {
            return ValidationReport::fail(
                run,
                ValidationGate::Duration,
                format!(
                    "output is {:.1}s shorter than source (limit {:.1}s) — truncated",
                    missing as f64 / 1e6,
                    spec.max_shorter_us as f64 / 1e6
                ),
            );
        }
    } else {
        let extra = out_duration - spec.source_duration_us;
        if extra > spec.max_longer_us {
            return ValidationReport::fail(
                run,
                ValidationGate::Duration,
                format!(
                    "output is {:.1}s longer than source (limit {:.1}s)",
                    extra as f64 / 1e6,
                    spec.max_longer_us as f64 / 1e6
                ),
            );
        }
    }

    // 4. Streams. A bare `-c:a eac3` silently drops every audio track but the
    //    default, and the file still plays — which is what makes it dangerous.
    run.push(ValidationGate::Streams);
    let got_audio = probe.streams_of(StreamKind::Audio).count();
    let got_subs = probe.streams_of(StreamKind::Subtitle).count();
    if got_audio < spec.expected_audio_streams {
        return ValidationReport::fail(
            run,
            ValidationGate::Streams,
            format!(
                "audio streams dropped: expected {}, got {}",
                spec.expected_audio_streams, got_audio
            ),
        );
    }
    if got_subs < spec.expected_subtitle_streams {
        return ValidationReport::fail(
            run,
            ValidationGate::Streams,
            format!(
                "subtitle streams dropped: expected {}, got {}",
                spec.expected_subtitle_streams, got_subs
            ),
        );
    }

    // 5. Size, last and only now that the output is known to be intact.
    run.push(ValidationGate::Size);
    if let SizePolicy::RequireSmaller { min_shrink } = spec.size_policy {
        let target = (spec.source_bytes as f64 * (1.0 - min_shrink)) as u64;
        if out_bytes > target {
            return ValidationReport::fail(
                run,
                ValidationGate::Size,
                format!(
                    "output {out_bytes} B did not shrink by {:.0}% of {} B",
                    min_shrink * 100.0,
                    spec.source_bytes
                ),
            );
        }
    }

    ValidationReport {
        passed: true,
        failed_gate: None,
        detail: "all gates passed".to_string(),
        gates_run: run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::StreamInfo;

    fn probe_with(duration_us: Option<u64>, audio: usize, subs: usize) -> MediaProbe {
        let mut streams = vec![StreamInfo {
            index: 0,
            kind: StreamKind::Video,
            codec: "hevc".into(),
            ..Default::default()
        }];
        for i in 0..audio {
            streams.push(StreamInfo {
                index: 1 + i as u32,
                kind: StreamKind::Audio,
                codec: "eac3".into(),
                ..Default::default()
            });
        }
        for i in 0..subs {
            streams.push(StreamInfo {
                index: 100 + i as u32,
                kind: StreamKind::Subtitle,
                codec: "subrip".into(),
                ..Default::default()
            });
        }
        MediaProbe {
            container: "matroska".into(),
            duration_us,
            streams,
            ..Default::default()
        }
    }

    /// One hour, 2 audio, 1 subtitle, must shrink 5%.
    fn spec() -> ValidationSpec {
        ValidationSpec {
            source_duration_us: 3_600_000_000,
            max_shorter_us: 5_000_000, // min(0.5%, 5s) -> 5s at this length
            max_longer_us: 1_000_000,
            expected_audio_streams: 2,
            expected_subtitle_streams: 1,
            source_bytes: 10_000_000_000,
            size_policy: SizePolicy::RequireSmaller { min_shrink: 0.05 },
        }
    }

    #[test]
    fn a_good_output_passes_every_gate() {
        let r = validate_output(
            &spec(),
            &probe_with(Some(3_600_000_000), 2, 1),
            0,
            4_000_000_000,
        );
        assert!(r.passed, "{}", r.detail);
        assert_eq!(r.gates_run, ValidationGate::ORDER);
    }

    /// The headline safety property. A truncated output is both far too short
    /// and far too small; it must be rejected for being short, and the size
    /// gate must never run — because on size alone it would have *passed*.
    #[test]
    fn truncated_output_fails_duration_before_size_is_consulted() {
        // The measured Turing AV1 failure: ~1 KB out, a few seconds of content.
        let truncated = probe_with(Some(2_000_000), 2, 1);
        let r = validate_output(&spec(), &truncated, 0, 1_024);

        assert!(!r.passed);
        assert_eq!(r.failed_gate, Some(ValidationGate::Duration));
        assert!(
            !r.gates_run.contains(&ValidationGate::Size),
            "Size must never be reached for a truncated output; gates run: {:?}",
            r.gates_run
        );
    }

    /// Proof that the ordering is what saves us: that same truncated file
    /// satisfies the size policy comfortably.
    #[test]
    fn the_truncated_output_would_have_passed_a_size_first_gate() {
        let s = spec();
        let SizePolicy::RequireSmaller { min_shrink } = s.size_policy else {
            unreachable!()
        };
        let target = (s.source_bytes as f64 * (1.0 - min_shrink)) as u64;
        assert!(
            1_024 < target,
            "1 KB is 'smaller', which is exactly why size cannot be the criterion"
        );
    }

    #[test]
    fn nonzero_exit_fails_first_and_probes_nothing() {
        let r = validate_output(&spec(), &probe_with(Some(3_600_000_000), 2, 1), 69, 1_024);
        assert_eq!(r.failed_gate, Some(ValidationGate::ExitCode));
        assert_eq!(r.gates_run, vec![ValidationGate::ExitCode]);
    }

    #[test]
    fn unprobeable_output_fails_before_duration() {
        let r = validate_output(&spec(), &probe_with(None, 2, 1), 0, 1_024);
        assert_eq!(r.failed_gate, Some(ValidationGate::Probe));
        assert!(!r.gates_run.contains(&ValidationGate::Duration));
    }

    #[test]
    fn dropped_audio_tracks_are_caught() {
        // The classic `-c:a eac3` mistake: file plays fine, one track survives.
        let r = validate_output(
            &spec(),
            &probe_with(Some(3_600_000_000), 1, 1),
            0,
            4_000_000_000,
        );
        assert_eq!(r.failed_gate, Some(ValidationGate::Streams));
        assert!(r.detail.contains("audio streams dropped"));
    }

    #[test]
    fn dropped_subtitles_are_caught() {
        let r = validate_output(
            &spec(),
            &probe_with(Some(3_600_000_000), 2, 0),
            0,
            4_000_000_000,
        );
        assert_eq!(r.failed_gate, Some(ValidationGate::Streams));
        assert!(r.detail.contains("subtitle"));
    }

    #[test]
    fn small_duration_drift_within_tolerance_is_accepted() {
        // 2s short on an hour: under the 5s absolute cap.
        let r = validate_output(
            &spec(),
            &probe_with(Some(3_598_000_000), 2, 1),
            0,
            4_000_000_000,
        );
        assert!(r.passed, "{}", r.detail);
    }

    #[test]
    fn a_percentage_only_tolerance_would_have_let_this_through() {
        // 30s short on an hour is 0.83% — under a 1% percentage rule, over the
        // absolute 5s cap. The cap is what catches it.
        let r = validate_output(
            &spec(),
            &probe_with(Some(3_570_000_000), 2, 1),
            0,
            4_000_000_000,
        );
        assert_eq!(r.failed_gate, Some(ValidationGate::Duration));
    }

    #[test]
    fn may_grow_lets_an_audio_stage_get_bigger() {
        // Opus -> EAC3 640k legitimately grows the file. Rejecting it would
        // strand the video stage that was meant to follow.
        let mut s = spec();
        s.size_policy = SizePolicy::MayGrow;
        let r = validate_output(
            &s,
            &probe_with(Some(3_600_000_000), 2, 1),
            0,
            12_000_000_000,
        );
        assert!(r.passed, "{}", r.detail);
        assert_eq!(r.gates_run, ValidationGate::ORDER);
    }

    #[test]
    fn insufficient_shrink_fails_size_but_only_after_everything_else_passed() {
        let r = validate_output(
            &spec(),
            &probe_with(Some(3_600_000_000), 2, 1),
            0,
            9_990_000_000,
        );
        assert_eq!(r.failed_gate, Some(ValidationGate::Size));
        assert_eq!(
            r.gates_run,
            ValidationGate::ORDER,
            "size runs last, not first"
        );
    }
}
