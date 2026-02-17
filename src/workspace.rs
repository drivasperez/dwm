use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;

use crate::{jj, names};

fn jjws_base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".jjws"))
}

fn repo_dir(repo_name: &str) -> Result<PathBuf> {
    Ok(jjws_base_dir()?.join(repo_name))
}

fn main_repo_path(repo_name: &str) -> Result<PathBuf> {
    let repo_dir = repo_dir(repo_name)?;
    let main_repo_file = repo_dir.join(".main-repo");
    let path = fs::read_to_string(&main_repo_file)
        .with_context(|| format!("could not read {}", main_repo_file.display()))?;
    Ok(PathBuf::from(path.trim()))
}

fn ensure_repo_dir(repo_name: &str, main_repo_root: &PathBuf) -> Result<PathBuf> {
    let dir = repo_dir(repo_name)?;
    fs::create_dir_all(&dir)?;
    let main_repo_file = dir.join(".main-repo");
    if !main_repo_file.exists() {
        fs::write(&main_repo_file, main_repo_root.to_string_lossy().as_ref())?;
    }
    Ok(dir)
}

pub fn new_workspace(name: Option<String>) -> Result<()> {
    let repo_name = jj::repo_name()?;
    let root = jj::root()?;
    let dir = ensure_repo_dir(&repo_name, &root)?;

    let ws_name = match name {
        Some(n) => n,
        None => names::generate_unique(&dir),
    };

    let ws_path = dir.join(&ws_name);
    if ws_path.exists() {
        bail!("workspace '{}' already exists at {}", ws_name, ws_path.display());
    }

    eprintln!("creating workspace '{}'...", ws_name);
    jj::workspace_add(&ws_path, &ws_name)?;
    eprintln!("workspace '{}' created at {}", ws_name, ws_path.display());

    // stdout: path for shell wrapper to cd into
    println!("{}", ws_path.display());
    Ok(())
}

pub fn delete_workspace(name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let jjws_base = jjws_base_dir()?;

    let (repo_name_str, ws_name) = match name {
        Some(name) => {
            // Deleting a named workspace: resolve repo from cwd
            let repo_name_str = if cwd.starts_with(&jjws_base) {
                let relative = cwd.strip_prefix(&jjws_base)?;
                relative.components()
                    .next()
                    .context("could not determine repo from workspace path")?
                    .as_os_str()
                    .to_string_lossy()
                    .to_string()
            } else {
                jj::repo_name()?
            };
            (repo_name_str, name)
        }
        None => {
            // Deleting current workspace: must be inside one
            if !cwd.starts_with(&jjws_base) {
                bail!(
                    "not inside a jjws workspace (current dir must be under {})",
                    jjws_base.display()
                );
            }
            let relative = cwd.strip_prefix(&jjws_base)?;
            let components: Vec<&std::ffi::OsStr> = relative.components()
                .map(|c| c.as_os_str())
                .collect();
            if components.len() < 2 {
                bail!("could not determine workspace name from current directory");
            }
            (
                components[0].to_string_lossy().to_string(),
                components[1].to_string_lossy().to_string(),
            )
        }
    };

    let ws_path = jjws_base.join(&repo_name_str).join(&ws_name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", ws_name, ws_path.display());
    }

    // Get main repo path before forgetting
    let main_repo = main_repo_path(&repo_name_str)?;

    eprintln!("forgetting workspace '{}'...", ws_name);
    jj::workspace_forget_from(&main_repo, &ws_name)?;

    eprintln!("removing {}...", ws_path.display());
    fs::remove_dir_all(&ws_path)?;
    eprintln!("workspace '{}' deleted", ws_name);

    // If we're inside the deleted workspace, cd to main repo
    if cwd.starts_with(&ws_path) {
        println!("{}", main_repo.display());
    }
    Ok(())
}

pub fn list_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    // Figure out which repo we're in by checking if we're in a jjws dir or a jj repo
    let cwd = std::env::current_dir()?;
    let jjws_base = jjws_base_dir()?;

    let (repo_name_str, main_repo) = if cwd.starts_with(&jjws_base) {
        // We're inside a jjws workspace
        let relative = cwd.strip_prefix(&jjws_base)?;
        let repo_name_str = relative.components()
            .next()
            .context("could not determine repo from workspace path")?
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let main_repo = main_repo_path(&repo_name_str)?;
        (repo_name_str, main_repo)
    } else {
        // We're in a regular jj repo
        let repo_name_str = jj::repo_name()?;
        let main_repo = jj::root()?;
        (repo_name_str, main_repo)
    };

    let repo_dir = repo_dir(&repo_name_str)?;
    if !repo_dir.exists() {
        return Ok(Vec::new());
    }

    // Get jj workspace list for change IDs, descriptions, bookmarks
    let jj_workspaces = jj::workspace_list_from(&main_repo).unwrap_or_default();

    let mut entries = Vec::new();

    // Find info for the default workspace
    let main_info = jj_workspaces.iter()
        .find(|(n, _)| n == "default")
        .map(|(_, info)| info.clone())
        .unwrap_or_default();

    // Add main repo entry
    let main_stat = jj::diff_stat(&main_repo, "trunk()", "@").unwrap_or_default();
    let main_modified = fs::metadata(&main_repo)
        .and_then(|m| m.modified())
        .ok();
    let main_description = if main_info.description.trim().is_empty() {
        jj::latest_description(&main_repo, "default")
    } else {
        main_info.description.clone()
    };
    entries.push(WorkspaceEntry {
        name: "default".to_string(),
        path: main_repo.clone(),
        last_modified: main_modified,
        diff_stat: main_stat,
        is_main: true,
        change_id: main_info.change_id.clone(),
        description: main_description,
        bookmarks: main_info.bookmarks.clone(),
    });

    // Scan workspace dirs
    let read_dir = fs::read_dir(&repo_dir)?;
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();

        // Get info for this workspace from jj list
        let ws_info = jj_workspaces.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, info)| info.clone());

        let has_info = ws_info.is_some();
        let info = ws_info.unwrap_or_default();

        let stat = if has_info {
            jj::diff_stat(&main_repo, "trunk()", &format!("{}@", name)).unwrap_or_default()
        } else {
            jj::DiffStat::default()
        };

        let description = if info.description.trim().is_empty() {
            jj::latest_description(&main_repo, &name)
        } else {
            info.description.clone()
        };

        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok();

        entries.push(WorkspaceEntry {
            name,
            path,
            last_modified: modified,
            diff_stat: stat,
            is_main: false,
            change_id: info.change_id,
            description,
            bookmarks: info.bookmarks,
        });
    }

    Ok(entries)
}

#[derive(Debug)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: PathBuf,
    pub last_modified: Option<std::time::SystemTime>,
    pub diff_stat: jj::DiffStat,
    pub is_main: bool,
    pub change_id: String,
    pub description: String,
    pub bookmarks: Vec<String>,
}
