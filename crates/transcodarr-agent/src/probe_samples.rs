// file: crates/transcodarr-agent/src/probe_samples.rs
// version: 1.0.0
// guid: c2cd18ce-20ad-4538-ae7d-0b4c4a97bd8c
// last-edited: 2026-08-16
//! The synthetic clips the trial decodes run against.
//!
//! Probing needs something to decode. Shipping fixtures would mean binary blobs
//! in the repository and a library the agent is not guaranteed to have reached
//! yet, so each clip is generated on the spot from `lavfi` — ten frames of
//! 320x240, which decodes in well under a second and is still enough for a
//! hardware decoder to have to initialise.
//!
//! ## Two things here are measured, not assumed
//!
//! **Profile strings are ffprobe's own text.** A [`DecoderTriple`] is matched by
//! exact equality, and the profile in a job's requirement comes from
//! `FileFacts::video_profile`, which is the `profile` field of ffprobe's JSON
//! verbatim. That text is `High 10`, with a space — not `High10`. Verified
//! byte-for-byte on ffprobe 8.1.2 and on the node's N-126175 build, and against
//! real library media, on 2026-08-16. A spelling invented here would produce a
//! triple that silently matches nothing, and the job would block at
//! `capability` with a reason that reads as a genuine hardware limitation.
//!
//! **Encoders are tried in order, because the first choice is often absent.**
//! The Mac this was developed on has `libsvtav1` and no `libaom-av1`; the GPU
//! node's original ffmpeg was built `--disable-libx264 --disable-libx265` and
//! could only reach H.264 through `libopenh264`. A single hard-coded encoder
//! per codec leaves such a node with no sample, hence every triple `Untested`,
//! hence no video work at all — the failure this list exists to avoid.
//!
//! ## Why a generated sample is checked before it is trusted
//!
//! The fallback encoders do not all honour `-profile:v`. `libopenh264` emits
//! `Constrained Baseline` whatever it is asked for. So a clip is probed after
//! it is written and discarded unless it really carries the profile it was
//! generated for: a mislabelled sample would attach a hardware verdict to the
//! wrong triple, which is worse than having no verdict at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use transcodarr_core::capability::{DecoderKind, DecoderTriple};
use transcodarr_core::plan::BitDepth;

/// Frames in each generated clip.
///
/// Enough that a decoder has to do real work and a partial decode is visible as
/// a short frame count; small enough that seventeen trials are a few seconds.
pub const TRIAL_SAMPLE_FRAMES: u64 = 10;

/// Below this, a file in the sample directory is treated as absent.
///
/// An interrupted encode leaves a header and nothing else, and reusing it would
/// make every subsequent probe fail for a reason that has nothing to do with
/// the hardware.
const MIN_SAMPLE_BYTES: u64 = 1024;

/// A clip we want on disk, and the exact stream it must contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSpec {
    /// ffmpeg codec name, as ffprobe reports it in `codec_name`.
    pub codec: &'static str,
    /// ffprobe's own `profile` text, exactly.
    pub profile: &'static str,
    /// Bit depth of the coded stream.
    pub bit_depth: BitDepth,
    /// Pixel format to encode into.
    pub pix_fmt: &'static str,
    /// `-profile:v` argument, where the encoder needs one to comply.
    pub profile_arg: Option<&'static str>,
}

impl SampleSpec {
    /// A filesystem-safe name for this spec's clip.
    ///
    /// Derived from the fields rather than stored, so a spec cannot acquire a
    /// name that disagrees with what it describes. `High 4:2:2` becomes
    /// `high-4-2-2`.
    pub fn slug(&self) -> String {
        let profile: String = self
            .profile
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        format!("{}_{}_{}", self.codec, profile, self.bit_depth.bits())
    }

    /// The hardware-decode triple this clip answers for.
    pub fn nvdec_triple(&self) -> DecoderTriple {
        DecoderTriple {
            codec: self.codec.to_string(),
            profile: self.profile.to_string(),
            bit_depth: self.bit_depth,
            kind: DecoderKind::Nvdec,
        }
    }

    /// The software-decode triple this clip answers for.
    ///
    /// Profile-free by design; see [`software_triples`].
    pub fn software_triple(&self) -> DecoderTriple {
        DecoderTriple {
            codec: self.codec.to_string(),
            profile: String::new(),
            bit_depth: self.bit_depth,
            kind: DecoderKind::Software,
        }
    }
}

