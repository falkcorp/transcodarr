<!-- file: changelog.d/20260816-gpu-node-can-take-audio-work.md -->
<!-- version: 1.0.0 -->
<!-- guid: 2e84c5b1-73f0-49da-96a7-508b1fc3e627 -->
<!-- last-edited: 2026-08-16 -->

### Fixed

#### An audio job asks for `AgentClass::Audio`, not `AgentClass::Cpu`

An audio pass is `-c:v copy`. It uses no video encoder, and `survey.rs` offers
`AgentClass::Audio` unconditionally for exactly that reason — its comment says
so. But `next_job` attached `AgentClass::Cpu` to every audio job, and an agent
offers `Cpu` only when it has libx264 or libx265. Audio work could therefore
land only on nodes with a software **video** encoder, which is unrelated to
anything an audio pass does.

`windows-rtx2070` has `hevc_nvenc` and no libx264. It advertised `[Audio, Gpu]`
and could never be handed a single audio job. `AgentClass::Audio` was generated
by every agent and required by nothing — a class nobody asks for is the shape
this bug made.

Placement is still gated by `Requirement::Encoder` and `Requirement::Muxer`,
which is right: those are what an audio pass actually needs, and a node without
`eac3` is still refused.

#### The standalone agent binary installs a tracing subscriber

`tracing_subscriber_init()` was an empty function. Its comment said this avoided
"pulling a subscriber crate into the agent's dependencies" to keep the binary
small; what it achieved was an agent that **discards every log event**, on the
one platform the binary exists for. The Windows node registered, reconnected in
a loop, and printed nothing to say so. The same library logs normally through
the `transcodarr` CLI, because that binary installs a subscriber — which is also
why this went unnoticed.

The saving was imaginary: `tracing-subscriber` is already in the workspace and
already linked by the CLI that uses this same library. `TRANSCODARR_LOG` then
`RUST_LOG`, defaulting to `info`, matching the CLI.
