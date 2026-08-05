### Changed

#### Handoff: a standing goal, and a current-state section that is actually current

The document now opens with the mandate rather than leaving it implied halfway
down: get everything possible done that does not genuinely require the owner,
take sane defaults and say which were taken, fix flaws where they are found
rather than filing them, and correct stale code and documentation on sight.

The *Current state* section had rotted badly enough to mislead. It claimed **no
orchestrator code exists yet** and described the CLI as "a single 508-line
`src/main.rs`", written before six crates and five subsystems landed. It now
carries a per-crate table whose last two rows — `transcodarr-agent` having no
transport client, and `transcodarr-cli` having no `serve` — are exactly what
remains in Phase 4.

*First action for the next session* pointed at `Connect`, which has shipped. It
now points at `ConnectClient`, with the five things it has to do, the
`boot_id`-per-process rule that makes reconnects cheap instead of destructive,
and the layering check that keeps SQLite out of the agent.
