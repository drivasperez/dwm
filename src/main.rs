mod agent;
mod cli;
mod config;
mod fs_copy;
mod git;
mod git_cow;
mod hooks;
#[allow(dead_code)]
mod jj;
mod jj_workspace_store;
mod names;
mod pool;
mod rex;
mod seed;
mod shell;
mod skill;
mod tui;
mod vcs;
mod workspace;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};
use workspace::Placement;

/// Where to take the person once a workspace is chosen. `--here` asks for
/// the current shell; otherwise Rex is used only when someone is at the
/// terminal, so agents and scripts calling dwm never move the Rex app's view.
fn placement(here: bool) -> Placement {
    let requested = if here {
        Placement::Here
    } else {
        Placement::Rex
    };
    let terminal = if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        workspace::Terminal::Interactive
    } else {
        workspace::Terminal::Unattended
    };
    workspace::effective_placement(requested, terminal)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::List {
        all: false,
        here: false,
    }) {
        Commands::New {
            name,
            at,
            from,
            here,
            command,
        } => workspace::new_workspace(
            name,
            at.as_deref(),
            from.as_deref(),
            // A command after `--` explicitly asks for a Rex session.
            if command.is_empty() {
                placement(here)
            } else {
                Placement::Rex
            },
            &command,
        ),
        Commands::List { all, here } => {
            if all {
                let entries = workspace::list_all_workspace_entries()?;
                if let Some(tui::PickerResult::Selected(path)) =
                    tui::run_picker_multi_repo(entries)?
                {
                    workspace::switch_to_path(std::path::Path::new(&path), placement(here))?;
                }
                return Ok(());
            }
            let (entries, repo_dir) = workspace::list_workspace_entries_and_dwm_dir()?;
            match tui::run_picker(
                entries,
                repo_dir,
                |name| {
                    workspace::delete_workspace(
                        Some(name.to_string()),
                        workspace::DeleteOutput::Quiet,
                        workspace::OpenPanes::Refuse,
                    )
                },
                workspace::list_workspace_entries,
            )? {
                Some(tui::PickerResult::Selected(path)) => {
                    workspace::switch_to_path(std::path::Path::new(&path), placement(here))?;
                }
                Some(tui::PickerResult::CreateNew(name)) => {
                    workspace::new_workspace(name, None, None, placement(here), &[])?;
                }
                None => {}
            }
            Ok(())
        }
        Commands::Status { all, json } => {
            let entries = if all {
                workspace::list_all_workspace_entries()?
            } else {
                workspace::list_workspace_entries()?
            };
            if json {
                workspace::print_status_json(&entries)
            } else {
                workspace::print_status(&entries);
                Ok(())
            }
        }
        Commands::Switch { name, here } => workspace::switch_workspace(&name, placement(here)),
        Commands::Rename { name, new_name } => workspace::rename_workspace(name, new_name),
        Commands::Delete { name } => workspace::delete_workspace(
            name,
            workspace::DeleteOutput::Verbose,
            workspace::OpenPanes::Ask,
        )
        .map(|_| ()),
        Commands::Prune { yes } => workspace::prune_workspaces(if yes {
            workspace::PruneMode::Delete
        } else {
            workspace::PruneMode::DryRun
        }),
        Commands::Lease { pool, at } => workspace::lease_workspace(&pool, at.as_deref()),
        Commands::Release { name } => workspace::release_workspace(name),
        Commands::Skill => {
            skill::print_skill();
            Ok(())
        }
        Commands::HookHandler => agent::handle_hook(),
        Commands::AgentSetup => agent::setup_agent_hooks(),
        Commands::Setup => {
            use owo_colors::OwoColorize;
            eprintln!("{}", "dwm setup".bold().cyan());
            eprintln!();
            eprintln!("{}", "Shell integration:".bold().yellow());
            shell::setup_shell_interactive()?;
            eprintln!();
            eprintln!("{}", "Agent status tracking:".bold().yellow());
            if rex::is_active() {
                eprintln!(
                    "  {}",
                    "Inside Rex, dwm reads agent status from Rex. Hooks only add agents running in other terminals."
                        .dimmed()
                );
            }
            agent::setup_agent_hooks()?;
            eprintln!();
            eprintln!("{}", "Agent skill:".bold().yellow());
            skill::setup_skill_interactive()?;
            eprintln!();
            eprintln!("{}", "Rex command palette:".bold().yellow());
            rex::setup_palette_interactive()?;
            Ok(())
        }
        Commands::Version => {
            use owo_colors::OwoColorize;
            println!(
                "{} {}",
                "dwm".bold().cyan(),
                env!("CARGO_PKG_VERSION").bright_white()
            );
            Ok(())
        }
        Commands::ShellSetup {
            posix,
            bash,
            zsh,
            fish,
        } => {
            let shell = if fish {
                Some(shell::Shell::Fish)
            } else if zsh {
                Some(shell::Shell::Zsh)
            } else if posix || bash {
                Some(shell::Shell::Bash)
            } else {
                None
            };
            shell::print_shell_setup(shell)
        }
    }
}
