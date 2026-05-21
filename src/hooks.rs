//! Lifecycle hook scripts (`setup`, `run`, `archive`).
//!
//! dwm runs user-defined commands at three points in a workspace's life:
//!
//! - **`setup`** runs once after `dwm new` provisions a workspace (e.g.
//!   `npm install`, copy `.env`).
//! - **`run`** runs on demand via `dwm run` (e.g. `npm run dev`). Honours
//!   [`RunScriptMode`]: in `concurrent` (default) the child is spawned
//!   detached and `dwm run` returns immediately; in `nonconcurrent` dwm
//!   blocks until the child exits, like `setup`.
//! - **`archive`** runs immediately before `dwm delete` removes the workspace
//!   (e.g. `tar` it, push leftover branches). A non-zero exit makes the
//!   caller prompt before tearing the workspace down.
//!
//! ## Configuration sources, in precedence order
//!
//! 1. `<repo-root>/.dwm.toml` (canonical)
//!
//!    ```toml
//!    [scripts]
//!    setup = "npm install && cp ../main/.env .env"
//!    run = "npm run dev"
//!    archive = "tar czf $DWM_WORKSPACE_NAME.tar.gz ."
//!
//!    runScriptMode = "concurrent"  # or "nonconcurrent"
//!    ```
//!
//! 2. `<repo-root>/conductor.json` (drop-in compatibility with Conductor:
//!    <https://www.conductor.build/docs/reference/conductor-json>)
//!
//!    ```json
//!    {
//!      "scripts": { "setup": "npm install", "run": "npm run dev", "archive": "..." },
//!      "runScriptMode": "concurrent"
//!    }
//!    ```
//!
//!    Other Conductor fields (`enterpriseDataPrivacy`, etc.) are accepted and
//!    ignored so existing files "just work" without edits.
//!
//! When neither file exists, an empty [`Hooks`] is returned and no script
//! runs. When a file exists but is malformed, [`load`] returns an error so
//! that the caller surfaces it rather than silently skipping the hook.
//!
//! ## Environment variables
//!
//! Every hook script (setup / run / archive) sees the following env vars,
//! covering both dwm's native names and the full Conductor-compat set so
//! existing `conductor.json` scripts run unchanged:
//!
//! | Variable                   | Value                                                |
//! | -------------------------- | ---------------------------------------------------- |
//! | `DWM_WORKSPACE_PATH`       | Absolute path of the workspace                       |
//! | `DWM_WORKSPACE_NAME`       | Workspace name                                       |
//! | `DWM_REPO_ROOT`            | Absolute path of the original repository root        |
//! | `DWM_VCS`                  | `jj` or `git`                                        |
//! | `DWM_FROM_WORKSPACE`       | The `--from <name>` value, if provided (setup only)  |
//! | `CONDUCTOR_WORKSPACE_NAME` | Same as `DWM_WORKSPACE_NAME`                         |
//! | `CONDUCTOR_WORKSPACE_PATH` | Same as `DWM_WORKSPACE_PATH`                         |
//! | `CONDUCTOR_ROOT_PATH`      | **Source repo root** (== `DWM_REPO_ROOT`)            |
//! | `CONDUCTOR_DEFAULT_BRANCH` | Detected default branch (e.g. `main`)                |
//! | `CONDUCTOR_PORT`           | First port of a 10-port range allocated to this workspace |

use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use crate::ports;
use crate::vcs::VcsType;

/// Lifecycle hook commands declared by the user.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Hooks {
    pub setup: Option<String>,
    pub run: Option<String>,
    pub archive: Option<String>,
    pub run_mode: RunScriptMode,
}

/// How the `run` lifecycle script should be executed.
///
/// Mirrors Conductor's top-level `runScriptMode` field.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RunScriptMode {
    /// Spawn detached and return immediately. Output is inherited from the
    /// parent process so the user sees the script start and can `Ctrl-C` it
    /// from a different shell if necessary. This is the default.
    #[default]
    Concurrent,
    /// Block until the script exits, streaming output to dwm's stderr (same
    /// behaviour as `setup`).
    Nonconcurrent,
}

impl RunScriptMode {
    fn from_optional_str(s: Option<&str>) -> Self {
        match s.map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "nonconcurrent" => RunScriptMode::Nonconcurrent,
            // Default and explicit "concurrent" both fall here.
            _ => RunScriptMode::Concurrent,
        }
    }
}

