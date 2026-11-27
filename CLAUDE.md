# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is dwm

dwm is a TUI workspace manager for [jj](https://martinvonz.github.io/jj/) (Jujutsu VCS) and git. It creates, lists, and deletes workspaces stored under `<repo-root>.dwm/worktrees/<name>/` (a sibling of the repo root), with a shell wrapper that auto-`cd`s into the selected workspace. It works with both jj and git repositories.

## Build & Test Commands

```sh
cargo build              # build
cargo t                  # run tests (aliased to cargo nextest run via .cargo/config.toml)
cargo t <test_name>      # run a single test by name
cargo clippy             # lint
```

We use **cargo-nextest** to run tests (`cargo t` is aliased in `.cargo/config.toml`).

## Testing Philosophy

Every bug fix should include a regression test. New parsing functions and utilities must have unit tests. Tests live as `#[cfg(test)] mod tests` inside each source file (not in a separate `tests/` directory).

## Architecture

**Execution flow:** `main.rs` → clap CLI (`cli.rs`) → dispatches to the `workspace` module (`src/workspace/mod.rs`) → VCS backends (`jj.rs`/`git.rs`) via `vcs.rs` trait → TUI picker in `tui.rs`.

### Module responsibilities

- **`cli.rs`** — Clap derive structs. Subcommands: `new`, `list`, `status`, `switch`, `rename`, `delete`, `prune`, `setup`.
- **`vcs.rs`** — VCS abstraction layer. Defines `VcsBackend` trait, `VcsType` enum, and owns `WorkspaceInfo` and `DiffStat` structs shared across backends.
- **`jj.rs`** — jj backend implementing `VcsBackend`. Runs `jj` as a subprocess via `Command`. Parsing functions for jj output are pure and unit-tested.
- **`jj_workspace_store.rs`** — Reads and rewrites jj's `.jj/repo/workspace_store/index` (a small protobuf) so a workspace's recorded path follows it when dwm moves the directory. No jj command does this. Takes jj's `index.lock` flock and refuses any field it doesn't recognise.
- **`git.rs`** — Git backend implementing `VcsBackend`. Runs `git` as a subprocess via `Command`.
- **`workspace/`** — Workspace business logic, split by operation and re-exported through `mod.rs`. Manages the per-repo `<repo-root>.dwm/` directory and a cross-platform repo registry (`dirs::data_dir()/dwm/repos.txt`) used by `list --all`. `WorkspaceEntry` is the main data struct passed to the TUI.
- **`tui.rs`** — Ratatui-based interactive table picker. Renders `WorkspaceEntry` data in a 6-column table (Name, Change, Description, Bookmarks, Modified, Changes).
- **`rex.rs`** — Integration with the Rex terminal, active when `REX_SERVER` is set (`DWM_REX=0`/`1` overrides). Every Rex operation goes through the embedded `src/rex/ops.lua`, piped to `rex do -`; `src/rex/palette.lua` is the command palette module `dwm setup` installs. The `RexApi` trait (live: `LiveRex`, tests: `FakeRex`) is injected via `WorkspaceDeps.rex`. Parsing of Rex output and pane-to-workspace matching are pure functions. Rex encodes empty Lua tables as `{}`, so lists are parsed with `lua_list`. Also emits dwm's own OSC 7501 status to stderr (`StatusReport`).
- **`pool.rs`** — Workspace pools for `dwm lease`/`dwm release`: lease records in `<repo-root>.dwm/leases/<ws>.json`, the exclusive leasing lock, `<pool>-<n>` member naming, and `lease_state` (a lease taken in Rex ends when its pane closes). The orchestration (`lease_workspace_inner`) lives in `workspace/lease.rs` and reuses `prune_verdict` plus `VcsBackend::reset_workspace`.
- **`skill.rs`** — The agent skill (`src/skill/SKILL.md`, embedded): `dwm skill` prints it and `dwm setup` installs it to `~/.claude/skills/dwm/`, updating copies that carry its marker. A test checks every `dwm <subcommand>` in its code against the CLI, so update the skill when commands change.
- **`names.rs`** — Random `adjective-noun` name generator for unnamed workspaces.
- **`shell.rs`** — Emits a shell wrapper function; subcommands that may produce a workspace path (`new`, `list`, `switch`, `delete`, `rename`, `prune`) capture stdout and `cd` into the result, while all other subcommands run the binary directly.

### Key patterns

- **stdout vs stderr convention:** stdout is reserved for machine-readable output (paths the shell wrapper acts on). All human messages go to stderr via `eprintln!`.
- **jj template parsing:** `jj.rs` uses NUL-separated (`\0`) fields in jj templates with `\0\n` as record separator, parsed by `parse_workspace_info()`. This avoids issues with descriptions containing tabs/newlines.
- **`latest_description()`** walks ancestors via `jj log` with revset `latest(ancestors(WS@) & description(glob:"?*"))` to find the first non-empty commit description.
- **Rex placement:** inside Rex, `switch`/`list`/`new` focus an existing pane in the workspace or open a session (`go_to_workspace`, `open_new_workspace`) and print no path, falling back to printing the path when Rex can't switch sessions (needs Remote Control) or `--here` is passed. `delete`/`prune` check for panes in the workspace first (`OpenPanes`); the pane dwm runs in (`REX_BLOCK`) is excluded. Agent status from Rex replaces hook-file status per workspace (`agent::combine_summaries`).
- **Pruning:** `dwm prune` asks each backend for a `PruneVerdict` via `VcsBackend::prune_verdict` (default: keep). The jj backend snapshots with `jj status` in the workspace and never runs `jj workspace update-stale` (it can discard on-disk edits); stale/sibling-op workspaces are kept. Parsing of `jj status` stderr and of the unpushed-commit log lives in pure functions in `jj.rs`/`git.rs`. Deletion goes through `delete_workspace_inner`.
- **Workspace storage:** workspaces live at `<repo-root>.dwm/worktrees/<name>/`, a sibling of the repo root (`dwm_dir` in `workspace/layout.rs`). They must not be nested inside the repo: a jj workspace has no `.git`, so git and editors walk up to the enclosing repo and would treat the whole workspace as ignored. Because of that, dwm writes no ignore rules. A cross-platform registry at `dirs::data_dir()/dwm/repos.txt` records repos that have ever had a workspace, used by `dwm list --all`. Agent status data lives in `<repo-root>.dwm/agent-status/` — a sibling of `worktrees/` so workspace names and internal metadata can't collide.

## Documentation

When adding new features, subcommands, or changing existing behavior, keep `README.md`, `site/index.html`, and, for anything agents use, the agent skill in `src/skill/SKILL.md` up to date with the changes.
