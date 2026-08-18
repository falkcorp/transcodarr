// file: crates/transcodarr-cli/src/admin.rs
// version: 1.2.0
// guid: 5b8e1a37-c204-4d69-9f52-a03e7b64c81d
// last-edited: 2026-08-18
//! The `admin` subcommand: operator diagnostics.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use transcodarr_agent::preflight;
use transcodarr_core::facts::SizeThresholds;
use transcodarr_core::policy;
use transcodarr_server::{
    Evaluator, Explainer, LibraryRecord, ProbeOptions, Prober, Runtime, ScanOptions, Scanner,
};

/// Where the database lives unless told otherwise.
const DEFAULT_DB: &str = "transcodarr.db";

/// Open the store through the server.
///
/// The CLI never links `transcodarr-store`. Two crates agreeing about
/// connection lifetimes, pragmas and the single-writer rule is one more thing
/// that can drift, and there is no benefit to it here.
fn open_store(path: &std::path::Path) -> Result<Runtime> {
    Runtime::open(path).with_context(|| format!("opening the database at {}", path.display()))
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Diagnose this machine's fitness to run transcodarr.
    Diagnose {
        /// Run the environment preflight probes.
        #[arg(long)]
        preflight: bool,
        /// Directory to exercise rename and ZFS probes against.
        #[arg(long, default_value = ".")]
        work_dir: PathBuf,
        /// Directory the database would live in, for the fsync probe.
        #[arg(long)]
        db_dir: Option<PathBuf>,
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Register or update a library.
    AddLibrary {
        /// Database file.
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// Stable identifier, e.g. `tv`.
        #[arg(long)]
        id: String,
        /// Operator-facing name.
        #[arg(long)]
        name: Option<String>,
        /// Root directory to scan.
        #[arg(long)]
        root: PathBuf,
        /// Where agents stage output.
        #[arg(long)]
        work_dir: PathBuf,
        /// Where replaced originals are retained.
        #[arg(long)]
        trash_dir: PathBuf,
        /// Skip files modified more recently than this many seconds ago.
        #[arg(long, default_value_t = 300)]
        min_mtime_age_s: i64,
    },

    /// List registered libraries.
    Libraries {
        /// Database file.
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
    },

    /// Discover, probe and evaluate a library.
    Scan {
        /// Database file.
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// Which library. Omit to run every enabled one.
        #[arg(long)]
        library: Option<String>,
        /// Discover only; do not probe or evaluate.
        #[arg(long)]
        discover_only: bool,
        /// Skip discovery and only probe and evaluate what is already known.
        #[arg(long)]
        no_discover: bool,
        /// Concurrent ffprobe invocations.
        #[arg(long, default_value_t = transcodarr_server::prober::DEFAULT_PROBE_CONCURRENCY)]
        probe_concurrency: usize,
    },

    /// Re-evaluate stored facts against the current policy.
    ///
    /// No filesystem access at all: this is the operation that re-derives every
    /// decision after a policy edit.
    Evaluate {
        /// Database file.
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// Which library. Omit to run every enabled one.
        #[arg(long)]
        library: Option<String>,
        /// Re-decide every file, not only those predating the current policy.
        #[arg(long)]
        force: bool,
    },

    /// Explain why one file is, or is not, being transcoded.
    Explain {
        /// Database file.
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// Absolute path to the file.
        path: String,
    },

    /// Run pending jobs on this machine: encode, validate, install.
    Run {
        /// Database file.
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// Which library. Omit to run every enabled one.
        #[arg(long)]
        library: Option<String>,
        /// How many jobs to attempt.
        #[arg(long, default_value_t = 1)]
        limit: u32,
        /// Print the ffmpeg command for each job without running anything.
        #[arg(long)]
        dry_run: bool,
        /// Only run jobs of this class: audio, videocpu, videogpu.
        #[arg(long)]
        class: Option<String>,
    },

    /// What needs transcoding, by decision.
    Summary {
        /// Database file.
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// Which library. Omit to summarise every enabled one.
        #[arg(long)]
        library: Option<String>,
    },

    /// Operate on individual jobs.
    Jobs {
        /// What to do.
        #[command(subcommand)]
        cmd: JobsCommand,
    },
}

/// The `admin jobs` group.
///
/// A group rather than a flat `admin cancel` so that `jobs list` and
/// `jobs show` can be added later without renaming this one. A job id is
/// already discoverable: `admin explain <path>` prints it.
#[derive(Subcommand, Debug)]
pub enum JobsCommand {
    /// Cancel a job.
    ///
    /// The escape hatch for a job that can never be satisfied — most often one
    /// whose stored requirements no longer match anything the installed code
    /// emits, which nothing else can clear.
    Cancel {
        /// Database file.
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// The job to cancel, as printed by `admin explain`.
        id: String,
        /// An operator note, recorded against the job's final event.
        #[arg(long)]
        reason: Option<String>,
        /// Cancel even though an agent is holding it.
        ///
        /// The server revokes the assignment on that agent's next heartbeat.
        /// A job mid-commit is refused regardless.
        #[arg(long)]
        force: bool,
    },
}

/// Dispatch an `admin` subcommand.
pub fn run(cmd: AdminCommand) -> Result<()> {
    match cmd {
        AdminCommand::Diagnose {
            preflight: run_preflight,
            work_dir,
            db_dir,
            json,
        } => {
            if !run_preflight {
                bail!("nothing to do; pass --preflight");
            }
            let db_dir = db_dir.unwrap_or_else(|| work_dir.clone());
            let report = preflight::run_all(&work_dir, &db_dir);

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render());
                println!();
                println!(
                    "commit eligible: {}",
                    if report.commit_eligible() {
                        "yes"
                    } else {
                        "NO - this machine may produce output but must not install it"
                    }
                );
            }

            // A failed probe is a failed command. Preflight exists to gate
            // deployment, and a gate that always exits 0 gates nothing.
            if report.any_failed() {
                bail!("preflight failed; see the table above");
            }
            Ok(())
        }

        AdminCommand::AddLibrary {
            db,
            id,
            name,
            root,
            work_dir,
            trash_dir,
            min_mtime_age_s,
        } => add_library(db, id, name, root, work_dir, trash_dir, min_mtime_age_s),

        AdminCommand::Libraries { db } => list_libraries(db),

        AdminCommand::Scan {
            db,
            library,
            discover_only,
            no_discover,
            probe_concurrency,
        } => scan(db, library, discover_only, no_discover, probe_concurrency),

        AdminCommand::Evaluate { db, library, force } => evaluate(db, library, force),

        AdminCommand::Explain { db, path } => explain(db, path),

        AdminCommand::Run {
            db,
            library,
            limit,
            dry_run,
            class,
        } => run_jobs(db, library, limit, dry_run, class),

        AdminCommand::Summary { db, library } => summary(db, library),
        AdminCommand::Jobs { cmd } => match cmd {
            JobsCommand::Cancel {
                db,
                id,
                reason,
                force,
            } => cancel_job(db, id, reason, force),
        },
    }
}