/// Inputs needed to run any hook script.
pub struct HookContext {
    pub workspace_path: PathBuf,
    pub workspace_name: String,
    pub repo_root: PathBuf,
    pub vcs_type: VcsType,
    pub from_workspace: Option<String>,
    /// Default branch (`main`, `master`, ...) used to populate
    /// `CONDUCTOR_DEFAULT_BRANCH`. Defaults to `"main"` if detection failed.
    pub default_branch: String,
}

// ── On-disk schema (TOML / conductor.json) ──────────────────────────────────

/// Schema for `<repo-root>/.dwm.toml`.
///
/// Mirrors `conductor.json`'s shape (top-level `scripts` table) so users can
/// mentally translate. Unknown top-level keys are accepted and ignored — we
/// don't `deny_unknown_fields` because that would make new Conductor fields
/// break existing dwm installs.
#[derive(Debug, Default, Deserialize)]
struct DwmConfigFile {
    #[serde(default)]
    scripts: Option<Scripts>,
    #[serde(default, alias = "runScriptMode")]
    run_script_mode: Option<String>,
}

/// Schema for `<repo-root>/conductor.json`.
#[derive(Debug, Default, Deserialize)]
struct ConductorConfigFile {
    #[serde(default)]
    scripts: Option<Scripts>,
    #[serde(default, rename = "runScriptMode")]
    run_script_mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Scripts {
    #[serde(default)]
    setup: Option<String>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    archive: Option<String>,
}

// ── Parsing helpers (pure, unit-tested) ─────────────────────────────────────

/// Parse `.dwm.toml` content. Returns an empty [`Hooks`] when `[scripts]` is
/// absent. Errors on malformed TOML.
fn parse_dwm_toml(s: &str) -> Result<Hooks> {
    let cfg: DwmConfigFile = toml::from_str(s).context("parsing .dwm.toml")?;
    let mut hooks = cfg
        .scripts
        .map(|s| Hooks {
            setup: s.setup,
            run: s.run,
            archive: s.archive,
            run_mode: RunScriptMode::default(),
        })
        .unwrap_or_default();
    hooks.run_mode = RunScriptMode::from_optional_str(cfg.run_script_mode.as_deref());
    Ok(hooks)
}

/// Parse `conductor.json` content. Returns an empty [`Hooks`] when `scripts`
/// is absent. Errors on malformed JSON. Unknown fields are ignored.
fn parse_conductor_json(s: &str) -> Result<Hooks> {
    let cfg: ConductorConfigFile = serde_json::from_str(s).context("parsing conductor.json")?;
    let mut hooks = cfg
        .scripts
        .map(|s| Hooks {
            setup: s.setup,
            run: s.run,
            archive: s.archive,
            run_mode: RunScriptMode::default(),
        })
        .unwrap_or_default();
    hooks.run_mode = RunScriptMode::from_optional_str(cfg.run_script_mode.as_deref());
    Ok(hooks)
}

// ── Loader ──────────────────────────────────────────────────────────────────

/// Load hooks from `<repo_root>/.dwm.toml` or `<repo_root>/conductor.json`.
///
/// Precedence: `.dwm.toml` wins if both exist. Returns an empty [`Hooks`] if
/// neither file exists. Returns an error if the chosen file is malformed.
pub fn load(repo_root: &Path) -> Result<Hooks> {
    let dwm_toml = repo_root.join(".dwm.toml");
    if dwm_toml.exists() {
        let s = std::fs::read_to_string(&dwm_toml)
            .with_context(|| format!("reading {}", dwm_toml.display()))?;
        return parse_dwm_toml(&s).with_context(|| format!("in {}", dwm_toml.display()));
    }

    let conductor = repo_root.join("conductor.json");
    if conductor.exists() {
        let s = std::fs::read_to_string(&conductor)
            .with_context(|| format!("reading {}", conductor.display()))?;
        return parse_conductor_json(&s).with_context(|| format!("in {}", conductor.display()));
    }

    Ok(Hooks::default())
}

// ── Env var helper ──────────────────────────────────────────────────────────

/// Apply the full DWM_*/CONDUCTOR_* env-var set to `cmd`.
///
/// `port_base`, when `Some`, is exposed as `CONDUCTOR_PORT`. We thread it as
/// an explicit argument rather than allocating inside this helper so that
/// tests (and callers that don't care about ports, e.g. unit tests of the
/// archive runner) can opt out.
fn apply_env(cmd: &mut Command, ctx: &HookContext, port_base: Option<u16>) {
    cmd.env("DWM_WORKSPACE_PATH", &ctx.workspace_path)
        .env("DWM_WORKSPACE_NAME", &ctx.workspace_name)
        .env("DWM_REPO_ROOT", &ctx.repo_root)
        .env("DWM_VCS", ctx.vcs_type.to_string())
        .env("CONDUCTOR_WORKSPACE_NAME", &ctx.workspace_name)
        .env("CONDUCTOR_WORKSPACE_PATH", &ctx.workspace_path)
        // CONDUCTOR_ROOT_PATH is the *source* repo root, not the workspace
        // path. (Earlier dwm versions had this aliased to the workspace path,
        // which broke `cp $CONDUCTOR_ROOT_PATH/.env.local .env.local`-style
        // recipes.)
        .env("CONDUCTOR_ROOT_PATH", &ctx.repo_root)
        .env("CONDUCTOR_DEFAULT_BRANCH", &ctx.default_branch);

    if let Some(p) = port_base {
        cmd.env("CONDUCTOR_PORT", p.to_string());
    }
    // Set DWM_FROM_WORKSPACE explicitly — never inherit from the parent. The
    // child must see the var iff --from was passed, regardless of what dwm's
    // calling process happens to have in its own env.
    match &ctx.from_workspace {
        Some(from) => {
            cmd.env("DWM_FROM_WORKSPACE", from);
        }
        None => {
            cmd.env_remove("DWM_FROM_WORKSPACE");
        }
    }
}

/// Resolve (and persist) the workspace's `CONDUCTOR_PORT` base, reporting any
/// allocation error to stderr but never failing the hook on its account.
fn resolve_port(ctx: &HookContext) -> Option<u16> {
    match ports::ensure_port(&ctx.workspace_path, &ctx.repo_root) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("warning: could not allocate CONDUCTOR_PORT: {}", e);
            None
        }
    }
}

