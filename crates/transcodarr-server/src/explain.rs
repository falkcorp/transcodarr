// file: crates/transcodarr-server/src/explain.rs
// version: 1.2.0
// guid: 4e0d29b7-8c61-4a35-b7f8-13a95206ecd4
// last-edited: 2026-08-16
//! Answering "why is this file not being transcoded?".
//!
//! The question an operator actually asks, and the one Tdarr could never
//! answer. Everything here is read from stored facts — no probe, no filesystem
//! access beyond resolving the path — so asking is cheap enough to ask often.
//!
//! The rule trace is the point. "No work" is not an answer; "matched rule
//! `already-eac3`, which yields no audio work" is one an operator can act on,
//! and it names the rule they would have to edit to change the outcome.

use transcodarr_core::policy::{self, Decision, Policy, RuleTrace};
use transcodarr_store::ReadPool;
use transcodarr_store::repo::{
    FileRecord, FileRepo, JobRecord, JobRepo, LibraryRecord, LibraryRepo,
};

use crate::ServerError;

/// Everything known about one file, and why.
#[derive(Debug, Clone)]
pub struct Explanation {
    /// The stored row.
    pub file: FileRecord,
    /// Which library it belongs to.
    pub library: LibraryRecord,
    /// The decision the *current* policy reaches from the stored facts.
    ///
    /// Recomputed rather than read back, so a stale stored decision shows up as
    /// a disagreement with `stored_decision` instead of being reported as the
    /// truth. That difference is exactly what an operator is chasing when they
    /// ask why a policy edit had no effect.
    pub decision: Option<Decision>,
    /// Which rules matched, and what each contributed.
    pub trace: Vec<RuleTrace>,
    /// The open job, when there is one.
    pub job: Option<JobRecord>,
    /// Why that job did not dispatch last round.
    pub blocked: Option<transcodarr_store::repo::DispatchBlock>,
    /// Whether the stored decision still agrees with the current policy.
    pub stored_is_current: bool,
}

impl Explanation {
    /// A human-readable rendering.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("path      {}\n", self.file.canonical_path));
        out.push_str(&format!(
            "library   {} ({})\n",
            self.library.name, self.library.id
        ));
        out.push_str(&format!(
            "size      {:.2} GiB\n",
            self.file.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        ));
        out.push_str(&format!("state     {}\n", self.file.state));

        match &self.file.facts {
            None => {
                // A file that was probed and failed is a different problem from
                // one nobody has looked at yet: the first needs the file
                // investigated, the second needs a probe run. Reporting both as
                // "not probed" sends half the operators to the wrong place.
                if self.file.state == transcodarr_core::file::FileState::ProbeFailed {
                    out.push_str("\nfacts     none -- the probe FAILED for this file\n");
                    out.push_str(&format!(
                        "reason    {}\n",
                        self.file
                            .decision_reason
                            .as_deref()
                            .unwrap_or("(not recorded)")
                    ));
                    out.push_str("          nothing can be decided until the file is readable\n");
                } else {
                    out.push_str("\nfacts     none stored -- this file has not been probed yet\n");
                    out.push_str("          nothing can be decided until it is\n");
                }
                return out;
            }
            Some(f) => {
                out.push_str(&format!(
                    "\nvideo     {} {} {}\n",
                    f.video_codec.as_deref().unwrap_or("none"),
                    f.video_profile.as_deref().unwrap_or(""),
                    f.video_bit_depth
                        .map(|d| format!("{}-bit", d.bits()))
                        .unwrap_or_default()
                ));
                out.push_str(&format!(
                    "audio     {} ({} track{})\n",
                    if f.audio_codecs.is_empty() {
                        "none".to_string()
                    } else {
                        f.audio_codecs.join(", ")
                    },
                    f.audio_track_count,
                    if f.audio_track_count == 1 { "" } else { "s" }
                ));
                out.push_str(&format!("subtitles {}\n", f.subtitle_track_count));
                if f.is_hdr || f.is_dovi || f.has_object_audio {
                    out.push_str(&format!(
                        "flags     {}{}{}\n",
                        if f.is_hdr { "HDR " } else { "" },
                        match (f.is_dovi, f.dovi_profile) {
                            (true, Some(p)) => format!("DolbyVision(profile {p}) "),
                            (true, None) => "DolbyVision ".to_string(),
                            _ => String::new(),
                        },
                        if f.has_object_audio {
                            "object-audio"
                        } else {
                            ""
                        }
                    ));
                }
            }
        }

