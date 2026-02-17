use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{names, vcs};

fn is_inside(cwd: &std::path::Path, ws_path: &std::path::Path) -> bool {
    cwd.starts_with(ws_path)
}

fn jjws_base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".jjws"))
}

fn repo_dir(jjws_base: &Path, repo_name: &str) -> PathBuf {
    jjws_base.join(repo_name)
}

fn main_repo_path(jjws_base: &Path, repo_name: &str) -> Result<PathBuf> {
    let repo_dir = repo_dir(jjws_base, repo_name);
    let main_repo_file = repo_dir.join(".main-repo");
    let path = fs::read_to_string(&main_repo_file)
        .with_context(|| format!("could not read {}", main_repo_file.display()))?;
    Ok(PathBuf::from(path.trim()))
}

fn ensure_repo_dir(jjws_base: &Path, repo_name: &str, main_repo_root: &Path, vcs_type: &str) -> Result<PathBuf> {
    let dir = repo_dir(jjws_base, repo_name);
    fs::create_dir_all(&dir)?;
    let main_repo_file = dir.join(".main-repo");
    if !main_repo_file.exists() {
        fs::write(&main_repo_file, main_repo_root.to_string_lossy().as_ref())?;
    }
    let vcs_file = dir.join(".vcs-type");
    if !vcs_file.exists() {
        fs::write(&vcs_file, vcs_type)?;
    }
    Ok(dir)
}

struct WorkspaceDeps {
    backend: Box<dyn vcs::VcsBackend>,
    cwd: PathBuf,
    jjws_base: PathBuf,
}

pub fn new_workspace(name: Option<String>, at: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let jjws_base = jjws_base_dir()?;
    let deps = WorkspaceDeps { backend, cwd, jjws_base };
    new_workspace_inner(&deps, name, at)
}

fn new_workspace_inner(deps: &WorkspaceDeps, name: Option<String>, at: Option<&str>) -> Result<()> {
    let repo_name = deps.backend.repo_name_from(&deps.cwd)?;
    let root = deps.backend.root_from(&deps.cwd)?;
    let dir = ensure_repo_dir(&deps.jjws_base, &repo_name, &root, deps.backend.vcs_name())?;

    let ws_name = match name {
        Some(n) => n,
        None => names::generate_unique(&dir),
    };

    let ws_path = dir.join(&ws_name);
    if ws_path.exists() {
        bail!("workspace '{}' already exists at {}", ws_name, ws_path.display());
    }

    eprintln!("creating workspace '{}'...", ws_name);
    deps.backend.workspace_add(&root, &ws_path, &ws_name, at)?;
    eprintln!("workspace '{}' created at {}", ws_name, ws_path.display());

    // stdout: path for shell wrapper to cd into
    println!("{}", ws_path.display());
    Ok(())
}

/// Deletes a workspace. Returns `true` if the cwd was inside the deleted
/// workspace and a redirect path was printed to stdout.
pub fn delete_workspace(name: Option<String>) -> Result<bool> {
    let cwd = std::env::current_dir()?;
    let jjws_base = jjws_base_dir()?;

    // We need a backend for the repo-name-from-cwd case.
    // When inside jjws dir we detect from the jjws repo dir;
    // otherwise we detect from cwd.
    let backend: Box<dyn vcs::VcsBackend> = if cwd.starts_with(&jjws_base) {
        let relative = cwd.strip_prefix(&jjws_base)?;
        let repo_name_str = relative.components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rd = repo_dir(&jjws_base, &repo_name_str);
        vcs::detect_from_jjws_dir(&rd)?
    } else {
        vcs::detect(&cwd)?
    };

    let deps = WorkspaceDeps { backend, cwd, jjws_base };
    delete_workspace_inner(&deps, name)
}

