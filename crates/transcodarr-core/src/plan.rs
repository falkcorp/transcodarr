// file: crates/transcodarr-core/src/plan.rs
// version: 1.2.0
// guid: 7e2b4c98-1d05-4a37-92f6-c8b30e1a5d64
// last-edited: 2026-08-16
//! Encoder identities, pixel formats, and ffmpeg argv construction.
//!
//! `build_ffmpeg_argv` is the single source of the command line. The dry-run
//! preview and the real execution both call it, so a preview cannot show one
//! command while a different one runs.

use std::path::PathBuf;

use crate::capability::{Platform, TransportMode};

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

/// What the server knows about where one agent can reach files.
///
/// The three fields [`agent_job_paths`] needs, pulled out so the resolver does
/// not take a whole `Capability` and become impossible to call from a test.
#[derive(Debug, Clone, Copy)]
pub struct AgentView<'a> {
    /// How this agent gets at the media.
    pub transport: TransportMode,
    /// Which separator its paths use. `None` is treated as the server's own.
    pub platform: Option<Platform>,
    /// Absolute path to the agent's work area, in the agent's namespace.
    pub workarea_path: &'a str,
}

/// Translate one job's locations into the paths a given agent will see.
///
/// **This is the only place a path crosses from the server's namespace into an
/// agent's.** `argv` is composed server-side and executed verbatim, and
/// `JobStarted` echoes it back for an equality check, so the translation has to
/// happen before `build_ffmpeg_argv` rather than anywhere downstream of it.
/// Putting it here also means the mount table's
/// `canonical_prefix` -> `local_path` rewrite — designed but never implemented,
/// and currently masked because every mount-mode run so far has been same-host
/// — has exactly one obvious home when it is written, instead of becoming a
/// second translator that can disagree with this one.
///
/// Under [`TransportMode::Stream`] the agent cannot resolve a canonical path at
/// all, so both ends are named inside its own work area: it fetches the source
/// to `input` and writes its encode to `output`, and the server moves the bytes
/// in both directions.
pub fn agent_job_paths(
    view: &AgentView<'_>,
    job_id: &str,
    attempt: i64,
    canonical_source: &std::path::Path,
    server_temp: &std::path::Path,
) -> JobPaths {
    if view.transport != TransportMode::Stream {
        // Today's behaviour, unchanged. A mount-mode agent resolves the
        // canonical path through its own mount table, which is why it is
        // required to advertise one covering the prefix.
        return JobPaths {
            input: canonical_source.to_path_buf(),
            output: server_temp.to_path_buf(),
        };
    }

    let ext = canonical_source
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "mkv".to_string());
    let stem = format!("{}.{attempt}", sanitise_component(job_id));
    JobPaths {
        input: join_for(view, &format!("{stem}.src.{ext}")),
        output: join_for(view, &format!("{stem}.partial.{ext}")),
    }
}

/// Join a file name onto the agent's work area using *that agent's* separator.
///
/// `Path::join` would use the server's, and the server is Linux while the only
/// streaming agent so far is Windows. ffmpeg would cope with the mixed result,
/// but `job_attempt.argv_json` is persisted so an operator can paste it into a
/// shell on that machine, and a path that merely happens to work is not the
/// same promise.
fn join_for(view: &AgentView<'_>, name: &str) -> PathBuf {
    let root = view.workarea_path.trim_end_matches(['/', '\\']);
    let sep = match view.platform {
        Some(Platform::Windows) => '\\',
        Some(Platform::Linux) => '/',
        None => std::path::MAIN_SEPARATOR,
    };
    PathBuf::from(format!("{root}{sep}{name}"))
}

/// Reduce an identifier to characters every filesystem here accepts.
///
/// The same rule the server's own `temp_path_for` and the agent's `WorkArea`
/// apply. A job id is operator- and scanner-derived, so it is not trusted to be
/// a safe path component.
fn sanitise_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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

#[cfg(test)]
mod agent_path_tests {
    use super::*;
    use std::path::Path;

    const CANON: &str = "/mnt/tv/Show/S01E01.mkv";
    const SERVER_TEMP: &str = "/mnt/tv/.work/u1.j1.0.partial.mkv";

