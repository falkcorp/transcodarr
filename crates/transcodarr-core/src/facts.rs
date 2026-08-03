// file: crates/transcodarr-core/src/facts.rs
// version: 1.1.0
// guid: 7c04e6a8-1b35-49df-a70c-2e58f3b91d64
// last-edited: 2026-08-03
//! Decision-relevant facts derived from a probe.
//!
//! Policy never sees a `MediaProbe`. It sees `FileFacts` — a flattened summary
//! of only what a decision can depend on. That boundary is what makes
//! re-evaluating 49,600 stored files cheap: facts are persisted once, and a
//! policy change re-runs over them without touching a single byte of media.

use serde::{Deserialize, Serialize};

use crate::plan::BitDepth;
use crate::probe::{MediaProbe, StreamKind, bit_depth_of};

/// Coarse size class, used to partition queues so a 60 GB remux cannot sit at
/// the head of the queue starving a hundred 400 MB episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SizeBucket {
    /// Small files.
    Small,
    /// Medium files.
    Medium,
    /// Large files, which get their own low concurrency cap.
    Large,
}

impl SizeBucket {
    /// The canonical spelling, which is also the value stored in SQLite.
    ///
    /// Lives here rather than in the store because the enum is
    /// `#[non_exhaustive]`: a downstream mapping would need a wildcard arm, and
    /// a wildcard here means a new bucket is silently persisted as an old one.
    pub fn as_str(self) -> &'static str {
        match self {
            SizeBucket::Small => "Small",
            SizeBucket::Medium => "Medium",
            SizeBucket::Large => "Large",
        }
    }

    /// Parse the canonical spelling. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Small" => SizeBucket::Small,
            "Medium" => SizeBucket::Medium,
            "Large" => SizeBucket::Large,
            _ => return None,
        })
    }
}

impl std::fmt::Display for SizeBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Byte boundaries between size buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeThresholds {
    /// Below this is `Small`.
    pub small_max_bytes: u64,
    /// Below this is `Medium`; at or above it is `Large`.
    pub medium_max_bytes: u64,
}

impl Default for SizeThresholds {
    fn default() -> Self {
        // 4 GB / 20 GB. The large band exists because the ZFS pool is
        // latency-bound: 47 concurrent 40-80 GB jobs produced per-file ETAs of
        // 3-34 hours, so those files need their own concurrency cap.
        Self {
            small_max_bytes: 4 * 1024 * 1024 * 1024,
            medium_max_bytes: 20 * 1024 * 1024 * 1024,
        }
    }
}

/// Classify a file size into a bucket.
pub fn size_bucket_for(bytes: u64, t: &SizeThresholds) -> SizeBucket {
    if bytes < t.small_max_bytes {
        SizeBucket::Small
    } else if bytes < t.medium_max_bytes {
        SizeBucket::Medium
    } else {
        SizeBucket::Large
    }
}

/// Everything a policy decision may depend on, and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FileFacts {
    /// Container format.
    pub container: String,
    /// Duration in microseconds.
    pub duration_us: Option<u64>,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Overall bitrate.
    pub bit_rate_bps: Option<u64>,
    /// Primary video codec, e.g. `hevc`.
    pub video_codec: Option<String>,
    /// Primary video profile, e.g. `Main 10`.
    pub video_profile: Option<String>,
    /// Primary video bit depth.
    pub video_bit_depth: Option<BitDepth>,
    /// Primary video pixel format.
    pub video_pix_fmt: Option<String>,
    /// Width in pixels.
    pub width: Option<u32>,
    /// Height in pixels.
    pub height: Option<u32>,
    /// True when the video carries an HDR transfer function.
    pub is_hdr: bool,
    /// True when Dolby Vision is present.
    pub is_dovi: bool,
    /// Dolby Vision profile, when known. Profile 7 is excluded from all work.
    pub dovi_profile: Option<u8>,
    /// True when an audio track carries object audio (Atmos, DTS:X).
    pub has_object_audio: bool,
    /// Audio codecs present, in container order.
    pub audio_codecs: Vec<String>,
    /// Number of audio tracks.
    pub audio_track_count: usize,
    /// Number of subtitle tracks.
    pub subtitle_track_count: usize,
}

/// Codecs that are lossless or effectively so, and are re-encoded to EAC3.
///
/// Opus is in this list at the owner's instruction: it is not lossless, but
/// Opus output is not wanted, so an Opus track is treated as needing
/// conversion. `aac`, `ac3`, `eac3` and `mp3` are deliberately left alone.
pub const AUDIO_NEEDING_CONVERSION: &[&str] = &[
    "truehd",
    "dts",
    "flac",
    "pcm",
    "mlp",
    "opus",
    "pcm_s16le",
    "pcm_s24le",
];

/// Transfer characteristics that mean HDR.
const HDR_TRANSFERS: &[&str] = &["smpte2084", "arib-std-b67"];