fn delete_workspace_inner(deps: &WorkspaceDeps, name: Option<String>) -> Result<bool> {
    let (repo_name_str, ws_name) = match name {
        Some(name) => {
            let repo_name_str = if deps.cwd.starts_with(&deps.jjws_base) {
                let relative = deps.cwd.strip_prefix(&deps.jjws_base)?;
                relative.components()
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
            if !deps.cwd.starts_with(&deps.jjws_base) {
                bail!(
                    "not inside a jjws workspace (current dir must be under {})",
                    deps.jjws_base.display()
                );
            }
            let relative = deps.cwd.strip_prefix(&deps.jjws_base)?;
            let components: Vec<&std::ffi::OsStr> = relative.components()
                .map(|c| c.as_os_str())
                .collect();
            if components.len() < 2 {
                bail!("could not determine workspace name from current directory");
            }
            (
                components[0].to_string_lossy().to_string(),
                components[1].to_string_lossy().to_string(),
            )
        }
    };

    let ws_path = deps.jjws_base.join(&repo_name_str).join(&ws_name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", ws_name, ws_path.display());
    }

    let main_repo = main_repo_path(&deps.jjws_base, &repo_name_str)?;

    eprintln!("forgetting workspace '{}'...", ws_name);
    deps.backend.workspace_remove(&main_repo, &ws_name, &ws_path)?;

    eprintln!("removing {}...", ws_path.display());
    fs::remove_dir_all(&ws_path)?;
    eprintln!("workspace '{}' deleted", ws_name);

    if is_inside(&deps.cwd, &ws_path) {
        println!("{}", main_repo.display());
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn rename_workspace(old_name: String, new_name: String) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let jjws_base = jjws_base_dir()?;

    let backend: Box<dyn vcs::VcsBackend> = if cwd.starts_with(&jjws_base) {
        let relative = cwd.strip_prefix(&jjws_base)?;
        let repo_name_str = relative.components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rd = repo_dir(&jjws_base, &repo_name_str);
        vcs::detect_from_jjws_dir(&rd)?
    } else {
        vcs::detect(&cwd)?
    };

    let deps = WorkspaceDeps { backend, cwd, jjws_base };
    rename_workspace_inner(&deps, &old_name, &new_name)
}

fn rename_workspace_inner(deps: &WorkspaceDeps, old_name: &str, new_name: &str) -> Result<()> {
    let repo_name_str = if deps.cwd.starts_with(&deps.jjws_base) {
        let relative = deps.cwd.strip_prefix(&deps.jjws_base)?;
        relative.components()
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

    let old_path = deps.jjws_base.join(&repo_name_str).join(old_name);
    if !old_path.exists() {
        bail!("workspace '{}' not found at {}", old_name, old_path.display());
    }

    let new_path = deps.jjws_base.join(&repo_name_str).join(new_name);
    if new_path.exists() {
        bail!("workspace '{}' already exists at {}", new_name, new_path.display());
    }

    let main_repo = main_repo_path(&deps.jjws_base, &repo_name_str)?;

    // Find the change_id for the old workspace
    let workspaces = deps.backend.workspace_list(&main_repo)?;
    let change_id = workspaces
        .iter()
        .find(|(n, _)| n == old_name)
        .map(|(_, info)| info.change_id.clone())
        .with_context(|| format!("workspace '{}' not found in VCS", old_name))?;

    // Remove old workspace from VCS
    eprintln!("forgetting workspace '{}'...", old_name);
    deps.backend.workspace_remove(&main_repo, old_name, &old_path)?;

    // Rename the directory
    eprintln!("renaming {} -> {}...", old_name, new_name);
    fs::rename(&old_path, &new_path)?;

    // Re-add with new name at the same change
    eprintln!("re-adding workspace '{}' at {}...", new_name, change_id);
    deps.backend.workspace_add(&main_repo, &new_path, new_name, Some(&change_id))?;

    eprintln!("workspace '{}' renamed to '{}'", old_name, new_name);
    Ok(())
}

pub fn list_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    let cwd = std::env::current_dir()?;
    let jjws_base = jjws_base_dir()?;

    let backend: Box<dyn vcs::VcsBackend> = if cwd.starts_with(&jjws_base) {
        let relative = cwd.strip_prefix(&jjws_base)?;
        let repo_name_str = relative.components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rd = repo_dir(&jjws_base, &repo_name_str);
        vcs::detect_from_jjws_dir(&rd)?
    } else {
        vcs::detect(&cwd)?
    };

    let deps = WorkspaceDeps { backend, cwd, jjws_base };
    list_workspace_entries_inner(&deps)
}

fn list_workspace_entries_inner(deps: &WorkspaceDeps) -> Result<Vec<WorkspaceEntry>> {
    let (repo_name_str, main_repo) = if deps.cwd.starts_with(&deps.jjws_base) {
        let relative = deps.cwd.strip_prefix(&deps.jjws_base)?;
        let repo_name_str = relative.components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let main_repo = main_repo_path(&deps.jjws_base, &repo_name_str)?;
        (repo_name_str, main_repo)
    } else {
        let repo_name_str = deps.backend.repo_name_from(&deps.cwd)?;
        let main_repo = deps.backend.root_from(&deps.cwd)?;
        (repo_name_str, main_repo)
    };

    let rd = repo_dir(&deps.jjws_base, &repo_name_str);
    if !rd.exists() {
        return Ok(Vec::new());
    }

    let main_ws_name = deps.backend.main_workspace_name();
    let vcs_workspaces = deps.backend.workspace_list(&main_repo).unwrap_or_default();

    let mut entries = Vec::new();

    // Find info for the main workspace
    let main_info = vcs_workspaces
        .iter()
        .find(|(n, _)| n == main_ws_name)
        .map(|(_, info)| info.clone())
        .unwrap_or_default();

    let main_stat = deps.backend
        .diff_stat_vs_trunk(&main_repo, &main_repo, main_ws_name)
        .unwrap_or_default();
    let main_modified = fs::metadata(&main_repo)
        .and_then(|m| m.modified())
        .ok();
    let main_description = if main_info.description.trim().is_empty() {
        deps.backend.latest_description(&main_repo, &main_repo, main_ws_name)
    } else {
        main_info.description.clone()
    };
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

        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok();

        let is_merged = if has_info {
            deps.backend.is_merged_into_trunk(&main_repo, &path, &name)
        } else {
            false
        };

        entries.push(WorkspaceEntry {
            is_stale: compute_is_stale(false, is_merged, modified),
            name,
            path,
            last_modified: modified,
            diff_stat: stat,
            is_main: false,
            change_id: info.change_id,
            description,
            bookmarks: info.bookmarks,
        });
    }

    Ok(entries)
}

const STALE_DAYS: u64 = 30;

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
}

