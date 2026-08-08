// file: crates/transcodarr-agent/src/preflight.rs
// version: 1.1.0
// guid: 4a7d2e69-b038-4517-8c94-1f60e3b7a2d5
// last-edited: 2026-08-07
//! Environment preflight — the four probes that must pass before any
//! orchestrator code is trusted on a machine.
//!
//! These run *before* the dispatcher exists on purpose. If the Windows node
//! cannot rename over an open file, the architecture changes: the GPU agent
//! becomes produce-only and a server-local agent performs commits. Finding that
//! out after a dispatcher has been built around the opposite assumption is the
//! expensive way to learn it.
//!
//! Every probe degrades to `Skipped` where it does not apply rather than
//! failing. A macOS development machine has no cgroup v2 and may have no ZFS;
//! that is not a defect, and a probe that cried failure there would train
//! everyone to ignore the output.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Outcome of a single probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeStatus {
    /// Probe passed.
    Pass,
    /// Probe failed. Read `detail` — this may be architecture-changing.
    Fail,
    /// Probe does not apply on this platform.
    Skipped,
    /// Probe ran but the result is concerning rather than fatal.
    Warn,
}

impl ProbeStatus {
    /// Short marker for the table.
    pub fn marker(self) -> &'static str {
        match self {
            ProbeStatus::Pass => "PASS",
            ProbeStatus::Fail => "FAIL",
            ProbeStatus::Skipped => "SKIP",
            ProbeStatus::Warn => "WARN",
        }
    }
}

/// One probe's result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Probe name.
    pub name: String,
    /// Outcome.
    pub status: ProbeStatus,
    /// What was observed, and what it implies.
    pub detail: String,
}

/// The full preflight report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightReport {
    /// Every probe, in run order.
    pub probes: Vec<ProbeResult>,
}

impl PreflightReport {
    /// Whether any probe failed outright.
    pub fn any_failed(&self) -> bool {
        self.probes.iter().any(|p| p.status == ProbeStatus::Fail)
    }

    /// Whether this machine may perform commits (the replace ritual).
    ///
    /// Gated on `RenameProbe` alone. An agent that cannot atomically replace a
    /// file can still *produce* output; it just must not install it.
    pub fn commit_eligible(&self) -> bool {
        self.probes
            .iter()
            .find(|p| p.name == "RenameProbe")
            .map(|p| p.status == ProbeStatus::Pass)
            .unwrap_or(false)
    }

    /// Render as a pass/fail table.
    pub fn render(&self) -> String {
        let w = self
            .probes
            .iter()
            .map(|p| p.name.len())
            .max()
            .unwrap_or(10)
            .max(6);
        let mut out = format!("{:<w$}  {:<6}  {}\n", "PROBE", "STATUS", "DETAIL", w = w);
        out.push_str(&format!("{}\n", "-".repeat(w + 8 + 40)));
        for p in &self.probes {
            out.push_str(&format!(
                "{:<w$}  {:<6}  {}\n",
                p.name,
                p.status.marker(),
                p.detail,
                w = w
            ));
        }
        out
    }
}

// ---- 1. RenameProbe --------------------------------------------------------

