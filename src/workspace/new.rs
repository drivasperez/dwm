use anyhow::{Context, Result, bail};
use owo_colors::OwoColorize;
use std::fs;

use crate::names::WorkspaceName;
use crate::workspace::layout::{workspace_path, worktrees_dir};
use crate::workspace::registry;
use crate::workspace::rex_ops::open_new_workspace;
use crate::workspace::types::{NewOutcome, Placement, WorkspaceDeps, resolve_repo_root};
use crate::{hooks, names, vcs};

/// Create a new workspace, auto-detecting the VCS from the current directory.
///
/// Prints the new workspace path to stdout so the shell wrapper can `cd` into it.
pub fn new_workspace(
    name: Option<String>,
    at: Option<&str>,
    from: Option<&str>,
    placement: Placement,
    command: &[String],
) -> Result<()> {
    let report = crate::rex::StatusReport::start(&match &name {
        Some(n) => format!("Creating workspace {n}"),
        None => "Creating a workspace".to_string(),
    });
    let cwd = std::env::current_dir()?;
    // If we're inside a dwm workspace the cwd may be a workspace dir; we still
    // want to detect from the underlying VCS, which works because both jj and
    // git treat the workspace dir as part of the same repo.
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let created = new_workspace_inner(&deps, name, at, from, placement, command)?;
    report.done(&format!("Created workspace {}", created.name));
    if let Some(dir) = created.cd {
        // stdout: path for shell wrapper to cd into
        println!("{}", dir.display());
    }
    Ok(())
}

/// Testable core of [`new_workspace`] that accepts injected [`WorkspaceDeps`].
///
/// `command` is typed into a new Rex session opened in the workspace, so it
/// needs Rex and [`Placement::Rex`].
pub(crate) fn new_workspace_inner(
    deps: &WorkspaceDeps,
    name: Option<String>,
    at: Option<&str>,
    from: Option<&str>,
    placement: Placement,
    command: &[String],
) -> Result<NewOutcome> {
    if !command.is_empty() && (deps.rex.is_none() || placement == Placement::Here) {
        bail!(
            "running a command in the new workspace needs a Rex session; run dwm inside Rex without --here"
        );
    }
    let name = name.map(WorkspaceName::try_from).transpose()?;
    let from = from.map(WorkspaceName::parse).transpose()?;
    create_workspace(deps, name, at, from.as_ref(), placement, command)
}

