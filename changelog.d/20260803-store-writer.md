### Added

#### `transcodarr-store::Writer` — the single writer

SQLite permits one writer at a time. Rather than let N callers discover that
through `SQLITE_BUSY` and retry storms, all writes funnel through one owning
thread that serialises them deliberately.

- **Priority lanes.** `Commit` > `Normal` > `Bulk`. A nightly bulk prune must
  not sit in front of the commit-ledger write holding a replace window open.
  The `Commit` lane also raises `synchronous` to `FULL`.
- **Per-operation `SAVEPOINT`.** Operations are batched into one transaction for
  throughput, so without isolation one bad op rolls back every innocent op
  batched with it. Each gets its own savepoint and fails alone — there is a test
  asserting exactly that.
- **Poison tracking**, honestly scoped. A `WriteOp` is a `FnOnce`, so the writer
  genuinely cannot re-run one; retrying would mean cloning arbitrary captured
  state. Retries belong to the caller, who still holds the inputs. What the
  writer *can* do is notice that operations of the same name keep failing and
  surface it, so a caller stuck in a retry loop becomes visible instead of
  silently burning the queue.

`WriteOp` carries a closure rather than an SQL string, so no SQL text escapes
the crate — the structural guarantee against a "fetch everything and filter in
the caller" pattern reappearing.

**Deviation from the spec, deliberate.** The architecture document types
`submit` as returning a `tokio::sync::oneshot::Receiver`. This uses `std`
channels and a plain thread, so the store carries no async runtime: it stays
usable from synchronous callers like `transcodarr admin explain`, and the server
bridges at its own boundary, which it must do anyway since it owns the runtime.