/// Verify that `rename(2)` can replace a file that is currently **open**, and
/// that the destination afterwards is the *new* file.
///
/// This is the single most consequential probe. The whole commit ritual is
/// "write a temp file, then one atomic rename onto the final path". POSIX
/// guarantees that; SMB and some Windows filesystems do not, and will either
/// refuse the rename or leave the destination pointing at the old data while a
/// reader holds it open.
///
/// Holding the destination open during the rename is the entire point — a naive
/// probe that renames over a *closed* file passes on filesystems where the real
/// operation would fail, which is worse than no probe at all.
pub fn rename_probe(dir: &Path) -> ProbeResult {
    let name = "RenameProbe".to_string();
    let dest = dir.join(".transcodarr-preflight-dest");
    let src = dir.join(".transcodarr-preflight-src");

    // Setup failure and rename failure mean completely different things and must
    // never be conflated. "I could not create a file here" is a wrong path or a
    // permissions problem — inconclusive. "I created the files and the rename
    // did the wrong thing" is architecture-changing. Reporting the first as the
    // second would wrongly demote a perfectly capable node to produce-only.
    let setup = || -> std::io::Result<File> {
        fs::write(&dest, b"OLD")?;
        fs::write(&src, b"NEW")?;
        // Hold the destination open across the rename. This is what SMB and
        // Windows sharing semantics typically refuse, and renaming over a
        // *closed* file would pass on filesystems where the real commit fails.
        File::open(&dest)
    };

    let holder = match setup() {
        Ok(h) => h,
        Err(e) => {
            let _ = fs::remove_file(&dest);
            let _ = fs::remove_file(&src);
            return ProbeResult {
                name,
                status: ProbeStatus::Warn,
                detail: format!(
                    "INCONCLUSIVE: could not create test files in {} ({e}). \
                     This says nothing about rename semantics — re-run against a \
                     directory this user can write to, such as the library path the \
                     agent will actually commit into.",
                    dir.display()
                ),
            };
        }
    };

    let renamed = fs::rename(&src, &dest);
    let after = fs::read(&dest);
    drop(holder);
    let _ = fs::remove_file(&dest);
    let _ = fs::remove_file(&src);

    match (renamed, after) {
        (Ok(()), Ok(bytes)) if bytes == b"NEW" => ProbeResult {
            name,
            status: ProbeStatus::Pass,
            detail: "rename over an open destination succeeded; contents are the new file".into(),
        },
        (Ok(()), Ok(bytes)) => ProbeResult {
            name,
            status: ProbeStatus::Fail,
            detail: format!(
                "rename reported success but the destination still holds the old contents \
                 ({} bytes). This machine must NOT be commit-eligible: it can produce \
                 output but a server-local agent must perform the replace.",
                bytes.len()
            ),
        },
        (Err(e), _) => ProbeResult {
            name,
            status: ProbeStatus::Fail,
            detail: format!(
                "rename over an open destination was refused ({e}). This machine must NOT \
                 be commit-eligible: it can produce output but a server-local agent must \
                 perform the replace."
            ),
        },
        (Ok(()), Err(e)) => ProbeResult {
            name,
            status: ProbeStatus::Fail,
            detail: format!(
                "rename succeeded but the destination could not be read back ({e}); \
                 treating as not commit-eligible."
            ),
        },
    }
}

// ---- 2. DB fsync latency ---------------------------------------------------

/// Warn above this p99. Healthy NVMe sits well under 1 ms; 10 ms means the
/// single writer is already the system's pacing constraint.
pub const FSYNC_WARN_US: u128 = 10_000;

/// Abort above this p99.
///
/// Chosen rather than inherited: the store is a single-writer SQLite with
/// `synchronous=FULL` for commit records, so every durable transaction costs at
/// least one fsync. At 100 ms p99 the writer tops out near 10 transactions per
/// second, which cannot keep up with job state transitions across a busy fleet
/// — the queue would stall behind durability rather than behind work. Filesystem
/// *type* is deliberately not the gate; every path here is ZFS, and measured
/// latency is what actually matters.
pub const FSYNC_ABORT_US: u128 = 100_000;

/// Classify a measured fsync p99 against the warn and abort thresholds.
///
/// Split out from [`fsync_probe`] so the thresholds can be tested without
/// depending on the disk underneath the test runner. Measurement and judgement
/// are different jobs: what storage a machine happens to have is a fact about
/// that machine, while where the boundaries sit is a decision this code owns,
/// and only the second one is worth asserting in a unit test.
fn classify_fsync_p99(p99_us: u128) -> ProbeStatus {
    if p99_us > FSYNC_ABORT_US {
        ProbeStatus::Fail
    } else if p99_us > FSYNC_WARN_US {
        ProbeStatus::Warn
    } else {
        ProbeStatus::Pass
    }
}

