### Added

#### The `Connect` stream, and the fence enforced over the wire

`AgentSession` now serves the bidirectional stream. An agent that has registered
can hold a connection, be accounted for, and have its commits judged.

The stream is identified by request metadata (`x-agent-id`, `x-agent-epoch`)
rather than by a message, because `AgentMessage` carries no identity — every
variant is a message *about* work, not about the sender. Adding a `Hello` to the
schema would also work and was not done: `agent.proto` is the reviewed agreement
between both ends, and a field serving the transport's convenience does not
belong in it.

What the stream enforces:

- **A stale epoch cannot open a stream, and cannot resolve a commit.** A stream
  opened under a superseded epoch belongs to a process instance the server has
  already replaced. A `CommitReport` bearing one is rejected and the intent is
  left untouched — that is the whole point of the fence, since a revoked
  instance's view of what happened on disk is exactly what its replacement was
  created to stop trusting.
- **Work the server does not recognise is revoked.** A job an agent claims to be
  running that is not assigned to it, under this epoch, in a state where work is
  legitimately held, is a survivor of a lost connection. The server has already
  counted that slot free, and two encodes writing one output is what the ledger
  exists to prevent.
- **A dropped connection marks the agent `Offline` and leaves its epoch alone.**
  Fencing on disconnect would kill a job running perfectly well behind a network
  fault.

`AgentTable` holds one connection per agent, newest wins, with the displaced
stream closed rather than left to linger — two live streams to one node is the
setup for handing it the same job twice and counting it once. Sends are bounded
and non-blocking, so an agent that stops reading cannot stall the loop talking
to everyone else.

**No `JobAssignment` is ever sent**, because there is no dispatch loop to decide
one. This is the half that has to be right before work can be handed out.

#### `CommitIntentRepo::live_for_job`

Added because the session only ever knows a job id, while `get` takes an
*intent* id. Passing one to the other answers "no live intent" for every job —
which reads as a correct refusal right up until a legitimate commit is refused
too. Two tests that appeared to pass were passing for that reason; the positive
cases are what caught it.
