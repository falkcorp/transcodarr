// file: crates/transcodarr-core/src/probe.rs
// version: 1.0.0
// guid: 1b6d3a82-5f47-4c90-a2e1-7d84c50b93f6
// last-edited: 2026-08-01
//! Parsed ffprobe output.
//!
//! ffprobe's JSON is loose — numbers arrive as strings, fields come and go by
//! codec and container. Everything lax is turned into typed values exactly
//! once, here, so no downstream code has to re-parse `"1920"` or guess whether
//! `bits_per_raw_sample` was present.

use serde::{Deserialize, Serialize};

use crate::plan::BitDepth;

/// What kind of data a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub enum StreamKind {
    /// Video.
    Video,
    /// Audio.
    Audio,
    /// Subtitles.
    Subtitle,
    /// Attachments, data, anything else. The default: an unrecognised
    /// `codec_type` must never be mistaken for video or audio.
    #[default]
    Other,
}

/// One parsed video, audio, subtitle or data stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamInfo {
    /// Position within the container.
    pub index: u32,
    /// Which kind of stream this is.
    pub kind: StreamKind,
    /// ffmpeg codec name, e.g. `hevc`, `eac3`, `subrip`.
    pub codec: String,
    /// Codec profile, e.g. `Main 10`, when reported.
    pub profile: Option<String>,
    /// Pixel format, video only.
    pub pix_fmt: Option<String>,
    /// Width in pixels, video only.
    pub width: Option<u32>,
    /// Height in pixels, video only.
    pub height: Option<u32>,
    /// Bits per raw sample, when reported.
    pub bit_depth: Option<u8>,
    /// Channel count, audio only.
    pub channels: Option<u16>,
    /// Channel layout string, e.g. `5.1(side)`.
    pub channel_layout: Option<String>,
    /// Declared bitrate in bits per second, when reported.
    pub bit_rate_bps: Option<u64>,
    /// ISO language tag.
    pub language: Option<String>,
    /// Human title tag.
    pub title: Option<String>,
    /// Whether the container marks this stream default.
    pub is_default: bool,
    /// Whether the container marks this stream forced.
    pub is_forced: bool,
}

/// Parsed ffprobe output for one media file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaProbe {
    /// Container format name(s) as ffprobe reports them.
    pub container: String,
    /// Duration in microseconds, when reported.
    pub duration_us: Option<u64>,
    /// Overall bitrate in bits per second, when reported.
    pub bit_rate_bps: Option<u64>,
    /// Container-reported size in bytes, when present.
    pub size_bytes: Option<u64>,
    /// Every stream, in container order.
    pub streams: Vec<StreamInfo>,
}

impl MediaProbe {
    /// Streams of a given kind, in container order.
    pub fn streams_of(&self, kind: StreamKind) -> impl Iterator<Item = &StreamInfo> {
        self.streams.iter().filter(move |s| s.kind == kind)
    }

    /// The first video stream, which is the one policy reasons about.
    pub fn primary_video(&self) -> Option<&StreamInfo> {
        self.streams_of(StreamKind::Video).next()
    }

    /// Count of audio streams.
    pub fn audio_count(&self) -> usize {
        self.streams_of(StreamKind::Audio).count()
    }

    /// Count of subtitle streams.
    pub fn subtitle_count(&self) -> usize {
        self.streams_of(StreamKind::Subtitle).count()
    }
}

/// Interpret a pixel format and reported sample depth as a [`BitDepth`].
///
/// `bits_per_raw_sample` is absent often enough that the pixel format has to be
/// the fallback — `yuv420p10le` is unambiguously 10-bit whatever the tag says.
/// Unknown formats resolve to 8-bit, the conservative answer: it never causes
/// an 8-bit source to be treated as 10-bit and silently upconverted.
pub fn bit_depth_of(pix_fmt: Option<&str>, bits_per_raw_sample: Option<u8>) -> BitDepth {
    if let Some(b) = bits_per_raw_sample {
        match b {
            0..=8 => return BitDepth::Eight,
            9..=10 => return BitDepth::Ten,
            _ => return BitDepth::Twelve,
        }
    }
    match pix_fmt {
        Some(p) if p.contains("12") => BitDepth::Twelve,
        Some(p) if p.contains("10") || p.contains("p010") => BitDepth::Ten,
        _ => BitDepth::Eight,
    }
}

// ---- wire shapes -----------------------------------------------------------
// Private mirrors of ffprobe's JSON. They exist only to be converted; nothing
// outside this module sees a stringly-typed field.

#[derive(Deserialize)]
struct RawProbe {
    #[serde(default)]
    format: RawFormat,
    #[serde(default)]
    streams: Vec<RawStream>,
}

