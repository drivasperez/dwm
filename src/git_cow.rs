//! Experimental tracked-file population using an independent Git index.
//! Git's refresh compares private clones against target blobs, so dirty or
//! concurrently edited source files cannot become destination changes.
use crate::{config::CopyMode, fs_copy, seed, vcs::VcsBackend};
use anyhow::{Context, Result, ensure};
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.current_dir(root).args(args);
    seed::output(command)
}

fn text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(root, args)?)?.trim().to_string())
}

fn fallback_reason(root: &Path, target: &str) -> Result<Option<String>> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Ok(Some(
            "native checkout cloning is unsupported on this platform".into(),
        ));
    }
    let config = text(root, &["config", "--list"])?;
    for line in config.lines() {
        let key = line
            .split('=')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if key.starts_with("filter.")
            || matches!(
                key.as_str(),
                "core.attributesfile"
                    | "core.hookspath"
                    | "core.sparsecheckout"
                    | "core.splitindex"
                    | "core.fsmonitor"
                    | "core.ignorestat"
                    | "extensions.worktreeconfig"
            )
            || (key == "core.autocrlf" && !line.ends_with("=false"))
        {
            return Ok(Some(format!("repository setting {key}")));
        }
    }
    for relative in ["info/attributes", "hooks/post-checkout"] {
        let path = text(root, &["rev-parse", "--git-path", relative])?;
        let path = root.join(path);
        if path.exists() {
            return Ok(Some(format!("repository has {relative}")));
        }
    }
    // Attributes at any level can change checkout bytes. Fall back rather than
    // trying to reproduce Git's conversion rules in this experimental path.
    let tree = git(root, &["ls-tree", "-r", "-z", target])?;
    for record in tree.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid git tree record")?;
        if record.starts_with(b"160000 ")
            || record[tab + 1..].rsplit(|b| *b == b'/').next() == Some(b".gitattributes")
        {
            return Ok(Some(
                "target contains submodules or checkout attributes".into(),
            ));
        }
    }
    // Include system/global attributes too. A temporary index lets Git resolve
    // attributes for the target without touching the source's index or files.
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("index");
    let mut read = Command::new("git");
    read.current_dir(root)
        .env("GIT_INDEX_FILE", &index)
        .args(["read-tree", target]);
    seed::output(read)?;
    let mut input = tempfile::tempfile()?;
    use std::io::{Seek, SeekFrom, Write};
    for record in tree.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid tree record")?;
        input.write_all(&record[tab + 1..])?;
        input.write_all(&[0])?;
    }
    input.seek(SeekFrom::Start(0))?;
    let mut attrs = Command::new("git");
    attrs
        .current_dir(root)
        .env("GIT_INDEX_FILE", &index)
        .args(["check-attr", "--cached", "--all", "-z", "--stdin"])
        .stdin(input);
    if !seed::output(attrs)?.is_empty() {
        return Ok(Some("checkout attributes apply to target files".into()));
    }
    Ok(None)
}