/// Every enabled library, or just the one named.
fn selected(store: &Runtime, library: Option<String>) -> Result<Vec<LibraryRecord>> {
    let libs = store.libraries(library.as_deref())?;
    if libs.is_empty() {
        bail!("no enabled libraries; add one with `admin add-library`");
    }
    Ok(libs)
}

fn add_library(
    db: PathBuf,
    id: String,
    name: Option<String>,
    root: PathBuf,
    work_dir: PathBuf,
    trash_dir: PathBuf,
    min_mtime_age_s: i64,
) -> Result<()> {
    // Checked here rather than at the first scan: a typo in a root path should
    // fail while the operator is still looking at the command they typed.
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let store = open_store(&db)?;
    let name = name.unwrap_or_else(|| id.clone());
    store.add_library(
        &id,
        &name,
        &root.to_string_lossy(),
        &work_dir.to_string_lossy(),
        &trash_dir.to_string_lossy(),
        min_mtime_age_s,
    )?;
    println!("library {id} registered at {}", root.display());
    Ok(())
}

fn list_libraries(db: PathBuf) -> Result<()> {
    let store = open_store(&db)?;
    let libs = store.libraries(None)?;
    if libs.is_empty() {
        println!("no libraries registered");
        return Ok(());
    }
    for l in libs {
        println!(
            "{:<12} {:<40} min_mtime_age={}s",
            l.id, l.root_path, l.min_mtime_age_s
        );
    }
    Ok(())
}

fn scan(
    db: PathBuf,
    library: Option<String>,
    discover_only: bool,
    no_discover: bool,
    probe_concurrency: usize,
) -> Result<()> {
    let store = open_store(&db)?;
    let libraries = selected(&store, library)?;

    let scanner = Scanner::new(
        store.pool().clone(),
        std::sync::Arc::clone(store.writer()),
        ScanOptions::default(),
    );
    let prober = Prober::new(
        store.pool().clone(),
        std::sync::Arc::clone(store.writer()),
        ProbeOptions {
            concurrency: probe_concurrency,
            ..ProbeOptions::default()
        },
    );
    let evaluator = Evaluator::new(
        store.pool().clone(),
        std::sync::Arc::clone(store.writer()),
        SizeThresholds::default(),
    );
    let policy = policy::default_space_saver();

    for lib in libraries {
        if !no_discover {
            let out = scanner.scan_library(&lib, "full")?;
            println!(
                "{}: {} seen, {} new, {} changed, {} missing, {} too recent ({} ms)",
                lib.id,
                out.files_seen,
                out.files_new,
                out.files_changed,
                out.files_missing,
                out.files_too_recent,
                out.duration_ms
            );
        }
        if discover_only {
            continue;
        }

        let probed = prober.probe_library(&lib.id)?;
        println!(
            "{}: probed {}, failed {}",
            lib.id, probed.probed, probed.failed
        );

        let evaluated = evaluator.evaluate_library(&lib.id, &policy)?;
        println!(
            "{}: evaluated {}, {} jobs created, {} nothing owed, {} quarantined",
            lib.id,
            evaluated.evaluated,
            evaluated.jobs_created,
            evaluated.no_work,
            evaluated.quarantined
        );
    }
    Ok(())
}

