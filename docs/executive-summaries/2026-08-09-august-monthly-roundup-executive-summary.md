<!-- file: docs/executive-summaries/2026-08-09-august-monthly-roundup-executive-summary.md -->
<!-- version: 1.0.0 -->
<!-- guid: 78440435-1fc7-4afc-80c9-19c3ceff169d -->
<!-- last-edited: 2026-08-09 -->

# Executive Summary: August 2026 Monthly Roundup

**Shipped:** PRs [#11–#73](https://github.com/falkcorp/transcodarr/pulls?q=is%3Apr+is%3Amerged+merged%3A2026-08-01..2026-08-31),
covering 2026-08-01 through 2026-08-08 (63 merged pull requests; 325 files
changed, +37,317/−1,740 lines across 64 commits)
**Prepared:** 2026-08-09
**Related doc:** [2026-06-30-june-monthly-roundup-executive-summary.md](2026-06-30-june-monthly-roundup-executive-summary.md)
— June's one-line change that disabled automated testing, repaired this month
in #53.

This is a roundup rather than a per-change summary: each section below groups an
arc of related work rather than one pull request. The important pull request
numbers are named inline as evidence.

**What this project is.** transcodarr is a self-hosted replacement for Tdarr,
the media transcoding system currently running the owner's library. It watches
a media library, decides which files are worth re-encoding, and farms that work
out across several machines — converting audio to a smaller format and video to
a more efficient codec while preserving every audio track, subtitle and piece of
metadata. The library it must handle is **49,600 files across roughly 82 TB**.

**Where it got to.** In eight days the project went from a single-file
command-line tool and a design document to a working distributed system: five of
its eight planned phases are code-complete, and a real file was carried all the
way through the pipeline — discovered, analysed, dispatched to a separate
machine, re-encoded, validated, and installed in place with the original safely
retained.

## Executive Summary

- **A distributed architecture, designed then built.** The month opened with a
  2,690-line architecture specification and a binding naming contract (#11), and
  then executed against it bottom-up. The deliberate sequencing put the
  irreversible risks — anything that could destroy a media file — first, before
  any of the machinery that would multiply them existed.
- **The codebase was restructured from one file into six components (#16–#20).**
  What was a 508-line script became a workspace of six parts with clean
  boundaries: a pure decision-making core with no ability to touch disk or
  network, a database layer, a wire protocol, a server, an agent that runs on
  worker machines, and a command-line interface.
- **The media library was catalogued for the first time (#25–#34).** All three
  libraries — 49,600 files, ~82 TB — were discovered in 43 seconds and then
  fully analysed. The system now holds a decision for every file and has
  **23,107 jobs queued** ready to run.
- **File replacement was made crash-safe before anything ran at scale
  (#35–#43).** Replacing a media file in place is the one genuinely dangerous
  operation here. A fault-injecting test harness kills the process at each of
  the nine steps of the replacement sequence and confirms the result is always
  either "original intact" or "replacement installed" — never a file that is
  neither.
- **Machines were made to talk to each other, and one job was proven end to end
  (#48–#69).** A network protocol, worker registration, a live job stream,
  scheduling, capacity accounting, and retry handling. On 6 August a real file
  went the full distance for the first time.
- **Automated testing was restored after seven weeks of silence (#53).** See the
  June summary for how it broke. Repairing it immediately exposed a second
  defect that had made the project buildable on exactly one computer.
- **Fourteen real defects were found and fixed**, most of them in code that had
  passing tests at the time. Several would have destroyed media files. These are
  listed individually below, because they are the clearest justification for
  what this month cost.
- **The backlog was made durable (#71).** Outstanding work had been living in a
  handoff document nobody re-read and an untracked scratch file. It is now 22
  tracked tasks, each carrying what "done" means and the evidence behind it.
- Dependency and tooling maintenance (#72, #73) closed the month, including
  retiring a two-month-old automated update that could no longer be applied.

**Highest-risk items this month** — the ones a stakeholder most needs to know
about, because each one either destroyed data, could have destroyed data, or
meant the system silently was not working at all:

- **#53 — nothing had been tested for seven weeks, and the project built on
  only one machine.** Automated testing had been dead since 13 June (see the
  June summary). Repairing it revealed that the database schema file the code
  depends on **had never been committed** — a rule in the owner's machine-wide
  ignore settings kept it out of every commit. A fresh copy of the project could
  not compile. Every "verified" claim made before 4 August had been verified on
  one laptop.
- **#14 — the code linter had never run.** A single unrecognised option makes
  the tool reject its entire configuration and quietly do nothing, which is
  indistinguishable from "no problems found" if you only check that the command
  succeeded. The project had no lint coverage at all until this was fixed.
- **#67 — the fleet could not dispatch any work whatsoever.** When a worker
  reported its capabilities across the network, the list of container formats it
  could write was dropped in translation and arrived empty. Every job requires
  one. Workers would connect, report healthy, and receive nothing, forever.
  **509 tests were passing at the time** — the one end-to-end test built its job
  with no requirements at all and so never exercised the matching.
- **#66 — a file-replacement step destroyed the original it exists to
  preserve.** The instruction telling a worker where to move the original file
  for safekeeping handed back the *destination* path instead. The worker renamed
  the original onto itself — a silent no-op — and then installed the new file
  over it. The unit test asserted the wrong path and passed.
- **#66 — two files with the same name overwrote each other in storage.** Two
  television shows each having an `S01E01.mkv` is the ordinary case, not an edge
  case; the retained originals collided.
- **#41 — the guard against truncated output silently stopped applying.** Output
  is checked by comparing the true end of the media stream, because a truncated
  file usually still claims the correct length in its header. The measurement
  used a technique that returns nothing at all on a long file, so it silently
  fell back to the very header value it was designed to distrust. Short test
  files passed either way.
- **#62 — the crash-recovery journal could never be recovered.** It was written
  into a directory named after the current process, so a restarted process — the
  exact and only circumstance the journal exists for — created a fresh empty
  directory and found nothing. The careful write-and-flush discipline around it
  was intact and unreachable.
- **#62 — the identity used to fence off a restarted worker was the machine's,
  not the process's.** A restarted worker resumed its previous identity and was
  never fenced, meaning a stale worker could still act. On the developer's Mac
  the same code fenced correctly; it failed only on the machines that matter.
- **#37 / #66 — every dispatched encode failed its final check.** The source's
  length and the output's length were measured two different ways, so they never
  agreed. Nothing could pass validation.
- **#13 — a "dry run" changed the filesystem.** The mode whose entire purpose is
  to change nothing was creating directories. Unknown quality presets were also
  being silently ignored rather than rejected.
- **#40 — the library scanner walked its own working and trash directories**,
  meaning it would rediscover files it had just set aside and process them
  again.
- **#69 — five separate faults in the job loop, none reachable by the
  single-job proof.** Requeued jobs were stranded permanently in a state the
  loop never looked at; nothing enforced a retry limit, so a failing job could
  occupy a worker slot forever; a rejected encode was killed outright rather
  than retried; the recovery process escalated every job instead of retrying it;
  and the scheduler was never consulted, so pausing the fleet did nothing at all.
- **#73 — the test suite failed at random, and the safety check it kept
  tripping had never been tested.** A storage-speed check ran on every database
  open with a fixed time limit, so on a busy machine a different unrelated test
  failed each run. Fixing it revealed the check itself had **no test anywhere** —
  the random failures were the only thing exercising it. The same flaw existed
  in a second copy of the check, which the restored automated testing caught
  within minutes.
- **#22 — the pre-flight check could not tell "not permitted" from "not
  possible".** A permissions error on a storage mount was being reported as the
  storage being incapable of safe file replacement, which would have silently
  disqualified working hardware.

**Verification note.** This month drew a hard line between two kinds of
evidence, and the distinction is worth a stakeholder's attention. The full test
suite (**517 tests**) passing is evidence about the tests. Separately, the
system was run against the real library — scanned, dispatched, encoded,
installed — and **the majority of the defects above were found by running it,
not by testing it**. Where a claim in this document rests only on tests, it says
so. Three items remain explicitly unverified and are listed under *What did not
get done*.

## What changed, in plain terms

### 1. The design, and the order it was built in

**What it was:** The month began with a committed architecture — a 2,690-line
specification, a naming contract fixing every database table, message and job
state, and an eight-phase delivery plan (#11). The project was also renamed
from `transcoderr` to `transcodarr` to match the naming convention of the
software family it sits alongside (#12).

**Why it mattered:** The sequencing was the point, not the paperwork. The plan
deliberately retires irreversible risks — the operations that can destroy a
media file — *before* building the distributed machinery that would run them
across many machines at once. A partially-built dispatcher that hands out work
it cannot account for is more dangerous than no dispatcher.

**The fix:** Phases were executed in order, bottom-up: a pure core with no
input or output, then storage, then a single machine doing real work with the
full safety ritual, then the network protocol, then dispatch across machines.

### 2. One file became six components

**What it was:** The project started as a 508-line command-line script. It is
now six separate components with enforced boundaries (#16–#20): a decision-making
core that is deliberately incapable of touching disk, network or database; a
storage layer; a wire protocol; a coordinating server; a worker agent; and the
command-line interface.

**Why it mattered:** Two of those boundaries are load-bearing rather than
cosmetic. The core being unable to perform input or output means every decision
it makes can be tested against invented data with no media files, no network and
no database — which is why the bulk of the rules governing what to do with a
file are exhaustively tested. And the worker agent is barred from depending on
the database layer, because it must remain copyable to a Windows machine without
dragging a database engine along with it.

**The fix:** The split landed across five pull requests, with the decision rules
built out as pure logic: what a file is, what should be done with it, what a
machine is capable of, and how a failure should be classified.

### 3. The media library, catalogued

**What it was:** Before this month nothing had ever inventoried the libraries.
Three libraries — television, anime and films — were discovered, then every file
analysed to determine what work it needs (#25–#34).

**Why it mattered:** This is the input to everything else. It also produced the
first real measurements the project has:

| Library | Files | Size | Queued jobs |
| --- | --- | --- | --- |
| television | 29,343 | 29,499 GiB | 15,388 |
| anime | 17,825 | 16,083 GiB | 6,916 |
| films | 2,432 | 37,233 GiB | 803 |
| **total** | **49,600** | **82,814 GiB** | **23,107** |

Discovery of all 49,600 files took 43 seconds. Analysis, which must read inside
every file, took roughly seven hours.

**The fix:** A scanner, an analyser and a decision engine, plus operator
commands to inspect any individual file's verdict and the reasoning behind it.
One measurement corrected a wrong assumption in the code: the analysis had been
limited to 4 files at a time on the theory that disk-seek-bound work does not
parallelise. Measured on the real storage, throughput went from 0.35 files per
second at 8-way to **2.0 at 32-way** — a 5.7× improvement — because
latency-bound work is precisely the case where a deep queue helps.

### 4. Crash-safe file replacement

**What it was:** The single most dangerous operation in the system: replacing a
real media file with a re-encoded one. A nine-step sequence with a written
journal, so an interrupted replacement can be reasoned about afterwards
(#35–#43).

**Why it mattered:** A crash, a power cut, or a killed process partway through
replacing a file must never leave the user with neither the original nor a
complete replacement. This is the risk the whole delivery order was arranged
around.

**The fix:** A fault-injecting test harness kills the process at each of the
nine steps against every reachable on-disk state, and asserts the outcome is
always recoverable to "source intact" or "replacement installed". An ambiguous
result is escalated for human attention rather than guessed at.

Crucially, **the harness was itself verified to be capable of failing**:
deliberately sabotaging the restore step so it claims success without doing
anything makes the harness report a violation. A safety check that cannot fail
is decoration, and this project has now found several that could not.

An architectural question was also settled here with measurement rather than
opinion: worker scratch space is required to sit on the same storage volume as
the destination. Putting it elsewhere moves the same bytes anyway, and trades an
instantaneous, atomic final step for a slow copy with a window in which neither
the original nor a complete replacement exists.

### 5. Machines talking to each other

**What it was:** The distributed half — a network protocol with an explicit
version gate, worker registration, a persistent two-way job stream, capacity
accounting, scheduling, retry handling and crash recovery (#44–#69).

**Why it mattered:** This is what makes it a fleet rather than a script. It is
also where the subtle failures live, because a distributed system fails by doing
nothing rather than by crashing.

**The fix:** Several rules here were settled deliberately and are worth
recording:

- **Permission is never inferred from silence.** No reply, a refusal, a dropped
  connection and a timeout are all treated identically: nothing is installed and
  the original is untouched.
- **The record of intent is written before the work is handed out**, which is
  what makes two workers targeting the same destination file impossible.
- **Capacity is recalculated from the database every cycle rather than tracked
  incrementally.** Incremental accounting is a second source of truth about who
  holds what, and its failure mode is silent: one missed release leaks a worker
  slot permanently, and the fleet quietly runs below capacity with nothing in
  any log.

On 6 August a real file completed the full journey for the first time: scanned,
analysed, dispatched to a worker, re-encoded, validated, installed, original
retained.

### 6. The recurring defect this project keeps finding

**What it was:** Not a single bug but a pattern, and the most valuable finding
of the month. **Five separate times**, a component was found fully built, fully
tested, and with nothing anywhere in the system actually calling it.

Every single one was a real defect: the crash-recovery routine, the container
format list at the network boundary, the retry-limit logic, the scheduler, and —
by a slightly different route — the storage-speed check that had no test.

**Why it mattered:** Each looked finished. Each had passing tests. In every case
the tests exercised the component directly while nothing in the running system
ever reached it, so the feature simply did not exist in practice. Pausing the
fleet did nothing. Retry limits were not enforced. Crash recovery never ran.

**The fix:** Beyond fixing each instance, this is now written into the project's
working rules: *when something exists and nothing calls it, that is the defect,
not a loose end.* A sweep for remaining instances is queued as tracked work.

### 7. Testing, restored and then made honest

**What it was:** Automated testing was repaired (#53) after seven weeks dead —
see the June summary for the cause. Late in the month the test suite itself was
found to be lying in two directions (#73).

**Why it mattered:** The suite had begun failing at random on the developer's
machine: a different unrelated test each run, all tripping a storage-speed check
that runs on every database open and compares against a fixed time limit. A
suite that fails randomly trains everyone to re-run rather than read, which is
how a real failure gets ignored.

**The fix:** The check is a production safety guard and was not weakened — the
production path still enforces it. Test databases now skip it, consistent with
24 places in the codebase that already did. More importantly, fixing it exposed
that **the guard had no test at all**; the random failures were the only thing
exercising it. It now has two, built by making the limit adjustable — no disk
can be made to fsync reliably *slower* than a fixed limit, which is exactly what
made the old failures random, but every disk is slower than zero.

Restored automated testing then immediately earned its keep: it caught the same
flaw in a second copy of the check, where a test was asserting that *the machine
running it had a fast disk* — a claim about hardware, not about the code. A
loaded build machine measured 199.87 ms and the check correctly reported a
problem; the test called that a bug.

Both fixes were verified by sabotage — deliberately breaking the thing each test
watches and confirming the test goes red.

### 8. The backlog made durable

**What it was:** Outstanding work had been living in three places that disagreed
with each other: a long handoff document, an untracked scratch file, and a
to-do list still describing the single-file tool the project had outgrown (#71).

**Why it mattered:** A task recorded in a document nobody re-reads is remembered,
not tracked. Three disagreeing sources is worse than one, because each looks
authoritative.

**The fix:** 22 tasks were written as individual files that a scheduled job
folds into a single list — a structure that lets parallel work add tasks without
colliding. Each carries what "done" means and the evidence behind it. Writing
them surfaced three defects in the tooling that does the folding, including one
where the changelog assembler produced output that failed the project's own
documentation checks. That assembler runs only at release time and **had never
been executed**, so the first release would have quietly published a broken file.

## What did not get done

Stated plainly so nobody assumes otherwise:

- **The Phase 4 scale milestone has not been run.** One job has been proven end
  to end; the load test and the 24-simultaneous-job hardware run have not
  happened. The system has 23,107 jobs queued and has completed one.
- **The GPU machine has never been checked.** The pre-flight suite has passed on
  two of three machines. The Windows GPU machine has not been tested, and the
  project's own design document is explicit that if it fails the file-replacement
  check, the architecture must change *at that point*, not later.
- **11.9 TB of the library is currently invisible to the system.** 2,271 files
  failed analysis and landed in a state the dispatcher does not look at, rather
  than being explicitly refused with a reason. The project's founding critique of
  the software it replaces is precisely this behaviour. This is the largest
  outstanding defect and is queued as tracked work.
- **Phases 5 through 7 are unstarted** — GPU support, monitoring and a user
  interface, and final hardening.
- **No performance figure in this document comes from a monitoring system.**
  Metric names exist but nothing exports them; every measurement here was taken
  by hand or in-process.

## Cost and effort notes

**Volume.** 63 merged pull requests, 325 files, +37,317/−1,740 lines, 64
commits, across eight calendar days. Five of eight planned phases are
code-complete. The test suite grew from nothing to 517 tests.

**Defects caught before production.** Fourteen are itemised above. At least four
would have destroyed media files: the replacement step that overwrote the
original it was preserving, the name collision in retained originals, the
truncation guard that stopped applying, and the unreachable crash-recovery
journal. Each was found before the system ran at any scale — which is the
specific return on having sequenced the work to retire irreversible risks first.

**Work that had to be redone.** June's automated-testing outage meant every
claim made before 4 August had been verified on a single machine and had to be
re-established once testing worked. A two-month-old automated dependency update
could no longer be applied to the restructured codebase and was redone by hand.

**Time lost to external causes.** A GitHub Actions outage on 7–8 August lasting
roughly six hours prevented automated checks from running at all — webhook
delivery was throttled to approximately 15% and events were being dropped
entirely, so one pull request received no checks whatsoever and had to be
verified by other means. This is noted because it appears in the timeline as
idle hours that were not idle work.

**The most reusable thing produced this month** is not code. It is a working
rule the project now applies by default: **a passing test suite is evidence
about the tests, not about the system.** Of the fourteen defects above, the
majority were found by running the software against the real library and reading
the logs — while the test suite was green. Two more were found by asking what
calls a given piece of code, and getting the answer "nothing".
