### Changed

#### Probe concurrency default raised from 4 to 16, from measurement

The original default of 4 was reasoned from the idea that `ffprobe` is
seek-bound against a latency-bound pool, so parallelism could not help.
Measured on the production pool (49,600 real files, Tdarr transcoding
alongside), that was backwards:

| concurrency | files/second |
| --- | --- |
| 8 | 0.35 |
| 32 | 2.0 |
| 96 | 2.18 (load average 35) |

Latency-bound is exactly the case where a deep queue helps — each probe spends
its time waiting rather than working, so the pool services many at once. The
knee is near 32; 96 buys 9% for triple the load. The default is 16, capturing
most of the gain while leaving headroom for transcodes sharing the pool, and
`--probe-concurrency` raises it for a dedicated ingest run.

#### Handoff records the Phase 2 milestone run

Discovery verified on all three real libraries: 49,600 files in 43 seconds,
matching the architecture document's prediction. The `min_mtime_age_s` guard
skipped 7 files that were being written at the time. Probe ingestion is
long-running and its state, log location and remaining assertions are recorded.
