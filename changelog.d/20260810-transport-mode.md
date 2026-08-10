<!-- file: changelog.d/20260810-transport-mode.md -->
<!-- version: 1.0.0 -->
<!-- guid: 25c1c859-dff0-4692-8b34-9e4f3f57032b -->
<!-- last-edited: 2026-08-10 -->

### Added

#### Transport mode, chosen per node

The design always called for two ways an agent reaches media, and only one was
ever built. This adds the concept the second one needs.

`TransportMode::Mount` is what every agent did before: read and write the
library directly through a share, with the server translating canonical paths to
that node's local paths. `TransportMode::Stream` is the other half — the server
sends the source bytes, the agent works on a local copy, and the result comes
back for the server to install. A streaming node needs no share, no credentials
and no knowledge of the server's storage layout.

The behavioural change is one line, and it is the whole point:
`Requirement::MountCovers` no longer binds a streaming agent. Such a node
advertises no mounts at all, so under the old rule it satisfied nothing and would
have been handed no work forever while reporting healthy.

This matters concretely. On 2026-08-10 all three of `windows-rtx2070`'s SMB
mounts to the server reported `Unavailable` — Windows drive mappings are
per-logon-session, and the session an agent runs in is not the interactive one
that holds them. Under mount-only transport that node is undispatchable no matter
how well its encoder works, and NVENC there is confirmed working.

Older agents are unaffected: the field is `#[serde(default)]` on the domain type
and `TM_MOUNT = 0` on the wire, so an agent that has never heard of transports
keeps exactly the behaviour it had. A new field that silently switched an existing
agent to streaming would have been the worst way to ship this.

Streaming's byte-moving RPCs and the server-side install are the next two changes;
this is the foundation they attach to.

### Fixed

#### `agent survey` no longer tells a streaming node it is broken

With no mounts it printed "this agent offers no mounts, so it can be given no
work", which under streaming is both wrong and actively misleading — it sends an
operator hunting a share problem on a node deliberately not using one. It now
reports the transport, and says the right thing for each.
