// file: crates/transcodarr-proto/src/convert.rs
// version: 1.0.0
// guid: 6d92f108-4b73-4a15-8c2e-01fb7d3a6592
// last-edited: 2026-08-04
//! The boundary between generated types and domain types.
//!
//! Everything here exists to stop one class of bug: a value this build does not
//! understand becoming a domain fact by default.
//!
//! proto3 gives an enum no way to say "unknown". A number outside the declared
//! set decodes to the zero variant, silently, and prost hands it over as a
//! plain `i32`. The highest-consequence case is `DecoderStatus`: if
//! `DS_VERIFIED_SOFT_FALLBACK` ever decayed to `VerifiedOk` across the wire the
//! Turing Hi10 trap comes straight back — ffmpeg reports success while decoding
//! on the CPU, and the scheduler hands 10-bit H.264 to a GPU node that will
//! crawl through it on one core.
//!
//! So every enum-like field is converted through a function that returns
//! [`ProtoError`] on anything unrecognised, and no conversion here falls back
//! to a default.
//!
//! The two directions are not symmetric. Converting *out* of core types needs a
//! wildcard arm, because the core enums are `#[non_exhaustive]` and this is a
//! downstream crate — the compiler will not force this file to be updated when
//! a variant is added. That arm returns an error rather than a placeholder, so
//! a variant this build cannot name is refused at the boundary instead of being
//! sent as something it is not.

use transcodarr_core::capability::{
    AgentClass, Capability, DecoderCapability, DecoderKind, DecoderStatus, DecoderTriple, Mount,
    Platform,
};
use transcodarr_core::plan::{BitDepth, EncoderId};

use crate::handshake::AgentIdentity;
use crate::message::{CommitPhase, LiveIntent};
use crate::{ProtoError, pb};

/// Refuse a wire value this build does not recognise.
fn unrecognised(field: &'static str, value: impl std::fmt::Display) -> ProtoError {
    ProtoError::Unrecognised {
        field,
        value: value.to_string(),
    }
}

// ---------------------------------------------------------------- identity --

impl From<AgentIdentity> for pb::AgentIdentity {
    fn from(v: AgentIdentity) -> Self {
        Self {
            agent_id: v.agent_id,
            agent_uid: v.agent_uid,
            boot_id: v.boot_id,
            agent_version: v.agent_version,
            proto_version: v.proto_version,
        }
    }
}

impl TryFrom<pb::AgentIdentity> for AgentIdentity {
    type Error = ProtoError;

    /// All three identifiers are required.
    ///
    /// An absent `boot_id` arrives as an empty string rather than as an
    /// absence, and an empty `boot_id` compares equal to the last empty one —
    /// which is the fencing rule's input. Two different process instances both
    /// sending nothing would look like a reconnect, and the second would
    /// inherit an epoch that should have been bumped.
    fn try_from(v: pb::AgentIdentity) -> Result<Self, Self::Error> {
        if v.agent_id.is_empty() {
            return Err(ProtoError::Missing("agent_id"));
        }
        if v.agent_uid.is_empty() {
            return Err(ProtoError::Missing("agent_uid"));
        }
        if v.boot_id.is_empty() {
            return Err(ProtoError::Missing("boot_id"));
        }
        Ok(Self {
            agent_id: v.agent_id,
            agent_uid: v.agent_uid,
            boot_id: v.boot_id,
            agent_version: v.agent_version,
            proto_version: v.proto_version,
        })
    }
}

// ----------------------------------------------------------------- decoder --

/// A decode path, refusing anything outside the declared set.
pub fn decoder_kind_from_pb(v: i32) -> Result<DecoderKind, ProtoError> {
    Ok(match pb::DecoderKind::try_from(v) {
        Ok(pb::DecoderKind::DkSoftware) => DecoderKind::Software,
        Ok(pb::DecoderKind::DkNvdec) => DecoderKind::Nvdec,
        Ok(pb::DecoderKind::DkQsv) => DecoderKind::Qsv,
        Ok(pb::DecoderKind::DkVaapi) => DecoderKind::Vaapi,
        Ok(pb::DecoderKind::DkVideotoolbox) => DecoderKind::Videotoolbox,
        Err(_) => return Err(unrecognised("decoder_kind", v)),
    })
}