fn evaluate(db: PathBuf, library: Option<String>, force: bool) -> Result<()> {
    let store = open_store(&db)?;
    let libraries = selected(&store, library)?;
    let policy = policy::default_space_saver();
    let evaluator = Evaluator::new(
        store.pool().clone(),
        std::sync::Arc::clone(store.writer()),
        SizeThresholds::default(),
    );

    for lib in libraries {
        if force {
            store.reset_evaluations(&lib.id)?;
        }
        let started = std::time::Instant::now();
        let out = evaluator.evaluate_library(&lib.id, &policy)?;
        println!(
            "{}: evaluated {} in {} ms, {} jobs created, {} nothing owed, {} quarantined, {} already busy",
            lib.id,
            out.evaluated,
            started.elapsed().as_millis(),
            out.jobs_created,
            out.no_work,
            out.quarantined,
            out.already_busy
        );
    }
    Ok(())
}

fn explain(db: PathBuf, path: String) -> Result<()> {
    let store = open_store(&db)?;
    let policy = policy::default_space_saver();
    let explanation = Explainer::new(store.pool().clone()).explain(&path, &policy)?;
    print!("{}", explanation.render());
    Ok(())
}

fn run_jobs(
    db: PathBuf,
    library: Option<String>,
    limit: u32,
    dry_run: bool,
    class: Option<String>,
) -> Result<()> {
    let only_class = match class.as_deref() {
        None => None,
        Some(raw) => Some(
            transcodarr_core::job::JobClass::parse(&normalise_class(raw))
                .ok_or_else(|| anyhow::anyhow!("unknown job class '{raw}'"))?,
        ),
    };
    let store = open_store(&db)?;
    let policy = policy::default_space_saver();
    let runner = transcodarr_server::LocalRunner::new(
        store.pool().clone(),
        std::sync::Arc::clone(store.writer()),
        transcodarr_server::ExecutorConfig::default(),
    );

    for lib in selected(&store, library)? {
        let out = runner.run_library(&lib, &policy, limit, dry_run, only_class)?;
        println!(
            "{}: {} attempted, {} installed, {} rejected, {} failed",
            lib.id, out.attempted, out.installed, out.rejected, out.failed
        );
        for j in &out.jobs {
            match (&j.resolution, &j.rejected) {
                // Stated as "saved"/"grew" rather than a signed number: an
                // audio pass to EAC3 640k legitimately grows the file, and a
                // bare `+0.33 MiB` reads as a saving to anyone skimming.
                (Some(r), _) => {
                    let mib = j.bytes_delta as f64 / (1024.0 * 1024.0);
                    let size = if j.bytes_delta >= 0 {
                        format!("saved {mib:.2} MiB")
                    } else {
                        format!("grew {:.2} MiB", -mib)
                    };
                    println!("  {:<12} {}  ({size})", r.label(), j.path);
                }
                (None, Some(why)) => {
                    println!("  {:<12} {}\n               {why}", "skipped", j.path)
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Accept `audio`, `videocpu`, `VideoGpu` and so on. The canonical spellings
/// are `CamelCase`, which is not what anyone types at a shell.
fn normalise_class(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "audio" => "Audio".into(),
        "videocpu" | "video-cpu" | "cpu" => "VideoCpu".into(),
        "videogpu" | "video-gpu" | "gpu" => "VideoGpu".into(),
        "probe" => "Probe".into(),
        "verify" => "Verify".into(),
        other => other.to_string(),
    }
}

fn summary(db: PathBuf, library: Option<String>) -> Result<()> {
    let store = open_store(&db)?;
    for lib in selected(&store, library)? {
        let started = std::time::Instant::now();
        let s = transcodarr_server::summarize(store.pool(), &lib.id)?;
        print!("{}", s.render());
        println!("  ({} ms)\n", started.elapsed().as_millis());
    }
    Ok(())
}

/// Cancel one job.
///
/// The refusals carry their own explanation, so this prints the error as-is
/// rather than restating it — the whole point of `CancelRefused` is that
/// "Committing" and "Succeeded" need different next moves from the operator.
fn cancel_job(db: PathBuf, id: String, reason: Option<String>, force: bool) -> Result<()> {
    let store = open_store(&db)?;
    let from = store.cancel_job(&id, reason.as_deref(), force)?;
    println!("cancelled {id} (was {from})");
    if from.holds_capacity() {
        // Said plainly because the alternative is an operator watching the
        // node and concluding the cancel did not take.
        println!(
            "  an agent was holding it; the server revokes the assignment on \
             that agent's next heartbeat"
        );
    }
    Ok(())
}
