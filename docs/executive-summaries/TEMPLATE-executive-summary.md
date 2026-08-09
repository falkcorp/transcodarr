<!-- file: docs/executive-summaries/TEMPLATE-executive-summary.md -->
<!-- version: 1.0.0 -->
<!-- guid: 621ee658-156a-41e2-a502-8971466b89f9 -->
<!-- last-edited: 2026-08-09 -->

# Executive summary template

Copy this file to
`docs/executive-summaries/<YYYY-MM-DD>-<slug>-executive-summary.md` and replace
everything below the rules. The date prefix is the period covered (for a
roundup) or the date of the change (for a single item); `last-edited` in the
header is when the document was actually written, which is often later.

Format adopted from `audiobook-organizer/docs/executive-summaries/`, which uses
two shapes. Pick the one that fits:

- **Roundup** — a month or a work period. Grouped by theme, not by pull
  request. This is the shape written out below.
- **Single change / incident** — one bug or one feature. Narrative prose under
  descriptive headings, ending in a "what it does now" paragraph. Use when
  there is one story to tell and grouping would obscure it.

## Rules that make these worth reading

1. **Write for someone who does not work on the code.** These justify cost and
   effort to a reader who will not read a diff. Every technical term gets a
   plain-language gloss in parentheses the first time it appears — "a race
   condition (two operations touching the same data at once)". If a sentence
   only parses for someone who already knows the codebase, rewrite it.
2. **Lead with consequence, not activity.** "The queue silently stopped
   dispatching work" beats "refactored the conversion boundary". The reader is
   buying outcomes, not commits.
3. **Cite evidence inline.** Pull request numbers, measured figures, file
   counts. A claim with a number behind it is checkable; one without is a
   feeling.
4. **Be honest about what went wrong, including self-inflicted damage.** A
   summary that only lists wins is marketing and gets read as such. The most
   valuable entries are usually the defects found, especially the ones this
   project caused itself — those are what justify the cost of the work that
   found them.
5. **Say what was actually verified, and how.** Distinguish "tests pass" from
   "run against real data" from "not yet verified". Never let a reader assume a
   stronger check than the one performed.
6. **Do not pad.** If a month was quiet, say it was quiet and say why. A short
   honest summary is more credible than a long one padded to look busy.

## The template

Everything inside the block below is the document to copy. Replace the
angle-bracketed placeholders, and delete any section that genuinely does not
apply rather than leaving it empty.

````markdown
# Executive Summary: <Period or Change Title>

**Shipped:** PRs [`#A–#B`](https://github.com/<org>/<repo>/pulls?q=is%3Apr+is%3Amerged+merged%3A<start>..<end>),
covering `<start>` through `<end>` (`<N>` merged pull requests; `<X>` files
changed, `+<ins>/−<del>` lines)
**Prepared:** `<date written>`
**Related docs:** `<links to deeper write-ups, or omit>`

`<One or two sentences framing the document: what period it covers, and whether
it is grouped by theme or told as a single story.>`

## Executive Summary

`<Bulleted themes. Each bullet opens with a bolded phrase naming the theme, then
explains in plain language what changed and why a non-engineer should care. Aim
for one bullet per coherent arc of work, not one per pull request. Name the most
important PR numbers inline as evidence.>`

- **`<Theme>`.** `<What it was, why it mattered, what it now does.>`

**Highest-risk items this period** — the ones a stakeholder most needs to know
about, because each one `<touched safety / could have destroyed data / went
undetected for weeks>`:

- **`#<PR>`** — `<the defect in one sentence, in plain language, with its
  consequence stated>`

**Verification note:** `<exactly what was checked and how — tests, CI, a real
run against production data — and explicitly what was NOT checked.>`

## What changed, in plain terms

### 1. `<Theme name>`

**What it was:** `<the situation before, in plain language>`

**Why it mattered:** `<the consequence to the user, the data, or the cost of the
work — not the technical consequence>`

**The fix:** `<what was done, with PR numbers, and the measured result if there
is one>`

### 2. `<Next theme>`

<…same three-part shape…>

## What did not get done

`<Scope that was deliberately deferred, blocked, or abandoned, and why. Naming
this is what stops the next reader assuming it was finished. Omit the section
only if genuinely nothing was deferred.>`

## Cost and effort notes

<Optional. Anything that helps justify spend: volume delivered, defects caught
before they reached production, work that had to be redone and why, or time lost
to external causes such as an upstream outage.>
````

## Worked examples in this repository

- [June 2026 roundup](2026-06-30-june-monthly-roundup-executive-summary.md) —
  a deliberately short one. Three pull requests, one of which caused seven
  weeks of damage. Shows how to write a quiet month honestly.
- [August 2026 roundup](2026-08-09-august-monthly-roundup-executive-summary.md)
  — the full shape. 63 pull requests grouped into eight themes, with a
  fourteen-item highest-risk list and an explicit *what did not get done*.
