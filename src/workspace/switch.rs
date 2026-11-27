use anyhow::Result;
use std::path::PathBuf;

use crate::names::WorkspaceName;
use crate::workspace::layout::{workspace_path, worktrees_dir};
use crate::workspace::rex_ops::go_to_workspace;
use crate::workspace::types::{Placement, WorkspaceDeps, resolve_repo_root};

/// Switch to the named workspace by printing its path to stdout for the shell
/// wrapper to `cd` into.
pub fn switch_workspace(name: &str, placement: Placement) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = crate::vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let path = switch_workspace_inner(&deps, name)?;
    if let Some(dir) = go_to_workspace(deps.rex.as_deref(), &deps.cwd, &path, placement)? {
        println!("{}", dir.display());
    }
    Ok(())
}

/// Resolve the path for the named workspace. Returns the path the shell should
/// `cd` into.
pub(crate) fn switch_workspace_inner(deps: &WorkspaceDeps, name: &str) -> Result<PathBuf> {
    let root = resolve_repo_root(deps)?;
    let main_ws_name = deps.backend.main_workspace_name();
    if name == main_ws_name {
        return Ok(root);
    }

    let name = WorkspaceName::parse(name)?;
    let ws_path = workspace_path(&worktrees_dir(&root), &name);
    if !ws_path.exists() {
        anyhow::bail!("workspace '{}' not found at {}", name, ws_path.display());
    }

    Ok(ws_path)
}

/// Go to a workspace chosen in the picker by its path. Prints the path for
/// the shell wrapper when it should `cd` there.
pub fn switch_to_path(path: &std::path::Path, placement: Placement) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let rex = crate::rex::connect();
    if let Some(dir) = go_to_workspace(rex.as_deref(), &cwd, path, placement)? {
        println!("{}", dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::new::new_workspace_inner;
    use crate::workspace::test_utils::{
        MockBackend, git_available, init_git_repo, init_jj_repo, jj_available, make_repo_with_vcs,
        with_data_dir,
    };
    use std::fs;

    #[test]
    fn switch_workspace_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("feat-x");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
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
            rex: None,
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
            rex: None,
        };

        let err = switch_workspace_inner(&deps, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
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
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("switch-target".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("switch-target");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            let path = switch_workspace_inner(&deps2, "switch-target").unwrap();
            assert_eq!(path, ws_dir);

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
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
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("switch-target".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("switch-target");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let path = switch_workspace_inner(&deps2, "switch-target").unwrap();
            assert_eq!(path, ws_dir);

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let path = switch_workspace_inner(&deps3, "default").unwrap();
            assert_eq!(path, main_repo);
        });
    }
}
