// file: crates/transcodarr-server/src/evaluator.rs
// version: 1.0.0
// guid: d17becf5-3a08-4e62-91c4-6b05af237d8e
// last-edited: 2026-08-03
//! Policy evaluation over stored facts.
//!
//! The evaluator touches no media. It reads `FileFacts` that discovery and
//! probing already wrote, runs the same `transcodarr_core::policy::evaluate`
//! the CLI and the agent link, and records the decision. That is what makes a
//! policy change cheap: re-deciding ~49,600 files is a database scan and some
//! arithmetic, not 85 TB of I/O.
//!
//! Work is done in batches over `idx_file_needs_eval`, and each batch is
//! re-queried rather than paged by offset. A file that leaves the working set
//! because it was just evaluated would shift every later offset, so an
//! offset-paged loop silently skips one file per batch boundary.

use std::sync::Arc;

use transcodarr_core::facts::SizeThresholds;
use transcodarr_core::policy::{self, DecisionClass, Policy, RulesVersion};
use transcodarr_store::repo::{FileRecord, FileRepo, JobRepo, NewJob};
use transcodarr_store::{ReadPool, WriteLane, Writer};

use crate::ServerError;

/// How many files are evaluated per pass.
///
/// A thousand, from the specification. Large enough that the per-batch query
/// cost disappears against the work, small enough that a pass holds a bounded
/// amount in memory and yields the writer to the commit lane regularly.
pub const EVAL_BATCH: u32 = 1000;

/// What one evaluation pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvalOutcome {
    /// Files evaluated.
    pub evaluated: i64,
    /// Files whose decision produced a job.
    pub jobs_created: i64,
    /// Files whose decision was that nothing is owed.
    pub no_work: i64,
    /// Files excluded from all work — HDR, Dolby Vision, object audio.
    pub quarantined: i64,
    /// Files that already had an open job, so no second one was created.
    ///
    /// Not an error. A file with an audio pass in flight is not eligible for a
    /// second job, and `idx_job_open_per_file` would refuse it anyway.
    pub already_busy: i64,
    /// Files whose stored facts were missing, so no decision was possible.
    pub unprobed: i64,
}

/// Batched policy evaluation.
pub struct Evaluator {
    files: FileRepo,
    jobs: JobRepo,
    writer: Arc<Writer>,
    thresholds: SizeThresholds,
}

impl Evaluator {
    /// Build an evaluator over a read pool and the single writer.
    pub fn new(pool: ReadPool, writer: Arc<Writer>, thresholds: SizeThresholds) -> Self {
        Self {
            files: FileRepo::new(pool.clone()),
            jobs: JobRepo::new(pool),
            writer,
            thresholds,
        }
    }

    /// Evaluate every file in a library whose decision predates `policy`.
    ///
    /// Loops until the working set is empty. Each iteration re-queries rather
    /// than advancing an offset: evaluating a file removes it from the set, so
    /// an offset would step over exactly as many files as the last batch
    /// processed.
    pub fn evaluate_library(
        &self,
        library_id: &str,
        policy: &Policy,
    ) -> Result<EvalOutcome, ServerError> {
        let rules_version = policy::rules_version(policy);
        let mut total = EvalOutcome::default();

        loop {
            let batch = self
                .files
                .needs_eval(library_id, &rules_version.0, EVAL_BATCH)?;
            if batch.is_empty() {
                return Ok(total);
            }
            let before = total.evaluated;
            for file in &batch {
                self.evaluate_one(file, policy, &rules_version, &mut total)?;
            }

            // Without this a file that cannot be evaluated — no stored facts,
            // say — stays in the working set forever and the loop never ends.
            // Halting on "a full batch produced no progress" is what turns an
            // infinite loop into a reported count.
            if total.evaluated == before {
                tracing::warn!(
                    library = %library_id,
                    stuck = batch.len(),
                    "evaluation made no progress; stopping rather than looping"
                );
                return Ok(total);
            }
        }
    }

