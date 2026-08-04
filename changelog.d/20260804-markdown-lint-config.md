### Fixed

#### Make the documentation lint pass, and say why where it does not apply

The markdown lint had never run — it lived in the CI workflow that failed to
parse — so its first real execution reported 1,067 findings nobody had had the
chance to see. Twenty-six were genuine and are fixed: bare code fences that
declared no language, a duplicated `Conventions` heading, a code span with a
trailing space inside it, lists and fences missing their surrounding blank
lines, two bare URLs, and a malformed nested code span in
`synthesis-decisions.md`.

The rest were rules that conflict with a documented convention of this
repository, and `.markdownlint-cli2.jsonc` now disables each with its reason
rather than leaving a permanently red check:

- **MD041** (first line must be a heading) cannot hold when every file is
  required to open with the four-line `file`/`version`/`guid`/`last-edited`
  header. The standard wins.
- **MD013** (line length) would reformat the schema appendix's tables and DDL
  into unreadability. The prose is already written to 80 columns by hand.
- **MD060** (table column padding) is churn that obscures the real diff across a
  21-table appendix.
- **MD036** (emphasis as heading) fires on the `**Milestone.**` sentence
  openers the design documents use throughout.
- **MD029** (ordered list prefix) wants the list under *Phases 5-7* renumbered
  `1, 2, 3`. Those numerals are the phase numbers; renumbering them would make
  the document state that phases 1 through 3 remain outstanding, which is
  false.

The `.standards/` submodule is excluded, since it is a checkout of another
repository and is linted there.
