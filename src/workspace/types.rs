use anyhow::Result;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::names::WorkspaceName;
use crate::rex::{self};
use crate::{agent, pool, vcs};

// ── Enums & shared types ────────────────────────────────────────────

/// Whether a workspace's changes have been merged into trunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeStatus {
    Merged,
    Unmerged,
}

/// Controls whether progress messages are printed to stderr during deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutput {
    /// Print progress messages to stderr.
    Verbose,
    /// Suppress progress messages (used by the TUI which owns the alternate screen).
    Quiet,
}

/// Where `switch`, `new` and the picker put you once the workspace is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Inside Rex, focus a pane already in the workspace or open a session
    /// for it. Elsewhere, change directory.
    Rex,
    /// Always change the current shell's directory.
    Here,
}

/// Whether a person is at the terminal dwm runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Interactive,
    /// Run by an agent or a script, with no one watching stderr.
    Unattended,
}

/// The placement to use for a request. An unattended dwm, such as one an
/// agent runs inside a Rex pane, changes directory rather than opening Rex
/// sessions and switching the app's view away from what the person is doing.
pub fn effective_placement(requested: Placement, terminal: Terminal) -> Placement {
    match terminal {
        Terminal::Interactive => requested,
        Terminal::Unattended => Placement::Here,
    }
}

/// What `dwm delete` does about Rex panes still open in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPanes {
    /// Refuse to delete, naming the panes.
    Refuse,
    /// Ask on the terminal whether to close them.
    Ask,
    /// Close them, ending the programs in them. [`OpenPanes::Ask`] does this
    /// once the person agrees; nothing else asks for it outside tests.
    #[cfg_attr(not(test), allow(dead_code))]
    Close,
}

/// Whether `dwm prune` only reports or also deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneMode {
    /// Report each workspace's verdict without deleting anything.
    DryRun,
    /// Delete every workspace whose verdict is prunable.
    Delete,
}

/// Common dependencies threaded through workspace operations, grouped so they
/// can be injected in tests without touching the real filesystem or VCS.
pub(crate) struct WorkspaceDeps {
    pub backend: Box<dyn vcs::VcsBackend>,
    pub cwd: PathBuf,
    /// The Rex server, when dwm runs inside Rex.
    pub rex: Option<Box<dyn crate::rex::RexApi>>,
}

/// What [`new_workspace_inner`] made.
#[derive(Debug)]
pub(crate) struct NewOutcome {
    pub name: WorkspaceName,
    /// Where the shell should `cd`, unless Rex showed a session instead.
    pub cd: Option<PathBuf>,
}

/// What [`prune_workspaces_inner`] decided and did.
#[derive(Debug, Default)]
pub(crate) struct PruneOutcome {
    /// Every non-main workspace with its verdict, sorted by name.
    pub verdicts: Vec<(String, vcs::PruneVerdict)>,
    /// Workspaces that were actually deleted.
    pub deleted: Vec<String>,
    /// Where the shell should `cd` if cwd was inside a deleted workspace.
    pub redirect: Option<PathBuf>,
}

/// All data needed to display a single row in the workspace picker or status output.
#[derive(Debug)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: PathBuf,
    pub last_modified: Option<SystemTime>,
    pub diff_stat: vcs::DiffStat,
    pub is_main: bool,
    pub change_id: String,
    pub description: String,
    pub bookmarks: Vec<String>,
    pub is_stale: bool,
    pub repo_name: Option<String>,
    pub main_repo_path: PathBuf,
    pub vcs_type: vcs::VcsType,
    pub agent_status: Option<agent::AgentSummary>,
    /// A Rex pane open in the workspace, when dwm runs inside Rex.
    pub rex_pane: Option<rex::PaneRef>,
    /// The workspace's lease, if one still holds it.
    pub lease: Option<pool::Lease>,
}

/// Inputs for the per-workspace fan-out in [`list_workspace_entries_inner`].
pub(crate) struct EntryCandidate {
    pub name: WorkspaceName,
    pub path: PathBuf,
    pub is_main: bool,
    pub info: vcs::WorkspaceInfo,
    pub has_info: bool,
}

/// Per-workspace fields computed in parallel via VCS calls.
pub(crate) struct ComputedEntry {
    pub diff_stat: vcs::DiffStat,
    pub description: String,
    pub modified: Option<SystemTime>,
    pub is_stale: bool,
}

/// Number of days of inactivity after which a workspace is considered stale.
pub(crate) const STALE_DAYS: u64 = 30;

/// Determine whether a non-main workspace should be shown as stale.
///
/// A workspace is stale if it has been merged into trunk, or if its last
/// modification time is more than [`STALE_DAYS`] days in the past.
pub(crate) fn compute_is_stale(merged: MergeStatus, last_modified: Option<SystemTime>) -> bool {
    if merged == MergeStatus::Merged {
        return true;
    }
    stale_by_mtime(last_modified)
}

