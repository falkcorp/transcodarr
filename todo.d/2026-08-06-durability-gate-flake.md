- [ ] **TODO-DURABILITY** Fix the flaky `DurabilityTooSlow` gate — `main` does
      not pass `cargo test` on the Mac, and it fails nondeterministically. Two
      tests failed on one run (`foreign_keys_are_enforced_not_merely_declared`,
      p99 109,934 us; `only_one_live_commit_intent_per_final_path`, p99 111,621
      us) and a different one on the next (`only_one_open_job_per_file`). All
      panic identically on an unwrapped `DurabilityTooSlow { limit_us: 100000 }`
      at `crates/transcodarr-store/src/db.rs:244`. The durability probe runs on
      every DB open, so a wall-clock threshold gates every test that opens a
      database, and macOS `/var/folders` fsync p99 sits ~10% over the limit under
      any load. This matters beyond the noise: a suite that fails randomly trains
      everyone to re-run instead of read, the M1 "green triple" exit criterion is
      not actually being met locally, and Linux CI runners hide it. Do **not**
      simply raise the constant — 100 ms is a deliberate production figure and
      the ZFS pool is latency-bound. Prefer making the limit an open-time
      parameter with a production default, so the check stays present everywhere
      rather than absent where most code runs.
- [ ] **TODO-PIPEFAIL** Stop piped verification commands reporting success for a
      failed run. `cargo test --workspace 2>&1 | tail -30` returned **exit 0**
      while two tests failed, because a pipeline's status is the last command's
      and `tail` always succeeds. This nearly buried TODO-DURABILITY. It is the
      same shape as the documented "clippy fails open" trap: verify what the
      command concluded, not that the shell returned 0. Audit the verification
      commands in the handoff, `CLAUDE.md` and the workflows for piped
      invocations, and require `set -o pipefail` (or no pipe at all).
