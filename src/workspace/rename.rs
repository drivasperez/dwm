use anyhow::{Result, bail};
use owo_colors::OwoColorize;
use std::path::PathBuf;

use crate::names::WorkspaceName;
use crate::pool;
use crate::rex::{self, RexApi};
use crate::workspace::layout::{self, dwm_dir, workspace_path, worktrees_dir};
use crate::workspace::types::{WorkspaceDeps, resolve_repo_root};

/// Rename a workspace. When `new_name` is `None` the first argument is treated
/// as the new name and the old name is inferred from the current directory.
pub fn rename_workspace(name: String, new_name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = crate::vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
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
pub(crate) fn infer_workspace_name_from_cwd(deps: &WorkspaceDeps) -> Result<String> {
    match layout::find_dwm_workspace(&deps.cwd) {
        Some((_, ws, _)) => Ok(ws.to_string()),
        None => bail!("not inside a dwm workspace"),
    }
}

/// Returns the path the shell should cd to if cwd was inside the renamed workspace.
pub(crate) fn rename_workspace_inner(
    deps: &WorkspaceDeps,
    old_name: &str,
    new_name: &str,
) -> Result<Option<PathBuf>> {
    let root = resolve_repo_root(deps)?;
    let main_ws_name = deps.backend.main_workspace_name();
    if old_name == main_ws_name {
        bail!("cannot rename the main workspace '{}'", old_name);
    }

    let old_name = WorkspaceName::parse(old_name)?;
    let new_name = WorkspaceName::parse(new_name)?;
    let worktrees = worktrees_dir(&root);
    let old_path = workspace_path(&worktrees, &old_name);
    if !old_path.exists() {
        bail!(
            "workspace '{}' not found at {}",
            old_name,
            old_path.display()
        );
    }

    let new_path = workspace_path(&worktrees, &new_name);
    if new_path.exists() {
        bail!(
            "workspace '{}' already exists at {}",
            new_name,
            new_path.display()
        );
    }

    // Rex reports where processes are now, so look before the move.
    let panes = deps
        .rex
        .as_deref()
        .and_then(|r| r.panes().ok())
        .unwrap_or_default();
    let panes_in_old = rex::panes_inside(&panes, &old_path, None);

    eprintln!(
        "{} workspace '{}' -> '{}'...",
        "renaming".cyan(),
        old_name.bold(),
        new_name.bold()
    );
    deps.backend.workspace_rename(
        &root,
        &old_path,
        &new_path,
        old_name.as_str(),
        new_name.as_str(),
    )?;

    eprintln!(
        "{} workspace '{}' renamed to '{}'",
        "✓".green(),
        old_name.bold(),
        new_name.bold()
    );

    pool::rename_lease(&dwm_dir(&root), &old_name, &new_name)?;

    if let Some(rex) = deps.rex.as_deref() {
        follow_rename_in_rex(
            rex,
            &panes_in_old,
            old_name.as_str(),
            new_name.as_str(),
            &new_path,
        );
    }

    if layout::is_inside(&deps.cwd, &old_path) {
        let relative = deps.cwd.strip_prefix(&old_path)?;
        Ok(Some(new_path.join(relative)))
    } else {
        Ok(None)
    }
}

/// After a rename, relabel Rex sessions named after the old workspace and
/// warn about other panes that were in it. Their processes follow the moved
/// directory, but shells and agents keep the old path as text and will try to
/// use it.
fn follow_rename_in_rex(
    rex: &dyn RexApi,
    panes_in_old: &[&rex::Pane],
    old_name: &str,
    new_name: &str,
    new_path: &std::path::Path,
) {
    let mut relabelled: Vec<&str> = Vec::new();
    for pane in panes_in_old {
        if pane.session_label.as_deref() == Some(old_name)
            && !relabelled.contains(&pane.session_id.as_str())
        {
            match rex.rename_session(&pane.session_id, new_name) {
                Ok(()) => relabelled.push(&pane.session_id),
                Err(e) => eprintln!(
                    "{} could not rename Rex session '{}': {e:#}",
                    "warning:".yellow(),
                    old_name
                ),
            }
        }
    }
    if !relabelled.is_empty() {
        eprintln!(
            "{} renamed Rex session '{}' to '{}'",
            "✓".green(),
            old_name,
            new_name.bold()
        );
    }
    let others: Vec<String> = panes_in_old
        .iter()
        .filter(|p| Some(p.block_id.as_str()) != rex.current_block())
        .map(|p| {
            let mut p = (*p).clone();
            if relabelled.contains(&p.session_id.as_str()) {
                p.session_label = Some(new_name.to_string());
            }
            p.describe()
        })
        .collect();
    if !others.is_empty() {
        eprintln!(
            "{} still in the old directory: {}. Restart programs there or cd to {}",
            "note:".yellow(),
            others.join(", "),
            new_path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::tests::{FakeRex, pane};
    use crate::workspace::list::list_workspace_entries_inner;
    use crate::workspace::new::new_workspace_inner;
    use crate::workspace::test_utils::{
        MockBackend, MockCall, git_available, init_git_repo, init_jj_repo, jj_available,
        make_repo_with_vcs, rex_fixture, run_ok, with_data_dir, ws,
    };
    use crate::workspace::types::Placement;
    use std::fs;

    #[test]
    fn rename_workspace_success() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("old-name");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        let redirect = rename_workspace_inner(&deps, "old-name", "new-name").unwrap();
        assert!(
            redirect.is_none(),
            "should not redirect when cwd is outside workspace"
        );

        assert!(!ws_dir.exists());
        assert!(worktrees_dir(&main_repo).join("new-name").exists());

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Rename {
                old_name, new_name, ..
            } => {
                assert_eq!(old_name, "old-name");
                assert_eq!(new_name, "new-name");
            }
            other => panic!("expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn rename_workspace_redirects_when_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("old-name");
        fs::create_dir_all(ws_dir.join("src")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
            rex: None,
        };

        let redirect = rename_workspace_inner(&deps, "old-name", "new-name").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, worktrees_dir(&main_repo).join("new-name/src"));
    }

    #[test]
    fn rename_workspace_preserves_files() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("old-name");
        fs::create_dir_all(ws_dir.join("src")).unwrap();
        fs::write(ws_dir.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(ws_dir.join("README.md"), "# hello").unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        rename_workspace_inner(&deps, "old-name", "new-name").unwrap();

        let new_dir = worktrees_dir(&main_repo).join("new-name");
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
            rex: None,
        };

        let err = rename_workspace_inner(&deps, "nonexistent", "new-name").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_new_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(worktrees_dir(&main_repo).join("old-name")).unwrap();
        fs::create_dir_all(worktrees_dir(&main_repo).join("new-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
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
            rex: None,
        };

        let err = rename_workspace_inner(&deps, "default", "new-name").unwrap_err();
        assert!(err.to_string().contains("cannot rename"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_rejects_invalid_target() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(worktrees_dir(&main_repo).join("old-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let err = rename_workspace_inner(&deps, "old-name", "..").unwrap_err();
        assert!(
            err.to_string().contains("invalid workspace name"),
            "error: {}",
            err
        );
    }

    #[test]
    fn rename_workspace_allows_dot_prefixed_target() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(worktrees_dir(&main_repo).join("old-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        rename_workspace_inner(&deps, "old-name", ".hidden").unwrap();
        assert!(worktrees_dir(&main_repo).join(".hidden").exists());
    }

    #[test]
    fn rename_infers_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("old-name");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            rex: None,
        };

        let old = infer_workspace_name_from_cwd(&deps).unwrap();
        assert_eq!(old, "old-name");

        let redirect = rename_workspace_inner(&deps, &old, "new-name").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, worktrees_dir(&main_repo).join("new-name"));

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Rename { old_name, new_name } => {
                assert_eq!(old_name, "old-name");
                assert_eq!(new_name, "new-name");
            }
            other => panic!("expected Rename, got {:?}", other),
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
            rex: None,
        };

        let err = infer_workspace_name_from_cwd(&deps).unwrap_err();
        assert!(
            err.to_string().contains("not inside a dwm workspace"),
            "error: {}",
            err
        );
    }

    #[test]
    fn e2e_git_rename_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let backend = crate::git::GitBackend::default();
            let deps = WorkspaceDeps {
                backend: Box::new(backend),
                cwd: main_repo.clone(),
                rex: None,
            };

            new_workspace_inner(
                &deps,
                Some("old-name".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let old_path = worktrees_dir(&main_repo).join("old-name");
            assert!(old_path.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            rename_workspace_inner(&deps2, "old-name", "new-name").unwrap();

            assert!(!old_path.exists(), "old dir should be gone");
            assert!(worktrees_dir(&main_repo).join("new-name").exists());

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo,
                rex: None,
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
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };

            new_workspace_inner(
                &deps,
                Some("my-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_path = worktrees_dir(&main_repo).join("my-ws");
            let subdir = ws_path.join("src");
            fs::create_dir_all(&subdir).unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: subdir,
                rex: None,
            };
            let redirect = rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();
            let redirect = redirect.expect("should redirect when cwd is inside renamed workspace");
            assert_eq!(redirect, worktrees_dir(&main_repo).join("renamed-ws/src"));

            let new_ws = worktrees_dir(&main_repo).join("renamed-ws");
            assert!(new_ws.exists());
            assert!(new_ws.join("src").exists());
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
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("old-name".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let old_path = worktrees_dir(&main_repo).join("old-name");
            assert!(old_path.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            rename_workspace_inner(&deps2, "old-name", "new-name").unwrap();

            assert!(!old_path.exists());
            let new_path = worktrees_dir(&main_repo).join("new-name");
            assert!(new_path.exists());

            // jj's own record of the workspace's location must follow the move.
            let out = std::process::Command::new("jj")
                .args(["workspace", "root", "--name", "new-name"])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                PathBuf::from(String::from_utf8(out.stdout).unwrap().trim()),
                fs::canonicalize(&new_path).unwrap()
            );

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps3).unwrap();
            assert!(entries.iter().any(|e| e.name == "new-name"));
            assert!(!entries.iter().any(|e| e.name == "old-name"));
        });
    }

    #[test]
    fn e2e_jj_rename_refuses_stale_workspace_without_touching_edits() {
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
                Some("my-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();

            let worktrees = worktrees_dir(&main_repo);
            let old_path = worktrees.join("my-ws");
            run_ok(&old_path, "jj", &["status"]);

            // Rewrite the secondary workspace's working-copy commit from the
            // main workspace. Advancing the operation log alone isn't stale.
            fs::write(main_repo.join("upstream.txt"), "upstream\n").unwrap();
            run_ok(&main_repo, "jj", &["commit", "-m", "upstream change"]);
            run_ok(&main_repo, "jj", &["rebase", "-r", "my-ws@", "-d", "@-"]);
            fs::write(old_path.join("unsnapshotted.txt"), "keep me\n").unwrap();

            let err = rename_workspace_inner(&deps, "my-ws", "renamed-ws").unwrap_err();
            assert!(err.to_string().contains("working copy is stale"), "{err:#}");
            assert!(old_path.is_dir());
            assert!(!worktrees.join("renamed-ws").exists());
            assert!(!old_path.join("upstream.txt").exists());
            assert_eq!(
                fs::read_to_string(old_path.join("unsnapshotted.txt")).unwrap(),
                "keep me\n"
            );
            let listed = run_ok(
                &main_repo,
                "jj",
                &[
                    "--ignore-working-copy",
                    "workspace",
                    "list",
                    "-T",
                    "name ++ \"\\n\"",
                ],
            );
            assert!(listed.lines().any(|name| name == "my-ws"), "{listed}");
            assert!(!listed.lines().any(|name| name == "renamed-ws"), "{listed}");
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
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("my-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_path = worktrees_dir(&main_repo).join("my-ws");
            let subdir = ws_path.join("src");
            fs::create_dir_all(&subdir).unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: subdir,
                rex: None,
            };
            let redirect = rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();
            let redirect = redirect.expect("should redirect when cwd is inside renamed workspace");
            assert_eq!(redirect, worktrees_dir(&main_repo).join("renamed-ws/src"));

            let new_ws = worktrees_dir(&main_repo).join("renamed-ws");
            assert!(new_ws.exists());
            assert!(new_ws.join("src").exists());
        });
    }

    #[test]
    fn rename_relabels_the_workspace_session() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let mut renamed_session = pane("p1", "a", &a, &[]);
        renamed_session.session_id = "session:a".into();
        let mut second_pane = pane("p2", "a", &a, &[]);
        second_pane.session_id = "session:a".into();
        let other_session = pane("p3", "something-else", &a, &[]);
        let fake = FakeRex::new(vec![renamed_session, second_pane, other_session], true);
        let (_, deps, _) = rex_fixture(tmp.path(), &fake);

        rename_workspace_inner(&deps, "a", "c").unwrap();
        assert_eq!(fake.calls(), vec!["panes", "rename session:a c"]);
    }
}
