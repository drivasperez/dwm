use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use super::layout::dwm_dir;

/// Return the path to the cross-platform repos registry file used by `--all`.
fn registry_path() -> Result<PathBuf> {
    // Allows benchmarks to keep their disposable repositories out of the
    // user's multi-repository dashboard on platforms without XDG data dirs.
    if let Some(path) = std::env::var_os("DWM_REGISTRY_PATH") {
        return Ok(PathBuf::from(path));
    }
    let data = dirs::data_dir().context("could not determine data directory")?;
    Ok(data.join("dwm").join("repos.txt"))
}

/// Append `repo_root` to the registry file if not already present.
///
/// The path is canonicalized so that invocations via a symlinked alias and the
/// real path don't produce duplicate registry entries. Falls back to the
/// original path if canonicalization fails (e.g. it doesn't exist yet).
///
/// Best-effort: errors are returned but the caller may choose to ignore them.
pub(crate) fn register_repo(repo_root: &Path) -> Result<()> {
    let path = registry_path()?;
    let canonical = fs::canonicalize(repo_root)
        .unwrap_or_else(|_| repo_root.to_path_buf())
        .to_string_lossy()
        .into_owned();

    let _lock = lock_registry(&path)?;
    let existing = read_registry(&path)?;
    if existing.lines().any(|l| l.trim() == canonical) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&canonical);
    updated.push('\n');
    write_registry(&path, &updated)
}

/// Take the registry's lock, so concurrent dwm processes don't lose each
/// other's entries between reading the file and writing it back.
fn lock_registry(path: &Path) -> Result<fs::File> {
    let dir = path.parent().context("registry path has no directory")?;
    fs::create_dir_all(dir)?;
    let mut lock_name = path.file_name().unwrap_or_default().to_os_string();
    lock_name.push(".lock");
    let lock_path = dir.join(lock_name);
    let file =
        fs::File::create(&lock_path).with_context(|| format!("opening {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    Ok(file)
}

fn read_registry(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(c) => Ok(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Replace the registry in one step, so a reader never sees it half written.
fn write_registry(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Read the registry, drop entries whose paths no longer exist or are no longer
/// dwm-managed (no `<repo>.dwm/` sibling), rewrite the file with the survivors, and
/// return them.
pub(crate) fn read_and_prune_registry() -> Result<Vec<PathBuf>> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let _lock = lock_registry(&path)?;
    let content = read_registry(&path)?;

    let mut survivors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut changed = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            changed = true;
            continue;
        }
        if !seen.insert(trimmed.to_string()) {
            // duplicate
            changed = true;
            continue;
        }
        let p = PathBuf::from(trimmed);
        if !p.exists() || !dwm_dir(&p).exists() {
            changed = true;
            continue;
        }
        survivors.push(p);
    }

    if changed {
        // Best-effort: listing shouldn't fail because the cleanup couldn't
        // be saved.
        let mut buf = String::new();
        for p in &survivors {
            buf.push_str(&p.to_string_lossy());
            buf.push('\n');
        }
        let _ = write_registry(&path, &buf);
    }

    Ok(survivors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_utils::{make_fake_repo, with_data_dir};

    #[test]
    fn registry_register_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        // Need a <repo>.dwm/ for read_and_prune_registry to keep it.
        fs::create_dir_all(dwm_dir(&repo)).unwrap();

        with_data_dir(tmp.path(), || {
            register_repo(&repo).unwrap();
            register_repo(&repo).unwrap(); // dedupe
            let entries = read_and_prune_registry().unwrap();
            assert_eq!(entries, vec![fs::canonicalize(&repo).unwrap()]);
        });
    }

    #[test]
    fn registry_keeps_every_entry_under_concurrent_updates() {
        // Regression: appends and the prune rewrite weren't serialised, so a
        // `dwm list --all` racing a `dwm new` could drop the new repo.
        let tmp = tempfile::tempdir().unwrap();
        let repos: Vec<PathBuf> = (0..16)
            .map(|i| {
                let repo = make_fake_repo(&tmp.path().join(format!("r{i}")), ".jj");
                fs::create_dir_all(dwm_dir(&repo)).unwrap();
                repo
            })
            .collect();

        with_data_dir(tmp.path(), || {
            std::thread::scope(|s| {
                for repo in &repos {
                    s.spawn(move || register_repo(repo).unwrap());
                    s.spawn(|| read_and_prune_registry().unwrap());
                }
            });
            let mut entries = read_and_prune_registry().unwrap();
            entries.sort();
            let mut want: Vec<PathBuf> =
                repos.iter().map(|r| fs::canonicalize(r).unwrap()).collect();
            want.sort();
            assert_eq!(entries, want);
        });
    }

    #[test]
    fn registry_prunes_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let alive = make_fake_repo(tmp.path(), ".jj");
        fs::create_dir_all(dwm_dir(&alive)).unwrap();
        let dead = tmp.path().join("nonexistent-repo");

        with_data_dir(tmp.path(), || {
            register_repo(&alive).unwrap();
            // Manually add a dead entry
            let path = registry_path().unwrap();
            let mut content = fs::read_to_string(&path).unwrap_or_default();
            content.push_str(&format!("{}\n", dead.display()));
            fs::write(&path, content).unwrap();

            let entries = read_and_prune_registry().unwrap();
            assert_eq!(entries, vec![fs::canonicalize(&alive).unwrap()]);

            // The file should now have been rewritten without the dead entry.
            let after = fs::read_to_string(&path).unwrap();
            assert!(!after.contains(dead.to_string_lossy().as_ref()));
        });
    }

    #[test]
    fn registry_prunes_repo_without_dwm_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj"); // no <repo>.dwm sibling

        with_data_dir(tmp.path(), || {
            register_repo(&repo).unwrap();
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.is_empty());
        });
    }

    #[test]
    fn registry_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        with_data_dir(tmp.path(), || {
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.is_empty());
        });
    }

    #[test]
    #[cfg(unix)]
    fn registry_dedupes_symlinked_and_real_paths() {
        // Regression: invoking dwm once via a symlinked path and once via the
        // real path used to produce two registry entries. Canonicalization in
        // register_repo collapses them to one.
        let tmp = tempfile::tempdir().unwrap();
        let real = make_fake_repo(tmp.path(), ".jj");
        fs::create_dir_all(dwm_dir(&real)).unwrap();

        let link = tmp.path().join("myrepo-symlink");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        with_data_dir(tmp.path(), || {
            register_repo(&link).unwrap();
            register_repo(&real).unwrap();
            let entries = read_and_prune_registry().unwrap();
            assert_eq!(
                entries.len(),
                1,
                "expected single entry, got: {:?}",
                entries
            );
            // The stored entry is the canonical form of `real`.
            let expected = fs::canonicalize(&real).unwrap();
            assert_eq!(entries[0], expected);
        });
    }
}
