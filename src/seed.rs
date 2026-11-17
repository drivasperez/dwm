//! Plan ignored-file copies before creating a workspace; never traverse symlinks.
use crate::{
    config::{CopyUnignored, WorkspaceConfig},
    fs_copy,
    vcs::VcsType,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub fn output(mut command: Command) -> Result<Vec<u8>> {
    let out = command.output().context("running VCS command")?;
    ensure!(
        out.status.success(),
        "VCS command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

pub fn paths(bytes: &[u8]) -> Result<BTreeSet<PathBuf>> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt;
                PathBuf::from(std::ffi::OsStr::from_bytes(p))
            };
            #[cfg(not(unix))]
            let path = PathBuf::from(std::str::from_utf8(p)?);
            fs_copy::validate_relative(&path)?;
            Ok(path)
        })
        .collect()
}

pub fn tracked(root: &Path, vcs: VcsType) -> Result<BTreeSet<PathBuf>> {
    let mut cmd = Command::new(if vcs == VcsType::Git { "git" } else { "jj" });
    cmd.current_dir(root);
    if vcs == VcsType::Git {
        cmd.args(["ls-files", "-z"]);
    } else {
        cmd.args([
            "--ignore-working-copy",
            "file",
            "list",
            "-T",
            "path ++ \"\\0\"",
        ]);
    }
    paths(&output(cmd)?)
}

/// A `git` command operating on `root`'s working tree. For jj, this points git
/// at the backing Git repository.
fn git_command(root: &Path, vcs: VcsType) -> Result<Command> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root);
    if vcs == VcsType::Jj {
        // jj documents Git-compatible ignore rules, including the underlying
        // repository's info/exclude and core.excludesFile. Its index is NOT
        // authoritative, so callers must not rely on it for trackedness;
        // tracked() asks jj instead.
        let mut jj = Command::new("jj");
        jj.current_dir(root)
            .args(["--ignore-working-copy", "git", "root"]);
        let git_dir =
            output(jj).context("ignored-file copying requires a Git-backed jj repository")?;
        let git_dir = String::from_utf8(git_dir)?;
        cmd.arg("--git-dir")
            .arg(git_dir.trim())
            .arg("--work-tree")
            .arg(root);
    }
    Ok(cmd)
}

/// The file listing Claude Code's `.worktreeinclude` convention: gitignore
/// syntax, naming ignored files to copy into new worktrees.
pub const WORKTREE_INCLUDE: &str = ".worktreeinclude";

/// Files under `root` matching the patterns in `root/.worktreeinclude`.
///
/// git does the gitignore-syntax matching for us: `ls-files --others
/// --ignored --exclude-from=<file>` lists every untracked file the patterns
/// match, descending into matched directories. Whether a match is actually
/// ignored (the convention only copies ignored files) and untracked is
/// decided later by the same checks `workspace.copy` entries go through.
fn worktree_include(root: &Path, vcs: VcsType) -> Result<BTreeSet<PathBuf>> {
    let include = root.join(WORKTREE_INCLUDE);
    match fs::symlink_metadata(&include) {
        Ok(meta) if meta.is_file() => (),
        Ok(_) => bail!("{WORKTREE_INCLUDE} is not a regular file"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e.into()),
    }
    let mut cmd = git_command(root, vcs)?;
    cmd.args(["ls-files", "-z", "--others", "--ignored", "--exclude-from"])
        .arg(&include);
    let listed = output(cmd).with_context(|| format!("reading {WORKTREE_INCLUDE}"))?;
    Ok(safe_paths(&listed))
}

/// Like [`paths`], but silently drops entries inside VCS/dwm metadata (git
/// walks a jj repository's `.jj/`, which a broad pattern could match) rather
/// than failing.
fn safe_paths(bytes: &[u8]) -> BTreeSet<PathBuf> {
    bytes
        .split(|b| *b == 0)
        .filter_map(|p| paths(p).ok())
        .flatten()
        .collect()
}

