// file: src/main.rs
// version: 0.8.0
// guid: 0f9e8d7c-6b5a-4c3d-2e1f-0a9b8c7d6e5f
// last-edited: 2026-07-31

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about = "Transcode media while preserving metadata (ffmpeg wrapper)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show media info via ffprobe (optionally as JSON)
    Info {
        /// Input media file
        input: String,
        /// Output as JSON (requires --features json)
        #[arg(long)]
        json: bool,
    },
    /// Transcode a file while preserving metadata
    Transcode {
        /// Input media file
        input: String,
        /// Optional output media file; if omitted, will write next to input as `<name>_transcoded.mkv`
        output: Option<String>,
        /// Preset name (e.g., original-h265)
        #[arg(long)]
        preset: Option<String>,
        /// Video codec (e.g., libx264, libx265, copy)
        #[arg(long, default_value = "libx264")]
        vcodec: String,
        /// Audio codec (e.g., aac, ac3, copy)
        #[arg(long, default_value = "aac")]
        acodec: String,
        /// Extra ffmpeg args (passed as-is after standard args)
        #[arg(long, num_args = 0.., value_delimiter = ' ')]
        extra: Vec<String>,
        /// Dry run: print command without executing
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
        /// Video codec (e.g., libx265)
        #[arg(long, default_value = "libx265")]
        vcodec: String,
        /// Audio codec (e.g., aac, ac3)
        #[arg(long, default_value = "aac")]
        acodec: String,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Info { input, json } => info(&input, json),
        Commands::Transcode {
            input,
            output,
            preset,
            vcodec,
            acodec,
            extra,
            dry_run,
        } => {
            // Determine safe output path
            let resolved_output = resolve_output_path(&input, output.as_deref(), Some("mkv"))?;
            let (vcodec2, acodec2, extra2) =
                apply_preset(preset.as_deref(), &vcodec, &acodec, &extra)?;
            if dry_run {
                let args = build_ffmpeg_args(
                    &input,
                    &resolved_output.to_string_lossy(),
                    &vcodec2,
                    &acodec2,
                    &extra2,
                );
                println!("[DRY RUN] {} -> {}", input, resolved_output.display());
                println!("  {}", format_ffmpeg_command(&args));
                Ok(())
            } else {
                transcode(
                    &input,
                    &resolved_output.to_string_lossy(),
                    &vcodec2,
                    &acodec2,
                    &extra2,
                )
            }
        }
        Commands::Batch {
            input_dir,
            output_dir,
            preset,
            vcodec,
            acodec,
            ext,
            input_exts,
            extra,
            dry_run,
        } => batch_transcode(
            &input_dir,
            &output_dir,
            preset.as_deref(),
            &vcodec,
            &acodec,
            &ext,
            &input_exts,
            &extra,
            dry_run,
        ),
    }
}

// Resolve a safe output path based on input and optional user-provided output.
// Rules:
// - If user output is provided and is not identical to input path, use it.
// - If user output is identical to input (same full path), or not provided,
//   create `<stem>_transcoded.<ext>` next to the input. Default ext is `mkv`.
fn resolve_output_path(
    input: &str,
    output_opt: Option<&str>,
    default_ext: Option<&str>,
) -> Result<PathBuf> {
    let in_path = Path::new(input)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(input));

    if let Some(out_str) = output_opt {
        let out_path_try = Path::new(out_str);
        let out_path_abs = if out_path_try.is_absolute() {
            out_path_try.to_path_buf()
        } else {
            // resolve relative to current dir
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(out_path_try)
        };

        // If identical to input, compute a safe sibling with suffix
        if paths_equivalent(&in_path, &out_path_abs) {
            return Ok(suffixed_output(&in_path, default_ext.unwrap_or("mkv")));
        }
        return Ok(out_path_abs);
    }

    // No output provided: compute default sibling with suffix and mkv
    Ok(suffixed_output(&in_path, default_ext.unwrap_or("mkv")))
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    // Try canonicalize to compare real paths, fall back to string comparison
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

fn suffixed_output(input_path: &Path, out_ext: &str) -> PathBuf {
    let parent = input_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = strict_stem(input_path);
    let mut final_name = String::with_capacity(stem.len() + 1 + 12 + out_ext.len());
    final_name.push_str(&stem);
    final_name.push_str("_transcoded.");
    final_name.push_str(out_ext);
    parent.join(final_name)
}

