use anyhow::Result;
use std::io::IsTerminal;
use std::path::PathBuf;

/// Subcommands whose stdout may be a workspace path that the shell wrapper
/// should `cd` into. This is the single source of truth — both the POSIX and
/// fish wrapper generators read from this list.
pub const CD_SUBCOMMANDS: &[&str] = &["new", "list", "switch", "delete", "rename"];

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

    fn function_output(&self) -> String {
        match self {
            Shell::Fish => fish_function(),
            Shell::Bash | Shell::Zsh => posix_function(),
        }
    }
}

/// Returns the POSIX shell function definition that wraps the `dwm` binary.
/// Subcommands listed in [`CD_SUBCOMMANDS`] (plus the bare invocation) capture
/// stdout and `cd` into the result. All other subcommands run directly.
fn posix_function() -> String {
    let cases = CD_SUBCOMMANDS.join("|");
    format!(
        r#"dwm() {{
    case "$1" in
        {cases}|"")
            local dir
            dir="$(command dwm "$@")" || return $?
            [ -n "$dir" ] && cd "$dir"
            ;;
        *)
            command dwm "$@"
            ;;
    esac
}}"#
    )
}

/// Returns the fish shell function definition that wraps the `dwm` binary.
fn fish_function() -> String {
    let cases = CD_SUBCOMMANDS.join(" ");
    format!(
        r#"function dwm
    switch "$argv[1]"
        case {cases} ""
            set -l dir (command dwm $argv)
            or return $status
            if test -n "$dir"
                cd "$dir"; or return 1
            end
        case '*'
            command dwm $argv
    end
end"#
    )
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

    // --- POSIX (bash/zsh) wrapper structure tests ---

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
    fn posix_function_includes_all_cd_subcommands() {
        let fn_str = posix_function();
        for sub in CD_SUBCOMMANDS {
            assert!(
                fn_str.contains(sub),
                "posix wrapper must include cd subcommand '{sub}'"
            );
        }
    }

    #[test]
    fn posix_function_passes_other_subcommands_through() {
        let fn_str = posix_function();
        assert!(
            fn_str.contains("*)\n            command dwm \"$@\""),
            "non-cd subcommands must pass through directly"
        );
    }

    #[test]
    fn posix_function_propagates_exit_code() {
        assert!(
            posix_function().contains("|| return $?"),
            "must propagate exit code on failure"
        );
    }

    #[test]
    fn posix_function_is_valid_posix_ish() {
        let fn_str = posix_function();
        let open = fn_str.matches('{').count();
        let close = fn_str.matches('}').count();
        assert_eq!(open, close, "braces must be balanced");
        assert!(fn_str.contains("local dir"));
    }

    // --- Fish wrapper structure tests ---

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
    fn fish_function_includes_all_cd_subcommands() {
        let fn_str = fish_function();
        for sub in CD_SUBCOMMANDS {
            assert!(
                fn_str.contains(sub),
                "fish wrapper must include cd subcommand '{sub}'"
            );
        }
    }

    #[test]
    fn fish_function_passes_other_subcommands_through() {
        let fn_str = fish_function();
        assert!(fn_str.contains("case '*'"), "must have a catch-all case");
        assert!(
            fn_str.contains("command dwm $argv"),
            "non-cd subcommands must pass through directly"
        );
    }

    #[test]
    fn fish_function_propagates_exit_code() {
        assert!(
            fish_function().contains("or return $status"),
            "must propagate exit code on failure"
        );
    }

    #[test]
    fn fish_function_uses_set_for_variables() {
        assert!(
            fish_function().contains("set -l dir"),
            "must use set -l for local variables"
        );
    }

    // --- POSIX wrapper integration tests (require bash) ---

    fn bash_available() -> bool {
        std::process::Command::new("bash")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Eval the POSIX wrapper in bash with a fake `dwm` binary that prints
    /// `target_dir` to stdout. Runs `dwm <args>` then `pwd`, returning the
    /// final working directory.
    fn run_posix_wrapper(args: &str, target_dir: &std::path::Path) -> String {
        let tmp = tempfile::tempdir().unwrap();

        // Create a fake dwm binary that prints the target directory.
        let fake_bin = tmp.path().join("dwm");
        std::fs::write(
            &fake_bin,
            format!("#!/bin/sh\necho '{}'", target_dir.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let wrapper = posix_function();
        let script = format!(
            "export PATH=\"{bin_dir}:$PATH\"\n{wrapper}\ndwm {args}\npwd",
            bin_dir = tmp.path().display(),
        );

        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "bash wrapper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The last line of stdout is `pwd` output (the current directory).
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().last().unwrap_or("").to_string()
    }

    #[test]
    fn posix_wrapper_cds_for_each_cd_subcommand() {
        if !bash_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("workspace");
        std::fs::create_dir(&target).unwrap();

        for sub in CD_SUBCOMMANDS {
            let pwd = run_posix_wrapper(&format!("{sub} some-arg"), &target);
            assert_eq!(
                pwd,
                target.to_str().unwrap(),
                "wrapper must cd after `dwm {sub}`"
            );
        }
    }

    #[test]
    fn posix_wrapper_cds_on_bare_invocation() {
        if !bash_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("workspace");
        std::fs::create_dir(&target).unwrap();

        let pwd = run_posix_wrapper("", &target);
        assert_eq!(
            pwd,
            target.to_str().unwrap(),
            "wrapper must cd on bare (no subcommand) invocation"
        );
    }

    #[test]
    fn posix_wrapper_does_not_cd_for_other_subcommands() {
        if !bash_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("workspace");
        std::fs::create_dir(&target).unwrap();

        for sub in ["version", "status", "shell-setup"] {
            let pwd = run_posix_wrapper(sub, &target);
            assert_ne!(
                pwd,
                target.to_str().unwrap(),
                "wrapper must NOT cd after `dwm {sub}`"
            );
        }
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
