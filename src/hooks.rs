//! Post-workspace-creation setup hooks.
//!
//! After `dwm new` finishes provisioning a workspace, dwm optionally runs a
//! user-defined `setup` script (e.g. `npm install`, copying `.env`, etc.) to
//! get the new workspace into a usable state.
//!
//! ## Configuration sources, in precedence order
//!
//! 1. `<repo-root>/.dwm.toml` (canonical)
//!
//!    ```toml
//!    [scripts]
//!    setup = "npm install && cp ../main/.env .env"
//!    # `run` and `archive` are reserved for future lifecycle hooks; they are
//!    # parsed and stored but not invoked yet.
//!    ```
//!
//! 2. `<repo-root>/conductor.json` (drop-in compatibility with Conductor:
//!    <https://www.conductor.build/docs/reference/conductor-json>)
//!
//!    ```json
//!    {
//!      "scripts": {
//!        "setup": "npm install",
//!        "run": "npm run dev",
//!        "archive": "..."
//!      }
//!    }
//!    ```
//!
//!    Other Conductor fields (`runScriptMode`, `enterpriseDataPrivacy`, etc.)
//!    are accepted and ignored so that an existing `conductor.json` "just
//!    works" without edits.
//!
//! When neither file exists, an empty [`Hooks`] is returned and no script runs.
//! When a file exists but is malformed, [`load`] returns an error so that
//! `dwm new` surfaces it rather than silently skipping the hook.
//!
//! ## Environment variables
//!
//! When the setup script runs, the following env vars are set on the child:
//!
//! - `DWM_WORKSPACE_PATH` — absolute path of the new workspace
//! - `DWM_WORKSPACE_NAME` — workspace name
//! - `DWM_REPO_ROOT` — absolute path of the original repository root
//! - `DWM_VCS` — `"jj"` or `"git"`
//! - `DWM_FROM_WORKSPACE` — the `--from <name>` value, if provided
//! - `CONDUCTOR_ROOT_PATH` — alias of `DWM_WORKSPACE_PATH`, mirroring
//!   Conductor's "workspace's repo path" semantic so existing scripts work.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use crate::vcs::VcsType;

/// Lifecycle hook commands declared by the user. Currently only [`Hooks::setup`]
/// is invoked; the others are parsed for forward-compat.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Hooks {
    pub setup: Option<String>,
    pub run: Option<String>,
    pub archive: Option<String>,
}

/// Inputs needed to run a hook: where the workspace is, where the repo is, and
/// the optional `--from` source. Used to populate child-process env vars.
pub struct HookContext {
    pub workspace_path: PathBuf,
    pub workspace_name: String,
    pub repo_root: PathBuf,
    pub vcs_type: VcsType,
    pub from_workspace: Option<String>,
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
}

/// Schema for `<repo-root>/conductor.json`. Same shape as `DwmConfigFile` —
/// kept as a distinct type only because `serde_json` and `toml` derive paths
/// stay tidier this way and we may diverge later.
#[derive(Debug, Default, Deserialize)]
struct ConductorConfigFile {
    #[serde(default)]
    scripts: Option<Scripts>,
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

impl From<Scripts> for Hooks {
    fn from(s: Scripts) -> Self {
        Hooks {
            setup: s.setup,
            run: s.run,
            archive: s.archive,
        }
    }
}

// ── Parsing helpers (pure, unit-tested) ─────────────────────────────────────

/// Parse `.dwm.toml` content. Returns an empty [`Hooks`] when `[scripts]` is
/// absent. Errors on malformed TOML.
fn parse_dwm_toml(s: &str) -> Result<Hooks> {
    let cfg: DwmConfigFile = toml::from_str(s).context("parsing .dwm.toml")?;
    Ok(cfg.scripts.map(Hooks::from).unwrap_or_default())
}

/// Parse `conductor.json` content. Returns an empty [`Hooks`] when `scripts`
/// is absent. Errors on malformed JSON. Unknown fields are ignored.
fn parse_conductor_json(s: &str) -> Result<Hooks> {
    let cfg: ConductorConfigFile = serde_json::from_str(s).context("parsing conductor.json")?;
    Ok(cfg.scripts.map(Hooks::from).unwrap_or_default())
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

// ── Runner ──────────────────────────────────────────────────────────────────

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
    let Some(cmd) = hooks.setup.as_deref() else {
        return Ok(());
    };
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Ok(());
    }