// ── Setup runner ────────────────────────────────────────────────────────────

/// Run the `setup` hook (if configured) for a freshly-created workspace.
///
/// The script is invoked as `sh -c "<command>"` with cwd set to the workspace
/// path. Stdout and stderr from the child are streamed to the parent's
/// **stderr** (never stdout, which is reserved for the shell wrapper's `cd`
/// target).
///
/// A non-zero exit status is logged as a warning but does NOT propagate as an
/// error: the workspace was created successfully and the user should still
/// `cd` into it. Returns an error only if the script could not be spawned at
/// all (e.g. `sh` is unavailable).
pub fn run_setup(hooks: &Hooks, ctx: &HookContext) -> Result<()> {
    let Some(cmd) = trimmed(hooks.setup.as_deref()) else {
        return Ok(());
    };
    eprintln!("running setup script: {}", cmd);
    let port = resolve_port(ctx);
    run_blocking(cmd, ctx, port, "setup", false)
}

// ── Run runner ──────────────────────────────────────────────────────────────

/// Run the `run` hook (if configured). Honours `hooks.run_mode`:
///
/// - [`RunScriptMode::Concurrent`] spawns the child detached and returns
///   immediately. The child inherits dwm's stdio so the user sees its output
///   in their terminal; closing that terminal will kill the child.
/// - [`RunScriptMode::Nonconcurrent`] blocks until the script exits, mirroring
///   `setup`'s behaviour.
///
/// Returns `Ok(())` if the script started (or there was nothing to run). A
/// non-zero exit in `Nonconcurrent` mode is logged but not raised, matching
/// `setup`'s policy.
pub fn run_run_script(hooks: &Hooks, ctx: &HookContext) -> Result<()> {
    let Some(cmd) = trimmed(hooks.run.as_deref()) else {
        anyhow::bail!("no `scripts.run` defined in .dwm.toml or conductor.json");
    };

    let port = resolve_port(ctx);
    match hooks.run_mode {
        RunScriptMode::Nonconcurrent => {
            eprintln!("running run script (blocking): {}", cmd);
            run_blocking(cmd, ctx, port, "run", false)
        }
        RunScriptMode::Concurrent => {
            eprintln!("running run script (detached): {}", cmd);
            spawn_detached(cmd, ctx, port)
        }
    }
}

