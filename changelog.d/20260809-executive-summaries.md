<!-- file: changelog.d/20260809-executive-summaries.md -->
<!-- version: 1.0.0 -->
<!-- guid: 5695b0ab-1374-4da1-bb71-68a515793ee9 -->
<!-- last-edited: 2026-08-09 -->

### Added

#### `docs/executive-summaries/`, with a template and two monthly roundups

Stakeholder-facing write-ups that justify what the work cost, in language that
does not assume the reader will open a diff. The format is adopted from
`audiobook-organizer/docs/executive-summaries/`, which had no template file —
one is written here, carrying the six rules that make the format worth reading
rather than only its section headings. The two most load-bearing: lead with
consequence rather than activity, and be honest about self-inflicted damage,
because a summary that only lists wins reads as marketing.

**August 2026** — 63 merged pull requests (#11–#73), 325 files, +37,317/−1,740
across eight days. Five of eight phases code-complete, one file carried end to
end through the real pipeline, 49,600 files catalogued and 23,107 jobs queued.
Fourteen defects itemised, at least four of which would have destroyed media,
and an explicit *what did not get done* naming the unrun scale milestone, the
never-tested GPU machine, and the 11.9 TB currently invisible to the dispatcher.

**June 2026** — three merged pull requests, one day, and a net *reduction* in
line count. Short because the month was.

#### June's cost, established rather than assumed

Writing the June summary settled where the seven-week CI outage came from, by
reading the workflow at the commits either side of the change rather than
repeating what the handoff says.

`fetch-depth: 0` had been the only key under its `with:` block, and was valid.
PRs #5/#6 added `submodules: recursive` one indentation level out from it, and
two sibling keys at mismatched indentation is a YAML syntax error. That is the
break. From 2026-06-13 until PR #53 on 2026-08-04 every run died in under a
second at the parse stage, and the outage went on to conceal the uncommitted
migration file that made the project buildable on exactly one machine.

This **corrects the handoff**, which states the file "contained a YAML syntax
error from the day it was written". It did not — it was valid for months, and a
one-line, correct-in-intent change broke it. The distinction carries the whole
lesson: the failure mode is not "nobody ever set CI up", it is "a small good
change broke a working thing and nobody checked that jobs still ran". Folded
into `TODO-HANDOFF` rather than fixed in place, since that task is already
rewriting the surrounding section.
