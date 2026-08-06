<!-- file: changelog.d/20260806-todo-fragments-and-assembler-fix.md -->
<!-- version: 1.0.0 -->
<!-- guid: d62a4175-a2f2-4643-af35-39832f19c3ab -->
<!-- last-edited: 2026-08-06 -->

### Added

#### Every outstanding task now has a `todo.d/` fragment

Fourteen fragments carrying 21 tasks — everything left across Phases 2 through
7, plus the work found while writing them down. Until now the backlog lived in
three places that disagreed: `docs/design/IMPLEMENTATION-HANDOFF.md`,
`NEXT-SESSION.md` (untracked), and the curated sections of `TODO.md`, which
still describe the single-binary transcoder the workspace split retired. A task
that exists in a handoff nobody has reread is not tracked, it is remembered.

Each fragment carries what "done" means and the evidence behind it, because the
recurring cost in this repository is not forgetting a task but rediscovering why
it mattered. Two of them record findings that had never been written down
anywhere:

- **The Phase 2 probe run finished, and failed on 2,271 files** — 1,105 anime,
  316 movies, 850 tv, roughly 11.9 TiB. The handoff predicted 8. Worse than the
  count is where they land: `(not evaluated)` rather than `Quarantined`, so they
  are invisible to the dispatcher instead of refused once with a reason. Being
  refused with a reason instead of silently skipped is a founding requirement of
  this project and the specific Tdarr behaviour it exists to replace. The
  finished run also puts the real queue at **23,107 open Pending jobs**, which
  is the number the Phase 4 load test has to be sized against.
- **`main` does not pass `cargo test` on macOS**, and fails
  nondeterministically — a different test each run, all on an unwrapped
  `DurabilityTooSlow` at `db.rs:244` where the fsync p99 probe measures ~110 ms
  against a hard 100 ms limit. The probe runs on every DB open, so a wall-clock
  threshold gates every test that opens a database.

### Fixed

#### `assemble_todo.py --dry-run` could not be redirected

Progress lines, the per-fragment `collect`/`no-op` log, and the header warnings
all went to stdout alongside the assembled document. `--dry-run > TODO.md` —
the one thing the flag exists to make safe — therefore produced a file opening
with a dozen lines of `collect todo.d/...` above its own header. Progress and
warnings now go to stderr, leaving stdout carrying only what each mode promises:
the document under `--dry-run`, the pending paths under `--check`.

Verified in both directions rather than assumed: `--check` exits 1 with
fragments pending and 0 in a directory with no `todo.ini`, and `--dry-run`
stdout now begins with `<!-- file: TODO.md -->`.

#### `todo.d/README.md` documented two files that have never existed

It stated that fragments are "excluded from markdownlint and prettier via
`.markdownlintignore` / `.prettierignore`". Neither file is in the repository,
and prettier is not wired into CI at all. Fragments **are** linted — the CI job
globs `**/*.md` and honours only the `ignores` list in
`.markdownlint-cli2.jsonc` — so a malformed fragment fails a PR for a file that
is about to be deleted. What actually makes the header exemption safe is
`MD041` being disabled repo-wide, for the opposite reason: every other file
opens with the mandatory header rather than a heading.

This is the same defect class the handoff keeps recording — a documented
mechanism nobody had executed. The fragments added here were linted before
being committed, and pass.

#### `scriv collect` produced a `CHANGELOG.md` that failed this repo's own lint

The changelog system's *gate* half runs on every PR, but its *assembling* half
has never executed here: `changelog-collect.yml` fires only on
`release: published`, there has been no release, and all 63 fragments written
since 2026-07-19 are still pending. Running `scriv collect` against a copy of
them confirmed it works — exit 0, every fragment consumed, three category
headings — and that its output **fails `MD033` and `MD022`**, on the
`<a id='changelog-vX.Y.Z'></a>` anchors scriv emits flush against each `##`
version heading.

Because collect commits with `[skip ci]`, the first release would have landed
that file silently and reddened the next, unrelated docs PR — the same
surfaces-far-from-its-cause shape as the workflow that failed to parse for
months. `CHANGELOG.md` is now excluded from markdownlint: it is generated, never
hand-edited, and its layout belongs to scriv. Disabling `MD022` repo-wide to
accommodate one generated file would have been the worse trade. `TODO-SCRIV`
tracks dry-running the workflow before the first real release.

#### The documented lint command disagreed with CI

Running `markdownlint-cli2 "**/*.md"` locally reported eight failures in
`.remember/`, the Remember plugin's session buffer. It is gitignored, so CI
never sees it and reports none — but it sits in the work tree, so anyone
following the documented command locally sees a red result CI will not
reproduce. A check whose local and CI verdicts disagree teaches people to
ignore the local one. `.remember/**` is now in the `ignores` list, and the bare
command reports 131 files and 0 errors on both sides.
