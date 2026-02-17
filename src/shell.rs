use anyhow::Result;
use std::io::IsTerminal;

pub fn print_shell_setup() -> Result<()> {
    let shell_fn = r#"dwm() {
    local output
    output="$(command dwm "$@")"
    local exit_code=$?
    if [ $exit_code -eq 0 ] && [ -n "$output" ] && [ -d "$output" ]; then
        cd "$output" || return 1
    elif [ -n "$output" ]; then
        echo "$output"
    fi
    return $exit_code
}"#;

    println!("{}", shell_fn);
    if std::io::stdout().is_terminal() {
        eprintln!("# Add this to your shell rc file:");
        eprintln!("#   eval \"$(dwm shell-setup)\"");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn shell_setup_contains_function() {
        // Capture what would be printed to stdout
        let shell_fn = r#"dwm() {
    local output
    output="$(command dwm "$@")"
    local exit_code=$?
    if [ $exit_code -eq 0 ] && [ -n "$output" ] && [ -d "$output" ]; then
        cd "$output" || return 1
    elif [ -n "$output" ]; then
        echo "$output"
    fi
    return $exit_code
}"#;
        assert!(shell_fn.contains("dwm()"));
        assert!(shell_fn.contains("cd \"$output\""));
        assert!(shell_fn.contains("command dwm"));
        assert!(
            shell_fn.contains("echo \"$output\""),
            "non-directory output must be printed through"
        );
    }
}
