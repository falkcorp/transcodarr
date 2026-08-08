<!-- file: changelog.d/20260807-durability-probe-flake.md -->
<!-- version: 1.0.0 -->
<!-- guid: f930db25-aaaa-46c7-b326-a8ac35b75012 -->
<!-- last-edited: 2026-08-07 -->

### Fixed

#### `cargo test` failed randomly on macOS, and never on the same test twice

`main` did not pass its own M1 exit criterion locally. Two tests failed on one
run and a different one on the next, always with an unwrapped
`DurabilityTooSlow { limit_us: 100000 }` — the startup fsync probe measuring a
p99 of ~110,000 µs in macOS `/var/folders`. The probe runs on **every** `Db::open`,
so a wall-clock threshold silently gated every test that opens a database, and
which one lost the race was down to machine load.

The cause was not the threshold. `Db::open_unchecked` already existed for
exactly this, documented "for tests and for in-memory use, where measuring fsync
latency measures nothing", and **24 test call sites across the workspace already
used it**. The only holdouts were the five inside `db.rs`'s own test module,
which still called `Db::open`. Those five are now consistent with the rest.
Production is untouched: `Runtime::open` still probes, and `FSYNC_ABORT_US`
still refuses a pool at 100 ms.

Suite time for `transcodarr-store --lib` dropped 6.8 s → 2.4 s, because 200
fsyncs no longer run per test database.

### Added

#### The durability probe finally has a test

Switching the five call sites alone would have left the probe with **zero**
coverage — nothing asserted `FSYNC_ABORT_US` or `DurabilityTooSlow` anywhere.
The intermittent failures were, perversely, the only thing exercising it, and
they were about to be silenced. That is the shape this repository keeps finding:
a guard nobody had watched succeed or fail on purpose.

The limit is now injectable (`open_inner` takes `Option<u128>` rather than a
`bool`), which makes the guard testable without touching hardware. No filesystem
can be made to fsync reliably *slower* than a fixed limit — that is precisely
what made the old failures nondeterministic — but every filesystem is slower
than zero and none is slower than `u128::MAX`. Moving the ceiling instead of the
disk turns an untestable guard into two deterministic assertions.

They are a pair on purpose: the refusal alone would still pass if `open_inner`
had been broken to reject everything, so the acceptance case is what makes the
refusal mean something. Verified by sabotage, per the standing rule that a check
which cannot fail is decoration — neutering the comparison to `if false && …`
turns the refusal test red while the acceptance test stays green, which is the
asymmetry that proves neither is passing vacuously.

Workspace suite: 515 → **517 passing**, and five consecutive runs of the store
tests were clean where two consecutive runs previously failed differently.

### Changed

#### Two traps recorded in the handoff

Both bit during this fix. A piped verification command reports the *pipe's* exit
code: `cargo test --workspace 2>&1 | tail -30` returned **0** while two tests
failed, because a pipeline's status is the last command's and `tail` always
succeeds. That nearly buried the defect above. It is the same shape as the
existing "clippy fails open" trap — the command did not conclude what its exit
code claimed — and `grep` inverts the hazard, exiting 1 when it selects nothing,
so filtering warnings out of a *passing* command can make it look failed.

The repository itself was already clean here, which is worth recording so nobody
re-audits it: all three custom workflows `set -euo pipefail`, and no workflow
pipes a `cargo` invocation. The habit was the defect, not the configuration.

Second, `cargo fmt -- --check` emits ~55 KB of warnings to stderr on a perfectly
clean tree, because `rustfmt.toml` carries nightly-only options that stable
rustfmt reports one per line. Read its exit code rather than scanning the output
for trouble.
