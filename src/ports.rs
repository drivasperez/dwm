//! Per-workspace port range allocator (Conductor-compat `CONDUCTOR_PORT`).
//!
//! Each workspace gets a stable, unique 10-port range. The base of the range
//! is exposed to lifecycle scripts as `CONDUCTOR_PORT`, mirroring Conductor's
//! semantics where `CONDUCTOR_PORT` is the first port of a 10-port slot.
//!
//! ## Design
//!
//! - Allocation state is a single decimal integer in
//!   `<repo>/.dwm/<workspace>/.dwm-port`.
//! - On first call to [`ensure_port`] for a workspace, we scan every other
//!   workspace's `.dwm-port` file in the repo, find the lowest unoccupied
//!   multiple-of-[`SPAN`] starting from [`BASE`], persist it to the marker
//!   file, and return it.
//! - On subsequent calls, we just read the marker file.
//!
//! This keeps the allocator simple and deterministic with no global state and
//! no extra dependencies. It does not actually check whether the host's TCP
//! port is free; that's a runtime concern of the user's `scripts.run` script.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// First port of the lowest range we will hand out.
pub const BASE: u16 = 3000;
/// Number of consecutive ports allocated per workspace.
pub const SPAN: u16 = 10;
/// Filename (within a workspace dir) used to persist the assigned base port.
pub const MARKER_FILENAME: &str = ".dwm-port";

/// Return the base port assigned to `workspace_dir`, allocating one if necessary.
///
/// `workspace_dir` is the on-disk path of the workspace (e.g.
/// `<repo>/.dwm/<name>`). `repo_root` is the original repository root used to
/// scan sibling workspaces for already-allocated ports.
///
/// The returned port is the first of a contiguous [`SPAN`]-port range that the
/// user's lifecycle scripts may use however they like. Allocation is stable: a
/// repeat call returns the same number.
pub fn ensure_port(workspace_dir: &Path, repo_root: &Path) -> Result<u16> {
    let marker = workspace_dir.join(MARKER_FILENAME);
    if let Some(existing) = read_port(&marker)? {
        return Ok(existing);
    }

    let dwm_dir = repo_root.join(".dwm");
    let occupied = scan_occupied(&dwm_dir, &marker)?;
    let chosen = first_free_slot(&occupied);

    // Make sure the workspace dir exists before writing into it. (It usually
    // already does, but during partial setup it may not.)
    fs::create_dir_all(workspace_dir).with_context(|| {
        format!(
            "creating workspace dir for port marker: {}",
            workspace_dir.display()
        )
    })?;
    fs::write(&marker, format!("{}\n", chosen))
        .with_context(|| format!("writing port marker {}", marker.display()))?;
    Ok(chosen)
}