#[derive(Deserialize, Default)]
struct RawFormat {
    #[serde(default)]
    format_name: String,
    duration: Option<String>,
    bit_rate: Option<String>,
    size: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawStream {
    #[serde(default)]
    index: u32,
    codec_type: Option<String>,
    #[serde(default)]
    codec_name: String,
    profile: Option<String>,
    pix_fmt: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    bits_per_raw_sample: Option<String>,
    channels: Option<u16>,
    channel_layout: Option<String>,
    bit_rate: Option<String>,
    #[serde(default)]
    tags: RawTags,
    #[serde(default)]
    disposition: RawDisposition,
}

#[derive(Deserialize, Default)]
struct RawTags {
    language: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawDisposition {
    #[serde(default)]
    default: u8,
    #[serde(default)]
    forced: u8,
}

fn parse_num<T: std::str::FromStr>(s: Option<&String>) -> Option<T> {
    s.and_then(|v| v.parse::<T>().ok())
}

/// Parse `ffprobe -print_format json -show_format -show_streams` output.
///
/// Malformed JSON is an error. *Missing fields are not* — ffprobe legitimately
/// omits `duration` on some containers and `bits_per_raw_sample` on most. A
/// parser that rejected those would refuse files that transcode perfectly well.
pub fn parse_ffprobe_json(raw: &str) -> Result<MediaProbe, crate::CoreError> {
    let p: RawProbe =
        serde_json::from_str(raw).map_err(|e| crate::CoreError::MalformedProbe(e.to_string()))?;

    let streams = p
        .streams
        .into_iter()
        .map(|s| StreamInfo {
            index: s.index,
            kind: match s.codec_type.as_deref() {
                Some("video") => StreamKind::Video,
                Some("audio") => StreamKind::Audio,
                Some("subtitle") => StreamKind::Subtitle,
                _ => StreamKind::Other,
            },
            codec: s.codec_name,
            profile: s.profile,
            pix_fmt: s.pix_fmt,
            width: s.width,
            height: s.height,
            bit_depth: parse_num::<u8>(s.bits_per_raw_sample.as_ref()),
            channels: s.channels,
            channel_layout: s.channel_layout,
            bit_rate_bps: parse_num::<u64>(s.bit_rate.as_ref()),
            language: s.tags.language,
            title: s.tags.title,
            is_default: s.disposition.default == 1,
            is_forced: s.disposition.forced == 1,
        })
        .collect();

    Ok(MediaProbe {
        container: p.format.format_name,
        // Duration arrives as fractional seconds; microseconds keep the
        // precision the duration gate needs without floating point creeping
        // into comparisons.
        duration_us: p
            .format
            .duration
            .as_ref()
            .and_then(|d| d.parse::<f64>().ok())
            .map(|secs| (secs * 1_000_000.0).round() as u64),
        bit_rate_bps: parse_num::<u64>(p.format.bit_rate.as_ref()),
        size_bytes: parse_num::<u64>(p.format.size.as_ref()),
        streams,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "streams": [
        {"index":0,"codec_type":"video","codec_name":"hevc","profile":"Main 10",
         "pix_fmt":"yuv420p10le","width":1920,"height":1080,
         "bits_per_raw_sample":"10","disposition":{"default":1,"forced":0}},
        {"index":1,"codec_type":"audio","codec_name":"truehd","channels":8,
         "channel_layout":"7.1","tags":{"language":"eng","title":"Atmos"},
         "disposition":{"default":1,"forced":0}},
        {"index":2,"codec_type":"audio","codec_name":"eac3","channels":6,
         "tags":{"language":"eng"},"disposition":{"default":0,"forced":0}},
        {"index":3,"codec_type":"subtitle","codec_name":"subrip",
         "tags":{"language":"eng"},"disposition":{"default":0,"forced":1}}
      ],
      "format": {"format_name":"matroska,webm","duration":"7245.500000",
                 "bit_rate":"8000000","size":"7245000000"}
    }"#;

    #[test]
    fn parses_streams_and_format() {
        let p = parse_ffprobe_json(SAMPLE).unwrap();
        assert_eq!(p.container, "matroska,webm");
        assert_eq!(p.duration_us, Some(7_245_500_000));
        assert_eq!(p.streams.len(), 4);
        assert_eq!(p.audio_count(), 2);
        assert_eq!(p.subtitle_count(), 1);
    }

    #[test]
    fn every_audio_and_subtitle_stream_is_visible() {
        // A bare `-c:a eac3` silently drops every track but the default, so the
        // model has to see all of them or the planner cannot map them.
        let p = parse_ffprobe_json(SAMPLE).unwrap();
        let langs: Vec<_> = p
            .streams_of(StreamKind::Audio)
            .map(|s| s.codec.as_str())
            .collect();
        assert_eq!(langs, vec!["truehd", "eac3"]);
        assert!(p.streams_of(StreamKind::Subtitle).any(|s| s.is_forced));
    }

    #[test]
    fn ten_bit_is_detected_from_the_tag() {
        let p = parse_ffprobe_json(SAMPLE).unwrap();
        let v = p.primary_video().unwrap();
        assert_eq!(
            bit_depth_of(v.pix_fmt.as_deref(), v.bit_depth),
            BitDepth::Ten
        );
    }

    #[test]
    fn ten_bit_is_detected_from_pix_fmt_when_the_tag_is_missing() {
        assert_eq!(bit_depth_of(Some("yuv420p10le"), None), BitDepth::Ten);
        assert_eq!(bit_depth_of(Some("p010le"), None), BitDepth::Ten);
    }

    #[test]
    fn unknown_depth_is_assumed_eight_never_ten() {
        // The conservative direction: guessing 8 can never cause an upconvert.
        assert_eq!(bit_depth_of(None, None), BitDepth::Eight);
        assert_eq!(bit_depth_of(Some("yuv420p"), None), BitDepth::Eight);
    }

    #[test]
    fn missing_optional_fields_are_not_an_error() {
        let minimal = r#"{"streams":[],"format":{"format_name":"mov,mp4"}}"#;
        let p = parse_ffprobe_json(minimal).unwrap();
        assert_eq!(p.duration_us, None);
        assert!(p.streams.is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_ffprobe_json("{not json").is_err());
    }
}
