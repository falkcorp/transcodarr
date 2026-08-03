// file: crates/transcodarr-core/src/plan.rs
// version: 1.1.0
// guid: 7e2b4c98-1d05-4a37-92f6-c8b30e1a5d64
// last-edited: 2026-08-03
//! Encoder identities, pixel formats, and ffmpeg argv construction.
//!
//! `build_ffmpeg_argv` is the single source of the command line. The dry-run
//! preview and the real execution both call it, so a preview cannot show one
//! command while a different one runs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// An encoder transcodarr knows how to plan for.
///
/// `#[non_exhaustive]`: new encoders are expected, and downstream matches must
/// keep compiling when they arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EncoderId {
    /// NVENC HEVC, hardware.
    HevcNvenc,
    /// x265, software HEVC.
    Libx265,
    /// x264, software AVC.
    Libx264,
    /// Dolby Digital Plus.
    Eac3,
    /// Dolby Digital.
    Ac3,
    /// Advanced Audio Coding.
    Aac,
    /// Stream copy — remux without re-encoding.
    Copy,
}

impl EncoderId {
    /// The ffmpeg codec name this encoder is invoked as.
    pub fn as_ffmpeg(self) -> &'static str {
        match self {
            EncoderId::HevcNvenc => "hevc_nvenc",
            EncoderId::Libx265 => "libx265",
            EncoderId::Libx264 => "libx264",
            EncoderId::Eac3 => "eac3",
            EncoderId::Ac3 => "ac3",
            EncoderId::Aac => "aac",
            EncoderId::Copy => "copy",
        }
    }

    /// Whether this encoder produces a video stream. Audio encoders and `Copy`
    /// have no pixel format.
    pub fn is_video(self) -> bool {
        matches!(
            self,
            EncoderId::HevcNvenc | EncoderId::Libx265 | EncoderId::Libx264
        )
    }
}

/// Source or target bit depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BitDepth {
    /// 8-bit.
    Eight,
    /// 10-bit.
    Ten,
    /// 12-bit.
    Twelve,
}

impl BitDepth {
    /// Bits per sample, which is how the depth is stored.
    ///
    /// A number rather than a name because the column is an INTEGER and
    /// because `#[non_exhaustive]` would force a wildcard arm on any
    /// downstream mapping — and a wildcard here would silently record 12-bit
    /// source as 8-bit, which is exactly the upconversion that must never
    /// happen.
    pub fn bits(self) -> u8 {
        match self {
            BitDepth::Eight => 8,
            BitDepth::Ten => 10,
            BitDepth::Twelve => 12,
        }
    }

    /// Recover a depth from bits per sample. `None` for anything else.
    pub fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            8 => BitDepth::Eight,
            10 => BitDepth::Ten,
            12 => BitDepth::Twelve,
            _ => return None,
        })
    }
}

/// Encoder-specific pixel format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PixFmt {
    /// 8-bit planar 4:2:0.
    Yuv420p,
    /// 10-bit planar 4:2:0, little-endian — what libx265 wants.
    Yuv420p10le,
    /// 10-bit semi-planar 4:2:0 — what NVENC wants.
    P010le,
}

impl PixFmt {
    /// The `-pix_fmt` value passed to ffmpeg.
    pub fn as_ffmpeg(self) -> &'static str {
        match self {
            PixFmt::Yuv420p => "yuv420p",
            PixFmt::Yuv420p10le => "yuv420p10le",
            PixFmt::P010le => "p010le",
        }
    }
}

