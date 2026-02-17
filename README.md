# jjws

A TUI workspace manager for [jj](https://martinvonz.github.io/jj/) (Jujutsu VCS).

jjws creates, lists, and deletes jj workspaces stored under `~/.jjws/<repo>/`, with a shell wrapper that auto-`cd`s into the selected workspace.

## Install

```sh
cargo install --path .
```

## Shell setup

Add to your shell config (`.bashrc`, `.zshrc`, etc.):

```sh
eval "$(jjws shell-setup)"
```

This wraps the `jjws` binary so that selecting a workspace automatically `cd`s into it.

## Usage

```sh
jjws new [name]     # create a workspace (name auto-generated if omitted)
jjws list           # interactive TUI picker to switch workspaces
jjws delete [name]  # delete a workspace (current one if omitted)
```

## Build

```sh
cargo build
cargo t          # run tests (uses cargo-nextest)
cargo clippy     # lint
```
