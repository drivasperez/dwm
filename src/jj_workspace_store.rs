//! Keeps jj's record of a workspace's location correct after dwm moves it.
//!
//! jj records each workspace's root directory in
//! `<repo>/.jj/repo/workspace_store/index`, a protobuf message defined in jj's
//! `simple_workspace_store.proto`:
//!
//! ```text
//! message Workspace  { string name = 1; bytes path = 2; }
//! message Workspaces { repeated Workspace workspaces = 1; }
//! ```
//!
//! `jj workspace rename` changes the name but keeps the path, and no jj command
//! records a new path for an existing workspace, so after dwm moves a workspace
//! directory `jj workspace root --name <ws>` would keep reporting the old one.
//! This module rewrites that single path, taking the same `index.lock` flock
//! that jj takes and replacing the file atomically, as jj does.
//!
//! The decoder only accepts the fields above. Anything else means jj changed
//! the format, and the caller leaves the file alone.

use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// One `Workspace` message from the index.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    path: Vec<u8>,
}

/// The only protobuf wire type the index uses (length-delimited).
const WIRE_TYPE_LEN: u64 = 2;

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let Some(&byte) = buf.get(*pos) else {
            bail!("truncated varint");
        };
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("varint too long")
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Split a message into `(field_number, bytes)` pairs, rejecting any field
/// that isn't length-delimited or whose number isn't in `allowed`.
fn read_fields<'a>(buf: &'a [u8], allowed: &[u64]) -> Result<Vec<(u64, &'a [u8])>> {
    let mut pos = 0;
    let mut fields = Vec::new();
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos)?;
        let (field, wire_type) = (key >> 3, key & 7);
        if wire_type != WIRE_TYPE_LEN || !allowed.contains(&field) {
            bail!("unexpected field {field} with wire type {wire_type}");
        }
        let len = usize::try_from(read_varint(buf, &mut pos)?)?;
        let end = pos.checked_add(len).filter(|&end| end <= buf.len());
        let Some(end) = end else {
            bail!("field {field} overruns the message");
        };
        fields.push((field, &buf[pos..end]));
        pos = end;
    }
    Ok(fields)
}

fn write_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    write_varint(out, (field << 3) | WIRE_TYPE_LEN);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn decode(buf: &[u8]) -> Result<Vec<Entry>> {
    read_fields(buf, &[1])?
        .into_iter()
        .map(|(_, workspace)| {
            let mut entry = Entry {
                name: String::new(),
                path: Vec::new(),
            };
            for (field, bytes) in read_fields(workspace, &[1, 2])? {
                match field {
                    1 => entry.name = String::from_utf8(bytes.to_vec())?,
                    _ => entry.path = bytes.to_vec(),
                }
            }
            Ok(entry)
        })
        .collect()
}

/// Encode as prost does for proto3: empty fields are omitted.
fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        let mut workspace = Vec::new();
        if !entry.name.is_empty() {
            write_field(&mut workspace, 1, entry.name.as_bytes());
        }
        if !entry.path.is_empty() {
            write_field(&mut workspace, 2, &entry.path);
        }
        write_field(&mut out, 1, &workspace);
    }
    out
}

