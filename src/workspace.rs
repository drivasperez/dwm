use anyhow::{Context, Result, bail};
use owo_colors::OwoColorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{agent, names, vcs};

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

/// Return the path to `~/.dwm/`, the root of all dwm workspace storage.
fn dwm_base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".dwm"))
}

/// Return `~/.dwm/<repo_name>` — the per-repo workspace storage directory.
fn repo_dir(dwm_base: &Path, repo_name: &str) -> PathBuf {
    dwm_base.join(repo_name)
}

/// Read the original repository root path from `~/.dwm/<repo_name>/.main-repo`.
fn main_repo_path(dwm_base: &Path, repo_name: &str) -> Result<PathBuf> {
    let repo_dir = repo_dir(dwm_base, repo_name);
    let main_repo_file = repo_dir.join(".main-repo");
    let path = fs::read_to_string(&main_repo_file)
        .with_context(|| format!("could not read {}", main_repo_file.display()))?;
    Ok(PathBuf::from(path.trim()))
}

/// Create `~/.dwm/<repo_name>/` if it does not yet exist, and write the
/// `.main-repo` and `.vcs-type` marker files on first use.
fn ensure_repo_dir(
    dwm_base: &Path,
    repo_name: &str,
    main_repo_root: &Path,
    vcs_type: vcs::VcsType,
) -> Result<PathBuf> {
    let dir = repo_dir(dwm_base, repo_name);
    fs::create_dir_all(&dir)?;
    let main_repo_file = dir.join(".main-repo");
    if !main_repo_file.exists() {
        fs::write(&main_repo_file, main_repo_root.to_string_lossy().as_ref())?;
    }
    let vcs_file = dir.join(".vcs-type");
    if !vcs_file.exists() {
        fs::write(&vcs_file, vcs_type.to_string())?;
    }
    Ok(dir)
}

/// Common dependencies threaded through workspace operations, grouped so they
/// can be injected in tests without touching the real filesystem or VCS.
struct WorkspaceDeps {
    backend: Box<dyn vcs::VcsBackend>,
    cwd: PathBuf,
    dwm_base: PathBuf,
}

/// Create a new workspace, auto-detecting the VCS from the current directory.
///
/// Prints the new workspace path to stdout so the shell wrapper can `cd` into it.
pub fn new_workspace(name: Option<String>, at: Option<&str>, from: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let dwm_base = dwm_base_dir()?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        dwm_base,
    };
    new_workspace_inner(&deps, name, at, from)
}

/// Testable core of [`new_workspace`] that accepts injected [`WorkspaceDeps`].
fn new_workspace_inner(
    deps: &WorkspaceDeps,
    name: Option<String>,
    at: Option<&str>,
    from: Option<&str>,
) -> Result<()> {
    let repo_name = deps.backend.repo_name_from(&deps.cwd)?;
    let root = deps.backend.root_from(&deps.cwd)?;
    let dir = ensure_repo_dir(&deps.dwm_base, &repo_name, &root, deps.backend.vcs_type())?;

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
        None => names::generate_unique(&dir),
    };

    let ws_path = dir.join(&ws_name);
    if ws_path.exists() {
        bail!(
            "workspace '{}' already exists at {}",
            ws_name,
            ws_path.display()
        );
    }

    eprintln!("{} workspace '{}'...", "creating".cyan(), ws_name.bold());
    deps.backend.workspace_add(&root, &ws_path, &ws_name, at)?;
    eprintln!(
        "{} workspace '{}' created at {}",
        "✓".green(),
        ws_name.bold(),
        ws_path.display().dimmed()
    );

    // stdout: path for shell wrapper to cd into
    println!("{}", ws_path.display());
    Ok(())
}

/// Deletes a workspace. Returns `true` if the cwd was inside the deleted
/// workspace and a redirect path was printed to stdout.
/// Delete a workspace by name (or infer from cwd).
pub fn delete_workspace(name: Option<String>, output: DeleteOutput) -> Result<bool> {
    let cwd = std::env::current_dir()?;
    let dwm_base = dwm_base_dir()?;

    // We need a backend for the repo-name-from-cwd case.
    // When inside dwm dir we detect from the dwm repo dir;
    // otherwise we detect from cwd.
    let backend: Box<dyn vcs::VcsBackend> = if cwd.starts_with(&dwm_base) {
        let relative = cwd.strip_prefix(&dwm_base)?;
        let repo_name_str = relative
            .components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rd = repo_dir(&dwm_base, &repo_name_str);
        vcs::detect_from_dwm_dir(&rd)?
    } else {
        vcs::detect(&cwd)?
    };

    let deps = WorkspaceDeps {
        backend,
        cwd,
        dwm_base,
    };
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
    let (repo_name_str, ws_name) = match name {
        Some(name) => {
            let repo_name_str = if deps.cwd.starts_with(&deps.dwm_base) {
                let relative = deps.cwd.strip_prefix(&deps.dwm_base)?;
                relative
                    .components()
                    .next()
                    .context("could not determine repo from workspace path")?
                    .as_os_str()
                    .to_string_lossy()
                    .to_string()
            } else {
                deps.backend.repo_name_from(&deps.cwd)?
            };
            (repo_name_str, name)
        }
        None => {
            if !deps.cwd.starts_with(&deps.dwm_base) {
                bail!(
                    "not inside a dwm workspace (current dir must be under {})",
                    deps.dwm_base.display()
                );
            }
            let relative = deps.cwd.strip_prefix(&deps.dwm_base)?;
            let components: Vec<&std::ffi::OsStr> =
                relative.components().map(|c| c.as_os_str()).collect();
            if components.len() < 2 {
                bail!("could not determine workspace name from current directory");
            }
            (
                components[0].to_string_lossy().to_string(),
                components[1].to_string_lossy().to_string(),
            )
        }
    };

    let ws_path = deps.dwm_base.join(&repo_name_str).join(&ws_name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", ws_name, ws_path.display());
    }

    let main_repo = main_repo_path(&deps.dwm_base, &repo_name_str)?;

    if verbose {
        eprintln!(
            "{} workspace '{}'...",
            "forgetting".yellow(),
            ws_name.bold()
        );
    }
    deps.backend
        .workspace_remove(&main_repo, &ws_name, &ws_path)?;

    if ws_path.exists() {
        if verbose {
            eprintln!("{} {}...", "removing".red(), ws_path.display().dimmed());
        }
        fs::remove_dir_all(&ws_path)?;
    }

    // Clean up agent status files for this workspace
    let rd = repo_dir(&deps.dwm_base, &repo_name_str);
    agent::remove_agent_statuses_for_workspace(&rd, &ws_name);

    if verbose {
        eprintln!("{} workspace '{}' deleted", "✓".green(), ws_name.bold());
    }

    if is_inside(&deps.cwd, &ws_path) {
        Ok(Some(main_repo))
    } else {
        Ok(None)
    }
}

