//! Workspace pools: numbered workspaces (`<pool>-1`, `<pool>-2`, …) that
//! agents and scripts lease, use, and give back, instead of creating and
//! deleting a workspace per task.
//!
//! A lease is a small JSON file in `<repo>.dwm/leases/<workspace>.json`. It
//! ends with `dwm release`, or, when it was taken from a Rex pane, as soon as
//! that pane closes. Leasing happens under an exclusive lock so two callers
//! never get the same workspace.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::rex::Pane;

/// One workspace on loan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub workspace: String,
    /// Seconds since the Unix epoch.
    pub leased_at: u64,
    /// The Rex pane that took the lease, whose closing ends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rex_block: Option<String>,
}

impl Lease {
    pub fn new(workspace: &str, rex_block: Option<&str>) -> Self {
        Self {
            workspace: workspace.to_string(),
            leased_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            rex_block: rex_block.map(String::from),
        }
    }
}

/// Whether a lease still holds its workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Held,
    /// The Rex pane that took it has closed.
    Ended,
}

/// Decide whether `lease` still holds, given the Rex panes when known.
///
/// Without a pane list (outside Rex, or Rex didn't answer) every lease holds:
/// handing out a workspace someone still uses is worse than making a new one.
pub fn lease_state(lease: &Lease, panes: Option<&[Pane]>) -> LeaseState {
    match (&lease.rex_block, panes) {
        (Some(block), Some(panes)) if !panes.iter().any(|p| &p.block_id == block) => {
            LeaseState::Ended
        }
        _ => LeaseState::Held,
    }
}

fn leases_dir(dwm: &Path) -> PathBuf {
    dwm.join("leases")
}

fn lease_path(dwm: &Path, workspace: &str) -> PathBuf {
    leases_dir(dwm).join(format!("{workspace}.json"))
}

/// Hold this while choosing and recording a lease. Released on drop.
pub struct PoolLock {
    _file: File,
}

