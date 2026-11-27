use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

use crate::names::WorkspaceName;
use crate::workspace::layout::{dwm_dir, workspace_entry_name, worktrees_dir};
use crate::workspace::registry::read_and_prune_registry;
use crate::workspace::types::{
    ComputedEntry, EntryCandidate, MergeStatus, WorkspaceDeps, WorkspaceEntry, compute_is_stale,
    resolve_repo_root, stale_by_mtime,
};
use crate::{agent, pool, rex, vcs};

/// Collect [`WorkspaceEntry`] values for all workspaces belonging to the
/// repository that contains the current directory.
pub fn list_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    Ok(list_workspace_entries_and_dwm_dir()?.0)
}

/// Like [`list_workspace_entries`], but also returns the repo's `<repo>.dwm/`
/// directory.
///
/// Callers that need both (e.g. `dwm list`, which polls agent status out of
/// `<repo>.dwm/`) should use this rather than resolving the repo root separately:
/// the root is already known from the listing, so looking it up again costs a
/// redundant `jj root` / `git rev-parse` subprocess.
pub fn list_workspace_entries_and_dwm_dir() -> Result<(Vec<WorkspaceEntry>, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let entries = list_workspace_entries_inner(&deps)?;
    let dwm = match entries.first() {
        Some(entry) => dwm_dir(&entry.main_repo_path),
        None => dwm_dir(&resolve_repo_root(&deps)?),
    };
    Ok((entries, dwm))
}

/// Testable core of [`list_workspace_entries`].
pub(crate) fn list_workspace_entries_inner(deps: &WorkspaceDeps) -> Result<Vec<WorkspaceEntry>> {
    let panes = deps.rex.as_deref().and_then(|r| r.panes().ok());
    list_entries_with_panes(deps, panes.as_deref())
}

