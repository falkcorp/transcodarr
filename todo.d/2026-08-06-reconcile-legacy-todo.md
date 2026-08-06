- [ ] **TODO-RECONCILE** Reconcile the curated sections of `TODO.md` against the
      Phase 0-7 plan. Those sections still describe the single-binary transcoder
      that the workspace split retired, and they are stale in a way that
      misleads: a reader takes them as the project's backlog. Retire the items
      obsoleted by the split into six crates and the distributed design — "embed
      static ffmpeg build", "add preset (web-optimized)", "progress reporting
      with ETA during batch conversion", "resume capability for interrupted
      batches" and "quality comparison reports" all describe `transcodarr local`
      rather than the orchestrator. Map the survivors onto phases instead of
      leaving them loose: hardware acceleration (VAAPI, NVENC, VideoToolbox) is
      Phase 5; "parallel processing for batch operations" is already the
      dispatcher, so delete it; proptest, cargo-mutants, fuzzing, tarpaulin and
      performance regression detection are Phase 7 (see TODO-PHASE7); "verify
      metadata preservation in tests" is the Phase 3 `file_stream` diff harness;
      and "quality validation tests" are already the validation gates — confirm
      and delete. Retirements and removals are normal direct edits of `TODO.md`,
      since the fragment system is add-only. While editing, fix the file's own
      header: it carries the placeholder guid
      `12345678-90ab-cdef-1234-567890abcdef` and has no `last-edited:` line at
      all.
