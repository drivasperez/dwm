use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::vcs::{self, DiffStat, PruneVerdict, VcsBackend, WorkspaceInfo};

/// Run `git` with the given arguments inside `dir`.
fn run_git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("failed to run git - is it installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Detect the trunk, as a full ref usable anywhere git takes a revision.
///
/// The remote's default branch (`origin/HEAD`) comes first: it names the real
/// trunk even when a stale local `main` or `master` exists, or when there is no
/// local branch of that name at all. Without a remote, local `main` then
/// `master`.
fn detect_trunk(dir: &Path) -> String {
    let exists = |r: &str| run_git_in(dir, &["rev-parse", "--verify", "--quiet", r]).is_ok();
    if let Ok(out) = run_git_in(
        dir,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) && let Some(trunk) = remote_default_ref(&out)
        && exists(&trunk)
    {
        return trunk;
    }
    for local in ["refs/heads/main", "refs/heads/master"] {
        if exists(local) {
            return local.to_string();
        }
    }
    "refs/heads/main".to_string()
}

/// The remote-tracking ref `git symbolic-ref refs/remotes/origin/HEAD`
/// printed, if it points at a branch of origin.
fn remote_default_ref(symbolic_ref_output: &str) -> Option<String> {
    let target = symbolic_ref_output.trim();
    let branch = target.strip_prefix("refs/remotes/origin/")?;
    (!branch.is_empty() && branch != "HEAD").then(|| target.to_string())
}

/// One record from `git worktree list --porcelain`.
struct WorktreeEntry {
    path: PathBuf,
    head: String,
    /// Branch name (without `refs/heads/` prefix), or `None` for detached HEAD.
    branch: Option<String>,
}

/// Parse the porcelain output of `git worktree list --porcelain` into a list
/// of [`WorktreeEntry`] values. Bare worktrees are silently skipped.
fn parse_worktree_list(output: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_head = String::new();
    let mut current_branch: Option<String> = None;
    let mut is_bare = false;

    for line in output.lines() {
        if line.is_empty() {
            // End of a worktree record
            if let Some(path) = current_path.take() {
                if !is_bare {
                    entries.push(WorktreeEntry {
                        path,
                        head: current_head.clone(),
                        branch: current_branch.take(),
                    });
                }
                current_head.clear();
                current_branch = None;
                is_bare = false;
            }
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            current_head = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(rest.to_string());
        } else if line == "bare" {
            is_bare = true;
        }
        // "detached" line — we keep branch as None
    }

    // Handle last record (output may not end with blank line)
    if let Some(path) = current_path.take()
        && !is_bare
    {
        entries.push(WorktreeEntry {
            path,
            head: current_head,
            branch: current_branch,
        });
    }

    entries
}

/// Build the diff range used to compare a worktree against trunk.
///
/// Three-dot (`trunk...HEAD`) diffs the merge base against HEAD, so the result
/// only contains the worktree's own changes. Two-dot (`trunk..HEAD`) would
/// additionally include the inverse of every commit that landed on trunk since
/// the worktree branched — wrong numbers, and in a large, fast-moving repo a
/// whole-repo diff computed once per worktree.
fn trunk_range(trunk: &str) -> String {
    format!("{}...HEAD", trunk)
}

/// Reason to keep a worktree given `git status --porcelain` output, or `None`
/// if it is clean. Untracked files count (they would be deleted); ignored
/// files do not (they are build output and caches, which pruning is for).
fn dirty_problem(porcelain: &str) -> Option<String> {
    let n = porcelain.lines().filter(|l| !l.trim().is_empty()).count();
    match n {
        0 => None,
        1 => Some("1 uncommitted change".to_string()),
        n => Some(format!("{n} uncommitted changes")),
    }
}

/// Arguments for listing commits on `HEAD` that are on neither trunk nor any
/// remote-tracking branch. This subsumes "ahead of upstream": a branch whose
/// upstream is behind has exactly those commits off every remote, and a branch
/// that was never pushed has every commit it added since trunk.
fn unpushed_log_args(trunk: &str) -> Vec<String> {
    [
        "log",
        "--format=%h%x00%s",
        "HEAD",
        "--not",
        "--remotes",
        trunk,
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Reason to keep a worktree given the output of [`unpushed_log_args`].
fn unpushed_problem(log: &str) -> Option<String> {
    let commits: Vec<(&str, &str)> = log
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split_once('\0').unwrap_or((l, "")))
        .collect();
    let (hash, subject) = commits.first()?;
    let n = commits.len();
    let noun = if n == 1 { "commit" } else { "commits" };
    Some(format!(
        "{n} {noun} not pushed to any remote (e.g. {hash} {subject:?})"
    ))
}

/// [`VcsBackend`] implementation that delegates to the `git` CLI via worktrees.
#[derive(Default)]
pub struct GitBackend {
    /// Memoised trunk branch name. `detect_trunk` runs up to 3 `git` subprocesses,
    /// and the result is invariant for the lifetime of a single dwm invocation,
    /// so cache it on the backend instance.
    trunk_cache: OnceLock<String>,
}

impl GitBackend {
    fn trunk(&self, dir: &Path) -> &str {
        self.trunk_cache.get_or_init(|| detect_trunk(dir))
    }
}

impl VcsBackend for GitBackend {
    fn workspace_start_revision(
        &self,
        repo_dir: &Path,
        name: &str,
        _info: &WorkspaceInfo,
    ) -> Result<String> {
        let output = run_git_in(repo_dir, &["worktree", "list", "--porcelain"])?;
        parse_worktree_list(&output)
            .into_iter()
            .find(|entry| entry.path.file_name().is_some_and(|n| n == name))
            .map(|entry| entry.head)
            .with_context(|| format!("workspace '{name}' not found"))
    }
    fn resolve_revision(&self, repo_dir: &Path, revision: &str) -> Result<String> {
        Ok(run_git_in(
            repo_dir,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{revision}^{{commit}}"),
            ],
        )?
        .trim()
        .to_string())
    }

    fn workspace_add_configured(
        &self,
        repo_dir: &Path,
        ws_path: &Path,
        name: &str,
        at: Option<&str>,
        source: &Path,
        config: &crate::config::WorkspaceConfig,
    ) -> Result<()> {
        if config.checkout == crate::config::Checkout::Cow {
            crate::git_cow::add(repo_dir, ws_path, name, at, source)
        } else {
            self.workspace_add(repo_dir, ws_path, name, at)
        }
    }
    fn root_from(&self, dir: &Path) -> Result<PathBuf> {
        let out = run_git_in(dir, &["rev-parse", "--show-toplevel"])?;
        Ok(PathBuf::from(out.trim()))
    }

    fn workspace_list(&self, repo_dir: &Path) -> Result<Vec<(String, WorkspaceInfo)>> {
        let out = run_git_in(repo_dir, &["worktree", "list", "--porcelain"])?;
        let worktrees = parse_worktree_list(&out);

        let mut results = Vec::new();
        for wt in worktrees {
            let name = wt
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let short_hash = if wt.head.len() >= 8 {
                wt.head[..8].to_string()
            } else {
                wt.head.clone()
            };

            let bookmarks: Vec<String> = wt.branch.into_iter().collect();

            results.push((
                name,
                WorkspaceInfo {
                    change_id: short_hash,
                    description: String::new(),
                    bookmarks,
                },
            ));
        }
        Ok(results)
    }

    fn workspace_add(
        &self,
        repo_dir: &Path,
        ws_path: &Path,
        name: &str,
        at: Option<&str>,
    ) -> Result<()> {
        let path_str = ws_path.to_string_lossy();
        let mut args = vec!["worktree", "add", &path_str, "-b", name];
        let resolved;
        if let Some(rev) = at {
            resolved = self.resolve_revision(repo_dir, rev)?;
            args.push(&resolved);
        }
        run_git_in(repo_dir, &args)?;
        Ok(())
    }

    fn workspace_remove(&self, repo_dir: &Path, _name: &str, ws_path: &Path) -> Result<()> {
        let path_str = ws_path.to_string_lossy();
        run_git_in(repo_dir, &["worktree", "remove", &path_str, "--force"])?;
        Ok(())
    }

    fn workspace_rename(
        &self,
        repo_dir: &Path,
        old_path: &Path,
        new_path: &Path,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<()> {
        let old_str = old_path.to_string_lossy();
        let new_str = new_path.to_string_lossy();
        run_git_in(repo_dir, &["worktree", "move", &old_str, &new_str])?;
        Ok(())
    }

    fn diff_stat_vs_trunk(
        &self,
        _repo_dir: &Path,
        worktree_dir: &Path,
        _ws_name: &str,
    ) -> Result<DiffStat> {
        // `--shortstat` because only the summary line is parsed: `--stat`
        // makes git render (and us pipe) one line per changed file.
        match run_git_in(
            worktree_dir,
            &[
                "diff",
                "--shortstat",
                &trunk_range(self.trunk(worktree_dir)),
            ],
        ) {
            Ok(text) => vcs::parse_diff_stat(&text),
            Err(_) => Ok(DiffStat::default()),
        }
    }

    fn latest_description(&self, _repo_dir: &Path, worktree_dir: &Path, _ws_name: &str) -> String {
        run_git_in(worktree_dir, &["log", "--format=%s", "-1"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    fn is_merged_into_trunk(&self, _repo_dir: &Path, worktree_dir: &Path, _ws_name: &str) -> bool {
        // Check if HEAD is an ancestor of trunk (i.e., fully merged)
        run_git_in(
            worktree_dir,
            &[
                "merge-base",
                "--is-ancestor",
                "HEAD",
                self.trunk(worktree_dir),
            ],
        )
        .is_ok()
    }

    fn vcs_type(&self) -> crate::vcs::VcsType {
        crate::vcs::VcsType::Git
    }

    fn main_workspace_name(&self) -> &'static str {
        vcs::VcsType::Git.main_workspace_name()
    }

    fn preview_log(
        &self,
        _repo_dir: &Path,
        worktree_dir: &Path,
        _ws_name: &str,
        limit: usize,
    ) -> String {
        let limit_str = format!("-{}", limit);
        run_git_in(
            worktree_dir,
            &["log", "--oneline", "--decorate", &limit_str],
        )
        .unwrap_or_default()
    }

    fn trunk_revision(&self, repo_dir: &Path) -> String {
        self.trunk(repo_dir).to_string()
    }

    fn reset_workspace(
        &self,
        repo_dir: &Path,
        worktree_dir: &Path,
        ws_name: &str,
        revision: &str,
    ) -> Result<()> {
        let commit = self.resolve_revision(repo_dir, revision)?;
        // Put the worktree back on the branch named after it. Resetting
        // whatever branch it was left on would move an unrelated branch the
        // last user switched to. If that name is checked out in another
        // worktree, detach rather than fight over it.
        if run_git_in(worktree_dir, &["checkout", "-B", ws_name, &commit]).is_err() {
            run_git_in(worktree_dir, &["checkout", "--detach", &commit])?;
        }
        Ok(())
    }

    fn prune_verdict(&self, _repo_dir: &Path, worktree_dir: &Path, _ws_name: &str) -> PruneVerdict {
        match run_git_in(
            worktree_dir,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        ) {
            Ok(out) => {
                if let Some(reason) = dirty_problem(&out) {
                    return PruneVerdict::Keep(reason);
                }
            }
            Err(e) => return PruneVerdict::Keep(format!("{e:#}")),
        }
        let trunk = self.trunk(worktree_dir).to_string();
        let args = unpushed_log_args(&trunk);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match run_git_in(worktree_dir, &args) {
            Ok(out) => match unpushed_problem(&out) {
                Some(reason) => PruneVerdict::Keep(reason),
                None => PruneVerdict::Prunable,
            },
            Err(e) => PruneVerdict::Keep(format!("could not compare with trunk '{trunk}': {e:#}")),
        }
    }

    fn preview_diff_stat(&self, _repo_dir: &Path, worktree_dir: &Path, _ws_name: &str) -> String {
        run_git_in(
            worktree_dir,
            &["diff", "--stat", &trunk_range(self.trunk(worktree_dir))],
        )
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_problem_counts_porcelain_entries() {
        assert_eq!(dirty_problem(""), None);
        assert_eq!(
            dirty_problem(" M src/a.rs\n").as_deref(),
            Some("1 uncommitted change")
        );
        assert_eq!(
            dirty_problem(" M src/a.rs\n?? new.txt\nUU conflict.rs\n").as_deref(),
            Some("3 uncommitted changes")
        );
    }

    #[test]
    fn unpushed_problem_parses_hash_and_subject() {
        assert_eq!(unpushed_problem(""), None);
        assert_eq!(
            unpushed_problem("abc1234\0add thing\ndef5678\0\n").as_deref(),
            Some("2 commits not pushed to any remote (e.g. abc1234 \"add thing\")")
        );
    }

    #[test]
    fn unpushed_log_args_negate_trunk_and_remotes() {
        let args = unpushed_log_args("refs/heads/main");
        let not = args.iter().position(|a| a == "--not").unwrap();
        let head = args.iter().position(|a| a == "HEAD").unwrap();
        assert!(head < not);
        assert!(args[not..].contains(&"--remotes".to_string()));
        assert!(args[not..].contains(&"refs/heads/main".to_string()));
    }

    #[test]
    fn parse_worktree_list_basic() {
        let output = "\
worktree /home/user/project
HEAD abc1234567890
branch refs/heads/main

worktree /home/user/.dwm/project/feature
HEAD def4567890123
branch refs/heads/feature

";
        let entries = parse_worktree_list(output);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(entries[0].head, "abc1234567890");
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(
            entries[1].path,
            PathBuf::from("/home/user/.dwm/project/feature")
        );
        assert_eq!(entries[1].branch.as_deref(), Some("feature"));
    }

    #[test]
    fn parse_worktree_list_bare_excluded() {
        let output = "\
worktree /home/user/project.git
HEAD 0000000000000000000000000000000000000000
bare

worktree /home/user/project
HEAD abc1234567890
branch refs/heads/main

";
        let entries = parse_worktree_list(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("/home/user/project"));
    }

    #[test]
    fn parse_worktree_list_detached_head() {
        let output = "\
worktree /home/user/project
HEAD abc1234567890
detached

";
        let entries = parse_worktree_list(output);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].branch.is_none());
    }

    #[test]
    fn parse_worktree_list_single() {
        let output = "\
worktree /home/user/project
HEAD abc1234567890
branch refs/heads/main
";
        let entries = parse_worktree_list(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_worktree_list_empty() {
        let entries = parse_worktree_list("");
        assert!(entries.is_empty());
    }

    #[test]
    fn git_backend_vcs_type() {
        assert_eq!(GitBackend::default().vcs_type(), crate::vcs::VcsType::Git);
    }

    #[test]
    fn git_backend_main_workspace_name() {
        assert_eq!(GitBackend::default().main_workspace_name(), "main-worktree");
    }

    #[test]
    fn trunk_cache_populated_after_first_call() {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "-b", "main", dir.path().to_str().unwrap()])
            .output()
            .expect("git must be installed to run this test");
        let _ = Command::new("git")
            .args([
                "-C",
                dir.path().to_str().unwrap(),
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .output();
        let backend = GitBackend::default();
        assert!(backend.trunk_cache.get().is_none());
        let trunk1 = backend.trunk(dir.path()).to_string();
        assert!(backend.trunk_cache.get().is_some());
        let trunk2 = backend.trunk(dir.path()).to_string();
        assert_eq!(trunk1, trunk2);
    }

    // Integration tests that require a real git repo
    #[test]
    fn integration_workspace_add_honors_starting_revision() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git_in(&repo, &["init", "-q", "-b", "main"]).unwrap();
        run_git_in(&repo, &["config", "user.email", "dwm@example.com"]).unwrap();
        run_git_in(&repo, &["config", "user.name", "dwm"]).unwrap();
        run_git_in(&repo, &["config", "commit.gpgsign", "false"]).unwrap();
        for content in ["first\n", "second\n"] {
            std::fs::write(repo.join("file.txt"), content).unwrap();
            run_git_in(&repo, &["add", "file.txt"]).unwrap();
            run_git_in(&repo, &["commit", "-qm", content.trim()]).unwrap();
        }

        let backend = GitBackend::default();
        let first = run_git_in(&repo, &["rev-parse", "HEAD~1"]).unwrap();
        let head = run_git_in(&repo, &["rev-parse", "HEAD"]).unwrap();
        // --at can supply a revision expression; --from supplies an ID.
        for (name, at, expected_id, expected_content) in [
            ("at", Some("HEAD~1"), first.trim(), "first\n"),
            ("from", Some(first.trim()), first.trim(), "first\n"),
            ("default", None, head.trim(), "second\n"),
        ] {
            let path = dir.path().join(name);
            backend.workspace_add(&repo, &path, name, at).unwrap();
            assert_eq!(
                run_git_in(&path, &["rev-parse", "HEAD"]).unwrap().trim(),
                expected_id
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file.txt")).unwrap(),
                expected_content
            );
        }
        assert!(
            backend
                .workspace_add(
                    &repo,
                    &dir.path().join("invalid"),
                    "invalid",
                    Some("missing-revision")
                )
                .is_err()
        );
    }

    #[test]
    fn integration_root_from() {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", dir.path().to_str().unwrap()])
            .output()
            .expect("git must be installed to run this test");
        let backend = GitBackend::default();
        let root = backend.root_from(dir.path()).unwrap();
        // Canonicalize both for comparison (handles /private/tmp on macOS)
        let expected = dir.path().canonicalize().unwrap();
        let actual = root.canonicalize().unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn trunk_range_is_three_dot() {
        assert_eq!(trunk_range("main"), "main...HEAD");
    }

    /// Regression: the trunk comparison used a two-dot range, so a worktree
    /// that had changed one file reported every file trunk had touched since
    /// the worktree branched (inverted, as deletions). It also made the diff
    /// proportional to trunk's churn rather than to the worktree's changes.
    #[test]
    fn integration_diff_stat_ignores_trunk_advancing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("git must be installed to run this test");
            assert!(out.status.success(), "git {:?} failed", args);
        };
        git(&["init", "-q", "-b", "main", "."]);
        git(&["config", "user.email", "dwm@example.com"]);
        git(&["config", "user.name", "dwm"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        std::fs::write(repo.join("trunk-only.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);

        // Branch a worktree off the current trunk tip and change one file.
        let wt = dir.path().join("wt");
        git(&[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "feature",
        ]);
        std::fs::write(wt.join("base.txt"), "base\nfeature\n").unwrap();
        let wt_git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&wt)
                .output()
                .expect("git must be installed to run this test");
            assert!(out.status.success(), "git {:?} failed", args);
        };
        wt_git(&["add", "-A"]);
        wt_git(&["commit", "-qm", "feature work"]);

        // Trunk moves on afterwards, touching a file the worktree never saw.
        std::fs::write(repo.join("trunk-only.txt"), "one\ntwo\nthree\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "trunk work"]);

        let backend = GitBackend::default();
        let stat = backend.diff_stat_vs_trunk(repo, &wt, "feature").unwrap();
        assert_eq!(stat.files_changed, 1);
        assert_eq!(stat.insertions, 1);
        assert_eq!(stat.deletions, 0);
    }

    #[test]
    fn integration_detect_trunk_defaults() {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "-b", "main", dir.path().to_str().unwrap()])
            .output()
            .expect("git must be installed to run this test");
        // Need at least one commit for the branch to exist
        let _ = Command::new("git")
            .args([
                "-C",
                dir.path().to_str().unwrap(),
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .output();
        let trunk = detect_trunk(dir.path());
        assert_eq!(trunk, "refs/heads/main");
    }

    #[test]
    fn integration_detect_trunk_master() {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "-b", "master", dir.path().to_str().unwrap()])
            .output()
            .expect("git must be installed to run this test");
        let _ = Command::new("git")
            .args([
                "-C",
                dir.path().to_str().unwrap(),
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .output();
        let trunk = detect_trunk(dir.path());
        assert_eq!(trunk, "refs/heads/master");
    }

    #[test]
    fn remote_default_ref_reads_symbolic_ref_output() {
        assert_eq!(
            remote_default_ref("refs/remotes/origin/develop\n").as_deref(),
            Some("refs/remotes/origin/develop")
        );
        assert_eq!(
            remote_default_ref("refs/remotes/origin/release/2.x").as_deref(),
            Some("refs/remotes/origin/release/2.x")
        );
        assert_eq!(remote_default_ref("refs/remotes/origin/"), None);
        assert_eq!(remote_default_ref("refs/remotes/upstream/main"), None);
        assert_eq!(remote_default_ref(""), None);
    }

    /// Regression: a stale local `main` won over the remote's real default
    /// branch, so diffs, merge checks and prune compared against the wrong one.
    #[test]
    fn integration_detect_trunk_prefers_origin_head_over_local_main() {
        let tmp = tempfile::tempdir().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git must be installed to run this test");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "develop"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "init"]);

        let clone = tmp.path().join("clone");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                upstream.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        // A local `main` that isn't the project's trunk.
        git(&clone, &["branch", "main"]);

        assert_eq!(detect_trunk(&clone), "refs/remotes/origin/develop");
        // The ref works as a range endpoint without a local `develop`.
        git(&clone, &["checkout", "-q", "main"]);
        git(&clone, &["branch", "-q", "-D", "develop"]);
        git(&clone, &["log", "-q", &trunk_range(&detect_trunk(&clone))]);
    }
}
