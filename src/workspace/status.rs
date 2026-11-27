use anyhow::Result;
use owo_colors::OwoColorize;
use std::io::Write;

use crate::workspace::rex_ops::session_label_for;
use crate::workspace::types::WorkspaceEntry;

/// Workspaces as JSON for scripts and Rex's command palette. Unlike the
/// table, this goes to stdout.
pub fn print_status_json(entries: &[WorkspaceEntry]) -> Result<()> {
    println!("{}", serde_json::to_string(&status_json(entries))?);
    Ok(())
}

fn status_json(entries: &[WorkspaceEntry]) -> serde_json::Value {
    let workspaces: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "path": e.path,
                "repo": e.repo_name,
                "main_repo_path": e.main_repo_path,
                "is_main": e.is_main,
                "change_id": e.change_id,
                "description": e.description,
                "bookmarks": e.bookmarks,
                "stale": e.is_stale,
                "modified": e
                    .last_modified
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                "diff": e.diff_stat,
                "agents": e.agent_status.as_ref().filter(|s| !s.is_empty()),
                "rex_pane": e.rex_pane,
                "session_label": session_label_for(&e.path),
                "lease": e.lease,
            })
        })
        .collect();
    serde_json::json!({ "workspaces": workspaces })
}

/// Print a non-interactive tabular workspace summary to stderr.
pub fn print_status(entries: &[WorkspaceEntry]) {
    let out = std::io::stderr().lock();
    let _ = print_status_to(entries, out);
}

