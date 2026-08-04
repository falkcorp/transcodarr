### Added

#### gRPC codegen, the conversion boundary, and buf contract checks

`tonic-build` now compiles `proto/transcodarr/v1/agent.proto`, with `protoc`
carried by `protoc-bin-vendored` rather than expected on `PATH`. `prost-build`
stopped bundling it at 0.11, and requiring it out-of-band would make the build
depend on how a machine happens to be provisioned. The path is handed to
`prost_build::Config` directly rather than exported as `PROTOC`, because
`std::env::set_var` is on this repository's disallowed list — it mutates
process-global state any other thread may be reading, which is why Rust 2024
makes it `unsafe`.

`build_transport(false)` is not cosmetic. The generated client would otherwise
carry an inherent `connect(dst)` constructor that collides with the client
method generated for `rpc Connect` — same name, same impl block, `E0592`.
Renaming the RPC would also resolve it and is the wrong fix: the schema is a
reviewed agreement between both ends, and the architecture document names
`Connect`.

`convert.rs` is the boundary, and every enum-like field crosses it through a
function that can refuse. proto3 gives an enum no way to say "unknown" — a
number outside the declared set decodes to the zero variant, silently. The
consequential case is `DecoderStatus`, where a soft fallback decaying to
`VerifiedOk` reintroduces the Turing Hi10 trap: ffmpeg reports success while
decoding on the CPU, and 10-bit H.264 goes to a node that crawls through it on
one core. Unknown *encoders* are the deliberate exception and are skipped
rather than refused, since an ffmpeg build lists hundreds this scheduler has no
opinion about and one unfamiliar name should not keep a working node out of the
fleet.

`buf` guards the contract without entering the build path: `buf lint`, plus a
breaking-change check against `main` — the only check that can tell a field
renumbered in a pull request from one that was always that way. Four naming
rules are excepted with the reasoning recorded in `buf.yaml`; every breaking
rule stays on.