/// Measure fsync latency on the candidate database path.
pub fn fsync_probe(dir: &Path, iterations: usize) -> ProbeResult {
    let name = "DbFsyncLatency".to_string();
    let path = dir.join(".transcodarr-preflight-fsync");

    let run = || -> anyhow::Result<Vec<u128>> {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        let mut samples = Vec::with_capacity(iterations);
        for i in 0..iterations {
            let buf = format!("{i:016}");
            let t = Instant::now();
            f.write_all(buf.as_bytes())?;
            f.sync_data()?;
            samples.push(t.elapsed().as_micros());
        }
        Ok(samples)
    };

    let result = run();
    let _ = fs::remove_file(&path);

    match result {
        Err(e) => ProbeResult {
            name,
            status: ProbeStatus::Fail,
            detail: format!("could not measure fsync latency: {e}"),
        },
        Ok(mut samples) => {
            samples.sort_unstable();
            let p50 = samples[samples.len() / 2];
            let p99 = samples[(samples.len() * 99 / 100).min(samples.len() - 1)];
            let detail = format!(
                "{} fsyncs: p50 {:.2} ms, p99 {:.2} ms",
                samples.len(),
                p50 as f64 / 1000.0,
                p99 as f64 / 1000.0
            );
            let status = classify_fsync_p99(p99);
            ProbeResult {
                name,
                status,
                detail: match status {
                    ProbeStatus::Fail => format!(
                        "{detail} — above the {} ms abort threshold; the single writer \
                         would be the bottleneck. Move the DB to faster storage.",
                        FSYNC_ABORT_US / 1000
                    ),
                    ProbeStatus::Warn => format!(
                        "{detail} — above the {} ms warn threshold; workable with \
                         batching but worth moving.",
                        FSYNC_WARN_US / 1000
                    ),
                    _ => detail,
                },
            }
        }
    }
}

// ---- 3. ZFS snapshot preflight --------------------------------------------

/// Read ZFS accounting for the dataset holding `path`.
///
/// In-place replacement reclaims nothing while a snapshot still references the
/// old blocks. Without this check `bytes_reclaimed` is a number the system
/// invents: it reports the difference in file sizes while the pool's free space
/// does not move at all. The operator has to know that *before* the first
/// commit, not after 2 TB of "savings" that never materialised.
pub fn zfs_probe(path: &Path) -> ProbeResult {
    let name = "ZfsSnapshotPolicy".to_string();

    let output = std::process::Command::new("zfs")
        .args([
            "list",
            "-Hp",
            "-o",
            "name,used,usedbysnapshots,available,referenced",
        ])
        .output();

    let Ok(out) = output else {
        return ProbeResult {
            name,
            status: ProbeStatus::Skipped,
            detail: "zfs not available on this machine".into(),
        };
    };
    if !out.status.success() {
        return ProbeResult {
            name,
            status: ProbeStatus::Skipped,
            detail: "zfs present but `zfs list` failed (not a ZFS host, or no permission)".into(),
        };
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let target = path.to_string_lossy();

    // Longest dataset name whose mount-ish prefix matches wins.
    let mut best: Option<(String, u64, u64, u64)> = None;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 5 {
            continue;
        }
        let (ds, used, snaps, avail) = (
            f[0].to_string(),
            f[1].parse::<u64>().unwrap_or(0),
            f[2].parse::<u64>().unwrap_or(0),
            f[3].parse::<u64>().unwrap_or(0),
        );
        if target.contains(ds.split('/').next_back().unwrap_or(&ds))
            && best.as_ref().map(|b| ds.len() > b.0.len()).unwrap_or(true)
        {
            best = Some((ds, used, snaps, avail));
        }
    }

    match best {
        None => ProbeResult {
            name,
            status: ProbeStatus::Skipped,
            detail: format!("no ZFS dataset matched {target}"),
        },
        Some((ds, used, snaps, avail)) => {
            let held_pct = if used > 0 {
                (snaps as f64 / used as f64) * 100.0
            } else {
                0.0
            };
            let detail = format!(
                "{ds}: used {:.1} GB, held by snapshots {:.1} GB ({:.1}%), available {:.1} GB",
                used as f64 / 1e9,
                snaps as f64 / 1e9,
                held_pct,
                avail as f64 / 1e9
            );
            // A material hold, not merely a non-zero one. U0 reports a few MB
            // held against 157 TB used; warning on that trains the operator to
            // ignore the probe. The threshold that matters is whether reclaim
            // reporting would be materially wrong: 1% of used, or 1 GB.
            let material = snaps > 1_000_000_000 || held_pct >= 1.0;
            if material {
                ProbeResult {
                    name,
                    status: ProbeStatus::Warn,
                    detail: format!(
                        "{detail} — snapshots retain replaced data, so reclaim must be \
                         measured from zfs accounting, never from file sizes."
                    ),
                }
            } else {
                ProbeResult {
                    name,
                    status: ProbeStatus::Pass,
                    detail,
                }
            }
        }
    }
}

