<!-- file: docs/design/CONTINUE-PROMPT.md -->
<!-- version: 1.0.0 -->
<!-- guid: 1e7a4c50-8b93-42df-9c26-0a5f31d8b64e -->
<!-- last-edited: 2026-08-03 -->

# Continuation prompt

Paste everything below the line into a fresh session to resume building
transcodarr. It is written to be self-sufficient with no prior context.

---

Continue building **transcodarr**, a self-hosted replacement for Tdarr, in
`~/repos/github.com/jdfalk/transcoderr` (repo is `falkcorp/transcodarr`; the
local directory still has the old name, which is fine).

**Read these three files first, in order, before touching anything:**

1. `docs/design/IMPLEMENTATION-HANDOFF.md` — orientation, working agreement,
   phase status, and six traps that have already cost time.
2. `docs/design/PHASE0-RESULTS.md` — preflight results from the real hardware.
3. `docs/design/distributed-architecture.md` § **Phased Delivery Plan** — the
   roadmap. `docs/design/synthesis-decisions.md` is the binding naming contract
   for SQL tables, RPCs, metrics and job states; treat it as authoritative.

## Standing authority — do not ask, just work

- **Run phases end-to-end.** Implement a phase, meet its documented Milestone,
  open the PR, merge it when CI is green, then start the next phase. Do not stop
  to summarise between phases. Do not ask permission.
- **Merging is pre-authorised** for tested work. The gate is evidence — CI
  green, a real test run, a verified build — not permission.
- **Use your judgement on design questions and document why in the commit
  message and PR body.** Do not block on the owner. If you disagree with the
  spec, say so in the PR and proceed under it.
- Stop only if a milestone genuinely fails, or a Phase 0-style finding changes
  the architecture.
- When context runs low, do not stall: land what is green, update
  `IMPLEMENTATION-HANDOFF.md` with real status, and say plainly what is left.

## Where things stand

`main` is green: `cargo fmt --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, and `cargo test` (124 passing) all clean.

- **Phase 0 — done** on U0 and U1, both commit-eligible.
- **Phase 1 — complete.** `transcodarr-core` is finished: `paths`, `plan`,
  `preset`, `probe`, `validate`, `capability`, `failure`, `facts`, `policy`.
  `transcodarr-agent` exists with `preflight` only.
- **Phase 2 is next** — `transcodarr-store`: schema as embedded migrations, the
  single-writer task with priority lanes and per-op `SAVEPOINT` isolation,
  `ReadPool`, the repositories, `Scanner` and `Evaluator`, plus
  `transcodarr admin explain <path>`.
- Phases 3–7 follow. **Revisit fatal-flaw D14 at Phase 3**, with measurements —
  it is the only ACCEPTED item of 56 and the owner deferred it deliberately.

## Working agreement

- Every PR needs a changelog fragment in `changelog.d/` or CI fails.
- Every file created or modified needs its 4-line header updated (`file:`,
  `version:`, `guid:`, `last-edited:`) with the version bumped. Comment style
  follows the file type.
- **Merge method is rebase.** Squash and merge commits are disabled.
- Before claiming anything is done: `cargo fmt -- --check`, then
  `cargo clippy --all-targets --all-features -- -D warnings`, then `cargo test`.
  All three green. That triple is the M1 exit criterion and currently passes —
  do not let it regress.

## Carry-forward items

1. **`CpuQuotaReader` under-reports.** It reads fixed cgroup v2 paths and
   reported 48 cores on U1 despite `CPUQuota=1600%`, because the quota lives on
   the delegated `tdarr-node.service` slice. It should resolve
   `/proc/self/cgroup`. It under-constrains rather than over-constrains so it is
   not dangerous, but a scheduler trusting it would over-commit U1 threefold.
   **Fix before Phase 4.**
2. **`windows-rtx2070` preflight has not run.** Tdarr's `get-nodes` API reports
   no address for it. It does **not** block Phase 2: commit eligibility is
   per-agent data (`agent.commit_eligible`), not a design assumption, so either
   outcome is already expressible. Ask the owner for the address when they are
   around; do not probe unidentified hosts.
3. **Always run preflight as the user that will commit**, never as root. On a
   `root_squash` export every path fails `EACCES` and the answer is meaningless
   — this already produced one wrong FAIL that would have demoted a capable node.

## Environment

- **`172.16.2.30`** is "the server" (U0) whenever the owner says "the server"
  unqualified. SSH as `jdfalk`. Holds the media, the ZFS pool and the Tdarr
  server container. **It has no GitHub credentials** — work committed there must
  be pulled to the Mac and pushed from there. It also has no `git-lfs`, so
  `git push` to it fails on the LFS-tracked `testdata/`; rsync the source
  instead (`--exclude target/ --exclude .git/ --exclude testdata/`) and build in
  `~/transcodarr-build`.
- **`172.16.2.35`** is U1, the CPU node, 48 cores, root SSH from U0. The Tdarr
  node runs as user `tdarr`.
- The shell is **zsh**: `for f in $VAR` does not word-split. GNU tools are
  `g`-prefixed (`gsed`, `ggrep`).

## Live Tdarr — do not disturb without being asked

Still in production during the build. Current settings, all deliberate:
`queueSortType=noSort`, `UNI_WORKERS=8`, U1 `CPUQuota=1600%`.
`~/ai/tdarr/tdarr-ensure-node.py` re-arms worker counts every 3 minutes via
cron, so live API changes are silently undone unless that file is edited too.

`~/ai/tdarr/tdarr-classify.py` is effectively the policy engine transcodarr is
replacing — worth reading before writing the `Evaluator`.

## Media-correctness rules that must survive into the implementation

From production measurement; expensive to rediscover.

- **Size is never an accept criterion.** A truncated file is always smaller. The
  measured Turing AV1/NVDEC failure gives exit 69 and a ~1 KB output. Compare
  duration against the **last packet PTS**, not the container header, tolerance
  asymmetric and capped at `min(0.5%, 5s)`. Already implemented in
  `core::validate` — do not weaken it.
- **Gate hardware decode per codec, never globally.** Turing NVDEC cannot decode
  AV1 at all, and silently soft-falls-back on 10-bit H.264.
- **Map all audio and subtitle streams.** A bare `-c:a eac3` drops every track
  but the default.
- **Preserve bit depth**: libx265 wants `yuv420p10le`, NVENC wants `p010le`.
  Never upconvert 8-bit.
- **Never re-encode HDR or Dolby Vision.** DV profile 7 and object audio are
  excluded from all work.
- **Re-encode lossless and Opus to EAC3 640k**; leave aac/ac3/eac3/mp3 alone.
  The owner does not want Opus output.
- **The ZFS pool is latency-bound** — 47 concurrent 40–80 GB jobs gave per-file
  ETAs of 3–34 hours. Large files need their own concurrency cap, and reclaim
  must be measured from `zfs used`/`usedbysnapshots`, never from file sizes.

Start with Phase 2. Do not stop to check in.
