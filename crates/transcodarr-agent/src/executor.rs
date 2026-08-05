// file: crates/transcodarr-agent/src/executor.rs
// version: 1.2.0
// guid: 8e5a04cb-71f2-4d63-9a80-3c6b1e97fa25
// last-edited: 2026-08-05
//! Running ffmpeg, watching it, and judging what it produced.
//!
//! Three decisions here are load-bearing, and each came from something that
//! went wrong in production:
//!
//! - **argv, never a shell.** Media filenames routinely contain quotes,
//!   semicolons, dollar signs and newlines. Passed as arguments they are inert;
//!   interpolated into a shell they are commands.
//! - **Progress via a file, not a pipe.** ffmpeg's `-progress` writes key=value
//!   lines. Read from a pipe, a reader that falls behind fills the pipe buffer
//!   and *blocks ffmpeg itself* — the encode stalls because nobody drained a
//!   progress report. Written to a file, a slow reader costs nothing.
//! - **Duration from the last packet PTS, never the container header.** This is
//!   the measured failure that motivates the whole validation design: the
//!   Turing NVDEC AV1 path exits 69 having written roughly a kilobyte, and a
//!   truncated MKV frequently *retains the source duration in its header*. A
//!   header-derived duration sails straight through the gate that exists to
//!   catch exactly this.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use transcodarr_core::plan::{EncodePlan, JobPaths, build_ffmpeg_argv};
use transcodarr_core::probe::{self, MediaProbe};
use transcodarr_core::validate::{ValidationReport, ValidationSpec, validate_output};

use crate::AgentError;

/// How often the progress file is re-read.
const PROGRESS_POLL: Duration = Duration::from_millis(500);

/// Which binaries to run and how patient to be.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// The ffmpeg binary.
    pub ffmpeg: String,
    /// The ffprobe binary.
    pub ffprobe: String,
    /// Abandon an encode that exceeds this. `None` for no limit.
    pub timeout: Option<Duration>,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            // No default wall-clock limit: a 60 GB remux legitimately runs for
            // hours, and a limit low enough to catch a hang would kill real
            // work. Progress stalling is the signal that matters, and the
            // dispatcher's lease covers the rest.
            timeout: None,
        }
    }
}

/// What ffmpeg last reported about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Output time in microseconds.
    pub out_time_us: u64,
    /// Frames written so far.
    pub frames: u64,
    /// Encoding speed, as ffmpeg reports it (e.g. `2.5x`).
    pub speed: Option<String>,
    /// Total bytes written so far.
    pub total_size: u64,
    /// Whether ffmpeg has reported itself finished.
    pub done: bool,
}

/// Reads ffmpeg's `-progress` output file.
///
/// A file rather than a pipe: a reader that falls behind on a pipe fills the
/// buffer and blocks ffmpeg, so the encode stalls waiting for someone to read a
/// status line. With a file, being slow costs nothing.
#[derive(Debug, Clone)]
pub struct ProgressTailer {
    path: PathBuf,
}

impl ProgressTailer {
    /// Watch the given progress file.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Where ffmpeg writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the most recent report.
    ///
    /// ffmpeg appends blocks of `key=value` lines terminated by `progress=`, so
    /// the newest value for each key is the last one in the file. Parsing the
    /// whole file and keeping the last of each key is both simplest and correct
    /// while ffmpeg is still appending to it.
    pub fn read(&self) -> Progress {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Progress::default();
        };
        Self::parse(&text)
    }

    /// Parse a progress file's contents.
    pub fn parse(text: &str) -> Progress {
        let mut p = Progress::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "out_time_us" | "out_time_ms" => {
                    // ffmpeg's `out_time_ms` is a misnomer -- it has always
                    // carried microseconds. Treating it as milliseconds
                    // understates progress by a thousand and makes a healthy
                    // encode look stalled.
                    if let Ok(v) = value.parse::<u64>() {
                        p.out_time_us = v;
                    }
                }
                "frame" => {
                    if let Ok(v) = value.parse::<u64>() {
                        p.frames = v;
                    }
                }
                "total_size" => {
                    if let Ok(v) = value.parse::<u64>() {
                        p.total_size = v;
                    }
                }
                "speed" => p.speed = Some(value.to_string()),
                "progress" => p.done = value == "end",
                _ => {}
            }
        }
        p
    }
}