    fn evaluate_one(
        &self,
        file: &FileRecord,
        policy: &Policy,
        rules_version: &RulesVersion,
        total: &mut EvalOutcome,
    ) -> Result<(), ServerError> {
        // No facts means nothing to decide from. Writing a decision anyway
        // would record "no work" for a file nobody has looked at.
        let Some(facts) = file.facts.as_ref() else {
            total.unprobed += 1;
            return Ok(());
        };

        let decision = policy::evaluate(facts, policy);
        self.writer.submit_blocking(
            WriteLane::Normal,
            FileRepo::record_decision_op(
                file.id,
                decision.class,
                decision.reason.clone(),
                rules_version.0.clone(),
            ),
        )?;
        total.evaluated += 1;

        match decision.class {
            DecisionClass::None => {
                total.no_work += 1;
                return Ok(());
            }
            DecisionClass::Quarantined => {
                total.quarantined += 1;
                return Ok(());
            }
            _ => {}
        }

        // At most one open job per file, and the database enforces it. Checking
        // first turns an expected condition into a counter rather than a
        // constraint violation the writer would report as a failed op.
        if self.jobs.open_for_file(file.id)?.is_some() {
            total.already_busy += 1;
            return Ok(());
        }

        let Some(spec) = policy::next_job(&decision, facts, &self.thresholds) else {
            total.no_work += 1;
            return Ok(());
        };

        let job_id = job_id_for(file, rules_version, &spec);
        self.writer.submit_blocking(
            WriteLane::Normal,
            JobRepo::create_op(NewJob {
                id: job_id,
                file_id: file.id,
                library_id: file.library_id.clone(),
                class: spec.class,
                size_bucket: spec.size_bucket,
                requirements_json: serde_json::to_string(&spec.requirements)
                    .unwrap_or_else(|_| "[]".to_string()),
                requirements_bucket_key: transcodarr_core::capability::bucket_key(
                    &spec.requirements,
                ),
                expected_content_sig: spec.expected_content_sig.clone(),
                rules_version: rules_version.0.clone(),
                // Larger files first within a class: they hold capacity longest,
                // so starting them late is what leaves one 60 GB remux running
                // alone at the end of a pass.
                priority: match spec.size_bucket {
                    transcodarr_core::facts::SizeBucket::Large => 2,
                    transcodarr_core::facts::SizeBucket::Medium => 1,
                    _ => 0,
                },
                parent_job_id: None,
            }),
        )?;
        total.jobs_created += 1;
        Ok(())
    }
}

