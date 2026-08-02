// file: crates/transcodarr-core/src/preset.rs
// version: 1.0.0
// guid: c94e6f28-70b1-4a53-8d2c-1f6a3b09e75d
// last-edited: 2026-08-01
//! Named quality presets.
//!
//! Replaces the original `apply_preset`, which used a string sentinel — it
//! compared `vcodec == "libx264"` to infer "the user did not pass `--vcodec`".
//! That silently misfired the moment someone explicitly asked for libx264.
//! Intent is now carried by `Option<EncoderId>`: `None` means unspecified,
//! which is a fact the type system can hold and a string cannot.

use crate::CoreError;
use crate::plan::{EncodePlan, EncoderId};

/// One named preset: the codecs it selects and the ffmpeg arguments it adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// Canonical name.
    pub name: &'static str,
    /// Alternative names accepted for the same preset.
    pub aliases: &'static [&'static str],
    /// Video encoder this preset selects.
    pub video: EncoderId,
    /// Audio encoder this preset selects.
    pub audio: EncoderId,
    /// Arguments appended before any user-supplied extras.
    pub args: &'static [&'static str],
    /// One-line summary, shown when a lookup fails.
    pub description: &'static str,
}

/// The built-in preset table.
pub const BUILTIN: &[Preset] = &[
    Preset {
        name: "original-h265",
        aliases: &["original"],
        video: EncoderId::Libx265,
        audio: EncoderId::Aac,
        // CRF 18 is widely treated as visually lossless for x265; `slow` buys
        // compression efficiency at encode time, which is the right trade for
        // an archive pass that runs once.
        args: &["-crf", "18", "-preset", "slow", "-b:a", "256k"],
        description: "archive quality: h265 CRF 18 slow, AAC 256k",
    },
    Preset {
        name: "tv-h265-fast",
        aliases: &["tv-fast"],
        video: EncoderId::Libx265,
        audio: EncoderId::Aac,
        args: &["-crf", "22", "-preset", "medium", "-b:a", "160k"],
        description: "TV episodes: h265 CRF 22 medium, AAC 160k",
    },
    Preset {
        name: "movie-quality",
        aliases: &["movie"],
        video: EncoderId::Libx265,
        audio: EncoderId::Aac,
        args: &["-crf", "16", "-preset", "slow", "-b:a", "320k"],
        description: "films: h265 CRF 16 slow, AAC 320k",
    },
];

/// Every accepted name, canonical and alias, as a human-readable list.
pub fn valid_names() -> String {
    BUILTIN
        .iter()
        .map(|p| {
            if p.aliases.is_empty() {
                p.name.to_string()
            } else {
                format!("{} (aliases: {})", p.name, p.aliases.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Look up a preset by canonical name or alias.
///
/// An unknown name is an error. The original swallowed it in a `_ => {}` arm
/// and carried on with default codecs, so a typo produced a full run at the
/// wrong settings that looked entirely successful.
pub fn lookup(name: &str) -> Result<&'static Preset, CoreError> {
    BUILTIN
        .iter()
        .find(|p| p.name == name || p.aliases.contains(&name))
        .ok_or_else(|| CoreError::UnknownPreset {
            name: name.to_string(),
            valid: valid_names(),
        })
}

/// Resolve a preset plus explicit overrides into a concrete [`EncodePlan`].
///
/// Precedence, unchanged from the original CLI:
///
/// 1. A preset supplies both codecs and a set of arguments.
/// 2. An explicit `vcodec` / `acodec` overrides whatever the preset chose.
/// 3. User `extra` arguments are appended **after** the preset's own, so a
///    user-supplied `-crf` wins over the preset's.
///
/// `default_video` / `default_audio` apply only when neither a preset nor an
/// explicit override provides one.
pub fn apply(
    preset: Option<&str>,
    vcodec: Option<EncoderId>,
    acodec: Option<EncoderId>,
    extra: &[String],
    default_video: EncoderId,
    default_audio: EncoderId,
) -> Result<EncodePlan, CoreError> {
    let found = preset.map(lookup).transpose()?;

    let video = vcodec.or(found.map(|p| p.video)).unwrap_or(default_video);
    let audio = acodec.or(found.map(|p| p.audio)).unwrap_or(default_audio);

    let mut args: Vec<String> = found
        .map(|p| p.args.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    args.extend(extra.iter().cloned());

    Ok(EncodePlan {
        video_codec: video,
        audio_codec: audio,
        pix_fmt: None,
        extra_args: args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_preset_is_an_error_not_a_silent_default() {
        let err = apply(
            Some("does-not-exist"),
            None,
            None,
            &[],
            EncoderId::Libx264,
            EncoderId::Aac,
        );
        match err {
            Err(CoreError::UnknownPreset { name, valid }) => {
                assert_eq!(name, "does-not-exist");
                assert!(valid.contains("original-h265"));
            }
            other => panic!("expected UnknownPreset, got {other:?}"),
        }
    }

    #[test]
    fn aliases_resolve_to_the_same_preset() {
        assert_eq!(
            lookup("original").unwrap(),
            lookup("original-h265").unwrap()
        );
        assert_eq!(lookup("tv-fast").unwrap(), lookup("tv-h265-fast").unwrap());
        assert_eq!(lookup("movie").unwrap(), lookup("movie-quality").unwrap());
    }

    #[test]
    fn explicit_codec_overrides_the_preset() {
        let plan = apply(
            Some("original-h265"),
            Some(EncoderId::Libx264),
            None,
            &[],
            EncoderId::Libx264,
            EncoderId::Aac,
        )
        .unwrap();
        assert_eq!(plan.video_codec, EncoderId::Libx264);
    }

    /// The regression the sentinel caused: asking for libx264 explicitly used
    /// to be indistinguishable from not asking at all, so the preset silently
    /// replaced it with libx265.
    #[test]
    fn explicitly_choosing_the_old_default_is_honoured() {
        let plan = apply(
            Some("original-h265"),
            Some(EncoderId::Libx264),
            Some(EncoderId::Aac),
            &[],
            EncoderId::Libx264,
            EncoderId::Aac,
        )
        .unwrap();
        assert_eq!(
            plan.video_codec,
            EncoderId::Libx264,
            "an explicit --vcodec libx264 must survive the preset"
        );
    }

    #[test]
    fn user_extras_are_appended_after_preset_args() {
        let plan = apply(
            Some("original-h265"),
            None,
            None,
            &["-crf".into(), "30".into()],
            EncoderId::Libx264,
            EncoderId::Aac,
        )
        .unwrap();
        let positions: Vec<usize> = plan
            .extra_args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-crf")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(positions.len(), 2, "preset crf plus user crf");
        assert_eq!(plan.extra_args[positions[1] + 1], "30", "user value last");
    }

    #[test]
    fn no_preset_falls_back_to_the_supplied_defaults() {
        let plan = apply(None, None, None, &[], EncoderId::Libx265, EncoderId::Aac).unwrap();
        assert_eq!(plan.video_codec, EncoderId::Libx265);
        assert_eq!(plan.audio_codec, EncoderId::Aac);
        assert!(plan.extra_args.is_empty());
    }
}
