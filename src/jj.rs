use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

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

pub fn root() -> Result<PathBuf> {
    let out = run_jj(&["root"])?;
    Ok(PathBuf::from(out.trim()))
}

pub fn root_from(dir: &Path) -> Result<PathBuf> {
    let out = run_jj_in(dir, &["root"])?;
    Ok(PathBuf::from(out.trim()))
}

pub fn repo_name() -> Result<String> {
    let root = root()?;
    let name = root
        .file_name()
        .context("repo root has no directory name")?
        .to_string_lossy()
        .to_string();
    Ok(name)
}

pub fn repo_name_from(dir: &Path) -> Result<String> {
    let root = root_from(dir)?;
    let name = root
        .file_name()
        .context("repo root has no directory name")?
        .to_string_lossy()
        .to_string();
    Ok(name)
}

#[derive(Debug, Default, Clone)]
pub struct WorkspaceInfo {
    pub change_id: String,
    pub description: String,
    pub bookmarks: Vec<String>,
}

pub fn workspace_list() -> Result<Vec<(String, WorkspaceInfo)>> {
    let out = run_jj(&[
        "workspace",
        "list",
        "-T",
        concat!(
            r#"name ++ "\0" ++ self.working_copy_commit().change_id().shortest(8) ++ "\0""#,
            r#" ++ self.working_copy_commit().description() ++ "\0""#,
            r#" ++ self.working_copy_commit().bookmarks().map(|b| b.name()).join(",") ++ "\0\n""#,
        ),
    ])?;
    parse_workspace_info(&out)
}

pub fn workspace_list_from(dir: &Path) -> Result<Vec<(String, WorkspaceInfo)>> {
    let out = run_jj_in(
        dir,
        &[
            "workspace",
            "list",
            "-T",
            concat!(
                r#"name ++ "\0" ++ self.working_copy_commit().change_id().shortest(8) ++ "\0""#,
                r#" ++ self.working_copy_commit().description() ++ "\0""#,
                r#" ++ self.working_copy_commit().bookmarks().map(|b| b.name()).join(",") ++ "\0\n""#,
            ),
        ],
    )?;
    parse_workspace_info(&out)
}

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

/// Find the latest ancestor of `workspace_name@` that has a non-empty description.
pub fn latest_description(dir: &Path, workspace_name: &str) -> String {
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
            if trimmed.is_empty() { String::new() } else { trimmed }
        }
        Err(_) => String::new(),
    }
}

pub fn workspace_add(path: &Path, name: &str) -> Result<()> {
    let path_str = path.to_string_lossy();
    run_jj(&["workspace", "add", "--name", name, &path_str])?;
    Ok(())
}

pub fn workspace_add_from(repo_dir: &Path, ws_path: &Path, name: &str) -> Result<()> {
    let path_str = ws_path.to_string_lossy();
    run_jj_in(repo_dir, &["workspace", "add", "--name", name, &path_str])?;
    Ok(())
}

pub fn workspace_forget(name: &str) -> Result<()> {
    run_jj(&["workspace", "forget", name])?;
    Ok(())
}

pub fn workspace_forget_from(dir: &Path, name: &str) -> Result<()> {
    run_jj_in(dir, &["workspace", "forget", name])?;
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct DiffStat {
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

pub fn diff_stat(dir: &Path, from: &str, to: &str) -> Result<DiffStat> {
    let out = run_jj_in(dir, &["diff", "--stat", "--from", from, "--to", to]);
    match out {
        Ok(text) => parse_diff_stat(&text),
        Err(_) => Ok(DiffStat::default()),
    }
}

fn parse_diff_stat(output: &str) -> Result<DiffStat> {
    // The last line looks like: "3 files changed, 10 insertions(+), 5 deletions(-)"
    // or just "0 files changed"
    if let Some(last_line) = output.lines().last() {
        if let Some(stat) = parse_diff_stat_line(last_line) {
            return Ok(stat);
        }
    }
    Ok(DiffStat::default())
}

fn parse_diff_stat_line(line: &str) -> Option<DiffStat> {
    let line = line.trim();
    if !line.contains("file") {
        return None;
    }
    let mut stat = DiffStat::default();

    for part in line.split(',') {
        let part = part.trim();
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.len() >= 2 {
            if let Ok(n) = tokens[0].parse::<u32>() {
                if tokens[1].starts_with("file") {
                    stat.files_changed = n;
                } else if tokens[1].starts_with("insertion") {
                    stat.insertions = n;
                } else if tokens[1].starts_with("deletion") {
                    stat.deletions = n;
                }
            }
        }
    }

    Some(stat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_stat_line() {
        let line = "3 files changed, 10 insertions(+), 5 deletions(-)";
        let stat = parse_diff_stat_line(line).unwrap();
        assert_eq!(stat.files_changed, 3);
        assert_eq!(stat.insertions, 10);
        assert_eq!(stat.deletions, 5);
    }

    #[test]
    fn parse_insertions_only() {
        let line = "1 file changed, 42 insertions(+)";
        let stat = parse_diff_stat_line(line).unwrap();
        assert_eq!(stat.files_changed, 1);
        assert_eq!(stat.insertions, 42);
        assert_eq!(stat.deletions, 0);
    }

    #[test]
    fn parse_deletions_only() {
        let line = "2 files changed, 7 deletions(-)";
        let stat = parse_diff_stat_line(line).unwrap();
        assert_eq!(stat.files_changed, 2);
        assert_eq!(stat.insertions, 0);
        assert_eq!(stat.deletions, 7);
    }

    #[test]
    fn parse_zero_changes() {
        let line = "0 files changed";
        let stat = parse_diff_stat_line(line).unwrap();
        assert_eq!(stat.files_changed, 0);
        assert_eq!(stat.insertions, 0);
        assert_eq!(stat.deletions, 0);
    }

    #[test]
    fn parse_non_stat_line_returns_none() {
        let line = " src/main.rs | 5 ++---";
        assert!(parse_diff_stat_line(line).is_none());
    }

    #[test]
    fn parse_diff_stat_multiline() {
        let output = " src/main.rs | 5 ++---\n src/lib.rs  | 3 +++\n 2 files changed, 5 insertions(+), 3 deletions(-)";
        let stat = parse_diff_stat(output).unwrap();
        assert_eq!(stat.files_changed, 2);
        assert_eq!(stat.insertions, 5);
        assert_eq!(stat.deletions, 3);
    }

    #[test]
    fn parse_workspace_info_basic() {
        let output = "default\0abc12345\0fix login bug\0main,dev\0\nfeature\0def67890\0add tests\0\0\n";
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
