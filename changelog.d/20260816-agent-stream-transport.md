<!-- file: changelog.d/20260816-agent-stream-transport.md -->
<!-- version: 1.0.0 -->
<!-- guid: 4c9a0e73-1b52-4f86-a3d0-72e58b1cd904 -->
<!-- last-edited: 2026-08-16 -->

### Added

#### The agent side of `TM_STREAM`

A streaming agent now fetches its source, encodes, and pushes the result back.
The server's halves of both RPCs already existed; nothing on the agent could
call them. `Link` only ever sent messages on the `Connect` stream, so there was
no handle a worker could make a transfer with.

`Link` now carries a second handle on the same channel — cloning a tonic client
is a refcount bump, not another connection — along with the agent id needed to
stamp a request. `FetchSource` and `PushOutput` are separate RPCs from
`Connect`, so each has to assert its own identity: the stamp that authenticated
the stream does not travel to anything else, and an unstamped transfer is
refused with an `Unauthenticated` that names nothing.

Under `TM_STREAM` the worker skips `ensure_same_device` and
`SourceGuard::observe`, which both stat `final_path` — a canonical path in the
*server's* namespace, an identifier here rather than a location. It also asks
for no commit grant and runs no ritual, because the server installs. The push
is the whole exchange, and a `CommitReport` on top would be a second verdict on
one outcome. `live_intents()` needs no branch for this: the ritual is the only
production writer of the journal, so a path that never runs the ritual leaves
it empty structurally.

### Changed

#### `transfer` moved from `transcodarr-server` to `transcodarr-proto`

Both ends of a stream transfer need the same `Sink` — the same offset-gap check
and the same whole-file blake3 gate. The agent cannot depend on
`transcodarr-server`, which links SQLite and would make the agent unbuildable
for the Windows node, so the alternative was a copy. `buf.yaml` already argues
this exact case about `FileChunk`: two definitions obliged to stay identical are
a second implementation free to drift, and here the thing that would drift is
the one gate standing between a truncated transfer and an installed corrupt
file.

`output_stream` joins it as the mirror of `source_stream`, for the
client-streaming direction. It reports a read failure **by omission** — ending
the stream without the final chunk, so the receiver never verifies a signature
and never declares the transfer complete. Synthesising a terminator would hand
the receiver a truncated file with a signature computed over the truncation,
which verifies perfectly.

### Fixed

A fetch that fails removes what it had written. A partial source left on disk
would be handed to ffmpeg as though it were whole, and a short input produces a
short output that the duration gate then blames on the encode.

The fetched source and the encode output are both swept when a streaming job
ends, however it ends. A fetched source is a whole copy of the original; one
left behind per failed job fills the work area, and the next fetch then fails
for want of room on a machine that looks idle.
