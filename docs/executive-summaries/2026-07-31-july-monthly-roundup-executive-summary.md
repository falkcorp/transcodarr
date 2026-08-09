<!-- file: docs/executive-summaries/2026-07-31-july-monthly-roundup-executive-summary.md -->
<!-- version: 1.0.0 -->
<!-- guid: 7b9ca10b-8254-4d2b-a7ec-d2765a885e27 -->
<!-- last-edited: 2026-08-09 -->

# Executive Summary: July 2026 Monthly Roundup

**Shipped:** PRs [#8–#10](https://github.com/falkcorp/transcodarr/pulls?q=is%3Apr+is%3Amerged+merged%3A2026-07-01..2026-07-31),
covering 2026-07-01 through 2026-07-31 (3 merged pull requests; 13 commits, 43
files changed, +17,833/−1,977 lines). A further four pull requests (#11–#14)
were **written on 31 July and merged on 1 August** — see the note on counting
below.
**Prepared:** 2026-08-09
**Related docs:**
[June roundup](2026-06-30-june-monthly-roundup-executive-summary.md) ·
[August roundup](2026-08-09-august-monthly-roundup-executive-summary.md)

July is the month the project was decided rather than built. Three merged pull
requests is a misleadingly small number: the substance of the month is a
**14,859-line design commit** written on the last day, which set the build order
that all of August then executed against.

**A note on counting, so these three documents reconcile.** The August roundup
counts pull requests by *merge* date, which puts #11–#14 in August. Their work
was done on 31 July. Nothing is double-counted — the August document counts them
once, and this one describes them without adding them to its own total — but a
reader comparing the two will notice the seam, and it is better named than
explained away.

## Executive Summary

- **The distributed architecture was designed in full (#11, written 31 July).**
  A 2,690-line specification, a 360-line naming contract fixing every database
  table, message, metric and job state, and an eight-phase delivery plan. This
  is the document August built from, and the reason August could move at 63
  pull requests in eight days.
- **The delivery order was chosen to retire irreversible risks first.** The
  plan deliberately sequences the operations that can destroy a media file
  ahead of the distributed machinery that would run them across many machines.
  This is the single most consequential decision of the month, and it is a
  sequencing decision rather than a technical one.
- **Two record-keeping systems were adopted (#8, #9, #10).** Changelog entries
  and to-do items are now added as individual files that a scheduled job folds
  into the shared documents. This exists because the project is built largely
  by AI agents working in parallel, and parallel work editing the same region
  of the same file collides on every change.
- **The project was renamed (#12, written 31 July).** `transcoderr` →
  `transcodarr`, to match the naming convention of the media-management
  software family it sits alongside.
- **The existing command-line tool got its first correctness fixes (#13, #14,
  written 31 July).** Two real defects in the tool that already existed, one of
  which meant a preview mode was not a preview.
- **Production operations continued on the system being replaced.** The live
  Tdarr instance — still doing the actual work while its replacement was
  designed — had a dispatch setting corrected and its worker capacity raised.

**Highest-risk items this month** — a short list, because the month produced
mostly documents rather than running code:

- **#13 — "dry run" was changing the filesystem.** The mode whose entire
  purpose is to show what *would* happen without doing it was creating
  directories on disk. Separately, an unrecognised quality preset was silently
  ignored rather than rejected, so a typo produced a real transcode at
  unintended settings instead of an error.
- **#14 — the code linter had never run, and looked like it was passing.** A
  single unrecognised option in its configuration file makes the tool reject
  the *entire* configuration and refuse to run. The command still exits
  successfully, which is indistinguishable from "no problems found". The
  project had no lint coverage at all up to this point. Repairing it
  immediately surfaced four genuine warnings that had been invisible.
- **The 414-task inventory was published unaudited**, and is a trap for anyone
  who finds it later. It was produced by an automated pass that was interrupted
  before its verification stage, was never reconciled against the architecture
  document, and may be missing whole subsystems. Its size makes it look
  authoritative. It is explicitly marked as reference-only, and the eight-phase
  plan supersedes it.

**Verification note.** Almost nothing in this month is verifiable by execution,
because almost nothing executable was produced — that is a fair description of
a design month, not a criticism of it. The design documents were checked
mechanically against each other rather than by review alone: every database
table referenced in example SQL was confirmed to be declared (21 of 21), every
declared table confirmed to be referenced, column definitions compared across
all 21 tables (zero differences), and message names, job states and metric
names cross-checked (64 of 64 metrics after two omissions were back-ported).
The two code fixes were verified by test. The linter repair was verified by
confirming the tool actually *ran*, not merely that the command succeeded —
which is the specific mistake that had hidden the problem.

## What changed, in plain terms

### 1. The design

**What it was:** The month's real output, written on 31 July: a full
specification for a distributed media-transcoding system — 2,690 lines of
architecture, a 360-line naming contract, and a 414-task inventory. 14,859
lines across three files in a single commit.

**Why it mattered:** The system being designed replaces Tdarr, which the owner
had been running in production and had accumulated specific, concrete
complaints about. The design is organised around those complaints rather than
around a feature list — each is a requirement with a named failure it prevents:

- Jobs declare what they need from a machine and are **rejected once with a
  reason** when nothing can run them, instead of being retried forever. The
  system being replaced retries indefinitely, so a permanently unroutable job
  is indistinguishable from a busy queue.
- A sensible default policy that works with **zero configuration**, rather than
  requiring a visual flow to be assembled before anything happens.
- Rules kept as **version-controlled text**, never a drag-and-drop flow
  builder — so a change to what the system does is reviewable, diffable and
  revertible.
- **Two ways to move work** — shared network storage with path translation, and
  direct streaming — chosen per machine rather than imposed globally.
- Scheduling that **adapts to observed load** instead of a fixed worker count.

**The fix:** The specification was written with an eight-phase delivery plan
attached, and — importantly — the phases are ordered by *risk*, not by
demonstrability. Phase 0 is a hardware pre-flight check that must run on the
real machines before any code depends on their behaviour, because the answer
changes the architecture. The file-replacement safety work comes before the
distributed dispatch that would multiply its consequences. Anything that makes
for a good demo comes last.

The design also carries a deliberate constraint worth recording: the fleet is
small (a handful of machines) and the library is large, so the storage layer is
a single-file database with no clustering and no leader election. Choosing the
*simpler* option explicitly, with the reasoning written down, is what stops a
later reader "upgrading" it.

### 2. Record-keeping for parallel work

**What it was:** Two systems adopted mid-month. Changelog entries are written as
individual files under `changelog.d/` and assembled at release time by an
established open-source tool (#8). To-do items work the same way under
`todo.d/`, assembled by a purpose-built script because no equivalent tool exists
for to-do lists (#9). Both were then documented for the AI agents that do most
of the work here (#10).

**Why it mattered:** This is not bookkeeping for its own sake. When several
contributors — human or automated — open changes in parallel, every one of them
that wants to add a changelog line edits the same region of the same file, and
they all conflict. One file per entry means no two changes ever touch the same
file. The cost of *not* doing this scales with how parallel the work is, and
August ran at 63 pull requests in eight days.

**The fix:** Both systems are opt-in by the presence of a configuration file, so
they are harmless in a repository that has not adopted them, and both delete the
fragments they consume so nothing is ever folded in twice. A required check
enforces a changelog entry on every pull request; to-do entries are deliberately
*not* enforced, because adding a task should not be a condition of shipping a
fix.

An honest footnote, established later: the changelog half of this was only ever
half-exercised. The check that *requires* an entry runs on every pull request
and works. The assembler that folds entries into the final document runs only at
release time, and as of this writing has still never run — 63 entries have
accumulated behind it. That was found and recorded in August.

### 3. The rename

**What it was:** `transcoderr` became `transcodarr` (#12), matching the naming
convention of the media-management software family it belongs alongside.

**Why it mattered:** Mostly cosmetic, but it produced a lesson worth keeping.
Sweeping the old name to the new one across the repository also rewrote it
inside URLs, producing links to a repository path that had never existed and
therefore would never redirect. Identifiers and addresses look alike to a
search-and-replace and are not the same thing.

**The fix:** The rename was applied, the broken URLs corrected, and the local
working copy deliberately left under the old directory name to avoid breaking
every absolute path in the project's own documentation at the same moment.

### 4. First fixes to the tool that already existed

**What it was:** Before any of the new system was built, the existing
command-line transcoder got two corrections (#13, #14) plus the four warnings
the repaired linter could finally see.

**Why it mattered:** The dry-run defect is the more serious of the two by a wide
margin. A preview mode exists so a user can check what a command will do before
letting it touch anything; one that creates directories has broken the only
promise it makes. The linter defect is subtler and, in hindsight, the more
instructive: the tool was failing *open*. It rejected its whole configuration
because of one unknown option, ran nothing, and reported success. Every check of
"did the linter pass?" returned yes.

**The fix:** Dry-run was made genuinely read-only and unknown presets now produce
an error instead of being ignored (#13). The linter configuration was repaired
(#14) and the four real warnings it had never been able to report were fixed.

This is the first appearance of a pattern that recurs through August and is
worth naming here, at its origin: **a check that cannot fail is decoration.**
The project has since found several — a linter that never ran, an automated
build that never parsed, a database lookup that could only ever return "not
found", and a safety harness that could not report a violation. Each looked
green.

### 5. Keeping the production system running

**What it was:** Tdarr — the system this project replaces — remained in
production throughout, doing the real transcoding work. Two operational changes
were made to it in July: a queue-ordering setting that had regressed was set
back, and the worker capacity on the CPU transcode machine was raised from 4 to
8.

**Why it mattered:** The queue-ordering setting is the larger of the two. With
several thousand files queued, the server re-sorted the entire queue on every
single dispatch request; switching it back to no sorting removed that work
entirely and was the single biggest throughput improvement available. Worker
capacity was raised after measuring that audio-only work uses far less CPU than
the video work the previous limit had been sized for.

**The fix:** Both were applied to the live system. **This work is not evidenced
in this repository's history** — the scripts driving that instance are not in
version control — so unlike everything else in this document it rests on
operational notes rather than on a reviewable commit. It is included because it
was real effort in this period and omitting it would understate the month; it is
flagged because it cannot be checked the way the rest can.

## What did not get done

- **Nothing was implemented.** No part of the designed system was built in July.
  Construction began on 1 August.
- **The hardware pre-flight was designed but not run.** The plan is explicit
  that it must execute on the real machines *before* Phase 2, because a failure
  on the Windows GPU machine changes the architecture. As of this writing it has
  still not been run on that machine — it remains outstanding two months later,
  which is exactly the sort of deferral this section exists to make visible.
- **The 414-task inventory was left unaudited** rather than reconciled against
  the architecture document.
- **The design's own known blemishes were left in place** — two placeholder
  values inside example configuration blocks, and one file that cannot carry the
  project's standard header because its format does not support comments.

## Cost and effort notes

**Volume.** 3 merged pull requests, 13 commits, 43 files, +17,833/−1,977. By
pull-request count this is the quietest month of the three; by consequence it is
arguably the most important, because it is the month that determined what
August would build and in what order.

**What the design bought.** August delivered 63 pull requests and five of eight
phases in eight days, against a specification written the day before it started.
That pace is only available when the sequencing argument has already been had —
and the sequencing is what mattered most, since it front-loaded every operation
capable of destroying a media file. Four such defects were caught in August
before the system ran at any scale. That is the return on July.

**What July also cost.** The month closed with the discovery that the project's
linter had never run at all. It did not yet close with the discovery that its
automated build had not run either — that came on 4 August, and it had been
broken since 13 June. Both belong to the same family, and both were invisible
for the same reason: the tooling reported success while doing nothing. July is
where the project started actively distrusting green results, and the three
roundups together are largely a record of what that distrust kept finding.
