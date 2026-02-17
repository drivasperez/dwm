# TODO

## Quick wins

- [x] `dwm new --at <revset>` — Create a workspace starting from a specific revision instead of always `@`. Pass `--revision` through to `jj workspace add`.
- [x] Inline delete from the picker — Press `d` on a highlighted workspace in the TUI to delete it (with confirmation). Avoids quit-type-relist cycle.
- [x] Filter/search in the picker — Press `/` to fuzzy-filter over workspace names, descriptions, and bookmarks.
- [x] Sort options — Cycle sort modes (name, recency, diff size) with a keypress like `s`. Default to most-recently-modified-first.

## Medium effort, high value

- [x] `dwm status` — Non-interactive, `git branch -v`-style summary printed to stderr. Useful for scripting and shell prompts. Could also be a `--no-tui` flag on `list`.
- [x] Stale workspace detection — Flag workspaces merged into trunk or unchanged for >30 days. Show a dim "stale" marker in the TUI, or a `dwm gc`/`dwm prune` command for bulk cleanup.
- [x] `dwm rename <old> <new>` — Rename a workspace (`jj workspace forget` + `jj workspace add --name` + move directory).
- [x] Multi-repo dashboard — `dwm list --all` to show workspaces across all repos under `~/.dwm/`, grouped by repo name.

## Exciting / ambitious

- [ ] Preview pane in the TUI — Right-side panel showing `jj log` or `jj diff --stat` for the highlighted workspace. Ratatui split layout.
- [ ] `dwm new --from <workspace>` — Fork an existing workspace by creating a new one and editing the same change the source points at.
- [ ] Shell prompt integration — Export `dwm_WORKSPACE` env var and provide snippets for starship/p10k/oh-my-zsh showing current workspace name + change ID.
- [ ] Workspace templates/hooks — `.dwm.toml` in repo root to configure default revset, auto-descriptions, post-create hooks (e.g., `cargo build`).

## Wild cards

- [ ] `dwm switch` (no TUI) — `cd`-style shortcut with tab-completion via `dwm completions zsh/bash/fish`.
- [ ] Workspace notes — `.dwm-note` file per workspace for freeform scratch text, displayed in the TUI.
