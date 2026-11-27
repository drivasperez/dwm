use anyhow::{Result, bail};
use owo_colors::OwoColorize;

use crate::vcs;
use crate::workspace::delete::{self, leased_verdict, open_panes_verdict};
use crate::workspace::layout::{self, dwm_dir, workspace_path, worktrees_dir};
use crate::workspace::types::{PruneMode, PruneOutcome, WorkspaceDeps, resolve_repo_root};

/// Delete (or, in [`PruneMode::DryRun`], list) workspaces whose work is safe to
/// discard. Prints a redirect path to stdout if cwd was inside a deleted one.
pub fn prune_workspaces(mode: PruneMode) -> Result<()> {
    let report =
        (mode == PruneMode::Delete).then(|| crate::rex::StatusReport::start("Pruning workspaces"));
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let outcome = prune_workspaces_inner(&deps, mode)?;
    if let Some(report) = report {
        report.done(&match outcome.deleted.len() {
            1 => "Pruned 1 workspace".to_string(),
            n => format!("Pruned {n} workspaces"),
        });
    }
    if let Some(redirect) = outcome.redirect {
        println!("{}", redirect.display());
    }
    Ok(())
}

pub(crate) fn prune_workspaces_inner(
    deps: &WorkspaceDeps,
    mode: PruneMode,
) -> Result<PruneOutcome> {
    let root = resolve_repo_root(deps)?;
    let worktrees = worktrees_dir(&root);
    let main_ws_name = deps.backend.main_workspace_name();

    let mut names = Vec::new();
    if worktrees.exists() {
        for entry in std::fs::read_dir(&worktrees)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let Some(name) = layout::workspace_entry_name(&entry) else {
                continue;
            };
            // The main workspace lives at the repo root, never under
            // worktrees/, but guard against a directory that shares its name.
            if name.as_str() != main_ws_name {
                names.push(name);
            }
        }
    }
    names.sort();

    let mut outcome = PruneOutcome::default();
    if names.is_empty() {
        eprintln!("no workspaces to prune");
        return Ok(outcome);
    }

    // Panes don't move between workspaces in the time prune takes, so one
    // look covers every workspace.
    let panes = deps.rex.as_deref().and_then(|r| r.panes().ok());
    let current_block = deps.rex.as_deref().and_then(|r| r.current_block());

    let mut failures = 0;
    for name in names {
        let ws_path = workspace_path(&worktrees, &name);
        // Check immediately before deleting so the verdict is as fresh as
        // possible when acting on it.
        let verdict = leased_verdict(&dwm_dir(&root), &name, panes.as_deref())
            .or_else(|| {
                panes
                    .as_deref()
                    .and_then(|p| open_panes_verdict(p, &ws_path, current_block))
            })
            .unwrap_or_else(|| deps.backend.prune_verdict(&root, &ws_path, name.as_str()));
        match &verdict {
            vcs::PruneVerdict::Keep(reason) => {
                eprintln!("{} {}: {}", "keep ".yellow(), name.bold(), reason);
            }
            vcs::PruneVerdict::Prunable => match mode {
                PruneMode::DryRun => eprintln!("{} {}", "prune".green(), name.bold()),
                PruneMode::Delete => {
                    match delete::delete_named_workspace(
                        deps,
                        &root,
                        &name,
                        false,
                        super::OpenPanes::Refuse,
                    ) {
                        Ok(redirect) => {
                            eprintln!("{} {} deleted", "✓".green(), name.bold());
                            outcome.redirect = outcome.redirect.or(redirect);
                            outcome.deleted.push(name.to_string());
                        }
                        Err(e) => {
                            failures += 1;
                            eprintln!("{} {}: delete failed: {e:#}", "error".red(), name.bold());
                        }
                    }
                }
            },
        }
        outcome.verdicts.push((name.to_string(), verdict));
    }

    let prunable = outcome
        .verdicts
        .iter()
        .filter(|(_, v)| *v == vcs::PruneVerdict::Prunable)
        .count();
    let kept = outcome.verdicts.len() - prunable;
    match mode {
        PruneMode::DryRun if prunable > 0 => eprintln!(
            "\n{prunable} prunable, {kept} kept (dry run; run `dwm prune --yes` to delete)"
        ),
        PruneMode::DryRun => eprintln!("\nnothing to prune, {kept} kept"),
        PruneMode::Delete => eprintln!("\n{} deleted, {kept} kept", outcome.deleted.len()),
    }
    if failures > 0 {
        bail!("failed to delete {failures} prunable workspace(s)");
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool;
    use crate::rex::ProgramState::Blocked;
    use crate::rex::tests::{FakeRex, pane};
    use crate::workspace::test_utils::{
        MockBackend, MockCall, git_available, jj_available, make_repo_with_vcs, pool_fixture,
        rex_fixture, run_ok, ws, ws_name,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    fn keep(reason: &str) -> vcs::PruneVerdict {
        vcs::PruneVerdict::Keep(reason.to_string())
    }

    /// Repo with `<repo>.dwm/worktrees/{a,b}`, where `a` is prunable and `b` not.
    fn prune_fixture(tmp: &Path) -> (PathBuf, MockBackend, Arc<Mutex<Vec<MockCall>>>) {
        let main_repo = make_repo_with_vcs(tmp);
        for ws in ["a", "b"] {
            fs::create_dir_all(worktrees_dir(&main_repo).join(ws)).unwrap();
        }
        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let mock =
            mock.with_prune_verdicts(&[("a", vcs::PruneVerdict::Prunable), ("b", keep("dirty"))]);
        (main_repo, mock, calls)
    }

    fn verdict_of<'a>(outcome: &'a PruneOutcome, name: &str) -> &'a vcs::PruneVerdict {
        &outcome
            .verdicts
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no verdict for {name}: {:?}", outcome.verdicts))
            .1
    }

    fn assert_keep_contains(outcome: &PruneOutcome, name: &str, needle: &str) {
        match verdict_of(outcome, name) {
            vcs::PruneVerdict::Keep(reason) => {
                assert!(
                    reason.contains(needle),
                    "{name}: {reason:?} lacks {needle:?}"
                )
            }
            other => panic!("{name}: expected Keep({needle}), got {other:?}"),
        }
    }

    #[test]
    fn prune_dry_run_reports_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, mock, calls) = prune_fixture(tmp.path());
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::DryRun).unwrap();
        assert_eq!(
            outcome.verdicts,
            vec![
                ("a".to_string(), vcs::PruneVerdict::Prunable),
                ("b".to_string(), keep("dirty")),
            ]
        );
        assert!(outcome.deleted.is_empty());
        assert!(outcome.redirect.is_none());
        assert!(calls.lock().unwrap().is_empty());
        assert!(worktrees_dir(&main_repo).join("a").exists());
        assert!(worktrees_dir(&main_repo).join("b").exists());
    }

    #[test]
    fn prune_delete_removes_only_prunable_via_delete_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, mock, calls) = prune_fixture(tmp.path());
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(outcome.deleted, vec!["a".to_string()]);
        assert!(!worktrees_dir(&main_repo).join("a").exists());
        assert!(worktrees_dir(&main_repo).join("b").exists());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0], MockCall::Remove { name, .. } if name == "a"));
    }

    #[test]
    fn prune_never_touches_main_workspace_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let dir = worktrees_dir(&main_repo).join("default");
        fs::create_dir_all(&dir).unwrap();
        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let mock = mock.with_prune_verdicts(&[("default", vcs::PruneVerdict::Prunable)]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert!(outcome.verdicts.is_empty());
        assert!(calls.lock().unwrap().is_empty());
        assert!(dir.exists());
    }

    #[test]
    fn prune_redirects_when_cwd_inside_deleted_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, mock, _calls) = prune_fixture(tmp.path());
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: worktrees_dir(&main_repo).join("a"),
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(outcome.redirect, Some(main_repo));
    }

    #[test]
    fn prune_without_worktrees_dir_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert!(outcome.verdicts.is_empty());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn e2e_jj_prune() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        run_ok(
            tmp.path(),
            "git",
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        let repo_path = tmp.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        run_ok(&repo_path, "jj", &["git", "init"]);
        let repo = repo_path.canonicalize().unwrap();
        run_ok(
            &repo,
            "jj",
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
        );
        fs::write(repo.join("base.txt"), "base").unwrap();
        run_ok(&repo, "jj", &["commit", "-m", "base"]);
        run_ok(&repo, "jj", &["bookmark", "create", "main", "-r", "@-"]);
        run_ok(&repo, "jj", &["git", "push", "--bookmark", "main"]);

        let worktrees = worktrees_dir(&repo);
        fs::create_dir_all(&worktrees).unwrap();
        let add = |name: &str| {
            let path = worktrees.join(name);
            run_ok(
                &repo,
                "jj",
                &[
                    "workspace",
                    "add",
                    "--name",
                    name,
                    "-r",
                    "main",
                    path.to_str().unwrap(),
                ],
            );
            path
        };

        // Fresh workspace: nothing to lose.
        add("clean");
        // On-disk edit that no jj command has snapshotted yet.
        let dirty = add("dirty");
        fs::write(dirty.join("wip.txt"), "wip").unwrap();
        // A described commit that only exists locally.
        let local = add("local");
        fs::write(local.join("local.txt"), "local").unwrap();
        run_ok(&local, "jj", &["commit", "-m", "local work"]);
        // The same, but pushed to the remote.
        let pushed = add("pushed");
        fs::write(pushed.join("pushed.txt"), "pushed").unwrap();
        run_ok(&pushed, "jj", &["commit", "-m", "pushed work"]);
        run_ok(&pushed, "jj", &["bookmark", "create", "feat", "-r", "@-"]);
        run_ok(&pushed, "jj", &["git", "push", "--bookmark", "feat"]);
        // Stale: its working-copy commit is rewritten from another workspace,
        // and it has an unsnapshotted edit on disk.
        let stale = add("stale");
        run_ok(&stale, "jj", &["status"]);
        run_ok(&repo, "jj", &["new", "main"]);
        fs::write(repo.join("other.txt"), "x").unwrap();
        run_ok(&repo, "jj", &["commit", "-m", "trunk moves"]);
        run_ok(&repo, "jj", &["rebase", "-r", "stale@", "-d", "@-"]);
        fs::write(stale.join("unsnapshotted.txt"), "precious").unwrap();

        let deps = WorkspaceDeps {
            backend: Box::new(crate::jj::JjBackend),
            cwd: repo.clone(),
            rex: None,
        };
        let dry = prune_workspaces_inner(&deps, PruneMode::DryRun).unwrap();
        assert_eq!(verdict_of(&dry, "clean"), &vcs::PruneVerdict::Prunable);
        assert_eq!(verdict_of(&dry, "pushed"), &vcs::PruneVerdict::Prunable);
        assert_keep_contains(&dry, "dirty", "uncommitted changes");
        assert_keep_contains(&dry, "local", "not on any remote bookmark");
        assert_keep_contains(&dry, "stale", "stale");
        assert!(worktrees.join("clean").exists(), "dry run must not delete");

        let done = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(
            done.deleted,
            vec!["clean".to_string(), "pushed".to_string()]
        );
        for gone in ["clean", "pushed"] {
            assert!(!worktrees.join(gone).exists(), "{gone} should be deleted");
        }
        for kept in ["dirty", "local", "stale"] {
            assert!(worktrees.join(kept).exists(), "{kept} should be kept");
        }
        // prune must never run `jj workspace update-stale`, which could
        // clobber this file.
        assert_eq!(
            fs::read_to_string(stale.join("unsnapshotted.txt")).unwrap(),
            "precious"
        );
        let listed = run_ok(&repo, "jj", &["workspace", "list", "-T", "name ++ \"\\n\""]);
        assert!(
            !listed.lines().any(|l| l == "clean" || l == "pushed"),
            "{listed}"
        );
        assert!(listed.lines().any(|l| l == "default"), "{listed}");
    }

    #[test]
    fn prune_keeps_workspaces_open_in_rex() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(
            vec![pane("p1", "a", &a, &[Blocked]), pane("p2", "a", &a, &[])],
            true,
        );
        let (_, deps, calls) = rex_fixture(tmp.path(), &fake);

        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(outcome.deleted, vec!["b"]);
        let (_, verdict) = &outcome.verdicts[0];
        match verdict {
            vcs::PruneVerdict::Keep(reason) => {
                assert_eq!(reason, "open in Rex session 'a' (zsh) and 1 more")
            }
            other => panic!("expected Keep, got {other:?}"),
        }
        // Only `b` reached the backend's delete.
        assert_eq!(calls.lock().unwrap().len(), 1);
        // One look decides every verdict; deleting `b` looks again just before
        // removing it.
        assert_eq!(fake.calls(), vec!["panes", "panes"]);
    }

    #[test]
    fn prune_keeps_leased_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, deps, _) = pool_fixture(
            tmp.path(),
            None,
            &[
                ("agents-1", vcs::PruneVerdict::Prunable),
                ("agents-2", vcs::PruneVerdict::Prunable),
            ],
        );
        pool::write_lease(
            &dwm_dir(&main_repo),
            &pool::Lease::new(&ws_name("agents-1"), None),
        )
        .unwrap();

        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(outcome.deleted, vec!["agents-2"]);
        assert!(
            matches!(&outcome.verdicts[0].1, vcs::PruneVerdict::Keep(r) if r.starts_with("leased")),
            "{:?}",
            outcome.verdicts
        );
    }

    #[test]
    fn e2e_git_prune() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        run_ok(
            tmp.path(),
            "git",
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        let repo_path = tmp.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        run_ok(&repo_path, "git", &["init", "-q", "-b", "main"]);
        let repo = repo_path.canonicalize().unwrap();
        run_ok(
            &repo,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "base"],
        );
        run_ok(
            &repo,
            "git",
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run_ok(&repo, "git", &["push", "-q", "-u", "origin", "main"]);
        let worktrees = worktrees_dir(&repo);
        fs::create_dir_all(&worktrees).unwrap();
        let add = |name: &str| {
            let path = worktrees.join(name);
            run_ok(
                &repo,
                "git",
                &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
            );
            path
        };

        add("clean");
        let dirty = add("dirty");
        fs::write(dirty.join("untracked.txt"), "wip").unwrap();
        let ignored = add("ignored");
        fs::write(ignored.join(".gitignore"), "target/\n").unwrap();
        run_ok(&ignored, "git", &["add", ".gitignore"]);
        run_ok(&ignored, "git", &["commit", "-q", "-m", "ignore target"]);
        run_ok(&ignored, "git", &["push", "-q", "-u", "origin", "ignored"]);
        fs::create_dir_all(ignored.join("target")).unwrap();
        fs::write(ignored.join("target/build.o"), "artifact").unwrap();
        let local = add("local");
        run_ok(
            &local,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "local work"],
        );
        let ahead = add("ahead");
        run_ok(
            &ahead,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "pushed"],
        );
        run_ok(&ahead, "git", &["push", "-q", "-u", "origin", "ahead"]);
        run_ok(
            &ahead,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "not yet pushed"],
        );

        let deps = WorkspaceDeps {
            backend: Box::new(crate::git::GitBackend::default()),
            cwd: repo.clone(),
            rex: None,
        };
        let dry = prune_workspaces_inner(&deps, PruneMode::DryRun).unwrap();
        assert_eq!(verdict_of(&dry, "clean"), &vcs::PruneVerdict::Prunable);
        assert_eq!(verdict_of(&dry, "ignored"), &vcs::PruneVerdict::Prunable);
        assert_keep_contains(&dry, "dirty", "uncommitted change");
        assert_keep_contains(&dry, "local", "\"local work\"");
        assert_keep_contains(&dry, "ahead", "1 commit not pushed");
        assert!(!dry.verdicts.iter().any(|(n, _)| n == "main-worktree"));

        let done = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(
            done.deleted,
            vec!["clean".to_string(), "ignored".to_string()]
        );
        for kept in ["dirty", "local", "ahead"] {
            assert!(worktrees.join(kept).exists(), "{kept} should be kept");
        }
        let listed = run_ok(&repo, "git", &["worktree", "list", "--porcelain"]);
        assert!(!listed.contains("/clean\n"), "{listed}");
    }
}
