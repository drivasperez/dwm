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
        Commands::New { name } => workspace::new_workspace(name),
        Commands::List => {
            let entries = workspace::list_workspace_entries()?;
            if let Some(path) = tui::run_picker(entries)? {
                println!("{}", path);
            }
            Ok(())
        }
        Commands::Delete { name } => workspace::delete_workspace(name),
        Commands::ShellSetup => shell::print_shell_setup(),
    }
}
