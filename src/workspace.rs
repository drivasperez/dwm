use anyhow::{Context, Result, bail};
use owo_colors::OwoColorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{agent, hooks, names, vcs};

/// Whether a workspace's changes have been merged into trunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeStatus {
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

/// Return `true` if `cwd` is equal to or a subdirectory of `ws_path`.
fn is_inside(cwd: &std::path::Path, ws_path: &std::path::Path) -> bool {
    cwd.starts_with(ws_path)
}

/// Return the per-repo dwm directory: `<repo_root>/.dwm/`.
fn dwm_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".dwm")
}

/// Return `true` if `dir` looks like a VCS root (contains `.jj` or `.git`).
fn is_vcs_root(dir: &Path) -> bool {
    dir.join(".jj").is_dir() || dir.join(".git").exists()
}

/// Walk up from `cwd` looking for `<repo_root>/.dwm/<workspace>/`.
///
/// Returns `Some((repo_root, workspace_name, workspace_path))` when the cwd
/// is inside a dwm-managed workspace, or `None` otherwise.
pub fn find_dwm_workspace(cwd: &Path) -> Option<(PathBuf, String, PathBuf)> {
    // We're looking for the pattern .../<repo_root>/.dwm/<ws_name>/...
    // Walk from cwd upwards and check each ancestor.
    let mut current = cwd.to_path_buf();
    loop {
        // If `current`'s parent is `.dwm` and that dir's parent is a VCS root,
        // then `current` is the workspace dir.
        if let Some(parent) = current.parent()
            && parent.file_name().and_then(|n| n.to_str()) == Some(".dwm")
            && let Some(grandparent) = parent.parent()
            && is_vcs_root(grandparent)
        {
            let ws_name = current
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Some((grandparent.to_path_buf(), ws_name, current));
        }
        if !current.pop() {
            break;
        }
    }
    None
}

/// Return the path to the cross-platform repos registry file used by `--all`.
fn registry_path() -> Result<PathBuf> {
    let data = dirs::data_dir().context("could not determine data directory")?;
    Ok(data.join("dwm").join("repos.txt"))
}

/// Append `repo_root` to the registry file if not already present.
///
/// Best-effort: errors are returned but the caller may choose to ignore them.
fn register_repo(repo_root: &Path) -> Result<()> {
    let path = registry_path()?;
    let canonical = repo_root.to_string_lossy().into_owned();

    // Read existing entries (if any) so we can dedupe.
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == canonical) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Append, ensuring a trailing newline.
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{}", canonical)?;
    Ok(())
}

/// Read the registry, drop entries whose paths no longer exist or are no longer
/// dwm-managed (no `.dwm/` subdir), rewrite the file with the survivors, and
/// return them.
fn read_and_prune_registry() -> Result<Vec<PathBuf>> {
    let path = registry_path()?;
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut survivors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut changed = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            changed = true;
            continue;
        }
        if !seen.insert(trimmed.to_string()) {
            // duplicate
            changed = true;
            continue;
        }
        let p = PathBuf::from(trimmed);
        if !p.exists() || !dwm_dir(&p).exists() {
            changed = true;
            continue;
        }
        survivors.push(p);
    }

    if changed {
        // Rewrite with surviving entries. Best-effort: ignore write errors,
        // since concurrent writers may race with us.
        let mut buf = String::new();
        for p in &survivors {
            buf.push_str(&p.to_string_lossy());
            buf.push('\n');
        }
        let _ = fs::write(&path, buf);
    }

    Ok(survivors)
}

/// Append `.dwm/` to the appropriate ignore file for `repo_root` if not present.
///
/// - For repos with a real `.git` directory we append to `.git/info/exclude`
///   (a per-clone, untracked ignore list).
/// - For jj-only repos we append to `<repo_root>/.gitignore` (jj honours the
///   gitignore syntax even when there is no git directory).
fn ensure_dwm_ignored(repo_root: &Path) -> Result<()> {
    let git_dir = repo_root.join(".git");
    let target = if git_dir.is_dir() {
        git_dir.join("info").join("exclude")
    } else {
        repo_root.join(".gitignore")
    };

    let existing = fs::read_to_string(&target).unwrap_or_default();
    let already = existing.lines().any(|l| {
        let t = l.trim();
        t == ".dwm" || t == ".dwm/" || t == "/.dwm" || t == "/.dwm/"
    });
    if already {
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, ".dwm/")?;
    Ok(())
}

/// Common dependencies threaded through workspace operations, grouped so they
/// can be injected in tests without touching the real filesystem or VCS.
struct WorkspaceDeps {
    backend: Box<dyn vcs::VcsBackend>,
    cwd: PathBuf,
}

/// Resolve the repo root for the current working directory, whether the cwd
/// is inside a dwm workspace (`<root>/.dwm/<ws>/...`) or anywhere else inside
/// the repo.
fn resolve_repo_root(deps: &WorkspaceDeps) -> Result<PathBuf> {
    if let Some((root, _, _)) = find_dwm_workspace(&deps.cwd) {
        return Ok(root);
    }
    deps.backend.root_from(&deps.cwd)
}

/// Create a new workspace, auto-detecting the VCS from the current directory.
///
/// Prints the new workspace path to stdout so the shell wrapper can `cd` into it.
pub fn new_workspace(name: Option<String>, at: Option<&str>, from: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    // If we're inside a dwm workspace the cwd may be a workspace dir; we still
    // want to detect from the underlying VCS, which works because both jj and
    // git treat the workspace dir as part of the same repo.
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps { backend, cwd };
    new_workspace_inner(&deps, name, at, from)
}

/// Testable core of [`new_workspace`] that accepts injected [`WorkspaceDeps`].
fn new_workspace_inner(
    deps: &WorkspaceDeps,
    name: Option<String>,
    at: Option<&str>,
    from: Option<&str>,
) -> Result<()> {
    let root = resolve_repo_root(deps)?;
    let dwm = dwm_dir(&root);
    let first_time = !dwm.exists();
    fs::create_dir_all(&dwm)?;

    if first_time {
        // Best-effort: don't fail workspace creation if ignore-update fails.
        if let Err(e) = ensure_dwm_ignored(&root) {
            eprintln!(
                "{} could not update ignore rules: {}",
                "warning:".yellow(),
                e
            );
        }
    }

    // Best-effort: register the repo for `--all`.
    let _ = register_repo(&root);

    // Resolve --from to a change ID by looking up the source workspace.
    let resolved_at;
    let at = if let Some(ws_name) = from {
        let workspaces = deps.backend.workspace_list(&root)?;
        let (_name, info) = workspaces
            .iter()
            .find(|(n, _)| n == ws_name)
            .with_context(|| format!("workspace '{}' not found", ws_name))?;
        resolved_at = info.change_id.clone();
        Some(resolved_at.as_str())
    } else {
        at
    };

    let ws_name = match name {
        Some(n) => {
            if n.starts_with('.') {
                bail!("workspace name cannot start with '.'");
            }
            n
        }
        None => names::generate_unique(&dwm),
    };

    let ws_path = dwm.join(&ws_name);
    if ws_path.exists() {
        bail!(
            "workspace '{}' already exists at {}",
            ws_name,
            ws_path.display()
        );
    }

    // Load post-creation hooks before doing any VCS work so a malformed
    // .dwm.toml / conductor.json is reported up-front rather than after we've
    // already provisioned the workspace.
    let loaded_hooks = hooks::load(&root)?;

    eprintln!("{} workspace '{}'...", "creating".cyan(), ws_name.bold());
    deps.backend.workspace_add(&root, &ws_path, &ws_name, at)?;
    eprintln!(
        "{} workspace '{}' created at {}",
        "✓".green(),
        ws_name.bold(),
        ws_path.display().dimmed()
    );

    let hook_ctx = hooks::HookContext {
        workspace_path: ws_path.clone(),
        workspace_name: ws_name.clone(),
        repo_root: root.clone(),
        vcs_type: deps.backend.vcs_type(),
        from_workspace: from.map(|s| s.to_string()),
    };
    hooks::run_setup(&loaded_hooks, &hook_ctx)?;

    // stdout: path for shell wrapper to cd into
    println!("{}", ws_path.display());
    Ok(())
}

