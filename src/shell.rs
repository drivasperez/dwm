use anyhow::Result;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// Returns the path to the shell's config file.
    fn config_path(&self) -> PathBuf {
        let home = dirs::home_dir().expect("could not determine home directory");
        match self {
            Shell::Fish => {
                if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
                    PathBuf::from(xdg).join("fish/config.fish")
                } else {
                    home.join(".config/fish/config.fish")
                }
            }
            Shell::Zsh => home.join(".zshrc"),
            Shell::Bash => home.join(".bashrc"),
        }
    }

    /// Returns the line that should be appended to the config file.
    fn setup_line(&self) -> &'static str {
        match self {
            Shell::Fish => "dwm shell-setup --fish | source",
            Shell::Bash | Shell::Zsh => r#"eval "$(dwm shell-setup)""#,
        }
    }

    fn function_output(&self) -> &'static str {
        match self {
            Shell::Fish => fish_function(),
            Shell::Bash | Shell::Zsh => posix_function(),
        }
    }
}

/// Returns the POSIX shell function definition that wraps the `dwm` binary.
/// When the binary prints a directory path to stdout the wrapper `cd`s into it;
/// otherwise it passes the output through to the caller.
fn posix_function() -> &'static str {
    r#"dwm() {
    local output
    output="$(command dwm "$@")"
    local exit_code=$?
    if [ $exit_code -eq 0 ] && [ -n "$output" ] && [ -d "$output" ]; then
        cd "$output" || return 1
    elif [ -n "$output" ]; then
        echo "$output"
    fi
    return $exit_code
}"#
}

/// Returns the fish shell function definition that wraps the `dwm` binary.
fn fish_function() -> &'static str {
    r#"function dwm
    set -l output (command dwm $argv)
    set -l exit_code $status
    if test $exit_code -eq 0 -a -n "$output" -a -d "$output"
        cd "$output"; or return 1
    else if test -n "$output"
        echo "$output"
    end
    return $exit_code
end"#
}

/// Detect the parent shell from environment variables.
fn detect_shell() -> Option<Shell> {
    // Check shell-specific version env vars first (most reliable).
    if std::env::var("FISH_VERSION").is_ok() {
        return Some(Shell::Fish);
    }
    if std::env::var("ZSH_VERSION").is_ok() {
        return Some(Shell::Zsh);
    }
    if std::env::var("BASH_VERSION").is_ok() {
        return Some(Shell::Bash);
    }
    // Fall back to $SHELL (login shell).
    if let Ok(shell) = std::env::var("SHELL") {
        if shell.ends_with("/fish") {
            return Some(Shell::Fish);
        }
        if shell.ends_with("/zsh") {
            return Some(Shell::Zsh);
        }
        if shell.ends_with("/bash") {
            return Some(Shell::Bash);
        }
    }
    None
}

