### Added

#### `AgentRepo`, and the fencing epoch that survives a server restart

The seventh of the eleven contracted repositories, arriving with the phase that
calls it. No migration was needed: `agent`, `agent_mount` and
`agent_capability_history` were already in the initial schema.

The reason it exists is durability of one number. `VersionGate` decides the
fencing epoch — a new process instance bumps it, a stream reconnect resumes it —
and that decision is worth nothing if the server forgets it on the way down. An
agent returning after a restart would be handed epoch 1 again while an intent
granted under the *previous* epoch 1 was still live, and the fence would pass a
grant it exists to reject. There is a test that stops the writer, reopens the
database and asserts the epoch came back.

The repository deliberately does **not** decide the epoch; it stores what it is
told. Two places computing the same monotonic counter is how they come to
disagree.

Three behaviours worth knowing:

- `instance_of` returns `agent_uid`, `boot_id` and `fencing_epoch` together,
  because they are one decision's input. Fetching the epoch and the boot id
  separately invites deciding against a boot id from before a registration and
  an epoch from after it.
- `connected_since_unix` is refreshed only when the `boot_id` changes. A
  reconnect is the same connection to an operator, and resetting it on every
  network blip would make an agent up for a week look like it just arrived —
  hiding exactly the flapping worth noticing.
- Changing status leaves the epoch alone. Going offline does not invalidate work
  already granted; fencing on disconnect would kill a job running perfectly well
  behind a dropped connection.
