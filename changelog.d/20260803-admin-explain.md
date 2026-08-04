### Added

#### `admin explain <path>` — why is this file not being transcoded?

The question Tdarr could never answer. Reads stored facts only, and prints the
rule trace: which rules matched, which did not, and what each contributed. "No
work" is not an answer; "matched `convert lossless and opus audio to eac3`,
audio stage planned" names the rule an operator would have to edit.

The decision is **recomputed** from the current policy rather than read back, so
a stored decision that predates the current policy shows up as a flagged
disagreement instead of being reported as the truth — which is exactly what an
operator is chasing when a policy edit appears to have had no effect. A failed
probe is distinguished from a file nobody has looked at yet, and reports the
ffprobe error: the two need different actions.

#### Operator commands: `add-library`, `libraries`, `scan`, `evaluate`, `summary`

`scan` runs discovery, probe ingestion and evaluation for a library. `evaluate
--force` re-derives every decision from stored facts with no filesystem access
at all. `summary` gives the decision/count/GiB breakdown — what needs
transcoding across the library — aggregated in SQL.

#### `Runtime` — the store's single consumer

Settles the layering question left open earlier: `transcodarr-cli` does **not**
link `transcodarr-store`. It calls into `transcodarr-server::Runtime`, so no
SQL, no `rusqlite` type and no repository appears in the CLI, and only one crate
holds opinions about connection lifetimes, pragmas and the single-writer rule.
The aggregate queries behind `summary` live in the repositories for the same
reason.

### Changed

#### `JobState` gains `Display`

So a job state renders the same way in the CLI, the logs and the database.
