### Fixed

#### `clippy.toml` was invalid, silently disabling all linting

Six keys were wrong: `cyclomatic-complexity-threshold` is a deprecated alias of
`cognitive-complexity-threshold`, so setting both was a duplicate-field error,
and five others had been renamed or removed upstream.

Clippy rejects its *entire* configuration file on a single unrecognised key, so
this did not degrade gracefully — `cargo clippy` refused to run at all, and had
been doing so silently.

| Old key | Now |
| --- | --- |
| `cyclomatic-complexity-threshold` | removed (alias of `cognitive-complexity-threshold`) |
| `large-error-types-threshold` | `large-error-threshold` |
| `large-futures-threshold` | `future-size-threshold` |
| `large-stack-arrays-threshold` | `too-large-for-stack` |
| `large-types-passed-by-value-threshold` | `pass-by-value-size-limit` |
| `string-literal-chars-threshold` | removed (no longer exists) |

Every remaining key is validated against clippy 0.1.97. This matters beyond
tidiness: the M1 exit criterion in `docs/design/distributed-architecture.md`
requires `cargo clippy --all-targets --all-features -- -D warnings` to be green,
which was impossible while the config was rejected.