/// Read a `.dwm-port` marker file. Returns `Ok(None)` when missing or
/// unparseable so callers can fall through to fresh allocation.
fn read_port(marker: &Path) -> Result<Option<u16>> {
    match fs::read_to_string(marker) {
        Ok(s) => Ok(s.trim().parse::<u16>().ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", marker.display()))),
    }
}

/// Walk every immediate child of `dwm_dir` and read its `.dwm-port` marker
/// file, returning the set of already-allocated base ports. `self_marker` is
/// excluded so a re-entrant call doesn't see itself as occupied.
fn scan_occupied(dwm_dir: &Path, self_marker: &Path) -> Result<HashSet<u16>> {
    let mut occupied = HashSet::new();
    let read_dir = match fs::read_dir(dwm_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(occupied),
        Err(e) => return Err(anyhow::Error::from(e)),
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let marker = path.join(MARKER_FILENAME);
        if marker == self_marker {
            continue;
        }
        if let Ok(Some(p)) = read_port(&marker) {
            occupied.insert(p);
        }
    }
    Ok(occupied)
}

/// Pick the lowest unoccupied multiple of [`SPAN`] starting from [`BASE`].
fn first_free_slot(occupied: &HashSet<u16>) -> u16 {
    let mut candidate: u32 = BASE as u32;
    while occupied.contains(&(candidate as u16)) {
        candidate += SPAN as u32;
        if candidate > u16::MAX as u32 {
            // Astronomically unlikely; fall back to BASE rather than panic.
            return BASE;
        }
    }
    candidate as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_repo(tmp: &Path) -> std::path::PathBuf {
        let dwm = tmp.join("repo/.dwm");
        fs::create_dir_all(&dwm).unwrap();
        tmp.join("repo")
    }

    #[test]
    fn empty_repo_gets_base() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_repo(tmp.path());
        let ws = repo.join(".dwm/alpha");
        fs::create_dir_all(&ws).unwrap();

        let p = ensure_port(&ws, &repo).unwrap();
        assert_eq!(p, BASE);
        // Marker file should now exist with the same value.
        let written = fs::read_to_string(ws.join(MARKER_FILENAME)).unwrap();
        assert_eq!(written.trim(), BASE.to_string());
    }

    #[test]
    fn second_workspace_gets_next_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_repo(tmp.path());

        let alpha = repo.join(".dwm/alpha");
        fs::create_dir_all(&alpha).unwrap();
        let beta = repo.join(".dwm/beta");
        fs::create_dir_all(&beta).unwrap();

        let pa = ensure_port(&alpha, &repo).unwrap();
        let pb = ensure_port(&beta, &repo).unwrap();
        assert_eq!(pa, BASE);
        assert_eq!(pb, BASE + SPAN);
    }

    #[test]
    fn holes_are_filled() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_repo(tmp.path());

        // Two pre-existing workspaces with 3000 and 3020 occupied (3010 is the hole).
        let a = repo.join(".dwm/aaa");
        let c = repo.join(".dwm/ccc");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&c).unwrap();
        fs::write(a.join(MARKER_FILENAME), "3000\n").unwrap();
        fs::write(c.join(MARKER_FILENAME), "3020\n").unwrap();

        let new_ws = repo.join(".dwm/new");
        fs::create_dir_all(&new_ws).unwrap();
        let p = ensure_port(&new_ws, &repo).unwrap();
        assert_eq!(p, 3010);
    }

    #[test]
    fn idempotent_on_existing_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_repo(tmp.path());
        let ws = repo.join(".dwm/x");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join(MARKER_FILENAME), "4242\n").unwrap();

        let p = ensure_port(&ws, &repo).unwrap();
        assert_eq!(p, 4242, "existing marker should be honoured verbatim");
        // Second call still returns the same.
        let p2 = ensure_port(&ws, &repo).unwrap();
        assert_eq!(p2, 4242);
    }

    #[test]
    fn ignores_self_marker_when_scanning() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_repo(tmp.path());
        let ws = repo.join(".dwm/lonely");
        fs::create_dir_all(&ws).unwrap();
        // No siblings at all → first slot should be BASE.
        let p = ensure_port(&ws, &repo).unwrap();
        assert_eq!(p, BASE);
    }

    #[test]
    fn malformed_marker_does_not_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_repo(tmp.path());

        let weird = repo.join(".dwm/weird");
        fs::create_dir_all(&weird).unwrap();
        fs::write(weird.join(MARKER_FILENAME), "not-a-number").unwrap();

        let new_ws = repo.join(".dwm/new");
        fs::create_dir_all(&new_ws).unwrap();
        // weird's port is unparseable → treated as "not occupying anything", so
        // the new workspace should still get BASE.
        let p = ensure_port(&new_ws, &repo).unwrap();
        assert_eq!(p, BASE);
    }

    #[test]
    fn first_free_slot_picks_base_when_empty() {
        let occupied: HashSet<u16> = HashSet::new();
        assert_eq!(first_free_slot(&occupied), BASE);
    }

    #[test]
    fn first_free_slot_skips_occupied() {
        let mut occupied: HashSet<u16> = HashSet::new();
        occupied.insert(BASE);
        occupied.insert(BASE + SPAN);
        assert_eq!(first_free_slot(&occupied), BASE + SPAN * 2);
    }
}
