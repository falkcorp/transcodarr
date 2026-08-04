### Added

#### `transcodarr-server` and the `Scanner`

The orchestration crate, and with it per-library discovery. The scanner walks a
library root, records what a `stat` can see, and notices what is gone. It never
opens a media file: probe facts are written once and survive every later scan
that finds the file unchanged, which is what makes rescanning ~49.6k files
cheap.

Two guards, both there because of a specific way the naive version destroys
data:

- **Mass-missing.** An unmounted library is indistinguishable from every file
  having been deleted, except by proportion. The scan counts what it *would*
  mark missing and refuses to write anything past 10% (above a floor of 25),
  closing the `scan_run` as `aborted` with the reason recorded.
- **`min_mtime_age_s`.** A file still being written has a recent mtime.
  Recording it races the writer; enqueueing it transcodes a partial file.

`.zfs`, `work`, `trash`, `@eaDir` and `lost+found` are never descended —
descending `.zfs` finds the library once per snapshot, and `work`/`trash` are
transcodarr's own areas, so walking them queues our own output as source.
Symlinks are not followed: one file reachable under two paths would be
transcoded twice.

Identity is `(dev, inode)` with `nlink` recorded. On platforms without stable
inode numbers the columns stay `NULL` rather than carrying a fabricated value —
a missing identity is recoverable, a wrong one silently merges two files.

#### `FileRepo::identity`

A narrow lookup returning size, mtime and identity without reconstructing facts
or parsing enums. It is the only per-file query in a scan, so it runs ~49,600
times per pass; building a full record for each of them, then discarding it for
every unchanged file, is the difference between a scan that finishes and one
that is the bottleneck.
