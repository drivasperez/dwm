use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "dwm", about = "Dan's Workspace Manager", version)]
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
        #[arg(long, conflicts_with = "from")]
        at: Option<String>,
        /// Fork from an existing workspace's current change
        #[arg(long, conflicts_with = "at")]
        from: Option<String>,
        /// Change directory in this shell even inside Rex, instead of opening a
        /// Rex session for the workspace
        #[arg(long)]
        here: bool,
        /// Command to run in the new workspace's Rex session, after `--`
        #[arg(last = true, conflicts_with = "here", value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// List workspaces and pick one interactively
    List {
        /// Show workspaces across all repos
        #[arg(long)]
        all: bool,
        /// Change directory in this shell even inside Rex, instead of focusing
        /// or opening a Rex session
        #[arg(long)]
        here: bool,
    },
    /// Print a non-interactive workspace summary
    Status {
        /// Show workspaces across all repos
        #[arg(long)]
        all: bool,
        /// Print JSON to stdout instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Switch to a workspace by name
    Switch {
        /// Workspace name
        name: String,
        /// Change directory in this shell even inside Rex, instead of focusing
        /// or opening a Rex session
        #[arg(long)]
        here: bool,
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
    /// Delete workspaces whose work is safe to discard (dry run unless --yes)
    Prune {
        /// Actually delete prunable workspaces instead of only listing them
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Lease a workspace from a pool (`<pool>-1`, `<pool>-2`, …) and print
    /// its path. Reuses a free member with nothing to lose, reset to a fresh
    /// change, or creates the next one
    Lease {
        /// Pool name; members are named `<pool>-<n>`
        pool: String,
        /// Revision to start from instead of trunk
        #[arg(long)]
        at: Option<String>,
    },
    /// End a workspace's lease (the current one if omitted)
    Release {
        /// Workspace name
        name: Option<String>,
    },
    /// Print the agent skill that teaches coding agents to use dwm
    Skill,
    /// Process a Claude Code hook event (used internally by hooks)
    #[command(name = "hook-handler", hide = true)]
    HookHandler,
    /// Set up Claude Code hooks for agent status tracking
    #[command(name = "agent-setup", hide = true)]
    AgentSetup,
    /// Run interactive setup for shell integration, agent hooks and skill, and Rex
    Setup,
    /// Print the current version
    Version,
    /// Print shell integration wrapper
    #[command(name = "shell-setup", hide = true)]
    ShellSetup {
        /// Emit POSIX (bash/zsh) wrapper
        #[arg(long, group = "shell_type")]
        posix: bool,
        /// Emit bash wrapper (alias for --posix)
        #[arg(long, group = "shell_type")]
        bash: bool,
        /// Emit zsh wrapper (alias for --posix)
        #[arg(long, group = "shell_type")]
        zsh: bool,
        /// Emit fish wrapper
        #[arg(long, group = "shell_type")]
        fish: bool,
    },
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
        assert!(matches!(
            cli.command,
            Some(Commands::List {
                all: false,
                here: false
            })
        ));
    }

    #[test]
    fn list_all_flag() {
        let cli = Cli::try_parse_from(["dwm", "list", "--all"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::List {
                all: true,
                here: false
            })
        ));
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
            matches!(cli.command, Some(Commands::New { name: Some(n), at: None, from: None, .. }) if n == "my-ws")
        );
    }

    #[test]
    fn new_with_at_flag() {
        let cli = Cli::try_parse_from(["dwm", "new", "--at", "abc123"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::New { name: None, at: Some(r), from: None, .. }) if r == "abc123")
        );
    }

    #[test]
    fn new_with_from_flag() {
        let cli = Cli::try_parse_from(["dwm", "new", "--from", "other-ws"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::New { name: None, at: None, from: Some(f), .. }) if f == "other-ws")
        );
    }

    #[test]
    fn new_with_from_and_name() {
        let cli = Cli::try_parse_from(["dwm", "new", "my-ws", "--from", "other-ws"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::New { name: Some(n), at: None, from: Some(f), .. }) if n == "my-ws" && f == "other-ws")
        );
    }

    #[test]
    fn new_at_and_from_conflict() {
        let err = Cli::try_parse_from(["dwm", "new", "--at", "abc", "--from", "ws"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn delete_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "delete", "foo"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Delete { name: Some(n) }) if n == "foo"));
    }

    #[test]
    fn prune_defaults_to_dry_run() {
        let cli = Cli::try_parse_from(["dwm", "prune"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Prune { yes: false })));
    }

    #[test]
    fn prune_yes_flag() {
        for flag in ["--yes", "-y"] {
            let cli = Cli::try_parse_from(["dwm", "prune", flag]).unwrap();
            assert!(matches!(cli.command, Some(Commands::Prune { yes: true })));
        }
    }

    #[test]
    fn switch_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "switch", "ws-name"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::Switch { name, here: false }) if name == "ws-name")
        );
    }

    #[test]
    fn status_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Status {
                all: false,
                json: false
            })
        ));
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
        assert!(matches!(
            cli.command,
            Some(Commands::ShellSetup {
                posix: false,
                bash: false,
                zsh: false,
                fish: false
            })
        ));
    }

    #[test]
    fn shell_setup_fish_flag() {
        let cli = Cli::try_parse_from(["dwm", "shell-setup", "--fish"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::ShellSetup { fish: true, .. })
        ));
    }

    #[test]
    fn shell_setup_bash_flag() {
        let cli = Cli::try_parse_from(["dwm", "shell-setup", "--bash"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::ShellSetup { bash: true, .. })
        ));
    }

    #[test]
    fn shell_setup_zsh_flag() {
        let cli = Cli::try_parse_from(["dwm", "shell-setup", "--zsh"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::ShellSetup { zsh: true, .. })
        ));
    }

    #[test]
    fn shell_setup_posix_flag() {
        let cli = Cli::try_parse_from(["dwm", "shell-setup", "--posix"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::ShellSetup { posix: true, .. })
        ));
    }

    #[test]
    fn shell_setup_mutually_exclusive_flags() {
        let err = Cli::try_parse_from(["dwm", "shell-setup", "--bash", "--fish"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn new_takes_a_command_after_double_dash() {
        let cli =
            Cli::try_parse_from(["dwm", "new", "ws", "--", "claude", "fix the bug", "--flag"])
                .unwrap();
        match cli.command {
            Some(Commands::New {
                name,
                command,
                here,
                ..
            }) => {
                assert_eq!(name.as_deref(), Some("ws"));
                assert_eq!(command, vec!["claude", "fix the bug", "--flag"]);
                assert!(!here);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn new_without_command_has_empty_command() {
        let cli = Cli::try_parse_from(["dwm", "new"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::New { command, .. }) if command.is_empty()));
    }

    #[test]
    fn new_here_conflicts_with_command() {
        let err = Cli::try_parse_from(["dwm", "new", "--here", "--", "claude"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn here_flags_parse() {
        let cli = Cli::try_parse_from(["dwm", "switch", "ws", "--here"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Switch { here: true, .. })
        ));
        let cli = Cli::try_parse_from(["dwm", "list", "--here"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::List { here: true, .. })
        ));
        let cli = Cli::try_parse_from(["dwm", "new", "--here"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::New { here: true, .. })
        ));
    }

    #[test]
    fn status_json_and_all_flags() {
        let cli = Cli::try_parse_from(["dwm", "status", "--all", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Status {
                all: true,
                json: true
            })
        ));
    }

    #[test]
    fn lease_and_release_parse() {
        let cli = Cli::try_parse_from(["dwm", "lease", "agents", "--at", "main"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::Lease { pool, at: Some(at) }) if pool == "agents" && at == "main")
        );
        let cli = Cli::try_parse_from(["dwm", "release"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Release { name: None })
        ));
        let cli = Cli::try_parse_from(["dwm", "release", "agents-2"]).unwrap();
        assert!(
            matches!(cli.command, Some(Commands::Release { name: Some(n) }) if n == "agents-2")
        );
        assert!(
            Cli::try_parse_from(["dwm", "lease"]).is_err(),
            "pool is required"
        );
    }

    #[test]
    fn version_subcommand_parses() {
        let cli = Cli::try_parse_from(["dwm", "version"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Version)));
    }

    #[test]
    fn version_flag_is_recognized() {
        let err = Cli::try_parse_from(["dwm", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn unknown_subcommand_errors() {
        let err = Cli::try_parse_from(["dwm", "bogus"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
