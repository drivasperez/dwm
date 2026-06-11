//! Optional integration with terminal multiplexers — currently zellij.
//!
//! When `dwm` is invoked from inside a multiplexer session (detected via
//! `$ZELLIJ`), it can:
//!
//! 1. Spawn a fresh tab for newly-created or switched-to workspaces, so each
//!    workspace owns its own pane stack. See
//!    [`Multiplexer::open_workspace_tab`] and [`Multiplexer::switch_to_tab`].
//! 2. Rename the active tab to reflect agent status, so a glance at the tab bar
//!    tells the user which workspace needs attention. See
//!    [`Multiplexer::set_tab_status`].
//!
//! All operations are best-effort and never fail the caller: if the
//! multiplexer is unreachable, the user just doesn't get the nicety.
//!
//! ## Multiplexer abstraction
//!
//! Even though this module currently only ships a zellij implementation, the
//! traits are intentionally generic ([`Multiplexer`]) so a tmux adapter could
//! slot in without touching the call sites.
//!
//! ## Disabling
//!
//! Unset `$ZELLIJ` before invoking `dwm`. With `$ZELLIJ` unset,
//! [`detect`] returns `None` and every call site falls through to the
//! ordinary "print path on stdout" behaviour.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::agent::{AgentStatus, AgentSummary};

// ---------------------------------------------------------------------------
// Status glyphs
// ---------------------------------------------------------------------------

/// The glyph for a "running / actively producing tokens" agent.
pub const GLYPH_WORKING: &str = "▶";
/// The glyph for an agent waiting on user input or permission.
pub const GLYPH_WAITING: &str = "●";
/// The glyph for an idle / done agent.
pub const GLYPH_IDLE: &str = "✓";

/// Pick the glyph that represents the most attention-needing agent state in
/// the supplied summary.
///
/// When a tab is shared by several agents, the glyph reflects the most urgent
/// one. The priority order, highest-attention first, is:
///
/// 1. `Waiting` — needs the human now.
/// 2. `Working` — busy producing tokens, no input required but worth knowing.
/// 3. `Idle`   — done; informational only.
///
/// Returns `None` for an empty summary so the caller can clear the glyph and
/// render a bare workspace name.
pub fn glyph_for_summary(summary: &AgentSummary) -> Option<&'static str> {
    summary.most_urgent().map(glyph_for_status)
}

/// Map a single [`AgentStatus`] to its glyph.
pub fn glyph_for_status(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Waiting => GLYPH_WAITING,
        AgentStatus::Working => GLYPH_WORKING,
        AgentStatus::Idle => GLYPH_IDLE,
    }
}

/// All known status glyphs. Used by [`strip_status_glyph`] when matching tab
/// names that may already have been decorated.
const ALL_GLYPHS: &[&str] = &[GLYPH_WORKING, GLYPH_WAITING, GLYPH_IDLE];

/// Strip a trailing whitespace + status glyph from a tab name, returning the
/// bare workspace name.
///
/// Useful when comparing "the tab named X" against a workspace name — the
/// actual tab might be named `"foo ▶"` because a hook handler decorated it.
/// dwm itself currently doesn't list zellij tabs (we rely on
/// `go-to-tab-name` exit status), but this is exposed so a future version
/// that does list tabs can match decorated names.
#[allow(dead_code)]
pub fn strip_status_glyph(name: &str) -> &str {
    for glyph in ALL_GLYPHS {
        if let Some(prefix) = name.strip_suffix(glyph) {
            return prefix.trim_end();
        }
    }
    name.trim_end()
}