/// The wire form of a decode path.
pub fn decoder_kind_to_pb(v: DecoderKind) -> Result<pb::DecoderKind, ProtoError> {
    Ok(match v {
        DecoderKind::Software => pb::DecoderKind::DkSoftware,
        DecoderKind::Nvdec => pb::DecoderKind::DkNvdec,
        DecoderKind::Qsv => pb::DecoderKind::DkQsv,
        DecoderKind::Vaapi => pb::DecoderKind::DkVaapi,
        DecoderKind::Videotoolbox => pb::DecoderKind::DkVideotoolbox,
        other => return Err(unrecognised("decoder_kind", format!("{other:?}"))),
    })
}

/// A trial-decode verdict.
///
/// The one conversion in this file worth reading twice. A soft fallback must
/// survive as a soft fallback: it is a *successful* decode that did not use the
/// hardware, and treating it as plain success is how 10-bit H.264 ends up on a
/// node that cannot decode it.
pub fn decoder_status_from_pb(v: i32) -> Result<DecoderStatus, ProtoError> {
    Ok(match pb::DecoderStatus::try_from(v) {
        Ok(pb::DecoderStatus::DsUntested) => DecoderStatus::Untested,
        Ok(pb::DecoderStatus::DsVerifiedOk) => DecoderStatus::VerifiedOk,
        Ok(pb::DecoderStatus::DsVerifiedSoftFallback) => DecoderStatus::VerifiedSoftFallback,
        Ok(pb::DecoderStatus::DsVerifiedFail) => DecoderStatus::VerifiedFail,
        Err(_) => return Err(unrecognised("decoder_status", v)),
    })
}

/// The wire form of a trial-decode verdict.
pub fn decoder_status_to_pb(v: DecoderStatus) -> Result<pb::DecoderStatus, ProtoError> {
    Ok(match v {
        DecoderStatus::Untested => pb::DecoderStatus::DsUntested,
        DecoderStatus::VerifiedOk => pb::DecoderStatus::DsVerifiedOk,
        DecoderStatus::VerifiedSoftFallback => pb::DecoderStatus::DsVerifiedSoftFallback,
        DecoderStatus::VerifiedFail => pb::DecoderStatus::DsVerifiedFail,
        other => return Err(unrecognised("decoder_status", format!("{other:?}"))),
    })
}

impl TryFrom<pb::DecoderTriple> for DecoderTriple {
    type Error = ProtoError;

    fn try_from(v: pb::DecoderTriple) -> Result<Self, Self::Error> {
        let bits = u8::try_from(v.bit_depth).map_err(|_| unrecognised("bit_depth", v.bit_depth))?;
        Ok(Self {
            codec: v.codec,
            profile: v.profile,
            bit_depth: BitDepth::from_bits(bits)
                .ok_or_else(|| unrecognised("bit_depth", v.bit_depth))?,
            kind: decoder_kind_from_pb(v.kind)?,
        })
    }
}

impl TryFrom<DecoderTriple> for pb::DecoderTriple {
    type Error = ProtoError;

    fn try_from(v: DecoderTriple) -> Result<Self, Self::Error> {
        Ok(Self {
            codec: v.codec,
            profile: v.profile,
            bit_depth: u32::from(v.bit_depth.bits()),
            kind: decoder_kind_to_pb(v.kind)? as i32,
        })
    }
}

impl TryFrom<pb::DecoderCapability> for DecoderCapability {
    type Error = ProtoError;

    fn try_from(v: pb::DecoderCapability) -> Result<Self, Self::Error> {
        Ok(Self {
            triple: v
                .triple
                .ok_or(ProtoError::Missing("decoder_capability.triple"))?
                .try_into()?,
            status: decoder_status_from_pb(v.status)?,
            evidence: v.evidence,
        })
    }
}

impl TryFrom<DecoderCapability> for pb::DecoderCapability {
    type Error = ProtoError;

    fn try_from(v: DecoderCapability) -> Result<Self, Self::Error> {
        Ok(Self {
            triple: Some(v.triple.try_into()?),
            status: decoder_status_to_pb(v.status)? as i32,
            evidence: v.evidence,
            probed_at_unix: 0,
        })
    }
}

// ------------------------------------------------------------------ mounts --

