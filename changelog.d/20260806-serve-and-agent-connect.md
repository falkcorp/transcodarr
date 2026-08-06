<!-- file: changelog.d/20260806-serve-and-agent-connect.md -->
<!-- version: 1.0.0 -->
<!-- guid: 6c93a15e-2b74-4d80-9f16-5a08e7b2413b -->
<!-- last-edited: 2026-08-06 -->

### Added

#### `transcodarr serve` and `transcodarr agent connect`

Both halves of the transport existed and nothing started either. Now a real
agent registers with a real server over gRPC, is issued a fencing epoch, and
holds a stream — verified by running the two processes, not only in tests.

The fleet table is created by the caller and handed to the session rather than
built inside it. Two tables would each work perfectly and see different fleets:
the dispatch loop would find nobody connected and dispatch nothing, forever,
with every part in working order.

An explicitly empty `--token` is refused rather than treated as "no
authentication". Someone who typed `--token ""`, or whose environment expanded
to nothing, meant to set one — opening the port at exactly that moment is the
opposite of what they asked for. An absent token still runs open, and says so
loudly at startup.

#### `transcodarr agent survey`

Prints the capability document this machine would register with, without talking
to anything. This is the command for "why is the dispatcher not giving this
agent work" — it answers what the machine actually advertised, including which
mount failed the rename probe and is therefore costing it `commit_eligible`.

#### `transcodarr-agent::survey`

Everything measured, nothing assumed:

- **Encoders and muxers come from `ffmpeg -encoders`.** A build without
  `libx265` that claimed it would fail every job it was given, an hour at a
  time. Matched as a whole name, so `aac` is not found inside `libfdk_aac`.
- **Cores come from the cgroup quota, not `nproc`.** Scheduling against 48 cores
  on a box limited to 38 is what produced load 127 on 2026-07-30.
- **An unrecognised platform advertises nothing.** A Mac is a development
  machine, not a node; claiming `Linux` there would let a `PlatformIn([Linux])`
  requirement match a machine nobody meant to include. Found by running the
  command on this laptop and reading the output.
- **Only an outright `Pass` earns `RP_ATOMIC_VERIFIED`.** A skipped or warning
  probe means the atomic rename was not demonstrated, and not-demonstrated must
  never become verified by falling through an `else`.

`capability_hash` covers the document, not the free-byte reading enriched onto
it afterwards. Free space changes every second, and folding it in would make
every registration look like an agent whose ffmpeg changed underneath it.

### Fixed

- The long-running verbs install a `tracing` subscriber. They emitted log lines
  into a subscriber that was never initialised, so `serve` ran silently — which
  is indistinguishable from not running at all. `local` and `admin` deliberately
  stay quiet: their stdout is parsed.
