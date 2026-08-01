### Added

#### Design documents for the distributed transcode orchestrator

`docs/design/distributed-architecture.md` specifies a self-hosted replacement for
Tdarr: a control plane plus capability-advertising worker nodes, a declarative
policy engine in place of a visual flow builder, and dispatch that admits a job
only when some node can actually run it — so an unsatisfiable job is rejected
once with a reason instead of retrying forever.

`docs/design/synthesis-decisions.md` records the base design, the resolutions
taken on each fatal flaw raised during critique, the subsystem breakdown, and the
naming contract (SQL tables, RPC methods, metric names) that the architecture
sections are written against.

`docs/design/task-inventory.json` is the raw, **unaudited** task decomposition.
It is a working artifact, not a plan of record: it was produced by a run that was
interrupted before its verification pass, so it has not been size-checked or
reconciled against the architecture document and may omit whole subsystems.

These are documents only. No runtime behaviour changes and the existing CLI is
untouched.
