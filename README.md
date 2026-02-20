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

Run `dwm shell-setup` interactively and it will offer to add the wrapper to your shell config automatically:

```sh
dwm shell-setup        # auto-detects your shell and offers to install
dwm shell-setup --fish # explicitly use fish
dwm shell-setup --zsh  # explicitly use zsh
dwm shell-setup --bash # explicitly use bash
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
```

## Build

```sh
cargo build
cargo t          # run tests (uses cargo-nextest)
cargo clippy     # lint
```
