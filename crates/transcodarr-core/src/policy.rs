// file: crates/transcodarr-core/src/policy.rs
// version: 1.6.0
// guid: 2d8f47a1-0c96-4b53-89e7-f14b6a03d752
// last-edited: 2026-08-16
//! The rules engine, and `Default Space Saver`.
//!
//! Rules are an ordered list of typed `when`/`then` entries evaluated
//! first-match-wins. Deliberately not a DSL and deliberately not a node graph:
//! this is the part of Tdarr that became unreviewable, and the whole point is
//! that a policy diffs cleanly in a pull request and can be re-run over stored
//! facts to show exactly which decisions would change.
//!
//! Evaluation is a pure function of `(FileFacts, Policy)`. No clock, no
//! filesystem, no randomness — so a decision is reproducible, and
//! `evaluate_explained` can show *why* rather than just *what*.

use serde::{Deserialize, Serialize};

use crate::capability::{
    AgentClass, ContainerId, DecoderKind, DecoderTriple, Requirement, Requirements,
};
use crate::facts::{FileFacts, SizeThresholds, content_sig, size_bucket_for};
use crate::job::{JobClass, JobSpec};
use crate::plan::{BitDepth, EncodePlan, EncoderId};
use crate::validate::{SizePolicy, ValidationSpec};

/// Predicates selecting which files a rule applies to. All present fields must
/// match — an empty `Match` matches everything, which is how a catch-all
/// terminal rule is written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Match {
    /// Video codec must be one of these.
    #[serde(default)]
    pub video_codec_in: Vec<String>,
    /// At least one audio codec must be one of these.
    #[serde(default)]
    pub audio_codec_any: Vec<String>,
    /// Minimum width in pixels.
    pub min_width: Option<u32>,
    /// Minimum overall bitrate.
    pub min_bit_rate_bps: Option<u64>,
    /// Minimum file size.
    pub min_size_bytes: Option<u64>,
    /// Require (or forbid) HDR.
    pub is_hdr: Option<bool>,
    /// Require (or forbid) Dolby Vision.
    pub is_dovi: Option<bool>,
}

impl Match {
    /// Whether these facts satisfy every present predicate.
    pub fn matches(&self, f: &FileFacts) -> bool {
        if !self.video_codec_in.is_empty() {
            match &f.video_codec {
                Some(c) if self.video_codec_in.iter().any(|w| w == c) => {}
                _ => return false,
            }
        }
        if !self.audio_codec_any.is_empty()
            && !f
                .audio_codecs
                .iter()
                .any(|c| self.audio_codec_any.iter().any(|w| c.contains(w.as_str())))
        {
            return false;
        }
        if let Some(w) = self.min_width {
            if f.width.unwrap_or(0) < w {
                return false;
            }
        }
        if let Some(b) = self.min_bit_rate_bps {
            if f.bit_rate_bps.unwrap_or(0) < b {
                return false;
            }
        }
        if let Some(s) = self.min_size_bytes {
            if f.size_bytes < s {
                return false;
            }
        }
        if let Some(h) = self.is_hdr {
            if f.is_hdr != h {
                return false;
            }
        }
        if let Some(d) = self.is_dovi {
            if f.is_dovi != d {
                return false;
            }
        }
        true
    }
}

/// What to do with a matching file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Action {
    /// Re-encode video with this encoder preference, in order.
    EncodeVideo {
        /// Encoders to try, most preferred first.
        encoder_preference: Vec<EncoderId>,
        /// Quality target (CRF or CQ).
        quality: u8,
    },
    /// Re-encode audio to this codec at this bitrate.
    EncodeAudio {
        /// Target codec.
        codec: EncoderId,
        /// Bitrate string, e.g. `640k`.
        bitrate: String,
    },
    /// Do nothing at all.
    Skip,
    /// Leave the video alone, but audio may still be planned.
    SkipVideo,
    /// Exclude the file from all work and flag it.
    Quarantine,
}

/// One named rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// Name, shown in explanations and diffs.
    pub name: String,
    /// Predicates.
    pub when: Match,
    /// What to do.
    pub then: Action,
}

/// An ordered rule list plus global thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// Rules, evaluated in order, first match wins per stage.
    pub rules: Vec<Rule>,
    /// Size bucket boundaries.
    #[serde(default)]
    pub size_thresholds: SizeThresholds,
}

/// Which stages a decision covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DecisionClass {
    /// Nothing to do.
    None,
    /// Audio only.
    Audio,
    /// Video only.
    Video,
    /// Audio first, then video as a follow-up job.
    AudioThenVideo,
    /// Excluded and flagged.
    Quarantined,
}

impl DecisionClass {
    /// The canonical spelling, which is also the value stored in SQLite.
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionClass::None => "None",
            DecisionClass::Audio => "Audio",
            DecisionClass::Video => "Video",
            DecisionClass::AudioThenVideo => "AudioThenVideo",
            DecisionClass::Quarantined => "Quarantined",
        }
    }

    /// Parse the canonical spelling. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "None" => DecisionClass::None,
            "Audio" => DecisionClass::Audio,
            "Video" => DecisionClass::Video,
            "AudioThenVideo" => DecisionClass::AudioThenVideo,
            "Quarantined" => DecisionClass::Quarantined,
            _ => return None,
        })
    }
}

