- [ ] **REQ-REFRESH** A job's requirements are written once at creation and no
      command can refresh them, so a policy *code* change that alters emitted
      requirements never reaches jobs that already exist.

      Measured on a database created by the pre-change binary: a `VideoGpu` job
      blocked at `capability` on `Decoder(DecoderTriple { codec: "h264",
      profile: "High 10", bit_depth: Ten, kind: Nvdec })` still reads exactly
      that after `admin evaluate` against a binary that no longer emits
      `Nvdec` at all — `evaluated 0, 0 jobs created`.

      Two independent barriers, either of which alone is enough:

      1. `rules_version` (`policy.rs:327`) is `blake3(serde_json(policy))` — a
         hash of the policy *config*. A code change that alters requirement
         generation leaves it byte-identical, so `needs_eval` never returns the
         file and the evaluator loop exits having looked at nothing.
      2. `evaluate_one` (`evaluator.rs:155`) returns `already_busy` as soon as
         an open job exists for the file, which is *before* `next_job`
         recomputes the spec. So even forcing the file back into the working
         set would not rewrite `requirements_json`.

      There is no recourse: `admin` has no cancel, reset or requeue command
      (`diagnose`, `add-library`, `libraries`, `scan`, `evaluate`, `explain`,
      `run`, `summary`), so an operator's only option is editing SQLite by
      hand. The job blocks forever and `explain` names a requirement no
      currently-installed code can ever emit, which reads as a capability gap
      rather than a stale row.

      Needs, roughly in order:

      1. Decide the refresh rule. Requirements on a `Pending` job are pure
         derived data and safe to rewrite; a `Dispatched`/`Running` job has an
         agent holding a lease against the old set and must not be touched
         mid-flight. Refresh `Pending` only, leave the rest to finish or fail.
      2. Rewrite `requirements_json` *and* `requirements_bucket_key` together —
         dispatch matches on the bucket key, so refreshing one without the
         other is worse than refreshing neither.
      3. A way to force the file back into the working set, since barrier 1
         means the policy hash will not do it. Either an `admin evaluate
         --all` that ignores the recorded `rules_version`, or fold a build
         identifier into `RulesVersion` so a code change invalidates decisions
         the way a config change already does. The second is more honest about
         what the version means but re-evaluates every file on every upgrade.
      4. An `admin jobs cancel <id>` regardless, as the escape hatch for every
         other way a job can become permanently unsatisfiable.

      Found while shipping the `Software` decode requirement: that change is
      correct for jobs created after it, and invisible to jobs created before.