/// The clips worth generating, most representative first.
///
/// Order is load-bearing in one place: the software triple for a codec and
/// depth is trialled against the *first* listed clip that exists, so the
/// canonical profile for each codec comes first and the exotic ones after.
///
/// The H.264 set is the measured Turing NVDEC boundary rather than a tidy list.
/// `Constrained Baseline`, `Main` and `High` decode in hardware at 8-bit;
/// `High 10` and `High 4:2:2` do not, and both fail *soft* — exit 0, correct
/// output, silently on the CPU. Those two are the reason profile is part of the
/// key at all: they share a codec and, in the 4:2:2 case, a bit depth with the
/// ones that work.
pub const CANDIDATE_SAMPLES: &[SampleSpec] = &[
    SampleSpec {
        codec: "h264",
        profile: "High",
        bit_depth: BitDepth::Eight,
        pix_fmt: "yuv420p",
        profile_arg: Some("high"),
    },
    SampleSpec {
        codec: "h264",
        profile: "Main",
        bit_depth: BitDepth::Eight,
        pix_fmt: "yuv420p",
        profile_arg: Some("main"),
    },
    SampleSpec {
        codec: "h264",
        profile: "Constrained Baseline",
        bit_depth: BitDepth::Eight,
        pix_fmt: "yuv420p",
        profile_arg: Some("baseline"),
    },
    SampleSpec {
        codec: "h264",
        profile: "High 4:2:2",
        bit_depth: BitDepth::Eight,
        pix_fmt: "yuv422p",
        profile_arg: Some("high422"),
    },
    SampleSpec {
        codec: "h264",
        profile: "High 10",
        bit_depth: BitDepth::Ten,
        pix_fmt: "yuv420p10le",
        profile_arg: Some("high10"),
    },
    SampleSpec {
        codec: "hevc",
        profile: "Main",
        bit_depth: BitDepth::Eight,
        pix_fmt: "yuv420p",
        profile_arg: None,
    },
    SampleSpec {
        codec: "hevc",
        profile: "Main 10",
        bit_depth: BitDepth::Ten,
        pix_fmt: "yuv420p10le",
        profile_arg: None,
    },
    SampleSpec {
        codec: "av1",
        profile: "Main",
        bit_depth: BitDepth::Eight,
        pix_fmt: "yuv420p",
        profile_arg: None,
    },
    SampleSpec {
        codec: "vp9",
        profile: "Profile 0",
        bit_depth: BitDepth::Eight,
        pix_fmt: "yuv420p",
        profile_arg: None,
    },
    SampleSpec {
        codec: "mpeg2video",
        profile: "Main",
        bit_depth: BitDepth::Eight,
        pix_fmt: "yuv420p",
        profile_arg: None,
    },
];

/// Encoders that can produce this codec, most faithful first.
///
/// "Faithful" means most likely to honour the requested profile and pixel
/// format: `libx264` can be asked for any H.264 profile, `h264_nvenc` for the
/// common ones, and `libopenh264` produces `Constrained Baseline` regardless.
/// They are all listed anyway, because a clip in the wrong profile is caught by
/// the check in [`ensure`] and a node with only the last of them still gets one
/// H.264 triple probed instead of none.
pub fn encoders_for(codec: &str) -> &'static [&'static str] {
    match codec {
        "h264" => &["libx264", "h264_nvenc", "libopenh264"],
        "hevc" => &["libx265", "hevc_nvenc"],
        "av1" => &["libsvtav1", "libaom-av1", "librav1e", "av1_nvenc"],
        "vp9" => &["libvpx-vp9"],
        "mpeg2video" => &["mpeg2video"],
        _ => &[],
    }
}

