# dwm

A TUI workspace manager for [jj](https://martinvonz.github.io/jj/) (Jujutsu VCS) and [git](https://git-scm.com/).

dwm creates, lists, and deletes workspaces stored under `~/.dwm/<repo>/`, with a shell wrapper that auto-`cd`s into the selected workspace. It works with both jj and git repositories.

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