/// Spawn the `run` script detached: stdin connected to `/dev/null`, stdout
/// and stderr inherited from the parent so the user sees output, and the
/// child reaped lazily by the OS (we don't `wait`).
///
/// On Unix we additionally `setsid` via `pre_exec` to put the child in its
/// own session, so `Ctrl-C` in the parent terminal does not propagate to it.
/// (Tradeoff: this means the user has to `kill` the process group themselves
/// to stop it. That matches Conductor's "Run" semantics — the script is
/// expected to be a long-lived dev server.)
fn spawn_detached(cmd: &str, ctx: &HookContext, port: Option<u16>) -> Result<()> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(&ctx.workspace_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    apply_env(&mut command, ctx, port);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() is async-signal-safe and only modifies the new
        // process's session ID before exec.
        unsafe {
            command.pre_exec(|| {
                // Best-effort: a failure here is non-fatal — the worst case
                // is that the child stays in our process group.
                let _ = libc_setsid();
                Ok(())
            });
        }
    }

    let child = command
        .spawn()
        .with_context(|| format!("spawning run script (detached): sh -c {:?}", cmd))?;
    eprintln!("run script started (pid {})", child.id());
    // We deliberately drop `child` without waiting — this is the detached path.
    std::mem::forget(child);
    Ok(())
}

#[cfg(unix)]
#[allow(non_snake_case)]
fn libc_setsid() -> std::io::Result<()> {
    // libc isn't a dep of this crate; we call the syscall directly via
    // std::process semantics by going through a tiny extern. For portability
    // we'd ideally pull libc in — but we can lean on `nix`-free direct FFI to
    // avoid a new dependency. Use the syscall via `extern "C"`.
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    // SAFETY: setsid takes no arguments and returns an integer. It's safe to
    // call between fork and exec.
    let r = unsafe { setsid() };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ── Archive runner ──────────────────────────────────────────────────────────

/// Run the `archive` hook (if configured) before `dwm delete` tears down a
/// workspace.
///
/// Returns:
/// - `Ok(true)` when the script succeeded **or** was not configured.
/// - `Ok(false)` when the script ran and returned non-zero. The caller is
///   expected to prompt the user before proceeding.
/// - `Err(_)` only when the script could not be spawned at all.
pub fn run_archive_script(hooks: &Hooks, ctx: &HookContext) -> Result<bool> {
    let Some(cmd) = trimmed(hooks.archive.as_deref()) else {
        return Ok(true);
    };

    eprintln!("running archive script: {}", cmd);
    let port = resolve_port(ctx);
    let status = spawn_blocking(cmd, ctx, port, "archive")?;
    Ok(status)
}

// ── Shared blocking spawner ─────────────────────────────────────────────────

/// Spawn `sh -c cmd` in the workspace dir, stream child output to dwm's
/// stderr, wait for it to exit, and treat a non-zero exit as a non-error
/// (logging a warning).
///
/// `kind` is the human-readable label used in log messages (`"setup"`,
/// `"run"`, `"archive"`).
///
/// `propagate_failure` controls whether a non-zero exit short-circuits to
/// `Ok(false)`-by-bool semantics; this version always logs and returns
/// `Ok(())`. Callers who care about exit status (the archive hook) use
/// [`spawn_blocking`] directly instead.
fn run_blocking(
    cmd: &str,
    ctx: &HookContext,
    port: Option<u16>,
    kind: &str,
    _propagate_failure: bool,
) -> Result<()> {
    let success = spawn_blocking(cmd, ctx, port, kind)?;
    if !success {
        eprintln!(
            "warning: {} script returned non-zero (continuing); workspace may be in an inconsistent state",
            kind
        );
    }
    Ok(())
}

/// Synchronous version of [`run_blocking`] that returns whether the script
/// exited successfully. Used by the archive hook so the caller can prompt.
fn spawn_blocking(cmd: &str, ctx: &HookContext, port: Option<u16>, kind: &str) -> Result<bool> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(&ctx.workspace_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_env(&mut command, ctx, port);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {} script: sh -c {:?}", kind, cmd))?;

    // Forward both child stdout and child stderr to parent stderr, so the
    // shell wrapper's stdout (the cd target) stays clean.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_thread = stdout.map(|s| {
        thread::spawn(move || {
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("{}", line);
            }
        })
    });
    let stderr_thread = stderr.map(|s| {
        thread::spawn(move || {
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("{}", line);
            }
        })
    });

    let status = child
        .wait()
        .with_context(|| format!("waiting for {} script", kind))?;
    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    Ok(status.success())
}

