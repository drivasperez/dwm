use std::fs;
use std::path::{Path, PathBuf};

use crate::names::WorkspaceName;

/// Return `true` if `cwd` is equal to or a subdirectory of `ws_path`.
pub(crate) fn is_inside(cwd: &Path, ws_path: &Path) -> bool {
    cwd.starts_with(ws_path)
}

/// Suffix appended to the repo's directory name to form its dwm directory.
pub(crate) const DWM_DIR_SUFFIX: &str = ".dwm";

/// Return the per-repo dwm directory: `<repo_parent>/<repo_name>.dwm/`, a
/// sibling of the repo root.
///
/// It lives outside the repo so that workspaces are not nested inside the
/// main checkout. A jj workspace has no `.git` of its own, so git (and editors
/// that use it, like Zed) walk up to the enclosing repo; if that repo ignores
/// the workspace's path, every file in the workspace is treated as ignored and
/// hidden from search.
///
/// Falls back to `<repo_root>/.dwm/` for a root with no final component (`/`),
/// which has no parent to put a sibling in.
pub(crate) fn dwm_dir(repo_root: &Path) -> PathBuf {
    match (repo_root.parent(), repo_root.file_name()) {
        (Some(parent), Some(name)) => {
            let mut dir_name = name.to_os_string();
            dir_name.push(DWM_DIR_SUFFIX);
            parent.join(dir_name)
        }
        _ => repo_root.join(DWM_DIR_SUFFIX),
    }
}

/// Inverse of [`dwm_dir`]: given a candidate dwm directory, return the repo
/// root it would belong to, if its name has the dwm suffix.
pub(crate) fn repo_root_for_dwm_dir(dwm: &Path) -> Option<PathBuf> {
    let name = dwm.file_name()?.to_str()?;
    let repo_name = name.strip_suffix(DWM_DIR_SUFFIX)?;
    if repo_name.is_empty() {
        return None;
    }
    Some(dwm.parent()?.join(repo_name))
}

/// Return the per-repo worktrees directory: `<repo_parent>/<repo_name>.dwm/worktrees/`.
///
/// This is the namespace workspaces live under. Sibling dirs under the dwm
/// directory (e.g. `agent-status/`) hold dwm-internal state and never collide
/// with workspace names.
pub(crate) fn worktrees_dir(repo_root: &Path) -> PathBuf {
    dwm_dir(repo_root).join("worktrees")
}

/// Return `true` if `dir` looks like a VCS root (contains `.jj` or `.git`).
pub(crate) fn is_vcs_root(dir: &Path) -> bool {
    dir.join(".jj").is_dir() || dir.join(".git").exists()
}

pub(crate) fn workspace_path(worktrees: &Path, name: &WorkspaceName) -> PathBuf {
    worktrees.join(name)
}

pub(crate) fn workspace_entry_name(entry: &fs::DirEntry) -> Option<WorkspaceName> {
    WorkspaceName::try_from(entry.file_name().into_string().ok()?).ok()
}

