# dwm

A TUI workspace manager for [jj](https://martinvonz.github.io/jj/) (Jujutsu VCS) and [git](https://git-scm.com/).

dwm creates, lists, and deletes workspaces stored under `<repo>/.dwm/worktrees/<name>/`, with a shell wrapper that auto-`cd`s into the selected workspace. It works with both jj and git repositories.

The first time you create a workspace in a repo, dwm adds `.dwm/` to your ignore rules — `.git/info/exclude` for git repos, `.gitignore` for jj-only repos — so the workspace dirs never end up tracked.

## Install

Homebrew:

```sh
brew install drivasperez/tap/dwm
```

Cargo:

```sh
cargo install dwm
```

Pre-built binaries and a shell installer are available on the [latest GitHub release](https://github.com/drivasperez/dwm/releases/latest).

## Shell setup

Run `dwm setup` interactively and it will offer to add the wrapper to your shell config and set up agent hooks automatically:

```sh
dwm setup
```

Or add it manually:

**Bash / Zsh** — add to `.bashrc` or `.zshrc`:

```sh
eval "$(dwm shell-setup)"
```

**Fish** — add to `~/.config/fish/config.fish`:

```fish
dwm shell-setup --fish | source
```

This wraps the `dwm` binary so that selecting a workspace automatically `cd`s into it.

## Usage

```sh
dwm new [name]          # create a workspace (name auto-generated if omitted)
dwm new --at <rev>      # create a workspace starting from a specific revision
dwm new --from <ws>     # fork from an existing workspace's current change
dwm list                # interactive TUI picker to switch workspaces
dwm list --all          # multi-repo dashboard across all repos
dwm status              # non-interactive workspace summary
dwm switch <name>       # switch to a workspace by name
dwm rename <old> <new>  # rename a workspace
dwm delete [name]       # delete a workspace (current one if omitted)
dwm setup               # interactive shell and agent setup
dwm version             # print the current version
dwm --version           # same, as a flag
```

## Setup hooks

After `dwm new` creates a workspace, it can run a setup script (e.g. `npm install`, copy `.env`, …) so the workspace is immediately usable. Configure it via `.dwm.toml` at the repo root:

```toml
[scripts]
setup = "npm install && cp ../main/.env .env"
# `run` and `archive` are reserved for future lifecycle hooks; parsed but not invoked yet
```

If `.dwm.toml` is absent but a [Conductor](https://www.conductor.build/docs/reference/conductor-json) `conductor.json` exists at the repo root, dwm reads `scripts.setup` from it as a drop-in fallback. Conductor-specific fields like `runScriptMode` and `enterpriseDataPrivacy` are accepted and ignored, so an existing `conductor.json` works without edits.

The script runs as `sh -c "<command>"` with the new workspace as its working directory. Stdout and stderr from the script are forwarded to dwm's stderr (dwm's stdout is reserved for the path the shell wrapper `cd`s into). A non-zero exit prints a warning but doesn't unwind — the workspace is still created and you're still `cd`d into it.

The script sees these environment variables:

| Variable                   | Value                                          |
| -------------------------- | ---------------------------------------------- |
| `DWM_WORKSPACE_PATH`       | Absolute path of the new workspace             |
| `DWM_WORKSPACE_NAME`       | Workspace name                                 |
| `DWM_REPO_ROOT`            | Absolute path of the original repo root        |
| `DWM_VCS`                  | `jj` or `git`                                  |
| `DWM_FROM_WORKSPACE`       | The `--from <name>` value, if provided         |
| `CONDUCTOR_ROOT_PATH`      | Alias of `DWM_REPO_ROOT`, for Conductor compat |
| `CONDUCTOR_WORKSPACE_PATH` | Alias of `DWM_WORKSPACE_PATH`                  |
| `CONDUCTOR_WORKSPACE_NAME` | Alias of `DWM_WORKSPACE_NAME`                  |

## Agent status tracking

dwm can show the status of [Claude Code](https://docs.anthropic.com/en/docs/claude-code) agents running in your workspaces. The TUI's "Agent" column displays per-workspace counts like `2 waiting, 1 working`.

To set it up, run:

```sh
dwm setup
```

This installs [Claude Code hooks](https://docs.anthropic.com/en/docs/claude-code/hooks) into `~/.claude/settings.json` that report agent status to dwm via the `dwm hook-handler` command.

**Statuses:**

- **waiting** (yellow) — agent needs user input or permission approval
- **working** (green) — agent is actively executing
- **idle** (gray) — agent finished its turn, waiting for the next prompt

Status is tracked per session, so multiple agents in the same workspace are counted independently.

## Build

```sh
cargo build
cargo t          # run tests (uses cargo-nextest)
cargo clippy     # lint
```
