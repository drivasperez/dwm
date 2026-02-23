use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::vcs;

/// How long before a status file is considered stale and ignored.
const STALE_TIMEOUT: Duration = Duration::from_secs(600);

/// Possible states of a Claude Code agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Working,
    Idle,
    Waiting,
}

/// On-disk representation of a single agent's status file.
#[derive(Debug, Deserialize, Serialize)]
pub struct AgentStatusFile {
    pub workspace: String,
    pub status: AgentStatus,
    pub updated_at: u64,
}

/// Aggregated agent counts for a single workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentSummary {
    pub waiting: u32,
    pub working: u32,
    pub idle: u32,
}

impl AgentSummary {
    pub fn is_empty(&self) -> bool {
        self.waiting == 0 && self.working == 0 && self.idle == 0
    }

    /// Return the most urgent status present, for color selection.
    pub fn most_urgent(&self) -> Option<AgentStatus> {
        if self.waiting > 0 {
            Some(AgentStatus::Waiting)
        } else if self.working > 0 {
            Some(AgentStatus::Working)
        } else if self.idle > 0 {
            Some(AgentStatus::Idle)
        } else {
            None
        }
    }
}

impl fmt::Display for AgentSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.waiting > 0 {
            parts.push(format!("{} waiting", self.waiting));
        }
        if self.working > 0 {
            parts.push(format!("{} working", self.working));
        }
        if self.idle > 0 {
            parts.push(format!("{} idle", self.idle));
        }
        write!(f, "{}", parts.join(", "))
    }
}

/// Return the `.agent-status` directory for a repo.
fn agent_status_dir(repo_dir: &Path) -> PathBuf {
    repo_dir.join(".agent-status")
}

/// Convert a unix timestamp to a [`SystemTime`].
fn system_time_from_epoch_secs(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Read all agent status files for a repo and return per-workspace summaries.
///
/// Stale entries (older than [`STALE_TIMEOUT`]) are silently ignored.
pub fn read_agent_summaries(repo_dir: &Path) -> HashMap<String, AgentSummary> {
    read_agent_summaries_at(repo_dir, SystemTime::now())
}

fn read_agent_summaries_at(repo_dir: &Path, now: SystemTime) -> HashMap<String, AgentSummary> {
    let dir = agent_status_dir(repo_dir);
    let mut map: HashMap<String, AgentSummary> = HashMap::new();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let status_file: AgentStatusFile = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Skip stale entries
        let updated = system_time_from_epoch_secs(status_file.updated_at);
        let age = now.duration_since(updated).unwrap_or(Duration::ZERO);
        if age > STALE_TIMEOUT {
            continue;
        }

        let summary = map.entry(status_file.workspace.clone()).or_default();
        match status_file.status {
            AgentStatus::Working => summary.working += 1,
            AgentStatus::Idle => summary.idle += 1,
            AgentStatus::Waiting => summary.waiting += 1,
        }
    }

    map
}

/// Write an agent status file for the given session.
pub fn write_agent_status(
    repo_dir: &Path,
    session_id: &str,
    workspace: &str,
    status: AgentStatus,
) -> Result<()> {
    let dir = agent_status_dir(repo_dir);
    fs::create_dir_all(&dir)?;

    let updated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let file = AgentStatusFile {
        workspace: workspace.to_string(),
        status,
        updated_at,
    };
    let json = serde_json::to_string(&file)?;

    // Atomic write: write to temp file, then rename
    let final_path = dir.join(format!("{}.json", session_id));
    let tmp_path = dir.join(format!(".tmp-{}.json", session_id));
    fs::write(&tmp_path, &json)?;
    fs::rename(&tmp_path, &final_path)?;

    Ok(())
}

