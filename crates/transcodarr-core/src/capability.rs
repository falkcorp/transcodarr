// file: crates/transcodarr-core/src/capability.rs
// version: 1.0.0
// guid: 3f81c5d9-2a64-4e07-b93a-6c05d81ef742
// last-edited: 2026-08-01
//! What an agent can do, what a job needs, and whether they match.
//!
//! This is the fix for the failure mode that motivated the whole project: a job
//! that no node can run must be *rejected once with a reason*, never queued and
//! retried forever. Requirements are declared by the plan and matched against
//! capabilities the agent advertises, so unroutable work is visible before it is
//! ever dispatched.
//!
//! `satisfies` lives here, in core, precisely so the server and the agent link
//! the same bytes. An agent re-checking its own assignment is then a genuine
//! bug detector rather than a second implementation that can drift.

use serde::{Deserialize, Serialize};

use crate::plan::{BitDepth, EncoderId};

/// A class of work an agent advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AgentClass {
    /// CPU transcode work.
    Cpu,
    /// GPU transcode work.
    Gpu,
    /// Audio-only work.
    Audio,
}

/// Which platform an agent runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Platform {
    /// Linux.
    Linux,
    /// Windows, including WSL2.
    Windows,
}

/// Container muxers a plan can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContainerId {
    /// Matroska (`.mkv`).
    Matroska,
    /// MPEG-4 (`.mp4`).
    Mp4,
}

/// A decode path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DecoderKind {
    /// Software decode. Always available.
    Software,
    /// NVIDIA NVDEC.
    Nvdec,
    /// Intel Quick Sync.
    Qsv,
    /// VA-API.
    Vaapi,
    /// Apple VideoToolbox.
    Videotoolbox,
}

/// The verdict from trial-decoding one codec on one decode path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DecoderStatus {
    /// Never trialled. Not proof of anything.
    Untested,
    /// Trial decode succeeded on the requested path.
    VerifiedOk,
    /// Trial decode *succeeded*, but ffmpeg fell back to software.
    ///
    /// This is the dangerous one, and why a boolean would not do. Turing NVDEC
    /// cannot decode 10-bit H.264: ffmpeg reports success while quietly
    /// decoding on the CPU. Treating that as hardware support hands Hi10 files
    /// to a GPU node that will crawl through them on one core.
    VerifiedSoftFallback,
    /// Trial decode failed outright.
    ///
    /// Turing NVDEC and AV1: exit 69 with roughly 1 KB of truncated output.
    VerifiedFail,
}

impl DecoderStatus {
    /// Whether this status satisfies a hardware decode requirement.
    ///
    /// **Only `VerifiedOk` does.** A soft fallback is a successful decode that
    /// did not use the hardware, so it must not satisfy a requirement that
    /// exists to select hardware. `Untested` is not evidence either — absence
    /// of a trial is not proof of capability.
    pub fn satisfies_hardware_requirement(self) -> bool {
        matches!(self, DecoderStatus::VerifiedOk)
    }
}

/// Codec, profile, depth and decode path — the key a decode verdict hangs on.
///
/// All four matter. "Can this node decode H.264?" has no single answer: yes at
/// 8-bit on NVDEC, only-in-software at 10-bit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DecoderTriple {
    /// ffmpeg codec name, e.g. `h264`, `av1`.
    pub codec: String,
    /// Codec profile, empty when it does not discriminate.
    pub profile: String,
    /// Source bit depth.
    pub bit_depth: BitDepth,
    /// Decode path being asked about.
    pub kind: DecoderKind,
}

/// A trial-decode result for one [`DecoderTriple`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecoderCapability {
    /// What was trialled.
    pub triple: DecoderTriple,
    /// How it went.
    pub status: DecoderStatus,
    /// Operator-readable evidence, e.g. the stderr line that decided it.
    pub evidence: String,
}