/// argv to generate one clip.
///
/// A vector, never a shell string: the output path is a directory the operator
/// named and has no business being re-parsed by `sh`.
pub fn encode_sample_argv(spec: &SampleSpec, encoder: &str, out: &Path) -> Vec<String> {
    let mut argv: Vec<String> = [
        "-hide_banner",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=10:duration=1",
        "-frames:v",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.push(TRIAL_SAMPLE_FRAMES.to_string());
    argv.push("-c:v".into());
    argv.push(encoder.into());
    if let Some(p) = spec.profile_arg {
        argv.push("-profile:v".into());
        argv.push(p.into());
    }
    argv.push("-pix_fmt".into());
    argv.push(spec.pix_fmt.into());
    argv.push(out.to_string_lossy().into_owned());
    argv
}

/// The clips that exist, by [`SampleSpec::slug`].
#[derive(Debug, Default)]
pub struct SampleSet {
    by_slug: BTreeMap<String, PathBuf>,
}

impl SampleSet {
    /// The clip for this spec, if one was produced.
    pub fn get(&self, spec: &SampleSpec) -> Option<&Path> {
        self.by_slug.get(&spec.slug()).map(PathBuf::as_path)
    }

    /// How many clips are available.
    pub fn len(&self) -> usize {
        self.by_slug.len()
    }

    /// Whether no clip could be produced at all.
    pub fn is_empty(&self) -> bool {
        self.by_slug.is_empty()
    }
}

/// Generate the clips this ffmpeg can produce, reusing any already present.
///
/// Idempotent, and deliberately strict about what counts as already present: an
/// existing file is reused only if it is large enough to be real *and* still
/// carries the profile it was generated for. An ffmpeg upgrade that changes
/// what an encoder emits therefore regenerates rather than silently attaching
/// yesterday's label to today's stream.
///
/// A spec that no available encoder can satisfy is simply absent from the set,
/// which downstream becomes `Untested` — never a guess.
pub fn ensure(ffmpeg: &str, ffprobe: &str, dir: &Path, encoder_listing: &str) -> SampleSet {
    let mut set = SampleSet::default();
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "cannot create the trial sample directory; no decoder will be probed");
        return set;
    }

    for spec in CANDIDATE_SAMPLES {
        let slug = spec.slug();
        let final_path = dir.join(format!("{slug}.mkv"));

        if is_usable(ffprobe, &final_path, spec) {
            set.by_slug.insert(slug, final_path);
            continue;
        }

        for encoder in encoders_for(spec.codec) {
            if !crate::survey::lists_name(encoder_listing, encoder) {
                continue;
            }
            let tmp = dir.join(format!("{slug}.tmp.mkv"));
            let argv = encode_sample_argv(spec, encoder, &tmp);
            if !run_quiet(ffmpeg, &argv) {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            // The encoder ran, but running is not complying: `libopenh264`
            // exits 0 having produced Constrained Baseline for any profile it
            // was asked for.
            if !is_usable(ffprobe, &tmp, spec) {
                tracing::debug!(
                    codec = spec.codec, profile = spec.profile, %encoder,
                    "encoder did not produce the requested profile; trying the next"
                );
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            if std::fs::rename(&tmp, &final_path).is_err() {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            tracing::debug!(codec = spec.codec, profile = spec.profile, %encoder, "generated a trial sample");
            set.by_slug.insert(slug, final_path);
            break;
        }
    }
    set
}

/// Whether this file is a real clip carrying exactly the stream `spec` names.
fn is_usable(ffprobe: &str, path: &Path, spec: &SampleSpec) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.len() >= MIN_SAMPLE_BYTES => {}
        _ => return false,
    }
    probed_stream(ffprobe, path)
        .is_some_and(|(codec, profile)| codec == spec.codec && profile == spec.profile)
}

/// `(codec_name, profile)` of a file's first video stream.
fn probed_stream(ffprobe: &str, path: &Path) -> Option<(String, String)> {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,profile",
            "-of",
            "default=nw=1:nk=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut codec = None;
    let mut profile = None;
    for line in text.lines() {
        // ffprobe prints `profile=High 4:2:2` -- split once, because the value
        // may itself contain nothing helpful to split on.
        match line.split_once('=') {
            Some(("codec_name", v)) => codec = Some(v.trim().to_string()),
            Some(("profile", v)) => profile = Some(v.trim().to_string()),
            _ => {}
        }
    }
    Some((codec?, profile?))
}

