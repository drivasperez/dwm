# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is dwm

dwm is a TUI workspace manager for [jj](https://martinvonz.github.io/jj/) (Jujutsu VCS) and git. It creates, lists, and deletes workspaces stored under `~/.dwm/<repo-name>/`, with a shell wrapper that auto-`cd`s into the selected workspace. It works with both jj and git repositories.

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

**Execution flow:** `main.rs` → clap CLI (`cli.rs`) → dispatches to `workspace.rs` functions → which call VCS backends (`jj.rs`/`git.rs`) via `vcs.rs` trait → TUI picker in `tui.rs`.

### Module responsibilities

- **`cli.rs`** — Clap derive structs. Subcommands: `new`, `list`, `status`, `switch`, `rename`, `delete`, `shell-setup`.
- **`vcs.rs`** — VCS abstraction layer. Defines `VcsBackend` trait, `VcsType` enum, and owns `WorkspaceInfo` and `DiffStat` structs shared across backends.
- **`jj.rs`** — jj backend implementing `VcsBackend`. Runs `jj` as a subprocess via `Command`. Parsing functions for jj output are pure and unit-tested.
- **`git.rs`** — Git backend implementing `VcsBackend`. Runs `git` as a subprocess via `Command`.
- **`workspace.rs`** — Business logic: workspace creation/deletion/listing/renaming/switching. Manages `~/.dwm/` directory layout. `WorkspaceEntry` is the main data struct passed to the TUI.
- **`tui.rs`** — Ratatui-based interactive table picker. Renders `WorkspaceEntry` data in a 6-column table (Name, Change, Description, Bookmarks, Modified, Changes).
- **`names.rs`** — Random `adjective-noun` name generator for unnamed workspaces.
- **`shell.rs`** — Emits a shell wrapper function; when `dwm` prints a directory path to stdout, the wrapper `cd`s into it.

### Key patterns

- **stdout vs stderr convention:** stdout is reserved for machine-readable output (paths the shell wrapper acts on). All human messages go to stderr via `eprintln!`.
- **jj template parsing:** `jj.rs` uses NUL-separated (`\0`) fields in jj templates with `\0\n` as record separator, parsed by `parse_workspace_info()`. This avoids issues with descriptions containing tabs/newlines.
- **`latest_description()`** walks ancestors via `jj log` with revset `latest(ancestors(WS@) & description(glob:"?*"))` to find the first non-empty commit description.
- **Workspace storage:** `~/.dwm/<repo>/.main-repo` file stores the path to the original repo. Each workspace is a subdirectory under `~/.dwm/<repo>/`.

## Documentation

When adding new features, subcommands, or changing existing behavior, keep both `README.md` and `site/index.html` up to date with the changes.