/// Select the pixel format an encoder needs for a given source bit depth.
///
/// The match is exhaustive on purpose — a new `EncoderId` or `BitDepth` must
/// force a decision here rather than falling through a wildcard to a format
/// that errors the job at runtime. libx265 wants `yuv420p10le` where NVENC
/// wants `p010le`; passing the wrong one fails the encode.
///
/// Returns `None` when no pixel format applies or none is supported:
///
/// - audio encoders and `Copy` have no pixel format at all;
/// - 12-bit has no target format here, so a 12-bit source is not planned for
///   video encode. Declining to encode is always safe; silently down-converting
///   to 10-bit would destroy source precision.
///
/// Bit depth is never *raised* — an 8-bit source stays 8-bit.
pub fn pix_fmt_for(enc: EncoderId, depth: BitDepth) -> Option<PixFmt> {
    match (enc, depth) {
        (EncoderId::Libx265, BitDepth::Eight) => Some(PixFmt::Yuv420p),
        (EncoderId::Libx265, BitDepth::Ten) => Some(PixFmt::Yuv420p10le),
        (EncoderId::Libx265, BitDepth::Twelve) => None,

        (EncoderId::Libx264, BitDepth::Eight) => Some(PixFmt::Yuv420p),
        (EncoderId::Libx264, BitDepth::Ten) => Some(PixFmt::Yuv420p10le),
        (EncoderId::Libx264, BitDepth::Twelve) => None,

        (EncoderId::HevcNvenc, BitDepth::Eight) => Some(PixFmt::Yuv420p),
        (EncoderId::HevcNvenc, BitDepth::Ten) => Some(PixFmt::P010le),
        (EncoderId::HevcNvenc, BitDepth::Twelve) => None,

        (EncoderId::Eac3, _) => None,
        (EncoderId::Ac3, _) => None,
        (EncoderId::Aac, _) => None,
        (EncoderId::Copy, _) => None,
    }
}

/// Input and output locations for one job, already translated to the paths the
/// executing machine sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobPaths {
    /// Source file to read.
    pub input: PathBuf,
    /// Destination file to write.
    pub output: PathBuf,
}

/// A fully-decided encode, ready to be turned into an ffmpeg command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodePlan {
    /// Video codec, or `Copy` to remux the video stream untouched.
    pub video_codec: EncoderId,
    /// Audio codec, or `Copy` to remux audio untouched.
    pub audio_codec: EncoderId,
    /// Pixel format, when the video encoder needs one pinned.
    pub pix_fmt: Option<PixFmt>,
    /// Extra arguments appended after the standard set, so they win.
    pub extra_args: Vec<String>,
}

/// Build the ffmpeg argument vector for a plan.
///
/// The standard set preserves metadata: `-map_metadata 0` copies global tags,
/// `-movflags use_metadata_tags` keeps them through MP4 muxing, and `-c:s copy`
/// keeps subtitle streams. Caller-supplied `extra_args` are appended last so a
/// user override beats a preset default, and the output path always comes last.
pub fn build_ffmpeg_argv(plan: &EncodePlan, paths: &JobPaths) -> Vec<String> {
    build_ffmpeg_argv_raw(
        plan.video_codec.as_ffmpeg(),
        plan.audio_codec.as_ffmpeg(),
        plan.pix_fmt,
        &plan.extra_args,
        paths,
    )
}

/// Assemble an ffmpeg argv from codec names given as plain strings.
///
/// This exists for the `local` CLI path, which has always accepted *any* ffmpeg
/// codec name — `--vcodec libsvtav1` works today and must keep working, while
/// [`EncoderId`] is deliberately a closed set the policy engine can reason
/// about. Rather than widening the enum with a stringly-typed escape hatch that
/// would infect every match on it, the two paths share this one assembler.
/// [`build_ffmpeg_argv`] is a thin typed wrapper over it, so the orchestrator
/// and the CLI cannot drift on argument order or metadata flags.
pub fn build_ffmpeg_argv_raw(
    vcodec: &str,
    acodec: &str,
    pix_fmt: Option<PixFmt>,
    extra: &[String],
    paths: &JobPaths,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-y".into(),
        "-i".into(),
        paths.input.to_string_lossy().into_owned(),
        "-map_metadata".into(),
        "0".into(),
        "-movflags".into(),
        "use_metadata_tags".into(),
        "-c:v".into(),
        vcodec.into(),
        "-c:a".into(),
        acodec.into(),
        "-c:s".into(),
        "copy".into(),
    ];

    if let Some(pf) = pix_fmt {
        args.push("-pix_fmt".into());
        args.push(pf.as_ffmpeg().into());
    }

    args.extend(extra.iter().cloned());
    args.push(paths.output.to_string_lossy().into_owned());
    args
}

