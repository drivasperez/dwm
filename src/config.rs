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
    pub checkout: Checkout,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
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
            .with_context(|| format!("in {}", path.display()));
    }
    let path = root.join("conductor.json");
    if path.exists() {
        // Only scripts are imported from Conductor, never dwm workspace settings.
        #[derive(Deserialize)]
        struct Conductor {
            #[serde(default)]
            scripts: Option<crate::hooks::Hooks>,
        }
        let config: Conductor = serde_json::from_str(&std::fs::read_to_string(&path)?)
            .context("parsing conductor.json")?;
        return Ok(Config {
            scripts: config.scripts.unwrap_or_default(),
            ..Config::default()
        });
    }
    Ok(Config::default())
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
        assert!(parse_toml("[workspace]\ncoppy = []").is_err());
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