/// `true` if `last_modified` is older than [`STALE_DAYS`]. Used to skip the
/// VCS-subprocess merged probe when inactivity alone is enough to mark a
/// workspace stale.
pub(crate) fn stale_by_mtime(last_modified: Option<SystemTime>) -> bool {
    if let Some(time) = last_modified
        && let Ok(duration) = time.elapsed()
    {
        return duration.as_secs() > STALE_DAYS * 86400;
    }
    false
}

/// Format a [`SystemTime`] as a human-readable relative age string such as
/// `"5m ago"`, `"3h ago"`, or `"2mo ago"`. Returns `"unknown"` when `time`
/// is `None` or when the elapsed time cannot be computed.
pub fn format_time_ago(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "unknown".to_string();
    };
    let Ok(duration) = time.elapsed() else {
        return "unknown".to_string();
    };
    let secs = duration.as_secs();
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{}d ago", days);
    }
    let months = days / 30;
    format!("{}mo ago", months)
}

/// Resolve the repo root for the current working directory, whether the cwd
/// is inside a dwm workspace (`<root>.dwm/worktrees/<ws>/...`) or anywhere
/// else inside the repo.
pub(crate) fn resolve_repo_root(deps: &WorkspaceDeps) -> Result<PathBuf> {
    if let Some((root, _, _)) = super::layout::find_dwm_workspace(&deps.cwd) {
        return Ok(root);
    }
    deps.backend.root_from(&deps.cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_merged_workspace_without_time_is_stale() {
        assert!(compute_is_stale(MergeStatus::Merged, None));
    }

    #[test]
    fn stale_old_workspace_is_stale() {
        let old_time = SystemTime::now() - std::time::Duration::from_secs(86400 * 31);
        assert!(compute_is_stale(MergeStatus::Unmerged, Some(old_time)));
    }

    #[test]
    fn stale_recent_workspace_is_not_stale() {
        let recent = SystemTime::now() - std::time::Duration::from_secs(86400 * 5);
        assert!(!compute_is_stale(MergeStatus::Unmerged, Some(recent)));
    }

    #[test]
    fn stale_unknown_time_not_merged_is_not_stale() {
        assert!(!compute_is_stale(MergeStatus::Unmerged, None));
    }

    #[test]
    fn stale_by_mtime_old_is_true() {
        let old = SystemTime::now() - std::time::Duration::from_secs(86400 * 31);
        assert!(stale_by_mtime(Some(old)));
    }

    #[test]
    fn stale_by_mtime_recent_is_false() {
        let recent = SystemTime::now() - std::time::Duration::from_secs(86400 * 5);
        assert!(!stale_by_mtime(Some(recent)));
    }

    #[test]
    fn stale_by_mtime_unknown_is_false() {
        assert!(!stale_by_mtime(None));
    }

    #[test]
    fn format_time_ago_none_returns_unknown() {
        assert_eq!(format_time_ago(None), "unknown");
    }

    #[test]
    fn format_time_ago_just_now() {
        let time = SystemTime::now() - std::time::Duration::from_secs(30);
        assert_eq!(format_time_ago(Some(time)), "just now");
    }

    #[test]
    fn format_time_ago_minutes() {
        let time = SystemTime::now() - std::time::Duration::from_secs(300);
        assert_eq!(format_time_ago(Some(time)), "5m ago");
    }

    #[test]
    fn format_time_ago_hours() {
        let time = SystemTime::now() - std::time::Duration::from_secs(7200);
        assert_eq!(format_time_ago(Some(time)), "2h ago");
    }

    #[test]
    fn format_time_ago_days() {
        let time = SystemTime::now() - std::time::Duration::from_secs(86400 * 5);
        assert_eq!(format_time_ago(Some(time)), "5d ago");
    }

    #[test]
    fn format_time_ago_months() {
        let time = SystemTime::now() - std::time::Duration::from_secs(86400 * 60);
        assert_eq!(format_time_ago(Some(time)), "2mo ago");
    }

    #[test]
    fn unattended_dwm_never_moves_rex() {
        // Regression guard: an agent running `dwm switch` inside a Rex pane
        // has REX_SERVER set, but must not switch the person's view.
        assert_eq!(
            effective_placement(Placement::Rex, Terminal::Unattended),
            Placement::Here
        );
        assert_eq!(
            effective_placement(Placement::Rex, Terminal::Interactive),
            Placement::Rex
        );
        assert_eq!(
            effective_placement(Placement::Here, Terminal::Interactive),
            Placement::Here
        );
    }

    #[test]
    fn stale_merged_workspace_is_stale() {
        assert!(compute_is_stale(
            MergeStatus::Merged,
            Some(SystemTime::now())
        ));
    }
}