/// Like [`list_workspace_entries_inner`], with the Rex panes already listed
/// (`None` outside Rex), so listing many repos asks Rex only once.
pub(crate) fn list_entries_with_panes(
    deps: &WorkspaceDeps,
    panes: Option<&[rex::Pane]>,
) -> Result<Vec<WorkspaceEntry>> {
    let main_repo = resolve_repo_root(deps)?;
    let dwm = dwm_dir(&main_repo);

    let rex_summaries = panes.map(agent::summaries_from_panes);
    let mut agent_summaries = agent::read_agent_summaries_with(&dwm, rex_summaries.as_ref());

    let main_ws_name = deps.backend.main_workspace_name();
    let vcs_workspaces = deps.backend.workspace_list(&main_repo).unwrap_or_default();
    let vcs_type = deps.backend.vcs_type();

    // Collect candidates in display order: main first, then each subdir of
    // <repo>.dwm/worktrees/ (in fs::read_dir order, matching previous behaviour).
    let mut candidates: Vec<EntryCandidate> = Vec::new();

    let main_info = vcs_workspaces
        .iter()
        .find(|(n, _)| n == main_ws_name)
        .map(|(_, info)| info.clone());
    candidates.push(EntryCandidate {
        name: WorkspaceName::parse(main_ws_name)?,
        path: main_repo.clone(),
        is_main: true,
        has_info: main_info.is_some(),
        info: main_info.unwrap_or_default(),
    });

    let worktrees = worktrees_dir(&main_repo);
    if worktrees.exists() {
        // Scan workspace dirs under <repo>.dwm/worktrees/. Internal dwm state
        // lives in sibling dirs (e.g. <repo>.dwm/agent-status/) so there's no need to
        // filter entries here — every dir under worktrees/ is a workspace.
        for entry in fs::read_dir(&worktrees)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = workspace_entry_name(&entry) else {
                continue;
            };
            let info = vcs_workspaces
                .iter()
                .find(|(n, _)| n == name.as_str())
                .map(|(_, info)| info.clone());
            candidates.push(EntryCandidate {
                name,
                path,
                is_main: false,
                has_info: info.is_some(),
                info: info.unwrap_or_default(),
            });
        }
    }

    // Fan out the per-workspace VCS calls. Each candidate runs up to 3 cold
    // subprocesses (diff_stat_vs_trunk, optional latest_description,
    // is_merged_into_trunk); these are independent across workspaces and dwarf
    // the cost of spawning an OS thread per workspace.
    let backend: &dyn vcs::VcsBackend = &*deps.backend;
    let main_repo_ref: &Path = &main_repo;
    let computed = std::thread::scope(|s| {
        let handles: Vec<_> = candidates
            .iter()
            .map(|c| {
                s.spawn(move || {
                    let modified = fs::metadata(&c.path).and_then(|m| m.modified()).ok();

                    let diff_stat = if c.is_main || c.has_info {
                        backend
                            .diff_stat_vs_trunk(main_repo_ref, &c.path, c.name.as_str())
                            .unwrap_or_default()
                    } else {
                        vcs::DiffStat::default()
                    };

                    let description = if c.info.description.trim().is_empty() {
                        backend.latest_description(main_repo_ref, &c.path, c.name.as_str())
                    } else {
                        c.info.description.clone()
                    };

                    let is_stale = if c.is_main {
                        false
                    } else if stale_by_mtime(modified) {
                        // Already stale by inactivity — skip the merged probe,
                        // which is a VCS subprocess we don't need.
                        true
                    } else {
                        let merge_status = if c.has_info
                            && backend.is_merged_into_trunk(main_repo_ref, &c.path, c.name.as_str())
                        {
                            MergeStatus::Merged
                        } else {
                            MergeStatus::Unmerged
                        };
                        compute_is_stale(merge_status, modified)
                    };

                    ComputedEntry {
                        diff_stat,
                        description,
                        modified,
                        is_stale,
                    }
                })
            })
            .collect();
        // Join every worker before propagating errors. Otherwise, scope
        // would rethrow panics from any workers left unjoined.
        candidates
            .iter()
            .zip(handles)
            .map(|(candidate, handle)| {
                handle.join().map_err(|_| {
                    anyhow::anyhow!(
                        "could not collect workspace info for '{}': worker panicked",
                        candidate.name
                    )
                })
            })
            .collect::<Vec<_>>()
    });
    let computed = computed.into_iter().collect::<Result<Vec<_>>>()?;

    let mut entries = Vec::with_capacity(candidates.len());
    for (c, comp) in candidates.into_iter().zip(computed) {
        let agent_status = agent_summaries.remove(c.name.as_str());
        let lease = pool::read_lease(&dwm, &c.name)
            .filter(|l| pool::lease_state(l, panes) == pool::LeaseState::Held);
        let rex_pane = panes.and_then(|p| {
            rex::pick_pane(&rex::panes_inside(p, &c.path, None)).map(rex::PaneRef::from)
        });
        entries.push(WorkspaceEntry {
            name: c.name.to_string(),
            path: c.path,
            last_modified: comp.modified,
            diff_stat: comp.diff_stat,
            is_main: c.is_main,
            change_id: c.info.change_id,
            description: comp.description,
            bookmarks: c.info.bookmarks,
            is_stale: comp.is_stale,
            repo_name: None,
            main_repo_path: main_repo.clone(),
            vcs_type,
            agent_status,
            rex_pane,
            lease,
        });
    }

    Ok(entries)
}

