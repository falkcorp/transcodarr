### Fixed

#### `--dry-run` no longer creates directories

`batch --dry-run` called `fs::create_dir_all` *before* the dry-run guard, so
previewing a run against a large library silently built the entire mirrored
output tree. Directory creation now happens below the guard, on the real path
only. A dry run touches nothing.

#### Unknown preset names are an error, not a silent no-op

`--preset typo-here` matched a bare `_ => {}` and was ignored, so the run
proceeded with default codecs while appearing to honour the preset — discovered,
if ever, an hour and two hundred files later. Unknown names now fail immediately
and list the valid presets.

#### `--dry-run` prints the actual ffmpeg command

The preview was a prose summary (`Would transcode X -> Y with vcodec=...`) that
could not be copy-pasted, verified, or diffed. Both the single-file and batch
dry runs now emit the exact argv that would be executed, shell-quoted.

#### `info` writes its report to stdout

`ffprobe -i` writes the human-readable report to *stderr*, so `transcodarr info`
produced nothing on stdout and could not be piped into anything. The report is
now captured and written to stdout; genuine failures still go to stderr and
still fail the command.

The test suite goes from 12 passed / 4 failed to **16 passed / 0 failed**
(2 ignored, unchanged). All four tests already existed and were asserting the
correct behaviour.
