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
dwm run                 # run scripts.run for the current workspace
dwm setup               # interactive shell and agent setup
dwm version             # print the current version
dwm --version           # same, as a flag
```

## Lifecycle hooks

dwm runs user-defined commands at three points in a workspace's life. Configure them via `.dwm.toml` at the repo root:

```toml
[scripts]
setup = "npm install && cp ../main/.env .env"   # after `dwm new`
run = "npm run dev"                              # via `dwm run`
archive = "tar czf $DWM_WORKSPACE_NAME.tar.gz ." # before `dwm delete`

# How `run` is executed (default: "concurrent"):
#   concurrent     — spawn detached and return immediately (long-lived dev servers)
#   nonconcurrent  — block until the script exits
runScriptMode = "concurrent"
```

| Hook      | When it fires                             | Failure behaviour                                       |
| --------- | ----------------------------------------- | ------------------------------------------------------- |
| `setup`   | After `dwm new` provisions the workspace  | Logged; workspace is still created                      |
| `run`     | On `dwm run` (manual)                     | Concurrent: returns once spawned. Nonconcurrent: logged |
| `archive` | Before `dwm delete` removes the workspace | Verbose mode prompts; TUI deletion logs and proceeds    |

### Conductor compatibility

If `.dwm.toml` is absent but a [Conductor](https://www.conductor.build/docs/reference/conductor-json) `conductor.json` exists at the repo root, dwm reads `scripts.{setup,run,archive}` and `runScriptMode` from it as a drop-in fallback. Other Conductor-specific fields (`enterpriseDataPrivacy`, etc.) are accepted and ignored, so an existing `conductor.json` works without edits.

> **Conductor users:** Drop your existing `conductor.json` into the repo and dwm will read it. All env vars in your setup / run / archive scripts work unchanged.

### Execution

Scripts run as `sh -c "<command>"` with the workspace as the working directory. Stdout and stderr are forwarded to dwm's stderr (dwm's stdout is reserved for the shell-wrapper `cd` target). Concurrent `run` scripts inherit dwm's stdio so output appears in your terminal; closing that terminal will kill the script.

### Environment variables

Every hook (setup / run / archive) sees:

| Variable                   | Value                                                        |
| -------------------------- | ------------------------------------------------------------ |
| `DWM_WORKSPACE_PATH`       | Absolute path of the workspace                               |
| `DWM_WORKSPACE_NAME`       | Workspace name                                               |
| `DWM_REPO_ROOT`            | Absolute path of the original repository root                |
| `DWM_VCS`                  | `jj` or `git`                                                |
| `DWM_FROM_WORKSPACE`       | The `--from <name>` value, if provided (setup only)          |
| `CONDUCTOR_WORKSPACE_NAME` | Same as `DWM_WORKSPACE_NAME`                                 |
| `CONDUCTOR_WORKSPACE_PATH` | Same as `DWM_WORKSPACE_PATH`                                 |
| `CONDUCTOR_ROOT_PATH`      | Same as `DWM_REPO_ROOT` (the source repo, NOT the workspace) |
| `CONDUCTOR_DEFAULT_BRANCH` | Detected default branch (e.g. `main`, `master`)              |
| `CONDUCTOR_PORT`           | First port of a 10-port range allocated to this workspace    |

### Port allocation

`CONDUCTOR_PORT` gives each workspace a stable, unique 10-port slot starting from 3000. Allocations are persisted to `<repo>/.dwm/<name>/.dwm-port` so they survive restarts. New workspaces fill holes left by deleted ones (e.g. with 3000 and 3020 occupied, the next workspace gets 3010).

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

## Zellij integration

When dwm runs inside a [zellij](https://zellij.dev/) session (i.e. `$ZELLIJ` is set), it cooperates with the multiplexer for one-tab-per-workspace workflows:

- **`dwm new`** opens a new zellij tab named after the workspace (`zellij action new-tab --cwd <ws> --name <name>`) instead of `cd`-ing your current pane. The new tab IS your new context, so the original shell is left alone.
- **`dwm switch <name>`** focuses an existing tab named `<name>` (`zellij action go-to-tab-name <name>`). If no such tab exists, dwm spawns a new one. dwm also tries the decorated forms `<name> ▶`, `<name> ●`, `<name> ✓` so a running agent's tab name is still discoverable.
- **Agent status glyphs** — when the Claude Code hook handler fires, it renames the current zellij tab to `<name> <glyph>`:

  | Glyph  | Meaning                                      |
  | ------ | -------------------------------------------- |
  | `▶`    | running — agent is actively producing tokens |
  | `●`    | waiting on user input or permission          |
  | `✓`    | idle / done                                  |
  | _none_ | stale or no agent activity                   |

  When several agents share a workspace, the most attention-needing state wins (waiting > working > idle).

Every zellij interaction is best-effort — if `zellij` isn't on `$PATH` or the action fails, dwm falls through to its non-zellij behaviour and prints a warning to stderr.

**To disable:** unset `$ZELLIJ` before running dwm (e.g. `env -u ZELLIJ dwm new`).

## Build

```sh
cargo build
cargo t          # run tests (uses cargo-nextest)
cargo clippy     # lint
```
