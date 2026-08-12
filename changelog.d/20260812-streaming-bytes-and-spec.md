<!-- file: changelog.d/20260812-streaming-bytes-and-spec.md -->
<!-- version: 1.0.0 -->
<!-- guid: 53546416-d217-4c6e-9957-90a02dcdbe72 -->
<!-- last-edited: 2026-08-12 -->

### Added

#### The byte plumbing for the streaming transport

`transcodarr-server::transfer` reads a source into an ordered chunk stream and
receives chunks into a file the server can vouch for. Nothing is wired to the
RPCs yet — `FetchSource` and `PushOutput` still refuse explicitly — so this adds
no behaviour an agent can reach.

Two properties carry the correctness, and both have tests that fail without
them. `offset` is checked rather than trusted, so a restarted stream is detected
instead of being appended to. And the final chunk carries a blake3 of the whole
file, checked before the bytes are used for anything: a truncated transfer is
*smaller*, and size is never an accept criterion here, so without the hash a
half-received encode would install cleanly over a good original.

### Documentation

#### The architecture document describes both transports

`distributed-architecture.md` documented only direct mounted access. It
contained no mention of upload, download, byte ranges, `TransportMode`,
`FetchSource` or `PushOutput`, and stated `MountCovers` as an unconditional
requirement — which made the second transport impossible to express. A node with
no usable share was undispatchable no matter how good its encoder was.

That was a defect in the document, not a decision: the code faithfully
implemented a spec that had dropped the requirement. The new *Transport modes*
section sets out both modes, why the server performs the commit ritual when the
agent cannot, and why `MountCovers` applies only to mount-mode nodes.
