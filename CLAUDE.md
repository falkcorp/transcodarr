<!-- file: CLAUDE.md -->
<!-- version: 2.6.0 -->
<!-- guid: 3c4d5e6f-7a8b-9c0d-1e2f-3a4b5c6d7e8f -->
<!-- last-edited: 2026-08-09 -->

# CLAUDE.md

transcodarr is a Rust CLI tool that wraps ffmpeg/ffprobe to transcode media files
while preserving metadata. Single binary, entry point at `src/main.rs`.

## Coding Standards

Org-wide coding standards are in the `.standards/` git submodule (cloned from `https://github.com/falkcorp/.github`).
Always clone with `git clone --recurse-submodules` so these are available.

Key files:

- **File headers (MANDATORY):** `.standards/instructions/file-headers.md`
- **Commit format:** `.standards/instructions/commit-messages.md`

## 🚨 CRITICAL: Documentation Update Protocol

This repository uses a direct-edit documentation workflow. The legacy doc-update scripts and
workflows are retired.

- Edit documentation directly in the target files.
- Always keep the required header (file path, version, guid) and bump the version on any change.
- Do not use create-doc-update.sh, doc_update_manager.py, or .github/doc-updates/.
- **Use `copilot-agent-util` for git operations** - Download latest from
  [releases](https://github.com/jdfalk/copilot-agent-util-rust/releases/latest)
  - The utility provides command filtering, safety checks, and consistent logging
  - VS Code tasks automatically use the utility when available
  - Use the utility directly for git commands: `copilot-agent-util git add`,
    `copilot-agent-util git commit`, etc.

## Canonical Source for Agent Instructions

- General and language-specific rules: **`.standards/instructions/`** — the
  `falkcorp/.github` submodule, shared by every repository in the org. Clone
  with `--recurse-submodules` or it will be empty.
- Shared document templates: **`.standards/templates/`** (e.g. the executive
  summary template). Reference them; do not copy them into this repository.
- Prompts: `.github/prompts/`
- System documentation: `.github/copilot-instructions.md`

This section previously named `.github/instructions/` as the canonical source.
**That directory has never existed in this repository** — it is a per-repo copy
carried by other repos in the org, and pointing at it here sent agents to a path
that is not present. The rules genuinely in force are the ones under
`.standards/`, which is why they are named above.

For all agent, Claude, or workflow tasks, **refer to the above files**. Do not duplicate or override
these rules elsewhere.

## 📝 Changelog & TODO — Use the Fragment System (MANDATORY)

**Do not hand-edit `CHANGELOG.md`, and do not add new tasks straight into the
`TODO.md` inbox.** Both files are assembled from per-change fragments so that
parallel PRs never collide on them.

- **`CHANGELOG.md` is assembled, not hand-edited.** Add a fragment under
  `changelog.d/` (run `scriv create`, or write the Markdown file by hand). The
  fragments are folded into `CHANGELOG.md` at release time by `scriv`, and a CI
  check (`changelog-check.yml`) requires one on each PR. See `changelog.d/README.md`.
- **New `TODO.md` tasks are added via fragments.** Drop a Markdown fragment in
  `todo.d/` (see `todo.d/README.md`) instead of editing the `## 📥 Inbox`
  section. `scripts/assemble_todo.py` folds fragments in daily. This is
  **add-only**: checking a task off or removing it is a normal direct edit of
  `TODO.md`.
