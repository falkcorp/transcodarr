### Added

#### Implementation handoff document

`docs/design/IMPLEMENTATION-HANDOFF.md` is the entry point for building
transcodarr out from the committed design, written to be read with no prior
context.

It records the decisions that are locked (where each phase is built, how
autonomous the work runs, that D14 is deferred to Phase 3), the working
agreement (changelog fragments, file headers, rebase-only merges, and the
fmt/clippy/test triple that must stay green), the environment facts including
the server's lack of GitHub credentials, and the media-correctness rules that
came from production measurement.

It also documents six traps that already cost time — the stale-base push that
would have reverted two commits of work, clippy failing open on an invalid
config, zsh not word-splitting unquoted variables, and the difference between
renaming an identifier and rewriting an address.
