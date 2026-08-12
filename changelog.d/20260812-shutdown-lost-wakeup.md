<!-- file: changelog.d/20260812-shutdown-lost-wakeup.md -->
<!-- version: 1.0.0 -->
<!-- guid: 688b4693-f004-4072-bc34-38cdfed28108 -->
<!-- last-edited: 2026-08-12 -->

### Fixed

#### The agent could park forever instead of shutting down

`Shutdown::stop` signalled waiters with `Notify::notify_waiters`, which wakes
only the tasks already parked at the instant it runs and leaves no permit for
one that arrives later. Both places that waited on it — the session loop and the
reconnect backoff — awaited `notified()` directly, with no level-triggered check
of the flag that `stop` had already set.

So whenever `stop` landed while the client was somewhere other than parked on
that await — mid-recovery, mid-register, mid-dispatch — the signal was lost. The
session loop then parked on a notification that had already been and gone, while
the inbound stream stayed open and silent. `session` never returned, so `run`
never reached the `is_stopped` check that would have ended it.

This was not a slow shutdown. It was a permanent one: a CI job sat parked on a
condvar at zero CPU for 26 hours, and the local suite reproduced it in roughly
1 run in 50. It presented as a hung `cargo test --workspace` and was twice
mistaken for a stuck runner, because a hang leaves no failing test behind to
point at.

`Shutdown::cancelled` replaces both waits. It registers as a waiter *before* it
reads the flag, and `stop` writes the flag *before* it notifies — two opposed
orderings, so no interleaving can miss both.

The regression test asserts under a `timeout`, deliberately: the bug hangs
rather than fails, and an assertion alone would reproduce the disease instead of
reporting it.
