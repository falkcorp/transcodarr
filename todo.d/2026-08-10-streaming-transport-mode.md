- [ ] **TODO-TRANSPORT-2** Specify and build the second transport mode — gRPC
      byte streaming — which the design intended and the architecture document
      lost. The owner's stated intent was two modes chosen per node: **direct
      access** with per-node path translation, and **streaming**, where the
      server sends the source bytes, the agent saves them locally, converts, and
      streams the result back, so the node needs to know nothing about the
      server's storage. Only the first exists. Verified 2026-08-10 that
      `distributed-architecture.md` contains **no** mention of upload, download,
      transfer, fetch or byte ranges, and that `Requirement::MountCovers` is
      unconditional — line 123 makes an untranslatable path an ineligibility, and
      even the GPU video example at line 1644 carries `MountCovers`. The proto
      matches: `Register` and a bidirectional `Connect` stream, with no message
      carrying file content. So the code faithfully implements the spec; the spec
      is what dropped the requirement. Fix the document first, then the code.
- [ ] **TODO-TRANSPORT-2-COMMIT** Decide who performs the commit ritual in
      streaming mode, because the agent cannot. An agent that receives bytes and
      returns bytes has no path to the destination, so the nine-step ritual —
      rename original to trash, install replacement, resolve the intent — has to
      move server-side. Note this is **already half-designed under another
      name**: the architecture document specifies that if the WSL2 node fails
      `RenameProbe`, "the GPU agent becomes produce-only and a U0-local agent
      performs commits". Produce-only *is* streaming mode's agent. Reuse that
      shape rather than inventing a second one, and make `MountCovers` a
      requirement only mount-mode nodes must satisfy instead of an unconditional
      one — that is a dispatch-matching change, not merely a file-copying
      feature.
- [ ] **TODO-TRANSPORT-2-UNBLOCKS-GPU** Note when scheduling the above: streaming
      mode is what lets `windows-rtx2070` do real work without a mount. As of
      2026-08-10 all three of its SMB mounts to the server report `Unavailable`
      (`W:` bigdata\books, `X:` bigdata, `Y:` winbackup), so under mount-only
      transport that node is undispatchable no matter how well its encoder works
      — and NVENC there is confirmed working (RTX 2070 SUPER, driver 610.47,
      `hevc_nvenc` encoded a synthetic clip successfully on 2026-08-10). Either
      restore the mounts or build streaming; streaming is the one that also
      removes the node's need to know anything about server paths.
- [ ] **TODO-WIN-SESSION** Run the Windows preflight in the *same logon session
      the agent will run in*, and record which context that is. Windows drive
      mappings are per-user **and per-logon-session**, and an elevated session
      gets a separate set from the interactive one (UAC linked logons).
      Demonstrated 2026-08-10: over SSH the session is `jfg\jdfalk`, `ELEVATED`,
      `net use` shows `W:`/`X:`/`Y:` as `Unavailable`, a direct UNC to
      `\\172.16.2.30\bigdata` returns "Access is denied", and `cmdkey /list`
      shows no stored credential for that host. The same mappings are fine in
      the owner's interactive desktop session.
      **The consequence for this project:** "the mounts work once the software
      starts" holds only if the agent runs in the interactive user session. A
      Windows **service** runs in session 0 with no mapped drives and usually no
      network credentials, and anything launched over SSH gets the separate
      elevated session proven above. So the launch mechanism is a correctness
      requirement, not an ops detail.
      This also invalidates the obvious way to run Phase 0 there: a `RenameProbe`
      executed over SSH-as-admin tests a context the agent never uses, so a pass
      or a fail would both prove nothing about the real one. Decide how the agent
      is launched first, then preflight inside that context. Prefer **UNC paths
      over drive letters** in the mount table regardless — a drive letter is a
      per-session alias, while a UNC path is not, though it still needs
      credentials in whatever session resolves it.
