use anyhow::{Context, Result, bail};
use owo_colors::OwoColorize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::names::WorkspaceName;
use crate::rex::{self, RexApi};
use crate::workspace::layout::{self, dwm_dir, workspace_path, worktrees_dir};
use crate::workspace::types::{DeleteOutput, OpenPanes, WorkspaceDeps, resolve_repo_root};
use crate::{agent, pool, vcs};

/// Deletes a workspace. Returns `true` if the cwd was inside the deleted
/// workspace and a redirect path was printed to stdout.
pub fn delete_workspace(
    name: Option<String>,
    output: DeleteOutput,
    open_panes: OpenPanes,
) -> Result<bool> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    if let Some(redirect) = delete_workspace_inner(&deps, name, output, open_panes)? {
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
    open_panes: OpenPanes,
) -> Result<Option<PathBuf>> {
    let verbose = output == DeleteOutput::Verbose;

    let (root, ws_name) = match name {
        Some(n) => {
            let root = resolve_repo_root(deps)?;
            (root, WorkspaceName::try_from(n)?)
        }
        None => {
            let (root, ws, _) = layout::find_dwm_workspace(&deps.cwd)
                .context("not inside a dwm workspace; provide a workspace name")?;
            (root, ws)
        }
    };

    delete_named_workspace(deps, &root, &ws_name, verbose, open_panes)
}

pub(crate) fn delete_named_workspace(
    deps: &WorkspaceDeps,
    root: &Path,
    ws_name: &WorkspaceName,
    verbose: bool,
    open_panes: OpenPanes,
) -> Result<Option<PathBuf>> {
    let ws_path = workspace_path(&worktrees_dir(root), ws_name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", ws_name, ws_path.display());
    }

    if let Some(rex) = deps.rex.as_deref() {
        deal_with_open_panes(rex, ws_name.as_str(), &ws_path, open_panes)?;
    }

    if verbose {
        eprintln!(
            "{} workspace '{}'...",
            "forgetting".yellow(),
            ws_name.bold()
        );
    }
    deps.backend
        .workspace_remove(root, ws_name.as_str(), &ws_path)?;

    if ws_path.exists() {
        if verbose {
            eprintln!("{} {}...", "removing".red(), ws_path.display().dimmed());
        }
        fs::remove_dir_all(&ws_path)?;
    }

    // Clean up agent status files for this workspace
    agent::remove_agent_statuses_for_workspace(&dwm_dir(root), ws_name.as_str());
    let _ = pool::remove_lease(&dwm_dir(root), ws_name);

    if verbose {
        eprintln!("{} workspace '{}' deleted", "✓".green(), ws_name.bold());
    }

    if layout::is_inside(&deps.cwd, &ws_path) {
        Ok(Some(root.to_path_buf()))
    } else {
        Ok(None)
    }
}

/// Before deleting a workspace, find Rex panes other than dwm's own that are
/// still in it, and refuse, ask, or close them as `policy` says. Deleting the
/// directory under a running agent would leave it working in a void.
fn deal_with_open_panes(
    rex: &dyn RexApi,
    ws_name: &str,
    ws_path: &Path,
    policy: OpenPanes,
) -> Result<()> {
    let panes = match rex.panes() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{} could not check Rex for panes in '{}': {e:#}",
                "warning:".yellow(),
                ws_name
            );
            return Ok(());
        }
    };
    let inside = rex::panes_inside(&panes, ws_path, rex.current_block());
    if inside.is_empty() {
        return Ok(());
    }
    let list = inside
        .iter()
        .map(|p| p.describe())
        .collect::<Vec<_>>()
        .join(", ");
    let close = match policy {
        OpenPanes::Close => true,
        OpenPanes::Refuse => false,
        OpenPanes::Ask => {
            eprintln!(
                "{} workspace '{}' is open in Rex: {}",
                "!".yellow().bold(),
                ws_name.bold(),
                list
            );
            eprint!(
                "  {} Close {} and delete the workspace? [y/N] ",
                "?".bold().cyan(),
                if inside.len() == 1 { "it" } else { "them" }
            );
            agent::confirm_from_tty()?
        }
    };
    if !close {
        bail!(
            "workspace '{}' is open in Rex ({}); close {} first",
            ws_name,
            list,
            if inside.len() == 1 { "it" } else { "them" }
        );
    }
    for pane in inside {
        rex.close_pane(pane)
            .with_context(|| format!("closing Rex {}", pane.describe()))?;
    }
    Ok(())
}

