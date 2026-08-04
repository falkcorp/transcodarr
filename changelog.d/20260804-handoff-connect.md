### Changed

#### Handoff records the `Connect` stream and the lookup bug it exposed

The server side of the transport is done — codegen, `AgentRepo`, `Register` and
the stream — and the document now says what the stream enforces, what it
deliberately does not (no `JobAssignment`, because there is no dispatch loop),
and why identity travels in request metadata rather than in the schema.

It also records the bug worth not repeating: `CommitIntentRepo::get` takes an
*intent* id while the session only knows a *job* id, so passing one to the other
answered "no live intent" for every job. One test merged earlier was green for
exactly that reason. When a lookup can only ever return `None`, every negative
test passes and proves nothing.