/// Detect the VCS backend for `cwd`, accounting for the case where `cwd` is
/// inside a dwm workspace dir.
fn detect_backend_from_cwd(cwd: &Path) -> Result<Box<dyn vcs::VcsBackend>> {
    // If we're inside a dwm workspace, the workspace dir itself contains
    // VCS metadata too, so plain detect() works. But we want to be tolerant
    // of being inside a workspace whose contents haven't been committed (the
    // workspace dir always has VCS state set up by jj/git when it was added).
    vcs::detect(cwd)
}

/// Deletes a workspace. Returns `true` if the cwd was inside the deleted
/// workspace and a redirect path was printed to stdout.
/// Delete a workspace by name (or infer from cwd).
pub fn delete_workspace(name: Option<String>, output: DeleteOutput) -> Result<bool> {
    let cwd = std::env::current_dir()?;
    let backend = detect_backend_from_cwd(&cwd)?;
    let deps = WorkspaceDeps { backend, cwd };
    if let Some(redirect) = delete_workspace_inner(&deps, name, output)? {
        println!("{}", redirect.display());
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Returns the path the shell should cd to if cwd was inside the deleted workspace.
fn delete_workspace_inner(
    deps: &WorkspaceDeps,
    name: Option<String>,
    output: DeleteOutput,
) -> Result<Option<PathBuf>> {
    let verbose = output == DeleteOutput::Verbose;

    let (root, ws_name) = match name {
        Some(n) => {
            let root = resolve_repo_root(deps)?;
            (root, n)
        }
        None => {
            let (root, ws, _) = find_dwm_workspace(&deps.cwd)
                .context("not inside a dwm workspace; provide a workspace name")?;
            (root, ws)
        }
    };

    let ws_path = dwm_dir(&root).join(&ws_name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", ws_name, ws_path.display());
    }

    if verbose {
        eprintln!(
            "{} workspace '{}'...",
            "forgetting".yellow(),
            ws_name.bold()
        );
    }
    deps.backend.workspace_remove(&root, &ws_name, &ws_path)?;

    if ws_path.exists() {
        if verbose {
            eprintln!("{} {}...", "removing".red(), ws_path.display().dimmed());
        }
        fs::remove_dir_all(&ws_path)?;
    }

    // Clean up agent status files for this workspace
    agent::remove_agent_statuses_for_workspace(&dwm_dir(&root), &ws_name);

    if verbose {
        eprintln!("{} workspace '{}' deleted", "✓".green(), ws_name.bold());
    }

    if is_inside(&deps.cwd, &ws_path) {
        Ok(Some(root))
    } else {
        Ok(None)
    }
}

/// Switch to the named workspace by printing its path to stdout for the shell
/// wrapper to `cd` into.
pub fn switch_workspace(name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = detect_backend_from_cwd(&cwd)?;
    let deps = WorkspaceDeps { backend, cwd };
    let path = switch_workspace_inner(&deps, name)?;
    println!("{}", path.display());
    Ok(())
}

/// Resolve the path for the named workspace. Returns the path the shell should
/// `cd` into.
fn switch_workspace_inner(deps: &WorkspaceDeps, name: &str) -> Result<PathBuf> {
    let root = resolve_repo_root(deps)?;
    let main_ws_name = deps.backend.main_workspace_name();
    if name == main_ws_name {
        return Ok(root);
    }

    let ws_path = dwm_dir(&root).join(name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", name, ws_path.display());
    }

    Ok(ws_path)
}

/// Rename a workspace. When `new_name` is `None` the first argument is treated
/// as the new name and the old name is inferred from the current directory.
pub fn rename_workspace(name: String, new_name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = detect_backend_from_cwd(&cwd)?;
    let deps = WorkspaceDeps { backend, cwd };

    let (old, new) = match new_name {
        Some(new) => (name, new),
        None => {
            // Infer old name from cwd
            let old = infer_workspace_name_from_cwd(&deps)?;
            (old, name)
        }
    };

    if let Some(redirect) = rename_workspace_inner(&deps, &old, &new)? {
        println!("{}", redirect.display());
    }
    Ok(())
}

/// Infer the current workspace name from the current directory path.
fn infer_workspace_name_from_cwd(deps: &WorkspaceDeps) -> Result<String> {
    match find_dwm_workspace(&deps.cwd) {
        Some((_, ws, _)) => Ok(ws),
        None => bail!("not inside a dwm workspace"),
    }
}

/// Returns the path the shell should cd to if cwd was inside the renamed workspace.
fn rename_workspace_inner(
    deps: &WorkspaceDeps,
    old_name: &str,
    new_name: &str,
) -> Result<Option<PathBuf>> {
    let root = resolve_repo_root(deps)?;
    let main_ws_name = deps.backend.main_workspace_name();
    if old_name == main_ws_name {
        bail!("cannot rename the main workspace '{}'", old_name);
    }

    let dwm = dwm_dir(&root);
    let old_path = dwm.join(old_name);
    if !old_path.exists() {
        bail!(
            "workspace '{}' not found at {}",
            old_name,
            old_path.display()
        );
    }

    if new_name.starts_with('.') {
        bail!("workspace name cannot start with '.'");
    }

    let new_path = dwm.join(new_name);
    if new_path.exists() {
        bail!(
            "workspace '{}' already exists at {}",
            new_name,
            new_path.display()
        );
    }

    eprintln!(
        "{} workspace '{}' -> '{}'...",
        "renaming".cyan(),
        old_name.bold(),
        new_name.bold()
    );
    deps.backend
        .workspace_rename(&root, &old_path, &new_path, old_name, new_name)?;

    eprintln!(
        "{} workspace '{}' renamed to '{}'",
        "✓".green(),
        old_name.bold(),
        new_name.bold()
    );

    if is_inside(&deps.cwd, &old_path) {
        let relative = deps.cwd.strip_prefix(&old_path)?;
        Ok(Some(new_path.join(relative)))
    } else {
        Ok(None)
    }
}

/// Return the per-repo `.dwm/` directory for the current working directory.
pub fn current_repo_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if let Some((root, _, _)) = find_dwm_workspace(&cwd) {
        return Ok(dwm_dir(&root));
    }
    let backend = vcs::detect(&cwd)?;
    let root = backend.root_from(&cwd)?;
    Ok(dwm_dir(&root))
}

/// Collect [`WorkspaceEntry`] values for all workspaces belonging to the
/// repository that contains the current directory.
pub fn list_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    let cwd = std::env::current_dir()?;
    let backend = detect_backend_from_cwd(&cwd)?;
    let deps = WorkspaceDeps { backend, cwd };
    list_workspace_entries_inner(&deps)
}

