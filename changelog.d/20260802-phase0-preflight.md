### Added

#### Phase 0: `transcodarr admin diagnose --preflight`

Four probes that must pass before any orchestrator code is trusted on a machine,
in a new `transcodarr-agent` crate. It depends on `transcodarr-core` and nothing
else internal — never on the store, so an agent stays copyable to the Windows
node without dragging SQLite along.

- **`RenameProbe`** creates a destination, **holds it open**, renames a second
  file over it, and verifies the destination is now the new file. Holding it
  open is the whole point: a probe that renames over a *closed* file passes on
  filesystems where the real commit would fail, which is worse than no probe.
  Failure here is architecture-changing — that node becomes produce-only and a
  server-local agent performs commits.
- **`DbFsyncLatency`** times 1000 fsyncs and reports p50/p99. Filesystem *type*
  is deliberately not the gate; measured latency is.
- **`ZfsSnapshotPolicy`** reads `used`/`usedbysnapshots`/`available`. Snapshots
  retaining replaced data means reclaim must be measured from ZFS accounting
  rather than file sizes, and the operator needs to know before the first commit
  rather than after a terabyte of savings that never materialised.
- **`CpuQuotaReader`** resolves effective cores from cgroup v2. The raw core
  count is the wrong number under a quota — U1 has 48 cores and runs at
  `CPUQuota=1600%`.

Probes degrade to `Skipped` where they do not apply rather than failing. A macOS
development machine has no cgroup v2 and may have no ZFS; reporting failure
there would train everyone to ignore the output. A failed probe exits non-zero,
because a gate that always exits 0 gates nothing.
