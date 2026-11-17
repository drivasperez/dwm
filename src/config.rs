//! Project configuration shared by workspace provisioning and lifecycle hooks.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CopyMode {
    #[default]
    Auto,
    Copy,
}

/// What to do with a file selected for copying (via `copy` or
/// `.worktreeinclude`) that is ignored in the source but not at the new
/// workspace's revision. Copying it would create an untracked, unignored file
/// that is easy to commit by accident.
#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CopyUnignored {
    /// Leave the file out and print a warning.
    #[default]
    Skip,
    /// Fail workspace creation before copying anything.
    Error,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Checkout {
    #[default]
    Standard,
    Cow,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub copy: Vec<PathBuf>,
    pub copy_mode: CopyMode,
    pub copy_unignored: CopyUnignored,
    pub checkout: Checkout,
}

/// Contents of `.dwm.toml`. Unknown keys are an error at every level: the file
/// is dwm's own format, so an unrecognised key is either a typo or a setting
/// this dwm version predates, and in both cases silently ignoring it would
/// leave the user believing a setting took effect when it did not.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub scripts: crate::hooks::Hooks,
    pub workspace: WorkspaceConfig,
}

pub fn parse_toml(input: &str) -> Result<Config> {
    toml::from_str(input).context("parsing .dwm.toml")
}

pub fn load(root: &Path) -> Result<Config> {
    let path = root.join(".dwm.toml");
    if path.exists() {
        return parse_toml(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("invalid config in {}", path.display()));
    }
    let path = root.join("conductor.json");
    if path.exists() {
        return Ok(Config {
            scripts: parse_conductor_json(&std::fs::read_to_string(&path)?)?,
            ..Config::default()
        });
    }
    Ok(Config::default())
}

/// Parse the `scripts` of a Conductor `conductor.json`. Only scripts are
/// imported from Conductor, never dwm workspace settings.
///
/// Unlike `.dwm.toml`, this is lenient at every level: the file belongs to
/// Conductor and may carry Conductor-only fields (top-level or inside
/// `scripts`) that dwm has no use for.
pub fn parse_conductor_json(input: &str) -> Result<crate::hooks::Hooks> {
    #[derive(Deserialize)]
    struct ConductorScripts {
        #[serde(default)]
        setup: Option<String>,
        #[serde(default)]
        run: Option<String>,
        #[serde(default)]
        archive: Option<String>,
    }
    #[derive(Deserialize)]
    struct Conductor {
        #[serde(default)]
        scripts: Option<ConductorScripts>,
    }
    let config: Conductor = serde_json::from_str(input).context("parsing conductor.json")?;
    Ok(config
        .scripts
        .map(|s| crate::hooks::Hooks {
            setup: s.setup,
            run: s.run,
            archive: s.archive,
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_config_defaults_and_validation() {
        assert_eq!(
            parse_toml("").unwrap().workspace.checkout,
            Checkout::Standard
        );
        let cfg =
            parse_toml("[workspace]\ncopy = ['target']\ncopy_mode = 'copy'\ncheckout = 'cow'")
                .unwrap();
        assert_eq!(cfg.workspace.copy, [PathBuf::from("target")]);
        assert_eq!(cfg.workspace.copy_mode, CopyMode::Copy);
        assert!(parse_toml("[workspace]\ncheckout = 'unknown'").is_err());
        assert_eq!(
            parse_toml("").unwrap().workspace.copy_unignored,
            CopyUnignored::Skip
        );
        assert_eq!(
            parse_toml("[workspace]\ncopy_unignored = 'error'")
                .unwrap()
                .workspace
                .copy_unignored,
            CopyUnignored::Error
        );
        assert!(parse_toml("[workspace]\ncopy_unignored = 'warn'").is_err());
        assert!(parse_toml("[workspace]\ncoppy = []").is_err());
    }

    /// A typo like `[worksapce]` must fail loudly rather than silently
    /// disable `copy`.
    #[test]
    fn unknown_top_level_table_is_rejected() {
        let err = parse_toml("[worksapce]\ncopy = ['.zed']").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("worksapce"), "{msg}");
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let err = parse_toml("other = 'thing'").unwrap_err();
        assert!(format!("{err:#}").contains("other"));
    }

    #[test]
    fn unknown_script_key_in_dwm_toml_is_rejected() {
        let err = parse_toml("[scripts]\nsetpu = 'npm install'").unwrap_err();
        assert!(format!("{err:#}").contains("setpu"));
        // Reserved-but-unused keys are accepted.
        let cfg = parse_toml("[scripts]\nrun = 'a'\narchive = 'b'").unwrap();
        assert_eq!(cfg.scripts.run.as_deref(), Some("a"));
        assert_eq!(cfg.scripts.archive.as_deref(), Some("b"));
    }

    #[test]
    fn load_error_names_the_unknown_key_and_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".dwm.toml");
        std::fs::write(&path, "[worksapce]\ncopy = ['.zed']\n").unwrap();
        let msg = format!("{:#}", load(dir.path()).unwrap_err());
        assert!(msg.contains("worksapce"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    #[test]
    fn conductor_json_stays_lenient() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("conductor.json"),
            r#"{"scripts":{"setup":"x","conductorOnly":"y"},"runScriptMode":"concurrent"}"#,
        )
        .unwrap();
        assert_eq!(
            load(dir.path()).unwrap().scripts.setup.as_deref(),
            Some("x")
        );
    }

    #[test]
    fn conductor_null_scripts_and_dwm_precedence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("conductor.json"), r#"{"scripts":null}"#).unwrap();
        assert_eq!(
            load(dir.path()).unwrap().scripts,
            crate::hooks::Hooks::default()
        );
        std::fs::write(dir.path().join("conductor.json"), "invalid").unwrap();
        std::fs::write(
            dir.path().join(".dwm.toml"),
            "[workspace]\ncopy = ['cache']",
        )
        .unwrap();
        assert_eq!(
            load(dir.path()).unwrap().workspace.copy,
            [PathBuf::from("cache")]
        );
    }
}
