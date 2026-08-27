use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::vcs::{self, DiffStat, VcsBackend, WorkspaceInfo};

/// Global flag that turns a `jj` invocation into a pure read of the repo.
///
/// Without it, *every* `jj` command first snapshots the working copy of the
/// workspace it runs in and writes a new operation to the op log, taking the
/// repo lock to do so. In a large repo that snapshot walks the whole working
/// tree, and the lock serialises the per-workspace calls dwm fans out in
/// parallel. Listing a repo needs at most one snapshot (see
/// [`VcsBackend::workspace_list`]), so every other query skips it.
const IGNORE_WC: &str = "--ignore-working-copy";

/// Prepend [`IGNORE_WC`] to `args`, for read-only queries.
fn read_only_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push(IGNORE_WC);
    full.extend_from_slice(args);
    full
}

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

/// Run a read-only `jj` query in the current working directory.
fn run_jj_ro(args: &[&str]) -> Result<String> {
    run_jj(&read_only_args(args))
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

/// Run a read-only `jj` query inside `dir`.
fn run_jj_ro_in(dir: &Path, args: &[&str]) -> Result<String> {
    run_jj_in(dir, &read_only_args(args))
}

/// Return the jj repository root from the current working directory.
pub fn root() -> Result<PathBuf> {
    let out = run_jj_ro(&["root"])?;
    Ok(PathBuf::from(out.trim()))
}

/// Return the jj repository root by running `jj root` inside `dir`.
pub fn root_from(dir: &Path) -> Result<PathBuf> {
    let out = run_jj_ro_in(dir, &["root"])?;
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

/// Format a workspace name as a jj revset operand, quoting it if it contains
/// characters that are not valid in a bare identifier (e.g. spaces).
fn revset_ws(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        format!("{}@", name)
    } else {
        format!("`{}`@", name)
    }
}

/// Revset naming the revision a workspace is currently on. The main workspace
/// is addressed as `@` so that queries work from any directory in the repo.
fn workspace_revision(ws_name: &str) -> String {
    if ws_name == vcs::VcsType::Jj.main_workspace_name() {
        "@".to_string()
    } else {
        revset_ws(ws_name)
    }
}

/// Walk the ancestor chain of `workspace_name@` and return the description of
/// the most recent commit that has a non-empty message. Returns an empty string
/// when no such ancestor exists or jj returns an error.
fn latest_description(dir: &Path, workspace_name: &str) -> String {
    let ws_at = revset_ws(workspace_name);
    let revset = format!(r#"latest(ancestors({ws_at}) & description(glob:"?*"))"#,);
    let result = run_jj_ro_in(
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
/// result. Propagates jj's error so callers can fall back to another revset.
fn diff_stat(dir: &Path, from: &str, to: &str) -> Result<DiffStat> {
    let text = run_jj_ro_in(dir, &["diff", "--stat", "--from", from, "--to", to])?;
    vcs::parse_diff_stat(&text)
}

/// Revset for the point at which `to` forked off trunk.
///
/// Diffing from here rather than from `trunk()` itself keeps the stat to the
/// workspace's own changes. `--from trunk()` compares the two tips, so a
/// workspace that simply hasn't been rebased reports every change that landed
/// on trunk since it branched, inverted — wrong numbers, and a diff whose cost
/// grows with trunk's churn rather than with the workspace.
fn fork_point_of(to: &str) -> String {
    format!("fork_point(trunk() | {})", to)
}

/// Diff stat for a workspace against the point it forked off trunk, falling
/// back to `trunk()` itself on jj versions without `fork_point()`.
fn diff_stat_vs_trunk(dir: &Path, to: &str) -> DiffStat {
    if let Ok(stat) = diff_stat(dir, &fork_point_of(to), to) {
        return stat;
    }
    diff_stat(dir, "trunk()", to).unwrap_or_default()
}

/// [`VcsBackend`] implementation that delegates to the `jj` CLI.
pub struct JjBackend;

impl VcsBackend for JjBackend {
    fn root_from(&self, dir: &Path) -> Result<PathBuf> {
        root_from(dir)
    }

    fn workspace_list(&self, repo_dir: &Path) -> Result<Vec<(String, WorkspaceInfo)>> {
        // Deliberately *not* a read-only call: this is the one command a
        // listing runs in the main workspace before fanning out, so letting it
        // snapshot keeps uncommitted edits in the main working copy visible in
        // the listing. Every per-workspace query that follows uses
        // [`IGNORE_WC`], so a listing snapshots once instead of once per query.
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
        Ok(diff_stat_vs_trunk(repo_dir, &workspace_revision(ws_name)))
    }

    fn latest_description(&self, repo_dir: &Path, _worktree_dir: &Path, ws_name: &str) -> String {
        latest_description(repo_dir, ws_name)
    }

    fn is_merged_into_trunk(&self, repo_dir: &Path, _worktree_dir: &Path, ws_name: &str) -> bool {
        let revset = format!("trunk()..{}", workspace_revision(ws_name));
        match run_jj_ro_in(
            repo_dir,
            &[
                "log",
                "-r",
                &revset,
                "--no-graph",
                "-T",
                "commit_id",
                "--limit",
                "1",
            ],
        ) {
            Ok(out) => out.trim().is_empty(),
            Err(_) => false,
        }
    }

    fn vcs_type(&self) -> crate::vcs::VcsType {
        crate::vcs::VcsType::Jj
    }

    fn main_workspace_name(&self) -> &'static str {
        vcs::VcsType::Jj.main_workspace_name()
    }

    fn preview_log(
        &self,
        repo_dir: &Path,
        _worktree_dir: &Path,
        ws_name: &str,
        limit: usize,
    ) -> String {
        let ancestor_rev = format!("ancestors({})", workspace_revision(ws_name));
        let limit_str = limit.to_string();
        run_jj_ro_in(
            repo_dir,
            &["log", "-r", &ancestor_rev, "--limit", &limit_str],
        )
        .unwrap_or_default()
    }

    fn preview_diff_stat(&self, repo_dir: &Path, _worktree_dir: &Path, ws_name: &str) -> String {
        let to = workspace_revision(ws_name);
        let diff =
            |from: &str| run_jj_ro_in(repo_dir, &["diff", "--stat", "--from", from, "--to", &to]);
        diff(&fork_point_of(&to))
            .or_else(|_| diff("trunk()"))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_args_prepends_ignore_working_copy() {
        assert_eq!(
            read_only_args(&["log", "-r", "@"]),
            vec!["--ignore-working-copy", "log", "-r", "@"]
        );
        assert_eq!(read_only_args(&[]), vec!["--ignore-working-copy"]);
    }

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

    #[test]
    fn parse_workspace_info_name_with_spaces() {
        let output = "my feature\0abc12345\0some description\0main\0\n";
        let result = parse_workspace_info(output).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "my feature");
        assert_eq!(result[0].1.change_id, "abc12345");
        assert_eq!(result[0].1.description, "some description");
    }

    #[test]
    fn workspace_revision_uses_at_for_main_workspace() {
        assert_eq!(workspace_revision("default"), "@");
        assert_eq!(workspace_revision("feature"), "feature@");
        assert_eq!(workspace_revision("my feature"), "`my feature`@");
    }

    #[test]
    fn fork_point_of_builds_merge_base_revset() {
        assert_eq!(fork_point_of("@"), "fork_point(trunk() | @)");
        assert_eq!(fork_point_of("feature@"), "fork_point(trunk() | feature@)");
    }

    #[test]
    fn revset_ws_simple_name() {
        assert_eq!(revset_ws("feature"), "feature@");
        assert_eq!(revset_ws("default"), "default@");
        assert_eq!(revset_ws("my-branch"), "my-branch@");
        assert_eq!(revset_ws("with_underscore"), "with_underscore@");
    }

    #[test]
    fn revset_ws_name_with_spaces() {
        assert_eq!(revset_ws("my feature"), "`my feature`@");
        assert_eq!(revset_ws("work in progress"), "`work in progress`@");
    }

    #[test]
    fn revset_ws_name_with_special_chars() {
        assert_eq!(revset_ws("feat/login"), "`feat/login`@");
        assert_eq!(revset_ws("fix.bug"), "`fix.bug`@");
    }
}