/// Take the repo's exclusive leasing lock, waiting for other leasers.
pub fn lock(dwm: &Path) -> Result<PoolLock> {
    let dir = leases_dir(dwm);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(".lock");
    let file = File::create(&path).with_context(|| format!("opening {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(PoolLock { _file: file })
}

/// The lease on `workspace`, if there is a readable one.
pub fn read_lease(dwm: &Path, workspace: &str) -> Option<Lease> {
    let text = fs::read_to_string(lease_path(dwm, workspace)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Record a lease, replacing any earlier one on the same workspace.
pub fn write_lease(dwm: &Path, lease: &Lease) -> Result<()> {
    let dir = leases_dir(dwm);
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".tmp-{}.json", lease.workspace));
    fs::write(&tmp, serde_json::to_string(lease)?)?;
    fs::rename(&tmp, lease_path(dwm, &lease.workspace))?;
    Ok(())
}

/// Remove the lease on `workspace`. Returns whether there was one.
pub fn remove_lease(dwm: &Path, workspace: &str) -> Result<bool> {
    match fs::remove_file(lease_path(dwm, workspace)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Move a lease along with a renamed workspace.
pub fn rename_lease(dwm: &Path, old: &str, new: &str) -> Result<()> {
    if let Some(mut lease) = read_lease(dwm, old) {
        lease.workspace = new.to_string();
        write_lease(dwm, &lease)?;
        remove_lease(dwm, old)?;
    }
    Ok(())
}

/// The member number of `name` in `pool`: `3` for `"agents-3"` in `"agents"`.
pub fn member_number(pool: &str, name: &str) -> Option<u32> {
    let digits = name.strip_prefix(pool)?.strip_prefix('-')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // `agents-01` would sort and print ambiguously next to `agents-1`.
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

/// The members of `pool` among `names`, in number order.
pub fn members<'a>(pool: &str, names: impl IntoIterator<Item = &'a str>) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = names
        .into_iter()
        .filter_map(|n| member_number(pool, n).map(|i| (i, n.to_string())))
        .collect();
    out.sort();
    out
}

/// The name for a new member: one past the highest number in use.
pub fn next_member_name(pool: &str, members: &[(u32, String)]) -> String {
    let next = members.iter().map(|(n, _)| *n).max().unwrap_or(0) + 1;
    format!("{pool}-{next}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::tests::pane;

    #[test]
    fn member_number_parses_pool_suffixes() {
        assert_eq!(member_number("agents", "agents-1"), Some(1));
        assert_eq!(member_number("agents", "agents-42"), Some(42));
        assert_eq!(member_number("my-pool", "my-pool-3"), Some(3));
        assert_eq!(member_number("agents", "agents"), None);
        assert_eq!(member_number("agents", "agents-"), None);
        assert_eq!(member_number("agents", "agents-x"), None);
        assert_eq!(member_number("agents", "agents-1a"), None);
        assert_eq!(member_number("agents", "agents-01"), None);
        assert_eq!(member_number("agents", "agentsx-1"), None);
        // A longer pool name sharing the prefix isn't a member.
        assert_eq!(member_number("agents", "agents-big-1"), None);
    }

    #[test]
    fn members_sort_numerically_and_next_name_follows_the_highest() {
        let names = ["agents-10", "other-1", "agents-2", "agents-1", "agents"];
        let m = members("agents", names);
        assert_eq!(
            m,
            vec![
                (1, "agents-1".to_string()),
                (2, "agents-2".to_string()),
                (10, "agents-10".to_string())
            ]
        );
        assert_eq!(next_member_name("agents", &m), "agents-11");
        assert_eq!(next_member_name("agents", &[]), "agents-1");
    }

    #[test]
    fn lease_ends_when_its_rex_pane_closes() {
        let lease = Lease::new("agents-1", Some("block:a"));
        let open = [pane("block:a", "s", Path::new("/x"), &[])];
        let other = [pane("block:b", "s", Path::new("/x"), &[])];
        assert_eq!(lease_state(&lease, Some(&open)), LeaseState::Held);
        assert_eq!(lease_state(&lease, Some(&other)), LeaseState::Ended);
        // Without Rex's view, assume it's still in use.
        assert_eq!(lease_state(&lease, None), LeaseState::Held);
    }

    #[test]
    fn lease_without_a_pane_holds_until_released() {
        let lease = Lease::new("agents-1", None);
        assert_eq!(lease_state(&lease, Some(&[])), LeaseState::Held);
    }

    #[test]
    fn lease_files_round_trip_rename_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let dwm = tmp.path();
        assert_eq!(read_lease(dwm, "a-1"), None);
        let lease = Lease::new("a-1", Some("block:x"));
        write_lease(dwm, &lease).unwrap();
        assert_eq!(read_lease(dwm, "a-1"), Some(lease.clone()));

        rename_lease(dwm, "a-1", "b-1").unwrap();
        assert_eq!(read_lease(dwm, "a-1"), None);
        assert_eq!(read_lease(dwm, "b-1").unwrap().workspace, "b-1");

        assert!(remove_lease(dwm, "b-1").unwrap());
        assert!(!remove_lease(dwm, "b-1").unwrap());
    }

    #[test]
    fn lease_without_block_omits_the_field() {
        let json = serde_json::to_string(&Lease::new("a-1", None)).unwrap();
        assert!(!json.contains("rex_block"), "{json}");
        let back: Lease = serde_json::from_str(r#"{"workspace":"a-1","leased_at":5}"#).unwrap();
        assert_eq!(back.rex_block, None);
    }

    #[test]
    fn lock_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let held = lock(tmp.path()).unwrap();
        let path = tmp.path().join("leases/.lock");
        let other = File::open(&path).unwrap();
        assert!(other.try_lock().is_err(), "a second leaser must wait");
        drop(held);
        assert!(other.try_lock().is_ok());
    }
}
