<!-- file: docs/design/IMPLEMENTATION-HANDOFF.md -->
<!-- version: 3.0.0 -->
<!-- guid: 9d4a7c31-6b28-4e5f-8a03-2c7e1b9f04d6 -->
<!-- last-edited: 2026-08-03 -->

# Implementation handoff — transcodarr

Start here. This document is the entry point for building transcodarr out from
the committed design. It assumes no prior conversation context.

Read in this order:

1. This file (orientation, working agreement, traps).
2. `docs/design/distributed-architecture.md` — the specification. Section
   **Phased Delivery Plan** (Phase 0–7) is the execution roadmap.
3. `docs/design/synthesis-decisions.md` — the binding naming contract. SQL
   tables, Rust types, RPCs, metrics, job states. **Treat as authoritative.**

## Phase status (updated 2026-08-02)

| Phase | State |
| --- | --- |
| **0 — Environment preflight** | **Done on U0 and U1**, both commit-eligible. `windows-rtx2070` not run — see `PHASE0-RESULTS.md`. Does not block Phase 2. |
| **1 — Workspace split and `transcodarr-core`** | **Complete.** All milestone criteria met with zero media, network or DB. |
| 2 — `transcodarr-store`, scanner, evaluator | **Code complete; milestone part-run.** Store, `Scanner`, `Prober`, `Evaluator`, `admin explain` and the operator commands all shipped. Discovery verified on all three real libraries (49,600 files in 43 s). Probe ingestion is long-running — see below. |
| 3 — Single-node executor and commit ritual | Not started. Revisit D14 here. |
| 4 — Protocol, one agent, dispatcher | Not started. |
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
redirect). `main` is green: `cargo build`, `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test`
(16 passed / 0 failed / 2 ignored) all pass.

Shipped so far — design plus groundwork only, **no orchestrator code exists yet**:

| PR | What |
| --- | --- |
| #11 | The three design documents |
| #12 | Rename `transcoderr` → `transcodarr` (crate, binary, repo) |
| #13 | Four CLI correctness bugs fixed (suite went 12/4 → 16/0) |
| #14 | `clippy.toml` repaired; lint had been silently dead |

The existing CLI is still a single 508-line `src/main.rs` with `info`,
`transcode` and `batch`. Phase 1 dissolves it into a workspace.

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

```
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

Phase 1, on the Mac: the workspace split and `transcodarr-core`. It needs no
media, no server access, and no network. Read
`distributed-architecture.md` § *Crate and Workspace Layout* and § *Phase 1*
first — the layout, the module list, and the table mapping every current
`src/main.rs` function to its new home are all specified there.

Phase 0 preflight must happen on the server before Phase 2 begins, because its
outcome can change the architecture.
