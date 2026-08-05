<!-- file: docs/design/IMPLEMENTATION-HANDOFF.md -->
<!-- version: 3.7.0 -->
<!-- guid: 9d4a7c31-6b28-4e5f-8a03-2c7e1b9f04d6 -->
<!-- last-edited: 2026-08-04 -->

# Implementation handoff — transcodarr

Start here. This document is the entry point for building transcodarr out from
the committed design. It assumes no prior conversation context.

Read in this order:

1. This file (orientation, working agreement, traps).
2. `docs/design/distributed-architecture.md` — the specification. Section
   **Phased Delivery Plan** (Phase 0–7) is the execution roadmap.
3. `docs/design/synthesis-decisions.md` — the binding naming contract. SQL
   tables, Rust types, RPCs, metrics, job states. **Treat as authoritative.**

## The standing goal

**Get everything possible done that does not genuinely require the owner.**
Where a decision has a sane default, take it, state which default you took, and
keep going. Do not stop to ask what can be reasonably assumed.

Bring the same standard to what you find along the way:

- **When you find a flaw, fix it.** Do not file it, do not note it for later,
  and do not route around it. This applies to whatever you are standing on, not
  only to the thing you set out to build — the two defects that had silently
  broken this repository for weeks (see below) were both found while starting
  unrelated work.
- **When you find code or documentation that is out of date, correct it.** A
  stale document is worse than a missing one: it is believed. This file has
  twice claimed a phase was unstarted while its code was merged, and once
  claimed no orchestrator code existed at all after five subsystems had landed.
- **A check that cannot fail is decoration.** Before trusting one, break the
  thing it watches and confirm it goes red. That standard already caught a crash
  matrix that could not fail, a lint that had never run, and a repository lookup
  that could only ever return `None` — which made every negative test around it
  pass while proving nothing.

Stop and ask only when proceeding would be unsafe, irreversible, or would waste
substantial work if the assumption turned out wrong. Otherwise: assume, act,
and say clearly in the report what you assumed.

## Read this first: CI was not running

Until 2026-08-04 **no CI had ever executed in this repository.**
`.github/workflows/ci.yml` contained a YAML syntax error from the day it was
written — `fetch-depth: 0` indented deeper than the sibling above it — so every
run failed in zero seconds at the parse stage and reported only "this run likely
failed because of a workflow file issue". `fmt`, `clippy`, `test` and
`build --release` had never run on a runner through all of Phases 2, 3 and 4.

That hid a second defect. `crates/transcodarr-store/migrations/0001_initial.sql`
— the schema `db.rs` embeds with `include_str!` — **was never committed.** A
blanket `*.sql` in the maintainer's *global* gitignore matched it, so `main`
compiled on one machine and nowhere else:

```console
$ git clone --branch main <repo> /tmp/mainclone && cargo build -p transcodarr-store
error: couldn't read crates/transcodarr-store/src/../migrations/0001_initial.sql
```

