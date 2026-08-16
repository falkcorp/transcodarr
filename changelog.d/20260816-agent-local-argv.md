<!-- file: changelog.d/20260816-agent-local-argv.md -->
<!-- version: 1.0.0 -->
<!-- guid: 1a5e7c40-93b2-4d68-8f01-2c6b94ae3d17 -->
<!-- last-edited: 2026-08-16 -->

### Added

#### `argv` is translated into the agent's own namespace

`JobAssignment.argv` has always been specified as "fully translated,
agent-local, exec'd with NO shell". It was not translated at all: the
orchestrator built it from `file.canonical_path` and the server's own work
directory and sent those verbatim. That happens to work when the agent shares a
filesystem with the server, which every run so far has, and cannot work for a
streaming agent — it is handed a path in a namespace it has no way to resolve.

The translation now happens in one place, `core::plan::agent_job_paths`, called
immediately before `build_ffmpeg_argv`. Under `TM_MOUNT` it returns exactly
what was sent before. Under `TM_STREAM` both ends of the job are named inside
the agent's own work area: it fetches the source to the input path and writes
its encode to the output path, and the server moves the bytes both ways.

The mount table's `canonical_prefix` -> `local_path` rewrite is specified and
still unimplemented — a separate gap, masked so far because every mount-mode
run has been same-host. It belongs at this same seam, and there is now exactly
one place for it rather than a second translator that could disagree.

#### `Capability.workarea_path`

The server cannot translate into a location it has no name for. The agent
already measured `workarea_free_bytes` against this directory; it now reports
the path as well.

**Not a substitution token in `argv`.** `{{input}}`-style placeholders were
considered and rejected on two guarantees the design already makes: `JobStarted`
echoes argv back and it must equal what was sent, and `job_attempt.argv_json` is
persisted so an operator can paste it into a shell on that agent and reproduce
the run byte-for-byte. A placeholder only the agent can expand breaks both.

A streaming agent that advertises no work area is refused at dispatch rather
than defaulted. An empty root joins to `/{job}.{attempt}.src.mkv` — the
filesystem root — and the failure would surface three steps later as an ffmpeg
error on another machine.

### Fixed

Paths for a Windows agent are joined with a backslash rather than the server's
separator. ffmpeg tolerates the mixed result, but the persisted argv is
promised to be pasteable on the machine that ran it, and one that merely happens
to work is not that promise.

Job ids are sanitised before becoming a path component. A job id is scanner- and
operator-derived, and under streaming the server is composing a path into
*another machine's* filesystem from it.
