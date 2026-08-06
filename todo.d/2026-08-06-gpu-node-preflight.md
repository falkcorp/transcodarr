- [ ] **TODO-GPU-PREFLIGHT** Run `transcodarr admin diagnose --preflight` on
      `windows-rtx2070` and record the result in `PHASE0-RESULTS.md` beside U0 and
      U1. This is the one piece of Phase 0 never executed, and it is an
      architecture decision rather than a checkbox: architecture document line
      2577 is explicit that if the WSL2 node fails `RenameProbe`, "the
      architecture changes **here, not later** — the GPU agent becomes
      produce-only and a U0-local agent performs commits. Discovering that after
      the dispatcher exists costs weeks." The dispatcher now exists, so this is
      already later than the design wanted, and Phase 5 is entirely about the GPU
      class — this is the last cheap moment. What hangs on the answer:
      `commit_eligible` requires **every** mount to have passed the rename probe,
      not merely one, and `RP_UNTESTED` grants nothing, so an unrun probe means
      the GPU node cannot commit at all and the produce/commit split has to be
      designed in rather than bolted on. Reaching the node may need the owner —
      the access path is not documented the way SSH to U0 and U1 is. If it cannot
      be reached unattended, say so and design Phase 5 under the explicit
      assumption that the GPU agent may be produce-only rather than assuming it
      can commit.