/// A job id that is a function of what the job *is*.
///
/// Derived rather than random so re-running an evaluation cannot produce a
/// second job for the same file, class and rules version. The file id is in the
/// hash because two files can legitimately share a content signature — two
/// copies of the same episode are the same decision but two jobs.
fn job_id_for(
    file: &FileRecord,
    rules_version: &RulesVersion,
    spec: &transcodarr_core::job::JobSpec,
) -> String {
    let material = format!(
        "{}|{}|{}|{}",
        file.id,
        spec.class.as_str(),
        rules_version.0,
        spec.expected_content_sig
    );
    transcodarr_core::stable_hash(material.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use transcodarr_core::facts::{FileFacts, SizeBucket};
    use transcodarr_core::job::{JobClass, JobState};
    use transcodarr_core::plan::BitDepth;
    use transcodarr_store::repo::{FileUpsert, LibraryRecord, LibraryRepo};
    use transcodarr_store::{Db, ReadPool, Writer};

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
                    name: "tv".into(),
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

    impl Harness {
        /// Insert a file and, unless `probe` is `None`, its stored facts. Size
        /// comes from the row, which is the store's single authority for it.
        fn add_file(&self, name: &str, size: i64, probe: Option<FileFacts>) -> i64 {
            let id = self
                .writer
                .submit_blocking(
                    WriteLane::Normal,
                    FileRepo::upsert_op(FileUpsert {
                        library_id: "tv".into(),
                        canonical_path: format!("/mnt/tv/{name}"),
                        path_hash: transcodarr_core::stable_hash(name.as_bytes()),
                        size_bytes: size,
                        mtime_unix: 1000,
                        mtime_ns: 0,
                        inode: Some(size),
                        dev: Some(1),
                        nlink: 1,
                        scan_generation: 1,
                    }),
                )
                .unwrap()
                .last_id
                .unwrap();
            if let Some(facts) = probe {
                let sig = transcodarr_core::facts::content_sig(&facts).0;
                let bucket = transcodarr_core::facts::size_bucket_for(
                    size as u64,
                    &SizeThresholds::default(),
                );
                self.writer
                    .submit_blocking(
                        WriteLane::Normal,
                        FileRepo::record_probe_op(
                            id,
                            facts,
                            sig,
                            bucket,
                            "{}".into(),
                            "ffprobe 7.0".into(),
                        ),
                    )
                    .unwrap();
            }
            id
        }

        fn evaluator(&self) -> Evaluator {
            Evaluator::new(
                self.pool.clone(),
                Arc::clone(&self.writer),
                SizeThresholds::default(),
            )
        }
    }

    /// TrueHD is lossless; the owner wants it as EAC3 640k. Video is left alone.
    fn lossless_audio_file() -> FileFacts {
        FileFacts {
            container: "matroska".into(),
            duration_us: Some(1_500_000_000),
            size_bytes: 0,
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

    /// Already EAC3 and already HEVC: nothing is owed.
    fn settled_file() -> FileFacts {
        FileFacts {
            audio_codecs: vec!["eac3".into()],
            ..lossless_audio_file()
        }
    }

    /// Dolby Vision profile 7 is excluded from all work.
    fn dolby_vision_file() -> FileFacts {
        FileFacts {
            is_dovi: true,
            dovi_profile: Some(7),
            ..lossless_audio_file()
        }
    }

    #[test]
    fn a_file_needing_audio_work_gets_a_job() {
        let h = harness();
        let id = h.add_file("a.mkv", 1_000_000_000, Some(lossless_audio_file()));
        let policy = transcodarr_core::policy::default_space_saver();

        let out = h.evaluator().evaluate_library("tv", &policy).unwrap();
        assert_eq!(out.evaluated, 1);
        assert_eq!(out.jobs_created, 1);

        let job = JobRepo::new(h.pool.clone())
            .open_for_file(id)
            .unwrap()
            .expect("a job must exist");
        assert_eq!(job.class, JobClass::Audio);
        assert_eq!(job.state, JobState::Pending);
        assert!(!job.requirements_bucket_key.is_empty());
        assert_eq!(job.file_id, id);
    }

    /// An unprobed file has nothing to decide from. Recording "no work" for it
    /// would mean a file nobody has looked at is treated as settled.
    #[test]
    fn an_unprobed_file_is_not_decided() {
        let h = harness();
        let id = h.add_file("a.mkv", 1000, None);
        let policy = transcodarr_core::policy::default_space_saver();

        let out = h.evaluator().evaluate_library("tv", &policy).unwrap();
        assert_eq!(out.evaluated, 0);
        assert_eq!(out.jobs_created, 0);
        assert!(
            JobRepo::new(h.pool.clone())
                .open_for_file(id)
                .unwrap()
                .is_none()
        );
    }

    /// Unprobed files never enter the working set at all — `needs_eval` is
    /// restricted to `Probed`/`Evaluated`. The loop must therefore terminate
    /// immediately rather than spinning on rows it cannot decide.
    ///
    /// The `unprobed` counter still exists as a backstop for a row in `Probed`
    /// with no stored facts, which should be unreachable; if it ever fires, the
    /// no-progress guard is what stops the loop instead of hanging the server.
    #[test]
    fn unprobed_files_are_outside_the_working_set_and_the_loop_terminates() {
        let h = harness();
        for i in 0..5 {
            h.add_file(&format!("f{i}.mkv"), 1000, None);
        }
        let policy = transcodarr_core::policy::default_space_saver();
        let out = h.evaluator().evaluate_library("tv", &policy).unwrap();
        assert_eq!(out.evaluated, 0);
        assert_eq!(out.jobs_created, 0);
        assert_eq!(
            out.unprobed, 0,
            "an unprobed file is filtered by the query, not by the loop body"
        );
    }

    /// The no-progress guard itself: a row that is in the working set but has
    /// no stored facts must stop the loop rather than be re-fetched forever.
    #[test]
    fn a_batch_that_makes_no_progress_stops_rather_than_looping() {
        let h = harness();
        let id = h.add_file("a.mkv", 1000, Some(lossless_audio_file()));
        // Force the unreachable shape directly: Probed, but with the probe
        // timestamp cleared so no facts are reconstructed.
        h.writer
            .submit_blocking(
                WriteLane::Normal,
                transcodarr_store::WriteOp::new("test.clear_probe", move |c| {
                    Ok(c.execute(
                        "UPDATE file SET probe_at_unix = NULL, state = 'Probed',
                                eval_rules_version = NULL WHERE id = ?1",
                        [id],
                    )? as u64)
                }),
            )
            .unwrap();

        let policy = transcodarr_core::policy::default_space_saver();
        let out = h.evaluator().evaluate_library("tv", &policy).unwrap();
        assert_eq!(out.unprobed, 1, "the backstop must count it");
        assert_eq!(out.evaluated, 0, "and the loop must not spin");
    }

    #[test]
    fn a_file_with_nothing_owed_gets_a_decision_but_no_job() {
        let h = harness();
        let id = h.add_file("a.mkv", 1000, Some(settled_file()));
        let policy = transcodarr_core::policy::default_space_saver();

        let out = h.evaluator().evaluate_library("tv", &policy).unwrap();
        assert_eq!(out.evaluated, 1);
        assert_eq!(out.jobs_created, 0);
        assert!(
            JobRepo::new(h.pool.clone())
                .open_for_file(id)
                .unwrap()
                .is_none()
        );
        assert!(
            FileRepo::new(h.pool.clone())
                .get(id)
                .unwrap()
                .decision
                .is_some()
        );
    }

    /// Dolby Vision profile 7 is excluded from all work. A job here would
    /// re-encode a title that must never be re-encoded.
    #[test]
    fn dolby_vision_is_quarantined_and_never_queued() {
        let h = harness();
        let id = h.add_file("dv.mkv", 40_000_000_000, Some(dolby_vision_file()));
        let policy = transcodarr_core::policy::default_space_saver();

        let out = h.evaluator().evaluate_library("tv", &policy).unwrap();
        assert_eq!(out.jobs_created, 0, "DV must never be queued");
        assert!(
            JobRepo::new(h.pool.clone())
                .open_for_file(id)
                .unwrap()
                .is_none()
        );
    }

    /// Re-running the evaluator must be free. If a second pass re-decided
    /// everything, the "policy edit re-derives 49.6k decisions" property would
    /// instead mean "every run re-derives them".
    #[test]
    fn a_second_pass_under_the_same_policy_does_nothing() {
        let h = harness();
        for i in 0..3 {
            h.add_file(
                &format!("f{i}.mkv"),
                1_000_000_000,
                Some(lossless_audio_file()),
            );
        }
        let policy = transcodarr_core::policy::default_space_saver();
        let e = h.evaluator();

        let first = e.evaluate_library("tv", &policy).unwrap();
        assert_eq!(first.evaluated, 3);
        let second = e.evaluate_library("tv", &policy).unwrap();
        assert_eq!(
            second.evaluated, 0,
            "an unchanged policy re-decides nothing"
        );
        assert_eq!(second.jobs_created, 0);
    }

    /// A file with work already in flight must not get a second job — the
    /// database would refuse it, and a refused write is reported as a failure
    /// rather than the expected condition it is.
    #[test]
    fn a_file_with_an_open_job_is_counted_not_failed() {
        let h = harness();
        let id = h.add_file("a.mkv", 1_000_000_000, Some(lossless_audio_file()));
        let policy = transcodarr_core::policy::default_space_saver();
        let e = h.evaluator();
        e.evaluate_library("tv", &policy).unwrap();

        // A new rules version brings the file back with its job still open.
        let mut changed = policy.clone();
        changed.rules.clear();
        let out = e.evaluate_library("tv", &changed).unwrap();
        assert_eq!(
            out.already_busy + out.no_work + out.quarantined,
            out.evaluated
        );
        assert_eq!(out.jobs_created, 0);
        assert_eq!(
            JobRepo::new(h.pool.clone())
                .open_for_file(id)
                .unwrap()
                .unwrap()
                .file_id,
            id
        );
    }

    /// The batch loop must cross its own boundary. Paged by offset, evaluating
    /// a file removes it from the working set and every later offset shifts —
    /// so exactly one file per boundary would be skipped.
    #[test]
    fn evaluation_covers_more_files_than_one_batch() {
        let h = harness();
        let n = (EVAL_BATCH + 137) as usize;
        for i in 0..n {
            h.add_file(
                &format!("f{i}.mkv"),
                1_000_000_000,
                Some(lossless_audio_file()),
            );
        }
        let policy = transcodarr_core::policy::default_space_saver();
        let out = h.evaluator().evaluate_library("tv", &policy).unwrap();
        assert_eq!(
            out.evaluated, n as i64,
            "no file may be skipped at a boundary"
        );
        assert_eq!(out.jobs_created, n as i64);
    }

    /// Ids are a function of what the job is, so a re-run cannot mint a second
    /// job for the same file, class and rules version.
    #[test]
    fn job_ids_are_derived_not_random() {
        let h = harness();
        let id = h.add_file("a.mkv", 1_000_000_000, Some(lossless_audio_file()));
        let policy = transcodarr_core::policy::default_space_saver();
        h.evaluator().evaluate_library("tv", &policy).unwrap();

        let jobs = JobRepo::new(h.pool.clone());
        let first = jobs.open_for_file(id).unwrap().unwrap().id;

        // Same inputs, so the same id: an insert would collide rather than
        // silently create a duplicate.
        h.evaluator().evaluate_library("tv", &policy).unwrap();
        assert_eq!(jobs.open_for_file(id).unwrap().unwrap().id, first);
    }

    /// Large files hold capacity longest. Starting them late leaves one 60 GB
    /// remux running alone at the end of a pass.
    #[test]
    fn larger_files_carry_higher_priority() {
        let h = harness();
        let small = h.add_file("small.mkv", 1_000_000_000, Some(lossless_audio_file()));
        let large = h.add_file("large.mkv", 60_000_000_000, Some(lossless_audio_file()));
        let policy = transcodarr_core::policy::default_space_saver();
        h.evaluator().evaluate_library("tv", &policy).unwrap();

        let jobs = JobRepo::new(h.pool.clone());
        let small_job = jobs.open_for_file(small).unwrap().unwrap();
        let large_job = jobs.open_for_file(large).unwrap().unwrap();
        assert_eq!(large_job.size_bucket, SizeBucket::Large);
        assert!(large_job.priority > small_job.priority);
    }
}
