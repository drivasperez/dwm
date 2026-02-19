use anyhow::{Context, Result, bail};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcsType {
    Jj,
    Git,
}

impl VcsType {
    pub fn to_backend(self) -> Box<dyn VcsBackend> {
        match self {
            VcsType::Jj => Box::new(crate::jj::JjBackend),
            VcsType::Git => Box::new(crate::git::GitBackend),
        }
    }
}

impl fmt::Display for VcsType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VcsType::Jj => write!(f, "jj"),
            VcsType::Git => write!(f, "git"),
        }
    }
}

impl FromStr for VcsType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "jj" => Ok(VcsType::Jj),
            "git" => Ok(VcsType::Git),
            other => bail!("unknown VCS type '{}'", other),
        }
    }
}

/// VCS-level metadata for a single workspace/worktree as reported by the
/// underlying VCS (jj or git).
#[derive(Debug, Default, Clone)]
pub struct WorkspaceInfo {
    /// Short change/commit id (8 hex chars).
    pub change_id: String,
    /// Commit message of the workspace's current revision.
    pub description: String,
    /// Branch or bookmark names pointing at this revision.
    pub bookmarks: Vec<String>,
}

/// Parsed summary line from `jj diff --stat` or `git diff --stat`.
#[derive(Debug, Default, Clone)]
pub struct DiffStat {
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

/// Compute a short FNV-1a hex hash of a path string, used to disambiguate
/// repos that share the same directory basename.
fn hash_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut h: u32 = 2166136261; // FNV-1a offset basis
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619); // FNV prime
    }
    format!("{:08x}", h)
}

/// Build the `~/.dwm/` sub-directory name for a repo.
///
/// The name is `<basename>-<8-char-hash>` so that two repos with the same
/// directory name but different paths get distinct dwm directories.
pub fn repo_dir_name(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{}-{}", name, hash_path(root))
}

/// Abstraction over jj and git that workspace operations are delegated to.
pub trait VcsBackend {
    /// Return the repository root given any directory inside the repo.
    fn root_from(&self, dir: &Path) -> Result<PathBuf>;

    /// Return the dwm directory name for the repo that contains `dir`.
    fn repo_name_from(&self, dir: &Path) -> Result<String> {
        let root = self.root_from(dir)?;
        Ok(repo_dir_name(&root))
    }

    /// List all workspaces/worktrees known to the VCS, returning `(name, info)` pairs.
    fn workspace_list(&self, repo_dir: &Path) -> Result<Vec<(String, WorkspaceInfo)>>;
    /// Create a new workspace/worktree at `ws_path` with the given `name`.
    /// `at` optionally specifies the starting revision.
    fn workspace_add(
        &self,
        repo_dir: &Path,
        ws_path: &Path,
        name: &str,
        at: Option<&str>,
    ) -> Result<()>;
    /// Remove the workspace/worktree from VCS tracking and delete its directory.
    fn workspace_remove(&self, repo_dir: &Path, name: &str, ws_path: &Path) -> Result<()>;
    /// Rename a workspace: update VCS metadata and move the directory.
    /// `old_path` and `new_path` are the workspace directories on disk.
    fn workspace_rename(
        &self,
        repo_dir: &Path,
        old_path: &Path,
        new_path: &Path,
        old_name: &str,
        new_name: &str,
    ) -> Result<()>;

    /// Return the diff stat between `trunk()` / main branch and the workspace's
    /// current revision.
    fn diff_stat_vs_trunk(
        &self,
        repo_dir: &Path,
        worktree_dir: &Path,
        ws_name: &str,
    ) -> Result<DiffStat>;
    /// Return the most recent non-empty commit description reachable from the
    /// workspace's head. Falls back to an empty string if none is found.
    fn latest_description(&self, repo_dir: &Path, worktree_dir: &Path, ws_name: &str) -> String;
    /// Return `true` if the workspace's changes have already been merged into
    /// the trunk branch (i.e. no un-merged commits exist).
    fn is_merged_into_trunk(&self, repo_dir: &Path, worktree_dir: &Path, ws_name: &str) -> bool;
    /// VCS type for this backend.
    fn vcs_type(&self) -> VcsType;
    /// Name of the primary workspace that lives in the original repo directory
    /// (e.g. `"default"` for jj, `"main-worktree"` for git).
    fn main_workspace_name(&self) -> &'static str;

    fn preview_log(
        &self,
        _repo_dir: &Path,
        _worktree_dir: &Path,
        _ws_name: &str,
        _limit: usize,
    ) -> String {
        String::new()
    }

    fn preview_diff_stat(&self, _repo_dir: &Path, _worktree_dir: &Path, _ws_name: &str) -> String {
        String::new()
    }
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
    bail!(
        "no jj or git repository found in {} or any parent directory",
        dir.display()
    )
}

