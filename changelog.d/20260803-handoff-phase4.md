### Changed

#### Handoff records Phase 4's start and the remaining scope

`CapacityLedger` is done; `transcodarr-proto`, `AgentSession`, `Dispatcher` and
`Reconciler` remain, listed in dependency order with what each needs. Records
that Phases 4-7 are each multi-session units and that a partial dispatcher —
one handing out work it cannot account for — is more dangerous than none.
