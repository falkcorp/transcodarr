### Added

#### `transcodarr-core::facts` — the decision surface

Policy never sees a `MediaProbe`; it sees `FileFacts`, a flat summary of only
what a decision may depend on. That boundary is what makes re-evaluating ~49,600
stored files cheap — facts are persisted once, and a policy change re-runs over
them without reading a byte of media.

The media-correctness rules are encoded here as behaviour with tests, not as
comments:

- Lossless audio (TrueHD/DTS/FLAC/PCM/MLP) **and Opus** are flagged for
  conversion to EAC3; `aac`/`ac3`/`eac3`/`mp3` are left alone.
- HDR and Dolby Vision video is never encodable. Tone mapping is lossy and
  irreversible.
- Dolby Vision profile 7 and object audio (Atmos, DTS:X) are excluded from *all*
  work, not merely from video encoding — ffmpeg cannot round-trip dual-layer DV,
  and a channel-based re-encode destroys object audio. An unknown DV profile
  vetoes too: not knowing is not permission.

`content_sig` hashes only decision-relevant facts, deliberately excluding size —
a file whose size changed but whose streams did not is the same decision. A plan
carries the signature it was built from so a stale plan cannot be applied to a
file that has since changed.

#### `transcodarr-core::job` — a checked state machine

`JobState::can_transition` makes an impossible edge a rejected write rather than
a row that contradicts its own history. Terminal rows are immutable; an operator
retry inserts a new job rather than reanimating the old one.

Two properties carry their reasoning in tests. `Eligible ⇄ Blocked` goes both
ways, because a new agent unblocks work and a departing one blocks it. And
capacity is released when a job *leaves* the admitted set rather than when it
terminates — a job moving to `Retrying` has stopped using its slot, and holding
the grant until it eventually terminates leaks capacity until the fleet
deadlocks.
