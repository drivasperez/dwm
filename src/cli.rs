use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "dwm", about = "Dan's Workspace Manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_defaults_to_list() {
        let cli = Cli::try_parse_from(["dwm"]).unwrap();
        assert!(
            cli.command.is_none(),
            "no subcommand should yield None (defaults to list)"
        );
    }

    #[test]
    fn explicit_list_subcommand() {
        let cli = Cli::try_parse_from(["dwm", "list"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::List { all: false })));
    }

    #[test]
    fn list_all_flag() {
        let cli = Cli::try_parse_from(["dwm", "list", "--all"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::List { all: true })));
    }

    #[test]
    fn help_flag_is_recognized() {
        let err = Cli::try_parse_from(["dwm", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn subcommand_help_is_recognized() {
        let err = Cli::try_parse_from(["dwm", "new", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn new_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "new", "my-ws"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::New { name: Some(n), at: None }) if n == "my-ws")
        );
    }

    #[test]
    fn new_with_at_flag() {
        let cli = Cli::try_parse_from(["dwm", "new", "--at", "abc123"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::New { name: None, at: Some(r) }) if r == "abc123")
        );
    }

    #[test]
    fn delete_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "delete", "foo"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Delete { name: Some(n) }) if n == "foo"));
    }

    #[test]
    fn switch_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "switch", "ws-name"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Switch { name }) if name == "ws-name"));
    }

    #[test]
    fn status_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "status"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Status)));
    }

    #[test]
    fn rename_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "rename", "old", "new"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::Rename { name, new_name: Some(nn) }) if name == "old" && nn == "new")
        );
    }

    #[test]
    fn shell_setup_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "shell-setup"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::ShellSetup)));
    }

    #[test]
    fn unknown_subcommand_errors() {
        let err = Cli::try_parse_from(["dwm", "bogus"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
