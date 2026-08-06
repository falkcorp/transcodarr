// file: crates/transcodarr-agent/src/survey.rs
// version: 1.0.0
// guid: 3d92b0a7-5c14-4b86-9e02-7fa3b1d6485c
// last-edited: 2026-08-06
//! What this machine can actually do, as a capability document.
//!
//! Every value here is *measured*, never assumed, and the difference matters at
//! both ends of the range:
//!
//! - **Encoders and muxers come from `ffmpeg -encoders`, not from a list.** A
//!   build without `libx265` that claims it would be handed x265 work and fail
//!   every job it was given, one hour at a time.
//! - **Cores come from the cgroup quota, not from `nproc`.** A delegated slice
//!   with `cpu.max 3800000 100000` has 38 cores available on a 48-core box, and
//!   scheduling against the 48 is how a machine ends up at load 127 — measured,
//!   on this fleet, on 2026-07-30.
//! - **A mount is only writable if the rename probe says so.** `RP_UNTESTED`
//!   grants nothing: the server refuses `commit_eligible` unless every mount
//!   passed, because `rename(2)` is atomic only within one filesystem and the
//!   whole commit ritual depends on it.
//!
//! ## What the hash does and does not cover
//!
//! `capability_hash` is computed over the *document* — classes, encoders,
//! decoders, mounts, cores — by the conversion boundary. It deliberately does
//! not cover free bytes or the rename verdict, which are enriched onto the wire
//! message afterwards. Free space changes every second; folding it into the
//! hash would make every registration look like an agent whose ffmpeg was
//! upgraded underneath it, and bury the one that matters.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use transcodarr_core::capability::{AgentClass, Capability, ContainerId, Mount, Platform};
use transcodarr_core::plan::EncoderId;
use transcodarr_proto::pb;

use crate::AgentError;
use crate::preflight;

/// Every encoder this build knows how to ask for.
const KNOWN_ENCODERS: [EncoderId; 6] = [
    EncoderId::HevcNvenc,
    EncoderId::Libx265,
    EncoderId::Libx264,
    EncoderId::Eac3,
    EncoderId::Ac3,
    EncoderId::Aac,
];

/// A mount this agent offers, as the operator described it.
#[derive(Debug, Clone)]
pub struct MountSpec {
    /// What the server calls it.
    pub canonical_prefix: String,
    /// Where this agent sees it.
    pub local_path: String,
}

/// How to survey this machine.
#[derive(Debug, Clone)]
pub struct SurveyConfig {
    /// The ffmpeg binary to interrogate.
    pub ffmpeg: String,
    /// The ffprobe binary to interrogate.
    pub ffprobe: String,
    /// Where this agent stages output.
    pub work_dir: String,
    /// The mounts it offers.
    pub mounts: Vec<MountSpec>,
    /// Operator labels, e.g. `rack=1`.
    pub labels: Vec<(String, String)>,
}

/// Survey this machine and produce the document to register with.
///
/// Slow by the standards of a startup path — it runs ffmpeg twice and a rename
/// probe per mount — and that is the right trade. Every one of those probes
/// replaces an assumption that would otherwise be discovered as a failed job.
pub fn survey(config: &SurveyConfig) -> Result<pb::Capability, AgentError> {
    let encoders = available_encoders(&config.ffmpeg);
    let muxers = available_muxers(&config.ffmpeg);

    let document = Capability {
        classes: classes_for(&encoders),
        encoders: encoders.clone(),
        muxers,
        // Trial decodes are Phase 5. An empty list means every hardware-decode
        // requirement goes unmet, which is the safe direction: `DS_UNTESTED`
        // must never satisfy one, because the Turing Hi10 failure exits zero
        // having silently decoded on the CPU.
        decoders: Vec::new(),
        effective_cores: effective_cores(),
        mounts: config
            .mounts
            .iter()
            .map(|m| Mount {
                canonical_prefix: m.canonical_prefix.clone(),
                local_path: m.local_path.clone(),
                writable: Path::new(&m.local_path).is_dir(),
            })
            .collect(),
        platform: platform(),
        workarea_free_bytes: free_bytes(Path::new(&config.work_dir)),
        labels: config.labels.clone(),
    };

    let muxer_names = muxer_names(&document.muxers);
    let mut wire: pb::Capability =
        document
            .try_into()
            .map_err(|e: transcodarr_proto::ProtoError| AgentError::Probe {
                path: "capability".into(),
                reason: e.to_string(),
            })?;

    // Enriched after the hash is computed. See the module documentation.
    wire.ffmpeg_version = tool_version(&config.ffmpeg);
    wire.ffprobe_version = tool_version(&config.ffprobe);
    wire.physical_cores = physical_cores();
    wire.muxers = muxer_names;
    for mount in &mut wire.mounts {
        mount.free_bytes = free_bytes(Path::new(&mount.local_path));
        mount.rename_probe = rename_verdict(Path::new(&mount.local_path)) as i32;
    }

    Ok(wire)
}

