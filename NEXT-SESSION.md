<!-- file: NEXT-SESSION.md -->
<!-- version: 3.4.0 -->
<!-- guid: c8f01a35-6d47-42b9-a0e5-317b6924cf80 -->
<!-- last-edited: 2026-08-16 -->

# Goal: a real transcode on the GPU node, both transports, audio and video

**Still not achieved.** No transcode has run on the GPU node (`windows-rtx2070`, 172.16.3.22) by either method. What
changed on 2026-08-12 is that two of the three blockers are gone, and the third
is now precisely scoped rather than mysterious.

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

ffmpeg on that box lives at `C:\Users\jdfal\bin\ffmpeg.exe` (note: `jdfal`, not
`jdfalk`).

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
3. **The agent-side stream path.** Half done (PR #83) — and the half that was
   done was not the half this entry described.

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

   ### What is left of step 3

   In `worker.rs::execute`, under `Stream`:

   - **Skip `ensure_same_device`** (worker.rs:161) and **`SourceGuard::observe`**
     (worker.rs:171). Both stat `final_path`, which the agent cannot see. The
     server builds the guard from the stored file row instead — already done in
     `push_output`.
   - **Fetch to `a.source_path`** before encoding. It is now an agent-local path
     the server chose, so `argv` and `judge()` both already point at it.
   - **Push instead of `commit_blocking`** (worker.rs:242). Report whatever
     resolution the server returns.

   **The plumbing is the actual work, and it does not exist yet.** `Link`
   (client.rs:263–322) only sends messages on the `Connect` stream; there is no
   fetch or push method, and `stamp()` (client.rs:677) is private and called
   only at the `connect()` call site (client.rs:580). `FetchSource` and
   `PushOutput` are separate RPCs needing an `AgentServiceClient<Channel>`
   plumbed into `LocalWorker` along with identity to stamp. Budget for that, not
   for a two-line branch.

   **Traps that still apply here:** an unstamped fetch gets an opaque
   `Unauthenticated`; an in-stream error is fatal, not end-of-stream
   (`transfer.rs:63`), and a reader that treats it as termination turns a
   missing source into a zero-byte fetch whose blake3 verifies fine.
4. **Prove it locally before Windows.** Server and agent both on the Mac, agent
   `--transport stream`, a real audio transcode end to end. None of that needs
   the GPU node. If it works locally, Windows is deployment; if it does not, you
   debug byte plumbing without mingw, NVENC and logon sessions confounding it.
5. **Then `windows-rtx2070`:** stream-mode audio, then video with `hevc_nvenc`.
6. **Mount mode last** — see below, it needs hands on the box.

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

## Known gap: a stranded ledger row wedges its file permanently

**This section previously understated the problem and prescribed the wrong
fix.** It said to add `ritual.recover_all()` to `serve.rs` at startup. That is
journal-driven, and the thing that wedges a file is the *ledger* row, which can
exist with no journal record at all (a crash between `grant_op` and the first
`journal.record`). What was actually found, all of it grep-verified:

- `idx_commit_intent_live` is `UNIQUE(final_path) WHERE state = 'live'` —
  keyed on the **path**, not on `(job_id, attempt)`. A stranded row therefore
  does not merely leak: it blocks *every* future attempt on that file, a retry
  under a fresh attempt number, and any brand-new job for the same path. Only
  `resolve_op` frees a path. The existing test
  `a_second_live_intent_on_one_path_is_structurally_impossible` proves the
  mechanism, and its sibling's doc comment names the consequence outright:
  "once resolved, the path is free again. Otherwise a retry could never
  install."
- `CommitIntentRepo::live()` — whose doc comment says "what the reconciler
  sweeps" — **has no production callers.** Only assertions in
  `commit_intent.rs` tests and two in `runner.rs`. `reconcile.rs`'s
  `Reconciler` works on job leases, not on ledger rows. `unknown_intents`
  (session.rs:241) reads the ledger but never writes it; it reports to the
  agent. The sweep that comment describes does not exist.
- **This is not new, and it is not confined to the server.** `LocalRunner` has
  it too: a crash between `grant_op` (runner.rs:301) and `resolve_op` strands
  the row, and the `recover_all()` at runner.rs:118 resolves the on-disk
  journal and never touches the ledger. The push change extended an existing
  hole to a second caller rather than opening a new one.

**A comment asserting that something is swept is weaker evidence than a
caller.** This one read as reassurance for however long it has been there.

### What the fix has to look like

Not a startup call. Startup is the wrong trigger for a condition that arises
continuously — a startup-only sweep cannot see an agent that dies at minute
five. It belongs with the reconciler's periodic pass, which already owns the
lease-expiry vocabulary (`LEASE_SECONDS`, `reconcile.rs`'s grace period).

Who may declare a live row dead, in order of difficulty:

| the row's agent is… | verdict |
|---|---|
| connected at the current epoch | never sweepable — someone is mid-replace |
| connected at a *newer* epoch | sweepable; `require_current_epoch` already recognises exactly this |
| not connected at all | needs a policy decision, not a derivation — lease expiry plus grace |

The row's `agent_id`/`agent_uid`/`boot_id` name the **streaming agent**, not the
server that staged for it, so "is this row mine?" is not available to the server
as a predicate. Do not reach for it.

Two facts settle the implementation, both verified:

1. **`resolve_op` is `WHERE id = ?1`, unguarded** (commit_intent.rs:219),
   unlike `advance_op` one function above it, which is
   `WHERE id = ?1 AND state = 'live'`. Harden it before writing any sweep, or a
   sweeper and a legitimately-finishing ritual both succeed and the sweeper
   frees a path that is mid-replace — precisely what the index exists to
   prevent. The asymmetry between the two looks unintentional.
2. **A ledger row carries every field of an `IntentRecord`** — job_id, attempt,
   agent_uid, boot_id, fencing_epoch, temp_path, final_path, trash_path,
   expected_content_sig, phase. So the sweep can synthesise an `IntentRecord`
   and reuse `recover_one`'s decision table verbatim. Do not write a second
   one: two copies of a fence are two fences that can drift.

Do this before running stream mode against anything that matters.

## Traps still standing

- Turing NVDEC cannot decode AV1 (exit 69, ~1 KB truncated) or 10-bit H.264
  (silent software fallback). Use an **8-bit H.264 source**, software decode,
  NVENC encode. Gate hardware decode per codec, never globally.
- **Probe by trial, never by asking ffmpeg what it lists.** Confirmed again
  today: that ffmpeg lists `av1_nvenc` and the hardware cannot do it.
- That ffmpeg build has **no libx264** — no software video fallback on the box.
- ICMP is filtered on `windows-rtx2070`. `ping` failing proves nothing; test port 22.
- **The agent must `stamp()` its fetch requests.** `client.rs` stamps
  `x-agent-id`/`x-agent-epoch` only at the `connect()` call site. `fetch_source`
  reads identity from that same metadata and refuses without it, so an unstamped
  fetch gets an opaque `Unauthenticated`. The two epochs — metadata and request
  body — must also agree, which they do trivially when both come from `stamp()`.
- **An in-stream error is fatal, not end-of-stream.** `transfer::source_stream`
  reports a missing source as a `Status::not_found` *inside* the stream
  (`transfer.rs:63`). A reader that treats stream errors as termination turns a
  missing source into a zero-byte fetch — and a blake3 of nothing verifies fine.
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

## The rule that keeps being right

An invariant verified where it is cheap to measure will be violated where it
matters. A green suite is evidence about the tests. Today's version: NVENC
"confirmed working" by standalone ffmpeg still has not transcoded anything
through transcodarr. **Run the thing.**