/// Compose a tab name from a workspace name and an optional glyph.
pub fn decorate_tab_name(workspace: &str, glyph: Option<&str>) -> String {
    match glyph {
        Some(g) => format!("{} {}", workspace, g),
        None => workspace.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Multiplexer trait + zellij implementation
// ---------------------------------------------------------------------------

/// Abstract terminal multiplexer integration.
///
/// Currently only [`Zellij`] implements this. A future `Tmux` impl would slot
/// in here. All methods return `bool` (success / failure) rather than
/// surfacing errors, because every multiplexer interaction is best-effort and
/// must not affect the rest of dwm's behaviour.
pub trait Multiplexer {
    /// Open a new tab for the given workspace, with the workspace name as the
    /// tab name. Returns `true` on success.
    fn open_workspace_tab(&self, ws_name: &str, ws_path: &Path) -> bool;

    /// Switch focus to an existing tab named exactly `ws_name`. Returns
    /// `true` if the tab existed and we successfully focused it. The caller
    /// can fall back to [`open_workspace_tab`](Self::open_workspace_tab) on
    /// `false`.
    fn switch_to_tab(&self, ws_name: &str) -> bool;

    /// Rename the **current** tab to `<ws_name> <glyph>` (or just `<ws_name>`
    /// when `glyph` is `None`).
    fn set_tab_status(&self, ws_name: &str, glyph: Option<&str>) -> bool;
}

/// Detect the active multiplexer from the environment.
///
/// Returns `Some(Zellij)` when `$ZELLIJ` is set, `None` otherwise.
pub fn detect() -> Option<Zellij> {
    if zellij_active() {
        Some(Zellij::new())
    } else {
        None
    }
}

/// Pure helper: returns `true` when `$ZELLIJ` is set.
pub fn zellij_active() -> bool {
    std::env::var_os("ZELLIJ").is_some()
}

/// Concrete zellij integration, driving the `zellij action` CLI.
///
/// The [`runner`](Zellij::runner) field is a function pointer rather than a
/// hard-coded `Command::new("zellij")` so unit tests can capture the argv we
/// would have run without invoking the real binary.
pub struct Zellij {
    runner: fn(&[&str]) -> bool,
}

impl Default for Zellij {
    fn default() -> Self {
        Self::new()
    }
}

impl Zellij {
    pub fn new() -> Self {
        Self {
            runner: run_zellij_real,
        }
    }

    /// Construct a `Zellij` whose command invocations call `runner` instead of
    /// spawning `zellij`. Used in unit tests.
    #[cfg(test)]
    fn with_runner(runner: fn(&[&str]) -> bool) -> Self {
        Self { runner }
    }
}

impl Multiplexer for Zellij {
    fn open_workspace_tab(&self, ws_name: &str, ws_path: &Path) -> bool {
        let cwd = ws_path.to_string_lossy();
        let args = new_tab_args(ws_name, &cwd);
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        (self.runner)(&argv)
    }

    fn switch_to_tab(&self, ws_name: &str) -> bool {
        let args = go_to_tab_args(ws_name);
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        (self.runner)(&argv)
    }

    fn set_tab_status(&self, ws_name: &str, glyph: Option<&str>) -> bool {
        let new_name = decorate_tab_name(ws_name, glyph);
        let args = rename_tab_args(&new_name);
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        (self.runner)(&argv)
    }
}

// ---------------------------------------------------------------------------
// Pure command-construction helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Build the argv for `zellij action new-tab --layout default --cwd <ws_path>
/// --name <ws_name>`.
pub fn new_tab_args(ws_name: &str, ws_path: &str) -> Vec<String> {
    vec![
        "action".to_string(),
        "new-tab".to_string(),
        "--layout".to_string(),
        "default".to_string(),
        "--cwd".to_string(),
        ws_path.to_string(),
        "--name".to_string(),
        ws_name.to_string(),
    ]
}

/// Build the argv for `zellij action go-to-tab-name <name>`.
pub fn go_to_tab_args(ws_name: &str) -> Vec<String> {
    vec![
        "action".to_string(),
        "go-to-tab-name".to_string(),
        ws_name.to_string(),
    ]
}

/// Build the argv for `zellij action rename-tab <new_name>`.
pub fn rename_tab_args(new_name: &str) -> Vec<String> {
    vec![
        "action".to_string(),
        "rename-tab".to_string(),
        new_name.to_string(),
    ]
}

/// Real zellij invocation. Runs `zellij <args...>` and returns `true` when the
/// process exits 0. Stdout/stderr are silenced; any error is swallowed so
/// callers don't have to care whether zellij is actually installed.
///
/// ## Tab existence detection
///
/// We don't query zellij for a tab list — the supported way to do that varies
/// across versions. Instead we rely on `zellij action go-to-tab-name <name>`'s
/// exit status: if the tab doesn't exist, the command exits non-zero and the
/// caller falls back to opening a new tab. This is the documented hint in
/// `zellij action go-to-tab-name --help`.
fn run_zellij_real(args: &[&str]) -> bool {
    let mut cmd = Command::new("zellij");
    cmd.args(args.iter().map(OsStr::new));
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.stdin(Stdio::null());
    match cmd.status() {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Convenience: high-level helper used by call sites
// ---------------------------------------------------------------------------

/// Switch to or spawn a workspace tab. Returns `true` if zellij handled the
/// switch (in which case the caller should suppress the stdout `cd` path).
///
/// Strategy:
///
/// 1. Try `zellij action go-to-tab-name <ws_name>`. If it succeeds, done.
/// 2. Otherwise the tab may have been decorated by the hook handler with a
///    trailing status glyph (`my-ws ▶`, `my-ws ●`, `my-ws ✓`). Try each
///    decorated form before giving up.
/// 3. Else, fall back to `zellij action new-tab --cwd <ws_path> --name
///    <ws_name>`. If *that* succeeds, done.
/// 4. Else, give up and let the caller print the path so the shell can `cd`.
///
/// We don't query zellij for the tab list because the supported way to do
/// that varies across versions; relying on `go-to-tab-name`'s exit status
/// keeps things simple. The cost is a couple of extra subprocess invocations
/// in the rare "decorated tab" case.
pub fn switch_or_open_tab<M: Multiplexer>(mux: &M, ws_name: &str, ws_path: &Path) -> bool {
    if mux.switch_to_tab(ws_name) {
        return true;
    }
    for glyph in ALL_GLYPHS {
        let decorated = decorate_tab_name(ws_name, Some(glyph));
        if mux.switch_to_tab(&decorated) {
            return true;
        }
    }
    mux.open_workspace_tab(ws_name, ws_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // ── Glyph selection ──────────────────────────────────────────────

    #[test]
    fn glyph_priority_waiting_beats_working() {
        let s = AgentSummary {
            waiting: 1,
            working: 5,
            idle: 9,
        };
        assert_eq!(glyph_for_summary(&s), Some(GLYPH_WAITING));
    }

    #[test]
    fn glyph_priority_working_beats_idle() {
        let s = AgentSummary {
            waiting: 0,
            working: 1,
            idle: 9,
        };
        assert_eq!(glyph_for_summary(&s), Some(GLYPH_WORKING));
    }

    #[test]
    fn glyph_for_only_idle() {
        let s = AgentSummary {
            waiting: 0,
            working: 0,
            idle: 1,
        };
        assert_eq!(glyph_for_summary(&s), Some(GLYPH_IDLE));
    }

    #[test]
    fn glyph_for_empty_is_none() {
        let s = AgentSummary::default();
        assert_eq!(glyph_for_summary(&s), None);
    }

    #[test]
    fn glyph_for_each_status() {
        assert_eq!(glyph_for_status(AgentStatus::Working), GLYPH_WORKING);
        assert_eq!(glyph_for_status(AgentStatus::Waiting), GLYPH_WAITING);
        assert_eq!(glyph_for_status(AgentStatus::Idle), GLYPH_IDLE);
    }

    // ── Name stripping ────────────────────────────────────────────────

    #[test]
    fn strip_glyph_removes_trailing_running() {
        assert_eq!(strip_status_glyph("my-ws ▶"), "my-ws");
    }

    #[test]
    fn strip_glyph_removes_trailing_waiting() {
        assert_eq!(strip_status_glyph("my-ws ●"), "my-ws");
    }

    #[test]
    fn strip_glyph_removes_trailing_idle() {
        assert_eq!(strip_status_glyph("my-ws ✓"), "my-ws");
    }

    #[test]
    fn strip_glyph_no_glyph_returns_input() {
        assert_eq!(strip_status_glyph("my-ws"), "my-ws");
    }

    #[test]
    fn strip_glyph_handles_extra_whitespace() {
        assert_eq!(strip_status_glyph("my-ws   ▶"), "my-ws");
    }

    #[test]
    fn strip_glyph_does_not_match_random_unicode() {
        // The bullet-like `•` isn't in our glyph set; should be left alone.
        assert_eq!(strip_status_glyph("my-ws •"), "my-ws •");
    }

    // ── decorate_tab_name ─────────────────────────────────────────────

    #[test]
    fn decorate_with_glyph_appends() {
        assert_eq!(decorate_tab_name("foo", Some(GLYPH_WORKING)), "foo ▶");
    }

    #[test]
    fn decorate_without_glyph_is_bare() {
        assert_eq!(decorate_tab_name("foo", None), "foo");
    }

    // ── Env detection ─────────────────────────────────────────────────

    #[test]
    fn detect_returns_none_when_zellij_unset() {
        // Save and clear $ZELLIJ for the duration of the test.
        let saved = std::env::var_os("ZELLIJ");
        // SAFETY: tests are run serially when sharing env; nextest gives each
        // test a fresh process. Even so, we restore the value after.
        unsafe {
            std::env::remove_var("ZELLIJ");
        }
        assert!(detect().is_none());
        assert!(!zellij_active());
        if let Some(v) = saved {
            unsafe {
                std::env::set_var("ZELLIJ", v);
            }
        }
    }

    #[test]
    fn detect_returns_some_when_zellij_set() {
        let saved = std::env::var_os("ZELLIJ");
        unsafe {
            std::env::set_var("ZELLIJ", "0");
        }
        assert!(detect().is_some());
        assert!(zellij_active());
        match saved {
            Some(v) => unsafe { std::env::set_var("ZELLIJ", v) },
            None => unsafe { std::env::remove_var("ZELLIJ") },
        }
    }

    // ── Pure command-construction ─────────────────────────────────────

    #[test]
    fn new_tab_args_has_expected_shape() {
        let args = new_tab_args("my-ws", "/tmp/repo/.dwm/my-ws");
        assert_eq!(
            args,
            vec![
                "action",
                "new-tab",
                "--layout",
                "default",
                "--cwd",
                "/tmp/repo/.dwm/my-ws",
                "--name",
                "my-ws",
            ]
        );
    }

    #[test]
    fn go_to_tab_args_has_expected_shape() {
        let args = go_to_tab_args("my-ws");
        assert_eq!(args, vec!["action", "go-to-tab-name", "my-ws"]);
    }

    #[test]
    fn rename_tab_args_has_expected_shape() {
        let args = rename_tab_args("my-ws ▶");
        assert_eq!(args, vec!["action", "rename-tab", "my-ws ▶"]);
    }

    // ── Multiplexer trait dispatch ────────────────────────────────────

    /// Recorded argv from the most recent runner call.
    static LAST_ARGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    /// Whether the next runner call should report success.
    static NEXT_SUCCESS: Mutex<bool> = Mutex::new(true);

    fn fake_runner(args: &[&str]) -> bool {
        let mut last = LAST_ARGS.lock().unwrap();
        *last = args.iter().map(|s| s.to_string()).collect();
        *NEXT_SUCCESS.lock().unwrap()
    }

    fn fake_zellij() -> Zellij {
        Zellij::with_runner(fake_runner)
    }

    fn reset_runner_state(success: bool) {
        LAST_ARGS.lock().unwrap().clear();
        *NEXT_SUCCESS.lock().unwrap() = success;
    }

    #[test]
    fn open_workspace_tab_invokes_new_tab() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runner_state(true);
        let mux = fake_zellij();
        let ok = mux.open_workspace_tab("ws-a", &PathBuf::from("/tmp/r/.dwm/ws-a"));
        assert!(ok);
        assert_eq!(
            *LAST_ARGS.lock().unwrap(),
            vec![
                "action",
                "new-tab",
                "--layout",
                "default",
                "--cwd",
                "/tmp/r/.dwm/ws-a",
                "--name",
                "ws-a",
            ]
        );
    }

    #[test]
    fn switch_to_tab_invokes_go_to_tab_name() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runner_state(true);
        let mux = fake_zellij();
        let ok = mux.switch_to_tab("ws-a");
        assert!(ok);
        assert_eq!(
            *LAST_ARGS.lock().unwrap(),
            vec!["action", "go-to-tab-name", "ws-a"]
        );
    }

    #[test]
    fn set_tab_status_with_glyph_renames() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runner_state(true);
        let mux = fake_zellij();
        let ok = mux.set_tab_status("ws-a", Some(GLYPH_WORKING));
        assert!(ok);
        assert_eq!(
            *LAST_ARGS.lock().unwrap(),
            vec!["action", "rename-tab", "ws-a ▶"]
        );
    }

    #[test]
    fn set_tab_status_without_glyph_renames_to_bare() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runner_state(true);
        let mux = fake_zellij();
        let ok = mux.set_tab_status("ws-a", None);
        assert!(ok);
        assert_eq!(
            *LAST_ARGS.lock().unwrap(),
            vec!["action", "rename-tab", "ws-a"]
        );
    }

    #[test]
    fn switch_or_open_uses_existing_tab_when_present() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runner_state(true);
        let mux = fake_zellij();
        let ok = switch_or_open_tab(&mux, "ws-a", &PathBuf::from("/tmp/r/.dwm/ws-a"));
        assert!(ok);
        // Only switch_to_tab was attempted.
        assert_eq!(
            *LAST_ARGS.lock().unwrap(),
            vec!["action", "go-to-tab-name", "ws-a"]
        );
    }

    /// A runner that records every call and reports `false` for `go-to-tab-name`,
    /// `true` for `new-tab`. Models "tab doesn't exist; fall through to spawn".
    fn fallback_runner(args: &[&str]) -> bool {
        let mut last = LAST_ARGS.lock().unwrap();
        *last = args.iter().map(|s| s.to_string()).collect();
        // Succeed for new-tab, fail for go-to-tab-name.
        !args.contains(&"go-to-tab-name")
    }

    #[test]
    fn switch_or_open_falls_back_to_new_tab() {
        let _guard = TEST_LOCK.lock().unwrap();
        LAST_ARGS.lock().unwrap().clear();
        let mux = Zellij::with_runner(fallback_runner);
        let ok = switch_or_open_tab(&mux, "ws-a", &PathBuf::from("/tmp/r/.dwm/ws-a"));
        assert!(ok);
        // The last recorded call should be the new-tab fallback.
        assert_eq!(
            *LAST_ARGS.lock().unwrap(),
            vec![
                "action",
                "new-tab",
                "--layout",
                "default",
                "--cwd",
                "/tmp/r/.dwm/ws-a",
                "--name",
                "ws-a",
            ]
        );
    }

    /// Serialises tests that touch the shared `LAST_ARGS` / `NEXT_SUCCESS`
    /// statics. Each test grabs this lock first so they don't clobber one
    /// another when nextest runs them in parallel.
    static TEST_LOCK: Mutex<()> = Mutex::new(());
}
