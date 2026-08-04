### Added

#### `transcodarr-proto` — the wire contract and the handshake rules

`proto/transcodarr/v1/agent.proto` is checked in as the agreement between
server and agent, ahead of codegen and deliberately so: it is worth reviewing as
a contract before generated code depends on it, and wiring `tonic-build` means
adding `protoc` to CI — a build-environment change that belongs with the commit
that actually needs a transport.

What is implemented now is the half a schema cannot express:

- **The version gate runs at `Register`.** An agent too old to be trusted is
  turned away while it is still asking permission, not discovered halfway
  through a commit where every remaining option is bad. An agent *newer* than
  the server is refused too — it may send fields this build cannot interpret,
  and a server guessing at one is how a fencing epoch gets silently ignored. An
  unparseable version is refused rather than assumed new enough, since assuming
  makes the gate bypassable by sending garbage.
- **`FencingEpoch` bumps only on a new process instance.** A stream reconnect
  resumes the existing epoch (flaw C9). This is the counterintuitive one:
  bumping on reconnect looks safer and is the opposite — every network blip
  would invalidate work still running perfectly well, and the agent would come
  back to find its own in-flight job fenced off.

`CommitPhase` is a separate type from the agent's `IntentPhase`. Collapsing them
would let an unrecognised wire value become a valid domain state by assignment;
parsing accepts either capitalisation, because the proto names and the stored
SQL values differ in case and both appear in practice.
