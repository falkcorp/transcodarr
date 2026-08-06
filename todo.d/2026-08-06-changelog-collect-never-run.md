- [ ] **TODO-SCRIV** Dry-run `changelog-collect.yml` before the first real
      release, because the assembling half of the changelog system has never
      executed in this repository. The workflow fires only on `release:
      published` (or manual `workflow_dispatch`), there has been no release, and
      63 fragments have accumulated since 2026-07-19 — consumed fragments are
      deleted, and none has ever been. The gate half works: `changelog-check.yml`
      greps the PR diff for an added fragment and is exercised on every PR. It is
      the collect half that is untested, which is this repository's signature
      defect shape — a documented mechanism nobody has run. Verified 2026-08-06
      that `scriv collect` itself succeeds (exit 0, all 63 fragments consumed,
      three category headings emitted) but that its **output failed the repo's
      own markdownlint config** on `MD033` (the `<a id='changelog-vX.Y.Z'></a>`
      anchors scriv emits) and `MD022` (that anchor sits flush against the `##`
      version heading). `CHANGELOG.md` is now excluded from lint for that reason.
      Since collect commits with `[skip ci]`, any remaining problem in its output
      lands silently and reddens the next unrelated docs PR instead — so run it
      via `workflow_dispatch` against a scratch tag and read the diff before
      trusting the first real release. Check two things the test run did not
      settle: scriv does not bump `CHANGELOG.md`'s own `version:` header and the
      file has no `last-edited:` line, both of which the repo-wide header rule
      wants.