impl std::fmt::Display for DecisionClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Target audio encode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioPlan {
    /// Target codec.
    pub codec: EncoderId,
    /// Target bitrate.
    pub bitrate: String,
}

/// Target video encode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoPlan {
    /// Encoders to try, most preferred first.
    pub encoder_preference: Vec<EncoderId>,
    /// Quality target.
    pub quality: u8,
    /// Source bit depth, which the output must preserve.
    pub source_depth: BitDepth,
}

/// The outcome of evaluating a policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// Which stages apply.
    pub class: DecisionClass,
    /// Audio stage, if any.
    pub audio: Option<AudioPlan>,
    /// Video stage, if any.
    pub video: Option<VideoPlan>,
    /// Why, in words an operator can act on.
    pub reason: String,
}

/// Which rule produced which effect, for `--explain`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleTrace {
    /// Rule name.
    pub rule: String,
    /// Whether it matched.
    pub matched: bool,
    /// What it contributed.
    pub effect: String,
}

/// Turn a decision into the encode it describes.
///
/// `None` when the decision owes no encode. The audio stage is emitted for
/// `Audio` *and* `AudioThenVideo`, because the video half of a two-stage
/// decision is a separate follow-up job with its own row — trying to express
/// both in one ffmpeg invocation is what makes a partial failure unrecoverable.
pub fn encode_plan_for(d: &Decision, _facts: &FileFacts) -> Option<EncodePlan> {
    match d.class {
        DecisionClass::None | DecisionClass::Quarantined => None,

        DecisionClass::Audio | DecisionClass::AudioThenVideo => {
            let audio = d.audio.as_ref()?;
            Some(EncodePlan {
                // Video is copied, not re-encoded. An audio pass that touches
                // video would re-encode every file in the library for a change
                // to its soundtrack.
                video_codec: EncoderId::Copy,
                audio_codec: audio.codec,
                pix_fmt: None,
                extra_args: vec![
                    "-b:a".to_string(),
                    audio.bitrate.clone(),
                    // Map every stream. A bare `-c:a eac3` silently keeps only
                    // the default track and drops the rest -- measured, and the
                    // single easiest way to quietly destroy a library.
                    "-map".to_string(),
                    "0".to_string(),
                ],
            })
        }

        DecisionClass::Video => {
            let video = d.video.as_ref()?;
            let encoder = *video.encoder_preference.first()?;
            Some(EncodePlan {
                video_codec: encoder,
                // Audio is copied in a video pass. If the audio also needed
                // work, the decision would have been AudioThenVideo.
                audio_codec: EncoderId::Copy,
                // Bit depth is preserved, never upconverted: libx265 wants
                // yuv420p10le and NVENC wants p010le, and the wrong one errors
                // the job outright.
                pix_fmt: crate::plan::pix_fmt_for(encoder, video.source_depth),
                extra_args: vec![
                    "-crf".to_string(),
                    video.quality.to_string(),
                    "-map".to_string(),
                    "0".to_string(),
                ],
            })
        }
    }
}

/// Build the validation contract for an output, from the source's facts.
///
/// Two production rules are encoded here and must not be softened:
///
/// - **Tolerance is asymmetric and absolutely capped at `min(0.5%, 5s)`.** A
///   percentage alone permits a 40-minute loss on a three-hour film, so the
///   absolute cap is what actually protects the media.
/// - **An audio-only pass may grow the file.** Re-encoding Opus or TrueHD to
///   EAC3 640k legitimately produces a larger output, and requiring shrinkage
///   would reject every audio job and strand the video stage meant to follow.
pub fn validation_spec_for(facts: &FileFacts, d: &Decision) -> ValidationSpec {
    let source_duration_us = facts.duration_us.unwrap_or(0);
    let max_shorter_us = std::cmp::min(source_duration_us / 200, 5_000_000);

    let size_policy = match d.class {
        DecisionClass::Audio | DecisionClass::AudioThenVideo => SizePolicy::MayGrow,
        // A video pass that saved nothing did work for no reason; 2% is low
        // enough not to reject a legitimately marginal encode.
        _ => SizePolicy::RequireSmaller { min_shrink: 0.02 },
    };

    ValidationSpec {
        source_duration_us,
        max_shorter_us,
        // Encoders round up by a frame or two; they do not invent minutes.
        max_longer_us: 2_000_000,
        expected_audio_streams: facts.audio_track_count,
        expected_subtitle_streams: facts.subtitle_track_count,
        source_bytes: facts.size_bytes,
        size_policy,
    }
}

/// A content-addressed version of a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesVersion(pub String);

/// Hash a policy. Bumping the version invalidates every stored skip marker by
/// construction, so a policy change cannot leave stale decisions behind.
pub fn rules_version(policy: &Policy) -> RulesVersion {
    let canonical = serde_json::to_string(policy).unwrap_or_default();
    RulesVersion(blake3::hash(canonical.as_bytes()).to_hex().to_string())
}

