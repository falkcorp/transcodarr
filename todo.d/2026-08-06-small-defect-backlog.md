- [ ] **TODO-GUIDS** Clear the small known-defect backlog in one PR — each item
      is currently a small lie in the repository. The guid
      `a1b2c3d4-e5f6-7890-abcd-ef1234567890` is shared by `.editorconfig`,
      `.github/dependabot.yml` and `.github/workflows/ci.yml`; a guid that
      identifies three files identifies none. `TODO.md` carries the placeholder
      `12345678-90ab-cdef-1234-567890abcdef` and no `last-edited:` line (overlaps
      TODO-RECONCILE — do it wherever lands first).
      `docs/design/distributed-architecture.md` still has `<new uuid>`
      placeholders at lines 169 and 214 inside example `Cargo.toml` blocks; known
      and harmless, but they have been known for a while. And
      `task-inventory.json` carries no file header because JSON cannot hold
      comments — either accept that permanently and say so where the header rule
      is stated, or add a sibling `task-inventory.header.md`, because right now
      it is an unexplained exception.
- [ ] **TODO-INVENTORY** Mark `docs/design/task-inventory.json` clearly as
      reference-only at the top of the file, or audit it. All 414 tasks were
      produced by a workflow interrupted before its verification pass, were never
      reconciled against the architecture document, and may be missing whole
      subsystems. The Phase 0-7 plan supersedes it. Its sheer size makes it look
      authoritative, which is precisely the risk — it must never be handed to
      implementer agents as-is.
- [ ] **TODO-DIRNAME** Rename the local clone from
      `~/repos/github.com/jdfalk/transcoderr` to match the repository and crate
      name `transcodarr`. Purely cosmetic and the owner's convenience. **Confirm
      before touching it** — it will break every absolute path in the handoff, in
      shell history, and in any running background job.
