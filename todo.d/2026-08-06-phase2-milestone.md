- [ ] **TODO-P2-MILESTONE** Close out the Phase 2 milestone, now that the probe
      run has finished — `awaiting probe 0` on all three libraries. Two
      assertions remain, one command each. The `admin summary` decision/GiB
      breakdown is already captured: anime 17,825 files / 16,083.0 GiB with 6,916
      Pending jobs; movies 2,432 / 37,232.5 GiB with 803; tv 29,343 / 29,498.8
      GiB with 15,388 — 49,600 files, 82,814 GiB and **23,107 open Pending
      jobs**. Record that in the milestone. Then run `admin evaluate --force` for
      the "re-derive every decision with zero filesystem I/O" claim, and confirm
      the decisions come out identical to the stored ones rather than treating
      the absence of an error as proof. Note the scale this reveals: 23,107
      queued jobs against a fleet that has run exactly one job end to end, and
      nearly 3.5x the depth live Tdarr is currently carrying. `admin config
      validate --diff` stays deferred to Phase 6 — no configuration format exists
      to validate, and inventing one now means guessing at settings Phases 4-6
      define.
