# Agent Status Feature Plan

## Overview

Add agent status tracking to dwm so the TUI shows which workspaces have Claude Code agents running and which need user attention. Uses Claude Code hooks to write status files that dwm reads during workspace listing.

## Status File Design

**Location:** `~/.dwm/<repo>/.agent-status/<workspace-name>.json`

This directory lives alongside `.main-repo` and `.vcs-type` in the dwm repo dir. The workspace listing code already skips dotfiles, so these won't appear as phantom workspaces.

**Format:**
```json
{
  "session_id": "abc123",
  "status": "working",
  "updated_at": 1708700000
}
```

**Status values:**
- `"working"` — Agent is actively processing (set on `PreToolUse`)
- `"waiting"` — Agent needs user input/permission (set on `Notification` with `idle_prompt` or `permission_prompt`)
- `"idle"` — Agent finished responding, waiting for next prompt (set on `Stop`)

On `SessionEnd`, the status file is **deleted** (session is over, no agent running).

**Liveness check:** Before displaying status, dwm validates the `updated_at` timestamp. If it's older than 10 minutes with no update, consider the agent dead (crashed without cleanup) and ignore/delete the file.

## Hook Architecture

### `dwm hook-handler` subcommand

Rather than shipping separate shell scripts, dwm itself acts as the hook handler. Claude Code hooks invoke `dwm hook-handler` as a command hook. The subcommand:

1. Reads JSON from stdin (Claude Code hook data)
2. Extracts `hook_event_name`, `session_id`, `cwd`, `notification_type`
3. Resolves which dwm repo + workspace the `cwd` belongs to
4. Writes/updates/deletes the appropriate `.agent-status/<workspace>.json` file

This is cleaner than separate scripts — single binary, no script installation, no PATH issues.

### Hook event → status mapping

| Hook Event | Matcher | Action |
|---|---|---|
| `PreToolUse` | (any) | Write `status: "working"` |
| `Stop` | (any) | Write `status: "idle"` |
| `Notification` | `idle_prompt\|permission_prompt` | Write `status: "waiting"` |
| `UserPromptSubmit` | (any) | Write `status: "working"` |
| `SessionEnd` | (any) | Delete status file |

Note: We skip `PostToolUse` — the agent is still working between tool calls. The `Stop` event is what signals the turn is truly done.

### Hook configuration

The `dwm agent-setup` command writes this to `~/.claude/settings.json` (user-level, applies to all projects):

```json
{
  "hooks": {
    "PreToolUse": [
      { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
    ],
    "Notification": [
      {
        "matcher": "idle_prompt|permission_prompt",
        "hooks": [{ "type": "command", "command": "dwm hook-handler" }]
      }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
    ],
    "SessionEnd": [
      { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
    ]
  }
}
```

## Implementation Steps

### Step 1: Define `AgentStatus` type

**File: `src/vcs.rs`** (or new `src/agent.rs`)

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Working,
    Idle,
    Waiting,  // Needs user attention
}
```

Add `agent_status: Option<AgentStatus>` to `WorkspaceEntry`.

### Step 2: Status file reading

**File: `src/agent.rs`** (new module)

Functions:
- `read_agent_status(repo_dir: &Path, workspace_name: &str) -> Option<AgentStatus>`
  - Reads `repo_dir/.agent-status/<workspace_name>.json`
  - Parses JSON, checks liveness (updated_at within 10 minutes)
  - Returns `None` if file missing, stale, or unparseable
- `write_agent_status(repo_dir: &Path, workspace_name: &str, session_id: &str, status: AgentStatus) -> Result<()>`
  - Creates `.agent-status/` dir if needed
  - Writes JSON atomically (write to temp, rename)
- `remove_agent_status(repo_dir: &Path, workspace_name: &str) -> Result<()>`
  - Deletes the status file

### Step 3: Wire into workspace listing

**File: `src/workspace.rs`**

In `list_workspace_entries_inner()`, after building each `WorkspaceEntry`, call `read_agent_status(repo_dir, &entry.name)` and set `entry.agent_status`.

### Step 4: TUI column

**File: `src/tui.rs`**

Add a 7th column "Agent" between "Modified" and "Changes" (or at the end):

| Status | Display | Color |
|---|---|---|
| `None` | `""` (empty) | — |
| `Working` | `"working"` | Green |
| `Idle` | `"idle"` | DarkGray |
| `Waiting` | `"waiting"` | Yellow, bold |

Adjust column widths to accommodate. The "waiting" state should be visually prominent since that's the primary use case — knowing which agents need attention.

### Step 5: `hook-handler` subcommand

**File: `src/cli.rs`** — Add `HookHandler` variant to the `Commands` enum.

**File: `src/agent.rs`** — Implement `handle_hook(stdin_json: &str) -> Result<()>`:
1. Parse stdin JSON
2. Determine workspace from `cwd`:
   - Check if `cwd` is under `~/.dwm/<repo>/<workspace>/` → extract repo+workspace
   - Otherwise, check if it's a main repo tracked by dwm → use main workspace name
3. Map `hook_event_name` + `notification_type` to action (write status or delete)
4. Perform the action

**File: `src/main.rs`** — Dispatch `HookHandler` to `agent::handle_hook()`.

The hook-handler reads all input from stdin and must complete quickly (Claude Code has a 10-minute timeout for command hooks, but we should be near-instant).

### Step 6: `agent-setup` subcommand

**File: `src/cli.rs`** — Add `AgentSetup` variant.

**File: `src/agent.rs`** — Implement `setup_hooks() -> Result<()>`:
1. Read `~/.claude/settings.json` (create if missing)
2. Merge hook configuration (preserve existing hooks, add dwm ones)
3. Write back
4. Print success message to stderr

Interactive confirmation similar to `shell-setup`.

### Step 7: Documentation

Update `README.md` and `site/index.html` with:
- New `agent-setup` subcommand
- How agent status tracking works
- Screenshot/description of the Agent column

### Step 8: Tests

- Unit tests for status file read/write/delete/liveness
- Unit tests for hook JSON parsing and workspace resolution
- Unit tests for settings.json merging in agent-setup
- Integration-style test: write status file, list workspaces, verify agent_status field

## Considerations

### Performance
The `PreToolUse` hook fires on every tool call, so `hook-handler` must be fast. Since it's just "read stdin, write a small file", this should be sub-millisecond. We should NOT do VCS operations or anything slow in the hook handler.

### Atomicity
Write status files atomically (write to `.agent-status/.tmp-<name>`, then rename) to avoid partial reads if dwm lists while the hook is writing.

### Cleanup
- `SessionEnd` deletes the file
- Liveness timeout (10 min) handles crashes
- `dwm delete <workspace>` should also clean up `.agent-status/<workspace>.json`
- Consider a `dwm agent-cleanup` or automatic cleanup during listing

### Main workspace
The main workspace (original repo dir) can also have an agent. The hook-handler needs to resolve this correctly — if `cwd` is the main repo path, map it to the main workspace name ("default" for jj, "main-worktree" for git).

### Multiple agents per workspace
For now, last-write-wins is fine. If two Claude Code sessions run in the same workspace, the status file reflects the most recent event. We could track by session_id later if needed.

### Hook handler finding the dwm binary
The hook config uses just `dwm hook-handler`. This works if dwm is on PATH. For `cargo install` users this is fine. We could also have `agent-setup` write the absolute path to the binary.