/// Derive the facts a policy decision may use.
pub fn derive_facts(probe: &MediaProbe, size_bytes: u64) -> FileFacts {
    let video = probe.primary_video();

    let is_hdr = video
        .and_then(|v| v.profile.as_deref())
        .map(|p| HDR_TRANSFERS.iter().any(|t| p.to_lowercase().contains(t)))
        .unwrap_or(false)
        || video
            .and_then(|v| v.pix_fmt.as_deref())
            .map(|p| HDR_TRANSFERS.iter().any(|t| p.contains(t)))
            .unwrap_or(false);

    let dovi_profile = video.and_then(|v| v.profile.as_deref()).and_then(|p| {
        let lp = p.to_lowercase();
        if lp.contains("dvhe") || lp.contains("dolby vision") || lp.contains("dvh1") {
            lp.split_whitespace()
                .find_map(|tok| tok.parse::<u8>().ok())
                .or(Some(0))
        } else {
            None
        }
    });

    let audio_codecs: Vec<String> = probe
        .streams_of(StreamKind::Audio)
        .map(|s| s.codec.to_lowercase())
        .collect();

    // Atmos rides inside TrueHD; DTS:X inside DTS-HD. Both appear in the title
    // tag far more reliably than anywhere structured, which is why the title is
    // consulted at all.
    let has_object_audio = probe.streams_of(StreamKind::Audio).any(|s| {
        let t = s.title.as_deref().unwrap_or("").to_lowercase();
        t.contains("atmos") || t.contains("dts:x") || t.contains("dts-x")
    });

    FileFacts {
        container: probe.container.clone(),
        duration_us: probe.duration_us,
        size_bytes,
        bit_rate_bps: probe.bit_rate_bps,
        video_codec: video.map(|v| v.codec.to_lowercase()),
        video_profile: video.and_then(|v| v.profile.clone()),
        video_bit_depth: video.map(|v| bit_depth_of(v.pix_fmt.as_deref(), v.bit_depth)),
        video_pix_fmt: video.and_then(|v| v.pix_fmt.clone()),
        width: video.and_then(|v| v.width),
        height: video.and_then(|v| v.height),
        is_hdr,
        is_dovi: dovi_profile.is_some(),
        dovi_profile,
        has_object_audio,
        audio_track_count: audio_codecs.len(),
        audio_codecs,
        subtitle_track_count: probe.subtitle_count(),
    }
}

/// A stable signature of the decision-relevant content facts.
///
/// A plan carries the signature of the facts it was built from; the agent
/// aborts if the source no longer matches. That is what stops a stale plan
/// built before an HDR remux from being applied after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSig(pub String);

/// Compute the content signature for a set of facts.
pub fn content_sig(facts: &FileFacts) -> ContentSig {
    // Only decision-relevant fields go in. Size deliberately does not: a file
    // whose size changed but whose streams did not is the same decision.
    let material = format!(
        "{}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}|{:?}|{}|{}",
        facts.container,
        facts.video_codec,
        facts.video_profile,
        facts.video_bit_depth,
        facts.width,
        facts.height,
        facts.is_hdr,
        facts.is_dovi,
        facts.dovi_profile,
        facts.audio_codecs.join(","),
        facts.subtitle_track_count,
    );
    ContentSig(blake3::hash(material.as_bytes()).to_hex().to_string())
}

impl FileFacts {
    /// Whether any audio track needs converting to EAC3.
    pub fn needs_audio_conversion(&self) -> bool {
        self.audio_codecs
            .iter()
            .any(|c| AUDIO_NEEDING_CONVERSION.iter().any(|n| c.contains(n)))
    }

    /// Whether the video may be re-encoded at all.
    ///
    /// HDR and Dolby Vision are never re-encoded — tone mapping is lossy and
    /// irreversible, and the result is worse than the file we started with.
    pub fn video_is_encodable(&self) -> bool {
        !self.is_hdr && !self.is_dovi
    }