impl From<pb::Mount> for Mount {
    /// The wire `Mount` carries more than the domain one needs.
    ///
    /// `pool_id`, `free_bytes` and the rename-probe verdict are server-side
    /// concerns. Pool identity in particular is *assigned* by the server and
    /// merely echoed by the agent, because two agents see one pool at different
    /// device numbers and budgeting per-mount would triple-count it.
    fn from(v: pb::Mount) -> Self {
        Self {
            canonical_prefix: v.canonical_prefix,
            local_path: v.local_path,
            writable: v.writable,
        }
    }
}

impl From<Mount> for pb::Mount {
    fn from(v: Mount) -> Self {
        Self {
            local_path: v.local_path,
            canonical_prefix: v.canonical_prefix,
            writable: v.writable,
            ..Default::default()
        }
    }
}

// ------------------------------------------------------------ vocabularies --

/// Work classes, by their wire spelling.
fn agent_class_from_str(s: &str) -> Result<AgentClass, ProtoError> {
    Ok(match s {
        "cpu" => AgentClass::Cpu,
        "gpu" => AgentClass::Gpu,
        "audio" => AgentClass::Audio,
        other => return Err(unrecognised("classes", other)),
    })
}

fn agent_class_to_str(v: AgentClass) -> Result<&'static str, ProtoError> {
    Ok(match v {
        AgentClass::Cpu => "cpu",
        AgentClass::Gpu => "gpu",
        AgentClass::Audio => "audio",
        other => return Err(unrecognised("classes", format!("{other:?}"))),
    })
}

/// Encoders are named as ffmpeg names them, which is how an agent reports them.
fn encoder_from_str(s: &str) -> Result<EncoderId, ProtoError> {
    Ok(match s {
        "hevc_nvenc" => EncoderId::HevcNvenc,
        "libx265" => EncoderId::Libx265,
        "libx264" => EncoderId::Libx264,
        "eac3" => EncoderId::Eac3,
        "ac3" => EncoderId::Ac3,
        "aac" => EncoderId::Aac,
        "copy" => EncoderId::Copy,
        other => return Err(unrecognised("encoders", other)),
    })
}

fn platform_from_str(s: &str) -> Result<Platform, ProtoError> {
    Ok(match s {
        "linux" => Platform::Linux,
        "windows" => Platform::Windows,
        other => return Err(unrecognised("platform", other)),
    })
}

fn platform_to_str(v: Platform) -> Result<&'static str, ProtoError> {
    Ok(match v {
        Platform::Linux => "linux",
        Platform::Windows => "windows",
        other => return Err(unrecognised("platform", format!("{other:?}"))),
    })
}

// -------------------------------------------------------------- capability --

impl TryFrom<pb::Capability> for Capability {
    type Error = ProtoError;

    /// An agent's advertised capability.
    ///
    /// Unknown encoders are **skipped, not refused**, and the asymmetry is
    /// deliberate. An ffmpeg build lists hundreds of encoders this scheduler
    /// has no opinion about; refusing the whole document because one of them is
    /// unfamiliar would keep a working node out of the fleet over an encoder
    /// nobody was going to use. An unknown *class* or *decode verdict* is a
    /// different matter — those are the values dispatch decisions are made
    /// from, so they are refused.
    fn try_from(v: pb::Capability) -> Result<Self, Self::Error> {
        let classes = v
            .classes
            .iter()
            .map(|c| agent_class_from_str(c))
            .collect::<Result<Vec<_>, _>>()?;

        let decoders = v
            .decoders
            .into_iter()
            .map(DecoderCapability::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        let platform = if v.platform.is_empty() {
            None
        } else {
            Some(platform_from_str(&v.platform)?)
        };

        Ok(Self {
            classes,
            encoders: v
                .encoders
                .iter()
                .filter_map(|e| encoder_from_str(e).ok())
                .collect(),
            muxers: Vec::new(),
            decoders,
            effective_cores: v.effective_cores,
            mounts: v.mounts.into_iter().map(Mount::from).collect(),
            platform,
            workarea_free_bytes: v.workarea_free_bytes,
            labels: v.labels.into_iter().collect(),
        })
    }
}

impl TryFrom<Capability> for pb::Capability {
    type Error = ProtoError;

