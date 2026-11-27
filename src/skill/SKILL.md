---
name: dwm
description: Get an isolated jj or git workspace for a task with dwm, and give it back afterwards. Use when you need a separate checkout to make changes, run tests, or push a branch without touching the workspace the user is in; when a task says to lease, claim, or use a pool workspace; or when you need to list a repo's workspaces and which agents are working in them.
---

<!-- Installed by `dwm setup`, which rewrites this file when dwm changes. Run `dwm skill` to print the current text. -->

# dwm

dwm manages workspaces for jj and git repositories. Each workspace is a separate checkout of the same repository, stored next to it in `<repo>.dwm/worktrees/<name>/`. Changes in one workspace don't touch the files of another, so you can work without disturbing the user's checkout.

## Getting a workspace for a task

Lease one from a pool. A pool is a set of numbered workspaces (`<pool>-1`, `<pool>-2`, …) that agents reuse instead of creating a new checkout per task:

```sh
dwm lease agents              # prints the workspace path on stdout
dwm lease agents --at <rev>   # start from <rev> instead of trunk
```

Run it from anywhere inside the repository. Use the path it prints for everything that follows: pass it as the working directory of every command, or `cd` into it at the start of each shell command, because a new shell may start somewhere else. Pick a pool name that describes the kind of work, such as `agents` or `review`, and reuse it; dwm creates members as needed.

The workspace you get starts on a fresh change on trunk (or `--at`). dwm only hands out a member whose previous work is safe to discard, and never one another agent holds, so two agents leasing at the same moment get different workspaces. A new member runs the repository's dwm setup scripts, which may take a while.

When the task is finished and your work is committed and pushed, or no longer needed, give the workspace back:

```sh
dwm release <name>    # or plain `dwm release` from inside it
```

A released workspace keeps its files until the next lease resets it, so release only once nothing in it still matters. Inside the Rex terminal a lease also ends when the pane that took it closes.

## Rules

- Work only in the workspace you leased or created. Never edit files in, or run `jj new`/`jj edit`/`git checkout` against, the user's main checkout or another workspace.
- Don't delete workspaces you didn't create, and don't delete pool members at all; release them instead. Inside the Rex terminal, `dwm delete` refuses or asks when a pane is still open in the workspace; don't work around that.
- If `dwm lease` reports that a member was skipped because it has uncommitted or unpushed work, leave that member alone. It may hold someone's work.
- jj workspaces have no `.git` directory, so tools that look for one (such as `gh`) can't find the repository from inside them. Pass the repository explicitly, for example `gh --repo owner/name`.

## Looking around

```sh
dwm status --json         # this repo's workspaces as JSON
dwm status --all --json   # every repo dwm knows about
```

Each workspace in the output has `name`, `path`, `is_main`, `change_id`, `description`, `bookmarks`, `diff`, `stale`, and:

- `agents`: counts of agents in it by status (`waiting`, `working`, `done`, `idle`), with `notes` holding what agents said they are doing, most urgent first, or `null`
- `lease`: the current lease (`workspace`, `leased_at`, and the Rex pane that holds it, if any), or `null`
- `rex_pane`: a Rex terminal pane open in the workspace, or `null`

Use this to see which workspaces are busy before acting on one.

## Other commands

`dwm rename` refuses stale jj workspaces. Leave them alone rather than running `jj workspace update-stale` on someone else's work.

`dwm new <name>` creates a workspace with a name of your choosing, and `dwm delete <name>` removes one; prefer leasing for task work. `dwm prune` lists workspaces whose work is safe to discard and `dwm prune --yes` deletes them; it never deletes leased workspaces, or, inside Rex, ones open in a pane. Don't run `dwm list`, which is an interactive picker for people.
