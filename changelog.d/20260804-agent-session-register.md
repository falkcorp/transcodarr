### Added

#### `AgentSession`: registration served over gRPC

The server side of the handshake, and the only place a `fencing_epoch` is
issued. Four gates run in order, cheapest first: token, then version, then the
capability conversion, then fencing.

**A rejection changes nothing in the database.** It is a clean response carrying
a reason an operator can act on — not an error, and not a partial write. An
agent that was refused is left exactly as it was, or being refused becomes a way
to overwrite a healthy row. There is a test for that specifically.

Two rules from the design are now enforced end to end rather than only in unit
tests:

- A stream reconnect resumes its epoch; a new process instance takes a new one.
  A reinstall — same operator name, new `agent_uid` — also takes a new epoch, so
  it cannot inherit a work area that is not its own.
- `commit_eligible` requires **every** reported mount to have passed the Phase 0
  rename probe. A node that renames atomically on one pool and not another must
  not be trusted to install everywhere, and `RP_UNTESTED` grants nothing:
  absence of a trial is not evidence of success.

Ten tests run against a real `tonic` server on a loopback port, dialled with the
generated client. That is worth the cost over in-process calls: the unit tests
already cover the gate and the repository, but not prost's encoding sitting
between them — an enum decoding to its zero variant, an epoch that does not
survive the trip. Those only appear when something actually serialises.

`Connect` returns `Unimplemented` and says so. A stream that accepted
assignments with no dispatch loop behind it would hand out work nothing is
accounting for; an explicit refusal leaves an agent connected, registered and
idle, which is recoverable.