/// Return the index with `name`'s path replaced by `path`, or `None` if the
/// index has no entry for `name`.
fn with_workspace_path(index: &[u8], name: &str, path: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut entries = decode(index)?;
    let Some(entry) = entries.iter_mut().find(|e| e.name == name) else {
        return Ok(None);
    };
    entry.path = path.to_vec();
    Ok(Some(encode(&entries)))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Result<Vec<u8>> {
    match path.to_str() {
        Some(s) => Ok(s.as_bytes().to_vec()),
        None => bail!("path is not valid UTF-8: {}", path.display()),
    }
}

/// Resolve the shared repo directory (`.jj/repo` in the main workspace) from
/// any workspace root. In a secondary workspace `.jj/repo` is a file holding
/// that directory's path, relative to the workspace's `.jj/` if not absolute.
fn repo_dir(workspace_root: &Path) -> Result<PathBuf> {
    let jj_dir = workspace_root.join(".jj");
    let repo = jj_dir.join("repo");
    if repo.is_dir() {
        return Ok(repo);
    }
    let target =
        fs::read_to_string(&repo).with_context(|| format!("reading {}", repo.display()))?;
    Ok(jj_dir.join(target.trim_end_matches(['\n', '\r'])))
}

/// Take jj's exclusive lock on `lock_path`, which jj may delete on release.
fn lock(lock_path: &Path) -> Result<File> {
    loop {
        let file =
            File::create(lock_path).with_context(|| format!("opening {}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        // jj removes the lock file when it releases the lock. If that
        // happened while we were waiting, we hold a lock on an unlinked file
        // that nobody else can see, so start again on a fresh one.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if file.metadata()?.nlink() == 0 {
                continue;
            }
        }
        return Ok(file);
    }
}

/// Record `workspace_root` as the location of workspace `name` in jj's index.
///
/// A repo created before jj had a workspace store, or one that doesn't list
/// `name`, is left untouched.
pub fn record_workspace_path(workspace_root: &Path, name: &str) -> Result<()> {
    let store = repo_dir(workspace_root)?.join("workspace_store");
    let index_path = store.join("index");
    if !index_path.exists() {
        return Ok(());
    }
    let root = fs::canonicalize(workspace_root)
        .with_context(|| format!("resolving {}", workspace_root.display()))?;

    let _lock = lock(&store.join("index.lock"))?;
    let index =
        fs::read(&index_path).with_context(|| format!("reading {}", index_path.display()))?;
    let updated = with_workspace_path(&index, name, &path_bytes(&root)?)
        .with_context(|| format!("decoding {}", index_path.display()))?;
    let Some(updated) = updated else {
        return Ok(());
    };

    let mut tmp = tempfile::NamedTempFile::new_in(&store)?;
    tmp.write_all(&updated)?;
    tmp.persist(&index_path)
        .with_context(|| format!("writing {}", index_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, path: &str) -> Entry {
        Entry {
            name: name.to_string(),
            path: path.as_bytes().to_vec(),
        }
    }

    /// An index for two workspaces, laid out byte for byte as jj 0.38 writes it.
    const JJ_INDEX: &[u8] = b"\x0a\x13\x0a\x07default\x12\x08/src/ghg\
\x0a\x20\x0a\x04feat\x12\x18/src/ghg.dwm/worktrees/f";

    #[test]
    fn decodes_index_written_by_jj() {
        assert_eq!(
            decode(JJ_INDEX).unwrap(),
            vec![
                entry("default", "/src/ghg"),
                entry("feat", "/src/ghg.dwm/worktrees/f")
            ]
        );
    }

    #[test]
    fn encode_round_trips_jj_bytes() {
        assert_eq!(encode(&decode(JJ_INDEX).unwrap()), JJ_INDEX);
    }

    #[test]
    fn empty_index_has_no_entries() {
        assert_eq!(decode(b"").unwrap(), vec![]);
        assert_eq!(encode(&[]), b"");
    }

    #[test]
    fn varint_lengths_over_127_round_trip() {
        let long = format!("/{}", "x".repeat(300));
        let entries = vec![entry("ws", &long)];
        assert_eq!(decode(&encode(&entries)).unwrap(), entries);
    }

    #[test]
    fn replaces_only_the_named_workspace() {
        let updated = with_workspace_path(JJ_INDEX, "feat", b"/elsewhere/feat")
            .unwrap()
            .unwrap();
        assert_eq!(
            decode(&updated).unwrap(),
            vec![
                entry("default", "/src/ghg"),
                entry("feat", "/elsewhere/feat")
            ]
        );
    }

    #[test]
    fn missing_workspace_is_not_an_error() {
        assert_eq!(with_workspace_path(JJ_INDEX, "nope", b"/x").unwrap(), None);
    }

    #[test]
    fn rejects_unknown_fields() {
        // Field 2 at the top level isn't part of `Workspaces`.
        assert!(decode(b"\x12\x00").is_err());
        // A varint field (wire type 0) inside a `Workspace`.
        assert!(decode(b"\x0a\x02\x18\x01").is_err());
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(decode(&JJ_INDEX[..JJ_INDEX.len() - 1]).is_err());
        assert!(decode(b"\x0a").is_err());
    }

    #[test]
    fn repo_dir_follows_secondary_workspace_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("main/.jj/repo");
        fs::create_dir_all(&main_repo).unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(ws.join(".jj")).unwrap();

        fs::write(ws.join(".jj/repo"), main_repo.to_str().unwrap()).unwrap();
        assert_eq!(repo_dir(&ws).unwrap(), main_repo);
        assert_eq!(repo_dir(&tmp.path().join("main")).unwrap(), main_repo);

        fs::write(ws.join(".jj/repo"), "../../main/.jj/repo").unwrap();
        assert_eq!(
            fs::canonicalize(repo_dir(&ws).unwrap()).unwrap(),
            fs::canonicalize(&main_repo).unwrap()
        );
    }
}
