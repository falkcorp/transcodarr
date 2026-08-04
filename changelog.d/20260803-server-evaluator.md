### Added

#### `Evaluator` — batched policy evaluation over stored facts

The evaluator touches no media. It reads `FileFacts` that discovery and probing
already wrote, runs the same `core::policy::evaluate` the CLI and the agent
link, records the decision, and derives a job where one is owed. That is the
Phase 2 headline property: re-deciding ~49.6k files is a database scan and some
arithmetic, not 85 TB of I/O.

Batches of 1000 over `idx_file_needs_eval`, each **re-queried rather than paged
by offset** — evaluating a file removes it from the working set, so every later
offset shifts and an offset-paged loop skips exactly one file per boundary.

Job ids are derived from `(file_id, class, rules_version, content_sig)` rather
than random, so a re-run cannot mint a second job for the same work. A file that
already has an open job is counted, not failed: `idx_job_open_per_file` would
refuse the insert, and an expected condition should not surface as a write
failure. Larger files carry higher priority — they hold capacity longest, and
starting them late leaves one 60 GB remux running alone at the end of a pass.

#### `capability::bucket_key`

A stable key over the **categorical** requirements only — `AgentClass`,
`Encoder`, `Decoder`, `Muxer`, `PlatformIn`, `LabelEquals`. `MinFreeBytes`,
`MinEffectiveCores` and `MountCovers` are deliberately excluded (flaw A5): they
carry per-file byte counts and paths, so including them drives cardinality
toward one bucket per job and collapses the matcher to O(queue), at which point
precomputed eligibility costs more than it saves. Requirements are sorted before
hashing, so planner-side ordering cannot split one bucket into two.
