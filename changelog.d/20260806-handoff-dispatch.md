<!-- file: changelog.d/20260806-handoff-dispatch.md -->
<!-- version: 1.0.0 -->
<!-- guid: 3e81f45c-7a29-4b60-98d1-2c6b054ea937 -->
<!-- last-edited: 2026-08-06 -->

### Changed

#### Handoff: Phase 4 works end to end

The phase table said no `JobAssignment` was ever sent. One now goes out, is
encoded, and is installed — proven on a real library, not only in tests. The
Phase 4 section, the crate table, the PR list and the next-session instruction
all move; the next action is the milestone, not more code.

Records the four defects the dispatch work uncovered, three of them found by
running the system rather than testing it — including muxers being dropped at
the conversion boundary, which made dispatch impossible while 509 tests stayed
green. The lesson is written down with them: a green suite is evidence about the
tests, not about the system.

Also flags that `ScheduleEngine` is built, tested, and still not consulted by
the dispatch loop, so schedule windows and pause overrides currently have no
effect.