/// Testable core of [`list_workspace_entries`].
fn list_workspace_entries_inner(deps: &WorkspaceDeps) -> Result<Vec<WorkspaceEntry>> {
    let main_repo = resolve_repo_root(deps)?;
    let dwm = dwm_dir(&main_repo);

    let mut agent_summaries = agent::read_agent_summaries(&dwm);

    let main_ws_name = deps.backend.main_workspace_name();
    let vcs_workspaces = deps.backend.workspace_list(&main_repo).unwrap_or_default();

    let mut entries = Vec::new();

    // Find info for the main workspace
    let main_info = vcs_workspaces
        .iter()
        .find(|(n, _)| n == main_ws_name)
        .map(|(_, info)| info.clone())
        .unwrap_or_default();

    let main_stat = deps
        .backend
        .diff_stat_vs_trunk(&main_repo, &main_repo, main_ws_name)
        .unwrap_or_default();
    let main_modified = fs::metadata(&main_repo).and_then(|m| m.modified()).ok();
    let main_description = if main_info.description.trim().is_empty() {
        deps.backend
            .latest_description(&main_repo, &main_repo, main_ws_name)
    } else {
        main_info.description.clone()
    };
    let vcs_type = deps.backend.vcs_type();
    entries.push(WorkspaceEntry {
        name: main_ws_name.to_string(),
        path: main_repo.clone(),
        last_modified: main_modified,
        diff_stat: main_stat,
        is_main: true,
        change_id: main_info.change_id.clone(),
        description: main_description,
        bookmarks: main_info.bookmarks.clone(),
        is_stale: false,
        repo_name: None,
        main_repo_path: main_repo.clone(),
        vcs_type,
        agent_status: agent_summaries.remove(main_ws_name),
    });

    if !dwm.exists() {
        return Ok(entries);
    }

    // Scan workspace dirs
    let read_dir = fs::read_dir(&dwm)?;
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();

        // Skip internal dot-prefixed entries (.agent-status, etc.)
        if name.starts_with('.') {
            continue;
        }

        let ws_info = vcs_workspaces
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, info)| info.clone());

        let has_info = ws_info.is_some();
        let info = ws_info.unwrap_or_default();

        let stat = if has_info {
            deps.backend
                .diff_stat_vs_trunk(&main_repo, &path, &name)
                .unwrap_or_default()
        } else {
            vcs::DiffStat::default()
        };

        let description = if info.description.trim().is_empty() {
            deps.backend.latest_description(&main_repo, &path, &name)
        } else {
            info.description.clone()
        };

        let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();

        let merge_status =
            if has_info && deps.backend.is_merged_into_trunk(&main_repo, &path, &name) {
                MergeStatus::Merged
            } else {
                MergeStatus::Unmerged
            };

        let agent_status = agent_summaries.remove(&name);
        entries.push(WorkspaceEntry {
            is_stale: compute_is_stale(merge_status, modified),
            repo_name: None,
            name,
            path,
            last_modified: modified,
            diff_stat: stat,
            is_main: false,
            change_id: info.change_id,
            description,
            bookmarks: info.bookmarks,
            main_repo_path: main_repo.clone(),
            vcs_type,
            agent_status,
        });
    }

    Ok(entries)
}

/// Number of days of inactivity after which a workspace is considered stale.
const STALE_DAYS: u64 = 30;

/// All data needed to display a single row in the workspace picker or status output.
#[derive(Debug)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: PathBuf,
    pub last_modified: Option<std::time::SystemTime>,
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
}

/// Determine whether a non-main workspace should be shown as stale.
///
/// A workspace is stale if it has been merged into trunk, or if its last
/// modification time is more than [`STALE_DAYS`] days in the past.
fn compute_is_stale(merged: MergeStatus, last_modified: Option<SystemTime>) -> bool {
    if merged == MergeStatus::Merged {
        return true;
    }
    if let Some(time) = last_modified
        && let Ok(duration) = time.elapsed()
    {
        return duration.as_secs() > STALE_DAYS * 86400;
    }
    false
}

/// Collect [`WorkspaceEntry`] values for every workspace across all repos
/// tracked in the registry.
pub fn list_all_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    let repos = read_and_prune_registry()?;
    let mut all_entries = Vec::new();

    for repo_root in repos {
        let backend = match vcs::detect(&repo_root) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let repo_label = repo_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());

        let deps = WorkspaceDeps {
            backend,
            cwd: repo_root.clone(),
        };

        match list_workspace_entries_inner(&deps) {
            Ok(entries) => {
                for mut entry in entries {
                    entry.repo_name = Some(repo_label.clone());
                    all_entries.push(entry);
                }
            }
            Err(e) => {
                eprintln!("warning: skipping repo '{}': {}", repo_label, e);
            }
        }
    }

    Ok(all_entries)
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

/// Print a non-interactive tabular workspace summary to stderr.
pub fn print_status(entries: &[WorkspaceEntry]) {
    let out = std::io::stderr().lock();
    let _ = print_status_to(entries, out);
}

