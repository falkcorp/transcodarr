<!-- file: changelog.d/20260805-journal-survives-restart.md -->
<!-- version: 1.0.0 -->
<!-- guid: 4d0c9b17-8e63-4a25-91fd-2b7a06e5c384 -->
<!-- last-edited: 2026-08-05 -->

### Fixed

#### The intent journal now survives a restart

The journal was opened at `<work_dir>/<agent_uid>/<boot_id>/journal`, inside the
work area's per-process namespace. A new `boot_id` is a new directory, so
`CommitRitual::recover_all()` read an empty one and found nothing — every time,
in exactly the case the journal exists for. The fsynced-before-every-step
discipline was intact and unreachable: recovery could not fail because it could
not see anything to recover.

Staged output belongs to one process instance; the journal is the opposite,
written to be read by whoever comes after a crash. It now lives at
`<work_dir>/<agent_uid>/.journal`, stable for the life of the installation, via
`WorkArea::open_journal()`. The leading dot is load-bearing: no `boot_id` can
sanitise to that name, so an agent booting as `journal` cannot stage output into
the directory recording what it was about to do.

`open_journal()` also adopts records left by the previous layout rather than
orphaning them. A `Retired` record means the original is in the trash and the
destination may be empty — the one state where abandoning the evidence is worst.

#### `boot_id` is per process again, on every platform

`boot_id()` was documented as "distinct per process" and read
`/proc/sys/kernel/random/boot_id`, which changes when the *machine* reboots. On
Linux a restarted agent presented the identifier of the instance that had
crashed, the server resumed its epoch, and nothing was fenced — the precise case
the fence exists for, since the new process cannot know what its predecessor had
in flight. On macOS the read failed and it fell back to `pid-N`, so the same code
fenced correctly on a laptop and not on the machine that matters.

### Added

#### `transcodarr-agent::identity`

`boot_id()` behind a `OnceLock`, generated once per process and reused across
every reconnect — a fresh one per connection attempt would turn each network
blip into an epoch bump, fencing work that is running perfectly well. Plus
`agent_uid()`, moved out of `transcodarr-server::runner` so the agent and the
in-process runner cannot drift apart on what they call themselves.
