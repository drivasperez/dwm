# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is jjws

jjws is a TUI workspace manager for [jj](https://martinvonz.github.io/jj/) (Jujutsu VCS). It creates, lists, and deletes jj workspaces stored under `~/.jjws/<repo-name>/`, with a shell wrapper that auto-`cd`s into the selected workspace.

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

**Execution flow:** `main.rs` → clap CLI (`cli.rs`) → dispatches to `workspace.rs` functions → which call `jj.rs` helpers → TUI picker in `tui.rs`.

### Module responsibilities

- **`cli.rs`** — Clap derive structs. Subcommands: `new`, `list`, `delete`, `shell-setup`.
- **`jj.rs`** — All `jj` CLI interactions. Runs `jj` as a subprocess via `Command`. Owns `WorkspaceInfo` and `DiffStat` structs. Parsing functions for jj output are pure and unit-tested.
- **`workspace.rs`** — Business logic: workspace creation/deletion/listing. Manages `~/.jjws/` directory layout. `WorkspaceEntry` is the main data struct passed to the TUI.
- **`tui.rs`** — Ratatui-based interactive table picker. Renders `WorkspaceEntry` data in a 6-column table (Name, Change, Description, Bookmarks, Modified, Changes).
- **`names.rs`** — Random `adjective-noun` name generator for unnamed workspaces.
- **`shell.rs`** — Emits a shell wrapper function; when `jjws` prints a directory path to stdout, the wrapper `cd`s into it.

### Key patterns

- **stdout vs stderr convention:** stdout is reserved for machine-readable output (paths the shell wrapper acts on). All human messages go to stderr via `eprintln!`.
- **jj template parsing:** `jj.rs` uses NUL-separated (`\0`) fields in jj templates with `\0\n` as record separator, parsed by `parse_workspace_info()`. This avoids issues with descriptions containing tabs/newlines.
- **`latest_description()`** walks ancestors via `jj log` with revset `latest(ancestors(WS@) & description(glob:"?*"))` to find the first non-empty commit description.
- **Workspace storage:** `~/.jjws/<repo>/.main-repo` file stores the path to the original repo. Each workspace is a subdirectory under `~/.jjws/<repo>/`.