/// Evaluate a policy against a file's facts.
pub fn evaluate(facts: &FileFacts, policy: &Policy) -> Decision {
    evaluate_explained(facts, policy).0
}

/// Evaluate, and report which rules fired.
pub fn evaluate_explained(facts: &FileFacts, policy: &Policy) -> (Decision, Vec<RuleTrace>) {
    let mut trace = Vec::new();

    // Hard exclusions come first and are not expressible as rules, because they
    // must not be overridable by a policy edit. DV profile 7 is dual-layer and
    // ffmpeg cannot round-trip it; object audio is destroyed by a channel-based
    // re-encode. Getting either wrong is unrecoverable.
    if facts.is_excluded() {
        trace.push(RuleTrace {
            rule: "<builtin exclusion>".into(),
            matched: true,
            effect: "quarantined".into(),
        });
        return (
            Decision {
                class: DecisionClass::Quarantined,
                audio: None,
                video: None,
                reason: if facts.has_object_audio {
                    "object audio (Atmos/DTS:X) would be destroyed by a channel re-encode".into()
                } else {
                    "Dolby Vision profile cannot be round-tripped".into()
                },
            },
            trace,
        );
    }

    let mut audio: Option<AudioPlan> = None;
    let mut video: Option<VideoPlan> = None;
    let mut video_vetoed = false;
    let mut reasons: Vec<String> = Vec::new();

    // HDR and DV veto the video stage but leave audio available. This is why
    // the two stages are decided independently: an HDR film with a TrueHD track
    // still has useful audio work, and coupling them would strand it.
    if !facts.video_is_encodable() {
        video_vetoed = true;
        reasons.push("video left alone (HDR or Dolby Vision)".into());
        trace.push(RuleTrace {
            rule: "<builtin HDR/DV veto>".into(),
            matched: true,
            effect: "video stage vetoed".into(),
        });
    }

    for rule in &policy.rules {
        let matched = rule.when.matches(facts);
        let effect = match (&rule.then, matched) {
            (_, false) => "no match".to_string(),
            (Action::Skip, true) => {
                trace.push(RuleTrace {
                    rule: rule.name.clone(),
                    matched: true,
                    effect: "skip all".into(),
                });
                return (
                    Decision {
                        class: DecisionClass::None,
                        audio: None,
                        video: None,
                        reason: format!("rule '{}' says skip", rule.name),
                    },
                    trace,
                );
            }
            (Action::Quarantine, true) => {
                trace.push(RuleTrace {
                    rule: rule.name.clone(),
                    matched: true,
                    effect: "quarantine".into(),
                });
                return (
                    Decision {
                        class: DecisionClass::Quarantined,
                        audio: None,
                        video: None,
                        reason: format!("rule '{}' quarantined this file", rule.name),
                    },
                    trace,
                );
            }
            (Action::SkipVideo, true) => {
                video_vetoed = true;
                "video stage vetoed".to_string()
            }
            (Action::EncodeAudio { codec, bitrate }, true) => {
                if audio.is_none() {
                    audio = Some(AudioPlan {
                        codec: *codec,
                        bitrate: bitrate.clone(),
                    });
                    reasons.push(format!("audio -> {}", codec.as_ffmpeg()));
                    "audio stage planned".to_string()
                } else {
                    "audio already planned by an earlier rule".to_string()
                }
            }
            (
                Action::EncodeVideo {
                    encoder_preference,
                    quality,
                },
                true,
            ) => {
                if video_vetoed {
                    "video stage vetoed, rule ignored".to_string()
                } else if video.is_none() {
                    video = Some(VideoPlan {
                        encoder_preference: encoder_preference.clone(),
                        quality: *quality,
                        source_depth: facts.video_bit_depth.unwrap_or(BitDepth::Eight),
                    });
                    reasons.push("video re-encode planned".into());
                    "video stage planned".to_string()
                } else {
                    "video already planned by an earlier rule".to_string()
                }
            }
        };
        trace.push(RuleTrace {
            rule: rule.name.clone(),
            matched,
            effect,
        });
    }

    let class = match (audio.is_some(), video.is_some()) {
        (false, false) => DecisionClass::None,
        (true, false) => DecisionClass::Audio,
        (false, true) => DecisionClass::Video,
        // Audio runs first and the video job is derived afterwards, from a
        // fresh probe of the audio stage's output. The two never run at once on
        // the same file.
        (true, true) => DecisionClass::AudioThenVideo,
    };

    let reason = if reasons.is_empty() {
        "no rule required work".to_string()
    } else {
        reasons.join("; ")
    };

    (
        Decision {
            class,
            audio,
            video,
            reason,
        },
        trace,
    )
}

