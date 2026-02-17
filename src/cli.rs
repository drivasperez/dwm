use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "jjws", about = "JJ Workspace Manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new workspace
    New {
        /// Workspace name (auto-generated if omitted)
        name: Option<String>,
        /// Start from a specific revision instead of @
        #[arg(long)]
        at: Option<String>,
    },
    /// List workspaces and pick one interactively
    List {
        /// Show workspaces across all repos
        #[arg(long)]
        all: bool,
    },
    /// Print a non-interactive workspace summary
    Status,
    /// Rename a workspace
    Rename {
        /// Current workspace name
        old_name: String,
        /// New workspace name
        new_name: String,
    },
    /// Delete a workspace (by name, or the current one if omitted)
    Delete {
        /// Workspace name to delete
        name: Option<String>,
    },
    /// Print shell integration wrapper
    ShellSetup,
}