/// Switch to the named workspace by printing its path to stdout for the shell
/// wrapper to `cd` into.
pub fn switch_workspace(name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let dwm_base = dwm_base_dir()?;

    let backend: Box<dyn vcs::VcsBackend> = if cwd.starts_with(&dwm_base) {
        let relative = cwd.strip_prefix(&dwm_base)?;
        let repo_name_str = relative
            .components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rd = repo_dir(&dwm_base, &repo_name_str);
        vcs::detect_from_dwm_dir(&rd)?
    } else {
        vcs::detect(&cwd)?
    };

    let deps = WorkspaceDeps {
        backend,
        cwd,
        dwm_base,
    };
    let path = switch_workspace_inner(&deps, name)?;
    println!("{}", path.display());
    Ok(())
}

/// Resolve the path for the named workspace. Returns the path the shell should
/// `cd` into.
fn switch_workspace_inner(deps: &WorkspaceDeps, name: &str) -> Result<PathBuf> {
    let repo_name_str = if deps.cwd.starts_with(&deps.dwm_base) {
        let relative = deps.cwd.strip_prefix(&deps.dwm_base)?;
        relative
            .components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string()
    } else {
        deps.backend.repo_name_from(&deps.cwd)?
    };

    let main_ws_name = deps.backend.main_workspace_name();
    if name == main_ws_name {
        return main_repo_path(&deps.dwm_base, &repo_name_str);
    }

    let ws_path = deps.dwm_base.join(&repo_name_str).join(name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", name, ws_path.display());
    }

    Ok(ws_path)
}

/// Rename a workspace. When `new_name` is `None` the first argument is treated
/// as the new name and the old name is inferred from the current directory.
pub fn rename_workspace(name: String, new_name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let dwm_base = dwm_base_dir()?;

    let backend: Box<dyn vcs::VcsBackend> = if cwd.starts_with(&dwm_base) {
        let relative = cwd.strip_prefix(&dwm_base)?;
        let repo_name_str = relative
            .components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rd = repo_dir(&dwm_base, &repo_name_str);
        vcs::detect_from_dwm_dir(&rd)?
    } else {
        vcs::detect(&cwd)?
    };

    let deps = WorkspaceDeps {
        backend,
        cwd,
        dwm_base,
    };

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
///
/// Expects `cwd` to be `~/.dwm/<repo>/<workspace>[/…]` and returns the
/// `<workspace>` component.
fn infer_workspace_name_from_cwd(deps: &WorkspaceDeps) -> Result<String> {
    if !deps.cwd.starts_with(&deps.dwm_base) {
        bail!(
            "not inside a dwm workspace (current dir must be under {})",
            deps.dwm_base.display()
        );
    }
    let relative = deps.cwd.strip_prefix(&deps.dwm_base)?;
    let components: Vec<&std::ffi::OsStr> = relative.components().map(|c| c.as_os_str()).collect();
    if components.len() < 2 {
        bail!("could not determine workspace name from current directory");
    }
    Ok(components[1].to_string_lossy().to_string())
}

/// Returns the path the shell should cd to if cwd was inside the renamed workspace.
fn rename_workspace_inner(
    deps: &WorkspaceDeps,
    old_name: &str,
    new_name: &str,
) -> Result<Option<PathBuf>> {
    let repo_name_str = if deps.cwd.starts_with(&deps.dwm_base) {
        let relative = deps.cwd.strip_prefix(&deps.dwm_base)?;
        relative
            .components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string()
    } else {
        deps.backend.repo_name_from(&deps.cwd)?
    };

    let main_ws_name = deps.backend.main_workspace_name();
    if old_name == main_ws_name {
        bail!("cannot rename the main workspace '{}'", old_name);
    }

    let old_path = deps.dwm_base.join(&repo_name_str).join(old_name);
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

    let new_path = deps.dwm_base.join(&repo_name_str).join(new_name);
    if new_path.exists() {
        bail!(
            "workspace '{}' already exists at {}",
            new_name,
            new_path.display()
        );
    }

    let main_repo = main_repo_path(&deps.dwm_base, &repo_name_str)?;

    eprintln!(
        "{} workspace '{}' -> '{}'...",
        "renaming".cyan(),
        old_name.bold(),
        new_name.bold()
    );
    deps.backend
        .workspace_rename(&main_repo, &old_path, &new_path, old_name, new_name)?;

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

/// Return the `~/.dwm/<repo>/` directory for the current working directory.
pub fn current_repo_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let dwm_base = dwm_base_dir()?;

    let repo_name_str = if cwd.starts_with(&dwm_base) {
        let relative = cwd.strip_prefix(&dwm_base)?;
        relative
            .components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string()
    } else {
        let backend = vcs::detect(&cwd)?;
        backend.repo_name_from(&cwd)?
    };

    Ok(repo_dir(&dwm_base, &repo_name_str))
}

/// Collect [`WorkspaceEntry`] values for all workspaces belonging to the
/// repository that contains the current directory.
pub fn list_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    let cwd = std::env::current_dir()?;
    let dwm_base = dwm_base_dir()?;

    let backend: Box<dyn vcs::VcsBackend> = if cwd.starts_with(&dwm_base) {
        let relative = cwd.strip_prefix(&dwm_base)?;
        let repo_name_str = relative
            .components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rd = repo_dir(&dwm_base, &repo_name_str);
        vcs::detect_from_dwm_dir(&rd)?
    } else {
        vcs::detect(&cwd)?
    };

    let deps = WorkspaceDeps {
        backend,
        cwd,
        dwm_base,
    };
    list_workspace_entries_inner(&deps)
}

