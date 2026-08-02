### Changed

#### Handoff doc records phase status

`IMPLEMENTATION-HANDOFF.md` now carries a phase table, the finished state of
`transcodarr-core`, and three carry-forward items: the un-run Windows preflight,
the `CpuQuotaReader` cgroup-path bug that under-reports a quota, and the
requirement to run preflight as the committing user rather than root.