    fn try_from(v: Capability) -> Result<Self, Self::Error> {
        // Computed from the document rather than carried alongside it: the hash
        // is what registration diffs against, and one the agent asserted could
        // disagree with what it actually sent.
        let capability_hash = v.hash();
        Ok(Self {
            platform: match v.platform {
                Some(p) => platform_to_str(p)?.to_string(),
                None => String::new(),
            },
            effective_cores: v.effective_cores,
            classes: v
                .classes
                .iter()
                .map(|c| agent_class_to_str(*c).map(str::to_string))
                .collect::<Result<Vec<_>, _>>()?,
            encoders: v
                .encoders
                .iter()
                .map(|e| e.as_ffmpeg().to_string())
                .collect(),
            decoders: v
                .decoders
                .into_iter()
                .map(pb::DecoderCapability::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            mounts: v.mounts.into_iter().map(pb::Mount::from).collect(),
            workarea_free_bytes: v.workarea_free_bytes,
            capability_hash,
            labels: v.labels.into_iter().collect(),
            ..Default::default()
        })
    }
}

// ------------------------------------------------------------ live intents --

impl TryFrom<pb::LiveIntent> for LiveIntent {
    type Error = ProtoError;

    fn try_from(v: pb::LiveIntent) -> Result<Self, Self::Error> {
        Ok(Self {
            job_id: v.job_id,
            attempt: i64::from(v.attempt),
            fencing_epoch: i64::try_from(v.fencing_epoch)
                .map_err(|_| unrecognised("fencing_epoch", v.fencing_epoch))?,
            phase: CommitPhase::parse(&v.phase)?,
            temp_path: v.temp_path,
            final_path: v.final_path,
            trash_path: v.trash_path,
        })
    }
}

impl TryFrom<LiveIntent> for pb::LiveIntent {
    type Error = ProtoError;