fn ignored(root: &Path, vcs: VcsType, candidates: &BTreeSet<PathBuf>) -> Result<BTreeSet<PathBuf>> {
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut cmd = git_command(root, vcs)?;
    let mut input = tempfile::tempfile()?;
    for path in candidates {
        input.write_all(path.as_os_str().as_encoded_bytes())?;
        input.write_all(&[0])?;
    }
    use std::io::{Seek, SeekFrom};
    input.seek(SeekFrom::Start(0))?;
    let out = cmd
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(Stdio::from(input))
        .output()?;
    ensure!(
        matches!(out.status.code(), Some(0 | 1)),
        "checking ignored files: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    paths(&out.stdout)
}

fn collect(
    root: &Path,
    relative: &Path,
    result: &mut BTreeSet<PathBuf>,
    depth: usize,
) -> Result<()> {
    ensure!(depth < 128, "copy directory nesting exceeds 128 levels");
    fs_copy::check_parents(root, relative)?;
    let meta = fs::symlink_metadata(root.join(relative))?;
    if meta.is_dir() {
        for entry in fs::read_dir(root.join(relative))? {
            let path = relative.join(entry?.file_name());
            // Never descend into internal state, including nested repositories.
            if fs_copy::validate_relative(&path).is_ok() {
                collect(root, &path, result, depth + 1)?;
            }
        }
    } else if meta.is_file() || meta.is_symlink() {
        result.insert(relative.to_path_buf());
    } else {
        bail!("unsupported special file: {}", relative.display());
    }
    Ok(())
}

pub struct SeedPlan {
    source: PathBuf,
    files: BTreeSet<PathBuf>,
}

impl SeedPlan {
    pub fn prepare(source: &Path, vcs: VcsType, config: &WorkspaceConfig) -> Result<Self> {
        let mut candidates = BTreeSet::new();
        for path in &config.copy {
            fs_copy::check_parents(source, path)?;
            match collect(source, path, &mut candidates, 0) {
                Ok(()) => (),
                Err(e)
                    if e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    eprintln!("warning: copy source missing: {}", path.display());
                }
                Err(e) => return Err(e),
            }
        }
        candidates.extend(worktree_include(source, vcs)?);
        let mut files = ignored(source, vcs, &candidates)?;
        if !files.is_empty() {
            let tracked = tracked(source, vcs)?;
            files.retain(|p| !tracked.contains(p));
        }
        Ok(Self {
            source: source.to_path_buf(),
            files,
        })
    }

    pub fn apply(&self, destination: &Path, vcs: VcsType, config: &WorkspaceConfig) -> Result<()> {
        if self.files.is_empty() {
            return Ok(());
        }
        let tracked = tracked(destination, vcs)?;
        let destination_ignored = ignored(destination, vcs, &self.files)?;
        let mut files = Vec::with_capacity(self.files.len());
        for path in &self.files {
            if destination_ignored.contains(path) {
                files.push(path);
                continue;
            }
            match config.copy_unignored {
                CopyUnignored::Skip => eprintln!(
                    "warning: not copying {}: it is not ignored at the new workspace's revision",
                    path.display()
                ),
                CopyUnignored::Error => bail!(
                    "copy path is not ignored at destination revision: {} \
                     (set workspace.copy_unignored = \"skip\" to skip such files)",
                    path.display()
                ),
            }
        }
        // Validate all conflicts before copying any files.
        for path in files.iter().copied() {
            fs_copy::check_parents(destination, path)?;
            ensure!(
                !path.ancestors().any(|p| tracked.contains(p))
                    && !tracked
                        .range(path.clone()..)
                        .next()
                        .is_some_and(|p| p.starts_with(path)),
                "copy conflicts with tracked destination: {}",
                path.display()
            );
            ensure!(
                !destination.join(path).try_exists()?
                    && fs::symlink_metadata(destination.join(path)).is_err(),
                "copy destination already exists: {}",
                path.display()
            );
        }
        let mut stats = fs_copy::CopyStats::default();
        for path in files {
            fs_copy::check_parents(&self.source, path)?;
            fs_copy::check_parents(destination, path)?;
            let dest = destination.join(path);
            fs::create_dir_all(dest.parent().context("missing parent")?)?;
            fs_copy::copy_file(&self.source.join(path), &dest, config.copy_mode, &mut stats)
                .with_context(|| format!("copying {}", path.display()))?;
        }
        eprintln!(
            "Copied {} files: {} cloned, {} copied ({} logical bytes)",
            stats.cloned + stats.copied,
            stats.cloned,
            stats.copied,
            stats.bytes
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(root: &Path, program: &str, args: &[&str]) {
        let mut cmd = Command::new(program);
        cmd.current_dir(root).args(args);
        output(cmd).unwrap();
    }

    fn init_repo(root: &Path, vcs: VcsType) {
        fs::create_dir_all(root).unwrap();
        if vcs == VcsType::Git {
            run(root, "git", &["init", "-q", "-b", "main"]);
            run(root, "git", &["config", "user.name", "dwm"]);
            run(root, "git", &["config", "user.email", "dwm@example.com"]);
            run(root, "git", &["config", "commit.gpgsign", "false"]);
        } else {
            run(root, "jj", &["git", "init"]);
        }
    }

    fn commit(root: &Path, vcs: VcsType) {
        if vcs == VcsType::Git {
            run(root, "git", &["add", "."]);
            run(root, "git", &["commit", "-qm", "fixture"]);
        } else {
            run(root, "jj", &["describe", "-m", "fixture"]);
        }
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn worktreeinclude_copies_only_ignored_matches() {
        for vcs in [VcsType::Git, VcsType::Jj] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("source");
            init_repo(&root, vcs);
            write(&root, ".gitignore", "node_modules/\n.env*\n!.env.example\n");
            write(
                &root,
                WORKTREE_INCLUDE,
                "# secrets\n.env*\n!.env.test\nnode_modules/\nconfig/local.json\n",
            );
            write(&root, ".env.example", "tracked");
            commit(&root, vcs);
            write(&root, ".env", "secret");
            write(&root, ".env.test", "excluded by negation");
            write(&root, "node_modules/a/index.js", "dep");
            // Matches .worktreeinclude but is not ignored, so the convention
            // says it must not be copied.
            write(&root, "config/local.json", "{}");
            write(
                &root,
                ".gitignore",
                "node_modules/\n.env*\n!.env.example\ndist/\n",
            );
            write(&root, "dist/out.js", "built");

            let config = WorkspaceConfig::default();
            let plan = SeedPlan::prepare(&root, vcs, &config).unwrap();
            assert_eq!(
                plan.files,
                BTreeSet::from([
                    PathBuf::from(".env"),
                    PathBuf::from("node_modules/a/index.js"),
                ]),
                "{vcs:?}"
            );

            let dest = dir.path().join("dest");
            vcs.to_backend()
                .workspace_add(
                    &root,
                    &dest,
                    "dest",
                    if vcs == VcsType::Jj { Some("@") } else { None },
                )
                .unwrap();
            plan.apply(&dest, vcs, &config).unwrap();
            assert_eq!(fs::read_to_string(dest.join(".env")).unwrap(), "secret");
            assert_eq!(
                fs::read_to_string(dest.join("node_modules/a/index.js")).unwrap(),
                "dep"
            );
            // jj snapshots untracked files into the working-copy commit, so
            // there config/local.json reaches dest by checkout, not by copying.
            if vcs == VcsType::Git {
                assert!(!dest.join("config/local.json").exists());
            }
            assert!(!dest.join("dist").exists());
        }
    }

    /// `.env` is ignored by the source's uncommitted `.gitignore` but not at
    /// the revision the new workspace checks out. By default it is skipped
    /// (the rest still copies); `copy_unignored = "error"` fails before
    /// copying anything.
    #[test]
    fn unignored_at_destination_is_skipped_or_errors() {
        for vcs in [VcsType::Git, VcsType::Jj] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("source");
            init_repo(&root, vcs);
            write(&root, ".gitignore", "cache/\n");
            write(&root, WORKTREE_INCLUDE, ".env\ncache/\n");
            commit(&root, vcs);
            // Base the destination on the committed revision; for jj the
            // .gitignore edit below lands in a fresh working-copy commit.
            let revision = if vcs == VcsType::Jj {
                run(&root, "jj", &["new"]);
                Some("@-")
            } else {
                None
            };
            write(&root, ".gitignore", "cache/\n.env\n");
            write(&root, ".env", "secret");
            write(&root, "cache/blob", "blob");

            let skip = WorkspaceConfig::default();
            let plan = SeedPlan::prepare(&root, vcs, &skip).unwrap();
            assert_eq!(
                plan.files,
                BTreeSet::from([PathBuf::from(".env"), PathBuf::from("cache/blob")]),
                "{vcs:?}"
            );

            let strict_dest = dir.path().join("strict");
            vcs.to_backend()
                .workspace_add(&root, &strict_dest, "strict", revision)
                .unwrap();
            let strict = WorkspaceConfig {
                copy_unignored: CopyUnignored::Error,
                ..Default::default()
            };
            let err = plan.apply(&strict_dest, vcs, &strict).unwrap_err();
            assert!(format!("{err:#}").contains(".env"), "{vcs:?}: {err:#}");
            assert!(!strict_dest.join("cache/blob").exists(), "{vcs:?}");

            let dest = dir.path().join("dest");
            vcs.to_backend()
                .workspace_add(&root, &dest, "dest", revision)
                .unwrap();
            plan.apply(&dest, vcs, &skip).unwrap();
            assert!(!dest.join(".env").exists(), "{vcs:?}");
            assert_eq!(
                fs::read_to_string(dest.join("cache/blob")).unwrap(),
                "blob",
                "{vcs:?}"
            );
        }
    }

    #[test]
    fn worktreeinclude_merges_with_workspace_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("source");
        init_repo(&root, VcsType::Git);
        write(&root, ".gitignore", "target/\n.env\n");
        write(&root, WORKTREE_INCLUDE, ".env\n");
        commit(&root, VcsType::Git);
        write(&root, ".env", "secret");
        write(&root, "target/debug/app", "bin");
        let config = WorkspaceConfig {
            copy: vec!["target".into(), ".env".into()],
            ..Default::default()
        };
        let plan = SeedPlan::prepare(&root, VcsType::Git, &config).unwrap();
        assert_eq!(
            plan.files,
            BTreeSet::from([PathBuf::from(".env"), PathBuf::from("target/debug/app")])
        );
    }

    #[test]
    fn missing_worktreeinclude_copies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), VcsType::Git);
        write(dir.path(), ".gitignore", ".env\n");
        write(dir.path(), ".env", "secret");
        let plan =
            SeedPlan::prepare(dir.path(), VcsType::Git, &WorkspaceConfig::default()).unwrap();
        assert!(plan.files.is_empty());
    }

    /// git walks a jj repository's `.jj/` directory, so a catch-all pattern
    /// must not pull jj's internal state into the copy (or fail on it).
    #[test]
    fn worktreeinclude_never_matches_jj_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root, VcsType::Jj);
        write(root, ".gitignore", "*.log\n");
        write(root, WORKTREE_INCLUDE, "*\n");
        write(root, "build.log", "log");
        let plan = SeedPlan::prepare(root, VcsType::Jj, &WorkspaceConfig::default()).unwrap();
        assert_eq!(plan.files, BTreeSet::from([PathBuf::from("build.log")]));
    }

    #[test]
    fn safe_paths_drops_metadata_entries() {
        assert_eq!(
            safe_paths(b".jj/repo/store\0.env\0sub/.git/HEAD\0\0a/b\0"),
            BTreeSet::from([PathBuf::from(".env"), PathBuf::from("a/b")])
        );
    }

    #[test]
    fn git_and_jj_seed_only_ignored_files_and_reject_conflicts() {
        for vcs in [VcsType::Git, VcsType::Jj] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("source");
            fs::create_dir(&root).unwrap();
            if vcs == VcsType::Git {
                run(&root, "git", &["init", "-q", "-b", "main"]);
                run(&root, "git", &["config", "user.name", "dwm"]);
                run(&root, "git", &["config", "user.email", "dwm@example.com"]);
                run(&root, "git", &["config", "commit.gpgsign", "false"]);
            } else {
                run(&root, "jj", &["git", "init"]);
            }
            fs::write(root.join(".gitignore"), "artifacts/*\n!artifacts/keep\n").unwrap();
            fs::create_dir(root.join("artifacts")).unwrap();
            fs::write(root.join("artifacts/keep"), "tracked").unwrap();
            if vcs == VcsType::Git {
                run(&root, "git", &["add", "."]);
                run(&root, "git", &["commit", "-qm", "fixture"]);
            } else {
                run(&root, "jj", &["describe", "-m", "fixture"]);
            }
            fs::write(root.join("artifacts/cache"), "cached").unwrap();
            let config = WorkspaceConfig {
                copy: vec!["artifacts".into()],
                ..Default::default()
            };
            let plan = SeedPlan::prepare(&root, vcs, &config).unwrap();
            assert_eq!(
                plan.files,
                BTreeSet::from([PathBuf::from("artifacts/cache")])
            );
            let dest = dir.path().join("dest");
            let backend = vcs.to_backend();
            backend
                .workspace_add(
                    &root,
                    &dest,
                    "dest",
                    if vcs == VcsType::Jj { Some("@") } else { None },
                )
                .unwrap();
            plan.apply(&dest, vcs, &config).unwrap();
            assert_eq!(
                fs::read_to_string(dest.join("artifacts/cache")).unwrap(),
                "cached"
            );
            fs::write(dest.join("artifacts/cache"), "private").unwrap();
            assert_eq!(
                fs::read_to_string(root.join("artifacts/cache")).unwrap(),
                "cached"
            );
            assert!(plan.apply(&dest, vcs, &config).is_err());
            assert_eq!(
                fs::read_to_string(dest.join("artifacts/keep")).unwrap(),
                "tracked"
            );
        }
    }
}