// ---- 4. CpuQuotaReader -----------------------------------------------------

/// Resolve effective cores, honouring a cgroup v2 CPU quota.
///
/// The raw core count is the wrong number whenever a quota is set — U1 runs
/// under `CPUQuota=1600%` and has 48 cores, so scheduling against 48 would
/// oversubscribe it threefold. Absence of cgroup v2 is normal (macOS, and
/// Linux without a delegated slice) and falls back to the core count.
pub fn cpu_quota_probe() -> ProbeResult {
    let name = "CpuQuotaReader".to_string();
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0);

    let candidates = [
        PathBuf::from("/sys/fs/cgroup/cpu.max"),
        PathBuf::from("/sys/fs/cgroup/system.slice/cpu.max"),
    ];

    for p in candidates.iter() {
        let Ok(text) = fs::read_to_string(p) else {
            continue;
        };
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() != 2 {
            continue;
        }
        if parts[0] == "max" {
            return ProbeResult {
                name,
                status: ProbeStatus::Pass,
                detail: format!(
                    "no quota set; effective cores = {cores:.1} ({})",
                    p.display()
                ),
            };
        }
        let (Ok(quota), Ok(period)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) else {
            continue;
        };
        if period <= 0.0 {
            continue;
        }
        let effective = quota / period;
        return ProbeResult {
            name,
            status: ProbeStatus::Pass,
            detail: format!(
                "cgroup v2 quota {:.0}/{:.0} -> effective cores {:.1} of {cores:.0} physical",
                quota, period, effective
            ),
        };
    }

    ProbeResult {
        name,
        status: ProbeStatus::Skipped,
        detail: format!("no cgroup v2 cpu.max found; effective cores = {cores:.1}"),
    }
}

