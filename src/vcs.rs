use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct WorkspaceInfo {
    pub change_id: String,
    pub description: String,
    pub bookmarks: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct DiffStat {
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

pub trait VcsBackend {
    fn root_from(&self, dir: &Path) -> Result<PathBuf>;

    fn repo_name_from(&self, dir: &Path) -> Result<String> {
        let root = self.root_from(dir)?;
        let name = root
            .file_name()
            .context("repo root has no directory name")?
            .to_string_lossy()
            .to_string();
        Ok(name)
    }

    fn workspace_list(&self, repo_dir: &Path) -> Result<Vec<(String, WorkspaceInfo)>>;
    fn workspace_add(&self, repo_dir: &Path, ws_path: &Path, name: &str, at: Option<&str>) -> Result<()>;
    fn workspace_remove(&self, repo_dir: &Path, name: &str, ws_path: &Path) -> Result<()>;
    fn diff_stat_vs_trunk(
        &self,
        repo_dir: &Path,
        worktree_dir: &Path,
        ws_name: &str,
    ) -> Result<DiffStat>;
    fn latest_description(
        &self,
        repo_dir: &Path,
        worktree_dir: &Path,
        ws_name: &str,
    ) -> String;
    fn is_merged_into_trunk(&self, repo_dir: &Path, worktree_dir: &Path, ws_name: &str) -> bool;
    fn vcs_name(&self) -> &'static str;
    fn main_workspace_name(&self) -> &'static str;
}

/// Detect the VCS backend for a directory by walking up looking for `.jj/` (priority) then `.git/`.
pub fn detect(dir: &Path) -> Result<Box<dyn VcsBackend>> {
    let mut current = dir.to_path_buf();
    loop {
        if current.join(".jj").is_dir() {
            return Ok(Box::new(crate::jj::JjBackend));
        }
        if current.join(".git").exists() {
            return Ok(Box::new(crate::git::GitBackend));
        }
        if !current.pop() {
            break;
        }
    }
    bail!("no jj or git repository found in {} or any parent directory", dir.display())
}

/// Detect VCS from a jjws repo directory by reading the `.vcs-type` file.
/// Defaults to jj for backward compatibility if the file doesn't exist.
pub fn detect_from_jjws_dir(repo_dir: &Path) -> Result<Box<dyn VcsBackend>> {
    let vcs_file = repo_dir.join(".vcs-type");
    let vcs_type = if vcs_file.exists() {
        std::fs::read_to_string(&vcs_file)
            .with_context(|| format!("could not read {}", vcs_file.display()))?
            .trim()
            .to_string()
    } else {
        "jj".to_string()
    };

    match vcs_type.as_str() {
        "jj" => Ok(Box::new(crate::jj::JjBackend)),
        "git" => Ok(Box::new(crate::git::GitBackend)),
        other => bail!("unknown VCS type '{}' in {}", other, vcs_file.display()),
    }
}

pub fn parse_diff_stat(output: &str) -> Result<DiffStat> {
    if let Some(last_line) = output.lines().last()
        && let Some(stat) = parse_diff_stat_line(last_line)
    {
        return Ok(stat);
    }
    Ok(DiffStat::default())
}

pub fn parse_diff_stat_line(line: &str) -> Option<DiffStat> {
    let line = line.trim();
    if !line.contains("file") {
        return None;
    }
    let mut stat = DiffStat::default();

    for part in line.split(',') {
        let part = part.trim();
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.len() >= 2
            && let Ok(n) = tokens[0].parse::<u32>()
        {
            if tokens[1].starts_with("file") {
                stat.files_changed = n;
            } else if tokens[1].starts_with("insertion") {
                stat.insertions = n;
            } else if tokens[1].starts_with("deletion") {
                stat.deletions = n;
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
    fn detect_jj_priority_over_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let backend = detect(dir.path()).unwrap();
        assert_eq!(backend.vcs_name(), "jj");
    }

    #[test]
    fn detect_git_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let backend = detect(dir.path()).unwrap();
        assert_eq!(backend.vcs_name(), "git");
    }

    #[test]
    fn detect_jj_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        let backend = detect(dir.path()).unwrap();
        assert_eq!(backend.vcs_name(), "jj");
    }

    #[test]
    fn detect_no_vcs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path()).is_err());
    }

    #[test]
    fn detect_from_jjws_dir_defaults_to_jj() {
        let dir = tempfile::tempdir().unwrap();
        let backend = detect_from_jjws_dir(dir.path()).unwrap();
        assert_eq!(backend.vcs_name(), "jj");
    }

    #[test]
    fn detect_from_jjws_dir_reads_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".vcs-type"), "git").unwrap();
        let backend = detect_from_jjws_dir(dir.path()).unwrap();
        assert_eq!(backend.vcs_name(), "git");
    }

    #[test]
    fn detect_from_jjws_dir_reads_jj() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".vcs-type"), "jj").unwrap();
        let backend = detect_from_jjws_dir(dir.path()).unwrap();
        assert_eq!(backend.vcs_name(), "jj");
    }

    #[test]
    fn detect_from_jjws_dir_unknown_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".vcs-type"), "svn").unwrap();
        assert!(detect_from_jjws_dir(dir.path()).is_err());
    }
}
