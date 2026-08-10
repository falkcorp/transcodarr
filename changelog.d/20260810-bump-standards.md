<!-- file: changelog.d/20260810-bump-standards.md -->
<!-- version: 1.0.0 -->
<!-- guid: e7dc653d-4f25-447b-a464-f9007cb050b7 -->
<!-- last-edited: 2026-08-10 -->

### Changed

#### `.standards` advanced to pick up CI, the shared template, and its own headers

Three changes landed upstream in `falkcorp/.github` and this bumps the submodule
pointer to all of them:

- **falkcorp/.github#1** — the org-wide executive summary template, merged from
  two that had been written independently on the same day in this repository and
  in `ubuntu-autoinstall-agent`.
- **falkcorp/.github#2** — a Super Linter workflow with autofix. That repository
  publishes coding standards to 45 others and had **no CI at all**; its first
  run found five violations in the instruction documents themselves, including a
  fenced code block with no language in `instructions/commit-messages.md`, the
  file that tells everyone else how to write one.
- **falkcorp/.github#3** — the four-line file header, applied to that
  repository's own eleven files. `instructions/file-headers.md` calls the header
  mandatory and did not have one.

Nothing in this repository changes behaviourally; `.standards` is documentation
and templates. The pointer moves so a fresh clone here gets the corrected
instruction files rather than the versions with the lint violations in them.
