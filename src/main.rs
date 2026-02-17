mod cli;
#[allow(dead_code)]
mod jj;
mod names;
mod shell;
mod tui;
mod workspace;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::New { name, at } => workspace::new_workspace(name, at.as_deref()),
        Commands::List => {
            let entries = workspace::list_workspace_entries()?;
            match tui::run_picker(entries)? {
                Some(tui::PickerResult::Selected(path)) => println!("{}", path),
                Some(tui::PickerResult::CreateNew(name)) => workspace::new_workspace(name, None)?,
                None => {}
            }
            Ok(())
        }
        Commands::Delete { name } => workspace::delete_workspace(name),
        Commands::ShellSetup => shell::print_shell_setup(),
    }
}