pub(crate) fn create_workspace(
    deps: &WorkspaceDeps,
    name: Option<WorkspaceName>,
    at: Option<&str>,
    from: Option<&WorkspaceName>,
    placement: Placement,
    command: &[String],
) -> Result<NewOutcome> {
    let root = resolve_repo_root(deps)?;
    let worktrees = worktrees_dir(&root);
    fs::create_dir_all(&worktrees)?;

    // Best-effort: register the repo for `--all`.
    let _ = registry::register_repo(&root);

    // Resolve --from to a change ID by looking up the source workspace.
    let resolved_at;
    let at = if let Some(ws_name) = from {
        let workspaces = deps.backend.workspace_list(&root)?;
        let (_name, info) = workspaces
            .iter()
            .find(|(n, _)| n == ws_name.as_str())
            .with_context(|| format!("workspace '{}' not found", ws_name))?;
        resolved_at = deps
            .backend
            .workspace_start_revision(&root, ws_name.as_str(), info)?;
        Some(resolved_at.as_str())
    } else {
        at
    };

    let ws_name = match name {
        Some(name) => name,
        None => WorkspaceName::try_from(names::generate_unique(&worktrees))?,
    };

    let ws_path = workspace_path(&worktrees, &ws_name);
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
    let config = crate::config::load(&root)?;
    if config.workspace.checkout == crate::config::Checkout::Cow
        && deps.backend.vcs_type() == vcs::VcsType::Jj
    {
        bail!(
            "workspace.checkout = 'cow' is experimental and currently supports Git only; jj supports workspace.copy"
        );
    }
    let source = match from {
        Some(name) if name.as_str() != deps.backend.main_workspace_name() => {
            workspace_path(&worktrees, name)
        }
        _ => root.clone(),
    };
    let resolved = at
        .map(|rev| deps.backend.resolve_revision(&root, rev))
        .transpose()?;
    let seed = crate::seed::SeedPlan::prepare(&source, deps.backend.vcs_type(), &config.workspace)?;

    eprintln!("{} workspace '{}'...", "creating".cyan(), ws_name.bold());
    deps.backend.workspace_add_configured(
        &root,
        &ws_path,
        ws_name.as_str(),
        resolved.as_deref(),
        &source,
        &config.workspace,
    )?;
    if let Err(e) = seed.apply(&ws_path, deps.backend.vcs_type(), &config.workspace) {
        // The workspace is seconds old and holds nothing but what was just
        // copied, so take it away again rather than leave a half-made one that
        // blocks retrying under the same name.
        let cleanup = deps
            .backend
            .discard_new_workspace(&root, ws_name.as_str(), &ws_path)
            .and_then(|()| {
                if ws_path.exists() {
                    fs::remove_dir_all(&ws_path)?;
                }
                Ok(())
            });
        return Err(match cleanup {
            Ok(()) => e.context(format!(
                "copying files into workspace '{ws_name}' failed, so it was removed again; setup was not run"
            )),
            Err(c) => e.context(format!(
                "copying files into {} failed, and removing the partial workspace also failed ({c:#}); it is still there",
                ws_path.display()
            )),
        });
    }
    eprintln!(
        "{} workspace '{}' created at {}",
        "✓".green(),
        ws_name.bold(),
        ws_path.display().dimmed()
    );

    let hook_ctx = hooks::HookContext {
        workspace_path: ws_path.clone(),
        workspace_name: ws_name.to_string(),
        repo_root: root.clone(),
        vcs_type: deps.backend.vcs_type(),
        from_workspace: from.map(|s| s.to_string()),
    };
    hooks::run_setup(&config.scripts, &hook_ctx)?;

    let cd = open_new_workspace(
        deps.rex.as_deref(),
        ws_name.as_str(),
        &ws_path,
        placement,
        command,
    )?;
    Ok(NewOutcome { name: ws_name, cd })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::tests::FakeRex;
    use crate::workspace::registry::read_and_prune_registry;
    use crate::workspace::test_utils::{
        MockBackend, MockCall, make_repo_with_vcs, rex_fixture, with_data_dir, ws,
    };

    #[test]
    fn new_workspace_calls_add() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("my-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Add {
                repo_dir,
                ws_path,
                name,
                at,
            } => {
                assert_eq!(repo_dir, &main_repo);
                assert_eq!(ws_path, &worktrees_dir(&main_repo).join("my-ws"));
                assert_eq!(name, "my-ws");
                assert!(at.is_none());
            }
            other => panic!("expected Add, got {:?}", other),
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
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, None, None, None, Placement::Rex, &[]).unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Add { name, .. } => {
                // Auto-generated name should be non-empty and contain a hyphen (adjective-noun)
                assert!(!name.is_empty());
                assert!(
                    name.contains('-'),
                    "auto name should be adjective-noun: {}",
                    name
                );
            }
            other => panic!("expected Add, got {:?}", other),
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
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("dup-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let err = new_workspace_inner(
                &deps,
                Some("dup-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap_err();
            assert!(err.to_string().contains("already exists"), "error: {}", err);
        });
    }

    #[test]
    fn new_workspace_allows_dot_prefixed_names() {
        // Pre-refactor `.foo` was rejected to avoid collision with `.agent-status`.
        // With workspaces under `<repo>.dwm/worktrees/`, there's no collision risk and
        // names starting with `.` are allowed.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some(".hidden".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
        });
        assert!(worktrees_dir(&main_repo).join(".hidden").exists());
    }

    #[test]
    fn new_workspace_rejects_invalid_names() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            for bad in [".", "..", "foo/bar", "with\\backslash", ""] {
                let err = new_workspace_inner(
                    &deps,
                    Some(bad.to_string()),
                    None,
                    None,
                    Placement::Rex,
                    &[],
                )
                .unwrap_err()
                .to_string();
                assert!(
                    err.contains("invalid workspace name"),
                    "expected rejection for {:?}, got: {}",
                    bad,
                    err
                );
            }
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
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("forked".to_string()),
                None,
                Some("source-ws"),
                Placement::Rex,
                &[],
            )
            .unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Add {
                ws_path, name, at, ..
            } => {
                assert_eq!(ws_path, &worktrees_dir(&main_repo).join("forked"));
                assert_eq!(name, "forked");
                assert_eq!(at.as_deref(), Some("abc12345"));
            }
            other => panic!("expected Add, got {:?}", other),
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
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            let err = new_workspace_inner(
                &deps,
                Some("forked".to_string()),
                None,
                Some("no-such-ws"),
                Placement::Rex,
                &[],
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("not found"),
                "error should mention not found: {}",
                err
            );
        });
    }

    #[test]
    fn new_workspace_creates_workspace_outside_repo_without_touching_ignores() {
        // The workspace lives in a sibling dir, so there is nothing to ignore
        // inside the repo and dwm must not edit any ignore file.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
        });
        assert!(tmp.path().join("myrepo.dwm/worktrees/ws").is_dir());
        assert!(!main_repo.join(".gitignore").exists());
        assert!(!main_repo.join(".dwm").exists());
    }

    #[test]
    fn new_workspace_registers_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.contains(&fs::canonicalize(&main_repo).unwrap()));
        });
    }

    #[test]
    fn e2e_failed_copy_removes_the_new_workspace() {
        // Regression: when copying failed after the worktree was created, dwm
        // left it behind, and retrying `dwm new` with the name then failed.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir(&root).unwrap();
        let run = |args: &[&str]| {
            let mut command = std::process::Command::new("git");
            command.current_dir(&root).args(args);
            String::from_utf8(crate::seed::output(command).unwrap()).unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.name", "dwm"]);
        run(&["config", "user.email", "dwm@example.com"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        // `cache/` is ignored here, but not at the commit the workspace gets.
        fs::write(root.join(".gitignore"), "cache/\n").unwrap();
        fs::create_dir(root.join("cache")).unwrap();
        fs::write(root.join("cache/value"), "x").unwrap();
        fs::write(
            root.join(".dwm.toml"),
            "[workspace]\ncopy = ['cache']\ncopy_unignored = 'error'\n",
        )
        .unwrap();
        let deps = WorkspaceDeps {
            backend: vcs::VcsType::Git.to_backend(),
            cwd: root.clone(),
            rex: None,
        };
        let new = |name: &str| {
            with_data_dir(tmp.path(), || {
                new_workspace_inner(&deps, Some(name.into()), None, None, Placement::Here, &[])
            })
        };

        let err = new("one").unwrap_err();
        assert!(format!("{err:#}").contains("removed again"), "{err:#}");
        let ws = worktrees_dir(&root).join("one");
        assert!(!ws.exists(), "the half-made workspace is gone");
        let listed = run(&["worktree", "list", "--porcelain"]);
        assert!(!listed.contains("/one\n"), "{listed}");
        let branches = run(&["branch", "--list", "one"]);
        assert!(branches.trim().is_empty(), "branch left behind: {branches}");

        // With the problem fixed, the same name works.
        fs::write(
            root.join(".dwm.toml"),
            "[workspace]\ncopy = ['cache']\ncopy_unignored = 'skip'\n",
        )
        .unwrap();
        new("one").unwrap();
        assert!(ws.exists());
    }

    #[test]
    fn e2e_configured_copy_runs_before_setup_and_from_uses_source() {
        for vcs in [vcs::VcsType::Git, vcs::VcsType::Jj] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("repo");
            fs::create_dir(&root).unwrap();
            let run = |program: &str, args: &[&str]| {
                let mut command = std::process::Command::new(program);
                command.current_dir(&root).args(args);
                crate::seed::output(command).unwrap()
            };
            if vcs == vcs::VcsType::Git {
                run("git", &["init", "-q", "-b", "main"]);
                run("git", &["config", "user.name", "dwm"]);
                run("git", &["config", "user.email", "dwm@example.com"]);
                run("git", &["config", "commit.gpgsign", "false"]);
            } else {
                run("jj", &["git", "init"]);
            }
            fs::write(root.join(".gitignore"), "cache/\nmarker\n").unwrap();
            fs::write(
                root.join(".dwm.toml"),
                "[workspace]\ncopy = ['cache']\n[scripts]\nsetup = 'cat cache/value > marker'\n",
            )
            .unwrap();
            if vcs == vcs::VcsType::Git {
                run("git", &["add", "."]);
                run("git", &["commit", "-qm", "fixture"]);
            } else {
                run("jj", &["describe", "-m", "fixture"]);
            }
            fs::create_dir(root.join("cache")).unwrap();
            fs::write(root.join("cache/value"), "main").unwrap();
            let deps = WorkspaceDeps {
                backend: vcs.to_backend(),
                cwd: root.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("one".into()),
                if vcs == vcs::VcsType::Jj {
                    Some("@")
                } else {
                    None
                },
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let one = worktrees_dir(&root).join("one");
            assert_eq!(fs::read_to_string(one.join("marker")).unwrap(), "main");
            fs::write(one.join("cache/value"), "source").unwrap();
            new_workspace_inner(
                &deps,
                Some("two".into()),
                None,
                Some("one"),
                Placement::Rex,
                &[],
            )
            .unwrap();
            assert_eq!(
                fs::read_to_string(worktrees_dir(&root).join("two/marker")).unwrap(),
                "source"
            );
            assert_eq!(
                fs::read_to_string(root.join("cache/value")).unwrap(),
                "main"
            );

            // Invalid configuration fails before creating VCS metadata or running setup.
            fs::write(
                root.join(".dwm.toml"),
                "[workspace]\ncopy = ['../escape']\n",
            )
            .unwrap();
            assert!(
                new_workspace_inner(
                    &deps,
                    Some("invalid".into()),
                    None,
                    None,
                    Placement::Rex,
                    &[]
                )
                .is_err()
            );
            assert!(!worktrees_dir(&root).join("invalid").exists());
            if vcs == vcs::VcsType::Jj {
                fs::write(root.join(".dwm.toml"), "[workspace]\ncheckout = 'cow'\n").unwrap();
                assert!(
                    new_workspace_inner(
                        &deps,
                        Some("unsupported".into()),
                        None,
                        None,
                        Placement::Rex,
                        &[]
                    )
                    .is_err()
                );
                assert!(!worktrees_dir(&root).join("unsupported").exists());
            }
        }
    }

    #[test]
    fn new_opens_a_session_instead_of_changing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = FakeRex::new(vec![], true).in_block("me");
        let (main_repo, deps, _calls) = rex_fixture(tmp.path(), &fake);

        let created = with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, Some("c".into()), None, None, Placement::Rex, &[]).unwrap()
        });
        assert_eq!(created.name.as_str(), "c");
        assert_eq!(created.cd, None);
        let opened = fake.opened.borrow();
        assert_eq!(opened[0].label, "c");
        assert_eq!(opened[0].cwd, ws(&main_repo, "c"));
    }

    #[test]
    fn new_with_command_types_it_into_the_session_even_when_unseen() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = FakeRex::new(vec![], false).in_block("me");
        let (main_repo, deps, _calls) = rex_fixture(tmp.path(), &fake);
        let command = vec!["claude".to_string(), "fix it".to_string()];

        let created = with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("c".into()),
                None,
                None,
                Placement::Rex,
                &command,
            )
            .unwrap()
        });
        let opened = fake.opened.borrow();
        assert_eq!(
            opened[0].initial_input.as_deref(),
            Some("claude 'fix it'\r")
        );
        assert_eq!(opened[0].create, crate::rex::CreateWhen::Always);
        // The session runs unseen, so the shell still goes to the workspace.
        assert_eq!(created.cd, Some(ws(&main_repo, "c")));
    }

    #[test]
    fn new_with_command_outside_rex_fails_before_creating_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };
        let err = new_workspace_inner(
            &deps,
            Some("c".into()),
            None,
            None,
            Placement::Rex,
            &["claude".to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("needs a Rex session"), "{err}");
        assert!(calls.lock().unwrap().is_empty());
        assert!(!ws(&main_repo, "c").exists());
    }
}