/// Render an argv as a copy-pasteable shell command, quoting only what needs it.
pub fn format_command(program: &str, args: &[String]) -> String {
    let mut out = String::from(program);
    for arg in args {
        out.push(' ');
        if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || "'\"\\$`".contains(c)) {
            out.push('\'');
            out.push_str(&arg.replace('\'', r"'\''"));
            out.push('\'');
        } else {
            out.push_str(arg);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_bit_targets_differ_by_encoder() {
        assert_eq!(
            pix_fmt_for(EncoderId::Libx265, BitDepth::Ten),
            Some(PixFmt::Yuv420p10le)
        );
        assert_eq!(
            pix_fmt_for(EncoderId::HevcNvenc, BitDepth::Ten),
            Some(PixFmt::P010le)
        );
    }

    #[test]
    fn eight_bit_is_never_upconverted() {
        for enc in [EncoderId::Libx265, EncoderId::Libx264, EncoderId::HevcNvenc] {
            assert_eq!(
                pix_fmt_for(enc, BitDepth::Eight),
                Some(PixFmt::Yuv420p),
                "{enc:?} must keep 8-bit at 8-bit"
            );
        }
    }

    #[test]
    fn audio_encoders_have_no_pixel_format() {
        for enc in [
            EncoderId::Eac3,
            EncoderId::Ac3,
            EncoderId::Aac,
            EncoderId::Copy,
        ] {
            for d in [BitDepth::Eight, BitDepth::Ten, BitDepth::Twelve] {
                assert_eq!(pix_fmt_for(enc, d), None);
            }
        }
    }

    #[test]
    fn argv_preserves_metadata_and_subtitles() {
        let plan = EncodePlan {
            video_codec: EncoderId::Libx265,
            audio_codec: EncoderId::Aac,
            pix_fmt: None,
            extra_args: vec![],
        };
        let paths = JobPaths {
            input: PathBuf::from("/in.mp4"),
            output: PathBuf::from("/out.mkv"),
        };
        let argv = build_ffmpeg_argv(&plan, &paths);
        for expected in ["-map_metadata", "0", "-movflags", "use_metadata_tags"] {
            assert!(argv.iter().any(|a| a == expected), "missing {expected}");
        }
        let s_idx = argv.iter().position(|a| a == "-c:s").unwrap();
        assert_eq!(argv[s_idx + 1], "copy");
    }

    #[test]
    fn output_path_is_always_last() {
        let plan = EncodePlan {
            video_codec: EncoderId::Libx265,
            audio_codec: EncoderId::Aac,
            pix_fmt: Some(PixFmt::Yuv420p10le),
            extra_args: vec!["-crf".into(), "18".into()],
        };
        let paths = JobPaths {
            input: PathBuf::from("/in.mp4"),
            output: PathBuf::from("/out.mkv"),
        };
        let argv = build_ffmpeg_argv(&plan, &paths);
        assert_eq!(argv.last().unwrap(), "/out.mkv");
    }

    #[test]
    fn extra_args_come_after_defaults_so_they_win() {
        let plan = EncodePlan {
            video_codec: EncoderId::Libx265,
            audio_codec: EncoderId::Aac,
            pix_fmt: None,
            extra_args: vec!["-crf".into(), "18".into()],
        };
        let paths = JobPaths {
            input: PathBuf::from("/in.mp4"),
            output: PathBuf::from("/out.mkv"),
        };
        let argv = build_ffmpeg_argv(&plan, &paths);
        let crf = argv.iter().position(|a| a == "-crf").unwrap();
        let cv = argv.iter().position(|a| a == "-c:v").unwrap();
        assert!(crf > cv, "user extras must be appended after the defaults");
    }

    #[test]
    fn format_command_quotes_paths_with_spaces() {
        let s = format_command("ffmpeg", &["-i".into(), "/m/My Show/ep 1.mkv".into()]);
        assert_eq!(s, "ffmpeg -i '/m/My Show/ep 1.mkv'");
    }
}
