use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "dwm", about = "Dan's Workspace Manager")]
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
    /// Switch to a workspace by name
    Switch {
        /// Workspace name
        name: String,
    },
    /// Rename a workspace
    Rename {
        /// New name (or old name if two args given)
        name: String,
        /// New name when renaming a different workspace
        new_name: Option<String>,
    },
    /// Delete a workspace (by name, or the current one if omitted)
    Delete {
        /// Workspace name to delete
        name: Option<String>,
    },
    /// Print shell integration wrapper
    ShellSetup,
}
