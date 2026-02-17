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

    match cli.command {
        Commands::New { name, at } => workspace::new_workspace(name, at.as_deref()),
        Commands::List => {
            loop {
                let entries = workspace::list_workspace_entries()?;
                match tui::run_picker(entries)? {
                    Some(tui::PickerResult::Selected(path)) => {
                        println!("{}", path);
                        break;
                    }
                    Some(tui::PickerResult::CreateNew(name)) => {
                        workspace::new_workspace(name, None)?;
                        break;
                    }
                    Some(tui::PickerResult::Delete(name)) => {
                        let redirected = workspace::delete_workspace(Some(name))?;
                        if redirected {
                            break;
                        }
                        continue;
                    }
                    None => break,
                }
            }
            Ok(())
        }
        Commands::Status => {
            let entries = workspace::list_workspace_entries()?;
            workspace::print_status(&entries);
            Ok(())
        }
        Commands::Delete { name } => workspace::delete_workspace(name).map(|_| ()),
        Commands::ShellSetup => shell::print_shell_setup(),
    }
}