pub fn add(
    root: &Path,
    destination: &Path,
    name: &str,
    at: Option<&str>,
    source: &Path,
) -> Result<()> {
    let backend = crate::git::GitBackend::default();
    let target = backend.resolve_revision(root, at.unwrap_or("HEAD"))?;
    if let Some(reason) = fallback_reason(root, &target)? {
        eprintln!("Checkout: standard Git fallback ({reason})");
        return backend.workspace_add(root, destination, name, Some(&target));
    }
    let probe = tempfile::tempdir_in(destination.parent().context("workspace has no parent")?)?;
    let mut supported = false;
    for path in seed::tracked(source, crate::vcs::VcsType::Git)? {
        if fs_copy::check_parents(source, &path).is_ok()
            && fs::symlink_metadata(source.join(&path)).is_ok_and(|m| m.is_file())
        {
            supported = fs_copy::try_clone(&source.join(path), &probe.path().join("clone"))?;
            break;
        }
    }
    drop(probe);
    if !supported {
        eprintln!(
            "Checkout: standard Git fallback (no reusable source or filesystem cloning unavailable)"
        );
        return backend.workspace_add(root, destination, name, Some(&target));
    }
    ensure!(
        fs::symlink_metadata(destination).is_err(),
        "workspace path already exists: {}",
        destination.display()
    );
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args([
            "worktree",
            "add",
            "--no-checkout",
            "--lock",
            "--reason",
            "dwm checkout population",
            "-b",
            name,
        ])
        .arg(destination)
        .arg(&target);
    seed::output(cmd)?;
    let result = populate(destination, source, &target);
    match result {
        Ok(()) => {
            let mut cmd = Command::new("git");
            cmd.current_dir(root).args(["worktree", "unlock"]).arg(destination);
            seed::output(cmd)?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!(
            "checkout population failed; partial workspace retained and locked at {}. Inspect it before using git worktree unlock and removing or repairing it", destination.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn init(root: &Path) {
        fs::create_dir(root).unwrap();
        git(root, &["init", "-q", "-b", "main"]).unwrap();
        git(root, &["config", "user.name", "dwm"]).unwrap();
        git(root, &["config", "user.email", "dwm@example.com"]).unwrap();
        git(root, &["config", "commit.gpgsign", "false"]).unwrap();
    }
    fn commit(root: &Path) {
        git(root, &["add", "."]).unwrap();
        git(root, &["commit", "-qm", "fixture"]).unwrap();
    }
    #[test]
    fn checkout_matches_git_with_dirty_source_and_different_revision() {
        temp_env::with_vars(
            [
                ("GIT_CONFIG_GLOBAL", Some("/dev/null")),
                ("GIT_CONFIG_NOSYSTEM", Some("1")),
            ],
            || {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().join("repo");
                init(&root);
                for file in [
                    "same",
                    "changed",
                    "missing",
                    "with space",
                    "with\ttab",
                    "with\nnewline",
                ] {
                    fs::write(root.join(file), "first\n").unwrap();
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{PermissionsExt, symlink};
                    symlink("same", root.join("link")).unwrap();
                    fs::set_permissions(root.join("same"), fs::Permissions::from_mode(0o755))
                        .unwrap();
                }
                commit(&root);
                let first = text(&root, &["rev-parse", "HEAD"]).unwrap();
                fs::write(root.join("changed"), "second\n").unwrap();
                commit(&root);
                // Both staged and unstaged changes must be excluded.
                fs::write(root.join("same"), "dirty\n").unwrap();
                git(&root, &["add", "same"]).unwrap();
                fs::write(root.join("changed"), "dirty too\n").unwrap();
                fs::remove_file(root.join("missing")).unwrap();
                let before = git(&root, &["status", "--porcelain"]).unwrap();
                let cow = dir.path().join("cow");
                let ordinary = dir.path().join("ordinary");
                add(&root, &cow, "cow", Some(&first), &root).unwrap();
                crate::git::GitBackend::default()
                    .workspace_add(&root, &ordinary, "ordinary", Some(&first))
                    .unwrap();
                assert_eq!(
                    git(&cow, &["status", "--porcelain"]).unwrap(),
                    git(&ordinary, &["status", "--porcelain"]).unwrap()
                );
                assert_eq!(
                    git(&cow, &["ls-files", "--stage", "-z"]).unwrap(),
                    git(&ordinary, &["ls-files", "--stage", "-z"]).unwrap()
                );
                assert_eq!(before, git(&root, &["status", "--porcelain"]).unwrap());
                for path in seed::tracked(&ordinary, crate::vcs::VcsType::Git).unwrap() {
                    if fs::symlink_metadata(ordinary.join(&path))
                        .unwrap()
                        .is_symlink()
                    {
                        assert_eq!(
                            fs::read_link(cow.join(&path)).unwrap(),
                            fs::read_link(ordinary.join(&path)).unwrap()
                        );
                    } else {
                        assert_eq!(
                            fs::read(cow.join(&path)).unwrap(),
                            fs::read(ordinary.join(&path)).unwrap()
                        );
                    }
                }
                fs::write(cow.join("same"), "private edit").unwrap();
                assert_eq!(fs::read_to_string(root.join("same")).unwrap(), "dirty\n");
                commit(&cow);
                let moved = dir.path().join("moved");
                let backend = crate::git::GitBackend::default();
                backend
                    .workspace_rename(&root, &cow, &moved, "cow", "moved")
                    .unwrap();
                backend.workspace_remove(&root, "moved", &moved).unwrap();
                assert!(!moved.exists());
            },
        );
    }

    #[test]
    fn attributes_and_hooks_use_normal_checkout() {
        temp_env::with_vars(
            [
                ("GIT_CONFIG_GLOBAL", Some("/dev/null")),
                ("GIT_CONFIG_NOSYSTEM", Some("1")),
            ],
            || {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().join("repo");
                init(&root);
                fs::write(root.join("file"), "hello\n").unwrap();
                fs::write(root.join(".gitattributes"), "file text eol=crlf\n").unwrap();
                commit(&root);
                let dest = dir.path().join("cow");
                add(&root, &dest, "cow", None, &root).unwrap();
                assert_eq!(fs::read(dest.join("file")).unwrap(), b"hello\r\n");
                assert!(fallback_reason(&root, "HEAD").unwrap().is_some());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let hook = root.join(".git/hooks/post-checkout");
                    fs::write(&hook, "#!/bin/sh\nprintf hook > hook-ran\n").unwrap();
                    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
                    let dest = dir.path().join("hooked");
                    add(&root, &dest, "hooked", None, &root).unwrap();
                    assert_eq!(fs::read_to_string(dest.join("hook-ran")).unwrap(), "hook");
                }
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_population_retains_locked_workspace_and_existing_names_are_safe() {
        temp_env::with_vars(
            [
                ("GIT_CONFIG_GLOBAL", Some("/dev/null")),
                ("GIT_CONFIG_NOSYSTEM", Some("1")),
            ],
            || {
                // Root bypasses file read permissions, so this failure fixture cannot run there.
                if unsafe { libc::geteuid() } == 0 {
                    return;
                }
                use std::os::unix::fs::PermissionsExt;
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().join("repo");
                init(&root);
                fs::write(root.join("a-readable"), "a").unwrap();
                fs::write(root.join("z-unreadable"), "z").unwrap();
                commit(&root);
                let probe = dir.path().join("probe");
                if !fs_copy::try_clone(&root.join("a-readable"), &probe).unwrap() {
                    return;
                }
                fs::set_permissions(root.join("z-unreadable"), fs::Permissions::from_mode(0))
                    .unwrap();
                let destination = dir.path().join("partial");
                let result = add(&root, &destination, "partial", None, &root);
                fs::set_permissions(root.join("z-unreadable"), fs::Permissions::from_mode(0o644))
                    .unwrap();
                assert!(result.is_err());
                assert!(destination.join(".git").exists());
                assert!(
                    text(&root, &["worktree", "list", "--porcelain"])
                        .unwrap()
                        .contains("locked dwm checkout population")
                );
                let existing = dir.path().join("existing");
                fs::create_dir(&existing).unwrap();
                fs::write(existing.join("precious"), "keep").unwrap();
                assert!(add(&root, &existing, "existing", None, &root).is_err());
                assert_eq!(
                    fs::read_to_string(existing.join("precious")).unwrap(),
                    "keep"
                );
            },
        );
    }
}

fn populate(destination: &Path, source: &Path, target: &str) -> Result<()> {
    git(destination, &["read-tree", target])?;
    let entries = seed::tracked(destination, crate::vcs::VcsType::Git)?;
    let mut stats = fs_copy::CopyStats::default();
    let mut cloned = std::collections::BTreeSet::new();
    for path in &entries {
        // A symlink ancestor or missing source is simply not reusable.
        if fs_copy::check_parents(source, path).is_err() {
            continue;
        }
        let source_path = source.join(path);
        match fs::symlink_metadata(&source_path) {
            Ok(meta) if meta.is_file() => (),
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
        let dest = destination.join(path);
        fs_copy::check_parents(destination, path)?;
        fs::create_dir_all(dest.parent().context("missing parent")?)?;
        match fs_copy::copy_file(&source_path, &dest, CopyMode::Auto, &mut stats) {
            Ok(()) => {
                cloned.insert(path.clone());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    // read-tree started a fresh index (no source stat cache). --really-refresh
    // hashes private files against target blobs; code 1 means differences.
    let out = Command::new("git")
        .current_dir(destination)
        .args(["update-index", "--really-refresh"])
        .output()?;
    ensure!(
        matches!(out.status.code(), Some(0 | 1)),
        "refreshing index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let changed = seed::paths(&git(destination, &["diff-files", "--name-only", "-z"])?)?;
    let mut reused = 0;
    for path in &cloned {
        if changed.contains(path) {
            fs::remove_file(destination.join(path))?;
        } else {
            reused += 1;
        }
    }
    // Without --force, checkout-index leaves verified clones in place.
    git(
        destination,
        &["checkout-index", "--all", "--index", "--quiet"],
    )?;
    git(destination, &["update-index", "--really-refresh"])?;
    ensure!(
        git(
            destination,
            &["status", "--porcelain", "--untracked-files=no"]
        )?
        .is_empty(),
        "populated checkout does not match the target revision"
    );
    eprintln!(
        "Checkout: {reused} verified source files reused ({} clone operations, {} copy operations); remaining files populated by Git",
        stats.cloned, stats.copied
    );
    Ok(())
}
