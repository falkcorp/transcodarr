### Added

#### Probe ingestion

`Prober` runs `ffprobe` over `Discovered` files and stores the parsed facts. It
is the only part of Phase 2 that opens a media file, and it does so exactly once
per file per change — everything downstream runs off the stored result, which is
what keeps a policy edit a database operation rather than an 85 TB read.

Concurrency defaults to 4 and is bounded separately from transcode concurrency.
Probing is seek-heavy against a latency-bound pool, so pointing 48 threads at it
produces 48 slow probes rather than 48 fast ones, and starves anything
transcoding alongside.

Every probe carries a 120s wall-clock limit. A file on a stalled mount would
otherwise hold a worker forever, and enough of them stop ingestion with no error
to look at.

Failure marks the file `ProbeFailed` with the reason, rather than leaving it
`Discovered` to be retried on every subsequent pass. Unparseable output is a
probe failure, not a crash — a non-media file with a media extension
legitimately produces one. One bad file never abandons the rest of its batch.

Arguments go to `ffprobe` as argv, never through a shell: filenames containing
quotes and semicolons are ordinary in a media library, and there is a test that
one cannot become a command.

Stored size comes from the `file` row, not from `format.size` — discovery is the
single authority for it, and a probe disagreeing would make the size bucket
describe a different file than the row does.
