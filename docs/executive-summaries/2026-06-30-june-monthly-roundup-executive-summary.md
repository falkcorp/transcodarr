<!-- file: docs/executive-summaries/2026-06-30-june-monthly-roundup-executive-summary.md -->
<!-- version: 1.0.0 -->
<!-- guid: 4e2477ad-dffc-4618-a4ee-ee07a788729e -->
<!-- last-edited: 2026-08-09 -->

# Executive Summary: June 2026 Monthly Roundup

**Shipped:** PRs [#4–#6](https://github.com/falkcorp/transcodarr/pulls?q=is%3Apr+is%3Amerged+merged%3A2026-06-01..2026-06-30),
all merged 2026-06-13 (3 merged pull requests; 13 files changed, +99/−255
lines). One further pull request, #7, was opened 2026-06-16 and never merged.
**Prepared:** 2026-08-09, retrospectively.
**Related doc:** [2026-08-09-august-monthly-roundup-executive-summary.md](2026-08-09-august-monthly-roundup-executive-summary.md)
— the month in which this project was actually built, and in which June's one
lasting side effect was found and repaired.

June was a quiet month on this repository, and this summary is short because
the month was. No product code was written. The work was repository
housekeeping done in a single sitting on 13 June: adopting the shared coding
standards used across the owner's projects, and pointing the automated build at
them.

That is the whole of it — **except that one line of that housekeeping silently
switched off all automated testing for roughly seven and a half weeks**, which
is the part of June that actually cost something, and the reason this
retrospective is worth writing at all.

## Executive Summary

- **Shared coding standards adopted (#4).** The project was wired to
  `falkcorp/.github` — a shared repository of coding standards used across the
  owner's projects — as a git submodule (a way of embedding one repository
  inside another so it stays in sync). Instructions previously scattered across
  the repository were centralized into three files: `CLAUDE.md`, `AGENTS.md`,
  and `.github/copilot-instructions.md`. Net effect on the codebase was a
  reduction: 99 lines added, 255 removed.
- **Automated build taught to fetch those standards (#5, #6).** The build
  pipeline checks out the code before testing it, and by default that checkout
  skips submodules — so the standards would have been absent every time. Both
  pull requests added the setting to fetch them.
- **A dependency update proposed and never taken (#7).** An automated bot opened
  a routine version bump of a benchmarking library on 16 June. It sat untouched
  for nearly two months and was eventually closed unmerged on 2026-08-08,
  because by then the project had been restructured and the change no longer
  applied anywhere. Its substance was redone by hand the same day.

**Highest-risk item this period** — there is exactly one, and it is
self-inflicted:

- **#5 / #6 — the change that switched off all automated testing.** Adding the
  submodule setting placed it at a different indentation level than the setting
  already beneath it. In the configuration language these files use (YAML,
  where indentation carries meaning the way brackets do in other languages),
  two sibling settings at mismatched indentation is a syntax error. The result
  was not a visible failure: **every automated run from that day forward died
  in under a second at the parse stage, before a single test could run**, and
  reported only "this run likely failed because of a workflow file issue".
  Nothing turned red in a way anyone noticed. It was not found until
  **2026-08-04**, roughly seven and a half weeks later, and repaired in PR #53.

**Verification note:** the causation above was verified directly for this
document rather than assumed, by reading the file at the commit before and
after the June change. Before it, the affected block held a single setting and
was valid. The June commit added a second setting one indentation level out
from the first, which is the error. No other June change had any lasting
effect.

**A correction this establishes.** The project's own handoff document states
that this configuration file "contained a YAML syntax error from the day it was
written". That is not what the history shows — the file was valid until 13
June, and this change is what broke it. The distinction matters, because the
handoff's version implies the build had never worked and no one had ever
noticed, while the truth is that a specific, small, well-intentioned change
broke a working thing and nobody checked. The second version is the one with a
lesson in it.

## What changed, in plain terms

### 1. Shared coding standards, centralized

**What it was:** The owner maintains several projects that share conventions —
how files are versioned and headed, how commit messages are formatted, language
style rules. Before June, this project carried its own scattered copies of those
instructions, which drift from the originals as soon as either side changes.

**Why it mattered:** Divergent copies of a standard are worse than no standard,
because each copy looks authoritative. It also matters specifically for
automated contributors: this project is built largely by AI agents working from
written instructions, and an agent reading a stale local copy will confidently
do the wrong thing.

**The fix:** The shared standards repository was embedded as a submodule at
`.standards/`, and local instruction files were reduced to pointers at it
(#4). The net line count went *down* by 156 lines — the change removed more
duplicated instruction than it added.

### 2. The automated build, pointed at those standards — and broken doing it

**What it was:** Embedding a repository as a submodule does not make it appear
automatically. The automated build checks out the project's code before running
tests, and that checkout ignores submodules unless told otherwise. Two pull
requests (#5, #6) added the instruction to include them.

**Why it mattered:** This is the entire cost of June, and it is worth stating
plainly for a reader deciding whether this work was worth paying for. The
change was correct in intent and one line long. It was also placed one
indentation level away from where it needed to be, and in this file format that
is a fatal error rather than a cosmetic one. From 13 June until 4 August, the
project had **no automated testing of any kind** — no compilation check, no
test run, no lint — while appearing to have it.

Two things made that expensive rather than merely embarrassing:

1. **A broken build file looks almost identical to no build file.** There is no
   red mark against a check that never became a job. Anyone glancing at the
   project saw an absence, not a failure.
2. **It concealed a second, independent defect for the same seven weeks.** The
   database schema file the code depends on had never actually been committed
   to the repository — a blanket rule in the owner's personal, machine-wide
   ignore settings matched it and quietly kept it out of every commit. The
   project therefore compiled on exactly one laptop and nowhere else on Earth.
   Working automated testing would have caught this within minutes of the first
   push, because a fresh checkout could not build. Instead it went unnoticed
   through three major phases of development.

**The fix:** Both were repaired together in August (PR #53, 2026-08-04): the
indentation was corrected so the build could parse and run for the first time,
and the repository's own ignore rules were amended to override the machine-wide
one so the schema file could never be silently dropped again. Full detail is in
the August roundup.

## What did not get done

Nothing was planned for June beyond the standards adoption, so nothing was
deferred. The project's design work and all of its implementation began the
following month.

## Cost and effort notes

June's direct effort was small — a single day's repository housekeeping,
13 files, and a net reduction in line count.

Its indirect cost was not small, and is the honest accounting a stakeholder
should see: a one-line mistake in that day's work removed the project's entire
safety net for **seven and a half weeks**, spanning the construction of three
major subsystems, and hid a second defect that would otherwise have been caught
within minutes. Neither the mistake nor its discovery cost much to fix — the
repair was part of a single pull request in August. What it cost was
**confidence in everything built during that window**: every "verified" claim
made between 13 June and 4 August was verified on one machine only, and had to
be treated as unproven until the build was working again and could confirm it
independently.

The lesson recorded in the project's handoff, and the reason this is worth
documenting rather than quietly fixing: **check that an automated run produced
jobs, not merely that nothing appears to have failed.** An absence of red is
not a green.
