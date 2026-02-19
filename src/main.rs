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
        Commands::New { name, at } => workspace::new_workspace(name, at.as_deref()),
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
            let entries = workspace::list_workspace_entries()?;
            match tui::run_picker(
                entries,
                |name| workspace::delete_workspace_quiet(Some(name.to_string())),
                workspace::list_workspace_entries,
            )? {
                Some(tui::PickerResult::Selected(path)) => println!("{}", path),
                Some(tui::PickerResult::CreateNew(name)) => {
                    workspace::new_workspace(name, None)?;
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
        Commands::Delete { name } => workspace::delete_workspace(name).map(|_| ()),
        Commands::ShellSetup => shell::print_shell_setup(),
    }
}
