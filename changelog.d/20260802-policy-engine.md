### Added

#### `transcodarr-core::policy` — the rules engine, and `Default Space Saver`

An ordered list of typed `when`/`then` rules, evaluated first-match-wins.
Deliberately not a DSL and deliberately not a node graph: this is the part of
Tdarr that became unreviewable, and the point is that a policy diffs cleanly in
a pull request and can be re-run over stored facts to show exactly which
decisions would change.

`evaluate` is a pure function of `(FileFacts, Policy)` — no clock, no
filesystem, no randomness — so a decision is reproducible, and
`evaluate_explained` can report *why* rather than only *what*.

Audio and video are decided independently. An HDR film with a TrueHD track still
has useful audio work; coupling the stages would strand it. Hard exclusions (DV
profile 7, object audio) are built in rather than expressed as rules, because a
policy edit must not be able to override them.

`Default Space Saver` is the built-in policy, meant to be useful with no
configuration at all. Notably its AV1 rule offers **only** libx265: Turing
cannot decode AV1, so listing NVENC would plan a job that could only fail.

Policy never encodes "not the GPU box". It names a decoder triple, and
capability matching does the rest — so an AV1 job simply fails to match the
Turing node before dispatch, instead of failing at runtime with exit 69 and a
truncated file.