// Derive the filename stem using the LAST '.' before the extension.
// This avoids truncating names that legitimately contain dots (e.g., "Episode 1.11 ... .mkv").
// For dotfiles (e.g., ".bashrc"), or names without extension, returns the whole name.
fn strict_stem(path: &Path) -> String {
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
            // Fallback: no recognizable extension position; return full name
            return name.to_string();
        }
    }
    // Ultimate fallback
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output")
        .to_string()
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

// Build the exact ffmpeg argument list used for a transcode.
// Factored out of `transcode` so `--dry-run` can print the real command instead
// of a prose summary: a dry run you cannot copy-paste is not a dry run.
fn build_ffmpeg_args(
    input: &str,
    output: &str,
    vcodec: &str,
    acodec: &str,
    extra: &[String],
) -> Vec<String> {
    // Build a conservative default arg list that tries to preserve metadata
    // -map_metadata 0 copies global metadata
    // -movflags use_metadata_tags preserves tags in MP4 containers
    // -c:s copy keeps subtitle streams
    let mut args = vec![
        "-hide_banner".to_string(),
        "-y".to_string(), // overwrite
        "-i".to_string(),
        input.to_string(),
        "-map_metadata".to_string(),
        "0".to_string(),
        "-movflags".to_string(),
        "use_metadata_tags".to_string(),
        "-c:v".to_string(),
        vcodec.to_string(),
        "-c:a".to_string(),
        acodec.to_string(),
        "-c:s".to_string(),
        "copy".to_string(),
    ];

    // Append any extra args the user provided
    args.extend(extra.iter().cloned());

    // Output path last
    args.push(output.to_string());

    args
}

// Render an argv as a copy-pasteable shell command, quoting only what needs it.
fn format_ffmpeg_command(args: &[String]) -> String {
    let mut out = String::from("ffmpeg");
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

fn transcode(
    input: &str,
    output: &str,
    vcodec: &str,
    acodec: &str,
    extra: &[String],
) -> Result<()> {
    let args = build_ffmpeg_args(input, output, vcodec, acodec, extra);

    let status = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to spawn ffmpeg; args: {:?}", &args))?;

    if !status.success() {
        bail!("ffmpeg exited with status: {:?}", status.code());
    }
    Ok(())
}

fn batch_transcode(
    input_dir: &str,
    output_dir: &str,
    preset: Option<&str>,
    vcodec: &str,
    acodec: &str,
    ext: &str,
    input_exts: &str,
    extra: &[String],
    dry_run: bool,
) -> Result<()> {
    let input_path = Path::new(input_dir);
    let output_path = Path::new(output_dir);

    if !input_path.exists() {
        bail!("Input directory does not exist: {}", input_dir);
    }

    // Check if input and output directories are the same
    let same_dir = paths_equivalent(input_path, output_path);

    // Parse comma-separated extensions
    let exts: Vec<&str> = input_exts.split(',').map(|s| s.trim()).collect();

    // Collect all media files recursively
    let files = collect_media_files(input_path, &exts)?;

    if files.is_empty() {
        println!("No media files found matching extensions: {}", input_exts);
        return Ok(());
    }

    // Apply preset once to get effective settings
    let (eff_vcodec, eff_acodec, eff_extra) = apply_preset(preset, vcodec, acodec, extra)?;

    if same_dir {
        println!(
            "Found {} files to transcode IN-PLACE (vcodec={}, acodec={}, ext={}) - output will use '_transcoded' suffix",
            files.len(),
            eff_vcodec,
            eff_acodec,
            ext
        );
    } else {
        println!(
            "Found {} files to transcode (vcodec={}, acodec={}, ext={})",
            files.len(),
            eff_vcodec,
            eff_acodec,
            ext
        );
    }

    for (idx, input_file) in files.iter().enumerate() {
        let output_file = if same_dir {
            // When writing to same directory, use safe suffix
            suffixed_output(input_file, ext)
        } else {
            // Calculate relative path and mirror structure in different output dir
            let rel_path = input_file
                .strip_prefix(input_path)
                .context("failed to strip prefix")?;

            let mut out = output_path.join(rel_path);
            out.set_extension(ext);
            out
        };

        println!(
            "\n[{}/{}] {} -> {}",
            idx + 1,
            files.len(),
            input_file.display(),
            output_file.display()
        );

        // A dry run must not touch the filesystem, so the output directory is
        // created only on the real path -- below this guard, never above it.
        if dry_run {
            let args = build_ffmpeg_args(
                &input_file.to_string_lossy(),
                &output_file.to_string_lossy(),
                &eff_vcodec,
                &eff_acodec,
                &eff_extra,
            );
            println!("  [DRY RUN] {}", format_ffmpeg_command(&args));
            continue;
        }

        // Ensure output directory exists
        if let Some(parent) = output_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create output dir: {:?}", parent))?;
        }

        // Perform the transcode
        if let Err(e) = transcode(
            &input_file.to_string_lossy(),
            &output_file.to_string_lossy(),
            &eff_vcodec,
            &eff_acodec,
            &eff_extra,
        ) {
            eprintln!("  ERROR: {}", e);
            eprintln!("  Skipping and continuing with next file...");
        }
    }

    println!("\nBatch transcode completed!");
    Ok(())
}

