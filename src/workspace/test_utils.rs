//! Shared test helpers used across workspace submodules.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::names::WorkspaceName;
use crate::rex::tests::FakeRex;
use crate::vcs;

use super::layout::worktrees_dir;
use super::types::WorkspaceDeps;

/// A call recorded by MockBackend.
#[derive(Debug, Clone)]
pub(crate) enum MockCall {
    Add {
        repo_dir: PathBuf,
        ws_path: PathBuf,
        name: String,
        at: Option<String>,
    },
    Remove {
        repo_dir: PathBuf,
        name: String,
        ws_path: PathBuf,
    },
    Rename {
        old_name: String,
        new_name: String,
    },
    Reset {
        name: String,
        revision: String,
    },
}

/// A mock VCS backend for unit tests.
pub(crate) struct MockBackend {
    /// The root path returned by root_from.
    pub root: PathBuf,
    /// Workspaces returned by workspace_list.
    pub workspaces: Vec<(String, vcs::WorkspaceInfo)>,
    /// Records every mutating call for assertions.
    pub calls: Arc<Mutex<Vec<MockCall>>>,
    /// Verdicts returned by prune_verdict, keyed by workspace name.
    pub prune_verdicts: std::collections::HashMap<String, vcs::PruneVerdict>,
    pub panic_on_diff_stat: bool,
}

impl MockBackend {
    pub fn new(
        root: PathBuf,
        workspaces: Vec<(String, vcs::WorkspaceInfo)>,
    ) -> (Self, Arc<Mutex<Vec<MockCall>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                root,
                workspaces,
                calls: Arc::clone(&calls),
                prune_verdicts: Default::default(),
                panic_on_diff_stat: false,
            },
            calls,
        )
    }

    pub fn with_prune_verdicts(mut self, verdicts: &[(&str, vcs::PruneVerdict)]) -> Self {
        self.prune_verdicts = verdicts
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect();
        self
    }
}

impl vcs::VcsBackend for MockBackend {
    fn root_from(&self, _dir: &std::path::Path) -> anyhow::Result<PathBuf> {
        Ok(self.root.clone())
    }

    fn workspace_list(
        &self,
        _repo_dir: &std::path::Path,
    ) -> anyhow::Result<Vec<(String, vcs::WorkspaceInfo)>> {
        Ok(self.workspaces.clone())
    }

    fn workspace_add(
        &self,
        repo_dir: &std::path::Path,
        ws_path: &std::path::Path,
        name: &str,
        at: Option<&str>,
    ) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(MockCall::Add {
            repo_dir: repo_dir.to_path_buf(),
            ws_path: ws_path.to_path_buf(),
            name: name.to_string(),
            at: at.map(|s| s.to_string()),
        });
        std::fs::create_dir_all(ws_path)?;
        Ok(())
    }

    fn workspace_remove(
        &self,
        repo_dir: &std::path::Path,
        name: &str,
        ws_path: &std::path::Path,
    ) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(MockCall::Remove {
            repo_dir: repo_dir.to_path_buf(),
            name: name.to_string(),
            ws_path: ws_path.to_path_buf(),
        });
        Ok(())
    }

    fn workspace_rename(
        &self,
        _repo_dir: &std::path::Path,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
        old_name: &str,
        new_name: &str,
    ) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(MockCall::Rename {
            old_name: old_name.to_string(),
            new_name: new_name.to_string(),
        });
        std::fs::rename(old_path, new_path)?;
        Ok(())
    }

    fn diff_stat_vs_trunk(
        &self,
        _repo_dir: &std::path::Path,
        _worktree_dir: &std::path::Path,
        _ws_name: &str,
    ) -> anyhow::Result<vcs::DiffStat> {
        assert!(!self.panic_on_diff_stat, "mock diff stat panic");
        Ok(vcs::DiffStat {
            files_changed: 1,
            insertions: 10,
            deletions: 2,
        })
    }

    fn latest_description(
        &self,
        _repo_dir: &std::path::Path,
        _worktree_dir: &std::path::Path,
        _ws_name: &str,
    ) -> String {
        "mock description".to_string()
    }

    fn is_merged_into_trunk(
        &self,
        _repo_dir: &std::path::Path,
        _worktree_dir: &std::path::Path,
        _ws_name: &str,
    ) -> bool {
        false
    }

    fn vcs_type(&self) -> vcs::VcsType {
        vcs::VcsType::Jj
    }

    fn main_workspace_name(&self) -> &'static str {
        "default"
    }

    fn reset_workspace(
        &self,
        _repo_dir: &std::path::Path,
        _worktree_dir: &std::path::Path,
        ws_name: &str,
        revision: &str,
    ) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(MockCall::Reset {
            name: ws_name.to_string(),
            revision: revision.to_string(),
        });
        Ok(())
    }

    fn prune_verdict(
        &self,
        _repo_dir: &std::path::Path,
        _worktree_dir: &std::path::Path,
        ws_name: &str,
    ) -> vcs::PruneVerdict {
        self.prune_verdicts
            .get(ws_name)
            .cloned()
            .unwrap_or_else(|| vcs::PruneVerdict::Keep("mock default".to_string()))
    }
}