/// Remove the agent status file for the given session.
pub fn remove_agent_status(repo_dir: &Path, session_id: &str) -> Result<()> {
    let path = agent_status_dir(repo_dir).join(format!("{}.json", session_id));
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Remove all agent status files for a given workspace name.
/// Used when a workspace is deleted.
pub fn remove_agent_statuses_for_workspace(repo_dir: &Path, workspace: &str) {
    let dir = agent_status_dir(repo_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Ok(sf) = serde_json::from_str::<AgentStatusFile>(&content)
            && sf.workspace == workspace
        {
            let _ = fs::remove_file(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// Hook handler
// ---------------------------------------------------------------------------

/// Resolve a `cwd` path to `(repo_dir, workspace_name)` using only the
/// filesystem — no VCS subprocess calls.
///
/// Returns `None` if the path doesn't correspond to a dwm-managed workspace.
fn resolve_workspace_from_cwd(dwm_base: &Path, cwd: &Path) -> Option<(PathBuf, String)> {
    // Case 1: cwd is under ~/.dwm/<repo>/<workspace>/...
    if let Ok(relative) = cwd.strip_prefix(dwm_base) {
        let mut components = relative.components();
        let repo_name = components.next()?.as_os_str().to_string_lossy().to_string();
        let ws_name = components.next()?.as_os_str().to_string_lossy().to_string();
        let repo_dir = dwm_base.join(&repo_name);
        return Some((repo_dir, ws_name));
    }

    // Case 2: cwd is under a main repo tracked by dwm.
    // Scan all ~/.dwm/*/.main-repo files to find a match.
    let entries = fs::read_dir(dwm_base).ok()?;
    for entry in entries.flatten() {
        let repo_path = entry.path();
        if !repo_path.is_dir() {
            continue;
        }
        let main_repo_file = repo_path.join(".main-repo");
        let main_repo_str = match fs::read_to_string(&main_repo_file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let main_repo = PathBuf::from(main_repo_str.trim());
        if cwd.starts_with(&main_repo) {
            // Determine the main workspace name from the VCS type
            let ws_name = match vcs::read_vcs_type(&repo_path) {
                Ok(vcs::VcsType::Jj) => "default",
                Ok(vcs::VcsType::Git) => "main-worktree",
                Err(_) => "default",
            };
            return Some((repo_path, ws_name.to_string()));
        }
    }

    None
}

/// Process a Claude Code hook event from stdin and update agent status files.
pub fn handle_hook() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    let json: serde_json::Value =
        serde_json::from_str(&input).context("invalid JSON from hook stdin")?;

    let event = json
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let session_id = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let cwd_str = json.get("cwd").and_then(|v| v.as_str()).unwrap_or("");

    if session_id.is_empty() || cwd_str.is_empty() {
        return Ok(()); // silently ignore incomplete data
    }

    let home = dirs::home_dir().context("could not determine home directory")?;
    let dwm_base = home.join(".dwm");

    let cwd = PathBuf::from(cwd_str);
    let (repo_dir, ws_name) = match resolve_workspace_from_cwd(&dwm_base, &cwd) {
        Some(r) => r,
        None => return Ok(()), // not a dwm workspace, silently ignore
    };

    match event {
        "PreToolUse" | "UserPromptSubmit" => {
            write_agent_status(&repo_dir, session_id, &ws_name, AgentStatus::Working)?;
        }
        "Stop" => {
            write_agent_status(&repo_dir, session_id, &ws_name, AgentStatus::Idle)?;
        }
        "Notification" => {
            let notification_type = json
                .get("notification_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match notification_type {
                "idle_prompt" | "permission_prompt" => {
                    write_agent_status(&repo_dir, session_id, &ws_name, AgentStatus::Waiting)?;
                }
                _ => {} // ignore other notification types
            }
        }
        "SessionEnd" => {
            remove_agent_status(&repo_dir, session_id)?;
        }
        _ => {} // ignore unknown events
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Agent setup
// ---------------------------------------------------------------------------

/// The hook configuration that dwm needs in ~/.claude/settings.json.
fn dwm_hook_config() -> serde_json::Value {
    serde_json::json!({
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
    })
}

fn display_path(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME")
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Check if dwm hooks are already installed in the given settings.
fn hooks_already_installed(settings: &serde_json::Value) -> bool {
    let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    let dwm_hooks = dwm_hook_config();
    for event_name in dwm_hooks.as_object().unwrap().keys() {
        let Some(arr) = hooks.get(event_name).and_then(|v| v.as_array()) else {
            return false;
        };
        let has_dwm = arr.iter().any(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hooks| {
                    hooks.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c == "dwm hook-handler")
                    })
                })
                .unwrap_or(false)
        });
        if !has_dwm {
            return false;
        }
    }
    true
}

/// Merge dwm hook configuration into the given settings object.
///
/// This is a pure function that takes existing settings and returns a new
/// settings object with dwm hooks added, preserving all other settings.
fn merge_dwm_hooks(mut settings: serde_json::Value) -> Result<serde_json::Value> {
    let dwm_hooks = dwm_hook_config();

    // Ensure root is an object
    let settings_obj = settings
        .as_object_mut()
        .context("settings.json root must be an object")?;

    // Get or create "hooks" object
    let hooks_obj = settings_obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("hooks must be an object")?;

    for (event_name, dwm_groups) in dwm_hooks.as_object().unwrap() {
        let arr = hooks_obj
            .entry(event_name)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .with_context(|| format!("hooks.{} must be an array", event_name))?;

        // Check if dwm hooks are already installed (look for "dwm hook-handler" command)
        let already_installed = arr.iter().any(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hooks| {
                    hooks.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c == "dwm hook-handler")
                    })
                })
                .unwrap_or(false)
        });

        if !already_installed {
            for group in dwm_groups.as_array().unwrap() {
                arr.push(group.clone());
            }
        }
    }

    Ok(settings)
}