    /// Whether this file is excluded from *all* work.
    ///
    /// Dolby Vision profile 7 carries a dual-layer structure that ffmpeg cannot
    /// round-trip, and object audio is destroyed by a channel-based re-encode.
    /// Unknown DV profiles veto too: not knowing is not permission.
    pub fn is_excluded(&self) -> bool {
        self.has_object_audio || matches!(self.dovi_profile, Some(7) | Some(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::StreamInfo;

    fn probe(video: Option<StreamInfo>, audio: &[(&str, Option<&str>)]) -> MediaProbe {
        let mut streams = Vec::new();
        if let Some(v) = video {
            streams.push(v);
        }
        for (i, (codec, title)) in audio.iter().enumerate() {
            streams.push(StreamInfo {
                index: 10 + i as u32,
                kind: StreamKind::Audio,
                codec: (*codec).into(),
                title: title.map(|t| t.to_string()),
                ..Default::default()
            });
        }
        MediaProbe {
            container: "matroska".into(),
            duration_us: Some(3_600_000_000),
            streams,
            ..Default::default()
        }
    }

    fn vid(codec: &str, profile: Option<&str>, pix: Option<&str>) -> StreamInfo {
        StreamInfo {
            index: 0,
            kind: StreamKind::Video,
            codec: codec.into(),
            profile: profile.map(str::to_string),
            pix_fmt: pix.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn lossless_audio_is_flagged_for_conversion() {
        for codec in ["truehd", "dts", "flac", "pcm_s24le", "mlp"] {
            let f = derive_facts(&probe(None, &[(codec, None)]), 1);
            assert!(f.needs_audio_conversion(), "{codec} should convert");
        }
    }

    /// Opus is not lossless, but the owner does not want Opus output, so it is
    /// treated as needing conversion.
    #[test]
    fn opus_is_converted_even_though_it_is_lossy() {
        let f = derive_facts(&probe(None, &[("opus", None)]), 1);
        assert!(f.needs_audio_conversion());
    }

    #[test]
    fn already_acceptable_audio_is_left_alone() {
        for codec in ["aac", "ac3", "eac3", "mp3"] {
            let f = derive_facts(&probe(None, &[(codec, None)]), 1);
            assert!(!f.needs_audio_conversion(), "{codec} should be left alone");
        }
    }

    #[test]
    fn hdr_video_is_never_encodable() {
        let f = derive_facts(
            &probe(Some(vid("hevc", Some("Main 10 smpte2084"), None)), &[]),
            1,
        );
        assert!(f.is_hdr);
        assert!(!f.video_is_encodable(), "HDR must never be re-encoded");
    }

    #[test]
    fn dolby_vision_is_never_encodable() {
        let f = derive_facts(&probe(Some(vid("hevc", Some("dvhe.07"), None)), &[]), 1);
        assert!(f.is_dovi);
        assert!(!f.video_is_encodable());
    }

    /// Profile 7 is dual-layer and ffmpeg cannot round-trip it, so the file is
    /// excluded from every kind of work, not merely from video encoding.
    #[test]
    fn dolby_vision_profile_7_is_excluded_entirely() {
        let f = derive_facts(&probe(Some(vid("hevc", Some("dvhe 7"), None)), &[]), 1);
        assert_eq!(f.dovi_profile, Some(7));
        assert!(f.is_excluded());
    }

    #[test]
    fn object_audio_is_excluded_entirely() {
        let f = derive_facts(&probe(None, &[("truehd", Some("TrueHD 7.1 Atmos"))]), 1);
        assert!(f.has_object_audio);
        assert!(f.is_excluded(), "a channel re-encode would destroy Atmos");
    }

    #[test]
    fn ordinary_sdr_video_is_encodable_and_not_excluded() {
        let f = derive_facts(
            &probe(
                Some(vid("h264", Some("High"), Some("yuv420p"))),
                &[("aac", None)],
            ),
            1,
        );
        assert!(f.video_is_encodable());
        assert!(!f.is_excluded());
    }

    #[test]
    fn ten_bit_depth_survives_into_the_facts() {
        let f = derive_facts(
            &probe(Some(vid("hevc", Some("Main 10"), Some("yuv420p10le"))), &[]),
            1,
        );
        assert_eq!(f.video_bit_depth, Some(BitDepth::Ten));
    }

    #[test]
    fn size_buckets_partition_at_the_thresholds() {
        let t = SizeThresholds::default();
        assert_eq!(size_bucket_for(1, &t), SizeBucket::Small);
        assert_eq!(
            size_bucket_for(10 * 1024 * 1024 * 1024, &t),
            SizeBucket::Medium
        );
        assert_eq!(
            size_bucket_for(60 * 1024 * 1024 * 1024, &t),
            SizeBucket::Large
        );
    }

    #[test]
    fn content_sig_is_stable_for_identical_facts() {
        let p = probe(Some(vid("hevc", Some("Main 10"), None)), &[("eac3", None)]);
        assert_eq!(
            content_sig(&derive_facts(&p, 1)),
            content_sig(&derive_facts(&p, 1))
        );
    }

    /// Size is not part of the signature: a file whose size changed but whose
    /// streams did not is still the same decision.
    #[test]
    fn content_sig_ignores_size_but_tracks_streams() {
        let p = probe(Some(vid("hevc", None, None)), &[("eac3", None)]);
        assert_eq!(
            content_sig(&derive_facts(&p, 1)),
            content_sig(&derive_facts(&p, 999_999))
        );

        let q = probe(Some(vid("hevc", None, None)), &[("truehd", None)]);
        assert_ne!(
            content_sig(&derive_facts(&p, 1)),
            content_sig(&derive_facts(&q, 1)),
            "changing the audio codec must change the signature"
        );
    }
}
