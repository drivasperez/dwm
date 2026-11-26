use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::vcs::VcsType;
use crate::workspace;

/// How long before a status file is considered stale and ignored.
const STALE_TIMEOUT: Duration = Duration::from_secs(600);

/// Possible states of a Claude Code agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Working,
    Idle,
    Waiting,
    /// Finished a piece of work the person hasn't looked at yet. Only Rex
    /// reports this (OSC 7501 `done`); hooks can't tell it apart from idle.
    Done,
}

impl AgentStatus {
    /// The word dwm shows for this status.
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Working => "working",
            AgentStatus::Idle => "idle",
            AgentStatus::Waiting => "waiting",
            AgentStatus::Done => "done",
        }
    }

    /// Higher is more in need of attention.
    pub fn urgency(self) -> u8 {
        match self {
            AgentStatus::Idle => 0,
            AgentStatus::Done => 1,
            AgentStatus::Working => 2,
            AgentStatus::Waiting => 3,
        }
    }
}

/// On-disk representation of a single agent's status file.
#[derive(Debug, Deserialize, Serialize)]
pub struct AgentStatusFile {
    pub workspace: String,
    pub status: AgentStatus,
    pub updated_at: u64,
}

/// One agent's own words about what it is doing, as reported to Rex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentNote {
    pub status: AgentStatus,
    pub text: String,
}

/// Aggregated agent counts for a single workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentSummary {
    pub waiting: u32,
    pub working: u32,
    pub done: u32,
    pub idle: u32,
    /// Messages from agents that report one (only through Rex), most urgent
    /// first.
    pub notes: Vec<AgentNote>,
}

impl AgentSummary {
    pub fn is_empty(&self) -> bool {
        self.waiting == 0 && self.working == 0 && self.done == 0 && self.idle == 0
    }

    /// Keep an agent's message, most urgent first.
    pub fn add_note(&mut self, note: AgentNote) {
        let at = self
            .notes
            .iter()
            .position(|n| n.status.urgency() < note.status.urgency())
            .unwrap_or(self.notes.len());
        self.notes.insert(at, note);
    }

    /// The message most in need of attention, if any agent left one.
    pub fn top_note(&self) -> Option<&AgentNote> {
        self.notes.first()
    }

    /// Count one more agent with the given status.
    pub fn add(&mut self, status: AgentStatus) {
        match status {
            AgentStatus::Working => self.working += 1,
            AgentStatus::Idle => self.idle += 1,
            AgentStatus::Waiting => self.waiting += 1,
            AgentStatus::Done => self.done += 1,
        }
    }