/// Run every probe against the given work directory and database directory.
pub fn run_all(work_dir: &Path, db_dir: &Path) -> PreflightReport {
    PreflightReport {
        probes: vec![
            rename_probe(work_dir),
            fsync_probe(db_dir, 1000),
            zfs_probe(work_dir),
            cpu_quota_probe(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rename_over_an_open_destination_works_on_a_posix_filesystem() {
        let d = TempDir::new().unwrap();
        let r = rename_probe(d.path());
        assert_eq!(r.status, ProbeStatus::Pass, "{}", r.detail);
    }

    #[test]
    fn rename_probe_leaves_no_litter() {
        let d = TempDir::new().unwrap();
        rename_probe(d.path());
        let left: Vec<_> = fs::read_dir(d.path()).unwrap().collect();
        assert!(left.is_empty(), "probe must clean up after itself");
    }

    /// A directory we cannot write to says nothing about rename semantics.
    /// Reporting FAIL there would wrongly demote a capable node to produce-only
    /// -- which is exactly what happened on U1's root_squash NFS mount.
    #[test]
    fn an_unwritable_directory_is_inconclusive_not_a_failure() {
        let r = rename_probe(Path::new("/nonexistent-transcodarr-preflight"));
        assert_eq!(r.status, ProbeStatus::Warn, "{}", r.detail);
        assert!(r.detail.contains("INCONCLUSIVE"), "{}", r.detail);
        assert!(
            !r.detail.contains("must NOT be commit-eligible"),
            "an inconclusive probe must not imply an architecture change"
        );
    }

    /// Inconclusive is still not eligible: silence is not consent.
    #[test]
    fn an_inconclusive_rename_probe_is_not_commit_eligible() {
        let r = PreflightReport {
            probes: vec![rename_probe(Path::new(
                "/nonexistent-transcodarr-preflight",
            ))],
        };
        assert!(!r.commit_eligible());
    }

    #[test]
    fn fsync_probe_reports_percentiles() {
        let d = TempDir::new().unwrap();
        let r = fsync_probe(d.path(), 50);
        assert!(r.detail.contains("p50"), "{}", r.detail);
        assert!(r.detail.contains("p99"), "{}", r.detail);
    }

    // This test used to also assert `status != Fail`, which is a claim about the
    // disk under whoever is running the suite, not about this code. A loaded CI
    // runner measured a p99 of 199.87 ms and the probe correctly reported Fail —
    // the probe was right and the assertion was wrong. Reporting a slow disk is
    // the whole job; a preflight that cannot say Fail is decoration.
    //
    // What is genuinely this code's decision is where the boundaries sit, so
    // that is what is asserted now — at the boundaries themselves, where an
    // inverted comparison or a swapped constant actually shows up.

    #[test]
    fn a_fast_disk_passes_the_fsync_thresholds() {
        assert_eq!(classify_fsync_p99(0), ProbeStatus::Pass);
        assert_eq!(classify_fsync_p99(FSYNC_WARN_US), ProbeStatus::Pass);
    }

    #[test]
    fn a_middling_disk_warns_rather_than_failing() {
        assert_eq!(classify_fsync_p99(FSYNC_WARN_US + 1), ProbeStatus::Warn);
        assert_eq!(classify_fsync_p99(FSYNC_ABORT_US), ProbeStatus::Warn);
    }

    #[test]
    fn a_disk_past_the_abort_threshold_fails() {
        assert_eq!(classify_fsync_p99(FSYNC_ABORT_US + 1), ProbeStatus::Fail);
        // The 199.87 ms a CI runner actually measured.
        assert_eq!(classify_fsync_p99(199_870), ProbeStatus::Fail);
    }

    #[test]
    fn fsync_probe_cleans_up() {
        let d = TempDir::new().unwrap();
        fsync_probe(d.path(), 10);
        assert!(fs::read_dir(d.path()).unwrap().next().is_none());
    }

    /// Absence of a facility is not a failure. A macOS dev machine has no
    /// cgroup v2 and possibly no ZFS; reporting FAIL there would train everyone
    /// to ignore the output.
    #[test]
    fn inapplicable_probes_skip_rather_than_fail() {
        let cpu = cpu_quota_probe();
        assert_ne!(cpu.status, ProbeStatus::Fail);
        let d = TempDir::new().unwrap();
        let zfs = zfs_probe(d.path());
        assert_ne!(zfs.status, ProbeStatus::Fail);
    }

    #[test]
    fn commit_eligibility_follows_the_rename_probe_alone() {
        let d = TempDir::new().unwrap();
        let ok = PreflightReport {
            probes: vec![rename_probe(d.path())],
        };
        assert!(ok.commit_eligible());

        let bad = PreflightReport {
            probes: vec![ProbeResult {
                name: "RenameProbe".into(),
                status: ProbeStatus::Fail,
                detail: String::new(),
            }],
        };
        assert!(
            !bad.commit_eligible(),
            "a node that cannot rename must never install a replacement"
        );
    }

    #[test]
    fn a_report_with_no_rename_probe_is_not_commit_eligible() {
        // Silence is not consent: an absent probe is not a passing one.
        let r = PreflightReport { probes: vec![] };
        assert!(!r.commit_eligible());
    }

    #[test]
    fn run_all_covers_the_four_probes_and_renders() {
        let d = TempDir::new().unwrap();
        let r = run_all(d.path(), d.path());
        assert_eq!(r.probes.len(), 4);
        let table = r.render();
        for n in [
            "RenameProbe",
            "DbFsyncLatency",
            "ZfsSnapshotPolicy",
            "CpuQuotaReader",
        ] {
            assert!(table.contains(n), "table missing {n}");
        }
    }
}