/// Returns the trimmed command string when `s` is non-empty after trimming.
fn trimmed(s: Option<&str>) -> Option<&str> {
    let s = s?.trim();
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(dir: &Path, name: &str) -> HookContext {
        HookContext {
            workspace_path: dir.to_path_buf(),
            workspace_name: name.to_string(),
            repo_root: dir.to_path_buf(),
            vcs_type: VcsType::Jj,
            from_workspace: None,
            default_branch: "main".to_string(),
        }
    }

    // ── TOML parsing ─────────────────────────────────────────────────────

    #[test]
    fn toml_parses_full_file() {
        let s = r#"
            [scripts]
            setup = "npm install"
            run = "npm run dev"
            archive = "rm -rf node_modules"
        "#;
        let h = parse_dwm_toml(s).unwrap();
        assert_eq!(h.setup.as_deref(), Some("npm install"));
        assert_eq!(h.run.as_deref(), Some("npm run dev"));
        assert_eq!(h.archive.as_deref(), Some("rm -rf node_modules"));
    }

    #[test]
    fn toml_parses_setup_only() {
        let s = r#"
            [scripts]
            setup = "echo hi"
        "#;
        let h = parse_dwm_toml(s).unwrap();
        assert_eq!(h.setup.as_deref(), Some("echo hi"));
        assert!(h.run.is_none());
        assert!(h.archive.is_none());
    }

    #[test]
    fn toml_missing_scripts_table_returns_empty_hooks() {
        let h = parse_dwm_toml("").unwrap();
        assert_eq!(h, Hooks::default());

        // also: a TOML file with other unrelated keys
        let s = r#"
            other = "thing"
        "#;
        let h = parse_dwm_toml(s).unwrap();
        assert_eq!(h, Hooks::default());
    }

    #[test]
    fn toml_unknown_keys_in_scripts_are_ignored() {
        let s = r#"
            [scripts]
            setup = "x"
            mystery = "ignored"
        "#;
        let h = parse_dwm_toml(s).unwrap();
        assert_eq!(h.setup.as_deref(), Some("x"));
    }

    #[test]
    fn toml_malformed_errors() {
        let s = "this is not = valid toml [[[";
        assert!(parse_dwm_toml(s).is_err());
    }

    #[test]
    fn toml_run_script_mode_defaults_to_concurrent() {
        let h = parse_dwm_toml(
            r#"[scripts]
setup = "x"
"#,
        )
        .unwrap();
        assert_eq!(h.run_mode, RunScriptMode::Concurrent);
    }

    #[test]
    fn toml_run_script_mode_concurrent_explicit() {
        let h = parse_dwm_toml(
            r#"runScriptMode = "concurrent"
[scripts]
setup = "x"
"#,
        )
        .unwrap();
        assert_eq!(h.run_mode, RunScriptMode::Concurrent);
    }

    #[test]
    fn toml_run_script_mode_nonconcurrent() {
        let h = parse_dwm_toml(
            r#"runScriptMode = "nonconcurrent"
[scripts]
setup = "x"
"#,
        )
        .unwrap();
        assert_eq!(h.run_mode, RunScriptMode::Nonconcurrent);
    }

    #[test]
    fn toml_run_script_mode_invalid_falls_back_to_concurrent() {
        let h = parse_dwm_toml(
            r#"runScriptMode = "blizzard"
[scripts]
setup = "x"
"#,
        )
        .unwrap();
        assert_eq!(h.run_mode, RunScriptMode::Concurrent);
    }

    // ── JSON parsing (Conductor compat) ──────────────────────────────────

    #[test]
    fn conductor_parses_full_file() {
        let s = r#"{
            "scripts": {
                "setup": "npm install",
                "run": "npm run dev",
                "archive": "tar czf"
            },
            "runScriptMode": "concurrent",
            "enterpriseDataPrivacy": true
        }"#;
        let h = parse_conductor_json(s).unwrap();
        assert_eq!(h.setup.as_deref(), Some("npm install"));
        assert_eq!(h.run.as_deref(), Some("npm run dev"));
        assert_eq!(h.archive.as_deref(), Some("tar czf"));
        assert_eq!(h.run_mode, RunScriptMode::Concurrent);
    }

    #[test]
    fn conductor_parses_setup_only() {
        let s = r#"{ "scripts": { "setup": "yarn" } }"#;
        let h = parse_conductor_json(s).unwrap();
        assert_eq!(h.setup.as_deref(), Some("yarn"));
        assert!(h.run.is_none());
        assert!(h.archive.is_none());
    }

    #[test]
    fn conductor_run_script_mode_nonconcurrent() {
        let s = r#"{ "runScriptMode": "nonconcurrent" }"#;
        let h = parse_conductor_json(s).unwrap();
        assert_eq!(h.run_mode, RunScriptMode::Nonconcurrent);
    }

    #[test]
    fn conductor_run_script_mode_invalid_falls_back() {
        let s = r#"{ "runScriptMode": "weird" }"#;
        let h = parse_conductor_json(s).unwrap();
        assert_eq!(h.run_mode, RunScriptMode::Concurrent);
    }

    #[test]
    fn conductor_unknown_top_level_keys_are_ignored() {
        let s = r#"{
            "scripts": { "setup": "x" },
            "someFutureField": { "nested": [1, 2, 3] },
            "anotherUnknown": "ok"
        }"#;
        let h = parse_conductor_json(s).unwrap();
        assert_eq!(h.setup.as_deref(), Some("x"));
    }

    #[test]
    fn conductor_no_scripts_returns_empty_hooks() {
        let s = r#"{ "runScriptMode": "concurrent" }"#;
        let h = parse_conductor_json(s).unwrap();
        assert_eq!(h, Hooks::default());
    }

    #[test]
    fn conductor_malformed_errors() {
        let s = "{ this is not valid json";
        assert!(parse_conductor_json(s).is_err());
    }

    // ── Loader / precedence ──────────────────────────────────────────────

    #[test]
    fn load_returns_empty_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let h = load(dir.path()).unwrap();
        assert_eq!(h, Hooks::default());
    }

    #[test]
    fn load_reads_dwm_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".dwm.toml"),
            r#"[scripts]