fn compute_is_stale(
    is_main: bool,
    is_merged: bool,
    last_modified: Option<SystemTime>,
) -> bool {
    if is_main {
        return false;
    }
    if is_merged {
        return true;
    }
    if let Some(time) = last_modified
        && let Ok(duration) = time.elapsed()
    {
        return duration.as_secs() > STALE_DAYS * 86400;
    }
    false
}

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

pub fn print_status(entries: &[WorkspaceEntry]) {
    let mut out = std::io::stderr().lock();
    // Column widths
    let name_w = entries.iter().map(|e| {
        let display = if e.is_main { format!("{} (main)", e.name) } else { e.name.clone() };
        display.len()
    }).max().unwrap_or(4).max(4);
    let change_w = 8;
    let bookmark_w = entries.iter().map(|e| e.bookmarks.join(", ").len()).max().unwrap_or(9).max(9);

    // Header
    let _ = writeln!(
        out,
        "{:<name_w$}  {:<change_w$}  {:<40}  {:<bookmark_w$}  {:<9}  CHANGES",
        "NAME", "CHANGE", "DESCRIPTION", "BOOKMARKS", "MODIFIED",
    );

    for entry in entries {
        let name_text = if entry.is_main {
            format!("{} (main)", entry.name)
        } else if entry.is_stale {
            format!("{} [stale]", entry.name)
        } else {
            entry.name.clone()
        };

        let desc = entry.description.lines().next().unwrap_or("");
        let desc_text: String = desc.chars().take(40).collect();

        let bookmarks_text = entry.bookmarks.join(", ");
        let time_text = format_time_ago(entry.last_modified);

        let stat = &entry.diff_stat;
        let changes_text = if stat.files_changed == 0 && stat.insertions == 0 && stat.deletions == 0 {
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

        let _ = writeln!(
            out,
            "{:<name_w$}  {:<change_w$}  {:<40}  {:<bookmark_w$}  {:<9}  {}",
            name_text,
            entry.change_id,
            desc_text,
            bookmarks_text,
            time_text,
            changes_text,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    #[test]
    fn is_inside_detects_cwd_within_workspace() {
        let ws = Path::new("/home/user/.jjws/myrepo/my-workspace");
        assert!(is_inside(ws, ws));
        assert!(is_inside(
            Path::new("/home/user/.jjws/myrepo/my-workspace/src"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_sibling_workspace() {
        let ws = Path::new("/home/user/.jjws/myrepo/my-workspace");
        assert!(!is_inside(
            Path::new("/home/user/.jjws/myrepo/other-workspace"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_main_repo() {
        let ws = Path::new("/home/user/.jjws/myrepo/my-workspace");
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
        fn new(root: PathBuf, workspaces: Vec<(String, vcs::WorkspaceInfo)>) -> (Self, Arc<Mutex<Vec<MockCall>>>) {
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

        fn workspace_add(&self, repo_dir: &Path, ws_path: &Path, name: &str, at: Option<&str>) -> Result<()> {
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

        fn diff_stat_vs_trunk(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            _ws_name: &str,
        ) -> Result<vcs::DiffStat> {
            Ok(vcs::DiffStat { files_changed: 1, insertions: 10, deletions: 2 })
        }

        fn latest_description(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            _ws_name: &str,
        ) -> String {
            "mock description".to_string()
        }

        fn is_merged_into_trunk(&self, _repo_dir: &Path, _worktree_dir: &Path, _ws_name: &str) -> bool {
            false
        }

        fn vcs_name(&self) -> &'static str {
            "mock"
        }

        fn main_workspace_name(&self) -> &'static str {
            "default"
        }
    }

    // ── Helper to set up a jjws repo dir on disk ─────────────────────

    /// Creates a jjws repo dir with `.main-repo` pointing at `main_repo`.
    /// Returns the jjws_base path.
    fn setup_jjws_dir(tmp: &Path, repo_name: &str, main_repo: &Path) -> PathBuf {
        let jjws_base = tmp.join("jjws");
        let rd = jjws_base.join(repo_name);
        fs::create_dir_all(&rd).unwrap();
        fs::write(rd.join(".main-repo"), main_repo.to_string_lossy().as_ref()).unwrap();
        fs::write(rd.join(".vcs-type"), "mock").unwrap();
        jjws_base
    }

    // ── list_workspace_entries_inner tests ────────────────────────────

    #[test]
    fn list_entries_from_inside_jjws() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        // Create a workspace subdir
        let ws_dir = jjws_base.join("myrepo/feat-x");
        fs::create_dir_all(&ws_dir).unwrap();

        let workspaces = vec![
            ("default".to_string(), vcs::WorkspaceInfo {
                change_id: "aaa".to_string(),
                description: "main desc".to_string(),
                bookmarks: vec!["main".to_string()],
            }),
            ("feat-x".to_string(), vcs::WorkspaceInfo {
                change_id: "bbb".to_string(),
                description: "feature".to_string(),
                bookmarks: vec![],
            }),
        ];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            jjws_base,
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
    fn list_entries_from_repo_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        let workspaces = vec![
            ("default".to_string(), vcs::WorkspaceInfo {
                change_id: "abc".to_string(),
                description: "".to_string(),
                bookmarks: vec![],
            }),
        ];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        // cwd is the repo itself (outside jjws)
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            jjws_base,
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
        // Don't create jjws dir — repo_dir won't exist
        let jjws_base = tmp.path().join("jjws");

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            jjws_base,
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
        let jjws_base = tmp.path().join("jjws");

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            jjws_base: jjws_base.clone(),
        };

        new_workspace_inner(&deps, Some("my-ws".to_string()), None).unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceAdd { repo_dir, ws_path, name, at } => {
                assert_eq!(repo_dir, &main_repo);
                assert_eq!(ws_path, &jjws_base.join("myrepo/my-ws"));
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
        let jjws_base = tmp.path().join("jjws");

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            jjws_base,
        };

        new_workspace_inner(&deps, None, None).unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceAdd { name, .. } => {
                // Auto-generated name should be non-empty and contain a hyphen (adjective-noun)
                assert!(!name.is_empty());
                assert!(name.contains('-'), "auto name should be adjective-noun: {}", name);
            }
            other => panic!("expected WorkspaceAdd, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_duplicate_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = tmp.path().join("jjws");

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            jjws_base: jjws_base.clone(),
        };

        // Create workspace once
        new_workspace_inner(&deps, Some("dup-ws".to_string()), None).unwrap();

        // Second attempt should fail
        let err = new_workspace_inner(&deps, Some("dup-ws".to_string()), None).unwrap_err();
        assert!(err.to_string().contains("already exists"), "error: {}", err);
    }

    // ── delete_workspace_inner tests ─────────────────────────────────

    #[test]
    fn delete_workspace_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        // Create the workspace dir to be deleted
        let ws_dir = jjws_base.join("myrepo/my-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        // cwd is outside the workspace being deleted
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            jjws_base: jjws_base.clone(),
        };

        let redirected = delete_workspace_inner(&deps, Some("my-ws".to_string())).unwrap();
        assert!(!redirected);

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::WorkspaceRemove { repo_dir, name, ws_path } => {
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
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        let ws_dir = jjws_base.join("myrepo/my-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo, vec![]);
        // cwd is inside the workspace being deleted
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
            jjws_base,
        };

        let redirected = delete_workspace_inner(&deps, Some("my-ws".to_string())).unwrap();
        assert!(redirected);
    }

    #[test]
    fn delete_workspace_infers_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        let ws_dir = jjws_base.join("myrepo/inferred-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            jjws_base,
        };

        // No name given — should infer repo=myrepo, ws=inferred-ws from cwd
        let _redirected = delete_workspace_inner(&deps, None).unwrap();

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
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            jjws_base,
        };

        let err = delete_workspace_inner(&deps, Some("nonexistent".to_string())).unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    // ── rename_workspace_inner tests ──────────────────────────────

    #[test]
    fn rename_workspace_success() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        let ws_dir = jjws_base.join("myrepo/old-name");
        fs::create_dir_all(&ws_dir).unwrap();

        let workspaces = vec![
            ("default".to_string(), vcs::WorkspaceInfo {
                change_id: "aaa".to_string(),
                description: "".to_string(),
                bookmarks: vec![],
            }),
            ("old-name".to_string(), vcs::WorkspaceInfo {
                change_id: "bbb".to_string(),
                description: "some work".to_string(),
                bookmarks: vec![],
            }),
        ];

        let (mock, calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            jjws_base: jjws_base.clone(),
        };

        rename_workspace_inner(&deps, "old-name", "new-name").unwrap();

        // Old dir gone, new dir exists
        assert!(!ws_dir.exists());
        assert!(jjws_base.join("myrepo/new-name").exists());

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        // First call: remove old
        match &calls[0] {
            MockCall::WorkspaceRemove { name, .. } => assert_eq!(name, "old-name"),
            other => panic!("expected WorkspaceRemove, got {:?}", other),
        }
        // Second call: add new at same change
        match &calls[1] {
            MockCall::WorkspaceAdd { name, at, .. } => {
                assert_eq!(name, "new-name");
                assert_eq!(at.as_deref(), Some("bbb"));
            }
            other => panic!("expected WorkspaceAdd, got {:?}", other),
        }
    }

    #[test]
    fn rename_workspace_old_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            jjws_base,
        };

        let err = rename_workspace_inner(&deps, "nonexistent", "new-name").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_new_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        fs::create_dir_all(jjws_base.join("myrepo/old-name")).unwrap();
        fs::create_dir_all(jjws_base.join("myrepo/new-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            jjws_base,
        };

        let err = rename_workspace_inner(&deps, "old-name", "new-name").unwrap_err();
        assert!(err.to_string().contains("already exists"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_refuses_main() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&main_repo).unwrap();
        let jjws_base = setup_jjws_dir(tmp.path(), "myrepo", &main_repo);

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            jjws_base,
        };

        let err = rename_workspace_inner(&deps, "default", "new-name").unwrap_err();
        assert!(err.to_string().contains("cannot rename"), "error: {}", err);
    }

    // ── compute_is_stale tests ────────────────────────────────────

    #[test]
    fn stale_main_is_never_stale() {
        assert!(!compute_is_stale(true, true, None));
        assert!(!compute_is_stale(true, false, None));
    }

    #[test]
    fn stale_merged_workspace_is_stale() {
        assert!(compute_is_stale(false, true, Some(SystemTime::now())));
    }

    #[test]
    fn stale_old_workspace_is_stale() {
        let old_time = SystemTime::now() - std::time::Duration::from_secs(86400 * 31);
        assert!(compute_is_stale(false, false, Some(old_time)));
    }

    #[test]
    fn stale_recent_workspace_is_not_stale() {
        let recent = SystemTime::now() - std::time::Duration::from_secs(86400 * 5);
        assert!(!compute_is_stale(false, false, Some(recent)));
    }

    #[test]
    fn stale_unknown_time_not_merged_is_not_stale() {
        assert!(!compute_is_stale(false, false, None));
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
                diff_stat: vcs::DiffStat { files_changed: 1, insertions: 10, deletions: 2 },
                is_main: true,
                change_id: "abc12345".to_string(),
                description: "main workspace".to_string(),
                bookmarks: vec!["main".to_string()],
                is_stale: false,
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
            },
        ];
        // Should not panic; output goes to stderr
        print_status(&entries);
    }
}
