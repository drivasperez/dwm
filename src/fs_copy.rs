//! Native, independently writable file clones. Never fall back to hard links.
use crate::config::CopyMode;
use std::{
    fs, io,
    path::{Component, Path},
};

#[derive(Debug, Default)]
pub struct CopyStats {
    pub cloned: u64,
    pub copied: u64,
    pub bytes: u64,
}

pub fn validate_relative(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(!path.as_os_str().is_empty(), "empty copy path");
    for part in path.components() {
        match part {
            Component::Normal(name)
                if ![".git", ".jj", ".dwm"]
                    .iter()
                    .any(|n| name.eq_ignore_ascii_case(n)) => {}
            _ => anyhow::bail!("unsafe workspace path: {}", path.display()),
        }
    }
    Ok(())
}

/// Reject symlinked ancestors; the leaf itself may be a symlink.
pub fn check_parents(root: &Path, relative: &Path) -> anyhow::Result<()> {
    validate_relative(relative)?;
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for part in parent.components() {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(meta) => anyhow::ensure!(
                    meta.is_dir() && !meta.is_symlink(),
                    "not a real directory: {}",
                    current.display()
                ),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
        || matches!(
            e.raw_os_error(),
            Some(libc::EXDEV | libc::ENOTSUP | libc::ENOSYS | libc::EINVAL)
        )
}

/// Probe a real source/destination pair without performing a full-copy fallback.
pub fn try_clone(source: &Path, destination: &Path) -> io::Result<bool> {
    match clone_file(source, destination) {
        Ok(()) => Ok(true),
        Err(e) if unsupported(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "macos")]
fn clone_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{
        ffi::CString,
        os::{
            fd::AsRawFd,
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
    };
    let source = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)?;
    if !source.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source changed type",
        ));
    }
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: the source fd is a live regular file; the destination path is
    // NUL-terminated and valid for the call. fclonefileat never overwrites.
    let result =
        unsafe { libc::fclonefileat(source.as_raw_fd(), libc::AT_FDCWD, destination.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn clone_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let source = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)?;
    let dest = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    // SAFETY: both descriptors are live regular files; FICLONE takes a source fd.
    let result = unsafe { libc::ioctl(dest.as_raw_fd(), 0x40049409 as _, source.as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        drop(dest);
        fs::remove_file(destination)?;
        Err(error)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_file(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

pub fn copy_file(
    source: &Path,
    destination: &Path,
    mode: CopyMode,
    stats: &mut CopyStats,
) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symlink copying is unsupported on this platform",
        ));
        stats.copied += 1;
        return Ok(());
    }
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only regular files and symlinks can be copied",
        ));
    }
    let cloned = if mode == CopyMode::Auto {
        try_clone(source, destination)?
    } else {
        false
    };
    let result = (|| {
        if !cloned {
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut input = options.open(source)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            // This operation owns the destination only after create_new succeeds.
            if let Err(e) = io::copy(&mut input, &mut output) {
                let _ = fs::remove_file(destination);
                return Err(e);
            }
        }
        fs::set_permissions(destination, meta.permissions())?;
        let file = fs::File::open(destination)?;
        file.set_times(
            fs::FileTimes::new()
                .set_accessed(meta.accessed()?)
                .set_modified(meta.modified()?),
        )?;
        Ok(())
    })();
    result?;
    stats.bytes += meta.len();
    if cloned {
        stats.cloned += 1;
    } else {
        stats.copied += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_are_independent_and_never_overwrite() {
        for mode in [CopyMode::Auto, CopyMode::Copy] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let dest = dir.path().join("dest");
            fs::write(&source, "original").unwrap();
            let mut stats = CopyStats::default();
            copy_file(&source, &dest, mode, &mut stats).unwrap();
            if mode == CopyMode::Auto && std::env::var_os("DWM_REQUIRE_NATIVE_CLONE").is_some() {
                assert_eq!(
                    stats.cloned, 1,
                    "this test run requires actual native cloning"
                );
            }
            assert!(copy_file(&source, &dest, mode, &mut stats).is_err());
            fs::write(&source, "source edit").unwrap();
            assert_eq!(fs::read_to_string(&dest).unwrap(), "original");
            fs::write(&dest, "dest edit").unwrap();
            assert_eq!(fs::read_to_string(&source).unwrap(), "source edit");
            fs::remove_file(&source).unwrap();
            assert_eq!(fs::read_to_string(&dest).unwrap(), "dest edit");
        }
    }
    #[test]
    fn unsafe_paths_and_error_classification() {
        for path in ["", "/tmp", "../foo", ".git/config", "x/.dwm/y", ".JJ/repo"] {
            assert!(validate_relative(Path::new(path)).is_err(), "{path}");
        }
        assert!(!unsupported(&io::Error::from_raw_os_error(libc::EACCES)));
        assert!(!unsupported(&io::Error::from_raw_os_error(libc::ENOSPC)));
        assert!(unsupported(&io::Error::from_raw_os_error(libc::EXDEV)));
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_and_modes() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, "executable").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        let dest = dir.path().join("dest");
        copy_file(&source, &dest, CopyMode::Auto, &mut CopyStats::default()).unwrap();
        assert_eq!(
            fs::metadata(dest).unwrap().permissions().mode() & 0o777,
            0o755
        );
        symlink("missing", dir.path().join("link")).unwrap();
        copy_file(
            &dir.path().join("link"),
            &dir.path().join("link-copy"),
            CopyMode::Auto,
            &mut CopyStats::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read_link(dir.path().join("link-copy")).unwrap(),
            Path::new("missing")
        );
        assert!(check_parents(dir.path(), Path::new("link/file")).is_err());
    }
}