/// A mount an agent can see, and the canonical prefix it corresponds to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Canonical path prefix as the server names it, e.g. `/mnt/media`.
    pub canonical_prefix: String,
    /// Where this agent sees it, e.g. `/media` or `Z:\\`.
    pub local_path: String,
    /// Whether the agent can write there.
    pub writable: bool,
}

/// The full capability document an agent advertises.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Capability {
    /// Work classes this agent accepts.
    pub classes: Vec<AgentClass>,
    /// Encoders available.
    pub encoders: Vec<EncoderId>,
    /// Muxers available.
    pub muxers: Vec<ContainerId>,
    /// Trial-decode verdicts.
    pub decoders: Vec<DecoderCapability>,
    /// Cores after any cgroup quota — what the agent can actually use.
    pub effective_cores: f64,
    /// Mounts visible to this agent.
    pub mounts: Vec<Mount>,
    /// Platform.
    pub platform: Option<Platform>,
    /// Free bytes in the agent's work area at probe time.
    pub workarea_free_bytes: u64,
    /// Arbitrary operator labels, e.g. `rack=1`.
    pub labels: Vec<(String, String)>,
}

impl Capability {
    /// Look up the recorded verdict for a decode triple.
    pub fn decoder_status(&self, triple: &DecoderTriple) -> DecoderStatus {
        self.decoders
            .iter()
            .find(|d| &d.triple == triple)
            .map(|d| d.status)
            .unwrap_or(DecoderStatus::Untested)
    }

    /// A stable hash of the capability document, for drift detection.
    pub fn hash(&self) -> String {
        // Canonicalised through serde_json so field order cannot change the
        // hash between builds.
        let json = serde_json::to_string(self).unwrap_or_default();
        blake3::hash(json.as_bytes()).to_hex().to_string()
    }
}

/// One thing a job needs from whatever runs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Requirement {
    /// Agent must advertise this work class.
    AgentClass(AgentClass),
    /// Agent must have this encoder.
    Encoder(EncoderId),
    /// Agent must decode this triple *in hardware*, verified.
    Decoder(DecoderTriple),
    /// Agent must have this muxer.
    Muxer(ContainerId),
    /// Agent must have at least this many effective cores.
    MinEffectiveCores(f64),
    /// Agent's work area must have at least this many free bytes.
    MinFreeBytes(u64),
    /// Agent must see a mount covering this canonical prefix, writably.
    MountCovers(String),
    /// Agent's platform must be one of these.
    PlatformIn(Vec<Platform>),
    /// Agent must carry this label.
    LabelEquals(String, String),
}

/// An ordered, AND-ed list of requirements attached to a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Requirements(pub Vec<Requirement>);

/// The specific requirement an agent failed, with an operator-readable reason.
///
/// Carrying the *reason* is the point. "No agent available" sends an operator
/// hunting; "requires hevc_nvenc, no agent advertises it" is actionable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnmetRequirement {
    /// Which requirement was not met.
    pub requirement: Requirement,
    /// Why, in words.
    pub detail: String,
}