pub(super) fn make_fake_repo(dir: &Path, vcs_marker: &str) -> PathBuf {
    let repo = dir.join("myrepo");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(repo.join(vcs_marker)).unwrap();
    repo
}

pub(super) fn make_repo_with_vcs(tmp: &Path) -> PathBuf {
    make_fake_repo(tmp, ".jj")
}

pub(super) fn with_data_dir<R, F: FnOnce() -> R>(dir: &Path, f: F) -> R {
    // Override XDG_DATA_HOME / Application Support so `dirs::data_dir()`
    // returns `dir`.
    #[cfg(target_os = "macos")]
    let var = "HOME";
    #[cfg(not(target_os = "macos"))]
    let var = "XDG_DATA_HOME";

    #[cfg(target_os = "macos")]
    fs::create_dir_all(dir.join("Library/Application Support")).unwrap();

    temp_env::with_var(var, Some(dir.as_os_str()), f)
}

pub(super) fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

pub(super) fn jj_available() -> bool {
    std::process::Command::new("jj")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Initialize a git repo with an initial commit.
/// Returns the canonicalized repo path.
pub(super) fn init_git_repo(dir: &Path) -> PathBuf {
    let dir_str = dir.to_str().unwrap();
    std::process::Command::new("git")
        .args(["init", "-b", "main", dir_str])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-C",
            dir_str,
            "commit",
            "--allow-empty",
            "-m",
            "initial commit",
        ])
        .output()
        .unwrap();
    dir.canonicalize().unwrap()
}

pub(super) fn init_jj_repo(dir: &Path) -> PathBuf {
    let dir_str = dir.to_str().unwrap();
    std::process::Command::new("jj")
        .args(["git", "init", dir_str])
        .output()
        .unwrap();
    // Create a "main" bookmark so trunk() resolves
    std::process::Command::new("jj")
        .args([
            "--repository",
            dir_str,
            "bookmark",
            "create",
            "main",
            "-r",
            "@-",
        ])
        .output()
        .unwrap();
    dir.canonicalize().unwrap()
}

/// Run a command, asserting success, with a fixed identity so commits can
/// be pushed regardless of the developer's own VCS config.
pub(super) fn run_ok(dir: &Path, program: &str, args: &[&str]) -> String {
    let out = std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("JJ_USER", "dwm test")
        .env("JJ_EMAIL", "dwm@example.com")
        .env("GIT_AUTHOR_NAME", "dwm test")
        .env("GIT_AUTHOR_EMAIL", "dwm@example.com")
        .env("GIT_COMMITTER_NAME", "dwm test")
        .env("GIT_COMMITTER_EMAIL", "dwm@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

pub(super) fn ws_name(name: &str) -> WorkspaceName {
    WorkspaceName::parse(name).unwrap()
}

pub(super) fn ws(main_repo: &Path, name: &str) -> PathBuf {
    worktrees_dir(main_repo).join(name)
}

/// A repo whose workspaces are `names`, each with the given verdict.
pub(super) fn pool_fixture(
    tmp: &Path,
    fake: Option<&FakeRex>,
    members: &[(&str, vcs::PruneVerdict)],
) -> (PathBuf, WorkspaceDeps, Arc<Mutex<Vec<MockCall>>>) {
    let main_repo = make_repo_with_vcs(tmp);
    for (name, _) in members {
        fs::create_dir_all(worktrees_dir(&main_repo).join(name)).unwrap();
    }
    let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
    let deps = WorkspaceDeps {
        backend: Box::new(mock.with_prune_verdicts(members)),
        cwd: main_repo.clone(),
        rex: fake.and_then(|f| f.boxed()),
    };
    (main_repo, deps, calls)
}

/// A repo with workspaces `a` and `b`, and the deps to act on it from
/// the main checkout with `fake` as the Rex server.
pub(super) fn rex_fixture(
    tmp: &Path,
    fake: &FakeRex,
) -> (PathBuf, WorkspaceDeps, Arc<Mutex<Vec<MockCall>>>) {
    pool_fixture(
        tmp,
        Some(fake),
        &[
            ("a", vcs::PruneVerdict::Prunable),
            ("b", vcs::PruneVerdict::Prunable),
        ],
    )
}