fn display_config_path(path: &std::path::Path) -> String {
    if let Ok(home) = std::env::var("HOME")
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Offer to append the setup line to the user's shell config file.
/// Returns `true` if the hint should be suppressed (already installed or just installed).
fn offer_install(shell: Shell) -> Result<bool> {
    let config = shell.config_path();
    let setup_line = shell.setup_line();
    let display = display_config_path(&config);

    // Check if already present.
    if config.exists() {
        let contents = std::fs::read_to_string(&config)?;
        if contents.contains(setup_line) {
            eprintln!("# Already installed in {display}");
            return Ok(true);
        }
    }

    // Prompt the user. Read from /dev/tty so this works even if stdin is redirected.
    eprint!("Add to {display}? [y/N] ");
    let tty = std::fs::File::open("/dev/tty");
    let response = match tty {
        Ok(f) => {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut line)?;
            line
        }
        Err(_) => String::new(),
    };

    if response.trim().eq_ignore_ascii_case("y") {
        // Ensure parent directory exists (relevant for fish config).
        if let Some(parent) = config.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config)?;
        // Add a newline before the setup line if the file doesn't end with one.
        let needs_newline = config.exists() && {
            let contents = std::fs::read_to_string(&config)?;
            !contents.is_empty() && !contents.ends_with('\n')
        };
        if needs_newline {
            writeln!(file)?;
        }
        writeln!(file, "{setup_line}")?;
        eprintln!("# Added to {display}");
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Print the shell integration wrapper to stdout.
///
/// When stdout is a terminal and we can detect the shell, offer to auto-install
/// the setup line into the user's config file. Otherwise, show a hint.
pub fn print_shell_setup(shell: Option<Shell>) -> Result<()> {
    let effective = shell.or_else(detect_shell);

    match effective {
        Some(s) => {
            println!("{}", s.function_output());
            if std::io::stdout().is_terminal() {
                let installed = offer_install(s)?;
                if !installed {
                    // Show the manual hint.
                    match s {
                        Shell::Fish => {
                            eprintln!("# Add this to your fish config:");
                            eprintln!("#   {}", s.setup_line());
                        }
                        Shell::Bash | Shell::Zsh => {
                            eprintln!("# Add this to your shell rc file:");
                            eprintln!("#   {}", s.setup_line());
                        }
                    }
                }
            }
        }
        None => {
            // Can't detect shell, emit posix and show generic hint.
            println!("{}", posix_function());
            if std::io::stdout().is_terminal() {
                eprintln!("# Add this to your shell rc file:");
                eprintln!("#   eval \"$(dwm shell-setup)\"");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- POSIX (bash/zsh) tests ---

    #[test]
    fn posix_function_defines_dwm() {
        let fn_str = posix_function();
        assert!(
            fn_str.starts_with("dwm() {"),
            "must define a dwm() shell function"
        );
        assert!(fn_str.ends_with('}'), "must close the function body");
    }

    #[test]
    fn posix_function_uses_command_to_bypass_wrapper() {
        assert!(
            posix_function().contains("command dwm"),
            "must use `command dwm` to avoid recursing into the wrapper"
        );
    }

    #[test]
    fn posix_function_cds_on_directory_output() {
        let fn_str = posix_function();
        assert!(
            fn_str.contains("-d \"$output\""),
            "must test whether output is a directory"
        );
        assert!(
            fn_str.contains("cd \"$output\""),
            "must cd into directory output"
        );
    }

    #[test]
    fn posix_function_echoes_non_directory_output() {
        assert!(
            posix_function().contains("echo \"$output\""),
            "non-directory output must be printed through to the user"
        );
    }

    #[test]
    fn posix_function_propagates_exit_code() {
        let fn_str = posix_function();
        assert!(
            fn_str.contains("local exit_code=$?"),
            "must capture the exit code"
        );
        assert!(
            fn_str.contains("return $exit_code"),
            "must propagate the exit code from the real binary"
        );
    }

    #[test]
    fn posix_function_is_valid_posix_ish() {
        let fn_str = posix_function();
        let open = fn_str.matches('{').count();
        let close = fn_str.matches('}').count();
        assert_eq!(open, close, "braces must be balanced");

        assert!(fn_str.contains("local output"));
        assert!(fn_str.contains("local exit_code"));
    }

    // --- Fish tests ---

    #[test]
    fn fish_function_defines_dwm() {
        let fn_str = fish_function();
        assert!(
            fn_str.starts_with("function dwm"),
            "must define a fish dwm function"
        );
        assert!(fn_str.ends_with("end"), "must close with end");
    }

    #[test]
    fn fish_function_uses_command_to_bypass_wrapper() {
        assert!(
            fish_function().contains("command dwm"),
            "must use `command dwm` to avoid recursing into the wrapper"
        );
    }

    #[test]
    fn fish_function_cds_on_directory_output() {
        let fn_str = fish_function();
        assert!(
            fn_str.contains("-d \"$output\""),
            "must test whether output is a directory"
        );
        assert!(
            fn_str.contains("cd \"$output\""),
            "must cd into directory output"
        );
    }

    #[test]
    fn fish_function_echoes_non_directory_output() {
        assert!(
            fish_function().contains("echo \"$output\""),
            "non-directory output must be printed through to the user"
        );
    }

    #[test]
    fn fish_function_propagates_exit_code() {
        let fn_str = fish_function();
        assert!(
            fn_str.contains("set -l exit_code $status"),
            "must capture the exit code"
        );
        assert!(
            fn_str.contains("return $exit_code"),
            "must propagate the exit code from the real binary"
        );
    }

    #[test]
    fn fish_function_uses_set_for_variables() {
        let fn_str = fish_function();
        assert!(
            fn_str.contains("set -l output"),
            "must use set -l for local variables"
        );
        assert!(
            fn_str.contains("set -l exit_code"),
            "must use set -l for local variables"
        );
    }

    // --- Shell enum method tests ---

    #[test]
    fn config_path_fish_default() {
        // Clear XDG_CONFIG_HOME to test default path.
        let _guard = temp_env::with_var("XDG_CONFIG_HOME", None::<&str>, || {
            let path = Shell::Fish.config_path();
            assert!(path.ends_with(".config/fish/config.fish"));
        });
    }

    #[test]
    fn config_path_fish_xdg() {
        temp_env::with_var("XDG_CONFIG_HOME", Some("/tmp/xdg-test"), || {
            let path = Shell::Fish.config_path();
            assert_eq!(path, PathBuf::from("/tmp/xdg-test/fish/config.fish"));
        });
    }

    #[test]
    fn config_path_zsh() {
        let path = Shell::Zsh.config_path();
        assert!(path.ends_with(".zshrc"));
    }

    #[test]
    fn config_path_bash() {
        let path = Shell::Bash.config_path();
        assert!(path.ends_with(".bashrc"));
    }

    #[test]
    fn setup_line_fish() {
        assert_eq!(Shell::Fish.setup_line(), "dwm shell-setup --fish | source");
    }

    #[test]
    fn setup_line_bash() {
        assert!(Shell::Bash.setup_line().contains("eval"));
    }

    #[test]
    fn setup_line_zsh() {
        assert!(Shell::Zsh.setup_line().contains("eval"));
    }

    // --- detect_shell tests ---

    #[test]
    fn detect_shell_fish_version() {
        temp_env::with_vars(
            [
                ("FISH_VERSION", Some("3.7.0")),
                ("ZSH_VERSION", None),
                ("BASH_VERSION", None),
            ],
            || {
                assert_eq!(detect_shell(), Some(Shell::Fish));
            },
        );
    }

    #[test]
    fn detect_shell_zsh_version() {
        temp_env::with_vars(
            [
                ("FISH_VERSION", None),
                ("ZSH_VERSION", Some("5.9")),
                ("BASH_VERSION", None),
            ],
            || {
                assert_eq!(detect_shell(), Some(Shell::Zsh));
            },
        );
    }

    #[test]
    fn detect_shell_bash_version() {
        temp_env::with_vars(
            [
                ("FISH_VERSION", None),
                ("ZSH_VERSION", None),
                ("BASH_VERSION", Some("5.2.0")),
            ],
            || {
                assert_eq!(detect_shell(), Some(Shell::Bash));
            },
        );
    }

    #[test]
    fn detect_shell_from_shell_env() {
        temp_env::with_vars(
            [
                ("FISH_VERSION", None),
                ("ZSH_VERSION", None),
                ("BASH_VERSION", None),
                ("SHELL", Some("/usr/bin/zsh")),
            ],
            || {
                assert_eq!(detect_shell(), Some(Shell::Zsh));
            },
        );
    }

    // --- print_shell_setup tests ---

    #[test]
    fn print_shell_setup_no_flag_succeeds() {
        print_shell_setup(None).expect("print_shell_setup(None) should succeed");
    }

    #[test]
    fn print_shell_setup_fish_succeeds() {
        print_shell_setup(Some(Shell::Fish)).expect("print_shell_setup(Fish) should succeed");
    }

    #[test]
    fn print_shell_setup_bash_succeeds() {
        print_shell_setup(Some(Shell::Bash)).expect("print_shell_setup(Bash) should succeed");
    }

    #[test]
    fn print_shell_setup_zsh_succeeds() {
        print_shell_setup(Some(Shell::Zsh)).expect("print_shell_setup(Zsh) should succeed");
    }

    // --- function_output tests ---

    #[test]
    fn function_output_fish_returns_fish() {
        assert!(Shell::Fish.function_output().contains("function dwm"));
    }

    #[test]
    fn function_output_bash_returns_posix() {
        assert!(Shell::Bash.function_output().contains("dwm() {"));
    }

    #[test]
    fn function_output_zsh_returns_posix() {
        assert!(Shell::Zsh.function_output().contains("dwm() {"));
    }
}