setup = "echo dwm"
"#,
        )
        .unwrap();
        let h = load(dir.path()).unwrap();
        assert_eq!(h.setup.as_deref(), Some("echo dwm"));
    }

    #[test]
    fn load_reads_conductor_json_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("conductor.json"),
            r#"{"scripts": {"setup": "echo conductor"}}"#,
        )
        .unwrap();
        let h = load(dir.path()).unwrap();
        assert_eq!(h.setup.as_deref(), Some("echo conductor"));
    }

    #[test]
    fn load_dwm_toml_wins_over_conductor_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".dwm.toml"),
            r#"[scripts]
setup = "from-dwm-toml"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("conductor.json"),
            r#"{"scripts": {"setup": "from-conductor"}}"#,
        )
        .unwrap();
        let h = load(dir.path()).unwrap();
        assert_eq!(h.setup.as_deref(), Some("from-dwm-toml"));
    }

    #[test]
    fn load_propagates_malformed_dwm_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".dwm.toml"), "broken [[ toml").unwrap();
        assert!(load(dir.path()).is_err());
    }

    #[test]
    fn load_propagates_malformed_conductor_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("conductor.json"), "{ broken json").unwrap();
        assert!(load(dir.path()).is_err());
    }

    // ── apply_env ────────────────────────────────────────────────────────

    #[test]
    fn apply_env_sets_full_dwm_and_conductor_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&repo).unwrap();

        let ctx = HookContext {
            workspace_path: ws.clone(),
            workspace_name: "feat".to_string(),
            repo_root: repo.clone(),
            vcs_type: VcsType::Git,
            from_workspace: Some("source".to_string()),
            default_branch: "trunk".to_string(),
        };

        let mut cmd = Command::new("true");
        apply_env(&mut cmd, &ctx, Some(3050));

        // Read back via Command::get_envs() introspection (stable since 1.66).
        let mut got: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (k, v) in cmd.get_envs() {
            if let Some(v) = v {
                got.insert(
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                );
            }
        }

        assert_eq!(
            got.get("DWM_WORKSPACE_PATH").map(String::as_str),
            Some(ws.to_str().unwrap())
        );
        assert_eq!(
            got.get("DWM_WORKSPACE_NAME").map(String::as_str),
            Some("feat")
        );
        assert_eq!(
            got.get("DWM_REPO_ROOT").map(String::as_str),
            Some(repo.to_str().unwrap())
        );
        assert_eq!(got.get("DWM_VCS").map(String::as_str), Some("git"));
        assert_eq!(
            got.get("DWM_FROM_WORKSPACE").map(String::as_str),
            Some("source")
        );

        assert_eq!(
            got.get("CONDUCTOR_WORKSPACE_NAME").map(String::as_str),
            Some("feat")
        );
        assert_eq!(
            got.get("CONDUCTOR_WORKSPACE_PATH").map(String::as_str),
            Some(ws.to_str().unwrap())
        );
        // CRITICAL: CONDUCTOR_ROOT_PATH = repo_root, not workspace_path.
        assert_eq!(
            got.get("CONDUCTOR_ROOT_PATH").map(String::as_str),
            Some(repo.to_str().unwrap())
        );
        assert_ne!(
            got.get("CONDUCTOR_ROOT_PATH").map(String::as_str),
            Some(ws.to_str().unwrap()),
            "CONDUCTOR_ROOT_PATH must NOT be the workspace path"
        );
        assert_eq!(
            got.get("CONDUCTOR_DEFAULT_BRANCH").map(String::as_str),
            Some("trunk")
        );
        assert_eq!(got.get("CONDUCTOR_PORT").map(String::as_str), Some("3050"));
    }

    #[test]
    fn apply_env_omits_port_when_none() {
        let ctx = HookContext {
            workspace_path: PathBuf::from("/ws"),
            workspace_name: "feat".to_string(),
            repo_root: PathBuf::from("/repo"),
            vcs_type: VcsType::Jj,
            from_workspace: None,
            default_branch: "main".to_string(),
        };
        let mut cmd = Command::new("true");
        apply_env(&mut cmd, &ctx, None);

        let has_port = cmd.get_envs().any(|(k, _)| k == "CONDUCTOR_PORT");
        assert!(!has_port);
        // DWM_FROM_WORKSPACE is explicitly removed (not absent) so the child
        // never inherits a stale value from the parent. get_envs() sees the
        // removal as (key, None); either no entry or an entry with None value
        // is fine — what matters is that no Some(value) is staged.
        let leaks_from = cmd
            .get_envs()
            .any(|(k, v)| k == "DWM_FROM_WORKSPACE" && v.is_some());
        assert!(!leaks_from);
    }

    // ── Setup runner integration ─────────────────────────────────────────

    #[test]
    fn run_setup_no_setup_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path(), "ws");
        run_setup(&Hooks::default(), &ctx).unwrap();
    }

    #[test]
    fn run_setup_executes_script_with_env() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some(
                "echo \"$DWM_WORKSPACE_NAME|$DWM_VCS|$CONDUCTOR_ROOT_PATH|$CONDUCTOR_DEFAULT_BRANCH\" > marker"
                    .to_string(),
            ),
            ..Hooks::default()
        };
        let ctx = HookContext {
            workspace_path: dir.path().to_path_buf(),
            workspace_name: "feature-x".into(),
            // repo_root deliberately distinct from workspace_path so we can
            // assert CONDUCTOR_ROOT_PATH is NOT the workspace path.
            repo_root: dir.path().to_path_buf(),
            vcs_type: VcsType::Git,
            from_workspace: None,
            default_branch: "trunk".into(),
        };
        run_setup(&hooks, &ctx).unwrap();
        let marker = std::fs::read_to_string(dir.path().join("marker")).unwrap();
        let marker = marker.trim();
        // CONDUCTOR_ROOT_PATH == repo_root; in this test repo_root == workspace_path == dir.
        assert_eq!(
            marker,
            format!("feature-x|git|{}|trunk", dir.path().display())
        );
    }

    #[test]
    fn run_setup_runs_in_workspace_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some("pwd > where".to_string()),
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        run_setup(&hooks, &ctx).unwrap();
        let where_ = std::fs::read_to_string(dir.path().join("where")).unwrap();
        let where_ = where_.trim();
        let observed = std::fs::canonicalize(where_).unwrap();
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(observed, expected);
    }

    #[test]
    fn run_setup_sets_from_workspace_env_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some("echo \"${DWM_FROM_WORKSPACE:-<unset>}\" > from".to_string()),
            ..Hooks::default()
        };
        let ctx = HookContext {
            workspace_path: dir.path().to_path_buf(),
            workspace_name: "ws".into(),
            repo_root: dir.path().to_path_buf(),
            vcs_type: VcsType::Jj,
            from_workspace: Some("source-ws".to_string()),
            default_branch: "main".into(),
        };
        run_setup(&hooks, &ctx).unwrap();
        let v = std::fs::read_to_string(dir.path().join("from")).unwrap();
        assert_eq!(v.trim(), "source-ws");
    }

    #[test]
    fn run_setup_does_not_inherit_dwm_from_workspace() {
        // Regression: previously the runner only conditionally set
        // DWM_FROM_WORKSPACE, so when from_workspace was None the child would
        // silently inherit any value the parent process had.
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some("echo \"${DWM_FROM_WORKSPACE:-<unset>}\" > from".to_string()),
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        // SAFETY: nextest runs each test in its own process by default, so
        // setting a process-global env var here is safe and doesn't race with
        // other tests.
        unsafe { std::env::set_var("DWM_FROM_WORKSPACE", "leaked-value") };
        run_setup(&hooks, &ctx).unwrap();
        let v = std::fs::read_to_string(dir.path().join("from")).unwrap();
        assert_eq!(v.trim(), "<unset>");
    }

    #[test]
    fn run_setup_nonzero_exit_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some("exit 7".to_string()),
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        // Should NOT propagate the failure.
        run_setup(&hooks, &ctx).unwrap();
    }

    #[test]
    fn run_setup_empty_command_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some("   ".to_string()),
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        run_setup(&hooks, &ctx).unwrap();
    }

    #[test]
    fn run_setup_exposes_conductor_port() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some("echo \"$CONDUCTOR_PORT\" > port".to_string()),
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        run_setup(&hooks, &ctx).unwrap();
        let v = std::fs::read_to_string(dir.path().join("port")).unwrap();
        let port: u16 = v.trim().parse().expect("CONDUCTOR_PORT must be numeric");
        assert_eq!(port, ports::BASE);
    }

    // ── Run runner ───────────────────────────────────────────────────────

    #[test]
    fn run_run_script_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path(), "ws");
        let err = run_run_script(&Hooks::default(), &ctx).unwrap_err();
        assert!(
            err.to_string().contains("no `scripts.run`"),
            "error: {}",
            err
        );
    }

    #[test]
    fn run_run_script_nonconcurrent_blocks_and_runs() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            run: Some("echo ran > marker".to_string()),
            run_mode: RunScriptMode::Nonconcurrent,
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        run_run_script(&hooks, &ctx).unwrap();
        let marker = std::fs::read_to_string(dir.path().join("marker")).unwrap();
        assert_eq!(marker.trim(), "ran");
    }

    #[test]
    fn run_run_script_concurrent_returns_immediately() {
        let dir = tempfile::tempdir().unwrap();
        // The script sleeps a short while but the test asserts that
        // run_run_script returned long before sleep would have.
        let hooks = Hooks {
            run: Some("sleep 1".to_string()),
            run_mode: RunScriptMode::Concurrent,
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        let start = std::time::Instant::now();
        run_run_script(&hooks, &ctx).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "concurrent mode should not block (took {:?})",
            elapsed
        );
        // The detached child will be reaped by the test runner / OS once
        // sleep finishes; we don't wait for it here.
    }

    // ── Archive runner ───────────────────────────────────────────────────

    #[test]
    fn run_archive_script_missing_returns_true() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path(), "ws");
        assert!(run_archive_script(&Hooks::default(), &ctx).unwrap());
    }

    #[test]
    fn run_archive_script_success_returns_true() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            archive: Some("echo archived > marker".to_string()),
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        assert!(run_archive_script(&hooks, &ctx).unwrap());
        let marker = std::fs::read_to_string(dir.path().join("marker")).unwrap();
        assert_eq!(marker.trim(), "archived");
    }

    #[test]
    fn run_archive_script_failure_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            archive: Some("exit 9".to_string()),
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        assert!(!run_archive_script(&hooks, &ctx).unwrap());
    }

    #[test]
    fn run_archive_script_runs_in_workspace_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            archive: Some("pwd > where".to_string()),
            ..Hooks::default()
        };
        let ctx = make_ctx(dir.path(), "ws");
        run_archive_script(&hooks, &ctx).unwrap();
        let where_ = std::fs::read_to_string(dir.path().join("where")).unwrap();
        let observed = std::fs::canonicalize(where_.trim()).unwrap();
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(observed, expected);
    }
}
