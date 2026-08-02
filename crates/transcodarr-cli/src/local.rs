// file: crates/transcodarr-cli/src/local.rs
// version: 1.0.0
// guid: 8d3c1b57-4e29-40fa-b6d8-25a7c091e463
// last-edited: 2026-08-01
//! The `local` subcommand: probe, transcode and batch, run right here.
//!
//! This module owns the I/O — spawning ffmpeg, walking directories, touching
//! the filesystem. Every decision it makes (which output path, which preset,
//! which argv) comes from `transcodarr-core`, so the same logic will drive the
//! orchestrator without a second implementation to keep in sync.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use transcodarr_core::paths;
use transcodarr_core::plan::{EncoderId, JobPaths, build_ffmpeg_argv_raw, format_command};
use transcodarr_core::preset;

#[derive(Subcommand, Debug)]
pub enum LocalCommand {
    /// Show media info via ffprobe (optionally as JSON)
    Info {
        /// Input media file
        input: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Transcode a file while preserving metadata
    Transcode {
        /// Input media file
        input: String,
        /// Optional output media file; if omitted, writes next to the input as
        /// `<name>_transcoded.mkv`
        output: Option<String>,
        /// Preset name (e.g., original-h265)
        #[arg(long)]
        preset: Option<String>,
        /// Video codec (e.g., libx264, libx265, copy) [default: libx264]
        #[arg(long)]
        vcodec: Option<String>,
        /// Audio codec (e.g., aac, ac3, copy) [default: aac]
        #[arg(long)]
        acodec: Option<String>,
        /// Extra ffmpeg args (passed as-is after standard args)
        #[arg(long, num_args = 0.., value_delimiter = ' ')]
        extra: Vec<String>,
        /// Dry run: print the command without executing
        #[arg(long)]
        dry_run: bool,
    },
    /// Batch transcode a directory recursively (default: h265+aac)
    Batch {
        /// Input directory to scan recursively
        input_dir: String,
        /// Output directory (mirrors input structure)
        output_dir: String,
        /// Preset name (e.g., original-h265)
        #[arg(long)]
        preset: Option<String>,
        /// Video codec (e.g., libx265) [default: libx265]
        #[arg(long)]
        vcodec: Option<String>,
        /// Audio codec (e.g., aac, ac3) [default: aac]
        #[arg(long)]
        acodec: Option<String>,
        /// Output file extension (e.g., mkv, mp4)
        #[arg(long, default_value = "mkv")]
        ext: String,
        /// File extensions to process (comma-separated)
        #[arg(long, default_value = "mp4,mkv,avi,mov,m4v,ts")]
        input_exts: String,
        /// Extra ffmpeg args (passed as-is after standard args)
        #[arg(long, num_args = 0.., value_delimiter = ' ')]
        extra: Vec<String>,
        /// Dry run: print commands without executing
        #[arg(long)]
        dry_run: bool,
    },
}

/// Dispatch a `local` subcommand.
pub fn run(cmd: LocalCommand) -> Result<()> {
    match cmd {
        LocalCommand::Info { input, json } => info(&input, json),
        LocalCommand::Transcode {
            input,
            output,
            preset,
            vcodec,
            acodec,
            extra,
            dry_run,
        } => transcode_one(
            &input,
            output.as_deref(),
            preset.as_deref(),
            vcodec.as_deref(),
            acodec.as_deref(),
            &extra,
            dry_run,
        ),
        LocalCommand::Batch {
            input_dir,
            output_dir,
            preset,
            vcodec,
            acodec,
            ext,
            input_exts,
            extra,
            dry_run,
        } => batch(
            &input_dir,
            &output_dir,
            preset.as_deref(),
            vcodec.as_deref(),
            acodec.as_deref(),
            &ext,
            &input_exts,
            &extra,
            dry_run,
        ),
    }
}

/// Make a path absolute against `base`, then canonicalise if it exists.
///
/// `transcodarr-core` is pure and cannot resolve symlinks, so path resolution
/// happens here and fully-resolved paths are handed in. Canonicalisation is
/// best-effort: an output path that does not exist yet cannot be canonicalised,
/// and the absolute form is correct for it.
fn resolved(p: &Path, base: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    abs.canonicalize().unwrap_or(abs)
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve codec choices, honouring preset then explicit override.
///
/// Returns plain strings because the `local` path has always accepted any
/// ffmpeg codec name, not just the ones `EncoderId` knows about.
fn resolve_codecs(
    preset_name: Option<&str>,
    vcodec: Option<&str>,
    acodec: Option<&str>,
    extra: &[String],
    default_video: EncoderId,
    default_audio: EncoderId,
) -> Result<(String, String, Vec<String>)> {
    let plan = preset::apply(preset_name, None, None, extra, default_video, default_audio)?;
    let v = vcodec
        .map(str::to_string)
        .unwrap_or_else(|| plan.video_codec.as_ffmpeg().to_string());
    let a = acodec
        .map(str::to_string)
        .unwrap_or_else(|| plan.audio_codec.as_ffmpeg().to_string());
    Ok((v, a, plan.extra_args))
}

fn info(input: &str, json: bool) -> Result<()> {
    let mut cmd = Command::new("ffprobe");
    if json {
        cmd.args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            input,
        ]);
    } else {
        cmd.args(["-hide_banner", "-i", input]);
    }

    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| "failed to spawn ffprobe")?;

    if !out.status.success() {
        std::io::stderr().write_all(&out.stderr)?;
        bail!("ffprobe exited with status: {:?}", out.status.code());
    }

    // ffprobe writes the JSON report to stdout but the human-readable `-i`
    // report to stderr. Both are the answer the user asked for, so both go to
    // stdout -- otherwise `info` prints nothing pipeable and cannot be used in
    // a shell pipeline at all.
    let mut stdout = std::io::stdout();
    stdout.write_all(&out.stdout)?;
    if !json {
        stdout.write_all(&out.stderr)?;
    }
    stdout.flush()?;
    Ok(())
}