/// Derive the *next* job for a decision, or `None` when there is nothing to do.
///
/// Only ever one job at a time per file. For `AudioThenVideo` this returns the
/// audio job; the video job is derived after the audio output is probed, which
/// is what keeps the two stages from racing on the same file.
pub fn next_job(d: &Decision, facts: &FileFacts, t: &SizeThresholds) -> Option<JobSpec> {
    let bucket = size_bucket_for(facts.size_bytes, t);
    let sig = content_sig(facts).0;

    match d.class {
        DecisionClass::None | DecisionClass::Quarantined => None,

        DecisionClass::Audio | DecisionClass::AudioThenVideo => {
            let plan = d.audio.as_ref()?;
            Some(JobSpec {
                class: JobClass::Audio,
                size_bucket: bucket,
                requirements: Requirements(vec![
                    // `Audio`, not `Cpu`. An audio pass is `-c:v copy` and uses
                    // no video encoder at all, but an agent offers `Cpu` only
                    // when it has libx264 or libx265 — so requiring `Cpu` here
                    // meant audio work could land only on nodes with a software
                    // *video* encoder, which has nothing to do with the job.
                    //
                    // It cost the GPU node outright: `windows-rtx2070` has
                    // `hevc_nvenc` and no libx264, advertises `[Audio, Gpu]`,
                    // and could never be given a single audio job despite
                    // advertising the class for it. `AgentClass::Audio` was
                    // generated by every agent and required by nothing — a
                    // class nobody asked for is the shape this bug made.
                    //
                    // What actually gates placement is the encoder and the
                    // muxer below, which is right: those are what an audio pass
                    // needs.
                    Requirement::AgentClass(AgentClass::Audio),
                    Requirement::Encoder(plan.codec),
                    Requirement::Muxer(ContainerId::Matroska),
                ]),
                expected_content_sig: sig,
            })
        }

        DecisionClass::Video => {
            let plan = d.video.as_ref()?;
            let encoder = *plan.encoder_preference.first()?;
            let gpu = matches!(encoder, EncoderId::HevcNvenc);

            let mut reqs = vec![
                Requirement::AgentClass(if gpu {
                    AgentClass::Gpu
                } else {
                    AgentClass::Cpu
                }),
                Requirement::Encoder(encoder),
                Requirement::Muxer(ContainerId::Matroska),
            ];

            // The decode requirement describes the decode this job actually
            // performs, which is a software one on both paths.
            //
            // This used to ask for `Nvdec` whenever the encoder was a hardware
            // one, on the stated grounds that "a hardware encoder implies
            // nothing about the decoder, which is the gap Hi10 falls through".
            // That reasoning is sound and the requirement was still wrong,
            // because it described a pipeline that does not exist:
            // `plan::build_ffmpeg_argv_raw` emits `-i <input>` immediately
            // before `-c:v` and appends its `extra` arguments *after* the codec
            // flags, and `-hwaccel` is an input option that must precede `-i`.
            // The builder cannot express one. Every GPU job software-decodes
            // and NVENC-encodes.
            //
            // So the NVDEC requirement was strictly stricter than the work, and
            // fail-closed matching turned that into refusing jobs the card
            // demonstrably completes. Measured on a Turing node 2026-08-16: a
            // 10-bit `High 10` source blocked at `capability`, while that job's
            // exact argv run by hand finished 300 frames of `hevc_nvenc` at 248
            // fps. `High 4:2:2` and `av1` were refused the same way.
            //
            // The agent still trials NVDEC and still reports per-profile
            // verdicts — they are worth having, and `survey` prints them,
            // because the profile carries a verdict that codec and depth do
            // not: at a fixed codec and depth, `Constrained Baseline`, `Main`
            // and `High` H.264 decode in hardware on that card while
            // `High 4:2:2` and `High 4:4:4 Predictive` silently fall back. They
            // become dispatch-relevant the moment the plan builder can ask for
            // a hardware decode. Until then, requiring them buys nothing.
            //
            // The profile is left empty because software decode does not vary
            // by it: ffmpeg either has the decoder compiled in or it does not.
            // Naming it would mean every profile nobody thought to trial —
            // `Main` is the common one, and it is in no candidate list — went
            // `Untested` and blocked the job, for no safety in return.
            if let Some(codec) = &facts.video_codec {
                reqs.push(Requirement::Decoder(DecoderTriple {
                    codec: codec.clone(),
                    profile: String::new(),
                    bit_depth: plan.source_depth,
                    kind: DecoderKind::Software,
                }));
            }

            Some(JobSpec {
                class: if gpu {
                    JobClass::VideoGpu
                } else {
                    JobClass::VideoCpu
                },
                size_bucket: bucket,
                requirements: Requirements(reqs),
                expected_content_sig: sig,
            })
        }
    }
}

