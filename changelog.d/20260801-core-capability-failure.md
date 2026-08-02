### Added

#### `transcodarr-core::capability` — declared requirements, matched before dispatch

The fix for the failure mode that motivated the project. A job declares what it
needs; an agent advertises what it has; `satisfies` matches them and returns the
*specific* unmet requirement with a reason. Work no node can run is visible
before it is dispatched, instead of being queued and retried forever.

`DecoderStatus` is deliberately four-valued rather than a boolean.
**`VerifiedSoftFallback` does not satisfy a hardware decode requirement** — a
soft fallback is a decode that *succeeded* without using the hardware, which is
exactly what Turing NVDEC does with 10-bit H.264. Treating it as support hands
Hi10 files to the GPU node to crawl through on one core. `Untested` does not
satisfy one either: the absence of a trial is not evidence of capability.

`satisfies` lives in core so the server and the agent link the same bytes, which
is what makes agent-side re-validation a real bug detector rather than a second
implementation that can drift.

#### `transcodarr-core::failure` — classification that decides what happens next

Failures split three ways: `Transient`, `CapabilityDrift`, `Permanent`. Only
capability drift excludes an agent, and only non-transient failures may
dead-letter.

That distinction is the whole point. NVENC session exhaustion is transient and
gets its own code, because reading it as "this card cannot encode" would exclude
the only GPU node and take all video work offline. Unrecognised failures default
to `Transient`/`Unknown` and retry — a wrong guess there costs one retry, while
guessing `Permanent` throws away work that would have succeeded.
