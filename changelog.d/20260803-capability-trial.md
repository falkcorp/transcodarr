### Added

#### Trial-decode classification — Phase 5's core

An encoder list is a claim, not a capability. `ffmpeg -decoders` on a Turing card
advertises AV1 and 10-bit H.264 support that does not work, and the two fail
differently — which is exactly why one boolean per codec is not enough:

- **AV1 fails hard**: exit 69, roughly a kilobyte of output. Loud, easy to catch.
- **10-bit H.264 fails soft**: ffmpeg exits **0**, having silently decoded on the
  CPU. The output is fine. What is wrong is that the scheduler now believes this
  card takes Hi10 work at GPU speed, and queues accordingly.

The soft case is the dangerous one, and it is why `VerifiedSoftFallback` is a
distinct verdict rather than folded into "works" — a trial that only checks the
exit code cannot tell the two apart. Detection matches ffmpeg's fallback
messages in stderr case-insensitively, since the wording varies across builds,
while ordinary progress chatter must not demote a working decoder.

Exit 0 with under 8 KiB written is also a failure: the AV1 path lands there when
ffmpeg is generous about its exit code, and reporting it as success would
advertise a decoder that produces a kilobyte of rubbish.

`classify` is a pure function of the trial outcome, so the measured Turing
behaviours are testable as fixtures without a GPU present.
