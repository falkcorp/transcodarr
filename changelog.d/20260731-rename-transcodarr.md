### Changed

#### Renamed the project from `transcoderr` to `transcodarr`

Adopts the Servarr-suite naming convention (sonarr, radarr, tdarr, transcodarr).
The Cargo package and the installed binary are both now `transcodarr`, and the
test helper `common::run_transcoderr` becomes `common::run_transcodarr`.

**This renames the binary.** Existing invocations of `transcoderr ...` must
become `transcodarr ...`; no compatibility shim is provided, because the rename
is deliberately being done before the distributed orchestrator is implemented
rather than after, while the blast radius is still nine files.

`repository` and `homepage` in `Cargo.toml` now point at
`https://github.com/falkcorp/transcodarr`.

No behaviour changes. The test suite result is unchanged from before the rename
(12 passed, 4 failed, 2 ignored); the four failures are pre-existing and
untouched here.