/// Collect [`WorkspaceEntry`] values for every workspace across all repos
/// tracked in the registry.
pub fn list_all_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    let repos = read_and_prune_registry()?;
    let mut all_entries = Vec::new();
    let panes = rex::connect().and_then(|r| r.panes().ok());

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
            rex: None,
        };

        match list_entries_with_panes(&deps, panes.as_deref()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::ProgramState::{Blocked, Working};
    use crate::rex::tests::{FakeRex, pane};
    use crate::workspace::new::new_workspace_inner;
    use crate::workspace::test_utils::{
        MockBackend, git_available, init_git_repo, init_jj_repo, jj_available, make_repo_with_vcs,
        rex_fixture, with_data_dir, ws,
    };
    use crate::workspace::types::{Placement, WorkspaceDeps};

    #[test]
    fn list_entries_returns_an_error_when_workers_panic() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(worktrees_dir(&main_repo).join("feature"))?;
        let (mut mock, _) = MockBackend::new(
            main_repo.clone(),
            vec![("feature".to_string(), vcs::WorkspaceInfo::default())],
        );
        mock.panic_on_diff_stat = true;
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let result = list_workspace_entries_inner(&deps);
        assert!(matches!(
            result,
            Err(err) if err.to_string().contains("'default': worker panicked")
        ));
        Ok(())
    }

    #[test]
    fn list_entries_from_inside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("feat-x");
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
            rex: None,
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
    fn list_entries_only_scans_worktrees_subdir() {
        // Sibling dirs under <repo>.dwm/ (e.g. agent-status/) must not show up as
        // workspaces. With the worktrees/ namespace this falls out for free
        // because list iteration only reads <repo>.dwm/worktrees/.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let dwm = dwm_dir(&main_repo);
        let ws_dir = dwm.join("worktrees/feat-x");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(dwm.join("agent-status")).unwrap();

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
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            !names.contains(&"agent-status"),
            "sibling internal dirs should not appear, got: {:?}",
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
            rex: None,
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
        // No <repo>.dwm/ — should still return the main entry only.

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1, "should have just the main entry");
        assert!(entries[0].is_main);
    }

    #[test]
    fn e2e_git_list_entries_main_only() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        let backend = crate::git::GitBackend::default();
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            rex: None,
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

        // Create a git worktree at <repo>.dwm/worktrees/feat-branch
        let ws_path = worktrees_dir(&main_repo).join("feat-branch");
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

        let backend = crate::git::GitBackend::default();
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            rex: None,
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
    fn e2e_git_worktree_with_changes() {
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
                Some("feature".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("feature");

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
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo,
                rex: None,
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
            rex: None,
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

        let ws_path = worktrees_dir(&main_repo).join("feat-ws");
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
            rex: None,
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
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("feature".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("feature");

            fs::write(ws_dir.join("hello.txt"), "hello world\n").unwrap();
            let ws_str = ws_dir.to_str().unwrap();
            std::process::Command::new("jj")
                .args(["--repository", ws_str, "describe", "-m", "add hello"])
                .output()
                .unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
                rex: None,
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

    /// Regression: the workspace diff was taken from `trunk()` itself, so a
    /// workspace that had simply not been rebased reported every change that
    /// landed on trunk since it branched (inverted) on top of its own.
    #[test]
    fn e2e_jj_diff_stat_ignores_trunk_advancing() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let main_str = main_repo.to_str().unwrap().to_string();

        let jj = |dir: &str, args: &[&str]| {
            let mut full = vec!["--repository", dir];
            full.extend_from_slice(args);
            let out = std::process::Command::new("jj")
                .args(&full)
                .output()
                .unwrap();
            assert!(out.status.success(), "jj {:?} failed: {:?}", args, out);
        };

        // Without a remote, the default `trunk()` resolves to the root commit,
        // which hides the difference this test is about. Point it at the local
        // `main` bookmark instead.
        jj(
            &main_str,
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.\"trunk()\"",
                "main",
            ],
        );

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("feature".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("feature");
            let ws_str = ws_dir.to_str().unwrap().to_string();

            // One line of work in the workspace.
            fs::write(ws_dir.join("hello.txt"), "hello\n").unwrap();
            jj(&ws_str, &["describe", "-m", "add hello"]);

            // Trunk moves on afterwards, touching files the workspace never saw.
            fs::write(main_repo.join("trunk-a.txt"), "one\ntwo\nthree\n").unwrap();
            fs::write(main_repo.join("trunk-b.txt"), "four\nfive\n").unwrap();
            jj(&main_str, &["describe", "-m", "trunk work"]);
            jj(&main_str, &["bookmark", "set", "main", "-r", "@"]);

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            let feat = entries.iter().find(|e| e.name == "feature").unwrap();
            assert_eq!(
                (
                    feat.diff_stat.files_changed,
                    feat.diff_stat.insertions,
                    feat.diff_stat.deletions
                ),
                (1, 1, 0),
                "workspace should report only its own change, got {:?}",
                feat.diff_stat
            );
        });
    }

    #[test]
    fn list_reports_rex_panes_and_their_agent_status() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(
            vec![
                pane("p1", "a", &a, &[Blocked]),
                pane("p2", "a", &a, &[Working]),
                pane("p3", "repo", &main_repo, &[]),
            ],
            true,
        );
        let (_, deps, _) = rex_fixture(tmp.path(), &fake);

        let entries = list_workspace_entries_inner(&deps).unwrap();
        let a_entry = entries.iter().find(|e| e.name == "a").unwrap();
        let agents = a_entry.agent_status.as_ref().unwrap();
        assert_eq!((agents.waiting, agents.working), (1, 1));
        assert_eq!(a_entry.rex_pane.as_ref().unwrap().block_id, "p1");
        let main = entries.iter().find(|e| e.is_main).unwrap();
        assert_eq!(main.rex_pane.as_ref().unwrap().block_id, "p3");
        assert_eq!(main.agent_status, None, "a plain shell is not an agent");
        let b_entry = entries.iter().find(|e| e.name == "b").unwrap();
        assert!(b_entry.rex_pane.is_none());
    }
}
