// file: crates/transcodarr-core/src/policy.rs
// version: 1.0.0
// guid: 2d8f47a1-0c96-4b53-89e7-f14b6a03d752
// last-edited: 2026-08-02
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
use crate::plan::{BitDepth, EncoderId};

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
                    Requirement::AgentClass(AgentClass::Cpu),
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

            // A hardware *encoder* implies nothing about the decoder. Asking for
            // the decode path explicitly is what keeps AV1 and Hi10 off the
            // Turing card: the requirement fails to match rather than the job
            // failing at runtime with a truncated output.
            if let Some(codec) = &facts.video_codec {
                reqs.push(Requirement::Decoder(DecoderTriple {
                    codec: codec.clone(),
                    profile: facts.video_profile.clone().unwrap_or_default(),
                    bit_depth: plan.source_depth,
                    kind: if gpu {
                        DecoderKind::Nvdec
                    } else {
                        DecoderKind::Software
                    },
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
            labels: vec![],
        };
        assert!(
            satisfies(&gpu, &job.requirements).is_err(),
            "AV1 must not route to the Turing node"
        );
    }

    /// A video job asks for its decode path explicitly. A hardware encoder
    /// implies nothing about the decoder, which is the gap Hi10 falls through.
    #[test]
    fn a_gpu_video_job_requires_a_verified_hardware_decode() {
        let f = facts("h264", BitDepth::Eight, &["aac"], 8_000_000);
        let d = evaluate(&f, &default_space_saver());
        let job = next_job(&d, &f, &SizeThresholds::default()).unwrap();
        assert!(
            job.requirements
                .0
                .iter()
                .any(|r| matches!(r, Requirement::Decoder(t) if t.kind == DecoderKind::Nvdec))
        );
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
