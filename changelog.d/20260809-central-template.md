<!-- file: changelog.d/20260809-central-template.md -->
<!-- version: 1.0.0 -->
<!-- guid: 5ee2468b-bc6c-4681-be08-0e4cd88b62c6 -->
<!-- last-edited: 2026-08-09 -->

### Changed

#### The executive summary template moved to the standards repository

It was written here on 2026-08-09. An identical template, for the same document
type, had been written independently in `ubuntu-autoinstall-agent` **the same
day** — neither knew about the other. That is the duplication the `.standards`
submodule exists to prevent, caught while it was happening.

Both were merged into one document and it now lives at
`.standards/templates/executive-summary.md` (falkcorp/.github#1), where the 45
repositories carrying that submodule can reference it. The local copy is
retired and `docs/executive-summaries/README.md` points at the shared one,
stating plainly that copying it back would reintroduce the drift.

Each version contributed what the other lacked. From `ubuntu-autoinstall-agent`:
the audience definition, the shape-selection table, the rule against overclaiming
merged-but-not-rolled-out work, both scaffolds, and the update convention — edit
the existing file, bump `version:`, never rename, so one subject stays in one
file. From here: do-not-pad, the requirement to distinguish "tests pass" from
"run against real data" from "not verified", honesty about self-inflicted
damage, and the *what did not get done* and *cost and effort* sections.

### Fixed

#### `CLAUDE.md` named a canonical instruction source that does not exist here

It declared `.github/instructions/` the "Canonical Source for Agent
Instructions". **That directory has never existed in this repository.** It is a
per-repo copy carried by 29 other repos in the org — 530 files in total, with
only one to three distinct versions of each, `rust.instructions.md` being
byte-identical across all 27 copies. The rules genuinely in force here are the
four files under `.standards/instructions/`, which the same document already
cited correctly two sections earlier.

So agents reading `CLAUDE.md` were pointed at an absent path and told it was
authoritative. It now names `.standards/instructions/` and `.standards/templates/`,
and records what the old pointer was, so the correction is not silently
re-reverted by someone copying the section back from another repo.
