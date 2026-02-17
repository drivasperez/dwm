use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::vcs::{self, DiffStat, VcsBackend, WorkspaceInfo};

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

/// Try to detect the trunk/main branch name.
/// Checks: main, master, then origin/HEAD symbolic ref.
fn detect_trunk(dir: &Path) -> String {
    // Check if "main" branch exists
    if run_git_in(dir, &["rev-parse", "--verify", "refs/heads/main"]).is_ok() {
        return "main".to_string();
    }
    // Check if "master" branch exists
    if run_git_in(dir, &["rev-parse", "--verify", "refs/heads/master"]).is_ok() {
        return "master".to_string();
    }
    // Try origin/HEAD
    if let Ok(out) = run_git_in(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        let trimmed = out.trim();
        if let Some(branch) = trimmed.strip_prefix("refs/remotes/origin/") {
            return branch.to_string();
        }
    }
    // Fallback
    "main".to_string()
}

struct WorktreeEntry {
    path: PathBuf,
    head: String,
    branch: Option<String>,
}

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

pub struct GitBackend;

impl VcsBackend for GitBackend {
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

            let description = run_git_in(&wt.path, &["log", "--format=%s", "-1"])
                .map(|s| s.trim().to_string())
                .unwrap_or_default();

            let bookmarks: Vec<String> = wt.branch.into_iter().collect();

            results.push((
                name,
                WorkspaceInfo {
                    change_id: short_hash,
                    description,
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
        _at: Option<&str>,
    ) -> Result<()> {
        let path_str = ws_path.to_string_lossy();
        run_git_in(repo_dir, &["worktree", "add", &path_str, "-b", name])?;
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
        _change_id: &str,
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
        let trunk = detect_trunk(worktree_dir);
        let range = format!("{}..HEAD", trunk);
        match run_git_in(worktree_dir, &["diff", "--stat", &range]) {
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
        let trunk = detect_trunk(worktree_dir);
        // Check if HEAD is an ancestor of trunk (i.e., fully merged)
        run_git_in(
            worktree_dir,
            &["merge-base", "--is-ancestor", "HEAD", &trunk],
        )
        .is_ok()
    }

    fn vcs_name(&self) -> &'static str {
        "git"
    }

    fn main_workspace_name(&self) -> &'static str {
        "main-worktree"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn git_backend_vcs_name() {
        assert_eq!(GitBackend.vcs_name(), "git");
    }

    #[test]
    fn git_backend_main_workspace_name() {
        assert_eq!(GitBackend.main_workspace_name(), "main-worktree");
    }

    // Integration tests that require a real git repo
    #[test]
    fn integration_root_from() {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", dir.path().to_str().unwrap()])
            .output();
        if status.is_err() {
            // git not installed, skip
            return;
        }
        let backend = GitBackend;
        let root = backend.root_from(dir.path()).unwrap();
        // Canonicalize both for comparison (handles /private/tmp on macOS)
        let expected = dir.path().canonicalize().unwrap();
        let actual = root.canonicalize().unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn integration_detect_trunk_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-b", "main", dir.path().to_str().unwrap()])
            .output();
        if status.is_err() {
            return;
        }
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
        assert_eq!(trunk, "main");
    }

    #[test]
    fn integration_detect_trunk_master() {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-b", "master", dir.path().to_str().unwrap()])
            .output();
        if status.is_err() {
            return;
        }
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
        assert_eq!(trunk, "master");
    }
}
