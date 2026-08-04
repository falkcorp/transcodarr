// file: crates/transcodarr-server/src/summary.rs
// version: 1.0.0
// guid: 1d73b8ae-0f42-4c95-86e1-b409f27d3a56
// last-edited: 2026-08-03
//! "What needs transcoding across 85 TB?" — the question Tdarr never answered.
//!
//! Aggregated in SQL rather than by fetching rows and counting here. The whole
//! claim of Phase 2 is that this returns in under a second over ~49,600 files;
//! pulling every row across the crate boundary to sum it would make that false
//! for no benefit — and it is exactly the pattern the store's no-SQL-escapes
//! rule exists to prevent. The queries therefore live in the repositories, and
//! this module only composes and renders them.

use transcodarr_core::file::FileState;
use transcodarr_store::ReadPool;
use transcodarr_store::repo::{FileRepo, JobRepo};

use crate::ServerError;

/// One decision class and what it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionTotals {
    /// The stored decision, or `None` for files not yet evaluated.
    pub decision: Option<String>,
    /// How many files.
    pub files: i64,
    /// How many bytes those files occupy.
    pub bytes: i64,
}

/// What a library looks like right now.
#[derive(Debug, Clone, PartialEq)]
pub struct LibrarySummary {
    /// Which library.
    pub library_id: String,
    /// Live files.
    pub total_files: i64,
    /// Live bytes.
    pub total_bytes: i64,
    /// Files still awaiting a probe.
    pub awaiting_probe: i64,
    /// Files whose probe failed.
    pub probe_failed: i64,
    /// Per-decision totals, largest first.
    pub by_decision: Vec<DecisionTotals>,
    /// Open jobs by state.
    pub open_jobs: Vec<(String, i64)>,
}

impl LibrarySummary {
    /// A human-readable rendering.
    pub fn render(&self) -> String {
        let gib = |b: i64| b as f64 / (1024.0 * 1024.0 * 1024.0);
        let mut out = format!(
            "library {}: {} files, {:.1} GiB\n",
            self.library_id,
            self.total_files,
            gib(self.total_bytes)
        );
        if self.awaiting_probe > 0 || self.probe_failed > 0 {
            out.push_str(&format!(
                "  awaiting probe {}, probe failed {}\n",
                self.awaiting_probe, self.probe_failed
            ));
        }
        out.push_str("\n  decision              files          GiB\n");
        for d in &self.by_decision {
            out.push_str(&format!(
                "  {:<20} {:>7} {:>12.1}\n",
                d.decision.as_deref().unwrap_or("(not evaluated)"),
                d.files,
                gib(d.bytes)
            ));
        }
        if !self.open_jobs.is_empty() {
            out.push_str("\n  open jobs\n");
            for (state, n) in &self.open_jobs {
                out.push_str(&format!("  {state:<20} {n:>7}\n"));
            }
        }
        out
    }
}

/// Summarise one library.
pub fn summarize(pool: &ReadPool, library_id: &str) -> Result<LibrarySummary, ServerError> {
    let files = FileRepo::new(pool.clone());
    let jobs = JobRepo::new(pool.clone());

    let (total_files, total_bytes) = files.totals(library_id)?;
    let states = files.state_counts(library_id)?;
    let count_of = |want: FileState| {
        states
            .iter()
            .find(|(s, _)| *s == want)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };

    Ok(LibrarySummary {
        library_id: library_id.to_string(),
        total_files,
        total_bytes,
        awaiting_probe: count_of(FileState::Discovered),
        probe_failed: count_of(FileState::ProbeFailed),
        by_decision: files
            .decision_totals(library_id)?
            .into_iter()
            .map(|(decision, f, b)| DecisionTotals {
                decision,
                files: f,
                bytes: b,
            })
            .collect(),
        open_jobs: jobs
            .open_counts_for_library(library_id)?
            .into_iter()
            .map(|(s, n)| (s.to_string(), n))
            .collect(),
    })
}
