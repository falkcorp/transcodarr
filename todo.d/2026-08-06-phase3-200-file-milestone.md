- [ ] **TODO-P3-MILESTONE** Run the Phase 3 200-file milestone with a
      `file_stream` diff harness — the last genuinely outstanding Phase 3 item
      (architecture document line 2634, proof 2): 200 real files transcoded end
      to end on U1 with byte-exact track preservation verified by diffing input
      and output `file_stream` rows. Ten files have run and the diff harness does
      not exist. Those ten do not count: they executed on the pre-#41 binary, so
      their validations compared header duration to header duration — consistent
      and therefore safe, but not the intended last-packet-PTS guard. The other
      two Phase 3 items the handoff still lists are done and should not be
      carried forward (see TODO-HANDOFF). The diff must catch the production
      rules that were expensive to learn: every audio **and** subtitle stream
      preserved, because a bare `-c:a eac3` silently drops all but the default
      track; bit depth preserved and never upconverted from 8-bit; HDR and Dolby
      Vision video never re-encoded; lossless (TrueHD/DTS/FLAC/PCM/MLP) and Opus
      converted to EAC3 640k while aac/ac3/eac3/mp3 are left alone. Size is never
      an accept criterion — a truncated file is always smaller — so duration
      compares against last-packet PTS, not the container header, and the
      duration gate runs before the size gate.
