use anyhow::Result;
use std::io::IsTerminal;

fn shell_function() -> &'static str {
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

pub fn print_shell_setup() -> Result<()> {
    println!("{}", shell_function());
    if std::io::stdout().is_terminal() {
        eprintln!("# Add this to your shell rc file:");
        eprintln!("#   eval \"$(dwm shell-setup)\"");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_function_defines_dwm() {
        let fn_str = shell_function();
        assert!(
            fn_str.starts_with("dwm() {"),
            "must define a dwm() shell function"
        );
        assert!(fn_str.ends_with('}'), "must close the function body");
    }

    #[test]
    fn shell_function_uses_command_to_bypass_wrapper() {
        // `command dwm` bypasses the shell function to call the real binary,
        // preventing infinite recursion.
        assert!(
            shell_function().contains("command dwm"),
            "must use `command dwm` to avoid recursing into the wrapper"
        );
    }

    #[test]
    fn shell_function_cds_on_directory_output() {
        let fn_str = shell_function();
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
    fn shell_function_echoes_non_directory_output() {
        assert!(
            shell_function().contains("echo \"$output\""),
            "non-directory output must be printed through to the user"
        );
    }

    #[test]
    fn shell_function_propagates_exit_code() {
        let fn_str = shell_function();
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
    fn shell_function_is_valid_posix_ish() {
        let fn_str = shell_function();
        // Basic structural checks: balanced braces, uses `local` for variables
        let open = fn_str.matches('{').count();
        let close = fn_str.matches('}').count();
        assert_eq!(open, close, "braces must be balanced");

        // All variables should be declared local to avoid polluting the caller's env
        assert!(fn_str.contains("local output"));
        assert!(fn_str.contains("local exit_code"));
    }

    #[test]
    fn print_shell_setup_succeeds() {
        // Smoke test: the function should not error
        print_shell_setup().expect("print_shell_setup should succeed");
    }
}
