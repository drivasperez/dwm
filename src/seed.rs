//! Plan ignored-file copies before creating a workspace; never traverse symlinks.
use crate::{config::WorkspaceConfig, fs_copy, vcs::VcsType};
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

#[cfg(test)]
mod tests {
    use super::*;
    fn run(root: &Path, program: &str, args: &[&str]) {
        let mut cmd = Command::new(program);
        cmd.current_dir(root).args(args);
        output(cmd).unwrap();
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

fn ignored(root: &Path, vcs: VcsType, candidates: &BTreeSet<PathBuf>) -> Result<BTreeSet<PathBuf>> {
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut cmd = Command::new("git");
    cmd.current_dir(root);
    if vcs == VcsType::Jj {
        // jj documents Git-compatible ignore rules, including the underlying
        // repository's info/exclude and core.excludesFile. Its index is NOT
        // authoritative, so check-ignore uses --no-index and tracked() uses jj.
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
        // Validate all conflicts before copying any files.
        for path in &self.files {
            ensure!(
                destination_ignored.contains(path),
                "copy path is not ignored at destination revision: {}",
                path.display()
            );
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
        for path in &self.files {
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
