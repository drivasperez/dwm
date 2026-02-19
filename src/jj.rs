use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::vcs::{self, DiffStat, VcsBackend, WorkspaceInfo};

/// Run `jj` with the given arguments in the current working directory.
fn run_jj(args: &[&str]) -> Result<String> {
    let output = Command::new("jj")
        .args(args)
        .output()
        .context("failed to run jj - is it installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run `jj` with the given arguments inside `dir`.
fn run_jj_in(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("jj")
        .args(args)
        .current_dir(dir)
        .output()
        .context("failed to run jj - is it installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Return the jj repository root from the current working directory.
pub fn root() -> Result<PathBuf> {
    let out = run_jj(&["root"])?;
    Ok(PathBuf::from(out.trim()))
}

/// Return the jj repository root by running `jj root` inside `dir`.
pub fn root_from(dir: &Path) -> Result<PathBuf> {
    let out = run_jj_in(dir, &["root"])?;
    Ok(PathBuf::from(out.trim()))
}

/// Return the basename of the current jj repository root directory.
pub fn repo_name() -> Result<String> {
    let root = root()?;
    let name = root
        .file_name()
        .context("repo root has no directory name")?
        .to_string_lossy()
        .to_string();
    Ok(name)
}

/// Return the jj template string used with `jj workspace list -T`.
///
/// Fields are separated by NUL (`\0`) and records by `\0\n` so that
/// descriptions containing tabs or newlines are parsed correctly.
fn workspace_list_template() -> &'static str {
    concat!(
        r#"name ++ "\0" ++ self.target().change_id().shortest(8) ++ "\0""#,
        r#" ++ self.target().description() ++ "\0""#,
        r#" ++ self.target().bookmarks().map(|b| b.name()).join(",") ++ "\0\n""#,
    )
}

/// Parse the NUL-delimited output produced by `jj workspace list` with
/// [`workspace_list_template`] into a list of `(workspace_name, WorkspaceInfo)` pairs.
fn parse_workspace_info(output: &str) -> Result<Vec<(String, WorkspaceInfo)>> {
    let mut results = Vec::new();
    for record in output.split("\0\n") {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\0').collect();
        if fields.len() >= 4 {
            let name = fields[0].to_string();
            let change_id = fields[1].to_string();
            let description = fields[2].to_string();
            let bookmarks: Vec<String> = fields[3]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            results.push((
                name,
                WorkspaceInfo {
                    change_id,
                    description,
                    bookmarks,
                },
            ));
        }
    }
    Ok(results)
}

/// Walk the ancestor chain of `workspace_name@` and return the description of
/// the most recent commit that has a non-empty message. Returns an empty string
/// when no such ancestor exists or jj returns an error.
fn latest_description(dir: &Path, workspace_name: &str) -> String {
    let revset = format!(
        r#"latest(ancestors({name}@) & description(glob:"?*"))"#,
        name = workspace_name
    );
    let result = run_jj_in(
        dir,
        &[
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            "description",
            "--limit",
            "1",
        ],
    );
    match result {
        Ok(text) => {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                String::new()
            } else {
                trimmed
            }
        }
        Err(_) => String::new(),
    }
}

/// Run `jj diff --stat --from <from> --to <to>` inside `dir` and parse the
/// result. Returns a zeroed [`DiffStat`] if jj reports an error.
fn diff_stat(dir: &Path, from: &str, to: &str) -> Result<DiffStat> {
    let out = run_jj_in(dir, &["diff", "--stat", "--from", from, "--to", to]);
    match out {
        Ok(text) => vcs::parse_diff_stat(&text),
        Err(_) => Ok(DiffStat::default()),
    }
}

/// [`VcsBackend`] implementation that delegates to the `jj` CLI.
pub struct JjBackend;

impl VcsBackend for JjBackend {
    fn root_from(&self, dir: &Path) -> Result<PathBuf> {
        root_from(dir)
    }

    fn repo_name_from(&self, dir: &Path) -> Result<String> {
        let root = self.root_from(dir)?;
        Ok(crate::vcs::repo_dir_name(&root))
    }

    fn workspace_list(&self, repo_dir: &Path) -> Result<Vec<(String, WorkspaceInfo)>> {
        let out = run_jj_in(
            repo_dir,
            &["workspace", "list", "-T", workspace_list_template()],
        )?;
        parse_workspace_info(&out)
    }

    fn workspace_add(
        &self,
        repo_dir: &Path,
        ws_path: &Path,
        name: &str,
        at: Option<&str>,
    ) -> Result<()> {
        let path_str = ws_path.to_string_lossy();
        let mut args = vec!["workspace", "add", "--name", name, &path_str];
        if let Some(rev) = at {
            args.push("--revision");
            args.push(rev);
        }
        run_jj_in(repo_dir, &args)?;
        Ok(())
    }

    fn workspace_remove(&self, repo_dir: &Path, name: &str, _ws_path: &Path) -> Result<()> {
        run_jj_in(repo_dir, &["workspace", "forget", name])?;
        Ok(())
    }

    fn workspace_rename(
        &self,
        _repo_dir: &Path,
        old_path: &Path,
        new_path: &Path,
        _old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        // Update stale working copy before rename (common when workspace hasn't been used recently)
        let _ = run_jj_in(old_path, &["workspace", "update-stale"]);
        // jj workspace rename updates VCS metadata (run inside the workspace dir)
        run_jj_in(old_path, &["workspace", "rename", new_name])?;
        // Then move the directory
        std::fs::rename(old_path, new_path)?;
        Ok(())
    }

    fn diff_stat_vs_trunk(
        &self,
        repo_dir: &Path,
        _worktree_dir: &Path,
        ws_name: &str,
    ) -> Result<DiffStat> {
        let to = if ws_name == "default" {
            "@".to_string()
        } else {
            format!("{}@", ws_name)
        };
        diff_stat(repo_dir, "trunk()", &to)
    }

    fn latest_description(&self, repo_dir: &Path, _worktree_dir: &Path, ws_name: &str) -> String {
        latest_description(repo_dir, ws_name)
    }

    fn is_merged_into_trunk(&self, repo_dir: &Path, _worktree_dir: &Path, ws_name: &str) -> bool {
        let revset = if ws_name == "default" {
            "trunk()..@".to_string()
        } else {
            format!("trunk()..{}@", ws_name)
        };
        match run_jj_in(
            repo_dir,
            &["log", "-r", &revset, "--no-graph", "-T", "commit_id"],
        ) {
            Ok(out) => out.trim().is_empty(),
            Err(_) => false,
        }
    }

    fn vcs_name(&self) -> &'static str {
        "jj"
    }

    fn main_workspace_name(&self) -> &'static str {
        "default"
    }

    fn preview_log(
        &self,
        repo_dir: &Path,
        _worktree_dir: &Path,
        ws_name: &str,
        limit: usize,
    ) -> String {
        let ancestor_rev = if ws_name == "default" {
            "ancestors(@)".to_string()
        } else {
            format!("ancestors({}@)", ws_name)
        };
        let limit_str = limit.to_string();
        run_jj_in(
            repo_dir,
            &["log", "-r", &ancestor_rev, "--limit", &limit_str],
        )
        .unwrap_or_default()
    }

    fn preview_diff_stat(&self, repo_dir: &Path, _worktree_dir: &Path, ws_name: &str) -> String {
        let to = if ws_name == "default" {
            "@".to_string()
        } else {
            format!("{}@", ws_name)
        };
        run_jj_in(
            repo_dir,
            &["diff", "--stat", "--from", "trunk()", "--to", &to],
        )
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_workspace_info_basic() {
        let output =
            "default\0abc12345\0fix login bug\0main,dev\0\nfeature\0def67890\0add tests\0\0\n";
        let result = parse_workspace_info(output).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "default");
        assert_eq!(result[0].1.change_id, "abc12345");
        assert_eq!(result[0].1.description, "fix login bug");
        assert_eq!(result[0].1.bookmarks, vec!["main", "dev"]);
        assert_eq!(result[1].0, "feature");
        assert_eq!(result[1].1.change_id, "def67890");
        assert_eq!(result[1].1.description, "add tests");
        assert!(result[1].1.bookmarks.is_empty());
    }

    #[test]
    fn parse_workspace_info_empty_bookmarks() {
        let output = "ws1\0aaa\0desc\0\0\n";
        let result = parse_workspace_info(output).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].1.bookmarks.is_empty());
    }

    #[test]
    fn parse_workspace_info_empty_input() {
        let output = "";
        let result = parse_workspace_info(output).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_workspace_info_multiline_description() {
        let output = "default\0abc\0first line\nsecond line\0bookmark1\0\n";
        let result = parse_workspace_info(output).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1.description, "first line\nsecond line");
        assert_eq!(result[0].1.bookmarks, vec!["bookmark1"]);
    }
}
