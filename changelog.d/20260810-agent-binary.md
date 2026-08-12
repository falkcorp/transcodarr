<!-- file: changelog.d/20260810-agent-binary.md -->
<!-- version: 1.0.0 -->
<!-- guid: 5cacfea9-0b0d-40a8-b6ab-f07ea48b0aaa -->
<!-- last-edited: 2026-08-11 -->

### Added

#### An agent binary with no database in it

The rule was already stated and already checked: `transcodarr-agent` must never
depend on `transcodarr-store`, because it "has to stay copyable to the Windows
node without dragging SQLite along", verified with `cargo tree`.

**The crate honoured it. Every shipped artifact defeated it.** There was one
binary, `transcodarr`, and it links `transcodarr-server` -> store -> SQLite. So
the thing an operator would actually copy to a worker node contained the entire
orchestrator and a C library it will never open. The check passed because it
asked about the dependency graph, and the property that mattered was about the
binary.

`transcodarr-agent` is now its own binary: **0 sqlite crates against the CLI's
3**, 19M against 38M. `transcodarr` keeps its `agent` verbs unchanged.

#### The streaming wire contract

`FetchSource` (server to agent) and `PushOutput` (agent to server), with
`FileChunk` carrying an explicit `offset`, an explicit `last`, and a
`content_sig` on the final chunk. Offset is carried rather than implied so a
restarted stream is detectable instead of appending into a corrupt file of
exactly the right length.

**The server methods return `unimplemented` with a reason, and streaming does
not work yet.** That is deliberate: an empty stream or an accepted-but-ignored
push would look like success and produce a job that reports done having moved no
bytes, which is this project's most frequent failure shape.

### Fixed

#### `.gitignore` was silently swallowing the new binary

`bin/` matched `crates/transcodarr-agent/src/bin/`, where Cargo keeps *source*.
The file was invisible to `git status` and would not have been committed — a
fresh clone would have built a workspace with no agent in it while every local
check passed.

This is the second instance of exactly this defect. The first was
`0001_initial.sql`, kept out of every commit by a global `*.sql` rule, which
made `main` compile on one machine and nowhere else. The fix is the same
negation the repository already carries for migrations, two lines below it.
