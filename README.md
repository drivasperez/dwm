# dwm

A TUI workspace manager for [jj](https://martinvonz.github.io/jj/) (Jujutsu VCS).

dwm creates, lists, and deletes jj workspaces stored under `~/.dwm/<repo>/`, with a shell wrapper that auto-`cd`s into the selected workspace.

## Install

```sh
cargo install --path .
```

## Shell setup

Add to your shell config (`.bashrc`, `.zshrc`, etc.):

```sh
eval "$(dwm shell-setup)"
```

This wraps the `dwm` binary so that selecting a workspace automatically `cd`s into it.

## Usage

```sh
dwm new [name]     # create a workspace (name auto-generated if omitted)
dwm list           # interactive TUI picker to switch workspaces
dwm delete [name]  # delete a workspace (current one if omitted)
```

## Build

```sh
cargo build
cargo t          # run tests (uses cargo-nextest)
cargo clippy     # lint
```
