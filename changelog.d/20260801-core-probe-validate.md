### Added

#### `transcodarr-core::probe` — typed ffprobe output

`parse_ffprobe_json` turns ffprobe's loose JSON into `MediaProbe` and
`StreamInfo` once, so nothing downstream re-parses `"1920"` or guesses whether
`bits_per_raw_sample` was present. Malformed JSON is an error; *missing fields
are not*, because ffprobe legitimately omits `duration` on some containers and
`bits_per_raw_sample` on most.

`bit_depth_of` resolves depth from the sample tag, falling back to the pixel
format. Unknown resolves to 8-bit — the conservative direction, since guessing
8 can never cause an upconvert.

#### `transcodarr-core::validate` — ordered output gates

**Size is not an accept criterion.** The measured Turing AV1/NVDEC failure
produces ffmpeg exit 69 and a ~1 KB output, and a truncated file is always
*smaller* — so a size-first gate accepts exactly the outputs that destroyed the
media it was meant to shrink.

Gates run `ExitCode → Probe → Duration → Streams → Size` and the first failure
is terminal. `ValidationReport::gates_run` records what actually executed, which
is how the ordering is proven rather than asserted: the truncation test checks
that `Size` was never reached, and a companion test demonstrates that the same
file *would* have passed a size-first check.

Duration tolerance is asymmetric and absolutely capped. A percentage-only rule
permits a 40-minute loss on a 3-hour film; there is a test for a 30s loss that
a 1% rule would have allowed through.

The `Streams` gate catches the classic `-c:a eac3` mistake, where every audio
track but the default is silently dropped and the file still plays.
`SizePolicy::MayGrow` exists for audio stages, since Opus → EAC3 640k
legitimately grows a file and rejecting it would strand the video stage meant to
follow.

### Changed

#### `Cargo.lock` is now committed

`.gitignore` excluded it. transcodarr ships a binary, so the lockfile belongs in
version control — reproducible builds depend on it, and the architecture
document already assumed it was tracked.