/// What one ffmpeg run produced.
#[derive(Debug, Clone)]
pub struct Execution {
    /// The exact argv, for reproducing the run by hand.
    ///
    /// Persisted before exec by the caller, so a failure can always be
    /// reproduced by pasting this into a shell on the agent.
    pub argv: Vec<String>,
    /// Process exit code, or -1 if it was signalled.
    pub exit_code: i32,
    /// Signal that killed it, if any.
    pub signal: Option<i32>,
    /// The tail of stderr, for the operator.
    pub stderr_tail: String,
    /// Bytes written.
    pub output_bytes: u64,
    /// How long it ran.
    pub elapsed: Duration,
    /// The final progress report.
    pub progress: Progress,
}

/// Runs encodes and validates their output.
///
/// `Clone` because an encode runs on a blocking thread: the config is two
/// binary paths and a timeout, so a clone is cheaper than the synchronisation
/// sharing it would need.
#[derive(Debug, Clone)]
pub struct Executor {
    config: ExecutorConfig,
}

impl Executor {
    /// Build an executor.
    pub fn new(config: ExecutorConfig) -> Self {
        Self { config }
    }

    /// The argv that would be run, without running it.
    ///
    /// Persisted to `job_attempt.argv_json` *before* exec, so a failure is
    /// always reproducible even if the process dies before reporting anything.
    pub fn argv_for(&self, plan: &EncodePlan, paths: &JobPaths) -> Vec<String> {
        build_ffmpeg_argv(plan, paths)
    }

    /// Run one encode.
    ///
    /// `on_progress` is called every [`PROGRESS_POLL`] with the latest report,
    /// so a caller can surface progress or notice a stall without polling the
    /// process itself.
    pub fn run(
        &self,
        plan: &EncodePlan,
        paths: &JobPaths,
        progress_path: &Path,
        on_progress: impl FnMut(&Progress),
    ) -> Result<Execution, AgentError> {
        let argv = self.argv_for(plan, paths);
        self.run_argv(&argv, &paths.output, progress_path, on_progress)
    }

    /// Run an argv composed elsewhere.
    ///
    /// This is what a dispatched job uses: `JobAssignment.argv` is built
    /// server-side and the agent does not compose it. Keeping one
    /// implementation under both entry points is deliberate — an agent that
    /// rebuilt the command locally could encode to a plan the server never
    /// authorised, and the difference would not show up until the output was
    /// already installed.
    pub fn run_argv(
        &self,
        argv: &[String],
        output: &Path,
        progress_path: &Path,
        mut on_progress: impl FnMut(&Progress),
    ) -> Result<Execution, AgentError> {
        let argv = argv.to_vec();
        let tailer = ProgressTailer::new(progress_path.to_path_buf());
        let _ = std::fs::remove_file(progress_path);

        let started = Instant::now();
        let mut child = Command::new(&self.config.ffmpeg)
            // `-nostdin` because ffmpeg reads stdin for interactive keys and
            // will otherwise consume the parent's, stealing input from whatever
            // launched the agent.
            .arg("-nostdin")
            .arg("-progress")
            .arg(progress_path)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AgentError::Execute {
                program: self.config.ffmpeg.clone(),
                source: e,
            })?;