/// Why prune must keep a workspace that Rex panes are open in, if any are.
pub(crate) fn open_panes_verdict(
    panes: &[rex::Pane],
    ws_path: &Path,
    current_block: Option<&str>,
) -> Option<vcs::PruneVerdict> {
    let inside = rex::panes_inside(panes, ws_path, current_block);
    let first = inside.first()?;
    let more = match inside.len() {
        1 => String::new(),
        n => format!(" and {} more", n - 1),
    };
    Some(vcs::PruneVerdict::Keep(format!(
        "open in Rex {}{more}",
        first.describe()
    )))
}

/// Why prune must keep a leased workspace, if the lease still holds.
pub(crate) fn leased_verdict(
    dwm: &Path,
    ws_name: &WorkspaceName,
    panes: Option<&[rex::Pane]>,
) -> Option<vcs::PruneVerdict> {
    let lease = pool::read_lease(dwm, ws_name)?;
    (pool::lease_state(&lease, panes) == pool::LeaseState::Held)
        .then(|| vcs::PruneVerdict::Keep(format!("leased; `dwm release {ws_name}` ends the lease")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::ProgramState::Working;
    use crate::rex::tests::{FakeRex, pane};
    use crate::workspace::list::list_workspace_entries_inner;
    use crate::workspace::new::new_workspace_inner;
    use crate::workspace::rename::rename_workspace_inner;
    use crate::workspace::switch::switch_workspace_inner;
    use crate::workspace::test_utils::{
        MockBackend, MockCall, git_available, init_git_repo, init_jj_repo, jj_available,
        make_repo_with_vcs, pool_fixture, rex_fixture, with_data_dir, ws, ws_name,
    };
    use crate::workspace::types::Placement;

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
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("my cool feature".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("my cool feature");
            assert!(ws_dir.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "my cool feature"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let switch_path = switch_workspace_inner(&deps3, "my cool feature").unwrap();
            assert_eq!(switch_path, ws_dir);

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            delete_workspace_inner(
                &deps4,
                Some("my cool feature".to_string()),
                DeleteOutput::Verbose,
                OpenPanes::Refuse,
            )
            .unwrap();
            assert!(!ws_dir.exists());
        });
    }

    #[test]
    fn delete_workspace_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("my-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        // cwd is outside the workspace being deleted
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        let redirect = delete_workspace_inner(
            &deps,
            Some("my-ws".to_string()),
            DeleteOutput::Verbose,
            OpenPanes::Refuse,
        )
        .unwrap();
        assert!(
            redirect.is_none(),
            "should not redirect when cwd is outside workspace"
        );

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Remove {
                repo_dir,
                name,
                ws_path,
            } => {
                assert_eq!(repo_dir, &main_repo);
                assert_eq!(name, "my-ws");
                assert_eq!(ws_path, &ws_dir);
            }
            other => panic!("expected Remove, got {:?}", other),
        }
        assert!(!ws_dir.exists());
    }

    #[test]
    fn delete_workspace_redirects_when_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("my-ws");
        fs::create_dir_all(ws_dir.join("src")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
            rex: None,
        };

        let redirect = delete_workspace_inner(
            &deps,
            Some("my-ws".to_string()),
            DeleteOutput::Verbose,
            OpenPanes::Refuse,
        )
        .unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, main_repo);
    }

    #[test]
    fn delete_workspace_infers_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("inferred-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            rex: None,
        };

        let _redirected =
            delete_workspace_inner(&deps, None, DeleteOutput::Verbose, OpenPanes::Refuse).unwrap();

        let calls = calls.lock().unwrap();
        match &calls[0] {
            MockCall::Remove { name, .. } => {
                assert_eq!(name, "inferred-ws");
            }
            other => panic!("expected Remove, got {:?}", other),
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
            rex: None,
        };

        let err = delete_workspace_inner(
            &deps,
            Some("nonexistent".to_string()),
            DeleteOutput::Verbose,
            OpenPanes::Refuse,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    #[test]
    fn workspace_commands_reject_non_names() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(worktrees_dir(&main_repo)).unwrap();
        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let errors = [
            switch_workspace_inner(&deps, ".").unwrap_err(),
            delete_workspace_inner(
                &deps,
                Some(".".into()),
                DeleteOutput::Quiet,
                OpenPanes::Refuse,
            )
            .unwrap_err(),
            rename_workspace_inner(&deps, ".", "new-name").unwrap_err(),
        ];
        for error in errors {
            assert!(
                error.to_string().contains("invalid workspace name"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn e2e_git_new_and_delete_workspace() {
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
                Some("test-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("test-ws");
            assert!(ws_dir.exists(), "workspace dir should exist after creation");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "test-ws"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            delete_workspace_inner(
                &deps3,
                Some("test-ws".to_string()),
                DeleteOutput::Verbose,
                OpenPanes::Refuse,
            )
            .unwrap();
            assert!(!ws_dir.exists());

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps4).unwrap();
            assert!(!entries.iter().any(|e| e.name == "test-ws"));
        });
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
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("test-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("test-ws");
            assert!(ws_dir.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "test-ws"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            delete_workspace_inner(
                &deps3,
                Some("test-ws".to_string()),
                DeleteOutput::Verbose,
                OpenPanes::Refuse,
            )
            .unwrap();
            assert!(!ws_dir.exists());

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps4).unwrap();
            assert!(!entries.iter().any(|e| e.name == "test-ws"));
        });
    }

    #[test]
    fn delete_refuses_while_a_pane_is_open_in_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(vec![pane("agent", "a-session", &a, &[Working])], true);
        let (_, deps, calls) = rex_fixture(tmp.path(), &fake);

        let err = delete_workspace_inner(
            &deps,
            Some("a".into()),
            DeleteOutput::Quiet,
            OpenPanes::Refuse,
        )
        .unwrap_err();
        assert!(err.to_string().contains("open in Rex"), "{err}");
        assert!(err.to_string().contains("a-session"), "{err}");
        assert!(calls.lock().unwrap().is_empty(), "nothing removed");
        assert!(a.exists());
    }

    #[test]
    fn delete_can_close_the_panes_first() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(vec![pane("agent", "a", &a, &[Working])], true);
        let (_, deps, calls) = rex_fixture(tmp.path(), &fake);

        delete_workspace_inner(
            &deps,
            Some("a".into()),
            DeleteOutput::Quiet,
            OpenPanes::Close,
        )
        .unwrap();
        assert_eq!(fake.calls(), vec!["panes", "close agent"]);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(!a.exists());
    }

    #[test]
    fn delete_ignores_the_pane_dwm_runs_in() {
        // That pane is sent back to the repo root by the shell wrapper.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(vec![pane("me", "a", &a, &[])], true).in_block("me");
        let (_, mut deps, _) = rex_fixture(tmp.path(), &fake);
        deps.cwd = a.clone();

        let redirect =
            delete_workspace_inner(&deps, None, DeleteOutput::Quiet, OpenPanes::Refuse).unwrap();
        assert_eq!(redirect, Some(main_repo));
    }

    #[test]
    fn delete_and_rename_carry_the_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, deps, _) = pool_fixture(
            tmp.path(),
            None,
            &[
                ("agents-1", vcs::PruneVerdict::Prunable),
                ("agents-2", vcs::PruneVerdict::Prunable),
            ],
        );
        let dwm = dwm_dir(&main_repo);
        pool::write_lease(&dwm, &pool::Lease::new(&ws_name("agents-1"), None)).unwrap();
        pool::write_lease(&dwm, &pool::Lease::new(&ws_name("agents-2"), None)).unwrap();

        rename_workspace_inner(&deps, "agents-1", "agents-7").unwrap();
        assert!(pool::read_lease(&dwm, &ws_name("agents-1")).is_none());
        assert!(pool::read_lease(&dwm, &ws_name("agents-7")).is_some());

        delete_workspace_inner(
            &deps,
            Some("agents-2".into()),
            DeleteOutput::Quiet,
            OpenPanes::Refuse,
        )
        .unwrap();
        assert!(pool::read_lease(&dwm, &ws_name("agents-2")).is_none());
    }
}