Both are fixed (PR #53). The repository's own `.gitignore` now negates the
pattern, where it takes precedence over anyone's home directory. **Treat any
"verified" claim made before 2026-08-04 as verified on one laptop only** — the
green triple was real, but it was never independently reproduced.

Two lessons worth keeping:

1. **A workflow that fails to parse looks almost exactly like no workflow.**
   There is no red X on a file that never became a job. Check that a run
   produced *jobs*, not just that the branch looks green.
2. **`git status` will not tell you about a file a global ignore is hiding.**
   `git check-ignore -v <path>` will, and a clean clone built in a temp
   directory is the only real proof that a repository is self-contained.

## Phase status (updated 2026-08-04)

| Phase | State |
| --- | --- |
| **0 — Environment preflight** | **Done on U0 and U1**, both commit-eligible. `windows-rtx2070` not run — see `PHASE0-RESULTS.md`. Does not block Phase 2. |
| **1 — Workspace split and `transcodarr-core`** | **Complete.** All milestone criteria met with zero media, network or DB. |
| 2 — `transcodarr-store`, scanner, evaluator | **Code complete; milestone part-run.** Store, `Scanner`, `Prober`, `Evaluator`, `admin explain` and the operator commands all shipped. Discovery verified on all three real libraries (49,600 files in 43 s). Probe ingestion is long-running — see below. |
| 3 — Single-node executor and commit ritual | **Mostly done.** Ritual, journal, crash matrix, executor, validation and `admin run` shipped and proven on real media. `TrashCan` retention and `CommitIntentRepo` remain, plus the 200-file milestone. **D14 decided — see below.** |
| 4 — Protocol, one agent, dispatcher | **In progress.** The server side of the transport is done: codegen, `AgentRepo`, `Register` and the `Connect` stream, all covered over a real gRPC channel. **`ConnectClient` does not exist and no `JobAssignment` is ever sent** — there is no dispatch loop, so an agent connects, is accounted for, and sits idle. |
| 5 — GPU class, capability probing | Not started. |
| 6 — Observability, schedules, UI | Not started. |
| 7 — Hardening | Not started. |

`transcodarr-core` is finished: `paths`, `plan`, `preset`, `probe`, `validate`,
`capability`, `failure`, `facts`, `policy`. `transcodarr-agent` exists with
`preflight` only. `transcodarr-store` has `db` (schema, migrations, pragma
verification, durability probe) and `writer` (lanes, per-op `SAVEPOINT`, poison
tracking).

### Phase 2 — status

Shipped, all merged to `main` (PRs #28, #30, #31, #32, #33):

- Schema as one embedded `STRICT` migration, pragma verification, migration
  checksum refusal, durability probe.
- `Writer` with priority lanes, per-op `SAVEPOINT`, poison tracking.
- `ReadPool`, and `LibraryRepo`/`FileRepo`/`JobRepo`/`DispatchBlockRepo`.
- `transcodarr-server`: `Scanner`, `Prober`, `Evaluator`, `Explainer`,
  `summarize`, `Runtime`.
- CLI: `admin add-library`, `libraries`, `scan`, `evaluate`, `explain`,
  `summary`.

243 tests. `cargo fmt -- --check`, `cargo clippy --all-targets --all-features
-- -D warnings` and `cargo test` all green.

**Layering question, now settled.** `transcodarr-cli` does not link
`transcodarr-store`; it calls `transcodarr-server::Runtime`. No SQL, no
`rusqlite` type and no repository appears in the CLI.

**Seven repositories deliberately not written.** `AgentRepo`,
`CommitIntentRepo`, `TrashRepo`, `ScheduleRepo`, `ConfigRepo` and `PoolRepo`
have no Phase 2 caller. They arrive with the phases that call them rather than
shipping as untested surface.

### Phase 2 milestone — what has actually been run

On the server (`172.16.2.30`), against the real libraries, built in
`~/transcodarr-build`, database at `~/tc/tc.db`:

| Library | Files | Size | Discovery |
| --- | --- | --- | --- |
| tv | 29,343 | — | 28.7 s |
| anime | 17,825 | 16.1 TiB | 8.5 s |
| movies | 2,432 | 36.4 TiB | 1.9 s |
| **total** | **49,600** | | **43 s** |

Exactly the ~49.6k the architecture document predicted. The `min_mtime_age_s`
guard skipped 7 files that were being written at the time — the guard working
in production, not in a test.

**Probe ingestion is the long pole and was still running at handoff.** It is
detached under `setsid` on the server, logging to `~/tc/full-probe.log`, at
`--probe-concurrency 32`. Check it with:

```bash
ssh jdfalk@172.16.2.30 'cd ~/transcodarr-build && \
  ./target/release/transcodarr admin summary --db ~/tc/tc.db'
```

At the measured ~2 files/second it needs roughly 7 hours for all 49,600. When it
finishes, the milestone's remaining assertions are one command each:
`admin summary` for the decision/GiB breakdown, and `admin evaluate --force` for
the "re-derive every decision with zero filesystem I/O" claim.

**Measured probe concurrency, correcting a wrong assumption in the code.**
The first version defaulted to 4 on the reasoning that seek-bound work does not
parallelise. Measured on the production pool with Tdarr running alongside:

| concurrency | files/second |
| --- | --- |
| 8 | 0.35 |
| 32 | 2.0 |
| 96 | 2.18 (load average 35) |

Latency-bound is precisely the case where a deep queue helps — each probe waits
rather than works. The knee is near 32; 96 buys 9% for triple the load. The
default is now 16, with `--probe-concurrency` for a dedicated ingest run.

### Phase 3 — status

Shipped and merged (PRs #35, #36, #37):

- `WorkArea` (namespaced by `agent_uid`/`boot_id`, identifiers sanitised,
  cross-device refused), `IntentJournal`, `CommitRitual` and recovery.
- The crash matrix: every phase against every reachable on-disk state.
- `Executor`, `ProgressTailer`, output validation.
- `policy::encode_plan_for` / `validation_spec_for`.
- `admin run` — encode, validate, install on one machine.

**D14 is decided: colocate the work area on the destination pool.** A
cross-device work area is now a hard refusal. `rename(2)` is atomic only within
one filesystem, and the copy-then-delete fallback has a window in which neither
the source nor a complete replacement exists. Staging on fast local scratch
moves the same bytes anyway and buys a *non-atomic* install — the I/O is not
saved, only deferred to the least recoverable moment. This closes the only
ACCEPTED item of the 56 fatal-flaw resolutions.

**Two things worth knowing:**

1. **The crash matrix was verified to be able to fail.** Sabotaging the restore
   step so it claims success without restoring anything makes it report
   `INVARIANT VIOLATED`. A crash matrix that cannot fail is decoration.
2. **A real bug was found only by running against real media.** Validation
   compared the source's *container header* duration against the output's
   *last-packet PTS*. A packet's PTS is the last frame's presentation time, not
   the end of the file, so it sits one frame lower — inventing a shortfall.
   Measured: header 5.000s, source PTS 4.900s, output PTS 4.900s. Every unit
   test passed with the bug present, because they supplied both durations by
   hand.

End-to-end verified on real ffmpeg media: FLAC installed as EAC3 640k, original
FLAC retained in the trash, video copied untouched, durations identical.

### Phase 3 — real-media run, and the bug it found

A bounded run of 10 audio jobs was executed against the live `anime` library
with Tdarr still running (the owner chose this over stopping Tdarr). Track
preservation verified on an installed file:

| | video | audio | subtitles |
| --- | --- | --- | --- |
| original | h264, mjpeg | aac, flac | 2 |
| installed | h264, mjpeg | eac3, eac3 | 2 |

Both audio tracks converted, neither dropped — `-map 0` doing its job. Size grew
7.08 → 7.16 GB, which `SizePolicy::MayGrow` correctly permits for an audio pass.

**The run found a serious bug that no test caught.** `last_packet_pts_us` used
`-read_intervals -60`, which returns *nothing* on a long file — so it returned
`None`, callers fell back to the container header duration, and the
last-packet-PTS guarantee silently stopped applying. Fixed in PR #41 with an
absolute seek point. Note that the 10-file run above executed on the pre-fix
binary, so those validations compared header duration to header duration:
consistent, and therefore safe, but not the intended guard.

Reproducing it needs a long real file; short fixtures pass either way, and the
behaviour differs by platform.

### Still outstanding in Phase 3

1. The **200-file milestone** proper, with input/output `file_stream` rows
   diffed for byte-exact track preservation. Ten files ran; the diff harness
   does not exist yet.
2. Wire `CommitIntentRepo` into `LocalRunner`, so the server-side ledger is
   written alongside the agent's journal. The repository exists and is tested;
   nothing calls it yet.
3. `TrashRepo` likewise: retention is implemented and tested, but the runner
   does not yet record a `trash_entry` when the ritual retains an original.

### Phase 4 — in progress

**Landed.** `CapacityLedger` (all-or-nothing permits, release on leaving the
admitted set, rebuild from the database before the first dispatch, separate
large-file cap). `Dispatcher` with the two-stage bucket/admission split.
`Reconciler` on its 5s tick. `ScheduleEngine`. Retry policy, dead-lettering and
agent quarantine. Metric names as constants. The proto semantics — `VersionGate`
and the fencing rule — and, as of 2026-08-04, gRPC codegen and the conversion
boundary (PR #54).

Three things about the codegen are worth knowing before touching it:

- **`protoc` is not expected on `PATH`.** It comes from
  `protoc-bin-vendored`, handed to `prost_build::Config` directly rather than
  exported as `PROTOC`, because `std::env::set_var` is on this repository's
  clippy disallowed list. A fresh clone builds with nothing installed but a
  Rust toolchain — verified with `PATH=/usr/bin:/bin`.
- **`build_transport(false)` is load-bearing.** The generated client would
  otherwise carry an inherent `connect(dst)` constructor that collides with the
  client method generated for `rpc Connect` (`E0592`). Build a client with
  `AgentServiceClient::new(channel)`. Do not "fix" this by renaming the RPC —
  the schema is the reviewed agreement between both ends.
- **`buf` guards the contract but is not in the build path.** `buf lint` and a
  breaking-change check against `main` run in CI. The against-reference must
  carry `subdir=crates/transcodarr-proto/proto`, or buf names the file relative
  to a different module root on each side and reports every unchanged file as
  deleted.

**`AgentRepo` and `Register` have landed** (PRs #56, #57). No migration was
needed: `agent`, `agent_mount` and `agent_capability_history` were already in
`0001_initial.sql`, so nothing here touched the live database on the server.
Registration is served over a real gRPC channel and covered by ten tests that
dial a loopback server rather than calling in-process.

Three decisions in that work are worth not relitigating:

- **A rejection changes nothing in the database.** It is a clean response with a
  reason, not an error and not a partial write, or being refused becomes a way
  to overwrite a healthy row. There is a test asserting the row is untouched.
- **A reinstall takes a new epoch.** Same operator name, new `agent_uid`, so it
  cannot inherit a work area that is not its own.
- **`commit_eligible` requires every mount to have passed the rename probe**,
  not merely one. `RP_UNTESTED` grants nothing.

**`Connect` has landed** (PR #59), with `AgentTable`. What it enforces, and
what it deliberately does not, is worth knowing before extending it:

- The stream is identified by **request metadata** (`x-agent-id`,
  `x-agent-epoch`), not by a message. `AgentMessage` carries no identity, and a
  `Hello` field serving the transport's convenience does not belong in a
  reviewed wire contract.
- A stale epoch cannot open a stream or resolve a commit; a `CommitReport`
  bearing one is rejected with the intent untouched. There is a paired test
  asserting the same report under the current epoch *does* resolve it, so the
  negative case cannot pass by everything being broken.
- `AgentTable` keeps one connection per agent, newest wins, displaced stream
  closed. `disconnect` is epoch-guarded so a slow teardown cannot evict the
  replacement that just arrived.
- **No `JobAssignment` is ever sent.** That is the dispatch loop's job.

A bug worth remembering: `CommitIntentRepo::get` takes an *intent* id and the
session only ever knows a *job* id. Passing one to the other answers "no live
intent" for every job, which reads as a correct refusal until a legitimate
commit is refused too — and one test merged in #57 was green for exactly that
reason. `live_for_job` exists now. When a lookup can only ever return `None`,
every negative test passes and proves nothing.

**Remaining, in dependency order:**

1. **`ConnectClient`** with `ReconnectPolicy` — registers, replays its
   `IntentJournal`, runs assignments through the existing `Executor` and
   `CommitRitual`. It must keep its `boot_id` across reconnects, or every
   network blip fences work that is still running fine.
3. **`serve` and `agent connect`** CLI verbs, and the loop tying
   `ScheduleEngine` → `Dispatcher` → session outbound → `Reconciler`.
4. **The milestone**: the DI-1 maximal-matching table as a CI-checked artifact,
   a `FakeAgent` load test, then 24 concurrent audio jobs sustained on U1. The
   milestone does **not** require the GPU node — Phase 4 is audio-class only.

One caveat on that milestone. It asserts `transcodarr_dispatch_latency_seconds`
p99 ≤ 100 ms, but `metrics.rs` is names-only with no exporter until Phase 6, so
the measurement will have to be made in-process against the same clock. That is
not the artifact the milestone text describes. Say so in the pull request rather
than letting a later reader assume a Prometheus histogram exists.

Note that `transcodarr-agent` must not acquire a `transcodarr-store`
dependency: it has to stay copyable to the Windows node without dragging SQLite
along. Check with `cargo tree -p transcodarr-agent -i transcodarr-store` rather
than by eye — a shared proto crate makes a transitive dependency easy to add by
accident.

### Phases 5-7 — not started

5. GPU class, capability probing, emergent two-stage.
6. Observability, schedules, UI.
7. Hardening.

### Scope note

Phases 4-7 are each multi-session units — Phase 6 alone is a metrics subsystem,
a scheduler and a web UI. Attempting them in one pass produces half-built
subsystems, which is worse than none: a partial dispatcher that hands out work
it cannot account for is more dangerous than no dispatcher. Take them one phase
per session, meeting each documented Milestone before moving on.

### Still outstanding in Phase 2

1. **Finish the probe run**, then confirm the two milestone assertions above.
2. **`admin config validate --diff`** is specified for this phase but needs a
   configuration file format that does not exist yet — there is nothing to
   validate or diff. Building one now would mean guessing at the schedule and
   dispatch settings Phases 4-6 define. Do it when the config subsystem lands.
3. **8 anime files failed to probe** in the first pass. Worth looking at what
   they are before assuming the rate is normal.

## Current state

`falkcorp/transcodarr` (renamed from `transcoderr` on 2026-07-31; old URLs
redirect). The local clone is still at `~/repos/github.com/jdfalk/transcoderr`
— only the repo and crate were renamed.

`main` is green, and as of 2026-08-04 that is verified by CI rather than only on
one laptop: **464 tests passing**, `cargo fmt -- --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo build
--release`, `buf lint`, `buf breaking` against `main`, and markdownlint.

Six crates:

| Crate | State |
| --- | --- |
| `transcodarr-core` | Complete. Pure: no tokio, no rusqlite, no tonic. |
| `transcodarr-store` | Schema, writer, read pool, seven of eleven repositories. |
| `transcodarr-proto` | Wire contract, version gate, codegen, conversion boundary. |
| `transcodarr-server` | Scanner, prober, evaluator, dispatcher, reconciler, schedule engine, capacity ledger, hardening, and the agent session. |
| `transcodarr-agent` | Work area, journal, commit ritual, executor, preflight, capability trial. **No transport client.** |
| `transcodarr-cli` | `local` and `admin` verbs. **No `serve`, no `agent connect`.** |

The last two rows are the whole of what is left in Phase 4.

The last eight pull requests, most recent first:

| PR | What |
| --- | --- |
| #60 | Handoff: the `Connect` stream and the lookup bug it exposed |
| #59 | The `Connect` stream and `AgentTable`; the fence enforced over the wire |
| #57 | `AgentSession`: registration served over gRPC |
| #56 | `AgentRepo`; the fencing epoch survives a server restart |
| #55 | Handoff corrected — the phase table had said Phase 4 was not started |
| #54 | gRPC codegen, the conversion boundary, buf contract checks |
| #53 | **CI repaired, and the schema it could not see committed** |

## Locked decisions

Decided by the owner on 2026-08-01. Do not relitigate these; if you believe one
is wrong, say so and proceed under it unless told otherwise.

- **Where to work.** Phase 1 (pure core, no I/O) is built **on the Mac** — it
  needs no media and is fully testable against generated fixtures. Phase 0
  preflight and Phase 2 onward run **on the server** (`172.16.2.30`), which has
  the libraries, the ZFS pool, and network reach to the GPU node.
- **Autonomy.** Run each phase end-to-end: implement, meet the phase's
  documented **Milestone** criteria, open and merge the PR, report, continue.
  Stop only when a milestone genuinely fails or the architecture must change.
  Reaffirmed 2026-08-04 and widened — see § *The standing goal*: take sane
  defaults rather than asking, and fix flaws and stale documentation wherever
  they are found rather than only where the current task points.
- **Merging.** Standing permission to merge tested work without asking. The gate
  is *evidence* — CI green, a real test run, a verified build — not permission.
  Renaming or deleting a repo is not covered by this.
- **D14** (agent work area colocated on the destination pool, doubling pool I/O
  to buy atomic rename): **revisit at Phase 3**, with measurements, rather than
  deciding in advance. Phase 3 is where the commit ritual is actually built. It
  is the only ACCEPTED item of 56 fatal-flaw resolutions; every other one is
  FIXED.

## Working agreement

**Per phase:** implement → meet the Milestone → PR → merge → report → next.

**Every PR needs a changelog fragment** in `changelog.d/` or CI fails. There is
a `Require changelog fragment` check. Escape hatches exist (`skip-changelog`
label, `[skip changelog]` in the title) but prefer writing the fragment.

**Every file you create or modify needs its 4-line header updated** — `file:`,
`version:`, `guid:`, `last-edited:` — with the version bumped. Comment style
follows the file type (`<!-- -->` for Markdown, `//` for Rust, `#` for
TOML/Python). Missing or stale headers fail review.

**Merge method is rebase.** Squash and merge-commit are both disabled on the
repo.

**Verification before claiming done**, in this order:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

All three must be green. That triple is the M1 exit criterion in the
architecture document, and it currently passes — do not let it regress.

## The phase plan

From `distributed-architecture.md` § Phased Delivery Plan. The sequencing is
deliberate: **irreversible risks are retired before any distributed machinery
exists to amplify them.** Do not reorder to get something demoable sooner.

- **Phase 0 — Environment preflight.** No orchestrator code. A
  `transcodarr admin diagnose --preflight` running four probes. *Must run on the
  real hardware.* If the WSL2/Windows node fails `RenameProbe`, **the
  architecture changes here, not later**: the GPU agent becomes produce-only and
  a server-local agent performs commits. Discovering that after the dispatcher
  exists costs weeks. **Do this before Phase 2.**
- **Phase 1 — Workspace split and `transcodarr-core`.** Mac-friendly. The
  508-line `src/main.rs` moves to `crates/transcodarr-cli/`; `transcodarr-core`
  gets no tokio, no rusqlite, no tonic. Milestone: the R70 fixture set passes
  with zero media files, zero network, zero DB — including a synthetic
  truncated-output probe that fails `ValidationGate::Duration` *before*
  `ValidationGate::Size` is consulted. All 16 legacy tests still pass against
  `transcodarr local`.
  **Note:** the doc says this phase fixes two bugs during extraction (dry-run
  creating directories, unknown presets silently ignored). **Both are already
  fixed on `main`** by PR #13 — do not redo them.
- **Phase 2 — `transcodarr-store`, scanner, evaluator.** Schema, migrations,
  the single-writer task, repositories, `Scanner`, `Evaluator`,
  `transcodarr admin explain <path>`. Milestone scans all three real libraries
  (~49.6k files).
- **Phase 3 — Single-node executor and commit ritual.** The risk-retirement
  milestone. A fault-injecting crash matrix across all nine ritual steps must
  always resolve to source-intact or replacement-installed, never neither.
  **Revisit D14 here.**
- **Phase 4 — Protocol, one agent, dispatcher** (audio class only).
- **Phase 5 — GPU class, capability probing, emergent two-stage.**
- **Phase 6 — Observability, schedules, UI.**
- **Phase 7 — Hardening.**

`task-inventory.json` (414 tasks) is **unaudited** — produced by a workflow
interrupted before its verification pass, never reconciled against the
architecture document, possibly missing whole subsystems. The Phase 0–7 plan
supersedes it. Use the inventory as a reference for detail, never as a plan of
record, and never hand it to implementer agents as-is.

## Environment

- **`172.16.2.30`** is "the server" (also called U0) whenever the owner says
  "the server" unqualified. Runs the Tdarr server container, holds the media
  libraries and the ZFS pool. Reachable by SSH as `jdfalk`.
- **`172.16.2.35`** is U1, the CPU transcode node (`unimatrixone-cpu`), 48
  cores. Root SSH from the server.
- **`windows-rtx2070`** is the GPU node (Turing RTX 2070, NVENC).
- Local Mac clone: `~/repos/github.com/jdfalk/transcoderr` — **the directory
  still has the old name**; only the repo and crate were renamed. Harmless;
  rename it whenever convenient.

**The server has no GitHub credentials.** No `gh`, no PAT, no authorized SSH
key. Work committed there must be pulled to the Mac and pushed from there:

```bash
git remote add unimatrix jdfalk@172.16.2.30:/home/jdfalk/repos/github/jdfalk/transcodarr
git fetch unimatrix 'refs/heads/BRANCH:refs/remotes/unimatrix/BRANCH'
```

Working Tdarr scripts on the server, **not in git** — `tdarr-classify.py` is
effectively the policy engine transcodarr is replacing, and is worth reading
before writing the `Evaluator`:

```text
~/ai/tdarr/tdarr-classify.py       # queue state from STORED probe data, no rescan
~/ai/tdarr/tdarr-ensure-node.py    # per-node worker limits + per-hour schedule (cron */3)
~/ai/tdarr/tdarr-watchdog.py       # restarts a dead node agent (cron */10)
~/ai/monitoring/prometheus/        # alert rules + exporter inventory
```

`tdarr-ensure-node.py` **re-arms worker counts every 3 minutes**, so changes
made live through the Tdarr API are silently undone unless that file is edited
too.

## Traps that already bit this session

Each of these cost real time. They are not hypothetical.

1. **The server's clone was 16 commits behind `main`.** A branch pushed from it
   would have reverted two commits' worth of work — deleting `.standards`,
   `CLAUDE.md`, `AGENTS.md`, `changelog.d/`, `todo.d/`, and 37 `.github/` files.
   **Always check ancestry before pushing a branch authored elsewhere:**

   ```bash
   git merge-base --is-ancestor origin/main BRANCH && echo OK || echo "STALE BASE"
   git diff --name-status origin/main BRANCH   # look for unexpected D lines
   ```

   The fix is to cherry-pick onto current `main`, not to force the branch.
2. **Clippy fails open.** A single unknown key makes clippy reject its *entire*
   config and refuse to run — indistinguishable from "no warnings" if you only
   check the build's exit code. This repo had no lint coverage at all until PR
   #14. Verify clippy actually *ran*, not just that the command returned 0.
3. **The shell is zsh, not bash.** `for f in $FILES` does **not** word-split.
   Use an explicit list or `${=FILES}`.
4. **GNU tools are `g`-prefixed** on the Mac: `gsed`, `ggrep`, `gawk`, `gdate`.
   BSD versions are the unprefixed defaults.
5. **Renaming is not always a safe sed.** Sweeping `transcoderr` → `transcodarr`
   through URLs produced `github.com/jdfalk/transcodarr`, a path that never
   existed and therefore never redirects. Identifiers and addresses need
   different treatment.
6. **A checker that finds problems is not automatically right.** A column-drift
   script reported 6 bad tables; the real cause was the parser taking one token
   per line while the DDL packs several. Verify a finding before reporting it.

## Verified state of the design docs

A mechanical cross-check of the architecture document against the naming
contract was run on 2026-07-31 and is worth trusting rather than repeating:

- Tables: **21/21** clean — every table referenced in SQL is declared, every
  declared table referenced.
- Table columns vs `CREATE TABLE` DDL: **0 differences across all 21 tables**.
- RPCs, job states: clean.
- Metrics: 64/64 after two Security-section additions were back-ported.

Two known cosmetic defects remain, both harmless: `<new uuid>` placeholders at
lines 169 and 214 (inside example `Cargo.toml` blocks), and
`task-inventory.json` carries no file header because JSON cannot hold comments.

## Open items

- **D14** — decide at Phase 3, with measurements.
- **Pre-existing duplicate guid** `a1b2c3d4-e5f6-7890-abcd-ef1234567890` shared
  by `.editorconfig`, `.github/dependabot.yml` and `.github/workflows/ci.yml`.
  Predates all this work; unrelated cleanup.
- **Local directory rename** — cosmetic, owner's convenience.

## Live Tdarr — scheduled action

Tdarr remains in production during the build. As of handoff: 6,575 files queued
(anime 5,209 · TV 780 · movies 586), 2,634 GB reclaimed lifetime. All three
libraries point at flow `d3uF5r_3e` (audio-only, `-c:v copy -c:a eac3 -b:a 640k
-c:s copy`) — there is no video encoder in it.

**An `at` job is scheduled for 15:00 on 2026-08-01** (`atq` job 2 on the server)
running `~/ai/tdarr/tdarr-up-away.sh`. The owner is leaving, so the noise and
thermal limits that held U1 at 4 worker slots no longer apply. It raises
`UNI_WORKERS` 4 → 16 and reasserts `CPUQuota=3800%`, backing up
`tdarr-ensure-node.py` first and logging to `~/tdarr-up-away.log`.

Sizing rationale: audio-only remux measures ~1.8–2.2 cores per job, so 16 slots
demand ~35 cores against a 3800% (38-core) cgroup ceiling on a 48-core box.
`CPUQuota` is the hard backstop and holds regardless of slot count. This is
**not** the 24 that caused load 127 on 2026-07-30 — that was video work at ~4.5
cores per job, ~110 cores demanded.

Revert:

```bash
sed -i 's/^UNI_WORKERS=16/UNI_WORKERS=4/' ~/ai/tdarr/tdarr-ensure-node.py
python3 ~/ai/tdarr/tdarr-ensure-node.py
```

Four known Tdarr issues were **deliberately left unfixed** — the owner chose the
scheduled throughput bump over repairing them:

1. `windows-rtx2070` has 3 GPU slots idle: the audio-only flow's `cpugate`
   requires `workerType == CPU`, so GPU slots take work and bail.
2. `queueSortType` regressed to `sortSizeSmallest`. It was `noSort`, the single
   biggest dispatch win — with 6,575 queued, the single-threaded server re-sorts
   the entire queue on every dispatch request.
3. Flow `hqhevcnvenc2` is broken and unassigned: both branches of its `gpugate`
   `customFunction` return `outputNumber: 1`, so a CPU worker falls straight
   into the video encoder. The CPU branch must return `outputNumber: 2` wired to
   a requeue terminal. **3,558 anime video files are parked behind this.**
4. 242 files sit in `Transcode error` (anime 125, TV 88, movies 29).

## Media-correctness rules that must not be lost

From production measurement. If the implementation loses these, rediscovering
them is expensive.

- **Size is never an accept criterion.** A truncated file is always smaller. The
  measured AV1/NVDEC hard failure produces ffmpeg exit 69 and a 1 KB output — a
  size-first gate accepts exactly the outputs that destroyed the media. Compare
  duration against the **last packet PTS**, not the container header (a
  truncated MKV usually keeps the source duration in its header), tolerance
  asymmetric and absolutely capped at `min(0.5%, 5s)`.
- **Hardware decode must be gated per codec, never enabled globally.** Turing
  NVDEC cannot decode AV1 at all (hard failure) and cannot decode 10-bit H.264
  (soft failure, silently falls back to software).
- **Map all audio and subtitle streams.** A bare `-c:a eac3` silently drops
  every track but the default.
- **Preserve bit depth.** libx265 wants `yuv420p10le`, NVENC wants `p010le`; the
  wrong one errors the job. Never upconvert 8-bit.
- **Never re-encode HDR or Dolby Vision video.** DV profile 7 and object-audio
  titles are excluded from all work by default; unknown DV vetoes.
- **Re-encode lossless (TrueHD/DTS/FLAC/PCM/MLP) and Opus to EAC3 640k.** Leave
  aac/ac3/eac3/mp3 alone. The owner specifically does not want Opus output.
- **Throughput ceilings.** NVENC on the RTX 2070: aggregate fps 71 / 101 / 117
  for 1 / 2 / 3 sessions — the encoder ASIC is the bottleneck, not CPU. One
  software x265 encode uses ~13 effective threads; ~4 saturate a 48-core box.
- **The ZFS pool is latency-bound.** 47 concurrent 40–80 GB jobs produced
  per-file ETAs of 3–34 hours; large files need their own low concurrency cap.
  ZFS snapshots also mean in-place replacement reclaims nothing until they
  expire — measure reclaim from `zfs used`/`usedbysnapshots`, never from file
  sizes.

## First action for the next session

**`ConnectClient`, the agent side of the transport.** Everything the server
needs is in place and covered; nothing on the agent knows how to talk to it.

Read § *Phase 4 — in progress* above first, for the ordered list, the three
codegen facts and the `Connect` semantics that will otherwise each cost an hour
to rediscover.

What it has to do:

1. Register, and keep the `fencing_epoch` and `boot_id` it was issued. **The
   `boot_id` must be generated once per process and reused across every
   reconnect** — a fresh one on each attempt turns every network blip into an
   epoch bump that fences work still running perfectly well.
2. Replay the fsynced `IntentJournal` in the `Register` call, before accepting
   anything, and act on the `unknown_job_ids` that come back.
3. Open the stream with `x-agent-id` and `x-agent-epoch` metadata. There is no
   identity in `AgentMessage`; see the `session.rs` module documentation.
4. Heartbeat on a timer, carrying the running set, and honour `Revoke` and
   `Drain`.
5. Reconnect with backoff, re-registering each time — the server resumes the
   epoch for the same `boot_id`, so a reconnect is cheap and correct.

`crates/transcodarr-server/tests/connect.rs` is the pattern to copy: a real
tonic server on a loopback port, dialled with the generated client. Note
`AgentServiceClient::new(channel)` rather than a `connect(dst)` constructor.

**Do not let `transcodarr-agent` acquire a `transcodarr-store` dependency.** It
has to stay copyable to the Windows node without dragging SQLite along. Check
with `cargo tree -p transcodarr-agent -i transcodarr-store` rather than by eye;
it currently holds, and a shared proto crate makes a transitive dependency easy
to add by accident.

After that comes `serve` and `agent connect`, then the dispatch loop — and only
then can the end-to-end proof be written: one audio job through register →
connect → assign → result → `RequestCommit` → `ReportCommit` → reconcile, with
a stale-epoch `ReportCommit` in the same test rejected and the job left
untouched. All of that is provable on the Mac with no media and no server
access.

The real-hardware milestone (24 concurrent audio jobs on U1) comes last, and
`transcodarr admin summary` on the server will say whether the Phase 2 probe run
ever finished.