/// Core logic for printing the status table to any Write implementation.
pub(crate) fn print_status_to<W: Write>(entries: &[WorkspaceEntry], mut out: W) -> Result<()> {
    use crate::workspace::types::format_time_ago;

    // Column widths
    let name_w = entries
        .iter()
        .map(|e| {
            let display = if e.is_main {
                format!("{} (main)", e.name)
            } else {
                e.name.clone()
            };
            display.len()
        })
        .max()
        .unwrap_or(4)
        .max(4);
    let change_w = 8;
    let bookmark_w = entries
        .iter()
        .map(|e| e.bookmarks.join(", ").len())
        .max()
        .unwrap_or(9)
        .max(9);
    let has_agents = entries
        .iter()
        .any(|e| e.agent_status.as_ref().is_some_and(|s| !s.is_empty()));
    let agent_w = if has_agents {
        entries
            .iter()
            .map(|e| {
                e.agent_status
                    .as_ref()
                    .map(|s| s.to_string().len())
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(6)
            .max(6)
    } else {
        0
    };

    // Header
    if has_agents {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "{:<name_w$}  {:<change_w$}  {:<40}  {:<bookmark_w$}  {:<9}  {:<agent_w$}  CHANGES",
                "NAME", "CHANGE", "DESCRIPTION", "BOOKMARKS", "MODIFIED", "AGENTS",
            )
            .bold()
            .dimmed()
        );
    } else {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "{:<name_w$}  {:<change_w$}  {:<40}  {:<bookmark_w$}  {:<9}  CHANGES",
                "NAME", "CHANGE", "DESCRIPTION", "BOOKMARKS", "MODIFIED",
            )
            .bold()
            .dimmed()
        );
    }

    for entry in entries {
        let name_text = if entry.is_main {
            format!("{} (main)", entry.name)
        } else if entry.is_stale {
            format!("{} [stale]", entry.name)
        } else {
            entry.name.clone()
        };

        let dim = entry.is_stale;
        let name_colored = {
            let s = format!("{:<name_w$}", name_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.cyan().to_string()
            }
        };

        let change_colored = {
            let s = format!("{:<change_w$}", entry.change_id);
            if dim {
                s.dimmed().to_string()
            } else {
                s.magenta().to_string()
            }
        };

        let desc = entry.description.lines().next().unwrap_or("");
        let desc_text: String = desc.chars().take(40).collect();
        let desc_colored = {
            let s = format!("{:<40}", desc_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.white().to_string()
            }
        };

        let bookmarks_text = entry.bookmarks.join(", ");
        let bookmarks_colored = {
            let s = format!("{:<bookmark_w$}", bookmarks_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.blue().to_string()
            }
        };

        let time_text = format_time_ago(entry.last_modified);
        let time_colored = {
            let s = format!("{:<9}", time_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.yellow().to_string()
            }
        };

        let stat = &entry.diff_stat;
        let changes_text = if stat.files_changed == 0 && stat.insertions == 0 && stat.deletions == 0
        {
            "clean".to_string()
        } else {
            let mut parts = Vec::new();
            if stat.insertions > 0 {
                parts.push(format!("+{}", stat.insertions));
            }
            if stat.deletions > 0 {
                parts.push(format!("-{}", stat.deletions));
            }
            if parts.is_empty() {
                format!("{} files", stat.files_changed)
            } else {
                parts.join(" ")
            }
        };

        let changes_colored = if dim {
            changes_text.dimmed().to_string()
        } else if stat.deletions > stat.insertions {
            changes_text.red().to_string()
        } else if stat.insertions > 0 {
            changes_text.green().to_string()
        } else {
            changes_text.dimmed().to_string()
        };

        if has_agents {
            let agent_colored = match &entry.agent_status {
                Some(summary) if !summary.is_empty() => {
                    let text = format!("{:<agent_w$}", summary);
                    if dim {
                        text.dimmed().to_string()
                    } else {
                        match summary.most_urgent() {
                            Some(crate::agent::AgentStatus::Waiting) => text.yellow().to_string(),
                            Some(crate::agent::AgentStatus::Working) => text.green().to_string(),
                            Some(crate::agent::AgentStatus::Done) => text.cyan().to_string(),
                            _ => text.dimmed().to_string(),
                        }
                    }
                }
                _ => format!("{:<agent_w$}", ""),
            };

            let _ = writeln!(
                out,
                "{}  {}  {}  {}  {}  {}  {}",
                name_colored,
                change_colored,
                desc_colored,
                bookmarks_colored,
                time_colored,
                agent_colored,
                changes_colored,
            );
        } else {
            let _ = writeln!(
                out,
                "{}  {}  {}  {}  {}  {}",
                name_colored,
                change_colored,
                desc_colored,
                bookmarks_colored,
                time_colored,
                changes_colored,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::ProgramState::Working;
    use crate::rex::tests::{FakeRex, pane};
    use crate::workspace::layout::dwm_dir;
    use crate::workspace::list::list_workspace_entries_inner;
    use crate::workspace::test_utils::{
        make_repo_with_vcs, pool_fixture, rex_fixture, ws, ws_name,
    };
    use crate::{pool, vcs};
    use std::fs;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn print_status_to_string(entries: &[WorkspaceEntry]) -> String {
        owo_colors::set_override(true);
        let mut buf = Vec::new();
        print_status_to(entries, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn print_status_does_not_panic() {
        let entries = vec![
            WorkspaceEntry {
                name: "default".to_string(),
                path: PathBuf::from("/tmp/repo"),
                last_modified: Some(SystemTime::now()),
                diff_stat: vcs::DiffStat {
                    files_changed: 1,
                    insertions: 10,
                    deletions: 2,
                },
                is_main: true,
                change_id: "abc12345".to_string(),
                description: "main workspace".to_string(),
                bookmarks: vec!["main".to_string()],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
                rex_pane: None,
                lease: None,
            },
            WorkspaceEntry {
                name: "feat-x".to_string(),
                path: PathBuf::from("/tmp/feat-x"),
                last_modified: None,
                diff_stat: vcs::DiffStat::default(),
                is_main: false,
                change_id: "def67890".to_string(),
                description: "feature work".to_string(),
                bookmarks: vec![],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
                rex_pane: None,
                lease: None,
            },
        ];
        // Should not panic; output goes to stderr
        print_status(&entries);
    }

    #[test]
    fn status_table_snapshot() {
        // Use fixed times relative to "now" for format_time_ago
        let now = SystemTime::now();
        let t_5m = now - std::time::Duration::from_secs(300);
        let t_2h = now - std::time::Duration::from_secs(7200);

        let entries = vec![
            WorkspaceEntry {
                name: "default".to_string(),
                path: PathBuf::from("/tmp/repo"),
                last_modified: Some(t_5m),
                diff_stat: vcs::DiffStat {
                    files_changed: 1,
                    insertions: 10,
                    deletions: 2,
                },
                is_main: true,
                change_id: "abc12345".to_string(),
                description: "refactor help system".to_string(),
                bookmarks: vec!["main".to_string()],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
                rex_pane: None,
                lease: None,
            },
            WorkspaceEntry {
                name: "hazy-quail".to_string(),
                path: PathBuf::from("/tmp/hazy-quail"),
                last_modified: Some(t_2h),
                diff_stat: vcs::DiffStat {
                    files_changed: 5,
                    insertions: 100,
                    deletions: 50,
                },
                is_main: false,
                change_id: "tqqorvwl".to_string(),
                description: "Live-updating list view".to_string(),
                bookmarks: vec![],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: Some(crate::agent::AgentSummary {
                    waiting: 1,
                    working: 0,
                    done: 0,
                    idle: 0,
                    notes: Vec::new(),
                }),
                rex_pane: None,
                lease: None,
            },
        ];

        let out = print_status_to_string(&entries);

        assert!(out.contains("NAME"));
        assert!(out.contains("default (main)"));
        assert!(out.contains("abc12345"));
        assert!(out.contains("refactor help system"));
        assert!(out.contains("main"));
        assert!(out.contains("5m ago"));
        assert!(out.contains("+10 -2"));

        assert!(out.contains("hazy-quail"));
        assert!(out.contains("tqqorvwl"));
        assert!(out.contains("Live-updating list view"));
        assert!(out.contains("2h ago"));
        assert!(out.contains("1 waiting"));
        assert!(out.contains("+100 -50"));

        // Verify ANSI codes are present (cyan for names)
        assert!(out.contains("\x1b[36m"));
    }

    #[test]
    fn status_json_includes_rex_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(vec![pane("p1", "a", &a, &[Working])], true);
        let (_, deps, _) = rex_fixture(tmp.path(), &fake);

        let entries = list_workspace_entries_inner(&deps).unwrap();
        let json = status_json(&entries);
        let list = json["workspaces"].as_array().unwrap();
        let a_json = list.iter().find(|w| w["name"] == "a").unwrap();
        assert_eq!(a_json["rex_pane"]["block_id"], "p1");
        assert_eq!(a_json["agents"]["working"], 1);
        assert_eq!(a_json["session_label"], "a");
        assert_eq!(a_json["is_main"], false);
        let main = list.iter().find(|w| w["is_main"] == true).unwrap();
        assert_eq!(main["session_label"], "myrepo");
        assert!(main["agents"].is_null());
        assert!(main["rex_pane"].is_null());
    }

    #[test]
    fn status_json_omits_leases_whose_rex_pane_closed() {
        // Regression: `status --json` reported a lease as held after its
        // pane closed, while `lease` and `prune` already treated it as ended.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let fake = FakeRex::new(vec![pane("block:alive", "x", &main_repo, &[])], true);
        let (main_repo, deps, _) = pool_fixture(
            tmp.path(),
            Some(&fake),
            &[
                ("agents-1", vcs::PruneVerdict::Prunable),
                ("agents-2", vcs::PruneVerdict::Prunable),
                ("agents-3", vcs::PruneVerdict::Prunable),
            ],
        );
        let dwm = dwm_dir(&main_repo);
        pool::write_lease(
            &dwm,
            &pool::Lease::new(&ws_name("agents-1"), Some("block:gone")),
        )
        .unwrap();
        pool::write_lease(
            &dwm,
            &pool::Lease::new(&ws_name("agents-2"), Some("block:alive")),
        )
        .unwrap();
        pool::write_lease(&dwm, &pool::Lease::new(&ws_name("agents-3"), None)).unwrap();

        let entries = list_workspace_entries_inner(&deps).unwrap();
        let json = status_json(&entries);
        let lease_of = |name: &str| {
            json["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["name"] == name)
                .unwrap()["lease"]
                .clone()
        };
        assert!(lease_of("agents-1").is_null(), "its pane closed");
        assert_eq!(lease_of("agents-2")["rex_block"], "block:alive");
        assert_eq!(lease_of("agents-3")["workspace"], "agents-3");
    }
}