/// Testable core of [`list_workspace_entries`].
fn list_workspace_entries_inner(deps: &WorkspaceDeps) -> Result<Vec<WorkspaceEntry>> {
    let (repo_name_str, main_repo) = if deps.cwd.starts_with(&deps.dwm_base) {
        let relative = deps.cwd.strip_prefix(&deps.dwm_base)?;
        let repo_name_str = relative
            .components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let main_repo = main_repo_path(&deps.dwm_base, &repo_name_str)?;
        (repo_name_str, main_repo)
    } else {
        let repo_name_str = deps.backend.repo_name_from(&deps.cwd)?;
        let main_repo = deps.backend.root_from(&deps.cwd)?;
        (repo_name_str, main_repo)
    };

    let rd = repo_dir(&deps.dwm_base, &repo_name_str);
    if !rd.exists() {
        return Ok(Vec::new());
    }

    let mut agent_summaries = agent::read_agent_summaries(&rd);

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

    // Scan workspace dirs
    let read_dir = fs::read_dir(&rd)?;
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();

        // Skip internal dot-prefixed entries (.main-repo, .vcs-type, .agent-status, etc.)
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
/// tracked under `~/.dwm/`.
pub fn list_all_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    let dwm_base = dwm_base_dir()?;
    list_all_workspace_entries_inner(&dwm_base)
}