fn collect_media_files(dir: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            // Recurse into subdirectories
            files.extend(collect_media_files(&path, extensions)?);
        } else if path.is_file() {
            if let Some(file_ext) = path.extension() {
                let file_ext_str = file_ext.to_string_lossy().to_lowercase();
                if extensions.iter().any(|e| e.to_lowercase() == file_ext_str) {
                    files.push(path);
                }
            }
        }
    }

    Ok(files)
}

// Compute effective codecs and args based on an optional preset.
// Precedence rules:
// - If preset is provided, it supplies default vcodec/acodec and extra args
// - Explicit --vcodec/--acodec override preset's codecs
// - User --extra are appended after preset extras so they override
fn apply_preset(
    preset: Option<&str>,
    vcodec: &str,
    acodec: &str,
    extra: &[String],
) -> Result<(String, String, Vec<String>)> {
    let mut out_v = vcodec.to_string();
    let mut out_a = acodec.to_string();
    let mut out_extra: Vec<String> = Vec::new();

    if let Some(name) = preset {
        match name {
            // "Original quality" intent: visually lossless-ish h265 and high-quality audio
            // x265 CRF 18 is commonly considered visually lossless; preset slow for quality
            // Use AAC at 256k for high-quality, universally compatible audio
            "original-h265" | "original" => {
                if vcodec == "libx264" {
                    // unchanged from default implies not specified
                    out_v = "libx265".to_string();
                }
                if acodec == "aac" {
                    // unchanged from default implies not specified
                    out_a = "aac".to_string();
                }
                out_extra.extend([
                    "-crf".to_string(),
                    "18".to_string(),
                    "-preset".to_string(),
                    "slow".to_string(),
                    // audio bitrate target (can be overridden by user extra)
                    "-b:a".to_string(),
                    "256k".to_string(),
                ]);
            }
            "tv-h265-fast" | "tv-fast" => {
                if vcodec == "libx264" {
                    out_v = "libx265".to_string();
                }
                if acodec == "aac" {
                    out_a = "aac".to_string();
                }
                out_extra.extend([
                    "-crf".to_string(),
                    "22".to_string(),
                    "-preset".to_string(),
                    "medium".to_string(),
                    "-b:a".to_string(),
                    "160k".to_string(),
                ]);
            }
            "movie-quality" | "movie" => {
                if vcodec == "libx264" {
                    out_v = "libx265".to_string();
                }
                if acodec == "aac" {
                    out_a = "aac".to_string();
                }
                out_extra.extend([
                    "-crf".to_string(),
                    "16".to_string(),
                    "-preset".to_string(),
                    "slow".to_string(),
                    "-b:a".to_string(),
                    "320k".to_string(),
                ]);
            }
            other => {
                // Silently ignoring an unknown preset is how you discover, an hour
                // and 200 files later, that nothing you asked for was applied.
                bail!(
                    "unknown preset '{}'; valid presets are: original-h265 (aliases: original), \
                     tv-h265-fast (aliases: tv-fast), movie-quality (aliases: movie)",
                    other
                );
            }
        }
    }

    // Append user extras last to allow override
    out_extra.extend(extra.iter().cloned());

    Ok((out_v, out_a, out_extra))
}