/// The built-in policy: convert unwanted audio, shrink oversized video, and
/// leave everything else alone.
///
/// Named for what it does rather than how it works. It is meant to be usable
/// with no configuration at all — the thing Tdarr makes you assemble by hand.
pub fn default_space_saver() -> Policy {
    Policy {
        rules: vec![
            Rule {
                name: "convert lossless and opus audio to eac3".into(),
                when: Match {
                    audio_codec_any: vec![
                        "truehd".into(),
                        "dts".into(),
                        "flac".into(),
                        "pcm".into(),
                        "mlp".into(),
                        "opus".into(),
                    ],
                    ..Default::default()
                },
                then: Action::EncodeAudio {
                    codec: EncoderId::Eac3,
                    bitrate: "640k".into(),
                },
            },
            Rule {
                name: "re-encode non-hevc video above 2 Mbps".into(),
                when: Match {
                    video_codec_in: vec!["h264".into(), "mpeg2video".into(), "vc1".into()],
                    min_bit_rate_bps: Some(2_000_000),
                    ..Default::default()
                },
                then: Action::EncodeVideo {
                    encoder_preference: vec![EncoderId::HevcNvenc, EncoderId::Libx265],
                    quality: 22,
                },
            },
            Rule {
                name: "re-encode av1 on cpu only".into(),
                when: Match {
                    video_codec_in: vec!["av1".into()],
                    ..Default::default()
                },
                // No NVENC in the preference list: Turing cannot decode AV1 at
                // all, so offering the GPU would plan a job that can only fail.
                then: Action::EncodeVideo {
                    encoder_preference: vec![EncoderId::Libx265],
                    quality: 22,
                },
            },
        ],
        size_thresholds: SizeThresholds::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::TransportMode;
    use crate::capability::{
        Capability, DecoderCapability, DecoderStatus, Mount, Platform, satisfies,
    };

    fn facts(video: &str, depth: BitDepth, audio: &[&str], bitrate: u64) -> FileFacts {
        FileFacts {
            container: "matroska".into(),
            duration_us: Some(3_600_000_000),
            size_bytes: 5_000_000_000,
            bit_rate_bps: Some(bitrate),
            video_codec: Some(video.into()),
            video_bit_depth: Some(depth),
            width: Some(1920),
            height: Some(1080),
            audio_codecs: audio.iter().map(|s| s.to_string()).collect(),
            audio_track_count: audio.len(),
            ..Default::default()
        }
    }

    /// The documented example: Opus stereo with HEVC video yields exactly one
    /// audio job and no video work at all.
    #[test]
    fn opus_with_hevc_gives_audio_only() {
        let d = evaluate(
            &facts("hevc", BitDepth::Ten, &["opus"], 4_000_000),
            &default_space_saver(),
        );
        assert_eq!(d.class, DecisionClass::Audio);
        assert_eq!(d.audio.as_ref().unwrap().codec, EncoderId::Eac3);
        assert!(d.video.is_none(), "hevc matches no encode-video rule");
    }

    #[test]
    fn h264_above_the_bitrate_floor_is_re_encoded() {
        let d = evaluate(
            &facts("h264", BitDepth::Eight, &["aac"], 8_000_000),
            &default_space_saver(),
        );
        assert_eq!(d.class, DecisionClass::Video);
        assert_eq!(
            d.video.as_ref().unwrap().encoder_preference[0],
            EncoderId::HevcNvenc
        );
    }

    #[test]
    fn low_bitrate_h264_is_left_alone() {
        let d = evaluate(
            &facts("h264", BitDepth::Eight, &["aac"], 900_000),
            &default_space_saver(),
        );
        assert_eq!(d.class, DecisionClass::None);
    }

    #[test]
    fn lossless_audio_plus_encodable_video_is_two_stages() {
        let d = evaluate(
            &facts("h264", BitDepth::Eight, &["truehd"], 8_000_000),
            &default_space_saver(),
        );
        assert_eq!(d.class, DecisionClass::AudioThenVideo);
    }

    /// HDR vetoes video but must not strand the audio work.
    #[test]
    fn hdr_vetoes_video_yet_audio_is_still_planned() {
        let mut f = facts("h264", BitDepth::Ten, &["truehd"], 9_000_000);
        f.is_hdr = true;
        let d = evaluate(&f, &default_space_saver());
        assert_eq!(d.class, DecisionClass::Audio);
        assert!(d.video.is_none(), "HDR must never be re-encoded");
        assert!(d.audio.is_some(), "audio work must survive the video veto");
    }

    #[test]
    fn object_audio_is_quarantined_not_processed() {
        let mut f = facts("h264", BitDepth::Eight, &["truehd"], 9_000_000);
        f.has_object_audio = true;
        let d = evaluate(&f, &default_space_saver());
        assert_eq!(d.class, DecisionClass::Quarantined);
        assert!(d.reason.contains("object audio"));
    }

    #[test]
    fn dolby_vision_profile_7_is_quarantined() {
        let mut f = facts("hevc", BitDepth::Ten, &["eac3"], 9_000_000);
        f.is_dovi = true;
        f.dovi_profile = Some(7);
        let d = evaluate(&f, &default_space_saver());
        assert_eq!(d.class, DecisionClass::Quarantined);
    }

    /// The documented AV1 case. Policy does not encode "not the GPU box" — it
    /// names a decoder triple, and capability matching does the rest.
    #[test]
    fn av1_plans_a_cpu_job_that_the_turing_node_cannot_satisfy() {
        let f = facts("av1", BitDepth::Eight, &["aac"], 6_000_000);
        let d = evaluate(&f, &default_space_saver());
        assert_eq!(d.class, DecisionClass::Video);
        assert_eq!(
            d.video.as_ref().unwrap().encoder_preference,
            vec![EncoderId::Libx265],
            "no NVENC offered: Turing cannot decode AV1 at all"
        );

        let job = next_job(&d, &f, &SizeThresholds::default()).unwrap();
        assert_eq!(job.class, JobClass::VideoCpu);

        // The GPU node advertises NVENC but not libx265, so it fails to match
        // *before* dispatch rather than failing at runtime with exit 69.
        let gpu = Capability {
            transport: TransportMode::Mount,
            classes: vec![AgentClass::Gpu],
            encoders: vec![EncoderId::HevcNvenc],
            muxers: vec![ContainerId::Matroska],
            decoders: vec![DecoderCapability {
                triple: DecoderTriple {
                    codec: "av1".into(),
                    profile: String::new(),
                    bit_depth: BitDepth::Eight,
                    kind: DecoderKind::Nvdec,
                },
                status: DecoderStatus::VerifiedFail,
                evidence: "exit 69".into(),
            }],
            effective_cores: 8.0,
            mounts: vec![Mount {
                canonical_prefix: "/mnt/media".into(),
                local_path: "Z:\\".into(),
                writable: true,
            }],
            platform: Some(Platform::Windows),
            workarea_free_bytes: u64::MAX,
            workarea_path: String::new(),
            labels: vec![],
        };
        assert!(
            satisfies(&gpu, &job.requirements).is_err(),
            "AV1 must not route to the Turing node"
        );
    }

    /// A video job asks for the decode it performs, which is a software one
    /// even when the encoder is NVENC.
    ///
    /// `build_ffmpeg_argv_raw` cannot emit `-hwaccel` — it is an input option
    /// and the builder puts nothing before `-i` — so requiring `Nvdec` here
    /// described a pipeline that does not run, and fail-closed matching turned
    /// that into refusing work the card completes.
    #[test]
    fn a_gpu_video_job_requires_the_software_decode_it_actually_performs() {
        let f = facts("h264", BitDepth::Eight, &["aac"], 8_000_000);
        let d = evaluate(&f, &default_space_saver());
        let job = next_job(&d, &f, &SizeThresholds::default()).unwrap();

        // The encoder is what makes this job a GPU job.
        assert!(
            job.requirements
                .0
                .iter()
                .any(|r| matches!(r, Requirement::AgentClass(AgentClass::Gpu))),
            "the job should still be a GPU job: {:?}",
            job.requirements.0
        );

        let decoders: Vec<&DecoderTriple> = job
            .requirements
            .0
            .iter()
            .filter_map(|r| match r {
                Requirement::Decoder(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(decoders.len(), 1, "exactly one decode path is asked for");
        assert_eq!(decoders[0].kind, DecoderKind::Software);
        assert_eq!(
            decoders[0].profile, "",
            "software decode does not vary by profile"
        );
    }

    /// The profiles NVDEC silently falls back on must not block a GPU job.
    ///
    /// These are the three the Turing node refused before the requirement was
    /// corrected: `High 4:2:2` and `High 10` trial as `VerifiedSoftFallback`
    /// and `av1` as `VerifiedFail`, yet each transcodes fine, because nothing
    /// in the job asks NVDEC to decode anything.
    #[test]
    fn a_profile_nvdec_cannot_handle_still_yields_a_satisfiable_requirement() {
        for (codec, profile, depth) in [
            ("h264", "High 4:2:2", BitDepth::Eight),
            ("h264", "High 10", BitDepth::Ten),
            ("av1", "Main", BitDepth::Eight),
        ] {
            let mut f = facts(codec, depth, &["aac"], 8_000_000);
            f.video_profile = Some(profile.to_string());
            let d = evaluate(&f, &default_space_saver());
            let Some(job) = next_job(&d, &f, &SizeThresholds::default()) else {
                continue;
            };
            for r in &job.requirements.0 {
                if let Requirement::Decoder(t) = r {
                    assert_eq!(
                        t.kind,
                        DecoderKind::Software,
                        "{codec} {profile} asked for a hardware decode the plan never performs"
                    );
                    assert_eq!(
                        t.profile, "",
                        "{codec} {profile} keyed the triple on a profile, so any agent that \
                         did not happen to trial it would refuse the job"
                    );
                }
            }
        }
    }

    #[test]
    fn audio_then_video_emits_the_audio_job_first() {
        let f = facts("h264", BitDepth::Eight, &["truehd"], 8_000_000);
        let d = evaluate(&f, &default_space_saver());
        let job = next_job(&d, &f, &SizeThresholds::default()).unwrap();
        assert_eq!(
            job.class,
            JobClass::Audio,
            "the video job is derived later, from the audio output's probe"
        );
    }

    #[test]
    fn quarantined_and_idle_decisions_produce_no_job() {
        let mut f = facts("h264", BitDepth::Eight, &["aac"], 900_000);
        assert!(
            next_job(
                &evaluate(&f, &default_space_saver()),
                &f,
                &SizeThresholds::default()
            )
            .is_none()
        );

        f.has_object_audio = true;
        assert!(
            next_job(
                &evaluate(&f, &default_space_saver()),
                &f,
                &SizeThresholds::default()
            )
            .is_none()
        );
    }

    #[test]
    fn evaluation_is_deterministic_for_fixed_inputs() {
        let f = facts("h264", BitDepth::Eight, &["truehd"], 8_000_000);
        let p = default_space_saver();
        let first = evaluate(&f, &p);
        for _ in 0..64 {
            assert_eq!(evaluate(&f, &p), first);
        }
    }

    #[test]
    fn rules_version_changes_when_the_policy_does() {
        let a = default_space_saver();
        let mut b = a.clone();
        assert_eq!(rules_version(&a), rules_version(&b));
        b.rules[0].name = "renamed".into();
        assert_ne!(
            rules_version(&a),
            rules_version(&b),
            "a policy edit must invalidate stored skip markers"
        );
    }

    #[test]
    fn explain_reports_which_rules_fired() {
        let f = facts("h264", BitDepth::Eight, &["truehd"], 8_000_000);
        let (_, trace) = evaluate_explained(&f, &default_space_saver());
        let fired: Vec<_> = trace.iter().filter(|t| t.matched).collect();
        assert!(fired.len() >= 2, "audio and video rules both fired");
        assert!(
            trace.iter().any(|t| !t.matched),
            "non-matches are shown too"
        );
    }

    #[test]
    fn an_empty_match_is_a_catch_all() {
        assert!(Match::default().matches(&facts("h264", BitDepth::Eight, &[], 1)));
    }
}

#[cfg(test)]
mod plan_builder_tests {
    use super::*;

    fn facts() -> FileFacts {
        FileFacts {
            container: "matroska".into(),
            duration_us: Some(3_600_000_000),
            size_bytes: 8_000_000_000,
            video_codec: Some("h264".into()),
            video_bit_depth: Some(BitDepth::Ten),
            audio_codecs: vec!["truehd".into()],
            audio_track_count: 3,
            subtitle_track_count: 5,
            ..FileFacts::default()
        }
    }

    /// A bare `-c:a eac3` silently keeps only the default track and drops the
    /// rest. Measured, and the single easiest way to quietly destroy a library.
    #[test]
    fn an_audio_plan_maps_every_stream_and_copies_video() {
        let f = facts();
        let d = evaluate(&f, &default_space_saver());
        let plan = encode_plan_for(&d, &f).expect("audio work is owed");

        assert_eq!(
            plan.video_codec,
            EncoderId::Copy,
            "video must not be touched"
        );
        assert!(
            plan.extra_args
                .windows(2)
                .any(|w| w[0] == "-map" && w[1] == "0"),
            "every stream must be mapped: {:?}",
            plan.extra_args
        );
    }

    /// Nothing owed means no encode. Emitting one anyway would re-encode the
    /// whole library for no reason.
    #[test]
    fn a_settled_file_yields_no_plan() {
        let f = FileFacts {
            audio_codecs: vec!["eac3".into()],
            video_codec: Some("hevc".into()),
            ..facts()
        };
        let d = evaluate(&f, &default_space_saver());
        if matches!(d.class, DecisionClass::None) {
            assert!(encode_plan_for(&d, &f).is_none());
        }
    }

    /// DV and object audio are excluded from all work. A plan here would
    /// re-encode a title that must never be re-encoded.
    #[test]
    fn a_quarantined_decision_yields_no_plan() {
        let d = Decision {
            class: DecisionClass::Quarantined,
            audio: None,
            video: None,
            reason: "dolby vision".into(),
        };
        assert!(encode_plan_for(&d, &facts()).is_none());
    }

    /// A percentage alone permits a 40-minute loss on a three-hour film. The
    /// absolute cap is what actually protects the media.
    #[test]
    fn the_duration_tolerance_is_capped_at_five_seconds() {
        let f = facts(); // one hour
        let d = evaluate(&f, &default_space_saver());
        let spec = validation_spec_for(&f, &d);

        // 0.5% of an hour is 18s, so the 5s cap must bind.
        assert_eq!(spec.max_shorter_us, 5_000_000);
    }

    /// ...but on a short file the percentage is tighter than the cap, and it
    /// is the percentage that must bind.
    #[test]
    fn the_tolerance_is_a_percentage_on_short_files() {
        let f = FileFacts {
            duration_us: Some(600_000_000), // ten minutes
            ..facts()
        };
        let d = evaluate(&f, &default_space_saver());
        let spec = validation_spec_for(&f, &d);
        assert_eq!(spec.max_shorter_us, 3_000_000, "0.5% of ten minutes");
    }

    /// Re-encoding TrueHD or Opus to EAC3 640k legitimately grows the file.
    /// Requiring shrinkage would reject every audio job and strand the video
    /// stage meant to follow it.
    #[test]
    fn an_audio_pass_is_allowed_to_grow_the_file() {
        let f = facts();
        let d = evaluate(&f, &default_space_saver());
        let spec = validation_spec_for(&f, &d);
        assert!(
            matches!(spec.size_policy, SizePolicy::MayGrow),
            "got {:?}",
            spec.size_policy
        );
    }

    #[test]
    fn a_video_pass_must_actually_save_space() {
        let d = Decision {
            class: DecisionClass::Video,
            audio: None,
            video: Some(VideoPlan {
                encoder_preference: vec![EncoderId::Libx265],
                quality: 22,
                source_depth: BitDepth::Ten,
            }),
            reason: "video".into(),
        };
        let spec = validation_spec_for(&facts(), &d);
        assert!(matches!(
            spec.size_policy,
            SizePolicy::RequireSmaller { .. }
        ));
    }

    /// Every audio and subtitle track must survive; the spec is what carries
    /// that expectation to the validator.
    #[test]
    fn the_spec_demands_every_track_survives() {
        let f = facts();
        let d = evaluate(&f, &default_space_saver());
        let spec = validation_spec_for(&f, &d);
        assert_eq!(spec.expected_audio_streams, 3);
        assert_eq!(spec.expected_subtitle_streams, 5);
    }

    /// libx265 wants yuv420p10le and NVENC wants p010le; the wrong one errors
    /// the job. Never upconvert 8-bit.
    #[test]
    fn a_video_plan_preserves_source_bit_depth() {
        for depth in [BitDepth::Eight, BitDepth::Ten] {
            let d = Decision {
                class: DecisionClass::Video,
                audio: None,
                video: Some(VideoPlan {
                    encoder_preference: vec![EncoderId::Libx265],
                    quality: 22,
                    source_depth: depth,
                }),
                reason: "video".into(),
            };
            let plan = encode_plan_for(&d, &facts()).unwrap();
            assert_eq!(
                plan.pix_fmt,
                crate::plan::pix_fmt_for(EncoderId::Libx265, depth),
                "depth {depth:?} must round-trip into the pixel format"
            );
        }
    }
}

#[cfg(test)]
mod audio_placement_tests {
    use super::*;
    use crate::capability::{Capability, Mount, Platform, TransportMode, satisfies};

    fn flac_file() -> FileFacts {
        FileFacts {
            container: "matroska".into(),
            duration_us: Some(10_000_000),
            size_bytes: 180_938,
            video_codec: Some("h264".into()),
            video_profile: Some("High".into()),
            video_bit_depth: Some(BitDepth::Eight),
            audio_codecs: vec!["flac".into()],
            audio_track_count: 1,
            ..FileFacts::default()
        }
    }

    /// `windows-rtx2070` as it actually surveys: `hevc_nvenc` and no libx264,
    /// so `classes_for` offers `Audio` and `Gpu` and withholds `Cpu`.
    fn gpu_only_node() -> Capability {
        Capability {
            classes: vec![AgentClass::Audio, AgentClass::Gpu],
            encoders: vec![EncoderId::HevcNvenc, EncoderId::Eac3, EncoderId::Ac3],
            muxers: vec![ContainerId::Matroska],
            effective_cores: 16.0,
            platform: Some(Platform::Windows),
            transport: TransportMode::Stream,
            workarea_path: "C:\\Users\\jdfalk\\tc-work".into(),
            mounts: Vec::<Mount>::new(),
            ..Capability::default()
        }
    }

    /// The bug that stopped the GPU node dead, as a test.
    ///
    /// An audio pass is `-c:v copy`. It uses no video encoder, and every agent
    /// advertises `AgentClass::Audio` unconditionally for exactly that reason.
    /// The job asked for `AgentClass::Cpu`, which an agent offers only when it
    /// has libx264 or libx265 — so audio work could land only on nodes with a
    /// software *video* encoder.
    ///
    /// `windows-rtx2070` has `hevc_nvenc` and no libx264. It advertised `Audio`
    /// and could never be handed a single audio job.
    #[test]
    fn an_audio_job_can_be_placed_on_a_node_with_no_software_video_encoder() {
        let f = flac_file();
        let d = evaluate(&f, &default_space_saver());
        let job = next_job(&d, &f, &SizeThresholds::default()).expect("audio work is owed");

        assert_eq!(job.class, JobClass::Audio);
        satisfies(&gpu_only_node(), &job.requirements)
            .expect("a GPU-only node advertises Audio and eac3; nothing here needs libx264");
    }

    /// The class asked for is the one every agent actually offers. Stated
    /// separately because the placement test above would also pass if the class
    /// requirement were dropped altogether, and the requirement is what keeps a
    /// job off a node that has withdrawn the class.
    #[test]
    fn an_audio_job_asks_for_the_audio_class() {
        let f = flac_file();
        let d = evaluate(&f, &default_space_saver());
        let job = next_job(&d, &f, &SizeThresholds::default()).expect("audio work is owed");

        assert!(
            job.requirements
                .0
                .contains(&Requirement::AgentClass(AgentClass::Audio)),
            "got {:?}",
            job.requirements
        );
        assert!(
            !job.requirements
                .0
                .contains(&Requirement::AgentClass(AgentClass::Cpu)),
            "a software video encoder is not a precondition for copying video"
        );
    }

    /// The encoder is what actually gates an audio job, and it must still bite:
    /// the class alone is universal and would place audio work anywhere.
    #[test]
    fn an_audio_job_is_still_refused_by_a_node_without_the_encoder() {
        let f = flac_file();
        let d = evaluate(&f, &default_space_saver());
        let job = next_job(&d, &f, &SizeThresholds::default()).expect("audio work is owed");

        let mut node = gpu_only_node();
        node.encoders = vec![EncoderId::HevcNvenc];
        let err = satisfies(&node, &job.requirements).expect_err("no eac3 on this node");
        assert!(err.detail.contains("eac3"), "{}", err.detail);
    }
}
