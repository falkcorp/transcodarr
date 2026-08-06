// file: crates/transcodarr-core/src/paths.rs
// version: 1.1.0
// guid: a3f7e015-8c24-4d69-b0a3-6e5f19c8d720
// last-edited: 2026-08-06
//! Output-path derivation.
//!
//! These functions are pure: they never touch the filesystem. The original
//! implementations in `src/main.rs` called `Path::canonicalize` and
//! `env::current_dir` inline, which made them untestable without a real tree
//! and would have dragged I/O into this crate. Both are now the caller's job —
//! resolve paths in the I/O layer, then hand absolute paths in here.

use std::path::{Path, PathBuf};

use crate::CoreError;

/// Derive a filename stem using the **last** `.` before the extension.
///
/// This avoids truncating names that legitimately contain dots, such as
/// `Episode 1.11 - Title.mkv`, which `Path::file_stem` would cut at the first
/// dot it finds from the right — losing `.11` in names with a trailing
/// version-like segment. For dotfiles (`.bashrc`) or names with no extension,
/// the whole filename is returned.
///
/// ```
/// use std::path::Path;
/// use transcodarr_core::paths::strict_stem;
/// assert_eq!(strict_stem(Path::new("Episode 1.11 - Title.mkv")), "Episode 1.11 - Title");
/// assert_eq!(strict_stem(Path::new(".bashrc")), ".bashrc");
/// ```
pub fn strict_stem(path: &Path) -> String {
    if let (Some(name_os), Some(ext_os)) = (path.file_name(), path.extension()) {
        if let (Some(name), Some(ext)) = (name_os.to_str(), ext_os.to_str()) {
            if !ext.is_empty() {
                let needle = format!(".{}", ext);
                if let Some(pos) = name.rfind(&needle) {
                    if pos > 0 {
                        return name[..pos].to_string();
                    }
                }
            }
            // No recognisable extension position; return the full name.
            return name.to_string();
        }
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output")
        .to_string()
}

/// Compare two paths for equivalence, lexically.
///
/// **The caller is responsible for canonicalising first** when symlink
/// resolution matters. This function cannot do it: resolving a symlink is a
/// filesystem read, and this crate performs no I/O. Passing two non-canonical
/// paths that happen to point at the same file will report `false`.
pub fn paths_equivalent(a: &Path, b: &Path) -> bool {
    a == b
}

/// Where a replaced original is retained.
///
/// The path *below the library root* is preserved under the trash directory,
/// rather than dropping every original into one flat folder. Two files called
/// `S01E01.mkv` in different show directories are the common case, not the
/// exotic one, and a flat trash makes the second one silently overwrite the
/// first -- destroying the very copy the trash exists to keep.
///
/// A `final_path` outside `library_root` falls back to the bare file name. That
/// should not happen; if it does, retaining the file somewhere is better than
/// building a path that escapes the trash directory entirely.
pub fn trash_path_for(trash_dir: &Path, library_root: &Path, final_path: &Path) -> PathBuf {
    let relative = final_path
        .strip_prefix(library_root)
        .ok()
        .filter(|r| !r.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| final_path.file_name().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("recovered"));

    // Only `Normal` components survive, so whatever arrives the result stays
    // inside the trash directory.
    let safe: PathBuf = relative
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect();
    trash_dir.join(if safe.as_os_str().is_empty() {
        PathBuf::from("recovered")
    } else {
        safe
    })
}

/// Build a sibling output path with a `_transcoded` suffix and the given
/// extension, used whenever writing next to the input would otherwise clobber
/// it.
pub fn suffixed_output(input_path: &Path, out_ext: &str) -> PathBuf {
    let parent = input_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = strict_stem(input_path);
    let mut final_name = String::with_capacity(stem.len() + 12 + out_ext.len());
    final_name.push_str(&stem);
    final_name.push_str("_transcoded.");
    final_name.push_str(out_ext);
    parent.join(final_name)
}

/// Resolve a safe output path from an input and an optional user-supplied
/// output.
///
/// Rules, unchanged from the original CLI behaviour:
///
/// - A user-supplied output that differs from the input is used as given,
///   resolved against `base_dir` when relative.
/// - A user-supplied output *identical* to the input, or no output at all,
///   yields `<stem>_transcoded.<default_ext>` beside the input. Overwriting the
///   source in place is never the default.
///
/// `base_dir` replaces the original's `env::current_dir()` call so this stays
/// pure. Both `input` and `base_dir` should already be absolute and
/// canonicalised by the caller if symlink-correct comparison is required.
pub fn resolve_output_path(
    input: &Path,
    output_opt: Option<&Path>,
    default_ext: &str,
    base_dir: &Path,
) -> Result<PathBuf, CoreError> {
    if input.file_name().is_none() {
        return Err(CoreError::InvalidPath(input.display().to_string()));
    }

    if let Some(out) = output_opt {
        let out_abs = if out.is_absolute() {
            out.to_path_buf()
        } else {
            base_dir.join(out)
        };
        if paths_equivalent(input, &out_abs) {
            return Ok(suffixed_output(input, default_ext));
        }
        return Ok(out_abs);
    }

    Ok(suffixed_output(input, default_ext))
}

/// Mirror `input` from `in_root` into `out_root`, replacing its extension.
///
/// When the two roots are equivalent the mirrored path would equal the input,
/// so a `_transcoded` suffix is applied instead — the in-place batch case.
pub fn plan_output_path(
    input: &Path,
    in_root: &Path,
    out_root: &Path,
    ext: &str,
) -> Result<PathBuf, CoreError> {
    if paths_equivalent(in_root, out_root) {
        return Ok(suffixed_output(input, ext));
    }
    let rel = input.strip_prefix(in_root).map_err(|_| {
        CoreError::InvalidPath(format!(
            "{} is not under {}",
            input.display(),
            in_root.display()
        ))
    })?;
    let mut out = out_root.join(rel);
    out.set_extension(ext);
    Ok(out)
}

#[cfg(test)]
mod trash_tests {
    use super::*;

    /// Two shows both have an `S01E01.mkv`. A flat trash directory makes the
    /// second replacement destroy the first original — the exact copy the trash
    /// exists to preserve.
    #[test]
    fn the_path_below_the_library_root_is_preserved() {
        let a = trash_path_for(
            Path::new("/mnt/tv/.trash"),
            Path::new("/mnt/tv"),
            Path::new("/mnt/tv/Show A/S01E01.mkv"),
        );
        let b = trash_path_for(
            Path::new("/mnt/tv/.trash"),
            Path::new("/mnt/tv"),
            Path::new("/mnt/tv/Show B/S01E01.mkv"),
        );
        assert_ne!(a, b);
        assert_eq!(a, Path::new("/mnt/tv/.trash/Show A/S01E01.mkv"));
    }

    /// Whatever arrives, the result stays inside the trash directory.
    #[test]
    fn the_result_never_escapes_the_trash_directory() {
        let p = trash_path_for(
            Path::new("/mnt/tv/.trash"),
            Path::new("/mnt/tv"),
            Path::new("/mnt/tv/../../etc/passwd"),
        );
        assert!(p.starts_with("/mnt/tv/.trash"), "{p:?}");
        assert!(!p.to_string_lossy().contains(".."));
    }

    /// A path outside the library still gets retained somewhere rather than
    /// producing a path that leaves the trash directory.
    #[test]
    fn a_path_outside_the_library_falls_back_to_its_name() {
        let p = trash_path_for(
            Path::new("/t"),
            Path::new("/mnt/tv"),
            Path::new("/elsewhere/a.mkv"),
        );
        assert_eq!(p, Path::new("/t/a.mkv"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_stem_keeps_dots_inside_names() {
        assert_eq!(
            strict_stem(Path::new("/x/Episode 1.11 - Title.mkv")),
            "Episode 1.11 - Title"
        );
    }

    #[test]
    fn strict_stem_handles_dotfiles_and_extensionless() {
        assert_eq!(strict_stem(Path::new("/x/.bashrc")), ".bashrc");
        assert_eq!(strict_stem(Path::new("/x/README")), "README");
    }

    #[test]
    fn no_output_yields_suffixed_sibling() {
        let got =
            resolve_output_path(Path::new("/m/a.mp4"), None, "mkv", Path::new("/cwd")).unwrap();
        assert_eq!(got, PathBuf::from("/m/a_transcoded.mkv"));
    }

    #[test]
    fn output_equal_to_input_never_overwrites_it() {
        let got = resolve_output_path(
            Path::new("/m/a.mp4"),
            Some(Path::new("/m/a.mp4")),
            "mkv",
            Path::new("/cwd"),
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/m/a_transcoded.mkv"));
    }

    #[test]
    fn relative_output_resolves_against_base_dir_not_process_cwd() {
        let got = resolve_output_path(
            Path::new("/m/a.mp4"),
            Some(Path::new("out.mkv")),
            "mkv",
            Path::new("/base"),
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/base/out.mkv"));
    }

    #[test]
    fn batch_mirrors_structure_into_a_different_root() {
        let got = plan_output_path(
            Path::new("/in/Show/S01/ep.mp4"),
            Path::new("/in"),
            Path::new("/out"),
            "mkv",
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/out/Show/S01/ep.mkv"));
    }

    #[test]
    fn batch_in_place_suffixes_instead_of_clobbering() {
        let got = plan_output_path(
            Path::new("/in/Show/ep.mp4"),
            Path::new("/in"),
            Path::new("/in"),
            "mkv",
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/in/Show/ep_transcoded.mkv"));
    }

    #[test]
    fn input_outside_the_root_is_an_error_not_a_silent_escape() {
        let err = plan_output_path(
            Path::new("/elsewhere/ep.mp4"),
            Path::new("/in"),
            Path::new("/out"),
            "mkv",
        );
        assert!(matches!(err, Err(CoreError::InvalidPath(_))));
    }
}