/// Check a capability document against a set of requirements.
///
/// Returns the *first* unmet requirement. Requirements are ordered so the
/// cheapest and most discriminating checks come first, which makes the reported
/// reason the most useful one rather than an incidental later failure.
pub fn satisfies(cap: &Capability, req: &Requirements) -> Result<(), UnmetRequirement> {
    for r in &req.0 {
        let unmet = |detail: String| UnmetRequirement {
            requirement: r.clone(),
            detail,
        };

        match r {
            Requirement::AgentClass(c) => {
                if !cap.classes.contains(c) {
                    return Err(unmet(format!(
                        "agent does not advertise class {c:?}; has {:?}",
                        cap.classes
                    )));
                }
            }
            Requirement::Encoder(e) => {
                if !cap.encoders.contains(e) {
                    return Err(unmet(format!("agent lacks encoder {}", e.as_ffmpeg())));
                }
            }
            Requirement::Decoder(t) => {
                let status = cap.decoder_status(t);
                if !status.satisfies_hardware_requirement() {
                    return Err(unmet(format!(
                        "decode of {} {:?} on {:?} is {:?}, not VerifiedOk",
                        t.codec, t.bit_depth, t.kind, status
                    )));
                }
            }
            Requirement::Muxer(m) => {
                if !cap.muxers.contains(m) {
                    return Err(unmet(format!("agent lacks muxer {m:?}")));
                }
            }
            Requirement::MinEffectiveCores(n) => {
                if cap.effective_cores < *n {
                    return Err(unmet(format!(
                        "agent has {:.1} effective cores, needs {:.1}",
                        cap.effective_cores, n
                    )));
                }
            }
            Requirement::MinFreeBytes(b) => {
                if cap.workarea_free_bytes < *b {
                    return Err(unmet(format!(
                        "work area has {} B free, needs {} B",
                        cap.workarea_free_bytes, b
                    )));
                }
            }
            Requirement::MountCovers(prefix) => {
                let covered = cap
                    .mounts
                    .iter()
                    .any(|m| prefix.starts_with(&m.canonical_prefix) && m.writable);
                if !covered {
                    return Err(unmet(format!(
                        "no writable mount covers {prefix}; agent sees {:?}",
                        cap.mounts
                            .iter()
                            .map(|m| &m.canonical_prefix)
                            .collect::<Vec<_>>()
                    )));
                }
            }
            Requirement::PlatformIn(allowed) => match cap.platform {
                Some(p) if allowed.contains(&p) => {}
                other => {
                    return Err(unmet(format!(
                        "agent platform {other:?} not in {allowed:?}"
                    )));
                }
            },
            Requirement::LabelEquals(k, v) => {
                let ok = cap.labels.iter().any(|(lk, lv)| lk == k && lv == v);
                if !ok {
                    return Err(unmet(format!("agent lacks label {k}={v}")));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turing_gpu() -> Capability {
        Capability {
            classes: vec![AgentClass::Gpu],
            encoders: vec![EncoderId::HevcNvenc],
            muxers: vec![ContainerId::Matroska],
            decoders: vec![
                DecoderCapability {
                    triple: DecoderTriple {
                        codec: "h264".into(),
                        profile: String::new(),
                        bit_depth: BitDepth::Eight,
                        kind: DecoderKind::Nvdec,
                    },
                    status: DecoderStatus::VerifiedOk,
                    evidence: "trial decode ok".into(),
                },
                DecoderCapability {
                    triple: DecoderTriple {
                        codec: "h264".into(),
                        profile: "High 10".into(),
                        bit_depth: BitDepth::Ten,
                        kind: DecoderKind::Nvdec,
                    },
                    // Measured: succeeds, but on the CPU.
                    status: DecoderStatus::VerifiedSoftFallback,
                    evidence: "hwaccel initialisation returned error".into(),
                },
                DecoderCapability {
                    triple: DecoderTriple {
                        codec: "av1".into(),
                        profile: String::new(),
                        bit_depth: BitDepth::Eight,
                        kind: DecoderKind::Nvdec,
                    },
                    // Measured: ffmpeg exit 69, ~1 KB truncated output.
                    status: DecoderStatus::VerifiedFail,
                    evidence: "exit 69, 1 KiB output".into(),
                },
            ],
            effective_cores: 8.0,
            mounts: vec![Mount {
                canonical_prefix: "/mnt/media".into(),
                local_path: "Z:\\".into(),
                writable: true,
            }],
            platform: Some(Platform::Windows),
            workarea_free_bytes: 500_000_000_000,
            labels: vec![],
        }
    }

    fn req_decode(codec: &str, profile: &str, depth: BitDepth) -> Requirements {
        Requirements(vec![Requirement::Decoder(DecoderTriple {
            codec: codec.into(),
            profile: profile.into(),
            bit_depth: depth,
            kind: DecoderKind::Nvdec,
        })])
    }

    /// The milestone assertion. A soft fallback is a *successful* decode that
    /// did not use the hardware; treating it as support is how Hi10 files end
    /// up crawling on the GPU box.
    #[test]
    fn soft_fallback_does_not_satisfy_a_decoder_requirement() {
        assert!(!DecoderStatus::VerifiedSoftFallback.satisfies_hardware_requirement());

        let err = satisfies(&turing_gpu(), &req_decode("h264", "High 10", BitDepth::Ten))
            .expect_err("Hi10 on Turing NVDEC must not match");
        assert!(
            err.detail.contains("VerifiedSoftFallback"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn untested_is_not_evidence_of_capability() {
        assert!(!DecoderStatus::Untested.satisfies_hardware_requirement());
        let err = satisfies(&turing_gpu(), &req_decode("vp9", "", BitDepth::Eight))
            .expect_err("never-trialled must not match");
        assert!(err.detail.contains("Untested"));
    }

    #[test]
    fn av1_hard_failure_is_rejected_before_dispatch() {
        let err = satisfies(&turing_gpu(), &req_decode("av1", "", BitDepth::Eight))
            .expect_err("AV1 on Turing NVDEC is a hard failure");
        assert!(err.detail.contains("VerifiedFail"));
    }

    #[test]
    fn verified_ok_matches() {
        assert!(satisfies(&turing_gpu(), &req_decode("h264", "", BitDepth::Eight)).is_ok());
    }

    #[test]
    fn a_missing_encoder_names_itself_in_the_reason() {
        let err = satisfies(
            &turing_gpu(),
            &Requirements(vec![Requirement::Encoder(EncoderId::Libx265)]),
        )
        .expect_err("GPU node has no libx265");
        assert!(err.detail.contains("libx265"), "{}", err.detail);
    }

    #[test]
    fn wrong_class_is_caught() {
        let err = satisfies(
            &turing_gpu(),
            &Requirements(vec![Requirement::AgentClass(AgentClass::Cpu)]),
        )
        .expect_err("GPU-only node");
        assert!(err.detail.contains("Cpu"));
    }

    #[test]
    fn a_non_writable_mount_does_not_cover_a_path() {
        let mut cap = turing_gpu();
        cap.mounts[0].writable = false;
        let err = satisfies(
            &cap,
            &Requirements(vec![Requirement::MountCovers("/mnt/media/tv".into())]),
        )
        .expect_err("read-only mount cannot receive a commit");
        assert!(err.detail.contains("no writable mount"));
    }

    #[test]
    fn insufficient_cores_and_space_are_reported_numerically() {
        let cap = turing_gpu();
        let e1 = satisfies(
            &cap,
            &Requirements(vec![Requirement::MinEffectiveCores(16.0)]),
        )
        .unwrap_err();
        assert!(e1.detail.contains("8.0"));

        let e2 = satisfies(
            &cap,
            &Requirements(vec![Requirement::MinFreeBytes(u64::MAX)]),
        )
        .unwrap_err();
        assert!(e2.detail.contains("needs"));
    }

    #[test]
    fn the_first_unmet_requirement_is_the_one_reported() {
        let reqs = Requirements(vec![
            Requirement::AgentClass(AgentClass::Cpu), // fails first
            Requirement::Encoder(EncoderId::Libx265), // would also fail
        ]);
        let err = satisfies(&turing_gpu(), &reqs).unwrap_err();
        assert_eq!(err.requirement, Requirement::AgentClass(AgentClass::Cpu));
    }

    #[test]
    fn an_empty_requirement_set_is_satisfied_by_anything() {
        assert!(satisfies(&Capability::default(), &Requirements::default()).is_ok());
    }

    #[test]
    fn capability_hash_changes_when_capability_does() {
        let a = turing_gpu();
        let mut b = a.clone();
        assert_eq!(a.hash(), b.hash());
        b.encoders.push(EncoderId::Libx265);
        assert_ne!(a.hash(), b.hash(), "drift must be detectable");
    }
}
