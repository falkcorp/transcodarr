<!-- file: changelog.d/20260806-fix-docs-lint.md -->
<!-- version: 1.0.0 -->
<!-- guid: a4e270c9-3b18-4d95-8f26-0517b93ce482 -->
<!-- last-edited: 2026-08-06 -->

### Fixed

#### The handoff broke the documentation lint

A paragraph wrapped so that `#67).` began a line, and markdownlint reads a line
starting with `#` as a heading (MD018). It was merged with the check red — the
wait-for-CI loop read a moment when no check was listed as pending and treated
that as everything having passed.

Reflowed so the reference never lands at the start of a line.
