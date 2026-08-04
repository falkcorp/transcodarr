### Fixed

#### Discovery never walks a library's own work or trash area

The exclusion list matches directory *names* — `work`, `trash`, `.zfs`,
`@eaDir`, `lost+found`. That cannot help an operator who sets `work_dir` to
something not called `work`. Setting it to `.transcodarr-work` inside the
library root, which is a perfectly reasonable thing to do and which I did while
configuring the real server, produces a directory the name list does not cover.

Discovery would then walk into it and enqueue transcodarr's own staged output
and retained originals as source material. The staged output is a *partial*
file, so this is not merely wasteful — it is deliberately transcoding a
truncated file.

The scanner now excludes each library's configured `work_dir` and `trash_dir` by
path, regardless of what they are called or where they sit.
