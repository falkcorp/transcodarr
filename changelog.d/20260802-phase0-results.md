### Fixed

#### `RenameProbe` no longer conflates a permissions error with a rename failure

Running preflight on U1 as `root` reported `FAIL — this machine must NOT be
commit-eligible`, when the truth was only that root could not create a file on a
`root_squash` NFS export. Setup failure and rename-semantics failure are now
distinct: the former reports `WARN / INCONCLUSIVE` and states explicitly that it
implies nothing about rename semantics.

The wrong version would have demoted a capable node to produce-only on the
strength of a permissions error — an architecture decision made from a misread
probe.

#### `ZfsSnapshotPolicy` warns only on a material snapshot hold

It previously warned on any non-zero value, which fired on U0 at roughly 0% of
157 TB. The threshold is now >1 GB or ≥1% of used. A probe that always warns is
a probe everyone learns to ignore.

### Added

#### `docs/design/PHASE0-RESULTS.md`

Preflight results from the real hardware. U0 and U1 both pass and are
commit-eligible; `windows-rtx2070` could not be identified from the Tdarr API
and remains the open item, with a note on why it does not block Phase 2 and
exactly how to close it.
