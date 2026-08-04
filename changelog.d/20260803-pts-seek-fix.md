### Fixed

#### The last-packet-PTS guard actually applies to real files now

`last_packet_pts_us` passed `-read_intervals -60`, which reads like "the last
sixty seconds" and is not. On a file long enough for the seek to matter it
silently returns nothing — exit 0, no output — so the function returned `None`,
callers fell back to the container header duration, and **the entire
last-packet-PTS guarantee quietly stopped applying**.

That is the one fallback the validation design forbids: a truncated MKV
frequently keeps the source duration in its header, which is the whole reason
the last packet is consulted.

Measured on a real 23-minute Blu-ray remux on the server:

```text
broken  (-read_intervals -60)          → (empty)
fixed   (-read_intervals 1362%+#100000) → 1421.962000
header duration                         → 1422.016000
```

The interval is now an absolute seek point computed from the header duration —
which is used only to decide *where to look*, never as the answer. It costs 0.3
seconds on a 7 GB file.

**Every existing test passed with this bug present**, because they all used
short fixtures where the broken form happens to work. A regression test with a
70-second file is added, but it does not reproduce the failure on macOS — the
behaviour differs by platform and ffprobe build. The fix is verified directly
against the file that failed.

### Added

#### `admin run --class`

Restricts a run to one job class. The queue is priority-ordered across all
classes, so without it a small run picks up the largest video encodes first —
hours of work when a bounded audio demonstration was wanted.