/// Detect VCS from a dwm repo directory by reading the `.vcs-type` file.
/// Defaults to jj for backward compatibility if the file doesn't exist.
pub fn detect_from_dwm_dir(repo_dir: &Path) -> Result<Box<dyn VcsBackend>> {
    let vcs_type = read_vcs_type(repo_dir)?;
    Ok(vcs_type.to_backend())
}

/// Read the VcsType from a dwm repo directory's `.vcs-type` file.
/// Defaults to Jj for backward compatibility if the file doesn't exist.
pub fn read_vcs_type(repo_dir: &Path) -> Result<VcsType> {
    let vcs_file = repo_dir.join(".vcs-type");
    if vcs_file.exists() {
        let content = std::fs::read_to_string(&vcs_file)
            .with_context(|| format!("could not read {}", vcs_file.display()))?;
        content
            .trim()
            .parse::<VcsType>()
            .with_context(|| format!("in {}", vcs_file.display()))
    } else {
        Ok(VcsType::Jj)
    }
}

/// Parse the full output of `jj diff --stat` or `git diff --stat`, extracting
/// the summary line at the end.
pub fn parse_diff_stat(output: &str) -> Result<DiffStat> {
    if let Some(last_line) = output.lines().last()
        && let Some(stat) = parse_diff_stat_line(last_line)
    {
        return Ok(stat);
    }
    Ok(DiffStat::default())
}

/// Parse a single diff summary line such as
/// `"3 files changed, 10 insertions(+), 5 deletions(-)"`.
/// Returns `None` if the line does not look like a summary line.
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
    fn vcs_type_from_str_jj() {
        assert_eq!("jj".parse::<VcsType>().unwrap(), VcsType::Jj);
    }

    #[test]
    fn vcs_type_from_str_git() {
        assert_eq!("git".parse::<VcsType>().unwrap(), VcsType::Git);
    }

    #[test]
    fn vcs_type_from_str_unknown() {
        assert!("svn".parse::<VcsType>().is_err());
    }

    #[test]
    fn vcs_type_display_roundtrip() {
        assert_eq!(
            VcsType::Jj.to_string().parse::<VcsType>().unwrap(),
            VcsType::Jj
        );
        assert_eq!(
            VcsType::Git.to_string().parse::<VcsType>().unwrap(),
            VcsType::Git
        );
    }

    #[test]
    fn vcs_type_to_backend_jj() {
        let backend = VcsType::Jj.to_backend();
        assert_eq!(backend.vcs_type(), VcsType::Jj);
    }

    #[test]
    fn vcs_type_to_backend_git() {
        let backend = VcsType::Git.to_backend();
        assert_eq!(backend.vcs_type(), VcsType::Git);
    }

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
    fn repo_dir_name_same_path_is_stable() {
        let path = std::path::Path::new("/home/user/projects/myrepo");
        assert_eq!(repo_dir_name(path), repo_dir_name(path));
    }

    #[test]
    fn repo_dir_name_starts_with_basename() {
        let path = std::path::Path::new("/home/user/myrepo");
        let dir_name = repo_dir_name(path);
        assert!(dir_name.starts_with("myrepo-"), "dir_name: {}", dir_name);
    }

    #[test]
    fn repo_dir_name_differs_for_same_basename_different_paths() {
        let path_a = std::path::Path::new("/work/a/myrepo");
        let path_b = std::path::Path::new("/work/b/myrepo");
        assert_ne!(repo_dir_name(path_a), repo_dir_name(path_b));
    }

    #[test]
    fn detect_jj_priority_over_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let backend = detect(dir.path()).unwrap();
        assert_eq!(backend.vcs_type(), VcsType::Jj);
    }

    #[test]
    fn detect_git_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let backend = detect(dir.path()).unwrap();
        assert_eq!(backend.vcs_type(), VcsType::Git);
    }

    #[test]
    fn detect_jj_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        let backend = detect(dir.path()).unwrap();
        assert_eq!(backend.vcs_type(), VcsType::Jj);
    }

    #[test]
    fn detect_no_vcs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path()).is_err());
    }

    #[test]
    fn detect_from_dwm_dir_defaults_to_jj() {
        let dir = tempfile::tempdir().unwrap();
        let backend = detect_from_dwm_dir(dir.path()).unwrap();
        assert_eq!(backend.vcs_type(), VcsType::Jj);
    }

    #[test]
    fn detect_from_dwm_dir_reads_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".vcs-type"), "git").unwrap();
        let backend = detect_from_dwm_dir(dir.path()).unwrap();
        assert_eq!(backend.vcs_type(), VcsType::Git);
    }

    #[test]
    fn detect_from_dwm_dir_reads_jj() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".vcs-type"), "jj").unwrap();
        let backend = detect_from_dwm_dir(dir.path()).unwrap();
        assert_eq!(backend.vcs_type(), VcsType::Jj);
    }

    #[test]
    fn detect_from_dwm_dir_unknown_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".vcs-type"), "svn").unwrap();
        assert!(detect_from_dwm_dir(dir.path()).is_err());
    }
}