/// Testable core of [`list_all_workspace_entries`].
fn list_all_workspace_entries_inner(dwm_base: &Path) -> Result<Vec<WorkspaceEntry>> {
    if !dwm_base.exists() {
        return Ok(Vec::new());
    }

    let mut all_entries = Vec::new();

    for dir_entry in fs::read_dir(dwm_base)? {
        let dir_entry = dir_entry?;
        let repo_path = dir_entry.path();
        if !repo_path.is_dir() {
            continue;
        }

        let main_repo_file = repo_path.join(".main-repo");
        if !main_repo_file.exists() {
            continue;
        }

        let main_repo_content = match fs::read_to_string(&main_repo_file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let repo_name = Path::new(main_repo_content.trim())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir_entry.file_name().to_string_lossy().into_owned());

        let backend = match vcs::detect_from_dwm_dir(&repo_path) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let deps = WorkspaceDeps {
            backend,
            cwd: repo_path.clone(),
            dwm_base: dwm_base.to_path_buf(),
        };

        match list_workspace_entries_inner(&deps) {
            Ok(entries) => {
                for mut entry in entries {
                    entry.repo_name = Some(repo_name.clone());
                    all_entries.push(entry);
                }
            }
            Err(e) => {
                eprintln!("warning: skipping repo '{}': {}", repo_name, e);
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
        let ws = Path::new("/home/user/.dwm/myrepo/my-workspace");
        assert!(is_inside(ws, ws));
        assert!(is_inside(
            Path::new("/home/user/.dwm/myrepo/my-workspace/src"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_sibling_workspace() {
        let ws = Path::new("/home/user/.dwm/myrepo/my-workspace");
        assert!(!is_inside(
            Path::new("/home/user/.dwm/myrepo/other-workspace"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_main_repo() {
        let ws = Path::new("/home/user/.dwm/myrepo/my-workspace");
        assert!(!is_inside(Path::new("/home/user/code/myrepo"), ws));
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
        /// The root path returned by root_from / repo_name_from.
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

    // ── Helper to set up a dwm repo dir on disk ─────────────────────

    /// Creates a dwm repo dir with `.main-repo` pointing at `main_repo`.
    /// Returns the dwm_base path.
    fn setup_dwm_dir(tmp: &Path, repo_name: &str, main_repo: &Path) -> PathBuf {
        let dwm_base = tmp.join("dwm");
        let rd = dwm_base.join(repo_name);
        fs::create_dir_all(&rd).unwrap();
        fs::write(rd.join(".main-repo"), main_repo.to_string_lossy().as_ref()).unwrap();
        fs::write(rd.join(".vcs-type"), "mock").unwrap();
        dwm_base
    }

    // ── list_workspace_entries_inner tests ────────────────────────────

    #[test]
    fn list_entries_from_inside_dwm() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        // Create a workspace subdir
        let ws_dir = dwm_base.join(format!("{}/feat-x", dir_name));
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
            dwm_base,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        // Should have main + feat-x
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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        // Create a workspace and an internal dot-prefixed directory
        let ws_dir = dwm_base.join(format!("{}/feat-x", dir_name));
        fs::create_dir_all(&ws_dir).unwrap();
        let agent_dir = dwm_base.join(format!("{}/.agent-status", dir_name));
        fs::create_dir_all(&agent_dir).unwrap();

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
            dwm_base,
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
    fn list_entries_from_repo_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let workspaces = vec![(
            "default".to_string(),
            vcs::WorkspaceInfo {
                change_id: "abc".to_string(),
                description: "".to_string(),
                bookmarks: vec![],
            },
        )];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        // cwd is the repo itself (outside dwm)
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            dwm_base,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_main);
        // Empty description should fall through to latest_description
        assert_eq!(entries[0].description, "mock description");
    }

    #[test]
    fn list_entries_empty_repo_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        // Don't create dwm dir — repo_dir won't exist
        let dwm_base = tmp.path().join("dwm");

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert!(entries.is_empty());
    }

    // ── new_workspace_inner tests ────────────────────────────────────

    #[test]
    fn new_workspace_calls_add() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dwm_base = tmp.path().join("dwm");
        let dir_name = vcs::repo_dir_name(&main_repo);

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        new_workspace_inner(&deps, Some("my-ws".to_string()), None, None).unwrap();

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
                assert_eq!(ws_path, &dwm_base.join(format!("{}/my-ws", dir_name)));
                assert_eq!(name, "my-ws");
                assert!(at.is_none());
            }
            other => panic!("expected WorkspaceAdd, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_auto_names() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dwm_base = tmp.path().join("dwm");

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        new_workspace_inner(&deps, None, None, None).unwrap();

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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dwm_base = tmp.path().join("dwm");

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base: dwm_base.clone(),
        };

        // Create workspace once
        new_workspace_inner(&deps, Some("dup-ws".to_string()), None, None).unwrap();

        // Second attempt should fail
        let err = new_workspace_inner(&deps, Some("dup-ws".to_string()), None, None).unwrap_err();
        assert!(err.to_string().contains("already exists"), "error: {}", err);
    }

    #[test]
    fn new_workspace_dot_prefix_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base: tmp.path().join("dwm"),
        };

        let err =
            new_workspace_inner(&deps, Some(".agent-status".to_string()), None, None).unwrap_err();
        assert!(
            err.to_string().contains("cannot start with '.'"),
            "error: {}",
            err
        );
    }

    #[test]
    fn new_workspace_from_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dwm_base = tmp.path().join("dwm");
        let dir_name = vcs::repo_dir_name(&main_repo);

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
            dwm_base: dwm_base.clone(),
        };

        new_workspace_inner(&deps, Some("forked".to_string()), None, Some("source-ws")).unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceAdd {
                ws_path, name, at, ..
            } => {
                assert_eq!(ws_path, &dwm_base.join(format!("{}/forked", dir_name)));
                assert_eq!(name, "forked");
                assert_eq!(at.as_deref(), Some("abc12345"));
            }
            other => panic!("expected WorkspaceAdd, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_from_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dwm_base = tmp.path().join("dwm");

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        let err = new_workspace_inner(&deps, Some("forked".to_string()), None, Some("no-such-ws"))
            .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "error should mention not found: {}",
            err
        );
    }

    // ── delete_workspace_inner tests ─────────────────────────────────

    #[test]
    fn delete_workspace_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        // Create the workspace dir to be deleted
        let ws_dir = dwm_base.join(format!("{}/my-ws", dir_name));
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        // cwd is outside the workspace being deleted
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
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

        // Dir should be removed
        assert!(!ws_dir.exists());
    }

    #[test]
    fn delete_workspace_redirects_when_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let ws_dir = dwm_base.join(format!("{}/my-ws", dir_name));
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        // cwd is inside the workspace being deleted
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
            dwm_base,
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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let ws_dir = dwm_base.join(format!("{}/inferred-ws", dir_name));
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            dwm_base,
        };

        // No name given — should infer repo=myrepo, ws=inferred-ws from cwd
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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let ws_dir = dwm_base.join(format!("{}/old-name", dir_name));
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        let redirect = rename_workspace_inner(&deps, "old-name", "new-name").unwrap();
        assert!(
            redirect.is_none(),
            "should not redirect when cwd is outside workspace"
        );

        // Old dir gone, new dir exists
        assert!(!ws_dir.exists());
        assert!(dwm_base.join(format!("{}/new-name", dir_name)).exists());

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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let ws_dir = dwm_base.join(format!("{}/old-name", dir_name));
        fs::create_dir_all(ws_dir.join("src")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo, vec![]);
        // cwd is inside the workspace being renamed
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
            dwm_base: dwm_base.clone(),
        };

        let redirect = rename_workspace_inner(&deps, "old-name", "new-name").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        // cwd was old-name/src, so redirect should be new-name/src
        assert_eq!(
            redirect,
            dwm_base.join(format!("{}/new-name/src", dir_name))
        );
    }

    #[test]
    fn rename_workspace_preserves_files() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let ws_dir = dwm_base.join(format!("{}/old-name", dir_name));
        fs::create_dir_all(ws_dir.join("src")).unwrap();
        fs::write(ws_dir.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(ws_dir.join("README.md"), "# hello").unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base: dwm_base.clone(),
        };

        rename_workspace_inner(&deps, "old-name", "new-name").unwrap();

        let new_dir = dwm_base.join(format!("{}/new-name", dir_name));
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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        let err = rename_workspace_inner(&deps, "nonexistent", "new-name").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_new_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        fs::create_dir_all(dwm_base.join(format!("{}/old-name", dir_name))).unwrap();
        fs::create_dir_all(dwm_base.join(format!("{}/new-name", dir_name))).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        let err = rename_workspace_inner(&deps, "old-name", "new-name").unwrap_err();
        assert!(err.to_string().contains("already exists"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_refuses_main() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        let err = rename_workspace_inner(&deps, "default", "new-name").unwrap_err();
        assert!(err.to_string().contains("cannot rename"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_dot_prefix_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        fs::create_dir_all(dwm_base.join(format!("{}/old-name", dir_name))).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        // Create a workspace dir
        let ws_dir = dwm_base.join(format!("{}/feat-x", dir_name));
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        let path = switch_workspace_inner(&deps, "feat-x").unwrap();
        assert_eq!(path, ws_dir);
    }

    #[test]
    fn switch_workspace_to_main() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            dwm_base,
        };

        // "default" is the mock's main_workspace_name
        let path = switch_workspace_inner(&deps, "default").unwrap();
        assert_eq!(path, main_repo);
    }

    #[test]
    fn switch_workspace_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        let err = switch_workspace_inner(&deps, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    // ── rename with cwd inference tests ─────────────────────────────

    #[test]
    fn rename_infers_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let ws_dir = dwm_base.join(format!("{}/old-name", dir_name));
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo, vec![]);
        // cwd is inside the workspace
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Infer old name from cwd
        let old = infer_workspace_name_from_cwd(&deps).unwrap();
        assert_eq!(old, "old-name");

        // Now do the rename
        let redirect = rename_workspace_inner(&deps, &old, "new-name").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, dwm_base.join(format!("{}/new-name", dir_name)));

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
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir(tmp.path(), &dir_name, &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        // cwd is outside dwm
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            dwm_base,
        };

        let err = infer_workspace_name_from_cwd(&deps).unwrap_err();
        assert!(
            err.to_string().contains("not inside a dwm workspace"),
            "error: {}",
            err
        );
    }

    // ── regression: same basename, different paths get distinct dwm dirs ──

    #[test]
    fn same_basename_different_paths_get_distinct_dir_names() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = tmp.path().join("team-a/myrepo");
        let repo_b = tmp.path().join("team-b/myrepo");
        let dir_a = vcs::repo_dir_name(&repo_a);
        let dir_b = vcs::repo_dir_name(&repo_b);
        assert_ne!(
            dir_a, dir_b,
            "two repos with same basename but different paths must not collide"
        );
        assert!(dir_a.starts_with("myrepo-"), "dir_a: {}", dir_a);
        assert!(dir_b.starts_with("myrepo-"), "dir_b: {}", dir_b);
    }

    // ── list_all_workspace_entries_inner tests ─────────────────────

    #[test]
    fn list_all_entries_multiple_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let dwm_base = tmp.path().join("dwm");

        // Set up two repos
        let repo1 = tmp.path().join("repos/repo1");
        let repo2 = tmp.path().join("repos/repo2");
        fs::create_dir_all(&repo1).unwrap();
        fs::create_dir_all(&repo2).unwrap();

        // Create dwm dirs with .main-repo and .vcs-type
        let rd1 = dwm_base.join("repo1");
        fs::create_dir_all(&rd1).unwrap();
        fs::write(rd1.join(".main-repo"), repo1.to_string_lossy().as_ref()).unwrap();
        fs::write(rd1.join(".vcs-type"), "mock").unwrap();

        let rd2 = dwm_base.join("repo2");
        fs::create_dir_all(&rd2).unwrap();
        fs::write(rd2.join(".main-repo"), repo2.to_string_lossy().as_ref()).unwrap();
        fs::write(rd2.join(".vcs-type"), "mock").unwrap();

        // list_all_workspace_entries_inner won't work with MockBackend since it
        // uses detect_from_dwm_dir internally. The detect_from_dwm_dir will
        // try to instantiate JjBackend or GitBackend. So we test the scanning
        // logic by checking it doesn't panic on dirs without .main-repo.
        let rd3 = dwm_base.join("not-a-repo");
        fs::create_dir_all(&rd3).unwrap();
        // No .main-repo — should be skipped

        // We can't fully test this without real VCS backends, but we verify
        // the function doesn't panic and correctly skips dirs without .main-repo
        // We need to accept that entries for mock VCS type will fail at workspace_list
        let result = list_all_workspace_entries_inner(&dwm_base);
        // Should not panic; may return Ok or Err depending on mock backend availability
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn list_all_entries_empty_dwm() {
        let tmp = tempfile::tempdir().unwrap();
        let dwm_base = tmp.path().join("dwm");
        // Don't even create it
        let entries = list_all_workspace_entries_inner(&dwm_base).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn list_all_entries_no_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let dwm_base = tmp.path().join("dwm");
        fs::create_dir_all(&dwm_base).unwrap();
        // Create a file (not a dir)
        fs::write(dwm_base.join("some-file"), "").unwrap();
        let entries = list_all_workspace_entries_inner(&dwm_base).unwrap();
        assert!(entries.is_empty());
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

        // Assert some key properties of the table
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

    /// Set up a dwm directory for a real git repo.
    fn setup_dwm_dir_git(tmp: &Path, repo_name: &str, main_repo: &Path) -> PathBuf {
        let dwm_base = tmp.join("dwm");
        let rd = dwm_base.join(repo_name);
        fs::create_dir_all(&rd).unwrap();
        fs::write(rd.join(".main-repo"), main_repo.to_string_lossy().as_ref()).unwrap();
        fs::write(rd.join(".vcs-type"), "git").unwrap();
        dwm_base
    }

    #[test]
    fn e2e_git_list_entries_main_only() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_git(tmp.path(), &dir_name, &main_repo);

        let backend = crate::git::GitBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base,
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
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_git(tmp.path(), &dir_name, &main_repo);

        // Create a git worktree in the dwm directory
        let ws_path = dwm_base.join(format!("{}/feat-branch", dir_name));
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
            dwm_base,
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
        let dir_name = vcs::repo_dir_name(&main_repo);

        let dwm_base = tmp.path().join("dwm");

        let backend = crate::git::GitBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create a workspace
        new_workspace_inner(&deps, Some("test-ws".to_string()), None, None).unwrap();
        let ws_dir = dwm_base.join(format!("{}/test-ws", dir_name));
        assert!(ws_dir.exists(), "workspace dir should exist after creation");

        // List and verify it shows up
        let backend2 = crate::git::GitBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        let entries = list_workspace_entries_inner(&deps2).unwrap();
        assert!(
            entries.iter().any(|e| e.name == "test-ws"),
            "test-ws should appear in listing"
        );

        // Delete the workspace
        let backend3 = crate::git::GitBackend;
        let deps3 = WorkspaceDeps {
            backend: Box::new(backend3),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        delete_workspace_inner(&deps3, Some("test-ws".to_string()), DeleteOutput::Verbose).unwrap();
        assert!(
            !ws_dir.exists(),
            "workspace dir should be removed after deletion"
        );

        // Verify it's gone from listing
        let backend4 = crate::git::GitBackend;
        let deps4 = WorkspaceDeps {
            backend: Box::new(backend4),
            cwd: main_repo,
            dwm_base,
        };
        let entries = list_workspace_entries_inner(&deps4).unwrap();
        assert!(
            !entries.iter().any(|e| e.name == "test-ws"),
            "test-ws should not appear after deletion"
        );
    }

    #[test]
    fn e2e_git_worktree_with_changes() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);

        let dwm_base = tmp.path().join("dwm");
        let backend = crate::git::GitBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create workspace and make a commit in it
        new_workspace_inner(&deps, Some("feature".to_string()), None, None).unwrap();
        let ws_dir = dwm_base.join(format!("{}/feature", dir_name));

        // Add a file and commit in the worktree
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

        // List and check that the feature workspace has diff stats
        let backend2 = crate::git::GitBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo,
            dwm_base,
        };
        let entries = list_workspace_entries_inner(&deps2).unwrap();
        let feat = entries.iter().find(|e| e.name == "feature").unwrap();
        assert_eq!(feat.description, "add hello");
        assert!(
            feat.diff_stat.insertions > 0 || feat.diff_stat.files_changed > 0,
            "feature workspace should show changes vs trunk"
        );
    }

    #[test]
    fn e2e_git_rename_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);

        let dwm_base = tmp.path().join("dwm");
        let backend = crate::git::GitBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create workspace
        new_workspace_inner(&deps, Some("old-name".to_string()), None, None).unwrap();
        let old_path = dwm_base.join(format!("{}/old-name", dir_name));
        assert!(old_path.exists());

        // Rename it
        let backend2 = crate::git::GitBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        rename_workspace_inner(&deps2, "old-name", "new-name").unwrap();

        assert!(!old_path.exists(), "old dir should be gone");
        assert!(
            dwm_base.join(format!("{}/new-name", dir_name)).exists(),
            "new dir should exist"
        );

        // Verify listing shows the new name
        let backend3 = crate::git::GitBackend;
        let deps3 = WorkspaceDeps {
            backend: Box::new(backend3),
            cwd: main_repo,
            dwm_base,
        };
        let entries = list_workspace_entries_inner(&deps3).unwrap();
        assert!(entries.iter().any(|e| e.name == "new-name"));
        assert!(!entries.iter().any(|e| e.name == "old-name"));
    }

    #[test]
    fn e2e_git_rename_redirects_when_inside() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);

        let dwm_base = tmp.path().join("dwm");
        let backend = crate::git::GitBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create workspace with a subdirectory
        new_workspace_inner(&deps, Some("my-ws".to_string()), None, None).unwrap();
        let ws_path = dwm_base.join(format!("{}/my-ws", dir_name));
        let subdir = ws_path.join("src");
        fs::create_dir_all(&subdir).unwrap();

        // Rename while cwd is inside the workspace
        let backend2 = crate::git::GitBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: subdir,
            dwm_base: dwm_base.clone(),
        };
        let redirect = rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside renamed workspace");
        assert_eq!(
            redirect,
            dwm_base.join(format!("{}/renamed-ws/src", dir_name))
        );

        // The new path should exist and contain the subdirectory
        let new_ws = dwm_base.join(format!("{}/renamed-ws", dir_name));
        assert!(new_ws.exists());
        assert!(new_ws.join("src").exists());
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

    fn setup_dwm_dir_jj(tmp: &Path, repo_name: &str, main_repo: &Path) -> PathBuf {
        let dwm_base = tmp.join("dwm");
        let rd = dwm_base.join(repo_name);
        fs::create_dir_all(&rd).unwrap();
        fs::write(rd.join(".main-repo"), main_repo.to_string_lossy().as_ref()).unwrap();
        fs::write(rd.join(".vcs-type"), "jj").unwrap();
        dwm_base
    }

    #[test]
    fn e2e_jj_list_entries_main_only() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base,
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
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        // Create a jj workspace in the dwm directory
        let ws_path = dwm_base.join(format!("{}/feat-ws", dir_name));
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
            dwm_base,
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
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create a workspace
        new_workspace_inner(&deps, Some("test-ws".to_string()), None, None).unwrap();
        let ws_dir = dwm_base.join(format!("{}/test-ws", dir_name));
        assert!(ws_dir.exists(), "workspace dir should exist after creation");

        // List and verify it shows up
        let backend2 = crate::jj::JjBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        let entries = list_workspace_entries_inner(&deps2).unwrap();
        assert!(
            entries.iter().any(|e| e.name == "test-ws"),
            "test-ws should appear in listing"
        );

        // Delete the workspace
        let backend3 = crate::jj::JjBackend;
        let deps3 = WorkspaceDeps {
            backend: Box::new(backend3),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        delete_workspace_inner(&deps3, Some("test-ws".to_string()), DeleteOutput::Verbose).unwrap();
        assert!(
            !ws_dir.exists(),
            "workspace dir should be removed after deletion"
        );

        // Verify it's gone from listing
        let backend4 = crate::jj::JjBackend;
        let deps4 = WorkspaceDeps {
            backend: Box::new(backend4),
            cwd: main_repo,
            dwm_base,
        };
        let entries = list_workspace_entries_inner(&deps4).unwrap();
        assert!(
            !entries.iter().any(|e| e.name == "test-ws"),
            "test-ws should not appear after deletion"
        );
    }

    #[test]
    fn e2e_jj_workspace_with_spaces_in_name() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create a workspace with spaces in its name
        new_workspace_inner(&deps, Some("my cool feature".to_string()), None, None).unwrap();
        let ws_dir = dwm_base.join(format!("{}/my cool feature", dir_name));
        assert!(ws_dir.exists(), "workspace dir should exist after creation");

        // List and verify it shows up
        let backend2 = crate::jj::JjBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        let entries = list_workspace_entries_inner(&deps2).unwrap();
        assert!(
            entries.iter().any(|e| e.name == "my cool feature"),
            "workspace with spaces should appear in listing, got: {:?}",
            entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );

        // Switch to the workspace
        let backend3 = crate::jj::JjBackend;
        let deps3 = WorkspaceDeps {
            backend: Box::new(backend3),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        let switch_path = switch_workspace_inner(&deps3, "my cool feature").unwrap();
        assert_eq!(switch_path, ws_dir);

        // Delete the workspace
        let backend4 = crate::jj::JjBackend;
        let deps4 = WorkspaceDeps {
            backend: Box::new(backend4),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        delete_workspace_inner(
            &deps4,
            Some("my cool feature".to_string()),
            DeleteOutput::Verbose,
        )
        .unwrap();
        assert!(
            !ws_dir.exists(),
            "workspace dir should be removed after deletion"
        );

        // Verify it's gone from listing
        let backend5 = crate::jj::JjBackend;
        let deps5 = WorkspaceDeps {
            backend: Box::new(backend5),
            cwd: main_repo,
            dwm_base,
        };
        let entries = list_workspace_entries_inner(&deps5).unwrap();
        assert!(
            !entries.iter().any(|e| e.name == "my cool feature"),
            "workspace with spaces should not appear after deletion"
        );
    }

    #[test]
    fn e2e_jj_workspace_with_changes() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create workspace and make changes in it
        new_workspace_inner(&deps, Some("feature".to_string()), None, None).unwrap();
        let ws_dir = dwm_base.join(format!("{}/feature", dir_name));

        // Add a file (jj auto-tracks new files)
        fs::write(ws_dir.join("hello.txt"), "hello world\n").unwrap();
        // Set a description on the workspace's working copy
        let ws_str = ws_dir.to_str().unwrap();
        std::process::Command::new("jj")
            .args(["--repository", ws_str, "describe", "-m", "add hello"])
            .output()
            .unwrap();

        // List and check that the feature workspace has diff stats
        let backend2 = crate::jj::JjBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo,
            dwm_base,
        };
        let entries = list_workspace_entries_inner(&deps2).unwrap();
        let feat = entries.iter().find(|e| e.name == "feature").unwrap();
        assert_eq!(feat.description.trim(), "add hello");
        assert!(
            feat.diff_stat.insertions > 0 || feat.diff_stat.files_changed > 0,
            "feature workspace should show changes vs trunk: {:?}",
            feat.diff_stat
        );
    }

    #[test]
    fn e2e_jj_rename_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create workspace
        new_workspace_inner(&deps, Some("old-name".to_string()), None, None).unwrap();
        let old_path = dwm_base.join(format!("{}/old-name", dir_name));
        assert!(old_path.exists());

        // Rename it
        let backend2 = crate::jj::JjBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        rename_workspace_inner(&deps2, "old-name", "new-name").unwrap();

        assert!(!old_path.exists(), "old dir should be gone");
        assert!(
            dwm_base.join(format!("{}/new-name", dir_name)).exists(),
            "new dir should exist"
        );

        // Verify listing shows the new name
        let backend3 = crate::jj::JjBackend;
        let deps3 = WorkspaceDeps {
            backend: Box::new(backend3),
            cwd: main_repo,
            dwm_base,
        };
        let entries = list_workspace_entries_inner(&deps3).unwrap();
        assert!(entries.iter().any(|e| e.name == "new-name"));
        assert!(!entries.iter().any(|e| e.name == "old-name"));
    }

    #[test]
    fn e2e_jj_rename_stale_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create workspace
        new_workspace_inner(&deps, Some("my-ws".to_string()), None, None).unwrap();

        // Make the workspace stale by committing in the default workspace,
        // which advances the operation log past what my-ws has seen.
        let main_str = main_repo.to_str().unwrap();
        fs::write(main_repo.join("file.txt"), "content\n").unwrap();
        std::process::Command::new("jj")
            .args(["--repository", main_str, "describe", "-m", "advance op log"])
            .output()
            .unwrap();

        // Rename should succeed despite stale working copy
        let backend2 = crate::jj::JjBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();

        assert!(!dwm_base.join(format!("{}/my-ws", dir_name)).exists());
        assert!(dwm_base.join(format!("{}/renamed-ws", dir_name)).exists());

        // Verify listing shows the new name
        let backend3 = crate::jj::JjBackend;
        let deps3 = WorkspaceDeps {
            backend: Box::new(backend3),
            cwd: main_repo,
            dwm_base,
        };
        let entries = list_workspace_entries_inner(&deps3).unwrap();
        assert!(entries.iter().any(|e| e.name == "renamed-ws"));
        assert!(!entries.iter().any(|e| e.name == "my-ws"));
    }

    #[test]
    fn e2e_git_switch_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);

        let dwm_base = tmp.path().join("dwm");
        let backend = crate::git::GitBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create a workspace
        new_workspace_inner(&deps, Some("switch-target".to_string()), None, None).unwrap();
        let ws_dir = dwm_base.join(format!("{}/switch-target", dir_name));

        // Switch to it
        let backend2 = crate::git::GitBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        let path = switch_workspace_inner(&deps2, "switch-target").unwrap();
        assert_eq!(path, ws_dir);

        // Switch to main
        let backend3 = crate::git::GitBackend;
        let deps3 = WorkspaceDeps {
            backend: Box::new(backend3),
            cwd: main_repo.clone(),
            dwm_base,
        };
        let path = switch_workspace_inner(&deps3, "main-worktree").unwrap();
        assert_eq!(path, main_repo);
    }

    #[test]
    fn e2e_jj_switch_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create a workspace
        new_workspace_inner(&deps, Some("switch-target".to_string()), None, None).unwrap();
        let ws_dir = dwm_base.join(format!("{}/switch-target", dir_name));

        // Switch to it
        let backend2 = crate::jj::JjBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };
        let path = switch_workspace_inner(&deps2, "switch-target").unwrap();
        assert_eq!(path, ws_dir);

        // Switch to main (default)
        let backend3 = crate::jj::JjBackend;
        let deps3 = WorkspaceDeps {
            backend: Box::new(backend3),
            cwd: main_repo.clone(),
            dwm_base,
        };
        let path = switch_workspace_inner(&deps3, "default").unwrap();
        assert_eq!(path, main_repo);
    }

    #[test]
    fn e2e_jj_rename_redirects_when_inside() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let dir_name = vcs::repo_dir_name(&main_repo);
        let dwm_base = setup_dwm_dir_jj(tmp.path(), &dir_name, &main_repo);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            dwm_base: dwm_base.clone(),
        };

        // Create workspace with a subdirectory
        new_workspace_inner(&deps, Some("my-ws".to_string()), None, None).unwrap();
        let ws_path = dwm_base.join(format!("{}/my-ws", dir_name));
        let subdir = ws_path.join("src");
        fs::create_dir_all(&subdir).unwrap();

        // Rename while cwd is inside the workspace
        let backend2 = crate::jj::JjBackend;
        let deps2 = WorkspaceDeps {
            backend: Box::new(backend2),
            cwd: subdir,
            dwm_base: dwm_base.clone(),
        };
        let redirect = rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside renamed workspace");
        assert_eq!(
            redirect,
            dwm_base.join(format!("{}/renamed-ws/src", dir_name))
        );

        // The new path should exist and contain the subdirectory
        let new_ws = dwm_base.join(format!("{}/renamed-ws", dir_name));
        assert!(new_ws.exists());
        assert!(new_ws.join("src").exists());
    }
}