/// Install dwm hook configuration into ~/.claude/settings.json.
pub fn setup_agent_hooks() -> Result<()> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let claude_dir = home.join(".claude");
    let settings_path = claude_dir.join("settings.json");
    let display = display_path(&settings_path);

    // Read existing settings or start fresh
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)
            .with_context(|| format!("could not read {}", settings_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("could not parse {}", settings_path.display()))?
    } else {
        serde_json::json!({})
    };

    // Check if already installed
    if hooks_already_installed(&settings) {
        eprintln!(
            "  {} Already installed in {}",
            "✓".green(),
            display.dimmed()
        );
        return Ok(());
    }

    // Prompt the user for permission
    eprint!(
        "  {} Add Claude Code hooks to {}? [y/N] ",
        "?".bold().cyan(),
        display.bold()
    );
    let tty = std::fs::File::open("/dev/tty");
    let response = match tty {
        Ok(f) => {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut line)?;
            line
        }
        Err(_) => String::new(),
    };

    if !response.trim().eq_ignore_ascii_case("y") {
        return Ok(());
    }

    settings = merge_dwm_hooks(settings)?;

    // Write back
    fs::create_dir_all(&claude_dir)?;
    let json = serde_json::to_string_pretty(&settings)?;
    fs::write(&settings_path, json)?;

    eprintln!("  {} Hooks installed to {}", "✓".green(), display.dimmed());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Convert a u64 epoch timestamp to SystemTime for test assertions.
    fn epoch(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn write_status_file(
        dir: &Path,
        session_id: &str,
        workspace: &str,
        status: &str,
        updated_at: u64,
    ) {
        let agent_dir = dir.join(".agent-status");
        fs::create_dir_all(&agent_dir).unwrap();
        let content = format!(
            r#"{{"workspace":"{}","status":"{}","updated_at":{}}}"#,
            workspace, status, updated_at
        );
        fs::write(agent_dir.join(format!("{}.json", session_id)), content).unwrap();
    }

    #[test]
    fn read_empty_dir() {
        let dir = TempDir::new().unwrap();
        let map = read_agent_summaries(dir.path());
        assert!(map.is_empty());
    }

    #[test]
    fn read_single_status() {
        let dir = TempDir::new().unwrap();
        let now = 1_000_000u64;
        write_status_file(dir.path(), "session1", "my-ws", "working", now);

        let map = read_agent_summaries_at(dir.path(), epoch(now));
        let summary = map.get("my-ws").unwrap();
        assert_eq!(summary.working, 1);
        assert_eq!(summary.waiting, 0);
        assert_eq!(summary.idle, 0);
    }

    #[test]
    fn read_multiple_agents_same_workspace() {
        let dir = TempDir::new().unwrap();
        let now = 1_000_000u64;
        write_status_file(dir.path(), "s1", "ws", "working", now);
        write_status_file(dir.path(), "s2", "ws", "waiting", now);
        write_status_file(dir.path(), "s3", "ws", "waiting", now);
        write_status_file(dir.path(), "s4", "ws", "idle", now);

        let map = read_agent_summaries_at(dir.path(), epoch(now));
        let summary = map.get("ws").unwrap();
        assert_eq!(summary.working, 1);
        assert_eq!(summary.waiting, 2);
        assert_eq!(summary.idle, 1);
    }

    #[test]
    fn read_multiple_workspaces() {
        let dir = TempDir::new().unwrap();
        let now = 1_000_000u64;
        write_status_file(dir.path(), "s1", "ws-a", "working", now);
        write_status_file(dir.path(), "s2", "ws-b", "idle", now);

        let map = read_agent_summaries_at(dir.path(), epoch(now));
        assert_eq!(map.get("ws-a").unwrap().working, 1);
        assert_eq!(map.get("ws-b").unwrap().idle, 1);
    }

    #[test]
    fn stale_entries_ignored() {
        let dir = TempDir::new().unwrap();
        let now = 1_000_000u64;
        let old = now - STALE_TIMEOUT.as_secs() - 1;
        write_status_file(dir.path(), "old-session", "ws", "working", old);
        write_status_file(dir.path(), "new-session", "ws", "idle", now);

        let map = read_agent_summaries_at(dir.path(), epoch(now));
        let summary = map.get("ws").unwrap();
        assert_eq!(summary.working, 0);
        assert_eq!(summary.idle, 1);
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        write_agent_status(dir.path(), "sess-123", "my-ws", AgentStatus::Waiting).unwrap();

        let map = read_agent_summaries(dir.path());
        let summary = map.get("my-ws").unwrap();
        assert_eq!(summary.waiting, 1);
    }

    #[test]
    fn remove_status() {
        let dir = TempDir::new().unwrap();
        write_agent_status(dir.path(), "sess-123", "my-ws", AgentStatus::Working).unwrap();
        remove_agent_status(dir.path(), "sess-123").unwrap();

        let map = read_agent_summaries(dir.path());
        assert!(map.is_empty());
    }

    #[test]
    fn remove_statuses_for_workspace() {
        let dir = TempDir::new().unwrap();
        write_agent_status(dir.path(), "s1", "ws-a", AgentStatus::Working).unwrap();
        write_agent_status(dir.path(), "s2", "ws-a", AgentStatus::Idle).unwrap();
        write_agent_status(dir.path(), "s3", "ws-b", AgentStatus::Working).unwrap();

        remove_agent_statuses_for_workspace(dir.path(), "ws-a");

        let map = read_agent_summaries(dir.path());
        assert!(!map.contains_key("ws-a"));
        assert_eq!(map.get("ws-b").unwrap().working, 1);
    }

    #[test]
    fn summary_display_all_statuses() {
        let s = AgentSummary {
            waiting: 2,
            working: 1,
            idle: 1,
        };
        assert_eq!(s.to_string(), "2 waiting, 1 working, 1 idle");
    }

    #[test]
    fn summary_display_single_status() {
        let s = AgentSummary {
            waiting: 0,
            working: 1,
            idle: 0,
        };
        assert_eq!(s.to_string(), "1 working");
    }

    #[test]
    fn summary_display_empty() {
        let s = AgentSummary::default();
        assert_eq!(s.to_string(), "");
        assert!(s.is_empty());
    }

    #[test]
    fn summary_most_urgent() {
        assert_eq!(
            AgentSummary {
                waiting: 1,
                working: 0,
                idle: 0
            }
            .most_urgent(),
            Some(AgentStatus::Waiting)
        );
        assert_eq!(
            AgentSummary {
                waiting: 0,
                working: 1,
                idle: 1
            }
            .most_urgent(),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            AgentSummary {
                waiting: 0,
                working: 0,
                idle: 1
            }
            .most_urgent(),
            Some(AgentStatus::Idle)
        );
        assert_eq!(AgentSummary::default().most_urgent(), None);
    }

    #[test]
    fn resolve_cwd_inside_dwm() {
        let dwm_base = PathBuf::from("/home/user/.dwm");
        let cwd = PathBuf::from("/home/user/.dwm/myrepo-abc123/my-feature/src");

        let result = resolve_workspace_from_cwd(&dwm_base, &cwd);
        assert!(result.is_some());
        let (repo_dir, ws_name) = result.unwrap();
        assert_eq!(repo_dir, PathBuf::from("/home/user/.dwm/myrepo-abc123"));
        assert_eq!(ws_name, "my-feature");
    }

    #[test]
    fn resolve_cwd_outside_dwm_no_match() {
        let dir = TempDir::new().unwrap();
        let dwm_base = dir.path().join(".dwm");
        fs::create_dir_all(&dwm_base).unwrap();

        let cwd = PathBuf::from("/some/random/dir");
        let result = resolve_workspace_from_cwd(&dwm_base, &cwd);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_cwd_main_repo() {
        let dir = TempDir::new().unwrap();
        let dwm_base = dir.path().join(".dwm");
        let repo_dir = dwm_base.join("myrepo-abc123");
        fs::create_dir_all(&repo_dir).unwrap();

        let main_repo = dir.path().join("repos").join("myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        fs::write(
            repo_dir.join(".main-repo"),
            main_repo.to_string_lossy().as_ref(),
        )
        .unwrap();
        fs::write(repo_dir.join(".vcs-type"), "git").unwrap();

        let cwd = main_repo.join("src");
        fs::create_dir_all(&cwd).unwrap();

        let result = resolve_workspace_from_cwd(&dwm_base, &cwd);
        assert!(result.is_some());
        let (resolved_repo, ws_name) = result.unwrap();
        assert_eq!(resolved_repo, repo_dir);
        assert_eq!(ws_name, "main-worktree");
    }

    #[test]
    fn hook_handler_parse_pre_tool_use() {
        let dir = TempDir::new().unwrap();
        let dwm_base = dir.path().join(".dwm");
        let repo_dir = dwm_base.join("myrepo-abc123");
        fs::create_dir_all(&repo_dir).unwrap();

        let ws_dir = repo_dir.join("my-feature");
        fs::create_dir_all(&ws_dir).unwrap();

        let (repo, ws) = resolve_workspace_from_cwd(&dwm_base, &PathBuf::from(ws_dir)).unwrap();
        write_agent_status(&repo, "test-sess", &ws, AgentStatus::Working).unwrap();

        let map = read_agent_summaries(&repo);
        assert_eq!(map.get("my-feature").unwrap().working, 1);
    }

    #[test]
    fn malformed_json_files_ignored() {
        let dir = TempDir::new().unwrap();
        let agent_dir = dir.path().join(".agent-status");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("bad.json"), "not valid json").unwrap();

        let map = read_agent_summaries(dir.path());
        assert!(map.is_empty());
    }

    #[test]
    fn non_json_files_ignored() {
        let dir = TempDir::new().unwrap();
        let agent_dir = dir.path().join(".agent-status");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("readme.txt"), "hello").unwrap();

        let map = read_agent_summaries(dir.path());
        assert!(map.is_empty());
    }

    #[test]
    fn setup_creates_fresh_settings() {
        // Test the merge logic directly
        let mut settings: serde_json::Value = serde_json::json!({});
        let dwm_hooks = dwm_hook_config();

        let hooks = settings
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| serde_json::json!({}));
        let hooks_obj = hooks.as_object_mut().unwrap();

        for (event_name, dwm_groups) in dwm_hooks.as_object().unwrap() {
            let existing = hooks_obj
                .entry(event_name)
                .or_insert_with(|| serde_json::json!([]));
            let arr = existing.as_array_mut().unwrap();
            for group in dwm_groups.as_array().unwrap() {
                arr.push(group.clone());
            }
        }

        assert!(hooks_obj.contains_key("PreToolUse"));
        assert!(hooks_obj.contains_key("Stop"));
        assert!(hooks_obj.contains_key("Notification"));
        assert!(hooks_obj.contains_key("UserPromptSubmit"));
        assert!(hooks_obj.contains_key("SessionEnd"));
    }

    #[test]
    fn setup_preserves_existing_hooks() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "my-other-tool" }] }
                ]
            }
        });

        let dwm_hooks = dwm_hook_config();
        let hooks_obj = settings["hooks"].as_object_mut().unwrap();

        for (event_name, dwm_groups) in dwm_hooks.as_object().unwrap() {
            let existing = hooks_obj
                .entry(event_name)
                .or_insert_with(|| serde_json::json!([]));
            let arr = existing.as_array_mut().unwrap();
            let already_installed = arr.iter().any(|group| {
                group
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .is_some_and(|c| c == "dwm hook-handler")
                        })
                    })
                    .unwrap_or(false)
            });
            if !already_installed {
                for group in dwm_groups.as_array().unwrap() {
                    arr.push(group.clone());
                }
            }
        }

        // PreToolUse should have both the existing and dwm hooks
        let pre_tool = hooks_obj["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 2);
    }

    #[test]
    fn setup_does_not_duplicate() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
                ]
            }
        });

        let dwm_hooks = dwm_hook_config();
        let hooks_obj = settings["hooks"].as_object_mut().unwrap();

        for (event_name, dwm_groups) in dwm_hooks.as_object().unwrap() {
            let existing = hooks_obj
                .entry(event_name)
                .or_insert_with(|| serde_json::json!([]));
            let arr = existing.as_array_mut().unwrap();
            let already_installed = arr.iter().any(|group| {
                group
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .is_some_and(|c| c == "dwm hook-handler")
                        })
                    })
                    .unwrap_or(false)
            });
            if !already_installed {
                for group in dwm_groups.as_array().unwrap() {
                    arr.push(group.clone());
                }
            }
        }

        // PreToolUse should still have just 1 entry (not duplicated)
        let pre_tool = hooks_obj["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
    }

    // --- Gap: CLI parse tests for new subcommands ---

    #[test]
    fn cli_hook_handler_parses() {
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        let cli = Cli::try_parse_from(["dwm", "hook-handler"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::HookHandler)));
    }

    #[test]
    fn cli_agent_setup_parses() {
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        let cli = Cli::try_parse_from(["dwm", "agent-setup"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::AgentSetup)));
    }

    #[test]
    fn cli_setup_parses() {
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        let cli = Cli::try_parse_from(["dwm", "setup"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Setup)));
    }

    // --- Gap: resolve_workspace_from_cwd with jj VcsType ---

    #[test]
    fn resolve_cwd_main_repo_jj() {
        let dir = TempDir::new().unwrap();
        let dwm_base = dir.path().join(".dwm");
        let repo_dir = dwm_base.join("myrepo-abc123");
        fs::create_dir_all(&repo_dir).unwrap();

        let main_repo = dir.path().join("repos").join("myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        fs::write(
            repo_dir.join(".main-repo"),
            main_repo.to_string_lossy().as_ref(),
        )
        .unwrap();
        fs::write(repo_dir.join(".vcs-type"), "jj").unwrap();

        let cwd = main_repo.join("src");
        fs::create_dir_all(&cwd).unwrap();

        let result = resolve_workspace_from_cwd(&dwm_base, &cwd);
        let (resolved_repo, ws_name) = result.unwrap();
        assert_eq!(resolved_repo, repo_dir);
        assert_eq!(ws_name, "default");
    }

    #[test]
    fn resolve_cwd_main_repo_no_vcs_type_defaults_to_jj() {
        let dir = TempDir::new().unwrap();
        let dwm_base = dir.path().join(".dwm");
        let repo_dir = dwm_base.join("myrepo-abc123");
        fs::create_dir_all(&repo_dir).unwrap();

        let main_repo = dir.path().join("repos").join("myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        fs::write(
            repo_dir.join(".main-repo"),
            main_repo.to_string_lossy().as_ref(),
        )
        .unwrap();
        // No .vcs-type file — should default to jj ("default")

        let result = resolve_workspace_from_cwd(&dwm_base, &main_repo);
        let (_resolved_repo, ws_name) = result.unwrap();
        assert_eq!(ws_name, "default");
    }

    // --- Gap: stale boundary condition (exactly at threshold) ---

    #[test]
    fn stale_boundary_exactly_at_threshold_is_not_stale() {
        let dir = TempDir::new().unwrap();
        let now = 1_000_000u64;
        let at_boundary = now - STALE_TIMEOUT.as_secs();
        write_status_file(dir.path(), "sess", "ws", "working", at_boundary);

        let map = read_agent_summaries_at(dir.path(), epoch(now));
        // updated_at is exactly at the threshold; check is `>` not `>=`, so NOT stale
        let summary = map.get("ws").unwrap();
        assert_eq!(summary.working, 1);
    }

    // --- Gap: remove_agent_status when file doesn't exist ---

    #[test]
    fn remove_nonexistent_status_is_ok() {
        let dir = TempDir::new().unwrap();
        // No status file exists; should not error
        let result = remove_agent_status(dir.path(), "nonexistent-session");
        assert!(result.is_ok());
    }

    // --- Gap: handle_hook silently ignores missing session_id/cwd ---

    #[test]
    fn resolve_cwd_dwm_base_only_returns_none() {
        // cwd is exactly the dwm_base with only one path component (repo name, no workspace)
        let dwm_base = PathBuf::from("/home/user/.dwm");
        let cwd = PathBuf::from("/home/user/.dwm/myrepo-abc123");

        let result = resolve_workspace_from_cwd(&dwm_base, &cwd);
        assert!(
            result.is_none(),
            "should need both repo and workspace components"
        );
    }

    // --- Gap: AgentStatus serde roundtrip ---

    #[test]
    fn agent_status_serde_roundtrip() {
        for status in [
            AgentStatus::Working,
            AgentStatus::Idle,
            AgentStatus::Waiting,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: AgentStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn agent_status_file_serde_roundtrip() {
        let file = AgentStatusFile {
            workspace: "my-ws".to_string(),
            status: AgentStatus::Waiting,
            updated_at: 1234567890,
        };
        let json = serde_json::to_string(&file).unwrap();
        let back: AgentStatusFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.workspace, "my-ws");
        assert_eq!(back.status, AgentStatus::Waiting);
        assert_eq!(back.updated_at, 1234567890);
    }

    // --- Gap: all stale entries → workspace not in map ---

    #[test]
    fn all_stale_entries_result_in_empty_map() {
        let dir = TempDir::new().unwrap();
        let now = 1_000_000u64;
        let old = now - STALE_TIMEOUT.as_secs() - 100;
        write_status_file(dir.path(), "s1", "ws", "working", old);
        write_status_file(dir.path(), "s2", "ws", "waiting", old);

        let map = read_agent_summaries_at(dir.path(), epoch(now));
        assert!(map.is_empty());
    }

    // --- Gap: write_agent_status overwrites existing session file ---

    #[test]
    fn write_overwrites_previous_status_for_same_session() {
        let dir = TempDir::new().unwrap();
        write_agent_status(dir.path(), "sess-1", "ws", AgentStatus::Working).unwrap();
        write_agent_status(dir.path(), "sess-1", "ws", AgentStatus::Waiting).unwrap();

        let map = read_agent_summaries(dir.path());
        let summary = map.get("ws").unwrap();
        // Should have 1 waiting, NOT 1 working + 1 waiting
        assert_eq!(summary.waiting, 1);
        assert_eq!(summary.working, 0);
    }

    // --- Gap: dwm_hook_config produces expected event keys ---

    #[test]
    fn hook_config_has_expected_events() {
        let config = dwm_hook_config();
        let obj = config.as_object().unwrap();
        assert!(obj.contains_key("PreToolUse"));
        assert!(obj.contains_key("Stop"));
        assert!(obj.contains_key("Notification"));
        assert!(obj.contains_key("UserPromptSubmit"));
        assert!(obj.contains_key("SessionEnd"));
        assert_eq!(obj.len(), 5);
    }

    #[test]
    fn hooks_already_installed_detects_presence() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
                ],
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
                ],
                "Notification": [
                    { "matcher": "idle_prompt|permission_prompt", "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
                ],
                "UserPromptSubmit": [
                    { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
                ],
                "SessionEnd": [
                    { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
                ]
            }
        });
        assert!(hooks_already_installed(&settings));
    }

    #[test]
    fn hooks_already_installed_false_when_missing() {
        let settings = serde_json::json!({});
        assert!(!hooks_already_installed(&settings));
    }

    #[test]
    fn hooks_already_installed_false_when_partial() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
                ]
            }
        });
        assert!(!hooks_already_installed(&settings));
    }

    #[test]
    fn hook_config_notification_has_matcher() {
        let config = dwm_hook_config();
        let notif = config["Notification"].as_array().unwrap();
        assert_eq!(notif.len(), 1);
        let matcher = notif[0]["matcher"].as_str().unwrap();
        assert!(matcher.contains("idle_prompt"));
        assert!(matcher.contains("permission_prompt"));
    }

    #[test]
    fn merge_dwm_hooks_creates_fresh_settings() {
        let settings = serde_json::json!({});
        let merged = merge_dwm_hooks(settings).unwrap();

        let hooks = merged["hooks"].as_object().unwrap();
        assert!(hooks.contains_key("PreToolUse"));
        assert!(hooks.contains_key("Stop"));
        assert!(hooks.contains_key("Notification"));
        assert!(hooks.contains_key("UserPromptSubmit"));
        assert!(hooks.contains_key("SessionEnd"));

        // Check one specifically
        let pre_tool = hooks["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(pre_tool[0]["hooks"][0]["command"], "dwm hook-handler");
    }

    #[test]
    fn merge_dwm_hooks_preserves_existing_hooks() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "my-other-tool" }] }
                ]
            },
            "other_setting": "val"
        });

        let merged = merge_dwm_hooks(settings).unwrap();
        let pre_tool = merged["hooks"]["PreToolUse"].as_array().unwrap();

        assert_eq!(pre_tool.len(), 2);
        assert_eq!(pre_tool[0]["hooks"][0]["command"], "my-other-tool");
        assert_eq!(pre_tool[1]["hooks"][0]["command"], "dwm hook-handler");
        assert_eq!(merged["other_setting"], "val");
    }

    #[test]
    fn merge_dwm_hooks_does_not_duplicate() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "dwm hook-handler" }] }
                ]
            }
        });

        let merged = merge_dwm_hooks(settings).unwrap();
        let pre_tool = merged["hooks"]["PreToolUse"].as_array().unwrap();

        // Should still be just 1
        assert_eq!(pre_tool.len(), 1);
    }

    #[test]
    fn merge_dwm_hooks_errors_on_invalid_structure() {
        let settings = serde_json::json!([]); // Not an object
        assert!(merge_dwm_hooks(settings).is_err());

        let settings = serde_json::json!({ "hooks": [] }); // hooks should be an object
        assert!(merge_dwm_hooks(settings).is_err());

        let settings = serde_json::json!({ "hooks": { "PreToolUse": {} } }); // event should be an array
        assert!(merge_dwm_hooks(settings).is_err());
    }
}