    eprintln!("running setup script: {}", cmd);

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(&ctx.workspace_path)
        .env("DWM_WORKSPACE_PATH", &ctx.workspace_path)
        .env("DWM_WORKSPACE_NAME", &ctx.workspace_name)
        .env("DWM_REPO_ROOT", &ctx.repo_root)
        .env("DWM_VCS", ctx.vcs_type.to_string())
        .env("CONDUCTOR_ROOT_PATH", &ctx.workspace_path)
        .envs(
            ctx.from_workspace
                .as_deref()
                .map(|f| ("DWM_FROM_WORKSPACE", f)),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning setup script: sh -c {:?}", cmd))?;

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

    let status = child.wait().context("waiting for setup script")?;
    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        eprintln!(
            "warning: setup script failed with exit code {}; workspace was created but is unconfigured",
            code
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── Runner integration ───────────────────────────────────────────────

    #[test]
    fn run_setup_no_setup_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = HookContext {
            workspace_path: dir.path().to_path_buf(),
            workspace_name: "ws".into(),
            repo_root: dir.path().to_path_buf(),
            vcs_type: VcsType::Jj,
            from_workspace: None,
        };
        run_setup(&Hooks::default(), &ctx).unwrap();
    }

    #[test]
    fn run_setup_executes_script_with_env() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some(
                "echo \"$DWM_WORKSPACE_NAME|$DWM_VCS|$CONDUCTOR_ROOT_PATH\" > marker".to_string(),
            ),
            ..Hooks::default()
        };
        let ctx = HookContext {
            workspace_path: dir.path().to_path_buf(),
            workspace_name: "feature-x".into(),
            repo_root: dir.path().to_path_buf(),
            vcs_type: VcsType::Git,
            from_workspace: None,
        };
        run_setup(&hooks, &ctx).unwrap();
        let marker = std::fs::read_to_string(dir.path().join("marker")).unwrap();
        let marker = marker.trim();
        assert_eq!(marker, format!("feature-x|git|{}", dir.path().display()));
    }

    #[test]
    fn run_setup_runs_in_workspace_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks {
            setup: Some("pwd > where".to_string()),
            ..Hooks::default()
        };
        let ctx = HookContext {
            workspace_path: dir.path().to_path_buf(),
            workspace_name: "ws".into(),
            repo_root: dir.path().to_path_buf(),
            vcs_type: VcsType::Jj,
            from_workspace: None,
        };
        run_setup(&hooks, &ctx).unwrap();
        let where_ = std::fs::read_to_string(dir.path().join("where")).unwrap();
        let where_ = where_.trim();
        // canonicalize both because macOS /tmp -> /private/tmp
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
        };
        run_setup(&hooks, &ctx).unwrap();
        let v = std::fs::read_to_string(dir.path().join("from")).unwrap();
        assert_eq!(v.trim(), "source-ws");
    }

    #[test]
    fn run_setup_unsets_from_workspace_when_absent() {
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
            from_workspace: None,
        };
        // Make sure no inherited env var leaks in.
        // SAFETY: tests in this module set a process-global env var; we then
        // assert the child does NOT see it because we did not pass it through.
        // However Command inherits parent env by default — to validate
        // "absent means unset on the child", we'd need .env_clear(). Since we
        // do inherit, we instead just verify the runner itself does not set
        // a literal "DWM_FROM_WORKSPACE" when the option is None. We do that
        // by ensuring the parent's env doesn't have it set first.
        // SAFETY: single-threaded test scope for this var.
        unsafe { std::env::remove_var("DWM_FROM_WORKSPACE") };
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
        let ctx = HookContext {
            workspace_path: dir.path().to_path_buf(),
            workspace_name: "ws".into(),
            repo_root: dir.path().to_path_buf(),
            vcs_type: VcsType::Jj,
            from_workspace: None,
        };
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
        let ctx = HookContext {
            workspace_path: dir.path().to_path_buf(),
            workspace_name: "ws".into(),
            repo_root: dir.path().to_path_buf(),
            vcs_type: VcsType::Jj,
            from_workspace: None,
        };
        run_setup(&hooks, &ctx).unwrap();
    }
}