        // stderr is drained on a thread. ffmpeg is chatty, and a full pipe
        // buffer blocks the encoder just as surely as an undrained progress
        // pipe would.
        let stderr = child.stderr.take();
        let drain = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            if let Some(mut s) = stderr {
                let _ = s.read_to_string(&mut buf);
            }
            buf
        });

        let mut last = Progress::default();
        let deadline = self.config.timeout.map(|t| started + t);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {}
                Err(e) => {
                    return Err(AgentError::Execute {
                        program: self.config.ffmpeg.clone(),
                        source: e,
                    });
                }
            }
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    let _ = child.kill();
                    let _ = child.wait();
                    break std::process::ExitStatus::default();
                }
            }
            let p = tailer.read();
            if p != last {
                on_progress(&p);
                last = p;
            }
            std::thread::sleep(PROGRESS_POLL);
        };

        let stderr_tail = drain.join().unwrap_or_default();
        let progress = tailer.read();
        let output_bytes = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);

        Ok(Execution {
            argv,
            exit_code: status.code().unwrap_or(-1),
            signal: signal_of(&status),
            stderr_tail: tail(&stderr_tail, 4000),
            output_bytes,
            elapsed: started.elapsed(),
            progress,
        })
    }

    /// Probe an output for validation.
    ///
    /// **Duration comes from the last packet's PTS, not the container header.**
    /// This is not a refinement: the measured AV1/NVDEC failure writes a
    /// truncated MKV that keeps the *source* duration in its header, so a
    /// header-derived duration passes the very gate meant to catch it. Reading
    /// the last packet costs one extra ffprobe over the tail of the file and is
    /// the difference between rejecting a destroyed file and installing it.
    pub fn probe_output(&self, path: &Path) -> Result<MediaProbe, AgentError> {
        let raw = self.run_ffprobe(
            &[
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ],
            path,
            None,
        )?;
        let mut parsed = probe::parse_ffprobe_json(&raw).map_err(|e| AgentError::Probe {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        parsed.duration_us = self.last_packet_pts_us(path)?.or(parsed.duration_us);
        Ok(parsed)
    }

    /// The presentation timestamp of the last packet, in microseconds.
    ///
    /// `None` when no packet could be read, which the validator treats as
    /// unprobeable — correctly, since a file with no readable packets is not
    /// media whatever its header claims.
    ///
    /// The interval is an **absolute** seek point, computed from the header
    /// duration. `-read_intervals -60` looks like "the last sixty seconds" and
    /// is not: on real media it silently returns nothing, so this function
    /// returned `None`, the caller fell back to the header duration, and the
    /// entire last-packet-PTS guarantee quietly stopped applying. It only
    /// appeared to work on short test fixtures. Measured on a 23-minute Blu-ray
    /// remux: the absolute form returns 1421.962s against a header duration of
    /// 1422.016s — the one-frame difference the guard exists to reason about —
    /// and costs 0.3 seconds on a 7 GB file.
    pub fn last_packet_pts_us(&self, path: &Path) -> Result<Option<u64>, AgentError> {
        let header_s = self.header_duration_s(path).unwrap_or(0.0);
        // Sixty seconds of tail is plenty to find the final packet, and seeking
        // rather than scanning is what keeps this cheap on a 60 GB file.
        let from = (header_s - 60.0).max(0.0);
        let raw = self.run_ffprobe(
            &[
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time",
                "-of",
                "csv=p=0",
                "-read_intervals",
            ],
            path,
            Some(&format!("{from:.3}%+#100000")),
        )?;
        let last = raw
            .lines()
            .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
            .fold(None::<f64>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
        Ok(last.map(|s| (s * 1_000_000.0) as u64))
    }

    /// The container header's duration, in seconds.
    ///
    /// Used only to decide where to seek. It is never the answer: a truncated
    /// MKV frequently keeps the source duration here, which is the whole reason
    /// the last packet is consulted at all.
    fn header_duration_s(&self, path: &Path) -> Option<f64> {
        let raw = self
            .run_ffprobe(
                &[
                    "-v",
                    "error",
                    "-show_entries",
                    "format=duration",
                    "-of",
                    "csv=p=0",
                ],
                path,
                None,
            )
            .ok()?;
        raw.trim().parse::<f64>().ok()
    }

    /// Judge an output against the ordered gates.
    ///
    /// A thin wrapper over `core::validate::validate_output` on purpose: the
    /// server and the agent link the *same* validation code, so agent-side
    /// re-validation genuinely detects a stale model rather than being a second
    /// implementation that can drift.
    pub fn validate(
        &self,
        spec: &ValidationSpec,
        output: &Path,
        exit_code: i32,
    ) -> Result<ValidationReport, AgentError> {
        let out_bytes = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
        // An unprobeable output is a validation failure, not an error. The 1 KB
        // AV1/NVDEC artefact lands here, and it must be *rejected*, not reported
        // as a tooling problem the caller might retry past. A default probe has
        // no duration, so the Duration gate fails it before Size is consulted.
        let probe = self.probe_output(output).unwrap_or_default();
        Ok(validate_output(spec, &probe, exit_code, out_bytes))
    }

    /// `extra` is a single positional value appended after `args` and before
    /// the path, for options like `-read_intervals` whose value is computed.
    fn run_ffprobe(
        &self,
        args: &[&str],
        path: &Path,
        extra: Option<&str>,
    ) -> Result<String, AgentError> {
        let mut cmd = Command::new(&self.config.ffprobe);
        cmd.args(args);
        if let Some(e) = extra {
            cmd.arg(e);
        }
        let out = cmd
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| AgentError::Execute {
                program: self.config.ffprobe.clone(),
                source: e,
            })?;
        if !out.status.success() {
            return Err(AgentError::Probe {
                path: path.display().to_string(),
                reason: format!(
                    "ffprobe exited {}: {}",
                    out.status.code().unwrap_or(-1),
                    tail(&String::from_utf8_lossy(&out.stderr), 300)
                ),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Write an argv to a file as JSON, before exec.
///
/// Persisted first so a failure is reproducible even when the process dies
/// before reporting anything at all.
pub fn persist_argv(path: &Path, argv: &[String]) -> Result<(), AgentError> {
    let body = serde_json::to_vec_pretty(argv).map_err(|e| AgentError::Execute {
        program: "argv".into(),
        source: std::io::Error::other(e),
    })?;
    let mut f = std::fs::File::create(path).map_err(|e| AgentError::Execute {
        program: path.display().to_string(),
        source: e,
    })?;
    f.write_all(&body).map_err(|e| AgentError::Execute {
        program: path.display().to_string(),
        source: e,
    })
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Slice on a character boundary: ffmpeg's stderr carries the filename, and
    // media paths are frequently non-ASCII.
    let start = s.len() - max;
    let idx = s
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| *i >= start)
        .unwrap_or(0);
    s[idx..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffmpeg's `out_time_ms` has always carried microseconds despite its name.
    /// Reading it as milliseconds understates progress a thousandfold and makes
    /// a healthy encode look stalled.
    #[test]
    fn out_time_ms_is_parsed_as_microseconds() {
        let p = ProgressTailer::parse("out_time_ms=1500000\nprogress=continue\n");
        assert_eq!(p.out_time_us, 1_500_000);
    }

    /// ffmpeg appends blocks; the newest value for a key is the last one.
    #[test]
    fn the_latest_block_wins() {
        let text = "frame=10\nout_time_us=1000\nprogress=continue\n\
                    frame=250\nout_time_us=9000\ntotal_size=4096\nspeed=2.5x\nprogress=end\n";
        let p = ProgressTailer::parse(text);
        assert_eq!(p.frames, 250);
        assert_eq!(p.out_time_us, 9000);
        assert_eq!(p.total_size, 4096);
        assert_eq!(p.speed.as_deref(), Some("2.5x"));
        assert!(p.done, "progress=end must mark it finished");
    }

    #[test]
    fn an_unfinished_encode_is_not_marked_done() {
        let p = ProgressTailer::parse("frame=5\nprogress=continue\n");
        assert!(!p.done);
    }

    #[test]
    fn garbage_lines_are_ignored_rather_than_panicking() {
        let p = ProgressTailer::parse("nonsense\nframe=notanumber\n=\nframe=7\n");
        assert_eq!(p.frames, 7);
    }

    #[test]
    fn a_missing_progress_file_reads_as_empty() {
        let t = ProgressTailer::new(PathBuf::from("/nonexistent/progress"));
        assert_eq!(t.read(), Progress::default());
    }

    /// The argv is built by core, so the agent and the CLI cannot drift on
    /// argument order or metadata flags. Checked here because the ritual
    /// persists it before exec as the reproduction recipe.
    #[test]
    fn the_argv_maps_all_streams_and_ends_with_the_output() {
        use transcodarr_core::plan::EncoderId;
        let e = Executor::new(ExecutorConfig::default());
        let plan = EncodePlan {
            video_codec: EncoderId::Copy,
            audio_codec: EncoderId::Eac3,
            pix_fmt: None,
            extra_args: vec!["-b:a".into(), "640k".into()],
        };
        let paths = JobPaths {
            input: "/mnt/tv/in.mkv".into(),
            output: "/w/out.mkv".into(),
        };
        let argv = e.argv_for(&plan, &paths);

        assert_eq!(argv.last().unwrap(), "/w/out.mkv", "output comes last");
        assert!(argv.windows(2).any(|w| w[0] == "-c:s" && w[1] == "copy"));
        assert!(
            argv.windows(2).any(|w| w[0] == "-b:a" && w[1] == "640k"),
            "extra args must survive"
        );
    }

    #[test]
    fn the_argv_is_persisted_as_reproducible_json() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("argv.json");
        persist_argv(&p, &["-i".to_string(), "/mnt/a b.mkv".to_string()]).unwrap();
        let back: Vec<String> = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(back, vec!["-i", "/mnt/a b.mkv"]);
    }

    #[test]
    fn a_long_stderr_is_tailed_on_a_character_boundary() {
        let s = "é".repeat(5000);
        let t = tail(&s, 100);
        assert!(t.len() <= 101, "len {}", t.len());
        assert!(t.chars().all(|c| c == 'é'));
    }

    /// A missing ffmpeg is reported, not panicked on.
    #[test]
    fn a_missing_ffmpeg_binary_is_an_error() {
        let e = Executor::new(ExecutorConfig {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ..ExecutorConfig::default()
        });
        let d = tempfile::TempDir::new().unwrap();
        let plan = EncodePlan {
            video_codec: transcodarr_core::plan::EncoderId::Copy,
            audio_codec: transcodarr_core::plan::EncoderId::Copy,
            pix_fmt: None,
            extra_args: vec![],
        };
        let paths = JobPaths {
            input: d.path().join("in.mkv"),
            output: d.path().join("out.mkv"),
        };
        let r = e.run(&plan, &paths, &d.path().join("prog"), |_| {});
        assert!(matches!(r, Err(AgentError::Execute { .. })), "{r:?}");
    }
}
