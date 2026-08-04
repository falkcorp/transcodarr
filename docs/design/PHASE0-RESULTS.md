<!-- file: docs/design/PHASE0-RESULTS.md -->
<!-- version: 1.0.1 -->
<!-- guid: c8f45b71-9e23-40da-b6c8-5a17e02f9364 -->
<!-- last-edited: 2026-08-04 -->

# Phase 0 — preflight results

Run 2026-08-02 with `transcodarr admin diagnose --preflight`, built from
`main` and executed on the real hardware.

## U0 — `unimatrixzero` (172.16.2.30), the server

| Probe | Status | Detail |
| --- | --- | --- |
| `RenameProbe` | **PASS** | rename over an open destination succeeded, `/mnt/bigdata` |
| `DbFsyncLatency` | **PASS** | 1000 fsyncs on `/opt/tdarr-server`: p50 2.60 ms, p99 6.27 ms |
| `ZfsSnapshotPolicy` | PASS | `bigdata/BD/bigdata`: 157.1 TB used, ~0 held by snapshots, 11.8 TB available |
| `CpuQuotaReader` | PASS | no quota; 48 effective cores |

**Commit eligible: yes.**

The fsync figure matters: p99 6.27 ms is comfortably inside the 10 ms warn
threshold, so a single-writer SQLite with `synchronous=FULL` will not be the
pacing constraint. The DB belongs on `/opt/tdarr-server` (`rpool/tdarr`), not on
the media pool.

## U1 — `unimatrixone` (172.16.2.35), CPU node

| Probe | Status | Detail |
| --- | --- | --- |
| `RenameProbe` | **PASS** | as user `tdarr`, on `/mnt/bigdata/tv` over NFS4 |
| `DbFsyncLatency` | PASS | `/tmp` is tmpfs, so the 0.00 ms reading is meaningless here |
| `ZfsSnapshotPolicy` | SKIP | media is NFS-mounted from U0; ZFS accounting lives on U0 |
| `CpuQuotaReader` | PASS | 48 effective cores — see caveat below |

**Commit eligible: yes.** NFS4 preserves POSIX rename-over-open semantics.

Two things worth knowing:

1. **Run the probe as the user that will actually commit.** As `root` every
   probe path failed with `EACCES`: the export is `root_squash` and
   `/mnt/bigdata/tv` is `jdfalk:jdfalk 775`. The Tdarr node runs as `tdarr`, and
   as `tdarr` the probe passes. A preflight run as the wrong user answers a
   question nobody asked.
2. **`CpuQuotaReader` reported 48 cores, but U1 currently runs under
   `CPUQuota=1600%`.** The reader consults cgroup v2 `cpu.max` at
   `/sys/fs/cgroup/system.slice/cpu.max`, while the quota is applied to
   `tdarr-node.service` — a *delegated* slice below that path. The probe needs
   to resolve the invoking process's own cgroup via `/proc/self/cgroup` instead
   of guessing at fixed paths. Tracked as a follow-up; it under-constrains
   rather than over-constrains, so it is not dangerous, but it would let the
   scheduler over-commit U1 by 3x if trusted.

## `windows-rtx2070` — NOT YET RUN

**This is the open Phase 0 item, and it is the one the milestone hinges on.**

Tdarr's `get-nodes` API reports no address for it, and several hosts on the
subnet have SSH open, so the node could not be identified with confidence and
was not probed.

### Why this does not block Phase 2

The architecture already accommodates either answer, because commit eligibility
is **data, not an assumption**: `agent.commit_eligible` is a per-agent column set
from this probe, and `PreflightReport::commit_eligible()` gates it. A node that
fails `RenameProbe` becomes produce-only and a commit-eligible agent performs
the replace — no schema, protocol or dispatcher change is required to express
that. So the work can proceed; what cannot happen is *assuming* the Windows node
is commit-eligible and discovering otherwise in production.

### To close it

Run, on the Windows node, as the user the Tdarr node service runs as, against
the library path it actually writes to:

```bash
transcodarr admin diagnose --preflight --work-dir <library path> --db-dir <temp>
```

If `RenameProbe` fails there, that is the expected result for SMB and is
correctly handled — mark the agent produce-only. If it passes, the GPU node may
commit directly.

## Defect found by running it

The first version conflated two very different failures. On U1 as root it
reported `RenameProbe FAIL — this machine must NOT be commit-eligible`, when the
truth was only that root could not create a file on a `root_squash` export.
Setup failure and rename-semantics failure now report differently: the former is
`WARN / INCONCLUSIVE` and explicitly says it implies nothing about rename
semantics.

That distinction matters more than it looks. The wrong version would have
demoted a perfectly capable node to produce-only on the strength of a
permissions error — an architecture decision made from a misread probe.

The ZFS threshold was also too sensitive: it warned on any non-zero snapshot
usage, which fired on U0 at ~0% of 157 TB. It now warns only on a material hold
(>1 GB or >=1% of used), because a probe that always warns is a probe everyone
learns to ignore.