/// Core logic for printing the status table to any Write implementation.
fn print_status_to<W: Write>(entries: &[WorkspaceEntry], mut out: W) -> Result<()> {
    // Column widths
    let name_w = entries
        .iter()
        .map(|e| {
            let display = if e.is_main {
                format!("{} (main)", e.name)
            } else {
                e.name.clone()
            };
            display.len()
        })
        .max()
        .unwrap_or(4)
        .max(4);
    let change_w = 8;
    let bookmark_w = entries
        .iter()
        .map(|e| e.bookmarks.join(", ").len())
        .max()
        .unwrap_or(9)
        .max(9);
    let has_agents = entries
        .iter()
        .any(|e| e.agent_status.as_ref().is_some_and(|s| !s.is_empty()));
    let agent_w = if has_agents {
        entries
            .iter()
            .map(|e| {
                e.agent_status
                    .as_ref()
                    .map(|s| s.to_string().len())
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(6)
            .max(6)
    } else {
        0
    };

    // Header
    if has_agents {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "{:<name_w$}  {:<change_w$}  {:<40}  {:<bookmark_w$}  {:<9}  {:<agent_w$}  CHANGES",
                "NAME", "CHANGE", "DESCRIPTION", "BOOKMARKS", "MODIFIED", "AGENTS",
            )
            .bold()
            .dimmed()
        );
    } else {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "{:<name_w$}  {:<change_w$}  {:<40}  {:<bookmark_w$}  {:<9}  CHANGES",
                "NAME", "CHANGE", "DESCRIPTION", "BOOKMARKS", "MODIFIED",
            )
            .bold()
            .dimmed()
        );
    }

    for entry in entries {
        let name_text = if entry.is_main {
            format!("{} (main)", entry.name)
        } else if entry.is_stale {
            format!("{} [stale]", entry.name)
        } else {
            entry.name.clone()
        };

        let dim = entry.is_stale;
        let name_colored = {
            let s = format!("{:<name_w$}", name_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.cyan().to_string()
            }
        };

        let change_colored = {
            let s = format!("{:<change_w$}", entry.change_id);
            if dim {
                s.dimmed().to_string()
            } else {
                s.magenta().to_string()
            }
        };

        let desc = entry.description.lines().next().unwrap_or("");
        let desc_text: String = desc.chars().take(40).collect();
        let desc_colored = {
            let s = format!("{:<40}", desc_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.white().to_string()
            }
        };

        let bookmarks_text = entry.bookmarks.join(", ");
        let bookmarks_colored = {
            let s = format!("{:<bookmark_w$}", bookmarks_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.blue().to_string()
            }
        };

        let time_text = format_time_ago(entry.last_modified);
        let time_colored = {
            let s = format!("{:<9}", time_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.yellow().to_string()
            }
        };

        let stat = &entry.diff_stat;
        let changes_text = if stat.files_changed == 0 && stat.insertions == 0 && stat.deletions == 0
        {
            "clean".to_string()
        } else {
            let mut parts = Vec::new();
            if stat.insertions > 0 {
                parts.push(format!("+{}", stat.insertions));
            }
            if stat.deletions > 0 {
                parts.push(format!("-{}", stat.deletions));
            }
            if parts.is_empty() {
                format!("{} files", stat.files_changed)
            } else {
                parts.join(" ")
            }
        };

        let changes_colored = if dim {
            changes_text.dimmed().to_string()
        } else if stat.deletions > stat.insertions {
            changes_text.red().to_string()
        } else if stat.insertions > 0 {
            changes_text.green().to_string()
        } else {
            changes_text.dimmed().to_string()
        };

        if has_agents {
            let agent_colored = match &entry.agent_status {
                Some(summary) if !summary.is_empty() => {
                    let text = format!("{:<agent_w$}", summary);
                    if dim {
                        text.dimmed().to_string()
                    } else {
                        match summary.most_urgent() {
                            Some(crate::agent::AgentStatus::Waiting) => text.yellow().to_string(),
                            Some(crate::agent::AgentStatus::Working) => text.green().to_string(),
                            _ => text.dimmed().to_string(),
                        }
                    }
                }
                _ => format!("{:<agent_w$}", ""),
            };

            let _ = writeln!(
                out,
                "{}  {}  {}  {}  {}  {}  {}",
                name_colored,
                change_colored,
                desc_colored,
                bookmarks_colored,
                time_colored,
                agent_colored,
                changes_colored,
            );
        } else {
            let _ = writeln!(
                out,
                "{}  {}  {}  {}  {}  {}",
                name_colored,
                change_colored,
                desc_colored,
                bookmarks_colored,
                time_colored,
                changes_colored,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn print_status_to_string(entries: &[WorkspaceEntry]) -> String {
        owo_colors::set_override(true);
        let mut buf = Vec::new();
        print_status_to(entries, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn is_inside_detects_cwd_within_workspace() {
        let ws = Path::new("/home/user/repos/myrepo/.dwm/my-workspace");
        assert!(is_inside(ws, ws));
        assert!(is_inside(
            Path::new("/home/user/repos/myrepo/.dwm/my-workspace/src"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_sibling_workspace() {
        let ws = Path::new("/home/user/repos/myrepo/.dwm/my-workspace");
        assert!(!is_inside(
            Path::new("/home/user/repos/myrepo/.dwm/other-workspace"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_main_repo() {
        let ws = Path::new("/home/user/repos/myrepo/.dwm/my-workspace");
        assert!(!is_inside(Path::new("/home/user/code/myrepo"), ws));
    }

    // ── find_dwm_workspace tests ──────────────────────────────────────

    fn make_fake_repo(dir: &Path, vcs_marker: &str) -> PathBuf {
        let repo = dir.join("myrepo");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(repo.join(vcs_marker)).unwrap();
        repo
    }

    #[test]
    fn find_dwm_workspace_in_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let ws = repo.join(".dwm").join("feature");
        fs::create_dir_all(&ws).unwrap();

        let (root, name, path) = find_dwm_workspace(&ws).unwrap();
        assert_eq!(root, repo);
        assert_eq!(name, "feature");
        assert_eq!(path, ws);
    }

    #[test]
    fn find_dwm_workspace_in_workspace_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".git");
        let ws = repo.join(".dwm").join("feat-x");
        let sub = ws.join("src/inner");
        fs::create_dir_all(&sub).unwrap();

        let (root, name, path) = find_dwm_workspace(&sub).unwrap();
        assert_eq!(root, repo);
        assert_eq!(name, "feat-x");
        assert_eq!(path, ws);
    }

    #[test]
    fn find_dwm_workspace_returns_none_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        // cwd is the repo root itself, not in a workspace
        assert!(find_dwm_workspace(&repo).is_none());
    }

    #[test]
    fn find_dwm_workspace_returns_none_when_no_vcs_root_parent() {
        let tmp = tempfile::tempdir().unwrap();
        // Create .dwm/ws without a VCS root parent
        let fake = tmp.path().join("notarepo");
        let ws = fake.join(".dwm").join("feature");
        fs::create_dir_all(&ws).unwrap();
        assert!(find_dwm_workspace(&ws).is_none());
    }

    // ── registry tests ────────────────────────────────────────────────

    fn with_data_dir<F: FnOnce()>(dir: &Path, f: F) {
        // Override XDG_DATA_HOME / Application Support so `dirs::data_dir()`
        // returns `dir`.
        #[cfg(target_os = "macos")]
        let var = "HOME";
        #[cfg(not(target_os = "macos"))]
        let var = "XDG_DATA_HOME";

        // For macOS, dirs::data_dir() = $HOME/Library/Application Support.
        // For tests we set HOME so that resolves under our tmp dir.
        // For Linux we just point XDG_DATA_HOME at tmp directly.
        #[cfg(target_os = "macos")]
        let value = dir.to_path_buf();
        #[cfg(not(target_os = "macos"))]
        let value = dir.to_path_buf();
        // On macOS the data_dir is $HOME/Library/Application Support; ensure it exists.
        #[cfg(target_os = "macos")]
        std::fs::create_dir_all(value.join("Library/Application Support")).unwrap();

        temp_env::with_var(var, Some(value.as_os_str()), f);
    }

    #[test]
    fn registry_register_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        // Need a .dwm/ for read_and_prune_registry to keep it.
        fs::create_dir_all(repo.join(".dwm")).unwrap();

        with_data_dir(tmp.path(), || {
            register_repo(&repo).unwrap();
            register_repo(&repo).unwrap(); // dedupe
            let entries = read_and_prune_registry().unwrap();
            assert_eq!(entries, vec![repo.clone()]);
        });
    }

    #[test]
    fn registry_prunes_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let alive = make_fake_repo(tmp.path(), ".jj");
        fs::create_dir_all(alive.join(".dwm")).unwrap();
        let dead = tmp.path().join("nonexistent-repo");

        with_data_dir(tmp.path(), || {
            register_repo(&alive).unwrap();
            // Manually add a dead entry
            let path = registry_path().unwrap();
            let mut content = fs::read_to_string(&path).unwrap_or_default();
            content.push_str(&format!("{}\n", dead.display()));
            fs::write(&path, content).unwrap();

            let entries = read_and_prune_registry().unwrap();
            assert_eq!(entries, vec![alive.clone()]);

            // The file should now have been rewritten without the dead entry.
            let after = fs::read_to_string(&path).unwrap();
            assert!(!after.contains(dead.to_string_lossy().as_ref()));
        });
    }

    #[test]
    fn registry_prunes_repo_without_dwm_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj"); // no .dwm subdir

        with_data_dir(tmp.path(), || {
            register_repo(&repo).unwrap();
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.is_empty());
        });
    }

    #[test]
    fn registry_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        with_data_dir(tmp.path(), || {
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.is_empty());
        });
    }

    // ── ensure_dwm_ignored tests ──────────────────────────────────────

    #[test]
    fn ignore_writes_to_git_info_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();

        ensure_dwm_ignored(&repo).unwrap();
        let exclude = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains(".dwm/"));
    }

    #[test]
    fn ignore_writes_to_gitignore_for_jj_only_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();

        ensure_dwm_ignored(&repo).unwrap();
        let gi = fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert!(gi.contains(".dwm/"));
    }

    #[test]
    fn ignore_does_not_duplicate_existing_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        fs::write(repo.join(".gitignore"), "target/\n.dwm/\n").unwrap();

        ensure_dwm_ignored(&repo).unwrap();
        let gi = fs::read_to_string(repo.join(".gitignore")).unwrap();
        // Still only one occurrence of .dwm/
        assert_eq!(gi.matches(".dwm/").count(), 1);
    }

    #[test]
    fn ignore_appends_newline_when_existing_file_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        fs::write(repo.join(".gitignore"), "target/").unwrap(); // no trailing newline

        ensure_dwm_ignored(&repo).unwrap();
        let gi = fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert!(gi.contains("target/\n"));
        assert!(gi.contains(".dwm/"));
    }

    #[test]
    fn ignore_recognizes_alternate_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        fs::write(repo.join(".gitignore"), "/.dwm\n").unwrap();

        ensure_dwm_ignored(&repo).unwrap();
        let gi = fs::read_to_string(repo.join(".gitignore")).unwrap();
        // Should not append because /.dwm is recognized as the same rule.
        assert!(!gi.contains(".dwm/"));
    }

    // ── MockBackend ──────────────────────────────────────────────────

    #[derive(Debug, Clone)]
    enum MockCall {
        WorkspaceAdd {
            repo_dir: PathBuf,
            ws_path: PathBuf,
            name: String,
            at: Option<String>,
        },
        WorkspaceRemove {
            repo_dir: PathBuf,
            name: String,
            ws_path: PathBuf,
        },
        WorkspaceRename {
            old_name: String,
            new_name: String,
        },
    }

    struct MockBackend {
        /// The root path returned by root_from.
        root: PathBuf,
        /// Workspaces returned by workspace_list.
        workspaces: Vec<(String, vcs::WorkspaceInfo)>,
        /// Records every mutating call for assertions.
        calls: Arc<Mutex<Vec<MockCall>>>,
    }

    impl MockBackend {
        fn new(
            root: PathBuf,
            workspaces: Vec<(String, vcs::WorkspaceInfo)>,
        ) -> (Self, Arc<Mutex<Vec<MockCall>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    root,
                    workspaces,
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }
    }

    impl vcs::VcsBackend for MockBackend {
        fn root_from(&self, _dir: &Path) -> Result<PathBuf> {
            Ok(self.root.clone())
        }

        fn workspace_list(&self, _repo_dir: &Path) -> Result<Vec<(String, vcs::WorkspaceInfo)>> {
            Ok(self.workspaces.clone())
        }

        fn workspace_add(
            &self,
            repo_dir: &Path,
            ws_path: &Path,
            name: &str,
            at: Option<&str>,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::WorkspaceAdd {
                repo_dir: repo_dir.to_path_buf(),
                ws_path: ws_path.to_path_buf(),
                name: name.to_string(),
                at: at.map(|s| s.to_string()),
            });
            // Create the directory so the workspace "exists" after add
            fs::create_dir_all(ws_path)?;
            Ok(())
        }

        fn workspace_remove(&self, repo_dir: &Path, name: &str, ws_path: &Path) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::WorkspaceRemove {
                repo_dir: repo_dir.to_path_buf(),
                name: name.to_string(),
                ws_path: ws_path.to_path_buf(),
            });
            Ok(())
        }

        fn workspace_rename(
            &self,
            _repo_dir: &Path,
            old_path: &Path,
            new_path: &Path,
            old_name: &str,
            new_name: &str,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::WorkspaceRename {
                old_name: old_name.to_string(),
                new_name: new_name.to_string(),
            });
            fs::rename(old_path, new_path)?;
            Ok(())
        }

        fn diff_stat_vs_trunk(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            _ws_name: &str,
        ) -> Result<vcs::DiffStat> {
            Ok(vcs::DiffStat {
                files_changed: 1,
                insertions: 10,
                deletions: 2,
            })
        }

        fn latest_description(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            _ws_name: &str,
        ) -> String {
            "mock description".to_string()
        }

        fn is_merged_into_trunk(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            _ws_name: &str,
        ) -> bool {
            false
        }

        fn vcs_type(&self) -> vcs::VcsType {
            vcs::VcsType::Jj
        }

        fn main_workspace_name(&self) -> &'static str {
            "default"
        }
    }

    // Helper: create a fake repo with `.jj/` as VCS marker so that
    // find_dwm_workspace recognizes the parent.
    fn make_repo_with_vcs(tmp: &Path) -> PathBuf {
        let repo = tmp.join("myrepo");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(repo.join(".jj")).unwrap();
        repo
    }

    // ── list_workspace_entries_inner tests ────────────────────────────

    #[test]
    fn list_entries_from_inside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let dwm = main_repo.join(".dwm");
        let ws_dir = dwm.join("feat-x");
        fs::create_dir_all(&ws_dir).unwrap();

        let workspaces = vec![
            (
                "default".to_string(),
                vcs::WorkspaceInfo {
                    change_id: "aaa".to_string(),
                    description: "main desc".to_string(),
                    bookmarks: vec!["main".to_string()],
                },
            ),
            (
                "feat-x".to_string(),
                vcs::WorkspaceInfo {
                    change_id: "bbb".to_string(),
                    description: "feature".to_string(),
                    bookmarks: vec![],
                },
            ),
        ];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert!(entries.len() >= 2);

        let main_entry = entries.iter().find(|e| e.is_main).unwrap();
        assert_eq!(main_entry.name, "default");
        assert_eq!(main_entry.change_id, "aaa");
        assert_eq!(main_entry.description, "main desc");
        assert_eq!(main_entry.path, main_repo);

        let feat_entry = entries.iter().find(|e| e.name == "feat-x").unwrap();
        assert_eq!(feat_entry.change_id, "bbb");
        assert_eq!(feat_entry.description, "feature");
        assert!(!feat_entry.is_main);
    }

    #[test]
    fn list_entries_skips_dot_prefixed_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let dwm = main_repo.join(".dwm");
        let ws_dir = dwm.join("feat-x");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(dwm.join(".agent-status")).unwrap();

        let workspaces = vec![
            (
                "default".to_string(),
                vcs::WorkspaceInfo {
                    change_id: "aaa".to_string(),
                    description: "".to_string(),
                    bookmarks: vec![],
                },
            ),
            (
                "feat-x".to_string(),
                vcs::WorkspaceInfo {
                    change_id: "bbb".to_string(),
                    description: "".to_string(),
                    bookmarks: vec![],
                },
            ),
        ];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            !names.contains(&".agent-status"),
            "dot-prefixed dirs should be excluded, got: {:?}",
            names
        );
        assert!(names.contains(&"feat-x"));
    }

    #[test]
    fn list_entries_from_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let workspaces = vec![(
            "default".to_string(),
            vcs::WorkspaceInfo {
                change_id: "abc".to_string(),
                description: "".to_string(),
                bookmarks: vec![],
            },
        )];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_main);
        // Empty description should fall through to latest_description
        assert_eq!(entries[0].description, "mock description");
    }

    #[test]
    fn list_entries_empty_repo_no_dwm_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        // No .dwm/ — should still return the main entry only.

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1, "should have just the main entry");
        assert!(entries[0].is_main);
    }

    // ── new_workspace_inner tests ────────────────────────────────────

    #[test]
    fn new_workspace_calls_add() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, Some("my-ws".to_string()), None, None).unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceAdd {
                repo_dir,
                ws_path,
                name,
                at,
            } => {
                assert_eq!(repo_dir, &main_repo);
                assert_eq!(ws_path, &main_repo.join(".dwm/my-ws"));
                assert_eq!(name, "my-ws");
                assert!(at.is_none());
            }
            other => panic!("expected WorkspaceAdd, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_auto_names() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, None, None, None).unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceAdd { name, .. } => {
                // Auto-generated name should be non-empty and contain a hyphen (adjective-noun)
                assert!(!name.is_empty());
                assert!(
                    name.contains('-'),
                    "auto name should be adjective-noun: {}",
                    name
                );
            }
            other => panic!("expected WorkspaceAdd, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_duplicate_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, Some("dup-ws".to_string()), None, None).unwrap();
            let err =
                new_workspace_inner(&deps, Some("dup-ws".to_string()), None, None).unwrap_err();
            assert!(err.to_string().contains("already exists"), "error: {}", err);
        });
    }

    #[test]
    fn new_workspace_dot_prefix_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        with_data_dir(tmp.path(), || {
            let err = new_workspace_inner(&deps, Some(".agent-status".to_string()), None, None)
                .unwrap_err();
            assert!(
                err.to_string().contains("cannot start with '.'"),
                "error: {}",
                err
            );
        });
    }

    #[test]
    fn new_workspace_from_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let workspaces = vec![(
            "source-ws".to_string(),
            vcs::WorkspaceInfo {
                change_id: "abc12345".to_string(),
                description: "some work".to_string(),
                bookmarks: vec![],
            },
        )];

        let (mock, calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, Some("forked".to_string()), None, Some("source-ws"))
                .unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceAdd {
                ws_path, name, at, ..
            } => {
                assert_eq!(ws_path, &main_repo.join(".dwm/forked"));
                assert_eq!(name, "forked");
                assert_eq!(at.as_deref(), Some("abc12345"));
            }
            other => panic!("expected WorkspaceAdd, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_from_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        with_data_dir(tmp.path(), || {
            let err =
                new_workspace_inner(&deps, Some("forked".to_string()), None, Some("no-such-ws"))
                    .unwrap_err();
            assert!(
                err.to_string().contains("not found"),
                "error should mention not found: {}",
                err
            );
        });
    }

    #[test]
    fn new_workspace_creates_ignore_entry_first_time() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        // jj-only repo → .gitignore

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, Some("ws".to_string()), None, None).unwrap();
        });
        let gi = fs::read_to_string(main_repo.join(".gitignore")).unwrap();
        assert!(gi.contains(".dwm/"));
    }

    #[test]
    fn new_workspace_registers_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, Some("ws".to_string()), None, None).unwrap();
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.contains(&main_repo));
        });
    }

    // ── delete_workspace_inner tests ─────────────────────────────────

    #[test]
    fn delete_workspace_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = main_repo.join(".dwm/my-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        // cwd is outside the workspace being deleted
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        let redirect =
            delete_workspace_inner(&deps, Some("my-ws".to_string()), DeleteOutput::Verbose)
                .unwrap();
        assert!(
            redirect.is_none(),
            "should not redirect when cwd is outside workspace"
        );

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceRemove {
                repo_dir,
                name,
                ws_path,
            } => {
                assert_eq!(repo_dir, &main_repo);
                assert_eq!(name, "my-ws");
                assert_eq!(ws_path, &ws_dir);
            }
            other => panic!("expected WorkspaceRemove, got {:?}", other),
        }
        assert!(!ws_dir.exists());
    }

    #[test]
    fn delete_workspace_redirects_when_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = main_repo.join(".dwm/my-ws");
        fs::create_dir_all(ws_dir.join("src")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
        };

        let redirect =
            delete_workspace_inner(&deps, Some("my-ws".to_string()), DeleteOutput::Verbose)
                .unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, main_repo);
    }

    #[test]
    fn delete_workspace_infers_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = main_repo.join(".dwm/inferred-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
        };

        let _redirected = delete_workspace_inner(&deps, None, DeleteOutput::Verbose).unwrap();

        let calls = calls.lock().unwrap();
        match &calls[0] {
            MockCall::WorkspaceRemove { name, .. } => {
                assert_eq!(name, "inferred-ws");
            }
            other => panic!("expected WorkspaceRemove, got {:?}", other),
        }
    }

    #[test]
    fn delete_workspace_not_found_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let err = delete_workspace_inner(
            &deps,
            Some("nonexistent".to_string()),
            DeleteOutput::Verbose,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    // ── rename_workspace_inner tests ──────────────────────────────

    #[test]
    fn rename_workspace_success() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = main_repo.join(".dwm/old-name");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        let redirect = rename_workspace_inner(&deps, "old-name", "new-name").unwrap();
        assert!(
            redirect.is_none(),
            "should not redirect when cwd is outside workspace"
        );

        assert!(!ws_dir.exists());
        assert!(main_repo.join(".dwm/new-name").exists());

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceRename {
                old_name, new_name, ..
            } => {
                assert_eq!(old_name, "old-name");
                assert_eq!(new_name, "new-name");
            }
            other => panic!("expected WorkspaceRename, got {:?}", other),
        }
    }

    #[test]
    fn rename_workspace_redirects_when_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = main_repo.join(".dwm/old-name");
        fs::create_dir_all(ws_dir.join("src")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
        };

        let redirect = rename_workspace_inner(&deps, "old-name", "new-name").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, main_repo.join(".dwm/new-name/src"));
    }

    #[test]
    fn rename_workspace_preserves_files() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = main_repo.join(".dwm/old-name");
        fs::create_dir_all(ws_dir.join("src")).unwrap();
        fs::write(ws_dir.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(ws_dir.join("README.md"), "# hello").unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        rename_workspace_inner(&deps, "old-name", "new-name").unwrap();

        let new_dir = main_repo.join(".dwm/new-name");
        assert!(new_dir.join("src/main.rs").exists());
        assert_eq!(
            fs::read_to_string(new_dir.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
        assert_eq!(
            fs::read_to_string(new_dir.join("README.md")).unwrap(),
            "# hello"
        );
    }

    #[test]
    fn rename_workspace_old_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let err = rename_workspace_inner(&deps, "nonexistent", "new-name").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_new_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(main_repo.join(".dwm/old-name")).unwrap();
        fs::create_dir_all(main_repo.join(".dwm/new-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let err = rename_workspace_inner(&deps, "old-name", "new-name").unwrap_err();
        assert!(err.to_string().contains("already exists"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_refuses_main() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let err = rename_workspace_inner(&deps, "default", "new-name").unwrap_err();
        assert!(err.to_string().contains("cannot rename"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_dot_prefix_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(main_repo.join(".dwm/old-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let err = rename_workspace_inner(&deps, "old-name", ".hidden").unwrap_err();
        assert!(
            err.to_string().contains("cannot start with '.'"),
            "error: {}",
            err
        );
    }

    // ── switch_workspace_inner tests ──────────────────────────────

    #[test]
    fn switch_workspace_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = main_repo.join(".dwm/feat-x");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let path = switch_workspace_inner(&deps, "feat-x").unwrap();
        assert_eq!(path, ws_dir);
    }

    #[test]
    fn switch_workspace_to_main() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
        };

        let path = switch_workspace_inner(&deps, "default").unwrap();
        assert_eq!(path, main_repo);
    }

    #[test]
    fn switch_workspace_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let err = switch_workspace_inner(&deps, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    // ── rename with cwd inference tests ─────────────────────────────

    #[test]
    fn rename_infers_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = main_repo.join(".dwm/old-name");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
        };

        let old = infer_workspace_name_from_cwd(&deps).unwrap();
        assert_eq!(old, "old-name");

        let redirect = rename_workspace_inner(&deps, &old, "new-name").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, main_repo.join(".dwm/new-name"));

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceRename { old_name, new_name } => {
                assert_eq!(old_name, "old-name");
                assert_eq!(new_name, "new-name");
            }
            other => panic!("expected WorkspaceRename, got {:?}", other),
        }
    }

    #[test]
    fn rename_refuses_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
        };

        let err = infer_workspace_name_from_cwd(&deps).unwrap_err();
        assert!(
            err.to_string().contains("not inside a dwm workspace"),
            "error: {}",
            err
        );
    }

    // ── compute_is_stale tests ────────────────────────────────────

    #[test]
    fn stale_merged_workspace_is_stale() {
        assert!(compute_is_stale(
            MergeStatus::Merged,
            Some(SystemTime::now())
        ));
    }

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

    // ── format_time_ago tests ───────────────────────────────────────

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

    // ── print_status tests ──────────────────────────────────────────

    #[test]
    fn print_status_does_not_panic() {
        let entries = vec![
            WorkspaceEntry {
                name: "default".to_string(),
                path: PathBuf::from("/tmp/repo"),
                last_modified: Some(SystemTime::now()),
                diff_stat: vcs::DiffStat {
                    files_changed: 1,
                    insertions: 10,
                    deletions: 2,
                },
                is_main: true,
                change_id: "abc12345".to_string(),
                description: "main workspace".to_string(),
                bookmarks: vec!["main".to_string()],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
            },
            WorkspaceEntry {
                name: "feat-x".to_string(),
                path: PathBuf::from("/tmp/feat-x"),
                last_modified: None,
                diff_stat: vcs::DiffStat::default(),
                is_main: false,
                change_id: "def67890".to_string(),
                description: "feature work".to_string(),
                bookmarks: vec![],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
            },
        ];
        // Should not panic; output goes to stderr
        print_status(&entries);
    }

    #[test]
    fn status_table_snapshot() {
        // Use fixed times relative to "now" for format_time_ago
        let now = SystemTime::now();
        let t_5m = now - std::time::Duration::from_secs(300);
        let t_2h = now - std::time::Duration::from_secs(7200);

        let entries = vec![
            WorkspaceEntry {
                name: "default".to_string(),
                path: PathBuf::from("/tmp/repo"),
                last_modified: Some(t_5m),
                diff_stat: vcs::DiffStat {
                    files_changed: 1,
                    insertions: 10,
                    deletions: 2,
                },
                is_main: true,
                change_id: "abc12345".to_string(),
                description: "refactor help system".to_string(),
                bookmarks: vec!["main".to_string()],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
            },
            WorkspaceEntry {
                name: "hazy-quail".to_string(),
                path: PathBuf::from("/tmp/hazy-quail"),
                last_modified: Some(t_2h),
                diff_stat: vcs::DiffStat {
                    files_changed: 5,
                    insertions: 100,
                    deletions: 50,
                },
                is_main: false,
                change_id: "tqqorvwl".to_string(),
                description: "Live-updating list view".to_string(),
                bookmarks: vec![],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: Some(crate::agent::AgentSummary {
                    waiting: 1,
                    working: 0,
                    idle: 0,
                }),
            },
        ];

        let out = print_status_to_string(&entries);

        assert!(out.contains("NAME"));
        assert!(out.contains("default (main)"));
        assert!(out.contains("abc12345"));
        assert!(out.contains("refactor help system"));
        assert!(out.contains("main"));
        assert!(out.contains("5m ago"));
        assert!(out.contains("+10 -2"));

        assert!(out.contains("hazy-quail"));
        assert!(out.contains("tqqorvwl"));
        assert!(out.contains("Live-updating list view"));
        assert!(out.contains("2h ago"));
        assert!(out.contains("1 waiting"));
        assert!(out.contains("+100 -50"));

        // Verify ANSI codes are present (cyan for names)
        assert!(out.contains("\x1b[36m"));
    }

    // ── E2E tests with real git repos ───────────────────────────────

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Initialize a git repo with an initial commit.
    /// Returns the canonicalized repo path.
    fn init_git_repo(dir: &Path) -> PathBuf {
        let dir_str = dir.to_str().unwrap();
        std::process::Command::new("git")
            .args(["init", "-b", "main", dir_str])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                dir_str,
                "commit",
                "--allow-empty",
                "-m",
                "initial commit",
            ])
            .output()
            .unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn e2e_git_list_entries_main_only() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        let backend = crate::git::GitBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1, "should have main worktree entry");
        assert!(entries[0].is_main);
        assert_eq!(entries[0].name, "main-worktree");
        assert_eq!(entries[0].path, main_repo);
        assert_eq!(entries[0].description, "initial commit");
    }

    #[test]
    fn e2e_git_list_entries_with_worktree() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        // Create a git worktree at <repo>/.dwm/feat-branch
        let ws_path = main_repo.join(".dwm/feat-branch");
        fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                main_repo.to_str().unwrap(),
                "worktree",
                "add",
                ws_path.to_str().unwrap(),
                "-b",
                "feat-branch",
            ])
            .output()
            .unwrap();

        let backend = crate::git::GitBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert!(
            entries.len() >= 2,
            "should have main + worktree, got {}",
            entries.len()
        );

        let main_entry = entries.iter().find(|e| e.is_main).unwrap();
        assert_eq!(main_entry.name, "main-worktree");
        assert_eq!(main_entry.path, main_repo);

        let feat_entry = entries.iter().find(|e| e.name == "feat-branch").unwrap();
        assert!(!feat_entry.is_main);
        assert!(feat_entry.path.ends_with("feat-branch"));
        assert!(feat_entry.bookmarks.contains(&"feat-branch".to_string()));
    }

    #[test]
    fn e2e_git_new_and_delete_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let backend = crate::git::GitBackend;
            let deps = WorkspaceDeps {
                backend: Box::new(backend),
                cwd: main_repo.clone(),
            };

            new_workspace_inner(&deps, Some("test-ws".to_string()), None, None).unwrap();
            let ws_dir = main_repo.join(".dwm/test-ws");
            assert!(ws_dir.exists(), "workspace dir should exist after creation");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo.clone(),
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "test-ws"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo.clone(),
            };
            delete_workspace_inner(&deps3, Some("test-ws".to_string()), DeleteOutput::Verbose)
                .unwrap();
            assert!(!ws_dir.exists());

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo,
            };
            let entries = list_workspace_entries_inner(&deps4).unwrap();
            assert!(!entries.iter().any(|e| e.name == "test-ws"));
        });
    }

    #[test]
    fn e2e_git_worktree_with_changes() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let backend = crate::git::GitBackend;
            let deps = WorkspaceDeps {
                backend: Box::new(backend),
                cwd: main_repo.clone(),
            };

            new_workspace_inner(&deps, Some("feature".to_string()), None, None).unwrap();
            let ws_dir = main_repo.join(".dwm/feature");

            fs::write(ws_dir.join("hello.txt"), "hello world\n").unwrap();
            let ws_str = ws_dir.to_str().unwrap();
            std::process::Command::new("git")
                .args(["-C", ws_str, "add", "hello.txt"])
                .output()
                .unwrap();
            std::process::Command::new("git")
                .args(["-C", ws_str, "commit", "-m", "add hello"])
                .output()
                .unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            let feat = entries.iter().find(|e| e.name == "feature").unwrap();
            assert_eq!(feat.description, "add hello");
            assert!(
                feat.diff_stat.insertions > 0 || feat.diff_stat.files_changed > 0,
                "feature workspace should show changes vs trunk"
            );
        });
    }

    #[test]
    fn e2e_git_rename_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let backend = crate::git::GitBackend;
            let deps = WorkspaceDeps {
                backend: Box::new(backend),
                cwd: main_repo.clone(),
            };

            new_workspace_inner(&deps, Some("old-name".to_string()), None, None).unwrap();
            let old_path = main_repo.join(".dwm/old-name");
            assert!(old_path.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo.clone(),
            };
            rename_workspace_inner(&deps2, "old-name", "new-name").unwrap();

            assert!(!old_path.exists(), "old dir should be gone");
            assert!(main_repo.join(".dwm/new-name").exists());

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo,
            };
            let entries = list_workspace_entries_inner(&deps3).unwrap();
            assert!(entries.iter().any(|e| e.name == "new-name"));
            assert!(!entries.iter().any(|e| e.name == "old-name"));
        });
    }

    #[test]
    fn e2e_git_rename_redirects_when_inside() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo.clone(),
            };

            new_workspace_inner(&deps, Some("my-ws".to_string()), None, None).unwrap();
            let ws_path = main_repo.join(".dwm/my-ws");
            let subdir = ws_path.join("src");
            fs::create_dir_all(&subdir).unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: subdir,
            };
            let redirect = rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();
            let redirect = redirect.expect("should redirect when cwd is inside renamed workspace");
            assert_eq!(redirect, main_repo.join(".dwm/renamed-ws/src"));

            let new_ws = main_repo.join(".dwm/renamed-ws");
            assert!(new_ws.exists());
            assert!(new_ws.join("src").exists());
        });
    }

    fn jj_available() -> bool {
        std::process::Command::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn init_jj_repo(dir: &Path) -> PathBuf {
        let dir_str = dir.to_str().unwrap();
        std::process::Command::new("jj")
            .args(["git", "init", dir_str])
            .output()
            .unwrap();
        // Create a "main" bookmark so trunk() resolves
        std::process::Command::new("jj")
            .args([
                "--repository",
                dir_str,
                "bookmark",
                "create",
                "main",
                "-r",
                "@-",
            ])
            .output()
            .unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn e2e_jj_list_entries_main_only() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1, "should have default workspace entry");
        assert!(entries[0].is_main);
        assert_eq!(entries[0].name, "default");
        assert_eq!(entries[0].path, main_repo);
    }

    #[test]
    fn e2e_jj_list_entries_with_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        let ws_path = main_repo.join(".dwm/feat-ws");
        fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::process::Command::new("jj")
            .args([
                "--repository",
                main_repo.to_str().unwrap(),
                "workspace",
                "add",
                "--name",
                "feat-ws",
                ws_path.to_str().unwrap(),
            ])
            .output()
            .unwrap();

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert!(
            entries.len() >= 2,
            "should have default + workspace, got {}",
            entries.len()
        );

        let main_entry = entries.iter().find(|e| e.is_main).unwrap();
        assert_eq!(main_entry.name, "default");
        assert_eq!(main_entry.path, main_repo);

        let feat_entry = entries.iter().find(|e| e.name == "feat-ws").unwrap();
        assert!(!feat_entry.is_main);
        assert!(feat_entry.path.ends_with("feat-ws"));
    }

    #[test]
    fn e2e_jj_new_and_delete_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            new_workspace_inner(&deps, Some("test-ws".to_string()), None, None).unwrap();
            let ws_dir = main_repo.join(".dwm/test-ws");
            assert!(ws_dir.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "test-ws"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            delete_workspace_inner(&deps3, Some("test-ws".to_string()), DeleteOutput::Verbose)
                .unwrap();
            assert!(!ws_dir.exists());

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
            };
            let entries = list_workspace_entries_inner(&deps4).unwrap();
            assert!(!entries.iter().any(|e| e.name == "test-ws"));
        });
    }

    #[test]
    fn e2e_jj_workspace_with_spaces_in_name() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            new_workspace_inner(&deps, Some("my cool feature".to_string()), None, None).unwrap();
            let ws_dir = main_repo.join(".dwm/my cool feature");
            assert!(ws_dir.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "my cool feature"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            let switch_path = switch_workspace_inner(&deps3, "my cool feature").unwrap();
            assert_eq!(switch_path, ws_dir);

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            delete_workspace_inner(
                &deps4,
                Some("my cool feature".to_string()),
                DeleteOutput::Verbose,
            )
            .unwrap();
            assert!(!ws_dir.exists());
        });
    }

    #[test]
    fn e2e_jj_workspace_with_changes() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            new_workspace_inner(&deps, Some("feature".to_string()), None, None).unwrap();
            let ws_dir = main_repo.join(".dwm/feature");

            fs::write(ws_dir.join("hello.txt"), "hello world\n").unwrap();
            let ws_str = ws_dir.to_str().unwrap();
            std::process::Command::new("jj")
                .args(["--repository", ws_str, "describe", "-m", "add hello"])
                .output()
                .unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            let feat = entries.iter().find(|e| e.name == "feature").unwrap();
            assert_eq!(feat.description.trim(), "add hello");
            assert!(
                feat.diff_stat.insertions > 0 || feat.diff_stat.files_changed > 0,
                "feature workspace should show changes vs trunk: {:?}",
                feat.diff_stat
            );
        });
    }

    #[test]
    fn e2e_jj_rename_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            new_workspace_inner(&deps, Some("old-name".to_string()), None, None).unwrap();
            let old_path = main_repo.join(".dwm/old-name");
            assert!(old_path.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            rename_workspace_inner(&deps2, "old-name", "new-name").unwrap();

            assert!(!old_path.exists());
            assert!(main_repo.join(".dwm/new-name").exists());

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
            };
            let entries = list_workspace_entries_inner(&deps3).unwrap();
            assert!(entries.iter().any(|e| e.name == "new-name"));
            assert!(!entries.iter().any(|e| e.name == "old-name"));
        });
    }

    #[test]
    fn e2e_jj_rename_stale_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            new_workspace_inner(&deps, Some("my-ws".to_string()), None, None).unwrap();

            let main_str = main_repo.to_str().unwrap();
            fs::write(main_repo.join("file.txt"), "content\n").unwrap();
            std::process::Command::new("jj")
                .args(["--repository", main_str, "describe", "-m", "advance op log"])
                .output()
                .unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();

            assert!(!main_repo.join(".dwm/my-ws").exists());
            assert!(main_repo.join(".dwm/renamed-ws").exists());

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
            };
            let entries = list_workspace_entries_inner(&deps3).unwrap();
            assert!(entries.iter().any(|e| e.name == "renamed-ws"));
            assert!(!entries.iter().any(|e| e.name == "my-ws"));
        });
    }

    #[test]
    fn e2e_git_switch_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo.clone(),
            };
            new_workspace_inner(&deps, Some("switch-target".to_string()), None, None).unwrap();
            let ws_dir = main_repo.join(".dwm/switch-target");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo.clone(),
            };
            let path = switch_workspace_inner(&deps2, "switch-target").unwrap();
            assert_eq!(path, ws_dir);

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend),
                cwd: main_repo.clone(),
            };
            let path = switch_workspace_inner(&deps3, "main-worktree").unwrap();
            assert_eq!(path, main_repo);
        });
    }

    #[test]
    fn e2e_jj_switch_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            new_workspace_inner(&deps, Some("switch-target".to_string()), None, None).unwrap();
            let ws_dir = main_repo.join(".dwm/switch-target");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            let path = switch_workspace_inner(&deps2, "switch-target").unwrap();
            assert_eq!(path, ws_dir);

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            let path = switch_workspace_inner(&deps3, "default").unwrap();
            assert_eq!(path, main_repo);
        });
    }

    #[test]
    fn e2e_jj_rename_redirects_when_inside() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
            };
            new_workspace_inner(&deps, Some("my-ws".to_string()), None, None).unwrap();
            let ws_path = main_repo.join(".dwm/my-ws");
            let subdir = ws_path.join("src");
            fs::create_dir_all(&subdir).unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: subdir,
            };
            let redirect = rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();
            let redirect = redirect.expect("should redirect when cwd is inside renamed workspace");
            assert_eq!(redirect, main_repo.join(".dwm/renamed-ws/src"));

            let new_ws = main_repo.join(".dwm/renamed-ws");
            assert!(new_ws.exists());
            assert!(new_ws.join("src").exists());
        });
    }
}
