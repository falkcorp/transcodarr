<!-- file: NEXT-SESSION.md -->
<!-- version: 3.0.1 -->
<!-- guid: c8f01a35-6d47-42b9-a0e5-317b6924cf80 -->
<!-- last-edited: 2026-08-12 -->

# Goal: a real transcode on the GPU node, both transports, audio and video

**Still not achieved.** No transcode has run on U1 by either method. What
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

### The agent runs on Windows — cross-compiled and executed on U1

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

1. **Wire the RPCs.** `AgentSession::fetch_source` / `push_output` still return
   `unimplemented`. `fetch_source` is small: check the epoch the way
   `CommitReport` does, look the job's source path up, hand it to
   `transfer::source_stream`.
2. **Server-side install for `push_output`.** Receive into a staging path with
   `transfer::Sink`, then follow `runner.rs:281–350` *literally*, including the
   ordering the comments there defend: `CommitIntentRepo::grant_op` **before**
   the ritual touches anything, `resolve_op` after whatever happened, the
   `trash_entry` row **last**. `AgentSession` will need a `WorkArea` +
   `CommitRitual` (constructor change; 3 call sites).
3. **The agent-side stream path** in `worker.rs`. Under `Stream`, fetch instead
   of reading `source_path`, and **do not** run the local ritual
   (`commit_blocking`, worker.rs:373).
   **The trap:** the intent journal exists to recover half-finished *installs*,
   and a streaming agent never installs. Reusing mount-mode journalling as-is
   writes `IntentRecord`s that can never resolve and hands the server
   `live_intents` for work it must not fence on. Decide what `live_intents()`
   returns under `Stream` — plausibly empty — and write down why.
4. **Prove it locally before Windows.** Server and agent both on the Mac, agent
   `--transport stream`, a real audio transcode end to end. None of that needs
   the GPU node. If it works locally, Windows is deployment; if it does not, you
   debug byte plumbing without mingw, NVENC and logon sessions confounding it.
5. **Then U1:** stream-mode audio, then video with `hevc_nvenc`.
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

## Traps still standing

- Turing NVDEC cannot decode AV1 (exit 69, ~1 KB truncated) or 10-bit H.264
  (silent software fallback). Use an **8-bit H.264 source**, software decode,
  NVENC encode. Gate hardware decode per codec, never globally.
- **Probe by trial, never by asking ffmpeg what it lists.** Confirmed again
  today: that ffmpeg lists `av1_nvenc` and the hardware cannot do it.
- That ffmpeg build has **no libx264** — no software video fallback on the box.
- ICMP is filtered on U1. `ping` failing proves nothing; test port 22.
- The `FakeServer` in `connect_client.rs` refuses to serve bytes on purpose.
  Do not weaken it to make a streaming test pass — a fake that returned an empty
  stream would let a streaming test pass while moving no bytes.

## The rule that keeps being right

An invariant verified where it is cheap to measure will be violated where it
matters. A green suite is evidence about the tests. Today's version: NVENC
"confirmed working" by standalone ffmpeg still has not transcoded anything
through transcodarr. **Run the thing.**