/// Walk up from `cwd` looking for `<repo_parent>/<repo_name>.dwm/worktrees/<workspace>/`.
///
/// Returns `Some((repo_root, workspace_name, workspace_path))` when the cwd
/// is inside a dwm-managed workspace, or `None` otherwise.
pub fn find_dwm_workspace(cwd: &Path) -> Option<(PathBuf, WorkspaceName, PathBuf)> {
    // Walk from cwd upwards and check each ancestor.
    let mut current = cwd.to_path_buf();
    loop {
        // If `current`'s parent is `worktrees`, its grandparent is
        // `<repo_name>.dwm`, and the sibling `<repo_name>` is a VCS root, then
        // `current` is the workspace dir.
        if let Some(parent) = current.parent()
            && parent.file_name().and_then(|n| n.to_str()) == Some("worktrees")
            && let Some(dwm) = parent.parent()
            && let Some(root) = repo_root_for_dwm_dir(dwm)
            && is_vcs_root(&root)
        {
            let ws_name = WorkspaceName::parse(current.file_name()?.to_str()?).ok()?;
            return Some((root, ws_name, current));
        }
        if !current.pop() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_utils::make_fake_repo;

    #[test]
    fn is_inside_detects_cwd_within_workspace() {
        let ws = Path::new("/home/user/repos/myrepo.dwm/worktrees/my-workspace");
        assert!(is_inside(ws, ws));
        assert!(is_inside(
            Path::new("/home/user/repos/myrepo.dwm/worktrees/my-workspace/src"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_sibling_workspace() {
        let ws = Path::new("/home/user/repos/myrepo.dwm/worktrees/my-workspace");
        assert!(!is_inside(
            Path::new("/home/user/repos/myrepo.dwm/worktrees/other-workspace"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_main_repo() {
        let ws = Path::new("/home/user/repos/myrepo.dwm/worktrees/my-workspace");
        assert!(!is_inside(Path::new("/home/user/code/myrepo"), ws));
    }

    #[test]
    fn find_dwm_workspace_in_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let ws = worktrees_dir(&repo).join("feature");
        fs::create_dir_all(&ws).unwrap();

        let (root, name, path) = find_dwm_workspace(&ws).unwrap();
        assert_eq!(root, repo);
        assert_eq!(name.as_str(), "feature");
        assert_eq!(path, ws);
    }

    #[test]
    fn find_dwm_workspace_in_workspace_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".git");
        let ws = worktrees_dir(&repo).join("feat-x");
        let sub = ws.join("src/inner");
        fs::create_dir_all(&sub).unwrap();

        let (root, name, path) = find_dwm_workspace(&sub).unwrap();
        assert_eq!(root, repo);
        assert_eq!(name.as_str(), "feat-x");
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
        // Create notarepo.dwm/worktrees/ws without a VCS root sibling
        let fake = tmp.path().join("notarepo");
        let ws = worktrees_dir(&fake).join("feature");
        fs::create_dir_all(&ws).unwrap();
        assert!(find_dwm_workspace(&ws).is_none());
    }

    #[test]
    fn find_dwm_workspace_ignores_sibling_internal_dirs() {
        // Regression: pre-refactor, find_dwm_workspace returned `.agent-status`
        // as a workspace name when cwd was under `<root>/.dwm/.agent-status/` (the layout at the time).
        // With the worktrees/ namespace that confusion is gone.
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let agent = dwm_dir(&repo).join("agent-status");
        fs::create_dir_all(&agent).unwrap();
        assert!(find_dwm_workspace(&agent).is_none());
    }

    #[test]
    fn find_dwm_workspace_allows_dot_prefixed_names() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let ws = worktrees_dir(&repo).join(".hidden-feature");
        fs::create_dir_all(&ws).unwrap();

        let (root, name, _) = find_dwm_workspace(&ws).unwrap();
        assert_eq!(root, repo);
        assert_eq!(name.as_str(), ".hidden-feature");
    }

    #[test]
    fn find_dwm_workspace_ignores_legacy_in_repo_layout() {
        // An in-repo `.dwm/worktrees/` is not a workspace location.
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let ws = repo.join(".dwm/worktrees/feature");
        fs::create_dir_all(&ws).unwrap();
        assert!(find_dwm_workspace(&ws).is_none());
    }

    #[test]
    fn dwm_dir_is_sibling_of_repo_root() {
        assert_eq!(
            dwm_dir(Path::new("/home/user/dev/ghg")),
            PathBuf::from("/home/user/dev/ghg.dwm")
        );
        assert_eq!(
            worktrees_dir(Path::new("/home/user/dev/ghg")),
            PathBuf::from("/home/user/dev/ghg.dwm/worktrees")
        );
    }

    #[test]
    fn dwm_dir_is_not_inside_repo_root() {
        // A jj workspace has no `.git` to stop git's upward search, so one
        // nested in the main checkout is treated as ignored paths of it.
        let root = Path::new("/home/user/dev/ghg");
        assert!(!dwm_dir(root).starts_with(root));
    }

    #[test]
    fn dwm_dir_keeps_dots_in_repo_name() {
        assert_eq!(
            dwm_dir(Path::new("/src/my.repo")),
            PathBuf::from("/src/my.repo.dwm")
        );
    }

    #[test]
    fn dwm_dir_falls_back_inside_filesystem_root() {
        assert_eq!(dwm_dir(Path::new("/")), PathBuf::from("/.dwm"));
    }

    #[test]
    fn repo_root_for_dwm_dir_inverts_dwm_dir() {
        let root = Path::new("/home/user/dev/my.repo");
        assert_eq!(repo_root_for_dwm_dir(&dwm_dir(root)).as_deref(), Some(root));
    }

    #[test]
    fn repo_root_for_dwm_dir_rejects_non_dwm_names() {
        assert_eq!(repo_root_for_dwm_dir(Path::new("/src/ghg")), None);
        // A bare `.dwm` (the legacy in-repo dir) has an empty repo name.
        assert_eq!(repo_root_for_dwm_dir(Path::new("/src/ghg/.dwm")), None);
    }
}
