# dwm

A TUI workspace manager for [jj](https://martinvonz.github.io/jj/) (Jujutsu VCS) and [git](https://git-scm.com/).

dwm creates, lists, and deletes workspaces stored under `<repo>.dwm/worktrees/<name>/`, with a shell wrapper that auto-`cd`s into the selected workspace. It works with both jj and git repositories.

The workspace directory sits next to the repo rather than inside it, so for `~/dev/myrepo` workspaces live in `~/dev/myrepo.dwm/worktrees/`. A jj workspace has no `.git` of its own, so git and editors that use it walk up to the nearest enclosing repo; keeping workspaces outside the repo means they are never mistaken for ignored files in it, and there is nothing to add to your ignore rules.

## Install

Shell installer (macOS and Linux):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/drivasperez/dwm/releases/latest/download/dwm-installer.sh | sh
```

Pre-built binaries are also available on the [latest GitHub release](https://github.com/drivasperez/dwm/releases/latest).

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
dwm new <name> -- <cmd> # in Rex, run a command in the new workspace's session
dwm list                # interactive TUI picker to switch workspaces
dwm list --all          # multi-repo dashboard across all repos
dwm status              # non-interactive workspace summary
dwm status --json       # the same as JSON on stdout (add --all for every repo)
dwm switch <name>       # switch to a workspace by name
dwm rename <old> <new>  # rename a workspace
dwm delete [name]       # delete a workspace (current one if omitted)
dwm prune               # list workspaces that are safe to delete (dry run)
dwm prune --yes         # delete them
dwm lease <pool>        # lease a workspace from a pool and print its path
dwm release [name]      # end a lease (current workspace if omitted)
dwm setup               # interactive shell, agent, and Rex setup
dwm skill               # print the skill that teaches coding agents to use dwm
dwm version             # print the current version
dwm --version           # same, as a flag
```

Workspace and pool names are single names, not paths. Leading dots are allowed (for example, `.scratch`), but `.` and `..` are not names.

`dwm prune` cleans up finished workspaces. It never touches the main workspace, and treats a workspace as prunable only when deleting it loses nothing: for jj, it snapshots the workspace (`jj status`) and then requires that every non-empty commit in `trunk()..<ws>@` is reachable from a remote bookmark, so uncommitted edits and local-only commits both keep it. A stale workspace, or one reporting a sibling operation, is kept rather than repaired with `jj workspace update-stale`, which can overwrite on-disk edits. For git, the worktree needs a clean `git status` (untracked files count, ignored files don't) and no commits that are on neither trunk nor any remote branch. Without `--yes` it only prints each workspace's verdict and reason; with it, prunable workspaces go through the same path as `dwm delete`.

The `Changes` column in `dwm list` and `dwm status` is each workspace's own diff, measured from the point it branched off trunk — a workspace that simply hasn't been rebased is not reported as containing everything that has since landed on trunk.

## Workspace pools

`dwm lease <pool>` hands out a workspace from a pool of numbered workspaces (`<pool>-1`, `<pool>-2`, …) and prints its path, for agents and scripts that need a workspace per task without creating and deleting one each time:

```sh
cd "$(dwm lease agents)"   # or: dwm lease agents --at <rev>
# ... work, push ...
dwm release
```

It reuses the lowest-numbered member that isn't leased, isn't open in a Rex pane, and has nothing to lose by the same rules as `dwm prune`, moving it to a fresh change on trunk (or `--at <rev>`): for jj a new change on that revision, for git its own `<pool>-N` branch reset to it. Members with uncommitted or unpushed work are skipped, and when none is free dwm creates the next member, running your setup hooks as `dwm new` does. Leasing takes a lock, so agents leasing at the same time never get the same workspace.

`dwm setup` offers to install a [Claude Code skill](https://docs.anthropic.com/en/docs/claude-code/skills) at `~/.claude/skills/dwm/SKILL.md` that teaches agents to lease, use, and release workspaces, and keeps it current when you run setup after updating dwm. For other agents, `dwm skill` prints the same text.

A lease lasts until `dwm release`. A lease taken from a Rex pane also ends when that pane closes, so a crashed agent doesn't hold its workspace forever. `dwm prune` keeps leased workspaces, and `dwm status --json` shows each lease that still holds.

## Setup hooks

After `dwm new` creates a workspace, it can run a setup script (e.g. `npm install`, copy `.env`, …) so the workspace is immediately usable. Configure it via `.dwm.toml` at the repo root:

```toml
[scripts]
setup = "npm install && cp ../main/.env .env"
# `run` and `archive` are reserved for future lifecycle hooks; parsed but not invoked yet
```

If `.dwm.toml` is absent but a [Conductor](https://www.conductor.build/docs/reference/conductor-json) `conductor.json` exists at the repo root, dwm reads `scripts.setup` from it as a drop-in fallback. Conductor-specific fields like `runScriptMode` and `enterpriseDataPrivacy` are accepted and ignored, so an existing `conductor.json` works without edits.

`.dwm.toml` itself is strict: an unknown key or table anywhere in it (a typo like `[worksapce]` or `setpu = ...`, or a setting newer than your installed dwm) is an error naming the key and the file, rather than being silently ignored.

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

## Copy-on-write storage

Opt into copying selected ignored files into new workspaces before setup runs:

```toml
[workspace]
copy = ["node_modules", "target"]
copy_mode = "auto"
```

`auto` uses native copy-on-write clones on APFS and Linux filesystems with reflink
support, falling back to ordinary copies where unavailable. `copy` forces ordinary
copies. Cloned regular files have independent contents: editing either workspace
or deleting the source does not change the other copy. Symlinks retain their
original targets, including absolute targets outside the workspace.

The source is the main repository, or the named workspace with `--from`. Entries
are literal relative paths, not glob patterns. Only files ignored at both source
and destination are copied; tracked files and VCS/dwm metadata are excluded. A
file that is ignored in the source but not at the new workspace's revision (for
example because the source has an uncommitted `.gitignore` change) is skipped
with a warning. Set `copy_unignored = "error"` under `[workspace]` to make
`dwm new` fail instead, before anything is copied.
Existing destination files are never overwritten. Missing sources are reported
and skipped. Keep source build/dependency directories idle while copying: this
is not an atomic snapshot of a running build.

dwm also honours Claude Code's `.worktreeinclude`
convention, so a repository already set up for Claude Code worktrees needs no extra
configuration. Put a `.worktreeinclude` at the root of the source workspace using
`.gitignore` syntax:

```gitignore
.env
.env.local
config/secrets.json
```

Every file matching a pattern is copied, as long as it is also ignored; matches that
are tracked or not ignored are skipped, so the file can never duplicate committed
content. These files go through the same checks, copy mode and conflict rules as
`copy` entries, and both sources can be used together.

Setup still runs afterward to reconcile dependencies with the destination's
lockfile. Some artifacts, notably Python virtual environments and caches with
absolute paths, cannot be reused reliably across directories. A copying failure
returns an error without running setup, and removes the new workspace again
(with its git branch) so the same name can be retried. Reports go to stderr; stdout remains the shell's destination path.
This works with Git and Git-backed jj repositories (including non-colocated jj).
The experimental jj simple backend does not support ignored-file copying.

For **experimental Git tracked-checkout sharing**, additionally set:

```toml
[workspace]
checkout = "cow" # default: "standard"
```

This creates a real Git worktree and uses independent filesystem clones for
reusable source files. Git verifies private files against the requested commit
and populates missing or different files. Staged and unstaged source changes are
not imported. Git `--at` selects a commit; jj retains its existing parent-revision
semantics. There is no daemon, mount, or extra baseline cache.

The experimental mode falls back to normal Git creation for unsupported
filesystems, checkout attributes/conversions, filters/LFS, submodules, hooks,
and certain index/worktree configurations. The creation report identifies a
fallback. A failed population leaves a locked partial Git worktree for inspection;
use `git worktree unlock <path>` before manually repairing or removing it.
Cloned file timestamps can differ from an ordinary fresh checkout, which may
matter to build tools. jj rejects `checkout = "cow"`; use `copy` entries there.

Copy-on-write saves physical disk blocks, not logical file sizes. New and
rewritten data and per-file metadata still consume space. Normal directory-size
reports can count shared blocks more than once. See [measurements and the
benchmark harness](docs/performance.md). `DWM_REGISTRY_PATH` can override the
repository registry location, which the benchmark uses to isolate its fixtures.

## Agent status tracking

dwm can show the status of [Claude Code](https://docs.anthropic.com/en/docs/claude-code) agents running in your workspaces. The TUI's "Agent" column displays per-workspace counts like `2 waiting, 1 working`.

Inside [Rex](#rex), this needs no setup: Claude Code reports its state to the terminal, and dwm reads it from Rex (see below). For agents running in other terminals, run:

```sh
dwm setup
```

This installs [Claude Code hooks](https://docs.anthropic.com/en/docs/claude-code/hooks) into `~/.claude/settings.json` that report agent status to dwm via the `dwm hook-handler` command.

Inside Rex, agents can also say what they are doing or waiting for, such as which permission they need. The picker shows the selected workspace's most urgent message under the table and every agent's message in the preview pane (`p`), and `dwm status --json` includes them.

An agent is *waiting* (yellow) when it needs your input or a permission approval, or has stopped with an error. It is *working* (green) while it runs, *done* (cyan) when it has finished something you haven't looked at yet, and *idle* (gray) when it is waiting for its next prompt. Only Rex can tell done apart from idle. Status is tracked per session, so multiple agents in the same workspace are counted independently.

## Rex

When dwm runs inside the [Rex](https://www.superlogical.com/rex) terminal (it checks for `REX_SERVER`), it works with Rex's sessions and panes instead of only changing your shell's directory.

dwm reads agent status from Rex. Programs report their state to Rex with the [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status) (OSC 7501), which Claude Code supports, and dwm counts each pane that reports a status as an agent in the workspace its working directory is in. Where a workspace has such panes, Rex's count replaces the hook files', so an agent with hooks installed isn't counted twice.

`dwm switch` and choosing a workspace in `dwm list` focus a Rex pane that is already in the workspace, preferring one running an agent. If there isn't one, dwm opens a new Rex session named after the workspace. `dwm new` does the same for the workspace it creates, and anything after `--` is typed into the new session's shell, so `dwm new fix-login -- claude "fix the login redirect"` starts an agent in a fresh workspace. Pass `--here` to `new`, `list`, or `switch` to change directory in the current shell instead. dwm only does this when it runs interactively: when an agent or script runs it without a terminal on stderr, it changes directory as before, so it never moves the Rex app's view from under you. A command after `--` is the exception, since it asks for a session explicitly.

Switching the Rex app to another session needs Remote Control, in Rex's Settings under General. Without it, dwm changes directory as before and says why. A session started with a command is still created, so you can find it in Rex's session list.

`dwm delete` checks for Rex panes in the workspace before removing it, since deleting the directory would leave whatever runs there working in a void. It names the panes and asks whether to close them; the picker's delete refuses instead and shows why. `dwm prune` keeps any workspace that is open in a Rex pane. The pane `dwm` itself runs in doesn't count, because the shell wrapper moves it out. `dwm rename` relabels a Rex session named after the old workspace and lists other panes that were in it, since shells and agents there still have the old path.

`dwm new` and `dwm prune --yes` report their progress on the Rex tab they run in.

`dwm setup` offers to install these Rex command palette actions in `dwm.lua` next to Rex's `init.lua`:

- **dwm: Switch Workspace…** (`dwm_switch`) focuses or opens a workspace session.
- **dwm: New Workspace…** (`dwm_new`) creates a workspace and opens a session in it. Leave the name blank for a generated name, and the shell command blank for a plain shell.
- **dwm: Prune Workspaces…** (`dwm_prune`) previews cleanup in a floating terminal and asks before deleting anything. It uses the normal prune checks and keeps workspaces open in Rex; close their panes first if you want to prune them.

New and Prune use the focused terminal's repository or ask you to pick a registered repo. New and the repository picker require a Rex version that supports palette prompts. Bind an action in `init.lua`, for example `rex.bind("cmd+shift+w", "dwm_switch")`. Run `dwm setup` again to update an existing integration.

Set `DWM_REX=0` to turn off Rex integration for CLI invocations.

## Build

```sh
cargo build
cargo t          # run tests (uses cargo-nextest)
cargo clippy     # lint
lua src/rex/palette_test.lua  # Rex palette tests (no running Rex server needed)
```

## License

[MIT](LICENSE)
