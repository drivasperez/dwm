mod agent;
mod cli;
mod git;
#[allow(dead_code)]
mod jj;
mod names;
mod shell;
mod tui;
mod vcs;
mod workspace;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::List { all: false }) {
        Commands::New { name, at, from } => {
            workspace::new_workspace(name, at.as_deref(), from.as_deref())
        }
        Commands::List { all } => {
            if all {
                let entries = workspace::list_all_workspace_entries()?;
                if let Some(tui::PickerResult::Selected(path)) =
                    tui::run_picker_multi_repo(entries)?
                {
                    println!("{}", path);
                }
                return Ok(());
            }
            let repo_dir = workspace::current_repo_dir()?;
            let entries = workspace::list_workspace_entries()?;
            match tui::run_picker(
                entries,
                repo_dir,
                |name| {
                    workspace::delete_workspace(
                        Some(name.to_string()),
                        workspace::DeleteOutput::Quiet,
                    )
                },
                workspace::list_workspace_entries,
            )? {
                Some(tui::PickerResult::Selected(path)) => println!("{}", path),
                Some(tui::PickerResult::CreateNew(name)) => {
                    workspace::new_workspace(name, None, None)?;
                }
                None => {}
            }
            Ok(())
        }
        Commands::Status => {
            let entries = workspace::list_workspace_entries()?;
            workspace::print_status(&entries);
            Ok(())
        }
        Commands::Switch { name } => workspace::switch_workspace(&name),
        Commands::Rename { name, new_name } => workspace::rename_workspace(name, new_name),
        Commands::Delete { name } => {
            workspace::delete_workspace(name, workspace::DeleteOutput::Verbose).map(|_| ())
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
            agent::setup_agent_hooks()?;
            Ok(())
        }
        Commands::Version => {
            use owo_colors::OwoColorize;
            println!("{} {}", "dwm".bold().cyan(), env!("CARGO_PKG_VERSION").bright_white());
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