        if let Some(d) = &self.decision {
            out.push_str(&format!("\ndecision  {} -- {}\n", d.class, d.reason));
        }
        if !self.stored_is_current {
            out.push_str(
                "          NOTE: the stored decision predates the current policy;\n\
                 \x20         run an evaluation to bring it up to date\n",
            );
        }

        if !self.trace.is_empty() {
            out.push_str("\nrules\n");
            for t in &self.trace {
                out.push_str(&format!(
                    "  {} {:<28} {}\n",
                    if t.matched { "✓" } else { "·" },
                    t.rule,
                    t.effect
                ));
            }
        }

        match (&self.job, &self.blocked) {
            (Some(j), Some(b)) => {
                out.push_str(&format!(
                    "\njob       {} {} ({})\n          not dispatching: {}\n",
                    j.id, j.state, j.class, b.blocking_stage
                ));
                // The stage alone says "capability" and stops. The requirement
                // that actually went unmet is the whole answer, and printing
                // only the category means the operator's next move is still to
                // restart the server under a debug filter.
                if let Some(reason) = b.reason() {
                    out.push_str(&format!("          {reason}\n"));
                }
            }
            (Some(j), None) => {
                out.push_str(&format!("\njob       {} {} ({})\n", j.id, j.state, j.class));
            }
            (None, _) => out.push_str("\njob       none open\n"),
        }
        out
    }
}

/// Reads the store to explain one file.
pub struct Explainer {
    files: FileRepo,
    jobs: JobRepo,
    libraries: LibraryRepo,
    blocks: transcodarr_store::repo::DispatchBlockRepo,
}

impl Explainer {
    /// Build an explainer over a read pool.
    pub fn new(pool: ReadPool) -> Self {
        Self {
            files: FileRepo::new(pool.clone()),
            jobs: JobRepo::new(pool.clone()),
            libraries: LibraryRepo::new(pool.clone()),
            blocks: transcodarr_store::repo::DispatchBlockRepo::new(pool),
        }
    }

