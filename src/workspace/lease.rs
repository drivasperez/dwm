use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::fs;
use std::path::PathBuf;

use crate::names::WorkspaceName;
use crate::rex::{self};
use crate::workspace::layout::{
    self, dwm_dir, workspace_entry_name, workspace_path, worktrees_dir,
};
use crate::workspace::types::{Placement, WorkspaceDeps, resolve_repo_root};
use crate::{pool, vcs};

/// Lease a workspace from `pool_name` and print its path to stdout.
pub fn lease_workspace(pool_name: &str, at: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let path = lease_workspace_inner(&deps, pool_name, at)?;
    println!("{}", path.display());
    Ok(())
}

/// Testable core of [`lease_workspace`]. Reuses the lowest-numbered member of
/// the pool that is not leased, not open in a Rex pane, and has nothing to
/// lose, moving it to a fresh change on `at` (trunk by default). When none
/// qualifies, creates the next member. Returns the leased workspace's path.
pub(crate) fn lease_workspace_inner(
    deps: &WorkspaceDeps,
    pool_name: &str,
    at: Option<&str>,
) -> Result<PathBuf> {
    let pool_name = WorkspaceName::parse(pool_name)?;
    let root = resolve_repo_root(deps)?;
    let dwm = dwm_dir(&root);
    let worktrees = worktrees_dir(&root);
    fs::create_dir_all(&worktrees)?;
    let _lock = pool::lock(&dwm)?;

    let rex = deps.rex.as_deref();
    let panes = rex.and_then(|r| r.panes().ok());
    let holder = rex.and_then(|r| r.current_block());
    let revision = match at {
        Some(rev) => rev.to_string(),
        None => deps.backend.trunk_revision(&root),
    };

    let mut names = Vec::new();
    for entry in fs::read_dir(&worktrees)? {
        let entry = entry?;
        if entry.path().is_dir()
            && let Some(name) = workspace_entry_name(&entry)
        {
            names.push(name);
        }
    }
    let members = pool::members(&pool_name, names);

    for (_, name) in &members {
        let ws_path = workspace_path(&worktrees, name);
        let busy = if let Some(lease) = pool::read_lease(&dwm, name)
            && pool::lease_state(&lease, panes.as_deref()) == pool::LeaseState::Held
        {
            Some("leased".to_string())
        } else if let Some(p) = panes.as_deref()
            && let Some(pane) = rex::panes_inside(p, &ws_path, None).first()
        {
            Some(format!("open in Rex {}", pane.describe()))
        } else {
            match deps.backend.prune_verdict(&root, &ws_path, name.as_str()) {
                vcs::PruneVerdict::Prunable => None,
                vcs::PruneVerdict::Keep(reason) => Some(reason),
            }
        };
        if let Some(reason) = busy {
            eprintln!("{} {}: {}", "skip ".dimmed(), name, reason.dimmed());
            continue;
        }
        deps.backend
            .reset_workspace(&root, &ws_path, name.as_str(), &revision)
            .with_context(|| format!("resetting '{name}' to {revision}"))?;
        pool::write_lease(&dwm, &pool::Lease::new(name, holder))?;
        eprintln!("{} leased '{}' at {}", "✓".green(), name.bold(), revision);
        return Ok(ws_path);
    }

    let name = pool::next_member_name(&pool_name, &members)?;
    let created = super::new::create_workspace(
        deps,
        Some(name),
        Some(&revision),
        None,
        Placement::Here,
        &[],
    )?;
    pool::write_lease(&dwm, &pool::Lease::new(&created.name, holder))?;
    eprintln!(
        "{} leased new workspace '{}'",
        "✓".green(),
        created.name.bold()
    );
    Ok(workspace_path(&worktrees, &created.name))
}

/// End the lease on a pool workspace, named or inferred from the current
/// directory, so the next `dwm lease` can reuse it.
pub fn release_workspace(name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: None,
    };
    release_workspace_inner(&deps, name)
}