/// Run a tool, discarding its output, and report whether it succeeded.
fn run_quiet(bin: &str, argv: &[String]) -> bool {
    Command::new(bin)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The software-decode triples to trial, each with the clip to trial it on.
///
/// One per codec and depth, with an empty profile. Software decode does not
/// vary by profile — ffmpeg either has the decoder compiled in or it does not —
/// and keying it by profile would mean every profile absent from
/// [`CANDIDATE_SAMPLES`] blocked the *CPU* path. `Main` H.264 is the common
/// case that would have been caught by that: real library media carries it and
/// no candidate list drafted from memory contained it.
pub fn software_triples(samples: &SampleSet) -> Vec<(DecoderTriple, PathBuf)> {
    let mut seen: BTreeMap<(String, u8), PathBuf> = BTreeMap::new();
    for spec in CANDIDATE_SAMPLES {
        let Some(path) = samples.get(spec) else {
            continue;
        };
        // First listed clip for a codec and depth wins, which is why the
        // canonical profile is listed first.
        seen.entry((spec.codec.to_string(), spec.bit_depth.bits()))
            .or_insert_with(|| path.to_path_buf());
    }
    CANDIDATE_SAMPLES
        .iter()
        .filter_map(|spec| {
            let path = seen.remove(&(spec.codec.to_string(), spec.bit_depth.bits()))?;
            Some((spec.software_triple(), path))
        })
        .collect()
}

/// The hardware-decode triples to trial, each with the clip to trial it on.
///
/// Empty unless this ffmpeg advertises the `cuda` hwaccel: without it every
/// trial would decode in software and report a hardware verdict that is purely
/// a description of the CPU.
pub fn nvdec_triples(samples: &SampleSet, hwaccel_listing: &str) -> Vec<(DecoderTriple, PathBuf)> {
    if !hwaccel_listing
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("cuda"))
    {
        return Vec::new();
    }
    CANDIDATE_SAMPLES
        .iter()
        .filter_map(|spec| Some((spec.nvdec_triple(), samples.get(spec)?.to_path_buf())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_is_filesystem_safe_and_names_what_it_describes() {
        let spec = CANDIDATE_SAMPLES
            .iter()
            .find(|s| s.profile == "High 4:2:2")
            .expect("the 4:2:2 spec is the measured NVDEC boundary case");
        assert_eq!(spec.slug(), "h264_high-4-2-2_8");
        assert!(!spec.slug().contains(['/', ':']));
    }

    /// Every candidate profile is ffprobe's own spelling, measured on
    /// 2026-08-16. `High10` and `Profile0` are what a list drafted from memory
    /// says, and they match nothing.
    #[test]
    fn candidate_profiles_use_ffprobes_spelling_with_spaces() {
        let profiles: Vec<&str> = CANDIDATE_SAMPLES.iter().map(|s| s.profile).collect();
        assert!(profiles.contains(&"High 10"), "{profiles:?}");
        assert!(profiles.contains(&"Main 10"), "{profiles:?}");
        assert!(profiles.contains(&"Profile 0"), "{profiles:?}");
        for p in profiles {
            assert!(
                !p.chars().any(|c| c.is_ascii_digit()) || p.contains(' ') || p == "Profile 0",
                "{p:?} looks like a from-memory spelling with the space removed"
            );
        }
    }

    /// The two measured Turing soft-fallback cases must both be probed, or the
    /// card is advertised as able to decode them.
    #[test]
    fn the_measured_turing_fallback_cases_are_candidates() {
        for (codec, profile) in [("h264", "High 10"), ("h264", "High 4:2:2")] {
            assert!(
                CANDIDATE_SAMPLES
                    .iter()
                    .any(|s| s.codec == codec && s.profile == profile),
                "{codec}/{profile} is a measured NVDEC soft fallback and must be trialled"
            );
        }
    }

    /// `Main` H.264 is what real library media carries and what no from-memory
    /// candidate list contained.
    #[test]
    fn the_common_real_world_profile_is_a_candidate() {
        assert!(
            CANDIDATE_SAMPLES
                .iter()
                .any(|s| s.codec == "h264" && s.profile == "Main")
        );
    }

    #[test]
    fn ten_bit_specs_encode_into_a_ten_bit_pixel_format() {
        for spec in CANDIDATE_SAMPLES {
            let ten = spec.bit_depth == BitDepth::Ten;
            assert_eq!(
                ten,
                spec.pix_fmt.contains("10"),
                "{} claims {:?} but encodes into {}",
                spec.slug(),
                spec.bit_depth,
                spec.pix_fmt
            );
        }
    }

    #[test]
    fn every_candidate_codec_has_at_least_one_encoder_to_try() {
        for spec in CANDIDATE_SAMPLES {
            assert!(
                !encoders_for(spec.codec).is_empty(),
                "{} can never be generated",
                spec.slug()
            );
        }
    }

    /// The node's original ffmpeg had neither libx264 nor libx265; the list is
    /// ordered so such a build still reaches H.264 through another encoder.
    #[test]
    fn h264_has_a_fallback_beyond_libx264() {
        let list = encoders_for("h264");
        assert_eq!(list.first(), Some(&"libx264"));
        assert!(
            list.len() > 1,
            "a build without libx264 must still have something to try"
        );
    }

    #[test]
    fn the_output_path_is_its_own_argv_element() {
        let spec = &CANDIDATE_SAMPLES[0];
        let out = Path::new("/tmp/a dir/with space.mkv");
        let argv = encode_sample_argv(spec, "libx264", out);
        assert_eq!(argv.last().map(String::as_str), out.to_str());
    }

    #[test]
    fn a_profile_argument_is_passed_only_when_the_spec_names_one() {
        let with = CANDIDATE_SAMPLES
            .iter()
            .find(|s| s.profile_arg.is_some())
            .unwrap();
        let without = CANDIDATE_SAMPLES
            .iter()
            .find(|s| s.profile_arg.is_none())
            .unwrap();
        assert!(
            encode_sample_argv(with, "libx264", Path::new("o.mkv"))
                .contains(&"-profile:v".to_string())
        );
        assert!(
            !encode_sample_argv(without, "libx265", Path::new("o.mkv"))
                .contains(&"-profile:v".to_string())
        );
    }

    #[test]
    fn the_pixel_format_is_passed_as_two_adjacent_elements() {
        let spec = CANDIDATE_SAMPLES
            .iter()
            .find(|s| s.bit_depth == BitDepth::Ten)
            .unwrap();
        let argv = encode_sample_argv(spec, "libx265", Path::new("o.mkv"));
        let i = argv.iter().position(|a| a == "-pix_fmt").unwrap();
        assert_eq!(argv[i + 1], spec.pix_fmt);
    }

    fn set_with(slugs: &[&str]) -> SampleSet {
        SampleSet {
            by_slug: slugs
                .iter()
                .map(|s| ((*s).to_string(), PathBuf::from(format!("/s/{s}.mkv"))))
                .collect(),
        }
    }

    /// Without `cuda` advertised, a hardware trial would decode in software and
    /// report a verdict about the CPU.
    #[test]
    fn no_hardware_triple_is_probed_when_cuda_is_absent() {
        let samples = set_with(&["h264_high_8"]);
        assert!(nvdec_triples(&samples, "videotoolbox\nvaapi\n").is_empty());
        assert_eq!(nvdec_triples(&samples, "cuda\n").len(), 1);
    }

    #[test]
    fn a_hardware_triple_carries_the_exact_profile() {
        let samples = set_with(&["h264_high-10_10"]);
        let t = nvdec_triples(&samples, "cuda\n");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0.profile, "High 10");
        assert_eq!(t[0].0.kind, DecoderKind::Nvdec);
    }

    /// The asymmetry that keeps `Main` H.264 -- and every profile nobody
    /// thought to trial -- off the CPU path's blocklist.
    #[test]
    fn software_triples_are_profile_free_and_one_per_codec_and_depth() {
        let samples = set_with(&[
            "h264_high_8",
            "h264_main_8",
            "h264_constrained-baseline_8",
            "h264_high-10_10",
        ]);
        let t = software_triples(&samples);
        assert_eq!(t.len(), 2, "three 8-bit clips are still one 8-bit triple");
        for (triple, _) in &t {
            assert!(triple.profile.is_empty(), "{triple:?}");
            assert_eq!(triple.kind, DecoderKind::Software);
        }
        let depths: Vec<_> = t.iter().map(|(x, _)| x.bit_depth).collect();
        assert!(depths.contains(&BitDepth::Eight) && depths.contains(&BitDepth::Ten));
    }

    /// A codec with no clip is absent rather than guessed at.
    #[test]
    fn a_codec_with_no_sample_yields_no_triple() {
        let t = software_triples(&set_with(&[]));
        assert!(t.is_empty());
    }
}
