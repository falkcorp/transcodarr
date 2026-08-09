<!-- file: docs/executive-summaries/README.md -->
<!-- version: 1.0.0 -->
<!-- guid: 2fe1fd2d-1291-456e-9e9e-d9a90dd28cab -->
<!-- last-edited: 2026-08-09 -->

# Executive summaries

Stakeholder-facing write-ups that explain completed work to someone who
**decides about it but does not read code** — what it cost, what it bought, and
what is still open.

## The template lives in the standards repository

**[`.standards/templates/executive-summary.md`](../../.standards/templates/executive-summary.md)**

Do not copy it here. That file is the org-wide template, shared by every
repository carrying the `.standards` submodule, and a local copy would drift
from it silently — which is the exact problem the submodule exists to prevent.
This repository briefly had its own copy; it was retired in favour of the
shared one on 2026-08-09, after an identical template was found to have been
written independently in another repository on the same day.

If the template needs changing, change it there. The fix reaches every
repository at once.

## Summaries in this repository

| Period | Document |
| --- | --- |
| June 2026 | [June roundup](2026-06-30-june-monthly-roundup-executive-summary.md) |
| July 2026 | [July roundup](2026-07-31-july-monthly-roundup-executive-summary.md) |
| August 2026 | [August roundup](2026-08-09-august-monthly-roundup-executive-summary.md) |

These three are written to be read as a set, and say so. June looks like nothing
happened, July looks quiet, and August looks enormous — but June's one-line
mistake created much of August's cost, and July's design is what let August move
at all. A pull-request count is a poor proxy for a period's value in either
direction.

## Adding one

Follow the naming and update conventions in the shared template. In particular:
when work continues on a subject that already has a summary, **edit that file
rather than adding a second one** — bump `version:`, refresh `last-edited:`,
extend the `**Shipped:**` range, and leave the filename alone.