pub(crate) fn release_workspace_inner(deps: &WorkspaceDeps, name: Option<String>) -> Result<()> {
    let (root, name) = match name {
        Some(n) => (resolve_repo_root(deps)?, WorkspaceName::try_from(n)?),
        None => {
            let (root, ws, _) = layout::find_dwm_workspace(&deps.cwd)
                .context("not inside a dwm workspace; provide a workspace name")?;
            (root, ws)
        }
    };
    // The name becomes a file path under leases/.
    let dwm = dwm_dir(&root);
    let _lock = pool::lock(&dwm)?;
    if pool::remove_lease(&dwm, &name)? {
        eprintln!("{} released '{}'", "✓".green(), name.bold());
    } else {
        eprintln!("'{}' was not leased", name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::tests::{FakeRex, pane};
    use crate::workspace::layout::{dwm_dir, worktrees_dir};
    use crate::workspace::test_utils::{
        MockCall, git_available, make_repo_with_vcs, pool_fixture, run_ok, with_data_dir, ws,
        ws_name,
    };
    use std::path::Path;

    fn lease(tmp: &Path, deps: &WorkspaceDeps, at: Option<&str>) -> PathBuf {
        with_data_dir(tmp, || lease_workspace_inner(deps, "agents", at).unwrap())
    }

    #[test]
    fn e2e_git_lease_reuses_and_resets_members() {
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
        let deps = WorkspaceDeps {
            backend: Box::new(crate::git::GitBackend::default()),
            cwd: repo.clone(),
            rex: None,
        };
        let lease = || {
            with_data_dir(tmp.path(), || {
                lease_workspace_inner(&deps, "agents", None).unwrap()
            })
        };

        let first = lease();
        assert_eq!(first, worktrees_dir(&repo).join("agents-1"));
        // Work that reached the remote is safe to discard on reuse, even on a
        // branch of the agent's own.
        run_ok(&first, "git", &["checkout", "-q", "-b", "fix-123"]);
        run_ok(
            &first,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "pushed work"],
        );
        run_ok(&first, "git", &["push", "-q", "-u", "origin", "fix-123"]);
        let fix_before = run_ok(&repo, "git", &["rev-parse", "fix-123"]);
        run_ok(
            &repo,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "trunk moves"],
        );
        release_workspace_inner(&deps, Some("agents-1".into())).unwrap();

        assert_eq!(lease(), first, "the released member is reused");
        let head = run_ok(&first, "git", &["log", "-1", "--format=%s"]);
        assert_eq!(head.trim(), "trunk moves", "reset to the current trunk");
        let branch = run_ok(&first, "git", &["branch", "--show-current"]);
        assert_eq!(branch.trim(), "agents-1", "back on its own branch");
        // Regression: the reset used to move whatever branch was checked out.
        let fix_after = run_ok(&repo, "git", &["rev-parse", "fix-123"]);
        assert_eq!(fix_before, fix_after, "the agent's branch is untouched");

        // An uncommitted change makes a released member unusable.
        fs::write(first.join("wip.txt"), "wip").unwrap();
        release_workspace_inner(&deps, Some("agents-1".into())).unwrap();
        assert_eq!(lease(), worktrees_dir(&repo).join("agents-2"));
        assert!(
            first.join("wip.txt").exists(),
            "the dirty member was left alone"
        );
    }

    #[test]
    fn lease_creates_the_first_member_from_trunk() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = FakeRex::new(vec![], true).in_block("block:me");
        let (main_repo, deps, calls) = pool_fixture(tmp.path(), Some(&fake), &[]);

        let path = lease(tmp.path(), &deps, None);
        assert_eq!(path, ws(&main_repo, "agents-1"));
        match &calls.lock().unwrap()[..] {
            [MockCall::Add { name, at, .. }] => {
                assert_eq!(name, "agents-1");
                assert_eq!(at.as_deref(), Some("trunk()"));
            }
            other => panic!("expected one Add, got {other:?}"),
        }
        let lease = pool::read_lease(&dwm_dir(&main_repo), &ws_name("agents-1")).unwrap();
        assert_eq!(lease.rex_block.as_deref(), Some("block:me"));
    }

    #[test]
    fn lease_reuses_a_free_member_and_resets_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, deps, calls) = pool_fixture(
            tmp.path(),
            None,
            &[
                ("agents-1", vcs::PruneVerdict::Prunable),
                ("agents-2", vcs::PruneVerdict::Prunable),
            ],
        );

        let path = lease(tmp.path(), &deps, Some("main"));
        assert_eq!(path, ws(&main_repo, "agents-1"));
        match &calls.lock().unwrap()[..] {
            [MockCall::Reset { name, revision }] => {
                assert_eq!((name.as_str(), revision.as_str()), ("agents-1", "main"))
            }
            other => panic!("expected one Reset, got {other:?}"),
        }
        assert!(pool::read_lease(&dwm_dir(&main_repo), &ws_name("agents-1")).is_some());

        // The next lease skips the member now on loan.
        let path = lease(tmp.path(), &deps, None);
        assert_eq!(path, ws(&main_repo, "agents-2"));
    }

    #[test]
    fn lease_skips_busy_members_and_creates_the_next() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let open = ws(&main_repo, "agents-2");
        fs::create_dir_all(&open).unwrap();
        let fake =
            FakeRex::new(vec![pane("block:other", "x", &open, &[])], true).in_block("block:me");
        let (main_repo, deps, calls) = pool_fixture(
            tmp.path(),
            Some(&fake),
            &[
                ("agents-1", vcs::PruneVerdict::Prunable), // leased below
                ("agents-2", vcs::PruneVerdict::Prunable), // open in Rex
                (
                    "agents-3",
                    vcs::PruneVerdict::Keep("1 unpushed commit".into()),
                ),
                ("other-9", vcs::PruneVerdict::Prunable), // not in the pool
            ],
        );
        pool::write_lease(
            &dwm_dir(&main_repo),
            &pool::Lease::new(&ws_name("agents-1"), Some("block:other")),
        )
        .unwrap();

        let path = lease(tmp.path(), &deps, None);
        assert_eq!(path, ws(&main_repo, "agents-4"));
        assert!(
            matches!(&calls.lock().unwrap()[..], [MockCall::Add { name, .. }] if name == "agents-4"),
            "nothing reset"
        );
    }

    #[test]
    fn lease_reclaims_a_member_whose_rex_pane_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = FakeRex::new(vec![], true).in_block("block:me");
        let (main_repo, deps, _) = pool_fixture(
            tmp.path(),
            Some(&fake),
            &[("agents-1", vcs::PruneVerdict::Prunable)],
        );
        let dwm = dwm_dir(&main_repo);
        pool::write_lease(
            &dwm,
            &pool::Lease::new(&ws_name("agents-1"), Some("block:gone")),
        )
        .unwrap();

        assert_eq!(lease(tmp.path(), &deps, None), ws(&main_repo, "agents-1"));
        let lease = pool::read_lease(&dwm, &ws_name("agents-1")).unwrap();
        assert_eq!(
            lease.rex_block.as_deref(),
            Some("block:me"),
            "re-leased to us"
        );
    }

    #[test]
    fn lease_rejects_bad_pool_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, deps, _) = pool_fixture(tmp.path(), None, &[]);
        assert!(lease_workspace_inner(&deps, "a/b", None).is_err());
        assert!(lease_workspace_inner(&deps, "..", None).is_err());
    }

    #[test]
    fn release_ends_a_lease_by_name_or_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, mut deps, _) = pool_fixture(
            tmp.path(),
            None,
            &[("agents-1", vcs::PruneVerdict::Prunable)],
        );
        let dwm = dwm_dir(&main_repo);
        pool::write_lease(&dwm, &pool::Lease::new(&ws_name("agents-1"), None)).unwrap();

        deps.cwd = ws(&main_repo, "agents-1");
        release_workspace_inner(&deps, None).unwrap();
        assert!(pool::read_lease(&dwm, &ws_name("agents-1")).is_none());
        // Releasing again is harmless.
        release_workspace_inner(&deps, Some("agents-1".into())).unwrap();
    }

    #[test]
    fn release_rejects_names_that_escape_the_leases_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, deps, _) = pool_fixture(tmp.path(), None, &[]);
        let victim = dwm_dir(&main_repo).join("victim.json");
        fs::create_dir_all(dwm_dir(&main_repo)).unwrap();
        fs::write(&victim, "{}").unwrap();
        for bad in ["../victim", "a/b", ".."] {
            assert!(
                release_workspace_inner(&deps, Some(bad.into())).is_err(),
                "{bad}"
            );
        }
        assert!(victim.exists());
    }
}