    /// Return the most urgent status present, for color selection.
    pub fn most_urgent(&self) -> Option<AgentStatus> {
        if self.waiting > 0 {
            Some(AgentStatus::Waiting)
        } else if self.working > 0 {
            Some(AgentStatus::Working)
        } else if self.done > 0 {
            Some(AgentStatus::Done)
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
        if self.done > 0 {
            parts.push(format!("{} done", self.done));
        }
        if self.idle > 0 {
            parts.push(format!("{} idle", self.idle));
        }
        write!(f, "{}", parts.join(", "))
    }
}

/// Return the `agent-status` directory inside a per-repo dwm dir (`<repo>.dwm/`).
fn agent_status_dir(repo_dir: &Path) -> PathBuf {
    repo_dir.join("agent-status")
}

/// Convert a unix timestamp to a [`SystemTime`].
fn system_time_from_epoch_secs(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Agent summaries for one repo from every source: the status files that
/// hooks write and, inside Rex, the status Rex's panes report.
pub fn current_agent_summaries(repo_dir: &Path) -> HashMap<String, AgentSummary> {
    read_agent_summaries_with(repo_dir, rex_summaries().as_ref())
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

        map.entry(status_file.workspace.clone())
            .or_default()
            .add(status_file.status);
    }

    map
}

/// Agent summaries derived from Rex panes, keyed by the real path of each
/// repo's `<repo>.dwm/` directory and then by workspace name.
pub type RexSummaries = HashMap<PathBuf, HashMap<String, AgentSummary>>;

/// Count every Rex pane that reports a status as one agent in the workspace
/// its working directory is in.
pub fn summaries_from_panes(panes: &[crate::rex::Pane]) -> RexSummaries {
    let mut out = RexSummaries::new();
    for pane in panes {
        let (Some(cwd), Some(status)) = (&pane.cwd, pane.agent_status()) else {
            continue;
        };
        let Some((dwm, ws_name)) = resolve_workspace_from_cwd(cwd) else {
            continue;
        };
        let summary = out
            .entry(crate::rex::real_path(&dwm))
            .or_default()
            .entry(ws_name)
            .or_default();
        summary.add(status);
        if let Some(note) = pane.agent_note() {
            summary.add_note(note);
        }
    }
    out
}

/// Ask Rex for agent status, when dwm runs inside Rex. `None` means there is
/// nothing from Rex to use, and the status files are the only source.
pub fn rex_summaries() -> Option<RexSummaries> {
    let rex = crate::rex::connect()?;
    rex.panes().ok().map(|panes| summaries_from_panes(&panes))
}

/// Combine the status files' summaries for one repo with Rex's.
///
/// For a workspace that has panes reporting status in Rex, Rex's summary
/// replaces the files'. An agent running in Rex may also have hooks writing
/// status files, and counting both would count it twice. The files still
/// cover agents running outside Rex.
pub fn combine_summaries(
    repo_dir: &Path,
    mut from_files: HashMap<String, AgentSummary>,
    rex: Option<&RexSummaries>,
) -> HashMap<String, AgentSummary> {
    if let Some(from_rex) = rex.and_then(|r| r.get(&crate::rex::real_path(repo_dir))) {
        for (ws, summary) in from_rex {
            from_files.insert(ws.clone(), summary.clone());
        }
    }
    from_files
}

/// Agent summaries for one repo from the status files and, given a Rex scan,
/// from Rex.
pub fn read_agent_summaries_with(
    repo_dir: &Path,
    rex: Option<&RexSummaries>,
) -> HashMap<String, AgentSummary> {
    combine_summaries(repo_dir, read_agent_summaries(repo_dir), rex)
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

/// Resolve a `cwd` path to `(agent_status_dir, workspace_name)` using only the
/// filesystem — no VCS subprocess calls.
///
/// Returns `None` if the path doesn't correspond to a dwm-managed workspace.
///
/// The returned `agent_status_dir` is the per-repo `<repo>.dwm/` directory (which is
/// the directory the agent-status JSON files live under, in `agent-status/`).
fn resolve_workspace_from_cwd(cwd: &Path) -> Option<(PathBuf, String)> {
    // Case 1: cwd is inside a dwm workspace under <root>.dwm/worktrees/<ws>/...
    if let Some((root, ws_name, _ws_path)) = workspace::find_dwm_workspace(cwd) {
        return Some((workspace::dwm_dir(&root), ws_name));
    }

    // Case 2: cwd is somewhere inside a repo root that has a sibling
    // `<root>.dwm/` dir. Walk up looking for a directory that has BOTH a VCS
    // marker and a dwm dir. That's the main repo for a dwm-managed clone.
    let mut current = cwd.to_path_buf();
    loop {
        let dwm = workspace::dwm_dir(&current);
        if dwm.is_dir() {
            // Determine main workspace name from VCS type.
            let ws_name = if current.join(".jj").is_dir() {
                VcsType::Jj.main_workspace_name()
            } else if current.join(".git").exists() {
                VcsType::Git.main_workspace_name()
            } else {
                // No VCS marker here; keep walking up.
                if !current.pop() {
                    return None;
                }
                continue;
            };
            return Some((dwm, ws_name.to_string()));
        }
        if !current.pop() {
            break;
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

    let cwd = PathBuf::from(cwd_str);
    let (repo_dir, ws_name) = match resolve_workspace_from_cwd(&cwd) {
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

pub(crate) fn display_path(path: &Path) -> String {
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

/// Read a yes/no answer from the terminal. Anything but `y` is no, as is
/// having no terminal to ask.
pub(crate) fn confirm_from_tty() -> Result<bool> {
    let response = match std::fs::File::open("/dev/tty") {
        Ok(f) => {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut line)?;
            line
        }
        Err(_) => String::new(),
    };
    Ok(response.trim().eq_ignore_ascii_case("y"))
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
    if !confirm_from_tty()? {
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
        let agent_dir = dir.join("agent-status");
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
            done: 0,
            idle: 1,
            notes: Vec::new(),
        };
        assert_eq!(s.to_string(), "2 waiting, 1 working, 1 idle");
    }

    #[test]
    fn summary_display_single_status() {
        let s = AgentSummary {
            waiting: 0,
            working: 1,
            done: 0,
            idle: 0,
            notes: Vec::new(),
        };
        assert_eq!(s.to_string(), "1 working");
    }

    #[test]
    fn summary_display_includes_done() {
        let mut s = AgentSummary::default();
        s.add(AgentStatus::Done);
        s.add(AgentStatus::Idle);
        assert_eq!(s.to_string(), "1 done, 1 idle");
        assert_eq!(s.most_urgent(), Some(AgentStatus::Done));
        s.add(AgentStatus::Working);
        assert_eq!(s.most_urgent(), Some(AgentStatus::Working));
    }

    #[test]
    fn notes_are_kept_most_urgent_first() {
        let note = |status, text: &str| AgentNote {
            status,
            text: text.into(),
        };
        let mut s = AgentSummary::default();
        s.add_note(note(AgentStatus::Idle, "ready"));
        s.add_note(note(AgentStatus::Working, "tests"));
        s.add_note(note(AgentStatus::Waiting, "allow?"));
        s.add_note(note(AgentStatus::Working, "build"));
        let texts: Vec<_> = s.notes.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(texts, ["allow?", "tests", "build", "ready"]);
        assert_eq!(s.top_note().unwrap().text, "allow?");
    }

    #[test]
    fn status_urgency_order() {
        let mut all = [
            AgentStatus::Working,
            AgentStatus::Idle,
            AgentStatus::Waiting,
            AgentStatus::Done,
        ];
        all.sort_by_key(|s| s.urgency());
        assert_eq!(
            all,
            [
                AgentStatus::Idle,
                AgentStatus::Done,
                AgentStatus::Working,
                AgentStatus::Waiting
            ]
        );
    }

    // ── Rex as a status source ──────────────────────────────────────

    fn rex_repo(tmp: &Path) -> (PathBuf, PathBuf) {
        let repo = tmp.join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        let ws = workspace::worktrees_dir(&repo).join("feat");
        fs::create_dir_all(ws.join("src")).unwrap();
        (repo, ws)
    }

    #[test]
    fn summaries_from_panes_maps_cwd_to_workspace() {
        use crate::rex::ProgramState::{Blocked, Done, Working};
        use crate::rex::tests::pane;
        let tmp = TempDir::new().unwrap();
        let (repo, ws) = rex_repo(tmp.path());
        let outside = tmp.path().join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        let panes = vec![
            pane("1", "s", &ws.join("src"), &[Working]),
            pane("2", "s", &ws, &[Blocked]),
            pane("3", "s", &repo, &[Done]),
            pane("4", "s", &ws, &[]),             // a plain shell
            pane("5", "s", &outside, &[Working]), // not a dwm repo
        ];
        let mut panes = panes;
        panes[1].records[0].msg = Some("Allow edit to src/foo.rs?".into());
        let map = summaries_from_panes(&panes);
        assert_eq!(map.len(), 1, "{map:?}");
        let repo_map = &map[&crate::rex::real_path(&workspace::dwm_dir(&repo))];
        let feat = &repo_map["feat"];
        assert_eq!((feat.working, feat.waiting, feat.idle), (1, 1, 0));
        let note = feat.top_note().unwrap();
        assert_eq!(
            (note.status, note.text.as_str()),
            (AgentStatus::Waiting, "Allow edit to src/foo.rs?")
        );
        assert_eq!(repo_map["default"].done, 1);
    }

    #[test]
    fn rex_replaces_file_status_per_workspace() {
        let tmp = TempDir::new().unwrap();
        let (repo, _) = rex_repo(tmp.path());
        let dwm = workspace::dwm_dir(&repo);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Hooks see two agents: one also running in Rex, one elsewhere.
        write_status_file(&dwm, "s1", "feat", "working", now);
        write_status_file(&dwm, "s2", "other", "waiting", now);

        let mut from_rex = HashMap::new();
        let mut feat = AgentSummary::default();
        feat.add(AgentStatus::Done);
        from_rex.insert("feat".to_string(), feat.clone());
        let mut rex = RexSummaries::new();
        rex.insert(crate::rex::real_path(&dwm), from_rex);

        let combined = read_agent_summaries_with(&dwm, Some(&rex));
        assert_eq!(combined["feat"], feat, "Rex wins where it has panes");
        assert_eq!(combined["other"].waiting, 1, "files cover the rest");

        let files_only = read_agent_summaries_with(&dwm, None);
        assert_eq!(files_only["feat"].working, 1);
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
                done: 0,
                idle: 0,
                notes: Vec::new(),
            }
            .most_urgent(),
            Some(AgentStatus::Waiting)
        );
        assert_eq!(
            AgentSummary {
                waiting: 0,
                working: 1,
                done: 0,
                idle: 1,
                notes: Vec::new(),
            }
            .most_urgent(),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            AgentSummary {
                waiting: 0,
                working: 0,
                done: 0,
                idle: 1,
                notes: Vec::new(),
            }
            .most_urgent(),
            Some(AgentStatus::Idle)
        );
        assert_eq!(AgentSummary::default().most_urgent(), None);
    }

    #[test]
    fn resolve_cwd_inside_dwm_workspace() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        let ws = workspace::worktrees_dir(&repo).join("my-feature");
        fs::create_dir_all(ws.join("src")).unwrap();

        let result = resolve_workspace_from_cwd(&ws.join("src"));
        assert!(result.is_some());
        let (agent_dir, ws_name) = result.unwrap();
        assert_eq!(agent_dir, workspace::dwm_dir(&repo));
        assert_eq!(ws_name, "my-feature");
    }

    #[test]
    fn resolve_cwd_outside_no_match() {
        let dir = TempDir::new().unwrap();
        // Random dir without VCS metadata.
        let result = resolve_workspace_from_cwd(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn resolve_cwd_main_repo_git() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(workspace::dwm_dir(&repo)).unwrap();
        let cwd = repo.join("src");
        fs::create_dir_all(&cwd).unwrap();

        let result = resolve_workspace_from_cwd(&cwd);
        let (agent_dir, ws_name) = result.unwrap();
        assert_eq!(agent_dir, workspace::dwm_dir(&repo));
        assert_eq!(ws_name, "main-worktree");
    }

    #[test]
    fn resolve_cwd_main_repo_jj() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        fs::create_dir_all(workspace::dwm_dir(&repo)).unwrap();
        let cwd = repo.join("src");
        fs::create_dir_all(&cwd).unwrap();

        let result = resolve_workspace_from_cwd(&cwd);
        let (_agent_dir, ws_name) = result.unwrap();
        assert_eq!(ws_name, "default");
    }

    #[test]
    fn resolve_cwd_main_repo_no_dwm_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        // No <repo>.dwm/ → not a dwm-managed repo.
        let result = resolve_workspace_from_cwd(&repo);
        assert!(result.is_none());
    }

    #[test]
    fn hook_handler_parse_pre_tool_use() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        let ws_dir = workspace::worktrees_dir(&repo).join("my-feature");
        fs::create_dir_all(&ws_dir).unwrap();

        let (agent_dir, ws) = resolve_workspace_from_cwd(&ws_dir).unwrap();
        write_agent_status(&agent_dir, "test-sess", &ws, AgentStatus::Working).unwrap();

        let map = read_agent_summaries(&agent_dir);
        assert_eq!(map.get("my-feature").unwrap().working, 1);
    }

    #[test]
    fn malformed_json_files_ignored() {
        let dir = TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent-status");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("bad.json"), "not valid json").unwrap();

        let map = read_agent_summaries(dir.path());
        assert!(map.is_empty());
    }

    #[test]
    fn non_json_files_ignored() {
        let dir = TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent-status");
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
