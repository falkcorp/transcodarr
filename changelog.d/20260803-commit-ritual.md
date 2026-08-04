### Added

#### The commit ritual, its intent journal, and the crash matrix

Phase 3's risk-retirement core. One invariant governs everything here:

> At every instant, and after any crash at any instant, either the original file
> is intact or the replacement is fully installed. Never neither.

No system call gives that for free. Replacing a file means moving the original
aside and moving the new one in, and between those renames the destination holds
nothing. The ritual survives a crash in that window by journalling its intent
**before** each step, so recovery can distinguish "about to retire" from
"already retired" — states indistinguishable from the filesystem alone.

`IntentJournal` writes `Granted` → `Retired` → `Installed`, each fsynced before
the step it describes. Records are written to a staging name and renamed into
place, so a crash mid-write leaves the previous record rather than a truncated
one; both the file and its directory are fsynced, because a `write` + `fsync`
leaves the directory entry unsynced and a power loss can otherwise leave a
journal that does not exist.

`WorkArea` is namespaced by `agent_uid`/`boot_id`, so two agents — or a
restarted agent and its own leftovers — cannot collide on a temporary name.
Identifiers arriving from the server are sanitised: a job id containing `..`
must not place a file outside the work area.

**A cross-device work area is a hard refusal.** `rename(2)` is atomic only
within one filesystem; the copy-then-delete fallback has a window where neither
the source nor a complete replacement exists. This is design decision D14
settled in the concrete: the work area is colocated on the destination pool,
paying roughly double pool I/O for the encode in exchange for an atomic install.
Staging on fast local scratch moves the same bytes anyway and buys a non-atomic
install — the I/O is not saved, only deferred to the least recoverable moment.

Recovery resolves to exactly four outcomes and never invents a fifth:
`Installed`, `SourceIntact`, `SourceRestored`, or `NeedsOperator`. The last is
produced only when the destination holds nothing *and* the original cannot be
found where the journal says it was put — never by guessing. Unresolved intents
are deliberately left on disk; they are the only record that a human is needed.

The crash matrix drives every phase against every reachable on-disk state and
asserts the invariant on each. The assertion was verified to actually fail: with
the restore step deliberately sabotaged, it reports `INVARIANT VIOLATED` rather
than passing.
