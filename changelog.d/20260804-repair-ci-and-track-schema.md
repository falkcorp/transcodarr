### Fixed

#### Repair the CI workflow, and commit the schema it could not see

`.github/workflows/ci.yml` has not parsed since it was written. In the
`detect-changes` checkout step, `fetch-depth: 0` was indented two spaces deeper
than the `submodules: recursive` it is a sibling of, which makes it a mapping
value in a position YAML does not allow one. Every run failed in zero seconds at
the workflow-parse stage, reported only as "this run likely failed because of a
workflow file issue" — so `cargo fmt`, `cargo clippy`, `cargo test` and
`cargo build --release` have not executed on a runner for the whole of Phase 3
and Phase 4.

With CI dead, a second defect stayed invisible: `crates/transcodarr-store/
migrations/0001_initial.sql` was never committed. A blanket `*.sql` in the
maintainer's *global* gitignore matched it, and it is the file `db.rs` embeds
with `include_str!`, so a clean clone of `main` could not compile the store
crate at all. It builds on the machine it was written on and nowhere else. The
repository's own `.gitignore` now negates the pattern for
`crates/*/migrations/*.sql`, where a repository-level rule takes precedence over
anyone's `~/`, and the schema is committed unchanged.

The `ci-summary` job's script was also moved off unquoted `$GITHUB_STEP_SUMMARY`
and interpolated expressions onto a quoted redirect with the values passed
through `env:`, which is both the injection-safe form and what makes
`actionlint` pass cleanly.
