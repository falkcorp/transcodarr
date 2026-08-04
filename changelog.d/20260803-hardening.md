### Added

#### Retry policy, dead-lettering, and agent quarantine

The failure this prevents is a loop that looks like progress. A job that fails,
retries immediately, fails again and repeats saturates the fleet while
completing nothing — and because every attempt *starts* successfully, the queue
looks busy throughout.

- **A permanent failure is never retried.** Re-running ffmpeg over a file with
  no video stream produces the same error, more slowly, while holding a slot.
  `CapabilityDrift` is deliberately *not* permanent: the job is fine, the
  server's model of that agent is wrong, so it retries elsewhere.
- **Retries back off and stop.** Exponential from 30s, capped at an hour —
  beyond that a job neither runs nor shows up as needing attention. Exhausted
  retries **dead-letter rather than fail**, because a dead-lettered job is
  retained with its history and never auto-retried: "we gave up and here is why"
  rather than "we forgot".
- **An agent that fails everything is quarantined** after 5 consecutive failures
  with no success (flaw B17). Continuing to feed it turns one broken machine
  into a fleet-wide outage, because every job routes to the agent with free
  slots — precisely the one finishing nothing.

Five, not three: a genuinely bad agent reaches five quickly, while three is
close enough to normal flakiness that a healthy node gets quarantined during an
unrelated storage blip. A success resets the streak completely — *consecutive*
is the operative word.

Quarantine clears only when an operator clears it. Automatic recovery on a timer
re-enters the loop that caused it: back, five more failures, out again, five
slots burned per cycle. A human clearing it means someone has looked (flaw C8).