fn transcode_one(
    input: &str,
    output: Option<&str>,
    preset_name: Option<&str>,
    vcodec: Option<&str>,
    acodec: Option<&str>,
    extra: &[String],
    dry_run: bool,
) -> Result<()> {
    let base = cwd();
    let in_path = resolved(Path::new(input), &base);
    let out_opt = output.map(|o| resolved(Path::new(o), &base));

    let out_path = paths::resolve_output_path(&in_path, out_opt.as_deref(), "mkv", &base)?;

    let (v, a, args) = resolve_codecs(
        preset_name,
        vcodec,
        acodec,
        extra,
        EncoderId::Libx264,
        EncoderId::Aac,
    )?;

    let job = JobPaths {
        input: in_path,
        output: out_path,
    };
    let argv = build_ffmpeg_argv_raw(&v, &a, None, &args, &job);

    if dry_run {
        println!(
            "[DRY RUN] {} -> {}",
            job.input.display(),
            job.output.display()
        );
        println!("  {}", format_command("ffmpeg", &argv));
        return Ok(());
    }

    run_ffmpeg(&argv)
}

#[allow(clippy::too_many_arguments)]
fn batch(
    input_dir: &str,
    output_dir: &str,
    preset_name: Option<&str>,
    vcodec: Option<&str>,
    acodec: Option<&str>,
    ext: &str,
    input_exts: &str,
    extra: &[String],
    dry_run: bool,
) -> Result<()> {
    let base = cwd();
    let in_root = resolved(Path::new(input_dir), &base);
    let out_root = resolved(Path::new(output_dir), &base);

    if !in_root.exists() {
        bail!("Input directory does not exist: {}", input_dir);
    }

    let exts: Vec<&str> = input_exts.split(',').map(str::trim).collect();
    let files = collect_media_files(&in_root, &exts)?;
    if files.is_empty() {
        println!("No media files found matching extensions: {}", input_exts);
        return Ok(());
    }

    let (v, a, args) = resolve_codecs(
        preset_name,
        vcodec,
        acodec,
        extra,
        EncoderId::Libx265,
        EncoderId::Aac,
    )?;

    let same_dir = paths::paths_equivalent(&in_root, &out_root);
    println!(
        "Found {} files to transcode{} (vcodec={}, acodec={}, ext={})",
        files.len(),
        if same_dir {
            " IN-PLACE - output will use '_transcoded' suffix"
        } else {
            ""
        },
        v,
        a,
        ext
    );

    for (idx, input_file) in files.iter().enumerate() {
        let output_file = paths::plan_output_path(input_file, &in_root, &out_root, ext)?;

        println!(
            "\n[{}/{}] {} -> {}",
            idx + 1,
            files.len(),
            input_file.display(),
            output_file.display()
        );

        let job = JobPaths {
            input: input_file.clone(),
            output: output_file,
        };
        let argv = build_ffmpeg_argv_raw(&v, &a, None, &args, &job);

        // A dry run must not touch the filesystem, so the output directory is
        // created only on the real path -- below this guard, never above it.
        if dry_run {
            println!("  [DRY RUN] {}", format_command("ffmpeg", &argv));
            continue;
        }

        if let Some(parent) = job.output.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create output dir: {:?}", parent))?;
        }

        if let Err(e) = run_ffmpeg(&argv) {
            eprintln!("  ERROR: {}", e);
            eprintln!("  Skipping and continuing with next file...");
        }
    }

    println!("\nBatch transcode completed!");
    Ok(())
}

fn run_ffmpeg(argv: &[String]) -> Result<()> {
    let status = Command::new("ffmpeg")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to spawn ffmpeg; args: {:?}", argv))?;

    if !status.success() {
        bail!("ffmpeg exited with status: {:?}", status.code());
    }
    Ok(())
}

/// Walk `dir` recursively, collecting files whose extension matches.
///
/// Stays in the CLI rather than moving into core: it performs I/O. The server's
/// `Scanner` gets its own implementation with symlink, depth and exclude-glob
/// guards, which this deliberately does not need.
fn collect_media_files(dir: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(collect_media_files(&path, extensions)?);
        } else if path.is_file() {
            if let Some(file_ext) = path.extension() {
                let got = file_ext.to_string_lossy().to_lowercase();
                if extensions.iter().any(|e| e.to_lowercase() == got) {
                    files.push(path);
                }
            }
        }
    }
    Ok(files)
}
