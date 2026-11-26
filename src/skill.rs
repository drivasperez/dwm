//! The agent skill that teaches coding agents how to use dwm.
//!
//! `dwm skill` prints it, and `dwm setup` offers to install it as a Claude
//! Code skill and keeps an installed copy current on later runs.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::fs;
use std::path::{Path, PathBuf};

/// The skill, as installed.
pub const SKILL: &str = include_str!("skill/SKILL.md");

/// Text that marks a SKILL.md as dwm's own, so setup never overwrites a skill
/// someone wrote by hand at the same path.
const MARKER: &str = "Installed by `dwm setup`";

/// Print the skill to stdout.
pub fn print_skill() {
    print!("{SKILL}");
}

/// Where Claude Code looks for personal skills: `$CLAUDE_CONFIG_DIR/skills`,
/// or `~/.claude/skills`.
fn skills_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => Some(PathBuf::from(dir).join("skills")),
        None => dirs::home_dir().map(|h| h.join(".claude").join("skills")),
    }
}

/// What setup should do about the skill file at some path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Install {
    /// Nothing there yet: ask before installing.
    Offer,
    /// dwm's skill, already current.
    Current,
    /// dwm's skill from another version: rewrite it without asking.
    Update,
    /// Someone else's file: leave it alone.
    Foreign,
}

fn install_action(existing: Option<&str>) -> Install {
    match existing {
        None => Install::Offer,
        Some(text) if text == SKILL => Install::Current,
        Some(text) if text.contains(MARKER) => Install::Update,
        Some(_) => Install::Foreign,
    }
}

fn write_skill(path: &Path) -> Result<()> {
    let dir = path.parent().context("skill path has no directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(path, SKILL).with_context(|| format!("writing {}", path.display()))
}

/// Offer to install the skill for Claude Code, or update an installed copy.
pub fn setup_skill_interactive() -> Result<()> {
    let Some(dir) = skills_dir() else {
        eprintln!(
            "  {} Could not find Claude Code's skills directory",
            "-".dimmed()
        );
        return Ok(());
    };
    let path = dir.join("dwm").join("SKILL.md");
    let display = crate::agent::display_path(&path);
    match install_action(fs::read_to_string(&path).ok().as_deref()) {
        Install::Current => {
            eprintln!("  {} Installed in {}", "✓".green(), display.dimmed());
        }
        Install::Update => {
            write_skill(&path)?;
            eprintln!("  {} Updated {}", "✓".green(), display.dimmed());
        }
        Install::Foreign => {
            eprintln!(
                "  {} {} exists and wasn't written by dwm; leaving it. `dwm skill` prints dwm's version.",
                "-".dimmed(),
                display
            );
        }
        Install::Offer => {
            eprint!(
                "  {} Install a skill that teaches agents to use dwm ({})? [y/N] ",
                "?".bold().cyan(),
                display.bold()
            );
            if crate::agent::confirm_from_tty()? {
                write_skill(&path)?;
                eprintln!("  {} Installed to {}", "✓".green(), display.dimmed());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_has_claude_code_frontmatter() {
        let mut lines = SKILL.lines();
        assert_eq!(lines.next(), Some("---"), "frontmatter must open the file");
        let front: Vec<&str> = lines.by_ref().take_while(|l| *l != "---").collect();
        assert!(front.contains(&"name: dwm"), "{front:?}");
        let description = front
            .iter()
            .find_map(|l| l.strip_prefix("description: "))
            .expect("a description");
        // Claude Code truncates long descriptions in its skill listing.
        assert!(description.len() <= 1024, "{} chars", description.len());
        assert!(SKILL.contains(MARKER), "setup must recognise its own file");
    }

    #[test]
    fn skill_documents_the_commands_agents_use() {
        for cmd in [
            "dwm lease",
            "dwm release",
            "dwm status --json",
            "--at",
            "dwm delete",
        ] {
            assert!(SKILL.contains(cmd), "skill should mention {cmd}");
        }
    }

    /// Every `dwm <subcommand>` written as code in the skill, from fenced
    /// blocks and inline code spans.
    fn commands_in_skill() -> Vec<String> {
        let mut out = Vec::new();
        let mut fenced = false;
        for line in SKILL.lines() {
            if line.starts_with("```") {
                fenced = !fenced;
                continue;
            }
            let mut spans: Vec<&str> = Vec::new();
            if fenced {
                spans.push(line);
            } else {
                spans.extend(line.split('`').skip(1).step_by(2));
            }
            for span in spans {
                if let Some(rest) = span.trim().strip_prefix("dwm ") {
                    let sub: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_lowercase())
                        .collect();
                    if !sub.is_empty() {
                        out.push(sub);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn skill_mentions_only_real_subcommands() {
        // Guards against the skill drifting from the CLI.
        use clap::CommandFactory;
        let cli = crate::cli::Cli::command();
        let known: Vec<&str> = cli.get_subcommands().map(|c| c.get_name()).collect();
        let used = commands_in_skill();
        assert!(used.contains(&"lease".to_string()), "{used:?}");
        for sub in used {
            assert!(
                known.contains(&sub.as_str()),
                "unknown subcommand `dwm {sub}`"
            );
        }
    }

    #[test]
    fn install_action_respects_files_it_did_not_write() {
        assert_eq!(install_action(None), Install::Offer);
        assert_eq!(install_action(Some(SKILL)), Install::Current);
        let older = SKILL.replace("dwm lease agents ", "dwm lease old ");
        assert_eq!(install_action(Some(&older)), Install::Update);
        assert_eq!(
            install_action(Some("---\nname: dwm\n---\nmy own notes")),
            Install::Foreign
        );
    }

    #[test]
    fn write_skill_creates_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skills/dwm/SKILL.md");
        write_skill(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL);
    }
}
