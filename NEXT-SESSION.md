<!-- file: NEXT-SESSION.md -->
<!-- version: 3.14.0 -->
<!-- guid: c8f01a35-6d47-42b9-a0e5-317b6924cf80 -->
<!-- last-edited: 2026-08-18 -->

# Goal: a real transcode on the GPU node, both transports, audio and video

**Audio over `TM_STREAM` is done — 2026-08-16.** `windows-rtx2070`
(172.16.3.22) fetched a 10s FLAC source from the Mac, transcoded it to EAC3,
pushed it back, and the server installed it: duration exactly `10.000000`,
original retained in trash, the node's work area swept clean.

What is left of the goal: **video with `hevc_nvenc`**, and **mount mode**, which
still needs hands on the box.

## Done 2026-08-18: `admin jobs cancel` (PR #91, `623419d`)

```
transcodarr admin jobs cancel <id> [--reason <text>] [--force]
```

The escape hatch `REQ-REFRESH` item 4 asked for. Before this, a job that had
become permanently unsatisfiable could not be cleared at all — `admin` had no
cancel, reset or requeue, so the only recourse was editing SQLite by hand while
`explain` went on naming a requirement no installed code can emit. **Items 1–3
of `REQ-REFRESH` are still open**, so a stale job must still be *cancelled*
rather than *refreshed*. The job id comes from `admin explain <path>`.

**`Cancelled` was a fully modelled state that nothing ever wrote** — in
`JobState`, admitted from anywhere non-terminal by `can_transition`, counted by
`metrics`, stamped with `finished_unix` by `transition_op`. The fifth instance
of the documented-tested-and-never-called pattern this file already tracks. The
detection method in that section found it in one grep.

**The reusable finding: three things that looked like they needed handling were
already covered, and adding any of them would have been a no-op that read as
load-bearing.** `cancel_job`'s doc comment records each so the next person does
not "fix" the apparent gap:

- **Capacity.** `orchestrator.rs:12` — *"Rebuild the ledger, do not maintain
  it."* `tick()` calls `rebuild_capacity` every pass and `rebuild` skips any
  state where `!occupies_slot`, so the slot frees itself. A CLI-side release
  would touch that process's in-memory ledger, never the running server's.
  **This is what makes a separate-process cancel safe at all** — with an
  incrementally maintained ledger it would have been a distributed-systems
  problem.
- **The commit intent.** `sweep_stranded_intents` already resolves live intents
  whose job is terminal and not `NeedsOperator`. A cancelled job is both.
- **The agent.** `on_heartbeat` revokes any running job whose state is not in
  `HELD_STATES`, and `worker.rs` sweeps its work area on every exit path. So
  `--force` needed **no new protocol message**.

Two guards, both mutation-tested — removing either turns exactly one test red:

- A job an agent holds needs `--force`. The stated need is never in flight.
- **A `Committing` job is refused even under `--force`.** That is the window
  between the ritual's two renames — the ambiguity `NeedsOperator` exists to
  record. Cancelling there races a rename on real files, and the intent sweep
  would then free a destination whose on-disk state nobody has determined.

**`--force` stops the install, not ffmpeg.** `worker.rs:351` checks the revoke
*after* the encode finishes, so a forced cancel lets the remaining encode run
and then discards it. Safe, and not instant. Worth knowing before someone files
it as a bug.

613 tests, up from 607. Clippy clean.

## Done 2026-08-12

### The hung `cargo test --workspace` — root-caused, fixed, merged (`c75a72c`)

It was never infrastructure. Last night's run was still alive after **26 hours**
at 0% CPU; sampling it named the frame outright.

`Shutdown::stop` signalled with `Notify::notify_waiters`, which wakes only tasks
**already parked** at that instant and stores no permit. Both waits — the
session loop and the reconnect backoff — awaited `notified()` directly with no
level-triggered read of the flag `stop` had already set. When `stop` landed
while the client was anywhere else (mid-recovery, mid-register, mid-dispatch)
the wakeup was lost, and the session loop parked forever on a notification that
had already been and gone.