    fn view(transport: TransportMode, platform: Option<Platform>, root: &str) -> AgentView<'_> {
        AgentView {
            transport,
            platform,
            workarea_path: root,
        }
    }

    /// The whole point of the mount transport is that the agent resolves the
    /// canonical path itself. Translating it would break the mode that works.
    #[test]
    fn a_mount_agent_still_gets_the_canonical_path() {
        let v = view(TransportMode::Mount, Some(Platform::Linux), "/var/work");
        let p = agent_job_paths(&v, "j1", 0, Path::new(CANON), Path::new(SERVER_TEMP));
        assert_eq!(p.input, Path::new(CANON));
        assert_eq!(p.output, Path::new(SERVER_TEMP));
    }

    /// A streaming agent cannot open the library at all, so neither end of the
    /// job may name it.
    #[test]
    fn a_streaming_agent_gets_both_ends_inside_its_own_work_area() {
        let v = view(TransportMode::Stream, Some(Platform::Linux), "/var/work");
        let p = agent_job_paths(&v, "j1", 0, Path::new(CANON), Path::new(SERVER_TEMP));

        assert_eq!(p.input, Path::new("/var/work/j1.0.src.mkv"));
        assert_eq!(p.output, Path::new("/var/work/j1.0.partial.mkv"));
        for got in [&p.input, &p.output] {
            let s = got.display().to_string();
            assert!(
                !s.contains("/mnt/tv"),
                "a streaming agent must never be handed a library path: {s}"
            );
        }
    }

    /// The server is Linux and the only streaming agent so far is Windows, so
    /// `Path::join` would pick the wrong separator. `argv` is persisted for an
    /// operator to paste into a shell on *that* machine.
    #[test]
    fn a_windows_agent_gets_windows_separators() {
        let v = view(
            TransportMode::Stream,
            Some(Platform::Windows),
            r"C:\Users\jdfalk\work",
        );
        let p = agent_job_paths(&v, "j1", 2, Path::new(CANON), Path::new(SERVER_TEMP));

        assert_eq!(
            p.input.display().to_string(),
            r"C:\Users\jdfalk\work\j1.2.src.mkv"
        );
        assert!(
            !p.output.display().to_string().contains('/'),
            "a mixed-separator path merely happens to work"
        );
    }

    /// A trailing separator on the advertised root must not double up: the
    /// resulting path still has to be pasteable.
    #[test]
    fn a_trailing_separator_on_the_root_is_not_doubled() {
        for root in ["/var/work/", r"C:\work\"] {
            let plat = if root.starts_with('C') {
                Platform::Windows
            } else {
                Platform::Linux
            };
            let v = view(TransportMode::Stream, Some(plat), root);
            let p = agent_job_paths(&v, "j1", 0, Path::new(CANON), Path::new(SERVER_TEMP));
            let s = p.input.display().to_string();
            assert!(!s.contains("//") && !s.contains(r"\\"), "{s}");
        }
    }

    /// Attempts land in different files, or a retry overwrites the staging of
    /// the attempt it is retrying.
    #[test]
    fn each_attempt_gets_its_own_file() {
        let v = view(TransportMode::Stream, Some(Platform::Linux), "/var/work");
        let a = agent_job_paths(&v, "j1", 0, Path::new(CANON), Path::new(SERVER_TEMP));
        let b = agent_job_paths(&v, "j1", 1, Path::new(CANON), Path::new(SERVER_TEMP));
        assert_ne!(a.input, b.input);
        assert_ne!(a.output, b.output);
    }

    /// A job id is scanner- and operator-derived. It is not trusted to be a
    /// safe path component, and a `..` in one must not escape the work area.
    #[test]
    fn a_hostile_job_id_cannot_escape_the_work_area() {
        let v = view(TransportMode::Stream, Some(Platform::Linux), "/var/work");
        let p = agent_job_paths(
            &v,
            "../../etc/cron.d/x",
            0,
            Path::new(CANON),
            Path::new(SERVER_TEMP),
        );
        assert!(
            p.input.starts_with("/var/work"),
            "escaped the work area: {}",
            p.input.display()
        );
        assert!(!p.input.display().to_string().contains(".."));
    }

    /// The input keeps the source's container. Handing ffmpeg a `.mkv` named
    /// `.bin` is a demux that fails for a reason nobody will connect to this.
    #[test]
    fn the_fetched_source_keeps_the_source_extension() {
        let v = view(TransportMode::Stream, Some(Platform::Linux), "/var/work");
        let p = agent_job_paths(
            &v,
            "j1",
            0,
            Path::new("/mnt/tv/ep.mp4"),
            Path::new(SERVER_TEMP),
        );
        assert!(p.input.display().to_string().ends_with(".src.mp4"));
    }
}