/// Which work classes this agent will accept.
///
/// Audio-only work needs no video encoder at all — it is `-c:v copy` — so an
/// agent always offers it. The others are offered only if the encoder they
/// require is actually present.
fn classes_for(encoders: &[EncoderId]) -> Vec<AgentClass> {
    let mut classes = vec![AgentClass::Audio];
    if encoders.contains(&EncoderId::Libx265) || encoders.contains(&EncoderId::Libx264) {
        classes.push(AgentClass::Cpu);
    }
    if encoders.contains(&EncoderId::HevcNvenc) {
        classes.push(AgentClass::Gpu);
    }
    classes
}

/// Encoders this ffmpeg actually has.
fn available_encoders(ffmpeg: &str) -> Vec<EncoderId> {
    let listing = tool_output(ffmpeg, &["-hide_banner", "-encoders"]).unwrap_or_default();
    KNOWN_ENCODERS
        .iter()
        .copied()
        .filter(|e| lists_name(&listing, e.as_ffmpeg()))
        .collect()
}

/// Muxers this ffmpeg actually has.
fn available_muxers(ffmpeg: &str) -> Vec<ContainerId> {
    let listing = tool_output(ffmpeg, &["-hide_banner", "-muxers"]).unwrap_or_default();
    let mut out = Vec::new();
    if lists_name(&listing, "matroska") {
        out.push(ContainerId::Matroska);
    }
    if lists_name(&listing, "mp4") {
        out.push(ContainerId::Mp4);
    }
    out
}

/// The wire spelling of the muxers, which the domain type does not carry.
fn muxer_names(muxers: &[ContainerId]) -> Vec<String> {
    muxers
        .iter()
        .map(|m| match m {
            ContainerId::Matroska => "matroska".to_string(),
            ContainerId::Mp4 => "mp4".to_string(),
            other => format!("{other:?}").to_lowercase(),
        })
        .collect()
}

/// Whether an ffmpeg listing names this codec.
///
/// Matched as a whole word in the name column. A substring test would find
/// `aac` inside `aac_at` and `libfdk_aac`, and report an encoder this build
/// cannot actually invoke by that name.
fn lists_name(listing: &str, name: &str) -> bool {
    listing.lines().any(|line| {
        // ffmpeg prints " V..... libx265   libx265 H.265 ..." — flags, then the
        // name, then the description.
        line.split_whitespace().nth(1) == Some(name)
    })
}

/// The first line of `-version`, which carries the build identity.
fn tool_version(bin: &str) -> String {
    tool_output(bin, &["-version"])
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_default()
}

/// Run a tool and capture stdout, or `None` if it could not be run.
fn tool_output(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Cores after any cgroup quota.
///
/// `cpu.max` is what actually binds inside a delegated slice; `nproc` reports
/// the machine and is the number that produced load 127 on a box that was only
/// ever allowed 38 cores.
fn effective_cores() -> f64 {
    if let Some(quota) = cgroup_quota() {
        return quota;
    }
    std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0)
}

/// cgroup v2 `cpu.max`, as a core count.
fn cgroup_quota() -> Option<f64> {
    let raw = std::fs::read_to_string("/sys/fs/cgroup/cpu.max").ok()?;
    let mut parts = raw.split_whitespace();
    let quota = parts.next()?;
    let period: f64 = parts.next()?.parse().ok()?;
    // "max" means unlimited, which is the absence of a quota rather than a
    // quota of zero — reporting zero cores would make the agent ineligible for
    // everything.
    if quota == "max" || period <= 0.0 {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    (quota > 0.0).then_some(quota / period)
}

/// How many cores the machine has, quota or no quota.
fn physical_cores() -> u32 {
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(0))
        .unwrap_or(0)
}

/// Free bytes on the filesystem holding `path`.
///
/// Read from `df` rather than `statvfs` so the agent keeps its dependency list
/// short — it has to stay copyable to the Windows node, and pulling in `libc`
/// for one number is a poor trade. An unreadable answer is zero, which reads as
/// "no room" and makes the agent ineligible rather than optimistic.
fn free_bytes(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    let out = Command::new("df")
        .arg("-Pk")
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().nth(3))
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb.saturating_mul(1024))
        .unwrap_or(0)
}

/// Run the Phase 0 rename probe against a mount.
///
/// `RP_UNTESTED` is not a default to fall back to comfortably: the server
/// refuses `commit_eligible` unless *every* mount reports
/// `RP_ATOMIC_VERIFIED`, so a probe that could not run correctly costs this
/// agent the right to install its own output — which is the outcome you want
/// when nobody has demonstrated that `rename(2)` is atomic there.
fn rename_verdict(path: &Path) -> pb::RenameProbeStatus {
    if !path.is_dir() {
        return pb::RenameProbeStatus::RpUntested;
    }
    // Only an outright Pass grants the verdict. Warn and Skipped both mean the
    // atomic rename was not demonstrated, and "not demonstrated" must not
    // become "verified" by falling through an else.
    match preflight::rename_probe(path).status {
        preflight::ProbeStatus::Pass => pb::RenameProbeStatus::RpAtomicVerified,
        preflight::ProbeStatus::Fail => pb::RenameProbeStatus::RpNotAtomic,
        _ => pb::RenameProbeStatus::RpUntested,
    }
}