    /// Explain the file at `canonical_path`.
    ///
    /// The path is hashed and looked up per library rather than matched as a
    /// string, because `idx_file_path` is on `(library_id, path_hash)` — a
    /// `LIKE` would be a full scan of 49,600 rows for a question asked from an
    /// interactive command.
    pub fn explain(
        &self,
        canonical_path: &str,
        policy: &Policy,
    ) -> Result<Explanation, ServerError> {
        let path_hash = transcodarr_core::stable_hash(canonical_path.as_bytes());
        let libraries = self.libraries.list_enabled()?;

        let mut found: Option<(LibraryRecord, FileRecord)> = None;
        for lib in libraries {
            if let Some(rec) = self.files.get_by_path_hash(&lib.id, &path_hash)? {
                found = Some((lib, rec));
                break;
            }
        }
        let (library, file) = found.ok_or_else(|| ServerError::UnknownPath {
            path: canonical_path.to_string(),
        })?;

        let (decision, trace) = match file.facts.as_ref() {
            Some(facts) => {
                let (d, t) = policy::evaluate_explained(facts, policy);
                (Some(d), t)
            }
            None => (None, Vec::new()),
        };

        let rules_version = policy::rules_version(policy);
        let stored_is_current =
            file.eval_rules_version.as_deref() == Some(rules_version.0.as_str());

        let job = self.jobs.open_for_file(file.id)?;
        let blocked = match &job {
            Some(j) => self.blocks.get(&j.id)?,
            None => None,
        };

        Ok(Explanation {
            file,
            library,
            decision,
            trace,
            job,
            blocked,
            stored_is_current,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use transcodarr_core::facts::{FileFacts, SizeThresholds};
    use transcodarr_core::plan::BitDepth;
    use transcodarr_store::repo::FileUpsert;
    use transcodarr_store::{Db, WriteLane, Writer};

    struct Harness {
        _dir: TempDir,
        pool: ReadPool,
        writer: Arc<Writer>,
    }

    fn harness() -> Harness {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open_unchecked(&path).unwrap();
        let pool = ReadPool::open(&path, 4).unwrap();
        let h = Harness {
            _dir: dir,
            pool,
            writer: Arc::new(Writer::start(db)),
        };
        h.writer
            .submit_blocking(
                WriteLane::Normal,
                LibraryRepo::upsert_op(LibraryRecord {
                    id: "tv".into(),
                    name: "Television".into(),
                    root_path: "/mnt/tv".into(),
                    work_dir: "/w".into(),
                    trash_dir: "/t".into(),
                    exclude_globs_json: "[]".into(),
                    enabled: true,
                    scan_parallelism: 4,
                    priority: 0,
                    min_mtime_age_s: 300,
                }),
            )
            .unwrap();
        h
    }

    fn truehd_facts() -> FileFacts {
        FileFacts {
            container: "matroska".into(),
            duration_us: Some(1_500_000_000),
            size_bytes: 1_000_000_000,
            bit_rate_bps: Some(8_000_000),
            video_codec: Some("hevc".into()),
            video_profile: Some("Main 10".into()),
            video_bit_depth: Some(BitDepth::Ten),
            video_pix_fmt: Some("yuv420p10le".into()),
            width: Some(1920),
            height: Some(1080),
            is_hdr: false,
            is_dovi: false,
            dovi_profile: None,
            has_object_audio: false,
            audio_codecs: vec!["truehd".into()],
            audio_track_count: 1,
            subtitle_track_count: 2,
        }
    }

    impl Harness {
        fn add(&self, path: &str, facts: Option<FileFacts>) -> i64 {
            let id = self
                .writer
                .submit_blocking(
                    WriteLane::Normal,
                    FileRepo::upsert_op(FileUpsert {
                        library_id: "tv".into(),
                        canonical_path: path.into(),
                        path_hash: transcodarr_core::stable_hash(path.as_bytes()),
                        size_bytes: 1_000_000_000,
                        mtime_unix: 1000,
                        mtime_ns: 0,
                        inode: Some(1),
                        dev: Some(1),
                        nlink: 1,
                        scan_generation: 1,
                    }),
                )
                .unwrap()
                .last_id
                .unwrap();
            if let Some(f) = facts {
                let sig = transcodarr_core::facts::content_sig(&f).0;
                self.writer
                    .submit_blocking(
                        WriteLane::Normal,
                        FileRepo::record_probe_op(
                            id,
                            f,
                            sig,
                            transcodarr_core::facts::SizeBucket::Small,
                            "{}".into(),
                            "ffprobe".into(),
                        ),
                    )
                    .unwrap();
            }
            id
        }
    }

    #[test]
    fn a_probed_file_is_explained_with_its_rule_trace() {
        let h = harness();
        h.add("/mnt/tv/a.mkv", Some(truehd_facts()));
        let policy = policy::default_space_saver();

        let e = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/a.mkv", &policy)
            .unwrap();
        assert!(e.decision.is_some());
        assert!(!e.trace.is_empty(), "the trace is the point of explain");
        let rendered = e.render();
        assert!(rendered.contains("truehd"));
        assert!(rendered.contains("Television"));
        assert!(rendered.contains("rules"));
    }

    /// "No work" is not an answer. An unprobed file must say what is missing
    /// rather than report a decision derived from nothing.
    #[test]
    fn an_unprobed_file_says_so_rather_than_reporting_no_work() {
        let h = harness();
        h.add("/mnt/tv/new.mkv", None);
        let policy = policy::default_space_saver();

        let e = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/new.mkv", &policy)
            .unwrap();
        assert!(e.decision.is_none());
        assert!(e.render().contains("has not been probed"));
    }

    /// A probe that failed and a file nobody has looked at are different
    /// problems: the first needs the file investigated, the second needs a
    /// probe run. Reporting both as "not probed" sends half the operators to
    /// the wrong place.
    #[test]
    fn a_failed_probe_is_distinguished_from_an_unprobed_file() {
        let h = harness();
        let id = h.add("/mnt/tv/broken.mkv", None);
        h.writer
            .submit_blocking(
                WriteLane::Normal,
                FileRepo::record_probe_failure_op(id, "moov atom not found".into()),
            )
            .unwrap();

        let policy = policy::default_space_saver();
        let rendered = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/broken.mkv", &policy)
            .unwrap()
            .render();
        assert!(rendered.contains("probe FAILED"));
        assert!(rendered.contains("moov atom not found"));
        assert!(!rendered.contains("has not been probed yet"));
    }

    /// The operator chasing "I edited the policy and nothing happened" needs
    /// the stored decision's staleness surfaced, not hidden behind a freshly
    /// recomputed one.
    #[test]
    fn a_stale_stored_decision_is_flagged_as_such() {
        let h = harness();
        let id = h.add("/mnt/tv/a.mkv", Some(truehd_facts()));
        h.writer
            .submit_blocking(
                WriteLane::Normal,
                FileRepo::record_decision_op(
                    id,
                    transcodarr_core::policy::DecisionClass::Audio,
                    "decided under an older policy".into(),
                    "some-old-version".into(),
                ),
            )
            .unwrap();

        let policy = policy::default_space_saver();
        let e = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/a.mkv", &policy)
            .unwrap();
        assert!(!e.stored_is_current);
        assert!(e.render().contains("predates the current policy"));
    }

    #[test]
    fn a_current_stored_decision_is_not_flagged() {
        let h = harness();
        let id = h.add("/mnt/tv/a.mkv", Some(truehd_facts()));
        let policy = policy::default_space_saver();
        let rv = policy::rules_version(&policy);
        h.writer
            .submit_blocking(
                WriteLane::Normal,
                FileRepo::record_decision_op(
                    id,
                    transcodarr_core::policy::DecisionClass::Audio,
                    "current".into(),
                    rv.0,
                ),
            )
            .unwrap();

        let e = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/a.mkv", &policy)
            .unwrap();
        assert!(e.stored_is_current);
        assert!(!e.render().contains("predates the current policy"));
    }

    #[test]
    fn an_open_job_is_reported() {
        let h = harness();
        let id = h.add("/mnt/tv/a.mkv", Some(truehd_facts()));
        crate::Evaluator::new(
            h.pool.clone(),
            Arc::clone(&h.writer),
            SizeThresholds::default(),
        )
        .evaluate_library("tv", &policy::default_space_saver())
        .unwrap();

        let e = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/a.mkv", &policy::default_space_saver())
            .unwrap();
        assert!(e.job.is_some(), "the job the evaluator created must show");
        assert_eq!(e.job.as_ref().unwrap().file_id, id);
        assert!(e.render().contains("Audio"));
    }

    /// The whole point of persisting the dispatcher's reasoning is that it
    /// reaches the operator. Rendering only `blocking_stage` prints "capacity"
    /// and stops — true, and useless, because the next move is still to restart
    /// the server under a debug filter and wait for the pass to come round.
    #[test]
    fn a_blocked_job_shows_the_reason_and_not_merely_the_stage() {
        let h = harness();
        h.add("/mnt/tv/a.mkv", Some(truehd_facts()));
        crate::Evaluator::new(
            h.pool.clone(),
            Arc::clone(&h.writer),
            SizeThresholds::default(),
        )
        .evaluate_library("tv", &policy::default_space_saver())
        .unwrap();

        let policy = policy::default_space_saver();
        let job_id = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/a.mkv", &policy)
            .unwrap()
            .job
            .expect("the evaluator should have opened one")
            .id;

        let reason = "no enabled, commit-eligible agent satisfies encoder(eac3)";
        h.writer
            .submit_blocking(
                WriteLane::Normal,
                transcodarr_store::repo::DispatchBlockRepo::upsert_op(
                    job_id,
                    "capability".into(),
                    Some(transcodarr_store::repo::DispatchBlock::detail_for(reason)),
                ),
            )
            .unwrap();

        let rendered = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/a.mkv", &policy)
            .unwrap()
            .render();
        assert!(
            rendered.contains("not dispatching: capability"),
            "{rendered}"
        );
        assert!(rendered.contains(reason), "{rendered}");
        // The stored JSON envelope is an implementation detail of the column,
        // not something to print at an operator.
        assert!(!rendered.contains("{\"reason\""), "{rendered}");
    }

    /// A path nobody has scanned is not an empty explanation — it is a
    /// different question, and answering it as "no work" would mislead.
    #[test]
    fn an_unknown_path_is_named_as_unknown() {
        let h = harness();
        let policy = policy::default_space_saver();
        let err = Explainer::new(h.pool.clone())
            .explain("/mnt/tv/never-seen.mkv", &policy)
            .unwrap_err();
        assert!(matches!(err, ServerError::UnknownPath { .. }), "{err:?}");
    }
}