    fn try_from(v: LiveIntent) -> Result<Self, Self::Error> {
        Ok(Self {
            job_id: v.job_id,
            attempt: u32::try_from(v.attempt).map_err(|_| unrecognised("attempt", v.attempt))?,
            fencing_epoch: u64::try_from(v.fencing_epoch)
                .map_err(|_| unrecognised("fencing_epoch", v.fencing_epoch))?,
            phase: v.phase.as_str().to_string(),
            temp_path: v.temp_path,
            final_path: v.final_path,
            trash_path: v.trash_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triple() -> DecoderTriple {
        DecoderTriple {
            codec: "h264".into(),
            profile: "High 10".into(),
            bit_depth: BitDepth::Ten,
            kind: DecoderKind::Nvdec,
        }
    }

    /// The conversion this file exists for. A soft fallback that decays to
    /// `VerifiedOk` anywhere on the round trip reintroduces the Turing Hi10
    /// trap: ffmpeg reports success while decoding on the CPU, and the
    /// scheduler hands 10-bit H.264 to a node that cannot decode it.
    #[test]
    fn a_soft_fallback_survives_the_round_trip_as_a_soft_fallback() {
        let core = DecoderCapability {
            triple: triple(),
            status: DecoderStatus::VerifiedSoftFallback,
            evidence: "nvdec unavailable for high10, using software".into(),
        };
        let wire = pb::DecoderCapability::try_from(core.clone()).unwrap();
        let back = DecoderCapability::try_from(wire).unwrap();
        assert_eq!(back.status, DecoderStatus::VerifiedSoftFallback);
        assert!(!back.status.satisfies_hardware_requirement());
        assert_eq!(back, core);
    }

    /// proto3 decodes an unknown enum number to the zero variant. Left alone, a
    /// verdict invented by a newer agent would arrive as `Untested` — which is
    /// a claim, not the absence of one.
    #[test]
    fn an_enum_number_outside_the_declared_set_is_refused_not_defaulted() {
        let e = decoder_status_from_pb(97).unwrap_err();
        assert!(
            matches!(
                e,
                ProtoError::Unrecognised {
                    field: "decoder_status",
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(decoder_kind_from_pb(97).is_err());
    }

    /// Every declared value survives, so the refusal above cannot be passing by
    /// rejecting everything.
    #[test]
    fn every_declared_status_converts_both_ways() {
        for s in [
            DecoderStatus::Untested,
            DecoderStatus::VerifiedOk,
            DecoderStatus::VerifiedSoftFallback,
            DecoderStatus::VerifiedFail,
        ] {
            let wire = decoder_status_to_pb(s).unwrap();
            assert_eq!(decoder_status_from_pb(wire as i32).unwrap(), s);
        }
        for k in [
            DecoderKind::Software,
            DecoderKind::Nvdec,
            DecoderKind::Qsv,
            DecoderKind::Vaapi,
            DecoderKind::Videotoolbox,
        ] {
            let wire = decoder_kind_to_pb(k).unwrap();
            assert_eq!(decoder_kind_from_pb(wire as i32).unwrap(), k);
        }
    }

    /// An absent `boot_id` arrives as an empty string, and an empty one
    /// compares equal to the last empty one — making two different process
    /// instances look like a reconnect, so the second inherits an epoch that
    /// should have been bumped.
    #[test]
    fn an_identity_missing_its_boot_id_is_refused() {
        let wire = pb::AgentIdentity {
            agent_id: "u1".into(),
            agent_uid: "uid-1".into(),
            boot_id: String::new(),
            agent_version: "1.0.0".into(),
            proto_version: 1,
        };
        assert!(matches!(
            AgentIdentity::try_from(wire).unwrap_err(),
            ProtoError::Missing("boot_id")
        ));
    }

    #[test]
    fn an_identity_round_trips() {
        let core = AgentIdentity {
            agent_id: "u1".into(),
            agent_uid: "uid-1".into(),
            boot_id: "boot-a".into(),
            agent_version: "1.0.0".into(),
            proto_version: 1,
        };
        let back = AgentIdentity::try_from(pb::AgentIdentity::from(core.clone())).unwrap();
        assert_eq!(back, core);
    }

    /// An unfamiliar *encoder* must not keep a working node out of the fleet;
    /// an unfamiliar *class* must, because dispatch decisions are made from it.
    #[test]
    fn an_unknown_encoder_is_skipped_but_an_unknown_class_is_refused() {
        let wire = pb::Capability {
            classes: vec!["audio".into()],
            encoders: vec!["eac3".into(), "libaom-av1".into()],
            effective_cores: 38.0,
            ..Default::default()
        };
        let core = Capability::try_from(wire).unwrap();
        assert_eq!(core.encoders, vec![EncoderId::Eac3]);

        let bad = pb::Capability {
            classes: vec!["quantum".into()],
            ..Default::default()
        };
        assert!(Capability::try_from(bad).is_err());
    }

    #[test]
    fn a_capability_round_trips_through_the_wire() {
        let core = Capability {
            classes: vec![AgentClass::Audio, AgentClass::Cpu],
            encoders: vec![EncoderId::Eac3, EncoderId::Copy],
            muxers: Vec::new(),
            decoders: vec![DecoderCapability {
                triple: triple(),
                status: DecoderStatus::VerifiedFail,
                evidence: "exit 69".into(),
            }],
            effective_cores: 38.0,
            mounts: vec![Mount {
                canonical_prefix: "/mnt/media".into(),
                local_path: "/media".into(),
                writable: true,
            }],
            platform: Some(Platform::Linux),
            workarea_free_bytes: 1 << 40,
            labels: vec![("rack".into(), "1".into())],
        };
        let wire = pb::Capability::try_from(core.clone()).unwrap();
        let back = Capability::try_from(wire).unwrap();
        assert_eq!(back, core);
    }

    /// The hash is what registration diffs against, so it is computed from the
    /// document rather than taken on the agent's word.
    #[test]
    fn the_wire_capability_carries_the_hash_of_the_document_it_came_from() {
        let core = Capability {
            classes: vec![AgentClass::Audio],
            effective_cores: 4.0,
            ..Default::default()
        };
        let wire = pb::Capability::try_from(core.clone()).unwrap();
        assert_eq!(wire.capability_hash, core.hash());
    }

    #[test]
    fn a_live_intent_round_trips() {
        let core = LiveIntent {
            job_id: "j1".into(),
            attempt: 2,
            fencing_epoch: 7,
            phase: CommitPhase::Retired,
            temp_path: "/w/j1.partial.mkv".into(),
            final_path: "/mnt/tv/a.mkv".into(),
            trash_path: "/t/a.mkv".into(),
        };
        let wire = pb::LiveIntent::try_from(core.clone()).unwrap();
        let back = LiveIntent::try_from(wire).unwrap();
        assert_eq!(back, core);
    }

    /// A phase spelled by a newer peer is refused rather than guessed at.
    #[test]
    fn a_live_intent_with_an_unknown_phase_is_refused() {
        let wire = pb::LiveIntent {
            job_id: "j1".into(),
            phase: "teleported".into(),
            ..Default::default()
        };
        assert!(LiveIntent::try_from(wire).is_err());
    }
}