/// Which platform this build is running on, if it is one the fleet knows.
///
/// `None` for anything else — a Mac, which is a development machine and not a
/// node. Reporting `Linux` there because it is also a unix would be a lie the
/// dispatcher believes: a `PlatformIn([Linux])` requirement would match, and
/// the job would be placed on a machine nobody meant to include. An unknown
/// platform advertises nothing and therefore satisfies no platform
/// requirement, which is the safe direction.
fn platform() -> Option<Platform> {
    if cfg!(target_os = "linux") {
        Some(Platform::Linux)
    } else if cfg!(windows) {
        Some(Platform::Windows)
    } else {
        None
    }
}

/// The labels an operator passed as `key=value`.
///
/// Anything without an `=` is dropped with a note rather than becoming a label
/// with an empty value — a silent empty label would satisfy a `LabelEquals`
/// requirement nobody meant it to.
pub fn parse_labels(raw: &[String]) -> Vec<(String, String)> {
    let mut out = BTreeMap::new();
    for item in raw {
        match item.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                out.insert(k.to_string(), v.to_string());
            }
            _ => tracing::warn!(label = %item, "ignoring a label that is not key=value"),
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `aac` must not be found inside `libfdk_aac`: the agent would advertise
    /// an encoder it cannot invoke by that name, and every job needing it would
    /// fail on the agent rather than at dispatch.
    #[test]
    fn an_encoder_is_matched_as_a_whole_name() {
        let listing = " A..... libfdk_aac           Fraunhofer FDK AAC\n A..... aac_at               AudioToolbox AAC\n";
        assert!(!lists_name(listing, "aac"));

        let with_aac = format!("{listing} A..... aac                  AAC (Advanced Audio)\n");
        assert!(lists_name(&with_aac, "aac"));
    }

    /// Audio work is `-c:v copy` and needs no video encoder, so it is always
    /// offered. The rest are offered only if the encoder is really there.
    #[test]
    fn classes_follow_the_encoders_that_are_present() {
        assert_eq!(classes_for(&[]), vec![AgentClass::Audio]);
        assert_eq!(
            classes_for(&[EncoderId::Libx265]),
            vec![AgentClass::Audio, AgentClass::Cpu]
        );
        assert_eq!(
            classes_for(&[EncoderId::HevcNvenc]),
            vec![AgentClass::Audio, AgentClass::Gpu]
        );
    }

    /// A label with no `=` must be dropped, not turned into an empty value that
    /// could satisfy a `LabelEquals` requirement by accident.
    #[test]
    fn labels_without_a_value_are_dropped() {
        let labels = parse_labels(&[
            "rack=1".to_string(),
            "broken".to_string(),
            "=novalue".to_string(),
        ]);
        assert_eq!(labels, vec![("rack".to_string(), "1".to_string())]);
    }

    #[test]
    fn a_label_may_contain_an_equals_sign_in_its_value() {
        let labels = parse_labels(&["expr=a=b".to_string()]);
        assert_eq!(labels, vec![("expr".to_string(), "a=b".to_string())]);
    }

    /// An unlimited quota is the *absence* of a limit, not a limit of zero —
    /// zero cores would make the agent ineligible for everything.
    #[test]
    fn effective_cores_is_never_zero() {
        assert!(effective_cores() > 0.0);
    }

    /// A path that does not exist has no room, which reads as ineligible rather
    /// than as optimistic.
    #[test]
    fn free_bytes_of_a_missing_path_is_zero() {
        assert_eq!(free_bytes(Path::new("/definitely/not/here")), 0);
    }

    /// A Mac is a development machine, not a node. Claiming to be Linux there
    /// would let a `PlatformIn([Linux])` requirement match a machine nobody
    /// meant to include.
    #[test]
    fn an_unknown_platform_is_advertised_as_nothing() {
        if cfg!(target_os = "linux") {
            assert_eq!(platform(), Some(Platform::Linux));
        } else if cfg!(windows) {
            assert_eq!(platform(), Some(Platform::Windows));
        } else {
            assert_eq!(platform(), None);
        }
    }

    /// Absence of a trial is not evidence of success.
    #[test]
    fn an_unprobeable_mount_is_untested_not_verified() {
        assert_eq!(
            rename_verdict(Path::new("/definitely/not/here")),
            pb::RenameProbeStatus::RpUntested
        );
    }

    #[test]
    fn a_real_directory_passes_the_rename_probe() {
        let d = tempfile::TempDir::new().unwrap();
        assert_eq!(
            rename_verdict(d.path()),
            pb::RenameProbeStatus::RpAtomicVerified
        );
    }
}