`Shutdown::cancelled` now registers as a waiter *before* reading the flag, and
`stop` writes the flag *before* notifying — two opposed orderings, so no
interleaving misses both.

| measurement | result |
|---|---|
| the one test ×260, pre-fix branch | 5 hangs |
| same test ×260, pre-merge `main` | 0 (latent, not absent) |
| **with fix**, whole `connect_client` binary ×150 | **0** |
| CI `Test Rust` | 2m00s, was 20+ min |

**Do not repeat the mistaken lesson:** the stated suspects (the
`FetchSource`/`PushOutput` stubs) were inert. And a single-run bisect against
`main` would have wrongly convicted the branch — the defect is latent there too.
For an intermittent fault, measure a *rate*, and sample a live hung process
rather than reasoning about it.

### The agent runs on Windows — cross-compiled and executed on the GPU node

`x86_64-pc-windows-gnu` builds clean after `brew install mingw-w64`. `ring` is
in the tree (via `tonic`'s `tls` → `rustls`) and cross-built without trouble, so
no workspace feature surgery was needed. 11 MB PE32+ binary, copied to
`C:\Users\jdfalk\transcodarr-agent.exe`, and its capability survey **ran on the
box**:

```text
platform  windows      classes  audio, gpu      transport  stream
encoders  hevc_nvenc, eac3, ac3, aac            mounts     none
```

Note what is *absent*: `av1_nvenc`. transcodarr's trial-probe correctly refused
the codec that ffmpeg falsely advertises. The trap held.

Rebuild with:

```sh
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
cargo build --release --target x86_64-pc-windows-gnu -p transcodarr-agent --bin transcodarr-agent
```

### There are two ffmpeg builds on that node — pass the right one

Note `jdfal`, not `jdfalk`, in both paths.

| path | build | use it? |
|---|---|---|
| `C:\Users\jdfal\ffgpl\ffmpeg-master-latest-win64-gpl\bin\ffmpeg.exe` | BtbN **GPL** `N-126175-g0056dd32fd-20260816`, has libx264/libx265 + nvenc | **yes — always** |
| `C:\Users\jdfal\bin\ffmpeg.exe` | BtbN **LGPL**, `--disable-libx264 --disable-libx265` | no (kept only so nothing that referenced it breaks) |

This matters more than it looks. Point `--ffmpeg` at the LGPL one and the agent
can generate almost no trial samples, so nearly every decoder triple comes back
`Untested`, so the node is offered no video work — and nothing in the output
says why beyond a single `WARN`. The `survey` subcommand now prints the decoder
table, so check it there first.

```sh
FF='C:\Users\jdfal\ffgpl\ffmpeg-master-latest-win64-gpl\bin'
ssh windows-gpu "C:\\Users\\jdfal\\bin\\transcodarr-agent.exe survey \
  --work-dir C:\\Users\\jdfal\\agentwork \
  --ffmpeg $FF\\ffmpeg.exe --ffprobe $FF\\ffprobe.exe --transport stream"
```

An earlier handoff said "that box's ffmpeg has no libx264" as though it were a
property of the machine. It was a property of the build that happened to be
installed, and there are now two.

### Streaming groundwork + the spec (PR #80)

`transcodarr-server::transfer` — chunked read, and a receiving `Sink` that
checks `offset` and verifies a whole-file blake3 before the bytes are used.
`distributed-architecture.md` now documents both transports; it previously had
zero mentions of `TransportMode`, `FetchSource`, `PushOutput`, upload, download
or byte ranges.

## What is left, in order

1. ~~**Wire `fetch_source`.**~~ **Done** (PR #81). It serves a held job's source
   through `transfer::source_stream`, behind three gates: epoch against the
   registry, the job row naming this caller at this epoch, and `HELD_STATES`.
   The third is not redundant — `transition_op` changes only `state` and leaves
   `agent_id`/`fencing_epoch` in place, so a failed job still names its last
   holder, and only a new `boot_id` bumps an epoch. Without it the agent that
   just failed a job could pull its source forever.
2. ~~**Server-side install for `push_output`.**~~ **Done** (PR #82). It stages
   with `transfer::Sink` and runs the commit ritual server-side, reusing
   `judge_commit` and the `resolve_op` path the mount agent already drives.

   **The instruction that used to be here was wrong, and is worth remembering
   as a class of error.** It said to follow `runner.rs:281–350` *literally*,
   including `grant_op` before the ritual. That would have collided on the
   primary key: the orchestrator already grants the intent at dispatch
   (`orchestrator.rs:410–419`, same `{job}:{attempt}` id), and `runner.rs`
   grants only because it is `LocalRunner` — "no dispatcher, no agents, no
   gRPC" — and nothing dispatched to it. One grep at the other call site
   answered in a minute what an argument from first principles would have got
   wrong. **Read the existing call sites before mirroring a ritual.**

   No constructor change was needed either: the work area is opened per library
   from `library.work_dir`, exactly as `run_library` does.
3. ~~**The agent-side stream path.**~~ **Done** (PR #83 server-side argv, PR #84
   agent-side transfer). Kept below because the way this entry was wrong is the
   reusable part.

   **What this entry got wrong:** it said "under `Stream`, fetch instead of
   reading `source_path`", which assumes `source_path` and `argv` are usable by
   a streaming agent. They are not, and were not for a mount agent on another
   host either. `argv` is specified as "fully translated, agent-local"
   (`agent.proto`) and "translated per candidate agent" (`doc:2220`), and
   **nothing translated it** — the orchestrator shipped `file.canonical_path`
   and the server's own work dir verbatim. Same-host runs masked it. There was
   no coherent place to fetch *to* until the assignment itself became
   agent-local. That is PR #83:
   `core::plan::agent_job_paths` + `Capability.workarea_path`.

   **Do not reintroduce substitution tokens in `argv`.** `{{input}}` is the
   obvious design and it is ruled out twice over: `JobStarted` carries "echoed
   argv, which must equal what was sent" (`doc:889`), and
   `job_attempt.argv_json` is persisted so an operator can paste it into a shell
   on that agent and reproduce byte-for-byte (`doc:2058`, `:2358`). A
   placeholder only the agent can expand breaks both.

   **`live_intents()` under `Stream` needs no code and no branch.** It is empty
   *structurally*: `CommitRitual::commit` is the only production writer of the
   journal (`commit.rs:270/297/322` — `:536` is a test), and a streaming agent
   never runs the ritual. That is the "write down why" this entry asked for.

   ### How step 3 came out (PR #84)

   `Link` gained a second handle on the same channel plus the agent id, and
   `stamp` became a free function so the transfer RPCs and `connect()` share
   one implementation. `worker.rs` branches at the top of `execute` rather than
   at each step: only encode-and-judge is common to both transports.

   `transfer` **moved to `transcodarr-proto`.** Both ends need the same `Sink`,
   and the agent cannot depend on `transcodarr-server` (SQLite → unbuildable
   for Windows). A copy was the only alternative, and `buf.yaml` already argues
   that case about `FileChunk`.

   Every guard was mutation-tested — five in `Link`, three in the worker. The
   one worth keeping in mind: **deleting the transport branch entirely still
   left eight of nine tests green.** Only the end-to-end test caught it, and
   only because its `final_path` names a directory that does not exist on the
   test machine. Point it at a file that happens to exist locally — the obvious
   thing to do on a single-machine fixture — and a streaming agent silently
   taking the mount path passes.
4. ~~**Prove it locally before Windows.**~~ **Done 2026-08-16 (PR #86).** Server
   and agent both on the Mac, `--transport stream`, a real 6s FLAC→EAC3 pass:
   fetched 101,377 bytes, encoded, pushed 512,116 back, server installed it.
   Library file replaced, duration exactly 6.000000s, original retained in
   trash, both work areas swept.

   **It did not work the first time, and what stopped it is the lesson of this
   whole file.** The agent registered, and every tick logged
   `dispatched=0 blocked=1` forever. `commit_eligible` required
   `!mounts.is_empty()` — and a `TM_STREAM` agent advertises **no mounts by
   design**, which `agent.proto` says outright. So it was permanently
   ineligible, and `Dispatcher::place` skips an ineligible agent as a candidate
   outright. **Every streaming agent was undispatchable, and the entire
   transport built over PRs #81–#84 was unreachable in production.**

   573 tests passed over it. They could not have caught it: every dispatch test
   registers agents through a harness that sets `commit_eligible: true`
   directly, bypassing the rule. **A fixture that asserts the precondition it
   exists to exercise cannot fail on it.** Check what your harness hardcodes
   before trusting what it proves.

   The `-progress` sink leak came out of the same run — `<temp>.progress`, one
   per job forever, pre-existing in mount mode and only visible because a
   streaming work area is swept and that was what was left sitting in it.

5. **`windows-rtx2070` stream mode** — **done. Audio 2026-08-16 (PR #87),
   video 2026-08-16 (PR #89).**

   Both video paths now run end to end, server on the Mac, `--transport
   stream`, verified on **frame counts rather than file size** — a decode that
   stops early still writes a smaller, structurally valid file, so size cannot
   tell success from truncation:

   | path | job | source → output | frames | encoder in the bitstream |
   |---|---|---|---|---|
   | GPU (this node) | `VideoGpu` | h264 `High` 20.2 Mbps → hevc `Main` 2.4 Mbps | 627 → 627 | `Lavc63.8.101 hevc_nvenc` |
   | CPU (Mac) | `VideoCpu` | av1 `Main` 1.2 Mbps → hevc `Main` 0.7 Mbps | 240 → 240 | `Lavc62.28.102 libx265` |

   The stream-level `encoder` tag is the witness worth using: libx265 stamps
   its build into an SEI and NVENC does not, so it corroborates the logs
   instead of restating them. Durations matched to the microsecond and the
   originals landed in trash.

   **Every run above used a database created by the binary under test.** That
   is the only configuration verified. A job created by an *earlier* binary
   keeps whatever requirements it was created with and nothing refreshes them —
   see the `REQ-REFRESH` trap at the end of this document. So start from a
   fresh database when re-verifying, and treat any pre-2026-08-17 database as
   holding jobs that block permanently on a `kind: Nvdec` requirement no
   current code can emit. That block is the filed bug, not a new one.

   **Both artifacts were stale when I first ran this, and both lied
   convincingly.** The deployed `transcodarr-agent.exe` predated the
   trial-decode commit by two hours, and `target/release/transcodarr`
   predated the policy change by one — so the first CPU run printed a
   requirement carrying `profile: "Main"` and looked like a logic bug in code
   that was in fact correct. 606 tests said nothing about either, because
   tests do not know which binary you deployed. **Check artifact mtimes
   against the commit before believing a run.**

   ### What blocked video — resolved 2026-08-16

   Not the transport. A `VideoGpu` job requires a `Decoder(..., Nvdec)` triple
   at `VerifiedOk`, and `survey.rs` shipped `decoders: Vec::new()`, so the
   requirement could never be met by anyone. Both halves of the chosen
   "install ffmpeg AND the fallback list" plan are now done, and the node
   reports a full decoder table (`feat/trial-decodes`).

   Measured verdicts on that card — worth keeping, because three of them
   contradict what a simpler model would predict:

   | triple | verdict |
   |---|---|
   | h264 `High` / `Main` / `Constrained Baseline`, 8-bit | hardware |
   | h264 `High 4:2:2`, 8-bit | **silent CPU fallback** |
   | h264 `High 10`, 10-bit | **silent CPU fallback** |
   | hevc `Main`, 8-bit and `Main 10`, 10-bit | hardware |
   | av1 `Main`, 8-bit | hard fail, exit 69 |
   | vp9 `Profile 0`, mpeg2video `Main`, 8-bit | hardware |

   - **10-bit HEVC works and 10-bit H.264 does not.** Any "this card can't do
     10-bit" shorthand is wrong in both directions.
   - **`High 4:2:2` fails at the same codec and depth as `High`, which works.**
     This is why `profile` cannot be dropped from `DecoderTriple` even though
     the Hi10 trap alone would seem to be a bit-depth story.
   - **av1 exit 69 is now confirmed**, not remembered. It had been asserted in
     module docs since Phase 1 against fixtures written from memory.

   Three traps for whoever touches this next:

   - **Profile strings are ffprobe's raw text and they contain spaces.**
     `High 10`, `Main 10`, `Profile 0`, `Constrained Baseline`,
     `High 4:2:2`. `docs/design/task-inventory.json` S14-005 says `High10`,
     `Main10`, `Profile0` — those match nothing, and a triple that matches
     nothing blocks the job at `capability` citing a hardware limit that does
     not exist. Verified across two ffprobe builds and real library media.
   - **The software path deliberately uses an empty profile.** See the comment
     at `policy.rs:549`. Do not "fix" the asymmetry.
   - **S14's file layout does not exist.** The spec's `capability/{matrix,
     trial,fixtures,cache}.rs` assume a `ToolRunner` trait, `ProbeError` and an
     async prober from S14-001..004 that were never built. The repo uses plain
     `Command`, in `probe_samples.rs` and `probe_caps.rs`. Follow the repo.

   Still deferred from S14: the on-disk capability cache (S14-009), so every
   agent start re-runs the trials. That costs ~1s once the clips exist, since
   `ensure` reuses them — measured 1.36s cold, 0.59s warm. Also deferred, per
   plan: `ReprobeCapabilities`, `fingerprint_watch`, `ArcSwap` swapping.

   **The deferral is cheaper than it looks, and the reason is worth writing
   down before someone re-derives it.** `run::prepare` surveys exactly once,
   before `client.run()`; the reconnect-and-backoff loop lives *inside*
   `client.run()` and reuses the capability it was handed. So the trials cost
   one survey per **process start**, not per reconnect — which matters
   precisely because this node has previously sat in a reconnect loop. Whole
   registration on it, trials included, measured ~2.5s.

   **Two bugs stood between the merged code and a working node, and neither was
   in the transport.** Both are the same shape: a thing that looked configured
   and did nothing.

   - **`AgentClass::Cpu` on every audio job.** An audio pass is `-c:v copy`, but
     an agent offers `Cpu` only when it has libx264/libx265. That ffmpeg build
     has no libx264, so the node advertised `[Audio, Gpu]` and was ineligible
     for the audio work it was built to take. `AgentClass::Audio` was generated
     by every agent and **required by no job at all** — check for that shape;
     a capability nothing asks for is usually a mismatch, not a spare.
   - **`tracing_subscriber_init()` was an empty function.** The standalone agent
     binary discarded every log event, so the node ran blind for three attempts
     while the same library logged fine under the CLI. **When a remote process
     produces no output, suspect the logger before the logic.**

   Two operational notes that cost real time:

   - **`timeout N ssh ...` does not kill the remote process.** Three orphaned
     agents accumulated and kept reconnecting, which is what produced a fleet of
     spurious registrations (epoch 44) and a `scp` that failed with the target
     file in use. `taskkill /F /IM transcodarr-agent.exe` between runs.
   - **`set VAR=value && cmd` in cmd.exe assigns `"value "`** — the space before
     `&&` is part of the value, so `RUST_LOG` silently matches nothing. Write
     `set "VAR=value" && cmd`.

   The recipe that works, from the Mac:

   ```sh
   # server must bind the LAN, not loopback
   transcodarr serve --db t.db --listen 0.0.0.0:7420 --tick-seconds 3
   ssh windows-gpu 'set "RUST_LOG=info" && C:\Users\jdfalk\transcodarr-agent.exe connect ^
     --server http://172.16.3.222:7420 --id win-rtx2070 ^
     --work-dir C:\Users\jdfalk\tc-work --transport stream ^
     --ffmpeg C:\Users\jdfal\ffgpl\ffmpeg-master-latest-win64-gpl\bin\ffmpeg.exe ^
     --ffprobe C:\Users\jdfal\ffgpl\ffmpeg-master-latest-win64-gpl\bin\ffprobe.exe'
   ```

   System `sqlite3` on the Mac is too old for this schema's `STRICT` tables
   (`malformed database schema`). Python's bundled sqlite is 3.53 and reads it
   fine — useful for `job.requirements_json`, which is what exposed the first
   bug.

6. **Mount mode last** — see below, it needs hands on the box.

## The pattern to keep checking for: documented, tested, and never called

Five times now, in the same shape — a facility that exists, whose
doc comment describes it as the answer to a real question, with **no production
caller**. Each had passing unit tests. A unit test proves a component works when
it is called; nothing in one can notice that nothing calls it.

- `CommitIntentRepo::live()` — "what the reconciler sweeps". Nothing swept.
  (PR #85)
- `tracing_subscriber_init()` — an empty function body. (PR #87)
- `AgentClass::Audio` — advertised by every agent, required by no job. (PR #87)
- `dispatch_block` — `dispatch.rs:22` says outright it is how you answer
  "nothing is running and I do not know why", and the dispatcher never wrote a
  row. `explain` then read the row and printed only `blocking_stage`, dropping
  the detail. Worse, `tick()` returned before the dispatcher ran when the fleet
  was empty or the schedule was paused, so the two conditions that stop the
  entire queue were the two with no record at all. (PR #88)
- `JobState::Cancelled` — a terminal state the whole stack already handled and
  no production code ever wrote. **This one is the proof the method works**:
  the grep below found it, and because every consumer was already built, the
  command that closed it was a guard and a `transition_op` call. (PR #91)

**How to find the next one:** grep for a public item, then grep for its callers
outside its own test module. If the only hits are the definition and its tests,
that is the shape. `findReferences` answers it in one call.

**And wiring observability is where you are most likely to break it.** PR #88's
first commit made `TickOutcome` non-default on a path that used to produce
nothing, so `run()` began logging at `info` every five seconds — ~17k lines a
day — in exactly the steady state a broken deployment sits in. It took a
two-minute run with the real binary to see; no test sits in a failure mode long
enough to notice volume.

## Mount mode needs the owner at the console

Not a software problem. Windows drive mappings are per-user **and
per-logon-session**, and SSH lands in a separate elevated one: all three SMB
mounts show `Unavailable`, UNC is denied, `cmdkey` is empty. So the agent must
run **in the interactive session** — a scheduled task set to "run only when the
user is logged on", an autologon, or a startup shortcut. That cannot be arranged
over SSH, so it is a request for the owner, not a task for the next session.

Prefer UNC paths over drive letters in the mount table regardless: a drive
letter is a per-session alias, a UNC path is not (though it still needs
credentials in whatever session resolves it).

## ~~Known gap: a stranded ledger row wedges its file permanently~~ Fixed (PR #85)

The reconciler's periodic pass now resolves live intents whose job has reached a
terminal state, and those whose job no longer exists. `resolve_op` was hardened
to `AND state = 'live'` first, as this section said it had to be.

**The predicate came out simpler than this section predicted, and the reason is
worth keeping.** The plan here was to classify by agent connectivity and lease
expiry, and to reuse the agent's `recover_one` decision table. Neither was
needed. The reconciler *already* escalates a live intent on an in-flight job, so
the only rows it could not see were those whose job had gone terminal — and a
terminal job's outcome has already been decided and recorded. There is nothing
left to adjudicate and no file to move, only a row to close.

That holds **only** because `NeedsOperator` is excluded. It is where an
ambiguous commit lands, and the live intent is what reserves the destination
while a human looks. Sweeping it would free a path whose on-disk state nobody
has determined. Do not "simplify" that carve-out away; it is what makes the rest
safe. Three mutations confirm it: dropping the sweep, dropping the
`NeedsOperator` guard, and dropping the terminal guard each fail a test.

**A comment asserting that something is swept is weaker evidence than a
caller.** `CommitIntentRepo::live()` claimed the reconciler swept it and had no
production callers for however long it had been there.

### Still open: `LocalRunner`

`admin run` has the same hole. It grants intents directly (runner.rs:301) and a
crash before `resolve_op` strands one, and it runs no orchestrator so no sweep
passes over it. Lower stakes — it is the single-machine path, and there is no
fleet to contend for the destination — but it is real.

## Traps still standing

- Turing NVDEC cannot decode AV1 (exit 69, ~1 KB truncated) or 10-bit H.264
  (silent software fallback). Use an **8-bit H.264 source**, software decode,
  NVENC encode. Gate hardware decode per codec, never globally.
- **Probe by trial, never by asking ffmpeg what it lists.** Confirmed again
  today: that ffmpeg lists `av1_nvenc` and the hardware cannot do it.
- The **LGPL** build at `C:\Users\jdfal\bin` has no libx264. The GPL build
  now installed at `C:\Users\jdfal\ffgpl\...` does. Pass the GPL one.
- ICMP is filtered on `windows-rtx2070`. `ping` failing proves nothing; test port 22.
- **The agent must `stamp()` its transfer requests.** Every RPC other than
  `Connect` has to assert its own identity; the stamp that authenticated the
  stream does not travel. Now held by `stamp_identity` and two tests in
  `stream_transport.rs` — removing the stamp fails seven of nine.
- **An in-stream error is fatal, not end-of-stream.** `transfer::source_stream`
  reports a missing source as a `Status::not_found` *inside* the stream. A
  reader that treats stream errors as termination turns a missing source into a
  zero-byte fetch — and a blake3 of nothing verifies fine. Held by
  `an_error_inside_the_stream_fails_the_fetch_rather_than_ending_it`; the agent
  uses `Streaming::message()` so the two cases are different types rather than
  two arms one can forget to write.
- **`req.attempt` is not checked against `job.attempt`.** Harmless for
  `fetch_source`, where every attempt reads the same source path, so a mislabeled
  chunk still carries correct bytes. **Not** harmless for `push_output`, which
  will key staging and the intent grant on `(job_id, attempt)` — check it there.
- **`judge_commit` is not a fence on its own.** It compares the caller's epoch
  against the *intent*, which is enough on `Connect` because that epoch arrived
  on a stream the server authenticated. On `FetchSource`/`PushOutput` the epoch
  is asserted by the caller in metadata, and a superseded instance can present
  the very epoch its own intent was granted under — passing every check that
  only compares the two. Use `require_current_epoch`, which asks the registry.
  A test caught this: the stale-epoch push installed.
- **A freshly created job is on attempt `0`, not `1`.** Seeding a test fixture
  at attempt 1 makes `push_output`'s attempt gate refuse everything, and every
  refusal test then passes while proving nothing about the gate it names. Nine
  of them did. Mutation-test refusal paths; a green refusal test is the easiest
  kind to get for free.
- The `FakeServer` in `connect_client.rs` refuses to serve bytes on purpose.
  Do not weaken it to make a streaming test pass — a fake that returned an empty
  stream would let a streaming test pass while moving no bytes.
- **`if let` inside a `for` over a requirement list asserts nothing when the
  list is empty.** The guard added with the `Software` decode requirement
  iterated `job.requirements.0` and asserted inside
  `if let Requirement::Decoder(t)`; deleting the `reqs.push` it exists to
  protect left it green. Count the matches and assert the count, then assert on
  the element. Same family as the attempt-`0` trap above: **mutation-test any
  guard whose subject might simply be absent.**
- **A job's requirements are frozen at creation and no command refreshes
  them.** So a *code* change that alters what `next_job` emits reaches new jobs
  only. `rules_version` hashes the policy config, not the code, so `needs_eval`
  never resurfaces the file (`admin evaluate` → `evaluated 0`); and
  `evaluate_one` returns `already_busy` before `next_job` runs, so forcing it
  would not help either. There is no cancel/requeue subcommand. Verified on a
  pre-change database: the job still names `kind: Nvdec` under a binary that
  cannot emit it. Filed as `REQ-REFRESH`. **When changing emitted requirements,
  test the upgrade path on an old database, not only a fresh one.**

## The rule that keeps being right

An invariant verified where it is cheap to measure will be violated where it
matters. A green suite is evidence about the tests. Today's version: NVENC
"confirmed working" by standalone ffmpeg still has not transcoded anything
through transcodarr. **Run the thing.**
