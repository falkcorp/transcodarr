<!-- file: changelog.d/20260809-july-executive-summary.md -->
<!-- version: 1.0.0 -->
<!-- guid: d76f87e8-513a-4e5f-a8a9-ced1cab2d631 -->
<!-- last-edited: 2026-08-09 -->

### Added

#### July 2026 executive summary, completing the June–August set

Three merged pull requests (#8–#10), 13 commits, 43 files, +17,833/−1,977 — and
a pull-request count that badly understates the month. July's real output is a
**14,859-line design commit** written on the 31st: a 2,690-line architecture, a
360-line naming contract, and the eight-phase delivery plan that all of August
executed against.

The summary argues that the sequencing decision was the month's most valuable
artefact, not the specification itself. Phases are ordered by risk rather than
by demonstrability, which put every operation capable of destroying a media file
ahead of the distributed machinery that would run it across many machines. Four
such defects were caught in August before the system ran at any scale — that is
the return on July, and it is the number this document exists to make visible.

Also recorded: the first appearance of the pattern that dominates the other two
roundups. #14 found that the linter had **never run** — one unrecognised option
makes it reject its whole configuration, do nothing, and exit successfully.
July is where the project started actively distrusting green results, three
weeks before it discovered the automated build had not run either.

#### A counting seam, named rather than smoothed over

PRs #11–#14 were written on 31 July and merged on 1 August. The August roundup
counts by merge date and therefore claims them; this one describes them without
adding them to its own total. Nothing is double-counted, but a reader comparing
the two documents will notice the discontinuity, so it is stated in the header
rather than left to be rediscovered.

#### One item flagged as unverifiable

July's operational work on the live Tdarr instance — correcting a queue-ordering
regression and raising worker capacity on the CPU node — is included because it
was real effort in the period, and explicitly flagged because it **cannot be
checked the way everything else can**: those scripts are not in version control,
so it rests on operational notes rather than a reviewable commit. Omitting it
would understate the month; including it silently would overstate the evidence.

### Changed

#### The three roundups now cross-link and carry a combined argument

Each summary links its siblings, and the template records what the set shows
that no single document does: June looks like nothing happened, July looks
quiet, August looks enormous — yet June's one-line mistake created much of
August's cost and July's design is what let August move at all. A month's
pull-request count is a poor proxy for its value in either direction, which is
the case for writing these at all.
