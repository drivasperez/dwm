use anyhow::{Context, Result, bail};
use owo_colors::OwoColorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::rex::{self, RexApi, Shown};
use crate::{agent, hooks, names, pool, vcs};

/// Whether a workspace's changes have been merged into trunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeStatus {
    Merged,
    Unmerged,
}

/// Controls whether progress messages are printed to stderr during deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutput {
    /// Print progress messages to stderr.
    Verbose,
    /// Suppress progress messages (used by the TUI which owns the alternate screen).
    Quiet,
}

/// Where `switch`, `new` and the picker put you once the workspace is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Inside Rex, focus a pane already in the workspace or open a session
    /// for it. Elsewhere, change directory.
    Rex,
    /// Always change the current shell's directory.
    Here,
}

/// Whether a person is at the terminal dwm runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Interactive,
    /// Run by an agent or a script, with no one watching stderr.
    Unattended,
}

/// The placement to use for a request. An unattended dwm, such as one an
/// agent runs inside a Rex pane, changes directory rather than opening Rex
/// sessions and switching the app's view away from what the person is doing.
pub fn effective_placement(requested: Placement, terminal: Terminal) -> Placement {
    match terminal {
        Terminal::Interactive => requested,
        Terminal::Unattended => Placement::Here,
    }
}

/// What `dwm delete` does about Rex panes still open in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPanes {
    /// Refuse to delete, naming the panes.
    Refuse,
    /// Ask on the terminal whether to close them.
    Ask,
    /// Close them, ending the programs in them. [`OpenPanes::Ask`] does this
    /// once the person agrees; nothing else asks for it outside tests.
    #[cfg_attr(not(test), allow(dead_code))]
    Close,
}

/// Return `true` if `cwd` is equal to or a subdirectory of `ws_path`.
fn is_inside(cwd: &std::path::Path, ws_path: &std::path::Path) -> bool {
    cwd.starts_with(ws_path)
}

/// Suffix appended to the repo's directory name to form its dwm directory.
const DWM_DIR_SUFFIX: &str = ".dwm";

/// Return the per-repo dwm directory: `<repo_parent>/<repo_name>.dwm/`, a
/// sibling of the repo root.
///
/// It lives outside the repo so that workspaces are not nested inside the
/// main checkout. A jj workspace has no `.git` of its own, so git (and editors
/// that use it, like Zed) walk up to the enclosing repo; if that repo ignores
/// the workspace's path, every file in the workspace is treated as ignored and
/// hidden from search.
///
/// Falls back to `<repo_root>/.dwm/` for a root with no final component (`/`),
/// which has no parent to put a sibling in.
pub(crate) fn dwm_dir(repo_root: &Path) -> PathBuf {
    match (repo_root.parent(), repo_root.file_name()) {
        (Some(parent), Some(name)) => {
            let mut dir_name = name.to_os_string();
            dir_name.push(DWM_DIR_SUFFIX);
            parent.join(dir_name)
        }
        _ => repo_root.join(DWM_DIR_SUFFIX),
    }
}

/// Inverse of [`dwm_dir`]: given a candidate dwm directory, return the repo
/// root it would belong to, if its name has the dwm suffix.
fn repo_root_for_dwm_dir(dwm: &Path) -> Option<PathBuf> {
    let name = dwm.file_name()?.to_str()?;
    let repo_name = name.strip_suffix(DWM_DIR_SUFFIX)?;
    if repo_name.is_empty() {
        return None;
    }
    Some(dwm.parent()?.join(repo_name))
}

/// Return the per-repo worktrees directory: `<repo_parent>/<repo_name>.dwm/worktrees/`.
///
/// This is the namespace workspaces live under. Sibling dirs under the dwm
/// directory (e.g. `agent-status/`) hold dwm-internal state and never collide
/// with workspace names.
pub(crate) fn worktrees_dir(repo_root: &Path) -> PathBuf {
    dwm_dir(repo_root).join("worktrees")
}

/// Return `true` if `dir` looks like a VCS root (contains `.jj` or `.git`).
fn is_vcs_root(dir: &Path) -> bool {
    dir.join(".jj").is_dir() || dir.join(".git").exists()
}

/// Reject workspace names that would break path construction. We allow leading
/// dots (with this layout, `.foo` is just a directory name inside
/// `<repo>.dwm/worktrees/` — no collision with `agent-status/` or other internal
/// dirs), but `.`, `..`, and names with path separators are still nonsense.
fn validate_workspace_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        bail!("invalid workspace name '{}'", name);
    }
    Ok(())
}

/// Walk up from `cwd` looking for `<repo_parent>/<repo_name>.dwm/worktrees/<workspace>/`.
///
/// Returns `Some((repo_root, workspace_name, workspace_path))` when the cwd
/// is inside a dwm-managed workspace, or `None` otherwise.
pub fn find_dwm_workspace(cwd: &Path) -> Option<(PathBuf, String, PathBuf)> {
    // Walk from cwd upwards and check each ancestor.
    let mut current = cwd.to_path_buf();
    loop {
        // If `current`'s parent is `worktrees`, its grandparent is
        // `<repo_name>.dwm`, and the sibling `<repo_name>` is a VCS root, then
        // `current` is the workspace dir.
        if let Some(parent) = current.parent()
            && parent.file_name().and_then(|n| n.to_str()) == Some("worktrees")
            && let Some(dwm) = parent.parent()
            && let Some(root) = repo_root_for_dwm_dir(dwm)
            && is_vcs_root(&root)
        {
            let ws_name = current
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Some((root, ws_name, current));
        }
        if !current.pop() {
            break;
        }
    }
    None
}

/// Return the path to the cross-platform repos registry file used by `--all`.
fn registry_path() -> Result<PathBuf> {
    // Allows benchmarks to keep their disposable repositories out of the
    // user's multi-repository dashboard on platforms without XDG data dirs.
    if let Some(path) = std::env::var_os("DWM_REGISTRY_PATH") {
        return Ok(PathBuf::from(path));
    }
    let data = dirs::data_dir().context("could not determine data directory")?;
    Ok(data.join("dwm").join("repos.txt"))
}

/// Append `repo_root` to the registry file if not already present.
///
/// The path is canonicalized so that invocations via a symlinked alias and the
/// real path don't produce duplicate registry entries. Falls back to the
/// original path if canonicalization fails (e.g. it doesn't exist yet).
///
/// Best-effort: errors are returned but the caller may choose to ignore them.
fn register_repo(repo_root: &Path) -> Result<()> {
    let path = registry_path()?;
    let canonical = fs::canonicalize(repo_root)
        .unwrap_or_else(|_| repo_root.to_path_buf())
        .to_string_lossy()
        .into_owned();

    // Read existing entries (if any) so we can dedupe.
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == canonical) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Append, ensuring a trailing newline.
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{}", canonical)?;
    Ok(())
}

/// Read the registry, drop entries whose paths no longer exist or are no longer
/// dwm-managed (no `<repo>.dwm/` sibling), rewrite the file with the survivors, and
/// return them.
fn read_and_prune_registry() -> Result<Vec<PathBuf>> {
    let path = registry_path()?;
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut survivors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut changed = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            changed = true;
            continue;
        }
        if !seen.insert(trimmed.to_string()) {
            // duplicate
            changed = true;
            continue;
        }
        let p = PathBuf::from(trimmed);
        if !p.exists() || !dwm_dir(&p).exists() {
            changed = true;
            continue;
        }
        survivors.push(p);
    }

    if changed {
        // Rewrite with surviving entries. Best-effort: ignore write errors,
        // since concurrent writers may race with us.
        let mut buf = String::new();
        for p in &survivors {
            buf.push_str(&p.to_string_lossy());
            buf.push('\n');
        }
        let _ = fs::write(&path, buf);
    }

    Ok(survivors)
}

/// Common dependencies threaded through workspace operations, grouped so they
/// can be injected in tests without touching the real filesystem or VCS.
struct WorkspaceDeps {
    backend: Box<dyn vcs::VcsBackend>,
    cwd: PathBuf,
    /// The Rex server, when dwm runs inside Rex.
    rex: Option<Box<dyn crate::rex::RexApi>>,
}

/// Resolve the repo root for the current working directory, whether the cwd
/// is inside a dwm workspace (`<root>.dwm/worktrees/<ws>/...`) or anywhere
/// else inside the repo.
fn resolve_repo_root(deps: &WorkspaceDeps) -> Result<PathBuf> {
    if let Some((root, _, _)) = find_dwm_workspace(&deps.cwd) {
        return Ok(root);
    }
    deps.backend.root_from(&deps.cwd)
}

/// Create a new workspace, auto-detecting the VCS from the current directory.
///
/// Prints the new workspace path to stdout so the shell wrapper can `cd` into it.
pub fn new_workspace(
    name: Option<String>,
    at: Option<&str>,
    from: Option<&str>,
    placement: Placement,
    command: &[String],
) -> Result<()> {
    let report = rex::StatusReport::start(&match &name {
        Some(n) => format!("Creating workspace {n}"),
        None => "Creating a workspace".to_string(),
    });
    let cwd = std::env::current_dir()?;
    // If we're inside a dwm workspace the cwd may be a workspace dir; we still
    // want to detect from the underlying VCS, which works because both jj and
    // git treat the workspace dir as part of the same repo.
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let created = new_workspace_inner(&deps, name, at, from, placement, command)?;
    report.done(&format!("Created workspace {}", created.name));
    if let Some(dir) = created.cd {
        // stdout: path for shell wrapper to cd into
        println!("{}", dir.display());
    }
    Ok(())
}

/// What [`new_workspace_inner`] made.
#[derive(Debug)]
struct NewOutcome {
    name: String,
    /// Where the shell should `cd`, unless Rex showed a session instead.
    cd: Option<PathBuf>,
}

/// Testable core of [`new_workspace`] that accepts injected [`WorkspaceDeps`].
///
/// `command` is typed into a new Rex session opened in the workspace, so it
/// needs Rex and [`Placement::Rex`].
fn new_workspace_inner(
    deps: &WorkspaceDeps,
    name: Option<String>,
    at: Option<&str>,
    from: Option<&str>,
    placement: Placement,
    command: &[String],
) -> Result<NewOutcome> {
    if !command.is_empty() && (deps.rex.is_none() || placement == Placement::Here) {
        bail!(
            "running a command in the new workspace needs a Rex session; run dwm inside Rex without --here"
        );
    }
    let root = resolve_repo_root(deps)?;
    let worktrees = worktrees_dir(&root);
    fs::create_dir_all(&worktrees)?;

    // Best-effort: register the repo for `--all`.
    let _ = register_repo(&root);

    // Resolve --from to a change ID by looking up the source workspace.
    let resolved_at;
    let at = if let Some(ws_name) = from {
        let workspaces = deps.backend.workspace_list(&root)?;
        let (_name, info) = workspaces
            .iter()
            .find(|(n, _)| n == ws_name)
            .with_context(|| format!("workspace '{}' not found", ws_name))?;
        resolved_at = deps
            .backend
            .workspace_start_revision(&root, ws_name, info)?;
        Some(resolved_at.as_str())
    } else {
        at
    };

    let ws_name = match name {
        Some(n) => {
            validate_workspace_name(&n)?;
            n
        }
        None => names::generate_unique(&worktrees),
    };

    let ws_path = worktrees.join(&ws_name);
    if ws_path.exists() {
        bail!(
            "workspace '{}' already exists at {}",
            ws_name,
            ws_path.display()
        );
    }

    // Load post-creation hooks before doing any VCS work so a malformed
    // .dwm.toml / conductor.json is reported up-front rather than after we've
    // already provisioned the workspace.
    let config = crate::config::load(&root)?;
    if config.workspace.checkout == crate::config::Checkout::Cow
        && deps.backend.vcs_type() == vcs::VcsType::Jj
    {
        bail!(
            "workspace.checkout = 'cow' is experimental and currently supports Git only; jj supports workspace.copy"
        );
    }
    let source = match from {
        Some(name) if name != deps.backend.main_workspace_name() => worktrees.join(name),
        _ => root.clone(),
    };
    let resolved = at
        .map(|rev| deps.backend.resolve_revision(&root, rev))
        .transpose()?;
    let seed = crate::seed::SeedPlan::prepare(&source, deps.backend.vcs_type(), &config.workspace)?;

    eprintln!("{} workspace '{}'...", "creating".cyan(), ws_name.bold());
    deps.backend.workspace_add_configured(
        &root,
        &ws_path,
        &ws_name,
        resolved.as_deref(),
        &source,
        &config.workspace,
    )?;
    seed.apply(&ws_path, deps.backend.vcs_type(), &config.workspace)
        .with_context(|| {
            format!(
                "workspace created at {} but file copying failed; setup was not run",
                ws_path.display()
            )
        })?;
    eprintln!(
        "{} workspace '{}' created at {}",
        "✓".green(),
        ws_name.bold(),
        ws_path.display().dimmed()
    );

    let hook_ctx = hooks::HookContext {
        workspace_path: ws_path.clone(),
        workspace_name: ws_name.clone(),
        repo_root: root.clone(),
        vcs_type: deps.backend.vcs_type(),
        from_workspace: from.map(|s| s.to_string()),
    };
    hooks::run_setup(&config.scripts, &hook_ctx)?;

    let cd = open_new_workspace(deps.rex.as_deref(), &ws_name, &ws_path, placement, command)?;
    Ok(NewOutcome { name: ws_name, cd })
}

/// Take the person into a workspace that was just created. Inside Rex that
/// means a new session for it, running `command` if one was given. Returns
/// where the shell should `cd` when no session was shown.
fn open_new_workspace(
    rex: Option<&dyn RexApi>,
    ws_name: &str,
    ws_path: &Path,
    placement: Placement,
    command: &[String],
) -> Result<Option<PathBuf>> {
    let cd = Some(ws_path.to_path_buf());
    let Some(rex) = rex else { return Ok(cd) };
    if placement == Placement::Here {
        return Ok(cd);
    }
    let initial_input =
        (!command.is_empty()).then(|| format!("{}\r", rex::shell_command_line(command)));
    let req = rex::OpenSession {
        label: ws_name.to_string(),
        cwd: ws_path.to_path_buf(),
        create: if initial_input.is_some() {
            rex::CreateWhen::Always
        } else {
            rex::CreateWhen::OnlyIfShown
        },
        initial_input,
    };
    let opened = match rex.open_session(&req) {
        Ok(o) => o,
        Err(e) if command.is_empty() => {
            eprintln!(
                "{} could not open a Rex session: {e:#}",
                "warning:".yellow()
            );
            return Ok(cd);
        }
        Err(e) => return Err(e.context("could not open a Rex session for the command")),
    };
    match opened.shown {
        Shown::Yes => {
            eprintln!("{} opened Rex session '{}'", "→".cyan(), ws_name.bold());
            Ok(None)
        }
        Shown::No(reason) => {
            if opened.created {
                eprintln!(
                    "{} started Rex session '{}', but Rex didn't switch to it",
                    "→".cyan(),
                    ws_name.bold()
                );
            }
            warn_not_shown(&reason);
            Ok(cd)
        }
    }
}

/// Explain why Rex didn't switch sessions. The usual cause is that the Rex
/// app doesn't take actions from scripts until Remote Control is on.
fn warn_not_shown(reason: &str) {
    eprintln!(
        "{} Rex won't switch sessions for dwm, so changing directory instead. Turn on Remote Control in Rex's Settings > General, or pass --here to skip Rex.",
        "note:".yellow()
    );
    eprintln!("  {}", reason.dimmed());
}

/// The Rex session label for a workspace path: the workspace's name, or the
/// repo directory's name for the main workspace.
fn session_label_for(path: &Path) -> String {
    if let Some((_, name, _)) = find_dwm_workspace(path) {
        return name;
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Take the person to an existing workspace. Inside Rex, focus a pane already
/// in it, or open a session for it. Returns where the shell should `cd` when
/// neither happened, including when the current pane is already there.
fn go_to_workspace(
    rex: Option<&dyn RexApi>,
    cwd: &Path,
    ws_path: &Path,
    placement: Placement,
) -> Result<Option<PathBuf>> {
    let cd = Some(ws_path.to_path_buf());
    let Some(rex) = rex else { return Ok(cd) };
    if placement == Placement::Here {
        return Ok(cd);
    }
    // Already there: just cd, unless this pane is about to close anyway.
    if rex.current_block().is_some()
        && rex.current_pane() == rex::CurrentPane::Lasting
        && is_inside(&rex::real_path(cwd), &rex::real_path(ws_path))
    {
        return Ok(cd);
    }
    let panes = match rex.panes() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{} could not list Rex panes: {e:#}", "warning:".yellow());
            return Ok(cd);
        }
    };
    let inside = rex::panes_inside(&panes, ws_path, rex.current_block());
    if let Some(pane) = rex::pick_pane(&inside) {
        return match rex.focus(pane) {
            Ok(Shown::Yes) => {
                eprintln!("{} switched to Rex {}", "→".cyan(), pane.describe());
                Ok(None)
            }
            Ok(Shown::No(reason)) => {
                warn_not_shown(&reason);
                Ok(cd)
            }
            Err(e) => {
                eprintln!(
                    "{} could not focus the Rex pane: {e:#}",
                    "warning:".yellow()
                );
                Ok(cd)
            }
        };
    }
    let label = session_label_for(ws_path);
    let req = rex::OpenSession {
        label: label.clone(),
        cwd: ws_path.to_path_buf(),
        initial_input: None,
        create: rex::CreateWhen::OnlyIfShown,
    };
    match rex.open_session(&req) {
        Ok(rex::Opened {
            shown: Shown::Yes, ..
        }) => {
            eprintln!("{} opened Rex session '{}'", "→".cyan(), label.bold());
            Ok(None)
        }
        Ok(rex::Opened {
            shown: Shown::No(reason),
            ..
        }) => {
            warn_not_shown(&reason);
            Ok(cd)
        }
        Err(e) => {
            eprintln!(
                "{} could not open a Rex session: {e:#}",
                "warning:".yellow()
            );
            Ok(cd)
        }
    }
}

/// Lease a workspace from `pool_name` and print its path to stdout.
pub fn lease_workspace(pool_name: &str, at: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let path = lease_workspace_inner(&deps, pool_name, at)?;
    println!("{}", path.display());
    Ok(())
}

/// Testable core of [`lease_workspace`]. Reuses the lowest-numbered member of
/// the pool that is not leased, not open in a Rex pane, and has nothing to
/// lose, moving it to a fresh change on `at` (trunk by default). When none
/// qualifies, creates the next member. Returns the leased workspace's path.
fn lease_workspace_inner(
    deps: &WorkspaceDeps,
    pool_name: &str,
    at: Option<&str>,
) -> Result<PathBuf> {
    validate_workspace_name(pool_name)?;
    let root = resolve_repo_root(deps)?;
    let dwm = dwm_dir(&root);
    let worktrees = worktrees_dir(&root);
    fs::create_dir_all(&worktrees)?;
    let _lock = pool::lock(&dwm)?;

    let rex = deps.rex.as_deref();
    let panes = rex.and_then(|r| r.panes().ok());
    let holder = rex.and_then(|r| r.current_block());
    let revision = match at {
        Some(rev) => rev.to_string(),
        None => deps.backend.trunk_revision(&root),
    };

    let mut names = Vec::new();
    for entry in fs::read_dir(&worktrees)? {
        let entry = entry?;
        if entry.path().is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    let members = pool::members(pool_name, names.iter().map(String::as_str));

    for (_, name) in &members {
        let ws_path = worktrees.join(name);
        let busy = if let Some(lease) = pool::read_lease(&dwm, name)
            && pool::lease_state(&lease, panes.as_deref()) == pool::LeaseState::Held
        {
            Some("leased".to_string())
        } else if let Some(p) = panes.as_deref()
            && let Some(pane) = rex::panes_inside(p, &ws_path, None).first()
        {
            Some(format!("open in Rex {}", pane.describe()))
        } else {
            match deps.backend.prune_verdict(&root, &ws_path, name) {
                vcs::PruneVerdict::Prunable => None,
                vcs::PruneVerdict::Keep(reason) => Some(reason),
            }
        };
        if let Some(reason) = busy {
            eprintln!("{} {}: {}", "skip ".dimmed(), name, reason.dimmed());
            continue;
        }
        deps.backend
            .reset_workspace(&root, &ws_path, name, &revision)
            .with_context(|| format!("resetting '{name}' to {revision}"))?;
        pool::write_lease(&dwm, &pool::Lease::new(name, holder))?;
        eprintln!("{} leased '{}' at {}", "✓".green(), name.bold(), revision);
        return Ok(ws_path);
    }

    let name = pool::next_member_name(pool_name, &members);
    let created = new_workspace_inner(
        deps,
        Some(name),
        Some(&revision),
        None,
        Placement::Here,
        &[],
    )?;
    pool::write_lease(&dwm, &pool::Lease::new(&created.name, holder))?;
    eprintln!(
        "{} leased new workspace '{}'",
        "✓".green(),
        created.name.bold()
    );
    Ok(worktrees.join(&created.name))
}

/// End the lease on a pool workspace, named or inferred from the current
/// directory, so the next `dwm lease` can reuse it.
pub fn release_workspace(name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: None,
    };
    release_workspace_inner(&deps, name)
}

fn release_workspace_inner(deps: &WorkspaceDeps, name: Option<String>) -> Result<()> {
    let (root, name) = match name {
        Some(n) => (resolve_repo_root(deps)?, n),
        None => {
            let (root, ws, _) = find_dwm_workspace(&deps.cwd)
                .context("not inside a dwm workspace; provide a workspace name")?;
            (root, ws)
        }
    };
    let dwm = dwm_dir(&root);
    let _lock = pool::lock(&dwm)?;
    if pool::remove_lease(&dwm, &name)? {
        eprintln!("{} released '{}'", "✓".green(), name.bold());
    } else {
        eprintln!("'{}' was not leased", name);
    }
    Ok(())
}

/// Go to a workspace chosen in the picker by its path. Prints the path for
/// the shell wrapper when it should `cd` there.
pub fn switch_to_path(path: &Path, placement: Placement) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let rex = rex::connect();
    if let Some(dir) = go_to_workspace(rex.as_deref(), &cwd, path, placement)? {
        println!("{}", dir.display());
    }
    Ok(())
}

/// Deletes a workspace. Returns `true` if the cwd was inside the deleted
/// workspace and a redirect path was printed to stdout.
/// Delete a workspace by name (or infer from cwd).
pub fn delete_workspace(
    name: Option<String>,
    output: DeleteOutput,
    open_panes: OpenPanes,
) -> Result<bool> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    if let Some(redirect) = delete_workspace_inner(&deps, name, output, open_panes)? {
        println!("{}", redirect.display());
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Returns the path the shell should cd to if cwd was inside the deleted workspace.
fn delete_workspace_inner(
    deps: &WorkspaceDeps,
    name: Option<String>,
    output: DeleteOutput,
    open_panes: OpenPanes,
) -> Result<Option<PathBuf>> {
    let verbose = output == DeleteOutput::Verbose;

    let (root, ws_name) = match name {
        Some(n) => {
            let root = resolve_repo_root(deps)?;
            (root, n)
        }
        None => {
            let (root, ws, _) = find_dwm_workspace(&deps.cwd)
                .context("not inside a dwm workspace; provide a workspace name")?;
            (root, ws)
        }
    };

    let ws_path = worktrees_dir(&root).join(&ws_name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", ws_name, ws_path.display());
    }

    if let Some(rex) = deps.rex.as_deref() {
        deal_with_open_panes(rex, &ws_name, &ws_path, open_panes)?;
    }

    if verbose {
        eprintln!(
            "{} workspace '{}'...",
            "forgetting".yellow(),
            ws_name.bold()
        );
    }
    deps.backend.workspace_remove(&root, &ws_name, &ws_path)?;

    if ws_path.exists() {
        if verbose {
            eprintln!("{} {}...", "removing".red(), ws_path.display().dimmed());
        }
        fs::remove_dir_all(&ws_path)?;
    }

    // Clean up agent status files for this workspace
    agent::remove_agent_statuses_for_workspace(&dwm_dir(&root), &ws_name);
    let _ = pool::remove_lease(&dwm_dir(&root), &ws_name);

    if verbose {
        eprintln!("{} workspace '{}' deleted", "✓".green(), ws_name.bold());
    }

    if is_inside(&deps.cwd, &ws_path) {
        Ok(Some(root))
    } else {
        Ok(None)
    }
}

/// Before deleting a workspace, find Rex panes other than dwm's own that are
/// still in it, and refuse, ask, or close them as `policy` says. Deleting the
/// directory under a running agent would leave it working in a void.
fn deal_with_open_panes(
    rex: &dyn RexApi,
    ws_name: &str,
    ws_path: &Path,
    policy: OpenPanes,
) -> Result<()> {
    let panes = match rex.panes() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{} could not check Rex for panes in '{}': {e:#}",
                "warning:".yellow(),
                ws_name
            );
            return Ok(());
        }
    };
    let inside = rex::panes_inside(&panes, ws_path, rex.current_block());
    if inside.is_empty() {
        return Ok(());
    }
    let list = inside
        .iter()
        .map(|p| p.describe())
        .collect::<Vec<_>>()
        .join(", ");
    let close = match policy {
        OpenPanes::Close => true,
        OpenPanes::Refuse => false,
        OpenPanes::Ask => {
            eprintln!(
                "{} workspace '{}' is open in Rex: {}",
                "!".yellow().bold(),
                ws_name.bold(),
                list
            );
            eprint!(
                "  {} Close {} and delete the workspace? [y/N] ",
                "?".bold().cyan(),
                if inside.len() == 1 { "it" } else { "them" }
            );
            agent::confirm_from_tty()?
        }
    };
    if !close {
        bail!(
            "workspace '{}' is open in Rex ({}); close {} first",
            ws_name,
            list,
            if inside.len() == 1 { "it" } else { "them" }
        );
    }
    for pane in inside {
        rex.close_pane(pane)
            .with_context(|| format!("closing Rex {}", pane.describe()))?;
    }
    Ok(())
}

/// Why prune must keep a workspace that Rex panes are open in, if any are.
fn open_panes_verdict(
    panes: &[rex::Pane],
    ws_path: &Path,
    current_block: Option<&str>,
) -> Option<vcs::PruneVerdict> {
    let inside = rex::panes_inside(panes, ws_path, current_block);
    let first = inside.first()?;
    let more = match inside.len() {
        1 => String::new(),
        n => format!(" and {} more", n - 1),
    };
    Some(vcs::PruneVerdict::Keep(format!(
        "open in Rex {}{more}",
        first.describe()
    )))
}

/// Why prune must keep a leased pool workspace, if it is leased.
fn leased_verdict(
    dwm: &Path,
    ws_name: &str,
    panes: Option<&[rex::Pane]>,
) -> Option<vcs::PruneVerdict> {
    let lease = pool::read_lease(dwm, ws_name)?;
    (pool::lease_state(&lease, panes) == pool::LeaseState::Held)
        .then(|| vcs::PruneVerdict::Keep(format!("leased; `dwm release {ws_name}` ends the lease")))
}

/// Whether `dwm prune` only reports or also deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneMode {
    /// Report each workspace's verdict without deleting anything.
    DryRun,
    /// Delete every workspace whose verdict is prunable.
    Delete,
}

/// Delete (or, in [`PruneMode::DryRun`], list) workspaces whose work is safe to
/// discard. Prints a redirect path to stdout if cwd was inside a deleted one.
pub fn prune_workspaces(mode: PruneMode) -> Result<()> {
    let report =
        (mode == PruneMode::Delete).then(|| rex::StatusReport::start("Pruning workspaces"));
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let outcome = prune_workspaces_inner(&deps, mode)?;
    if let Some(report) = report {
        report.done(&match outcome.deleted.len() {
            1 => "Pruned 1 workspace".to_string(),
            n => format!("Pruned {n} workspaces"),
        });
    }
    if let Some(redirect) = outcome.redirect {
        println!("{}", redirect.display());
    }
    Ok(())
}

/// What [`prune_workspaces_inner`] decided and did.
#[derive(Debug, Default)]
struct PruneOutcome {
    /// Every non-main workspace with its verdict, sorted by name.
    verdicts: Vec<(String, vcs::PruneVerdict)>,
    /// Workspaces that were actually deleted.
    deleted: Vec<String>,
    /// Where the shell should `cd` if cwd was inside a deleted workspace.
    redirect: Option<PathBuf>,
}

fn prune_workspaces_inner(deps: &WorkspaceDeps, mode: PruneMode) -> Result<PruneOutcome> {
    let root = resolve_repo_root(deps)?;
    let worktrees = worktrees_dir(&root);
    let main_ws_name = deps.backend.main_workspace_name();

    let mut names: Vec<String> = Vec::new();
    if worktrees.exists() {
        for entry in fs::read_dir(&worktrees)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // The main workspace lives at the repo root, never under
            // worktrees/, but guard against a directory that shares its name.
            if name != main_ws_name {
                names.push(name);
            }
        }
    }
    names.sort();

    let mut outcome = PruneOutcome::default();
    if names.is_empty() {
        eprintln!("no workspaces to prune");
        return Ok(outcome);
    }

    // Panes don't move between workspaces in the time prune takes, so one
    // look covers every workspace.
    let panes = deps.rex.as_deref().and_then(|r| r.panes().ok());
    let current_block = deps.rex.as_deref().and_then(|r| r.current_block());

    let mut failures = 0;
    for name in names {
        let ws_path = worktrees.join(&name);
        // Check immediately before deleting so the verdict is as fresh as
        // possible when acting on it.
        let verdict = leased_verdict(&dwm_dir(&root), &name, panes.as_deref())
            .or_else(|| {
                panes
                    .as_deref()
                    .and_then(|p| open_panes_verdict(p, &ws_path, current_block))
            })
            .unwrap_or_else(|| deps.backend.prune_verdict(&root, &ws_path, &name));
        match &verdict {
            vcs::PruneVerdict::Keep(reason) => {
                eprintln!("{} {}: {}", "keep ".yellow(), name.bold(), reason);
            }
            vcs::PruneVerdict::Prunable => match mode {
                PruneMode::DryRun => eprintln!("{} {}", "prune".green(), name.bold()),
                PruneMode::Delete => {
                    match delete_workspace_inner(
                        deps,
                        Some(name.clone()),
                        DeleteOutput::Quiet,
                        OpenPanes::Refuse,
                    ) {
                        Ok(redirect) => {
                            eprintln!("{} {} deleted", "✓".green(), name.bold());
                            outcome.redirect = outcome.redirect.or(redirect);
                            outcome.deleted.push(name.clone());
                        }
                        Err(e) => {
                            failures += 1;
                            eprintln!("{} {}: delete failed: {e:#}", "error".red(), name.bold());
                        }
                    }
                }
            },
        }
        outcome.verdicts.push((name, verdict));
    }

    let prunable = outcome
        .verdicts
        .iter()
        .filter(|(_, v)| *v == vcs::PruneVerdict::Prunable)
        .count();
    let kept = outcome.verdicts.len() - prunable;
    match mode {
        PruneMode::DryRun if prunable > 0 => eprintln!(
            "\n{prunable} prunable, {kept} kept (dry run; run `dwm prune --yes` to delete)"
        ),
        PruneMode::DryRun => eprintln!("\nnothing to prune, {kept} kept"),
        PruneMode::Delete => eprintln!("\n{} deleted, {kept} kept", outcome.deleted.len()),
    }
    if failures > 0 {
        bail!("failed to delete {failures} prunable workspace(s)");
    }
    Ok(outcome)
}

/// Switch to the named workspace by printing its path to stdout for the shell
/// wrapper to `cd` into.
pub fn switch_workspace(name: &str, placement: Placement) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let path = switch_workspace_inner(&deps, name)?;
    if let Some(dir) = go_to_workspace(deps.rex.as_deref(), &deps.cwd, &path, placement)? {
        println!("{}", dir.display());
    }
    Ok(())
}

/// Resolve the path for the named workspace. Returns the path the shell should
/// `cd` into.
fn switch_workspace_inner(deps: &WorkspaceDeps, name: &str) -> Result<PathBuf> {
    let root = resolve_repo_root(deps)?;
    let main_ws_name = deps.backend.main_workspace_name();
    if name == main_ws_name {
        return Ok(root);
    }

    let ws_path = worktrees_dir(&root).join(name);
    if !ws_path.exists() {
        bail!("workspace '{}' not found at {}", name, ws_path.display());
    }

    Ok(ws_path)
}

/// Rename a workspace. When `new_name` is `None` the first argument is treated
/// as the new name and the old name is inferred from the current directory.
pub fn rename_workspace(name: String, new_name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };

    let (old, new) = match new_name {
        Some(new) => (name, new),
        None => {
            // Infer old name from cwd
            let old = infer_workspace_name_from_cwd(&deps)?;
            (old, name)
        }
    };

    if let Some(redirect) = rename_workspace_inner(&deps, &old, &new)? {
        println!("{}", redirect.display());
    }
    Ok(())
}

/// Infer the current workspace name from the current directory path.
fn infer_workspace_name_from_cwd(deps: &WorkspaceDeps) -> Result<String> {
    match find_dwm_workspace(&deps.cwd) {
        Some((_, ws, _)) => Ok(ws),
        None => bail!("not inside a dwm workspace"),
    }
}

/// Returns the path the shell should cd to if cwd was inside the renamed workspace.
fn rename_workspace_inner(
    deps: &WorkspaceDeps,
    old_name: &str,
    new_name: &str,
) -> Result<Option<PathBuf>> {
    let root = resolve_repo_root(deps)?;
    let main_ws_name = deps.backend.main_workspace_name();
    if old_name == main_ws_name {
        bail!("cannot rename the main workspace '{}'", old_name);
    }

    let worktrees = worktrees_dir(&root);
    let old_path = worktrees.join(old_name);
    if !old_path.exists() {
        bail!(
            "workspace '{}' not found at {}",
            old_name,
            old_path.display()
        );
    }

    validate_workspace_name(new_name)?;

    let new_path = worktrees.join(new_name);
    if new_path.exists() {
        bail!(
            "workspace '{}' already exists at {}",
            new_name,
            new_path.display()
        );
    }

    // Rex reports where processes are now, so look before the move.
    let panes = deps
        .rex
        .as_deref()
        .and_then(|r| r.panes().ok())
        .unwrap_or_default();
    let panes_in_old = rex::panes_inside(&panes, &old_path, None);

    eprintln!(
        "{} workspace '{}' -> '{}'...",
        "renaming".cyan(),
        old_name.bold(),
        new_name.bold()
    );
    deps.backend
        .workspace_rename(&root, &old_path, &new_path, old_name, new_name)?;

    eprintln!(
        "{} workspace '{}' renamed to '{}'",
        "✓".green(),
        old_name.bold(),
        new_name.bold()
    );

    pool::rename_lease(&dwm_dir(&root), old_name, new_name)?;

    if let Some(rex) = deps.rex.as_deref() {
        follow_rename_in_rex(rex, &panes_in_old, old_name, new_name, &new_path);
    }

    if is_inside(&deps.cwd, &old_path) {
        let relative = deps.cwd.strip_prefix(&old_path)?;
        Ok(Some(new_path.join(relative)))
    } else {
        Ok(None)
    }
}

/// After a rename, relabel Rex sessions named after the old workspace and
/// warn about other panes that were in it. Their processes follow the moved
/// directory, but shells and agents keep the old path as text and will try to
/// use it.
fn follow_rename_in_rex(
    rex: &dyn RexApi,
    panes_in_old: &[&rex::Pane],
    old_name: &str,
    new_name: &str,
    new_path: &Path,
) {
    let mut relabelled: Vec<&str> = Vec::new();
    for pane in panes_in_old {
        if pane.session_label.as_deref() == Some(old_name)
            && !relabelled.contains(&pane.session_id.as_str())
        {
            match rex.rename_session(&pane.session_id, new_name) {
                Ok(()) => relabelled.push(&pane.session_id),
                Err(e) => eprintln!(
                    "{} could not rename Rex session '{}': {e:#}",
                    "warning:".yellow(),
                    old_name
                ),
            }
        }
    }
    if !relabelled.is_empty() {
        eprintln!(
            "{} renamed Rex session '{}' to '{}'",
            "✓".green(),
            old_name,
            new_name.bold()
        );
    }
    let others: Vec<String> = panes_in_old
        .iter()
        .filter(|p| Some(p.block_id.as_str()) != rex.current_block())
        .map(|p| {
            let mut p = (*p).clone();
            if relabelled.contains(&p.session_id.as_str()) {
                p.session_label = Some(new_name.to_string());
            }
            p.describe()
        })
        .collect();
    if !others.is_empty() {
        eprintln!(
            "{} still in the old directory: {}. Restart programs there or cd to {}",
            "note:".yellow(),
            others.join(", "),
            new_path.display()
        );
    }
}

/// Collect [`WorkspaceEntry`] values for all workspaces belonging to the
/// repository that contains the current directory.
pub fn list_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    Ok(list_workspace_entries_and_dwm_dir()?.0)
}

/// Like [`list_workspace_entries`], but also returns the repo's `<repo>.dwm/`
/// directory.
///
/// Callers that need both (e.g. `dwm list`, which polls agent status out of
/// `<repo>.dwm/`) should use this rather than resolving the repo root separately:
/// the root is already known from the listing, so looking it up again costs a
/// redundant `jj root` / `git rev-parse` subprocess.
pub fn list_workspace_entries_and_dwm_dir() -> Result<(Vec<WorkspaceEntry>, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let backend = vcs::detect(&cwd)?;
    let deps = WorkspaceDeps {
        backend,
        cwd,
        rex: crate::rex::connect(),
    };
    let entries = list_workspace_entries_inner(&deps)?;
    let dwm = match entries.first() {
        Some(entry) => dwm_dir(&entry.main_repo_path),
        None => dwm_dir(&resolve_repo_root(&deps)?),
    };
    Ok((entries, dwm))
}

/// Inputs for the per-workspace fan-out in [`list_workspace_entries_inner`].
struct EntryCandidate {
    name: String,
    path: PathBuf,
    is_main: bool,
    info: vcs::WorkspaceInfo,
    has_info: bool,
}

/// Per-workspace fields computed in parallel via VCS calls.
struct ComputedEntry {
    diff_stat: vcs::DiffStat,
    description: String,
    modified: Option<SystemTime>,
    is_stale: bool,
}

/// Testable core of [`list_workspace_entries`].
fn list_workspace_entries_inner(deps: &WorkspaceDeps) -> Result<Vec<WorkspaceEntry>> {
    let panes = deps.rex.as_deref().and_then(|r| r.panes().ok());
    list_entries_with_panes(deps, panes.as_deref())
}

/// Like [`list_workspace_entries_inner`], with the Rex panes already listed
/// (`None` outside Rex), so listing many repos asks Rex only once.
fn list_entries_with_panes(
    deps: &WorkspaceDeps,
    panes: Option<&[rex::Pane]>,
) -> Result<Vec<WorkspaceEntry>> {
    let main_repo = resolve_repo_root(deps)?;
    let dwm = dwm_dir(&main_repo);

    let rex_summaries = panes.map(agent::summaries_from_panes);
    let mut agent_summaries = agent::read_agent_summaries_with(&dwm, rex_summaries.as_ref());

    let main_ws_name = deps.backend.main_workspace_name();
    let vcs_workspaces = deps.backend.workspace_list(&main_repo).unwrap_or_default();
    let vcs_type = deps.backend.vcs_type();

    // Collect candidates in display order: main first, then each subdir of
    // <repo>.dwm/worktrees/ (in fs::read_dir order, matching previous behaviour).
    let mut candidates: Vec<EntryCandidate> = Vec::new();

    let main_info = vcs_workspaces
        .iter()
        .find(|(n, _)| n == main_ws_name)
        .map(|(_, info)| info.clone());
    candidates.push(EntryCandidate {
        name: main_ws_name.to_string(),
        path: main_repo.clone(),
        is_main: true,
        has_info: main_info.is_some(),
        info: main_info.unwrap_or_default(),
    });

    let worktrees = worktrees_dir(&main_repo);
    if worktrees.exists() {
        // Scan workspace dirs under <repo>.dwm/worktrees/. Internal dwm state
        // lives in sibling dirs (e.g. <repo>.dwm/agent-status/) so there's no need to
        // filter entries here — every dir under worktrees/ is a workspace.
        for entry in fs::read_dir(&worktrees)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let info = vcs_workspaces
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, info)| info.clone());
            candidates.push(EntryCandidate {
                name,
                path,
                is_main: false,
                has_info: info.is_some(),
                info: info.unwrap_or_default(),
            });
        }
    }

    // Fan out the per-workspace VCS calls. Each candidate runs up to 3 cold
    // subprocesses (diff_stat_vs_trunk, optional latest_description,
    // is_merged_into_trunk); these are independent across workspaces and dwarf
    // the cost of spawning an OS thread per workspace.
    let backend: &dyn vcs::VcsBackend = &*deps.backend;
    let main_repo_ref: &Path = &main_repo;
    let computed: Vec<ComputedEntry> = std::thread::scope(|s| {
        let handles: Vec<_> = candidates
            .iter()
            .map(|c| {
                s.spawn(move || {
                    let modified = fs::metadata(&c.path).and_then(|m| m.modified()).ok();

                    let diff_stat = if c.is_main || c.has_info {
                        backend
                            .diff_stat_vs_trunk(main_repo_ref, &c.path, &c.name)
                            .unwrap_or_default()
                    } else {
                        vcs::DiffStat::default()
                    };

                    let description = if c.info.description.trim().is_empty() {
                        backend.latest_description(main_repo_ref, &c.path, &c.name)
                    } else {
                        c.info.description.clone()
                    };

                    let is_stale = if c.is_main {
                        false
                    } else if stale_by_mtime(modified) {
                        // Already stale by inactivity — skip the merged probe,
                        // which is a VCS subprocess we don't need.
                        true
                    } else {
                        let merge_status = if c.has_info
                            && backend.is_merged_into_trunk(main_repo_ref, &c.path, &c.name)
                        {
                            MergeStatus::Merged
                        } else {
                            MergeStatus::Unmerged
                        };
                        compute_is_stale(merge_status, modified)
                    };

                    ComputedEntry {
                        diff_stat,
                        description,
                        modified,
                        is_stale,
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut entries = Vec::with_capacity(candidates.len());
    for (c, comp) in candidates.into_iter().zip(computed) {
        let agent_status = agent_summaries.remove(&c.name);
        let rex_pane = panes.and_then(|p| {
            rex::pick_pane(&rex::panes_inside(p, &c.path, None)).map(rex::PaneRef::from)
        });
        entries.push(WorkspaceEntry {
            name: c.name,
            path: c.path,
            last_modified: comp.modified,
            diff_stat: comp.diff_stat,
            is_main: c.is_main,
            change_id: c.info.change_id,
            description: comp.description,
            bookmarks: c.info.bookmarks,
            is_stale: comp.is_stale,
            repo_name: None,
            main_repo_path: main_repo.clone(),
            vcs_type,
            agent_status,
            rex_pane,
        });
    }

    Ok(entries)
}

/// Number of days of inactivity after which a workspace is considered stale.
const STALE_DAYS: u64 = 30;

/// All data needed to display a single row in the workspace picker or status output.
#[derive(Debug)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: PathBuf,
    pub last_modified: Option<std::time::SystemTime>,
    pub diff_stat: vcs::DiffStat,
    pub is_main: bool,
    pub change_id: String,
    pub description: String,
    pub bookmarks: Vec<String>,
    pub is_stale: bool,
    pub repo_name: Option<String>,
    pub main_repo_path: PathBuf,
    pub vcs_type: vcs::VcsType,
    pub agent_status: Option<agent::AgentSummary>,
    /// A Rex pane open in the workspace, when dwm runs inside Rex.
    pub rex_pane: Option<rex::PaneRef>,
}

/// Determine whether a non-main workspace should be shown as stale.
///
/// A workspace is stale if it has been merged into trunk, or if its last
/// modification time is more than [`STALE_DAYS`] days in the past.
fn compute_is_stale(merged: MergeStatus, last_modified: Option<SystemTime>) -> bool {
    if merged == MergeStatus::Merged {
        return true;
    }
    stale_by_mtime(last_modified)
}

/// `true` if `last_modified` is older than [`STALE_DAYS`]. Used to skip the
/// VCS-subprocess merged probe when inactivity alone is enough to mark a
/// workspace stale.
fn stale_by_mtime(last_modified: Option<SystemTime>) -> bool {
    if let Some(time) = last_modified
        && let Ok(duration) = time.elapsed()
    {
        return duration.as_secs() > STALE_DAYS * 86400;
    }
    false
}

/// Collect [`WorkspaceEntry`] values for every workspace across all repos
/// tracked in the registry.
pub fn list_all_workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    let repos = read_and_prune_registry()?;
    let mut all_entries = Vec::new();
    let panes = rex::connect().and_then(|r| r.panes().ok());

    for repo_root in repos {
        let backend = match vcs::detect(&repo_root) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let repo_label = repo_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());

        let deps = WorkspaceDeps {
            backend,
            cwd: repo_root.clone(),
            rex: None,
        };

        match list_entries_with_panes(&deps, panes.as_deref()) {
            Ok(entries) => {
                for mut entry in entries {
                    entry.repo_name = Some(repo_label.clone());
                    all_entries.push(entry);
                }
            }
            Err(e) => {
                eprintln!("warning: skipping repo '{}': {}", repo_label, e);
            }
        }
    }

    Ok(all_entries)
}

/// Format a [`SystemTime`] as a human-readable relative age string such as
/// `"5m ago"`, `"3h ago"`, or `"2mo ago"`. Returns `"unknown"` when `time`
/// is `None` or when the elapsed time cannot be computed.
pub fn format_time_ago(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "unknown".to_string();
    };
    let Ok(duration) = time.elapsed() else {
        return "unknown".to_string();
    };
    let secs = duration.as_secs();
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{}d ago", days);
    }
    let months = days / 30;
    format!("{}mo ago", months)
}

/// Workspaces as JSON for scripts and Rex's command palette. Unlike the
/// table, this goes to stdout.
pub fn print_status_json(entries: &[WorkspaceEntry]) -> Result<()> {
    println!("{}", serde_json::to_string(&status_json(entries))?);
    Ok(())
}

fn status_json(entries: &[WorkspaceEntry]) -> serde_json::Value {
    let workspaces: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "path": e.path,
                "repo": e.repo_name,
                "main_repo_path": e.main_repo_path,
                "is_main": e.is_main,
                "change_id": e.change_id,
                "description": e.description,
                "bookmarks": e.bookmarks,
                "stale": e.is_stale,
                "modified": e
                    .last_modified
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                "diff": e.diff_stat,
                "agents": e.agent_status.as_ref().filter(|s| !s.is_empty()),
                "rex_pane": e.rex_pane,
                "session_label": session_label_for(&e.path),
                "lease": pool::read_lease(&dwm_dir(&e.main_repo_path), &e.name),
            })
        })
        .collect();
    serde_json::json!({ "workspaces": workspaces })
}

/// Print a non-interactive tabular workspace summary to stderr.
pub fn print_status(entries: &[WorkspaceEntry]) {
    let out = std::io::stderr().lock();
    let _ = print_status_to(entries, out);
}

/// Core logic for printing the status table to any Write implementation.
fn print_status_to<W: Write>(entries: &[WorkspaceEntry], mut out: W) -> Result<()> {
    // Column widths
    let name_w = entries
        .iter()
        .map(|e| {
            let display = if e.is_main {
                format!("{} (main)", e.name)
            } else {
                e.name.clone()
            };
            display.len()
        })
        .max()
        .unwrap_or(4)
        .max(4);
    let change_w = 8;
    let bookmark_w = entries
        .iter()
        .map(|e| e.bookmarks.join(", ").len())
        .max()
        .unwrap_or(9)
        .max(9);
    let has_agents = entries
        .iter()
        .any(|e| e.agent_status.as_ref().is_some_and(|s| !s.is_empty()));
    let agent_w = if has_agents {
        entries
            .iter()
            .map(|e| {
                e.agent_status
                    .as_ref()
                    .map(|s| s.to_string().len())
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(6)
            .max(6)
    } else {
        0
    };

    // Header
    if has_agents {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "{:<name_w$}  {:<change_w$}  {:<40}  {:<bookmark_w$}  {:<9}  {:<agent_w$}  CHANGES",
                "NAME", "CHANGE", "DESCRIPTION", "BOOKMARKS", "MODIFIED", "AGENTS",
            )
            .bold()
            .dimmed()
        );
    } else {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "{:<name_w$}  {:<change_w$}  {:<40}  {:<bookmark_w$}  {:<9}  CHANGES",
                "NAME", "CHANGE", "DESCRIPTION", "BOOKMARKS", "MODIFIED",
            )
            .bold()
            .dimmed()
        );
    }

    for entry in entries {
        let name_text = if entry.is_main {
            format!("{} (main)", entry.name)
        } else if entry.is_stale {
            format!("{} [stale]", entry.name)
        } else {
            entry.name.clone()
        };

        let dim = entry.is_stale;
        let name_colored = {
            let s = format!("{:<name_w$}", name_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.cyan().to_string()
            }
        };

        let change_colored = {
            let s = format!("{:<change_w$}", entry.change_id);
            if dim {
                s.dimmed().to_string()
            } else {
                s.magenta().to_string()
            }
        };

        let desc = entry.description.lines().next().unwrap_or("");
        let desc_text: String = desc.chars().take(40).collect();
        let desc_colored = {
            let s = format!("{:<40}", desc_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.white().to_string()
            }
        };

        let bookmarks_text = entry.bookmarks.join(", ");
        let bookmarks_colored = {
            let s = format!("{:<bookmark_w$}", bookmarks_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.blue().to_string()
            }
        };

        let time_text = format_time_ago(entry.last_modified);
        let time_colored = {
            let s = format!("{:<9}", time_text);
            if dim {
                s.dimmed().to_string()
            } else {
                s.yellow().to_string()
            }
        };

        let stat = &entry.diff_stat;
        let changes_text = if stat.files_changed == 0 && stat.insertions == 0 && stat.deletions == 0
        {
            "clean".to_string()
        } else {
            let mut parts = Vec::new();
            if stat.insertions > 0 {
                parts.push(format!("+{}", stat.insertions));
            }
            if stat.deletions > 0 {
                parts.push(format!("-{}", stat.deletions));
            }
            if parts.is_empty() {
                format!("{} files", stat.files_changed)
            } else {
                parts.join(" ")
            }
        };

        let changes_colored = if dim {
            changes_text.dimmed().to_string()
        } else if stat.deletions > stat.insertions {
            changes_text.red().to_string()
        } else if stat.insertions > 0 {
            changes_text.green().to_string()
        } else {
            changes_text.dimmed().to_string()
        };

        if has_agents {
            let agent_colored = match &entry.agent_status {
                Some(summary) if !summary.is_empty() => {
                    let text = format!("{:<agent_w$}", summary);
                    if dim {
                        text.dimmed().to_string()
                    } else {
                        match summary.most_urgent() {
                            Some(crate::agent::AgentStatus::Waiting) => text.yellow().to_string(),
                            Some(crate::agent::AgentStatus::Working) => text.green().to_string(),
                            Some(crate::agent::AgentStatus::Done) => text.cyan().to_string(),
                            _ => text.dimmed().to_string(),
                        }
                    }
                }
                _ => format!("{:<agent_w$}", ""),
            };

            let _ = writeln!(
                out,
                "{}  {}  {}  {}  {}  {}  {}",
                name_colored,
                change_colored,
                desc_colored,
                bookmarks_colored,
                time_colored,
                agent_colored,
                changes_colored,
            );
        } else {
            let _ = writeln!(
                out,
                "{}  {}  {}  {}  {}  {}",
                name_colored,
                change_colored,
                desc_colored,
                bookmarks_colored,
                time_colored,
                changes_colored,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn print_status_to_string(entries: &[WorkspaceEntry]) -> String {
        owo_colors::set_override(true);
        let mut buf = Vec::new();
        print_status_to(entries, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn is_inside_detects_cwd_within_workspace() {
        let ws = Path::new("/home/user/repos/myrepo.dwm/worktrees/my-workspace");
        assert!(is_inside(ws, ws));
        assert!(is_inside(
            Path::new("/home/user/repos/myrepo.dwm/worktrees/my-workspace/src"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_sibling_workspace() {
        let ws = Path::new("/home/user/repos/myrepo.dwm/worktrees/my-workspace");
        assert!(!is_inside(
            Path::new("/home/user/repos/myrepo.dwm/worktrees/other-workspace"),
            ws,
        ));
    }

    #[test]
    fn is_inside_false_for_main_repo() {
        let ws = Path::new("/home/user/repos/myrepo.dwm/worktrees/my-workspace");
        assert!(!is_inside(Path::new("/home/user/code/myrepo"), ws));
    }

    // ── find_dwm_workspace tests ──────────────────────────────────────

    fn make_fake_repo(dir: &Path, vcs_marker: &str) -> PathBuf {
        let repo = dir.join("myrepo");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(repo.join(vcs_marker)).unwrap();
        repo
    }

    #[test]
    fn find_dwm_workspace_in_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let ws = worktrees_dir(&repo).join("feature");
        fs::create_dir_all(&ws).unwrap();

        let (root, name, path) = find_dwm_workspace(&ws).unwrap();
        assert_eq!(root, repo);
        assert_eq!(name, "feature");
        assert_eq!(path, ws);
    }

    #[test]
    fn find_dwm_workspace_in_workspace_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".git");
        let ws = worktrees_dir(&repo).join("feat-x");
        let sub = ws.join("src/inner");
        fs::create_dir_all(&sub).unwrap();

        let (root, name, path) = find_dwm_workspace(&sub).unwrap();
        assert_eq!(root, repo);
        assert_eq!(name, "feat-x");
        assert_eq!(path, ws);
    }

    #[test]
    fn find_dwm_workspace_returns_none_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        // cwd is the repo root itself, not in a workspace
        assert!(find_dwm_workspace(&repo).is_none());
    }

    #[test]
    fn find_dwm_workspace_returns_none_when_no_vcs_root_parent() {
        let tmp = tempfile::tempdir().unwrap();
        // Create notarepo.dwm/worktrees/ws without a VCS root sibling
        let fake = tmp.path().join("notarepo");
        let ws = worktrees_dir(&fake).join("feature");
        fs::create_dir_all(&ws).unwrap();
        assert!(find_dwm_workspace(&ws).is_none());
    }

    #[test]
    fn find_dwm_workspace_ignores_sibling_internal_dirs() {
        // Regression: pre-refactor, find_dwm_workspace returned `.agent-status`
        // as a workspace name when cwd was under `<root>/.dwm/.agent-status/` (the layout at the time).
        // With the worktrees/ namespace that confusion is gone.
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let agent = dwm_dir(&repo).join("agent-status");
        fs::create_dir_all(&agent).unwrap();
        assert!(find_dwm_workspace(&agent).is_none());
    }

    #[test]
    fn find_dwm_workspace_allows_dot_prefixed_names() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let ws = worktrees_dir(&repo).join(".hidden-feature");
        fs::create_dir_all(&ws).unwrap();

        let (root, name, _) = find_dwm_workspace(&ws).unwrap();
        assert_eq!(root, repo);
        assert_eq!(name, ".hidden-feature");
    }

    #[test]
    fn find_dwm_workspace_ignores_legacy_in_repo_layout() {
        // An in-repo `.dwm/worktrees/` is not a workspace location.
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        let ws = repo.join(".dwm/worktrees/feature");
        fs::create_dir_all(&ws).unwrap();
        assert!(find_dwm_workspace(&ws).is_none());
    }

    // ── dwm_dir tests ─────────────────────────────────────────────────

    #[test]
    fn dwm_dir_is_sibling_of_repo_root() {
        assert_eq!(
            dwm_dir(Path::new("/home/user/dev/ghg")),
            PathBuf::from("/home/user/dev/ghg.dwm")
        );
        assert_eq!(
            worktrees_dir(Path::new("/home/user/dev/ghg")),
            PathBuf::from("/home/user/dev/ghg.dwm/worktrees")
        );
    }

    #[test]
    fn dwm_dir_is_not_inside_repo_root() {
        // A jj workspace has no `.git` to stop git's upward search, so one
        // nested in the main checkout is treated as ignored paths of it.
        let root = Path::new("/home/user/dev/ghg");
        assert!(!dwm_dir(root).starts_with(root));
    }

    #[test]
    fn dwm_dir_keeps_dots_in_repo_name() {
        assert_eq!(
            dwm_dir(Path::new("/src/my.repo")),
            PathBuf::from("/src/my.repo.dwm")
        );
    }

    #[test]
    fn dwm_dir_falls_back_inside_filesystem_root() {
        assert_eq!(dwm_dir(Path::new("/")), PathBuf::from("/.dwm"));
    }

    #[test]
    fn repo_root_for_dwm_dir_inverts_dwm_dir() {
        let root = Path::new("/home/user/dev/my.repo");
        assert_eq!(repo_root_for_dwm_dir(&dwm_dir(root)).as_deref(), Some(root));
    }

    #[test]
    fn repo_root_for_dwm_dir_rejects_non_dwm_names() {
        assert_eq!(repo_root_for_dwm_dir(Path::new("/src/ghg")), None);
        // A bare `.dwm` (the legacy in-repo dir) has an empty repo name.
        assert_eq!(repo_root_for_dwm_dir(Path::new("/src/ghg/.dwm")), None);
    }

    // ── registry tests ────────────────────────────────────────────────

    fn with_data_dir<R, F: FnOnce() -> R>(dir: &Path, f: F) -> R {
        // Override XDG_DATA_HOME / Application Support so `dirs::data_dir()`
        // returns `dir`.
        #[cfg(target_os = "macos")]
        let var = "HOME";
        #[cfg(not(target_os = "macos"))]
        let var = "XDG_DATA_HOME";

        // For macOS, dirs::data_dir() = $HOME/Library/Application Support.
        // For tests we set HOME so that resolves under our tmp dir.
        // For Linux we just point XDG_DATA_HOME at tmp directly.
        #[cfg(target_os = "macos")]
        let value = dir.to_path_buf();
        #[cfg(not(target_os = "macos"))]
        let value = dir.to_path_buf();
        // On macOS the data_dir is $HOME/Library/Application Support; ensure it exists.
        #[cfg(target_os = "macos")]
        std::fs::create_dir_all(value.join("Library/Application Support")).unwrap();

        temp_env::with_var(var, Some(value.as_os_str()), f)
    }

    #[test]
    fn registry_register_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj");
        // Need a <repo>.dwm/ for read_and_prune_registry to keep it.
        fs::create_dir_all(dwm_dir(&repo)).unwrap();

        with_data_dir(tmp.path(), || {
            register_repo(&repo).unwrap();
            register_repo(&repo).unwrap(); // dedupe
            let entries = read_and_prune_registry().unwrap();
            assert_eq!(entries, vec![fs::canonicalize(&repo).unwrap()]);
        });
    }

    #[test]
    fn registry_prunes_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let alive = make_fake_repo(tmp.path(), ".jj");
        fs::create_dir_all(dwm_dir(&alive)).unwrap();
        let dead = tmp.path().join("nonexistent-repo");

        with_data_dir(tmp.path(), || {
            register_repo(&alive).unwrap();
            // Manually add a dead entry
            let path = registry_path().unwrap();
            let mut content = fs::read_to_string(&path).unwrap_or_default();
            content.push_str(&format!("{}\n", dead.display()));
            fs::write(&path, content).unwrap();

            let entries = read_and_prune_registry().unwrap();
            assert_eq!(entries, vec![fs::canonicalize(&alive).unwrap()]);

            // The file should now have been rewritten without the dead entry.
            let after = fs::read_to_string(&path).unwrap();
            assert!(!after.contains(dead.to_string_lossy().as_ref()));
        });
    }

    #[test]
    fn registry_prunes_repo_without_dwm_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = make_fake_repo(tmp.path(), ".jj"); // no <repo>.dwm sibling

        with_data_dir(tmp.path(), || {
            register_repo(&repo).unwrap();
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.is_empty());
        });
    }

    #[test]
    fn registry_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        with_data_dir(tmp.path(), || {
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.is_empty());
        });
    }

    #[test]
    #[cfg(unix)]
    fn registry_dedupes_symlinked_and_real_paths() {
        // Regression: invoking dwm once via a symlinked path and once via the
        // real path used to produce two registry entries. Canonicalization in
        // register_repo collapses them to one.
        let tmp = tempfile::tempdir().unwrap();
        let real = make_fake_repo(tmp.path(), ".jj");
        fs::create_dir_all(dwm_dir(&real)).unwrap();

        let link = tmp.path().join("myrepo-symlink");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        with_data_dir(tmp.path(), || {
            register_repo(&link).unwrap();
            register_repo(&real).unwrap();
            let entries = read_and_prune_registry().unwrap();
            assert_eq!(
                entries.len(),
                1,
                "expected single entry, got: {:?}",
                entries
            );
            // The stored entry is the canonical form of `real`.
            let expected = fs::canonicalize(&real).unwrap();
            assert_eq!(entries[0], expected);
        });
    }

    // ── MockBackend ──────────────────────────────────────────────────

    #[derive(Debug, Clone)]
    enum MockCall {
        Add {
            repo_dir: PathBuf,
            ws_path: PathBuf,
            name: String,
            at: Option<String>,
        },
        Remove {
            repo_dir: PathBuf,
            name: String,
            ws_path: PathBuf,
        },
        Rename {
            old_name: String,
            new_name: String,
        },
        Reset {
            name: String,
            revision: String,
        },
    }

    struct MockBackend {
        /// The root path returned by root_from.
        root: PathBuf,
        /// Workspaces returned by workspace_list.
        workspaces: Vec<(String, vcs::WorkspaceInfo)>,
        /// Records every mutating call for assertions.
        calls: Arc<Mutex<Vec<MockCall>>>,
        /// Verdicts returned by prune_verdict, keyed by workspace name.
        prune_verdicts: std::collections::HashMap<String, vcs::PruneVerdict>,
    }

    impl MockBackend {
        fn new(
            root: PathBuf,
            workspaces: Vec<(String, vcs::WorkspaceInfo)>,
        ) -> (Self, Arc<Mutex<Vec<MockCall>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    root,
                    workspaces,
                    calls: Arc::clone(&calls),
                    prune_verdicts: Default::default(),
                },
                calls,
            )
        }

        fn with_prune_verdicts(mut self, verdicts: &[(&str, vcs::PruneVerdict)]) -> Self {
            self.prune_verdicts = verdicts
                .iter()
                .map(|(n, v)| (n.to_string(), v.clone()))
                .collect();
            self
        }
    }

    impl vcs::VcsBackend for MockBackend {
        fn root_from(&self, _dir: &Path) -> Result<PathBuf> {
            Ok(self.root.clone())
        }

        fn workspace_list(&self, _repo_dir: &Path) -> Result<Vec<(String, vcs::WorkspaceInfo)>> {
            Ok(self.workspaces.clone())
        }

        fn workspace_add(
            &self,
            repo_dir: &Path,
            ws_path: &Path,
            name: &str,
            at: Option<&str>,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::Add {
                repo_dir: repo_dir.to_path_buf(),
                ws_path: ws_path.to_path_buf(),
                name: name.to_string(),
                at: at.map(|s| s.to_string()),
            });
            // Create the directory so the workspace "exists" after add
            fs::create_dir_all(ws_path)?;
            Ok(())
        }

        fn workspace_remove(&self, repo_dir: &Path, name: &str, ws_path: &Path) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::Remove {
                repo_dir: repo_dir.to_path_buf(),
                name: name.to_string(),
                ws_path: ws_path.to_path_buf(),
            });
            Ok(())
        }

        fn workspace_rename(
            &self,
            _repo_dir: &Path,
            old_path: &Path,
            new_path: &Path,
            old_name: &str,
            new_name: &str,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::Rename {
                old_name: old_name.to_string(),
                new_name: new_name.to_string(),
            });
            fs::rename(old_path, new_path)?;
            Ok(())
        }

        fn diff_stat_vs_trunk(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            _ws_name: &str,
        ) -> Result<vcs::DiffStat> {
            Ok(vcs::DiffStat {
                files_changed: 1,
                insertions: 10,
                deletions: 2,
            })
        }

        fn latest_description(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            _ws_name: &str,
        ) -> String {
            "mock description".to_string()
        }

        fn is_merged_into_trunk(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            _ws_name: &str,
        ) -> bool {
            false
        }

        fn vcs_type(&self) -> vcs::VcsType {
            vcs::VcsType::Jj
        }

        fn main_workspace_name(&self) -> &'static str {
            "default"
        }

        fn reset_workspace(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            ws_name: &str,
            revision: &str,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(MockCall::Reset {
                name: ws_name.to_string(),
                revision: revision.to_string(),
            });
            Ok(())
        }

        fn prune_verdict(
            &self,
            _repo_dir: &Path,
            _worktree_dir: &Path,
            ws_name: &str,
        ) -> vcs::PruneVerdict {
            self.prune_verdicts
                .get(ws_name)
                .cloned()
                .unwrap_or_else(|| vcs::PruneVerdict::Keep("mock default".to_string()))
        }
    }

    // Helper: create a fake repo with `.jj/` as VCS marker so that
    // find_dwm_workspace recognizes the parent.
    fn make_repo_with_vcs(tmp: &Path) -> PathBuf {
        let repo = tmp.join("myrepo");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(repo.join(".jj")).unwrap();
        repo
    }

    // ── list_workspace_entries_inner tests ────────────────────────────

    #[test]
    fn list_entries_from_inside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("feat-x");
        fs::create_dir_all(&ws_dir).unwrap();

        let workspaces = vec![
            (
                "default".to_string(),
                vcs::WorkspaceInfo {
                    change_id: "aaa".to_string(),
                    description: "main desc".to_string(),
                    bookmarks: vec!["main".to_string()],
                },
            ),
            (
                "feat-x".to_string(),
                vcs::WorkspaceInfo {
                    change_id: "bbb".to_string(),
                    description: "feature".to_string(),
                    bookmarks: vec![],
                },
            ),
        ];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert!(entries.len() >= 2);

        let main_entry = entries.iter().find(|e| e.is_main).unwrap();
        assert_eq!(main_entry.name, "default");
        assert_eq!(main_entry.change_id, "aaa");
        assert_eq!(main_entry.description, "main desc");
        assert_eq!(main_entry.path, main_repo);

        let feat_entry = entries.iter().find(|e| e.name == "feat-x").unwrap();
        assert_eq!(feat_entry.change_id, "bbb");
        assert_eq!(feat_entry.description, "feature");
        assert!(!feat_entry.is_main);
    }

    #[test]
    fn list_entries_only_scans_worktrees_subdir() {
        // Sibling dirs under <repo>.dwm/ (e.g. agent-status/) must not show up as
        // workspaces. With the worktrees/ namespace this falls out for free
        // because list iteration only reads <repo>.dwm/worktrees/.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let dwm = dwm_dir(&main_repo);
        let ws_dir = dwm.join("worktrees/feat-x");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(dwm.join("agent-status")).unwrap();

        let workspaces = vec![
            (
                "default".to_string(),
                vcs::WorkspaceInfo {
                    change_id: "aaa".to_string(),
                    description: "".to_string(),
                    bookmarks: vec![],
                },
            ),
            (
                "feat-x".to_string(),
                vcs::WorkspaceInfo {
                    change_id: "bbb".to_string(),
                    description: "".to_string(),
                    bookmarks: vec![],
                },
            ),
        ];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir,
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            !names.contains(&"agent-status"),
            "sibling internal dirs should not appear, got: {:?}",
            names
        );
        assert!(names.contains(&"feat-x"));
    }

    #[test]
    fn list_entries_from_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let workspaces = vec![(
            "default".to_string(),
            vcs::WorkspaceInfo {
                change_id: "abc".to_string(),
                description: "".to_string(),
                bookmarks: vec![],
            },
        )];

        let (mock, _calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_main);
        // Empty description should fall through to latest_description
        assert_eq!(entries[0].description, "mock description");
    }

    #[test]
    fn list_entries_empty_repo_no_dwm_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        // No <repo>.dwm/ — should still return the main entry only.

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1, "should have just the main entry");
        assert!(entries[0].is_main);
    }

    // ── new_workspace_inner tests ────────────────────────────────────

    #[test]
    fn new_workspace_calls_add() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("my-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Add {
                repo_dir,
                ws_path,
                name,
                at,
            } => {
                assert_eq!(repo_dir, &main_repo);
                assert_eq!(ws_path, &worktrees_dir(&main_repo).join("my-ws"));
                assert_eq!(name, "my-ws");
                assert!(at.is_none());
            }
            other => panic!("expected Add, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_auto_names() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(&deps, None, None, None, Placement::Rex, &[]).unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Add { name, .. } => {
                // Auto-generated name should be non-empty and contain a hyphen (adjective-noun)
                assert!(!name.is_empty());
                assert!(
                    name.contains('-'),
                    "auto name should be adjective-noun: {}",
                    name
                );
            }
            other => panic!("expected Add, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_duplicate_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("dup-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let err = new_workspace_inner(
                &deps,
                Some("dup-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap_err();
            assert!(err.to_string().contains("already exists"), "error: {}", err);
        });
    }

    #[test]
    fn new_workspace_allows_dot_prefixed_names() {
        // Pre-refactor `.foo` was rejected to avoid collision with `.agent-status`.
        // With workspaces under `<repo>.dwm/worktrees/`, there's no collision risk and
        // names starting with `.` are allowed.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some(".hidden".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
        });
        assert!(worktrees_dir(&main_repo).join(".hidden").exists());
    }

    #[test]
    fn new_workspace_rejects_invalid_names() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            for bad in [".", "..", "foo/bar", "with\\backslash", ""] {
                let err = new_workspace_inner(
                    &deps,
                    Some(bad.to_string()),
                    None,
                    None,
                    Placement::Rex,
                    &[],
                )
                .unwrap_err()
                .to_string();
                assert!(
                    err.contains("invalid workspace name"),
                    "expected rejection for {:?}, got: {}",
                    bad,
                    err
                );
            }
        });
    }

    #[test]
    fn new_workspace_from_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let workspaces = vec![(
            "source-ws".to_string(),
            vcs::WorkspaceInfo {
                change_id: "abc12345".to_string(),
                description: "some work".to_string(),
                bookmarks: vec![],
            },
        )];

        let (mock, calls) = MockBackend::new(main_repo.clone(), workspaces);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("forked".to_string()),
                None,
                Some("source-ws"),
                Placement::Rex,
                &[],
            )
            .unwrap();
        });

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Add {
                ws_path, name, at, ..
            } => {
                assert_eq!(ws_path, &worktrees_dir(&main_repo).join("forked"));
                assert_eq!(name, "forked");
                assert_eq!(at.as_deref(), Some("abc12345"));
            }
            other => panic!("expected Add, got {:?}", other),
        }
    }

    #[test]
    fn new_workspace_from_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            let err = new_workspace_inner(
                &deps,
                Some("forked".to_string()),
                None,
                Some("no-such-ws"),
                Placement::Rex,
                &[],
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("not found"),
                "error should mention not found: {}",
                err
            );
        });
    }

    #[test]
    fn new_workspace_creates_workspace_outside_repo_without_touching_ignores() {
        // The workspace lives in a sibling dir, so there is nothing to ignore
        // inside the repo and dwm must not edit any ignore file.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
        });
        assert!(tmp.path().join("myrepo.dwm/worktrees/ws").is_dir());
        assert!(!main_repo.join(".gitignore").exists());
        assert!(!main_repo.join(".dwm").exists());
    }

    #[test]
    fn new_workspace_registers_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        with_data_dir(tmp.path(), || {
            new_workspace_inner(
                &deps,
                Some("ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let entries = read_and_prune_registry().unwrap();
            assert!(entries.contains(&fs::canonicalize(&main_repo).unwrap()));
        });
    }

    // ── prune_workspaces_inner tests ─────────────────────────────────

    fn keep(reason: &str) -> vcs::PruneVerdict {
        vcs::PruneVerdict::Keep(reason.to_string())
    }

    /// Repo with `<repo>.dwm/worktrees/{a,b}`, where `a` is prunable and `b` not.
    fn prune_fixture(tmp: &Path) -> (PathBuf, MockBackend, Arc<Mutex<Vec<MockCall>>>) {
        let main_repo = make_repo_with_vcs(tmp);
        for ws in ["a", "b"] {
            fs::create_dir_all(worktrees_dir(&main_repo).join(ws)).unwrap();
        }
        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let mock =
            mock.with_prune_verdicts(&[("a", vcs::PruneVerdict::Prunable), ("b", keep("dirty"))]);
        (main_repo, mock, calls)
    }

    #[test]
    fn prune_dry_run_reports_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, mock, calls) = prune_fixture(tmp.path());
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::DryRun).unwrap();
        assert_eq!(
            outcome.verdicts,
            vec![
                ("a".to_string(), vcs::PruneVerdict::Prunable),
                ("b".to_string(), keep("dirty")),
            ]
        );
        assert!(outcome.deleted.is_empty());
        assert!(outcome.redirect.is_none());
        assert!(calls.lock().unwrap().is_empty());
        assert!(worktrees_dir(&main_repo).join("a").exists());
        assert!(worktrees_dir(&main_repo).join("b").exists());
    }

    #[test]
    fn prune_delete_removes_only_prunable_via_delete_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, mock, calls) = prune_fixture(tmp.path());
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(outcome.deleted, vec!["a".to_string()]);
        assert!(!worktrees_dir(&main_repo).join("a").exists());
        assert!(worktrees_dir(&main_repo).join("b").exists());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0], MockCall::Remove { name, .. } if name == "a"));
    }

    #[test]
    fn prune_never_touches_main_workspace_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let dir = worktrees_dir(&main_repo).join("default");
        fs::create_dir_all(&dir).unwrap();
        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let mock = mock.with_prune_verdicts(&[("default", vcs::PruneVerdict::Prunable)]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert!(outcome.verdicts.is_empty());
        assert!(calls.lock().unwrap().is_empty());
        assert!(dir.exists());
    }

    #[test]
    fn prune_redirects_when_cwd_inside_deleted_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let (main_repo, mock, _calls) = prune_fixture(tmp.path());
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: worktrees_dir(&main_repo).join("a"),
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(outcome.redirect, Some(main_repo));
    }

    #[test]
    fn prune_without_worktrees_dir_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };
        let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert!(outcome.verdicts.is_empty());
        assert!(calls.lock().unwrap().is_empty());
    }

    // ── delete_workspace_inner tests ─────────────────────────────────

    #[test]
    fn delete_workspace_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("my-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        // cwd is outside the workspace being deleted
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        let redirect = delete_workspace_inner(
            &deps,
            Some("my-ws".to_string()),
            DeleteOutput::Verbose,
            OpenPanes::Refuse,
        )
        .unwrap();
        assert!(
            redirect.is_none(),
            "should not redirect when cwd is outside workspace"
        );

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Remove {
                repo_dir,
                name,
                ws_path,
            } => {
                assert_eq!(repo_dir, &main_repo);
                assert_eq!(name, "my-ws");
                assert_eq!(ws_path, &ws_dir);
            }
            other => panic!("expected Remove, got {:?}", other),
        }
        assert!(!ws_dir.exists());
    }

    #[test]
    fn delete_workspace_redirects_when_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("my-ws");
        fs::create_dir_all(ws_dir.join("src")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
            rex: None,
        };

        let redirect = delete_workspace_inner(
            &deps,
            Some("my-ws".to_string()),
            DeleteOutput::Verbose,
            OpenPanes::Refuse,
        )
        .unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, main_repo);
    }

    #[test]
    fn delete_workspace_infers_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("inferred-ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            rex: None,
        };

        let _redirected =
            delete_workspace_inner(&deps, None, DeleteOutput::Verbose, OpenPanes::Refuse).unwrap();

        let calls = calls.lock().unwrap();
        match &calls[0] {
            MockCall::Remove { name, .. } => {
                assert_eq!(name, "inferred-ws");
            }
            other => panic!("expected Remove, got {:?}", other),
        }
    }

    #[test]
    fn delete_workspace_not_found_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let err = delete_workspace_inner(
            &deps,
            Some("nonexistent".to_string()),
            DeleteOutput::Verbose,
            OpenPanes::Refuse,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    // ── rename_workspace_inner tests ──────────────────────────────

    #[test]
    fn rename_workspace_success() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("old-name");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        let redirect = rename_workspace_inner(&deps, "old-name", "new-name").unwrap();
        assert!(
            redirect.is_none(),
            "should not redirect when cwd is outside workspace"
        );

        assert!(!ws_dir.exists());
        assert!(worktrees_dir(&main_repo).join("new-name").exists());

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Rename {
                old_name, new_name, ..
            } => {
                assert_eq!(old_name, "old-name");
                assert_eq!(new_name, "new-name");
            }
            other => panic!("expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn rename_workspace_redirects_when_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("old-name");
        fs::create_dir_all(ws_dir.join("src")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.join("src"),
            rex: None,
        };

        let redirect = rename_workspace_inner(&deps, "old-name", "new-name").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, worktrees_dir(&main_repo).join("new-name/src"));
    }

    #[test]
    fn rename_workspace_preserves_files() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("old-name");
        fs::create_dir_all(ws_dir.join("src")).unwrap();
        fs::write(ws_dir.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(ws_dir.join("README.md"), "# hello").unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        rename_workspace_inner(&deps, "old-name", "new-name").unwrap();

        let new_dir = worktrees_dir(&main_repo).join("new-name");
        assert!(new_dir.join("src/main.rs").exists());
        assert_eq!(
            fs::read_to_string(new_dir.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
        assert_eq!(
            fs::read_to_string(new_dir.join("README.md")).unwrap(),
            "# hello"
        );
    }

    #[test]
    fn rename_workspace_old_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let err = rename_workspace_inner(&deps, "nonexistent", "new-name").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_new_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(worktrees_dir(&main_repo).join("old-name")).unwrap();
        fs::create_dir_all(worktrees_dir(&main_repo).join("new-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let err = rename_workspace_inner(&deps, "old-name", "new-name").unwrap_err();
        assert!(err.to_string().contains("already exists"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_refuses_main() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let err = rename_workspace_inner(&deps, "default", "new-name").unwrap_err();
        assert!(err.to_string().contains("cannot rename"), "error: {}", err);
    }

    #[test]
    fn rename_workspace_rejects_invalid_target() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(worktrees_dir(&main_repo).join("old-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let err = rename_workspace_inner(&deps, "old-name", "..").unwrap_err();
        assert!(
            err.to_string().contains("invalid workspace name"),
            "error: {}",
            err
        );
    }

    #[test]
    fn rename_workspace_allows_dot_prefixed_target() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        fs::create_dir_all(worktrees_dir(&main_repo).join("old-name")).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        rename_workspace_inner(&deps, "old-name", ".hidden").unwrap();
        assert!(worktrees_dir(&main_repo).join(".hidden").exists());
    }

    // ── switch_workspace_inner tests ──────────────────────────────

    #[test]
    fn switch_workspace_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("feat-x");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let path = switch_workspace_inner(&deps, "feat-x").unwrap();
        assert_eq!(path, ws_dir);
    }

    #[test]
    fn switch_workspace_to_main() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo.clone(),
            rex: None,
        };

        let path = switch_workspace_inner(&deps, "default").unwrap();
        assert_eq!(path, main_repo);
    }

    #[test]
    fn switch_workspace_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let err = switch_workspace_inner(&deps, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"), "error: {}", err);
    }

    // ── rename with cwd inference tests ─────────────────────────────

    #[test]
    fn rename_infers_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let ws_dir = worktrees_dir(&main_repo).join("old-name");
        fs::create_dir_all(&ws_dir).unwrap();

        let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: ws_dir.clone(),
            rex: None,
        };

        let old = infer_workspace_name_from_cwd(&deps).unwrap();
        assert_eq!(old, "old-name");

        let redirect = rename_workspace_inner(&deps, &old, "new-name").unwrap();
        let redirect = redirect.expect("should redirect when cwd is inside workspace");
        assert_eq!(redirect, worktrees_dir(&main_repo).join("new-name"));

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::Rename { old_name, new_name } => {
                assert_eq!(old_name, "old-name");
                assert_eq!(new_name, "new-name");
            }
            other => panic!("expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn rename_refuses_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());

        let (mock, _calls) = MockBackend::new(main_repo.clone(), vec![]);
        let deps = WorkspaceDeps {
            backend: Box::new(mock),
            cwd: main_repo,
            rex: None,
        };

        let err = infer_workspace_name_from_cwd(&deps).unwrap_err();
        assert!(
            err.to_string().contains("not inside a dwm workspace"),
            "error: {}",
            err
        );
    }

    // ── compute_is_stale tests ────────────────────────────────────

    #[test]
    fn stale_merged_workspace_is_stale() {
        assert!(compute_is_stale(
            MergeStatus::Merged,
            Some(SystemTime::now())
        ));
    }

    #[test]
    fn stale_merged_workspace_without_time_is_stale() {
        assert!(compute_is_stale(MergeStatus::Merged, None));
    }

    #[test]
    fn stale_old_workspace_is_stale() {
        let old_time = SystemTime::now() - std::time::Duration::from_secs(86400 * 31);
        assert!(compute_is_stale(MergeStatus::Unmerged, Some(old_time)));
    }

    #[test]
    fn stale_recent_workspace_is_not_stale() {
        let recent = SystemTime::now() - std::time::Duration::from_secs(86400 * 5);
        assert!(!compute_is_stale(MergeStatus::Unmerged, Some(recent)));
    }

    #[test]
    fn stale_unknown_time_not_merged_is_not_stale() {
        assert!(!compute_is_stale(MergeStatus::Unmerged, None));
    }

    #[test]
    fn stale_by_mtime_old_is_true() {
        let old = SystemTime::now() - std::time::Duration::from_secs(86400 * 31);
        assert!(stale_by_mtime(Some(old)));
    }

    #[test]
    fn stale_by_mtime_recent_is_false() {
        let recent = SystemTime::now() - std::time::Duration::from_secs(86400 * 5);
        assert!(!stale_by_mtime(Some(recent)));
    }

    #[test]
    fn stale_by_mtime_unknown_is_false() {
        assert!(!stale_by_mtime(None));
    }

    // ── format_time_ago tests ───────────────────────────────────────

    #[test]
    fn format_time_ago_none_returns_unknown() {
        assert_eq!(format_time_ago(None), "unknown");
    }

    #[test]
    fn format_time_ago_just_now() {
        let time = SystemTime::now() - std::time::Duration::from_secs(30);
        assert_eq!(format_time_ago(Some(time)), "just now");
    }

    #[test]
    fn format_time_ago_minutes() {
        let time = SystemTime::now() - std::time::Duration::from_secs(300);
        assert_eq!(format_time_ago(Some(time)), "5m ago");
    }

    #[test]
    fn format_time_ago_hours() {
        let time = SystemTime::now() - std::time::Duration::from_secs(7200);
        assert_eq!(format_time_ago(Some(time)), "2h ago");
    }

    #[test]
    fn format_time_ago_days() {
        let time = SystemTime::now() - std::time::Duration::from_secs(86400 * 5);
        assert_eq!(format_time_ago(Some(time)), "5d ago");
    }

    #[test]
    fn format_time_ago_months() {
        let time = SystemTime::now() - std::time::Duration::from_secs(86400 * 60);
        assert_eq!(format_time_ago(Some(time)), "2mo ago");
    }

    // ── print_status tests ──────────────────────────────────────────

    #[test]
    fn print_status_does_not_panic() {
        let entries = vec![
            WorkspaceEntry {
                name: "default".to_string(),
                path: PathBuf::from("/tmp/repo"),
                last_modified: Some(SystemTime::now()),
                diff_stat: vcs::DiffStat {
                    files_changed: 1,
                    insertions: 10,
                    deletions: 2,
                },
                is_main: true,
                change_id: "abc12345".to_string(),
                description: "main workspace".to_string(),
                bookmarks: vec!["main".to_string()],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
                rex_pane: None,
            },
            WorkspaceEntry {
                name: "feat-x".to_string(),
                path: PathBuf::from("/tmp/feat-x"),
                last_modified: None,
                diff_stat: vcs::DiffStat::default(),
                is_main: false,
                change_id: "def67890".to_string(),
                description: "feature work".to_string(),
                bookmarks: vec![],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
                rex_pane: None,
            },
        ];
        // Should not panic; output goes to stderr
        print_status(&entries);
    }

    #[test]
    fn status_table_snapshot() {
        // Use fixed times relative to "now" for format_time_ago
        let now = SystemTime::now();
        let t_5m = now - std::time::Duration::from_secs(300);
        let t_2h = now - std::time::Duration::from_secs(7200);

        let entries = vec![
            WorkspaceEntry {
                name: "default".to_string(),
                path: PathBuf::from("/tmp/repo"),
                last_modified: Some(t_5m),
                diff_stat: vcs::DiffStat {
                    files_changed: 1,
                    insertions: 10,
                    deletions: 2,
                },
                is_main: true,
                change_id: "abc12345".to_string(),
                description: "refactor help system".to_string(),
                bookmarks: vec!["main".to_string()],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: None,
                rex_pane: None,
            },
            WorkspaceEntry {
                name: "hazy-quail".to_string(),
                path: PathBuf::from("/tmp/hazy-quail"),
                last_modified: Some(t_2h),
                diff_stat: vcs::DiffStat {
                    files_changed: 5,
                    insertions: 100,
                    deletions: 50,
                },
                is_main: false,
                change_id: "tqqorvwl".to_string(),
                description: "Live-updating list view".to_string(),
                bookmarks: vec![],
                is_stale: false,
                repo_name: None,
                main_repo_path: PathBuf::from("/tmp/repo"),
                vcs_type: vcs::VcsType::Jj,
                agent_status: Some(crate::agent::AgentSummary {
                    waiting: 1,
                    working: 0,
                    done: 0,
                    idle: 0,
                    notes: Vec::new(),
                }),
                rex_pane: None,
            },
        ];

        let out = print_status_to_string(&entries);

        assert!(out.contains("NAME"));
        assert!(out.contains("default (main)"));
        assert!(out.contains("abc12345"));
        assert!(out.contains("refactor help system"));
        assert!(out.contains("main"));
        assert!(out.contains("5m ago"));
        assert!(out.contains("+10 -2"));

        assert!(out.contains("hazy-quail"));
        assert!(out.contains("tqqorvwl"));
        assert!(out.contains("Live-updating list view"));
        assert!(out.contains("2h ago"));
        assert!(out.contains("1 waiting"));
        assert!(out.contains("+100 -50"));

        // Verify ANSI codes are present (cyan for names)
        assert!(out.contains("\x1b[36m"));
    }

    // ── E2E tests with real git repos ───────────────────────────────

    #[test]
    fn e2e_configured_copy_runs_before_setup_and_from_uses_source() {
        for vcs in [vcs::VcsType::Git, vcs::VcsType::Jj] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("repo");
            fs::create_dir(&root).unwrap();
            let run = |program: &str, args: &[&str]| {
                let mut command = std::process::Command::new(program);
                command.current_dir(&root).args(args);
                crate::seed::output(command).unwrap()
            };
            if vcs == vcs::VcsType::Git {
                run("git", &["init", "-q", "-b", "main"]);
                run("git", &["config", "user.name", "dwm"]);
                run("git", &["config", "user.email", "dwm@example.com"]);
                run("git", &["config", "commit.gpgsign", "false"]);
            } else {
                run("jj", &["git", "init"]);
            }
            fs::write(root.join(".gitignore"), "cache/\nmarker\n").unwrap();
            fs::write(
                root.join(".dwm.toml"),
                "[workspace]\ncopy = ['cache']\n[scripts]\nsetup = 'cat cache/value > marker'\n",
            )
            .unwrap();
            if vcs == vcs::VcsType::Git {
                run("git", &["add", "."]);
                run("git", &["commit", "-qm", "fixture"]);
            } else {
                run("jj", &["describe", "-m", "fixture"]);
            }
            fs::create_dir(root.join("cache")).unwrap();
            fs::write(root.join("cache/value"), "main").unwrap();
            let deps = WorkspaceDeps {
                backend: vcs.to_backend(),
                cwd: root.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("one".into()),
                if vcs == vcs::VcsType::Jj {
                    Some("@")
                } else {
                    None
                },
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let one = worktrees_dir(&root).join("one");
            assert_eq!(fs::read_to_string(one.join("marker")).unwrap(), "main");
            fs::write(one.join("cache/value"), "source").unwrap();
            new_workspace_inner(
                &deps,
                Some("two".into()),
                None,
                Some("one"),
                Placement::Rex,
                &[],
            )
            .unwrap();
            assert_eq!(
                fs::read_to_string(worktrees_dir(&root).join("two/marker")).unwrap(),
                "source"
            );
            assert_eq!(
                fs::read_to_string(root.join("cache/value")).unwrap(),
                "main"
            );

            // Invalid configuration fails before creating VCS metadata or running setup.
            fs::write(
                root.join(".dwm.toml"),
                "[workspace]\ncopy = ['../escape']\n",
            )
            .unwrap();
            assert!(
                new_workspace_inner(
                    &deps,
                    Some("invalid".into()),
                    None,
                    None,
                    Placement::Rex,
                    &[]
                )
                .is_err()
            );
            assert!(!worktrees_dir(&root).join("invalid").exists());
            if vcs == vcs::VcsType::Jj {
                fs::write(root.join(".dwm.toml"), "[workspace]\ncheckout = 'cow'\n").unwrap();
                assert!(
                    new_workspace_inner(
                        &deps,
                        Some("unsupported".into()),
                        None,
                        None,
                        Placement::Rex,
                        &[]
                    )
                    .is_err()
                );
                assert!(!worktrees_dir(&root).join("unsupported").exists());
            }
        }
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Initialize a git repo with an initial commit.
    /// Returns the canonicalized repo path.
    fn init_git_repo(dir: &Path) -> PathBuf {
        let dir_str = dir.to_str().unwrap();
        std::process::Command::new("git")
            .args(["init", "-b", "main", dir_str])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                dir_str,
                "commit",
                "--allow-empty",
                "-m",
                "initial commit",
            ])
            .output()
            .unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn e2e_git_list_entries_main_only() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        let backend = crate::git::GitBackend::default();
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1, "should have main worktree entry");
        assert!(entries[0].is_main);
        assert_eq!(entries[0].name, "main-worktree");
        assert_eq!(entries[0].path, main_repo);
        assert_eq!(entries[0].description, "initial commit");
    }

    #[test]
    fn e2e_git_list_entries_with_worktree() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        // Create a git worktree at <repo>.dwm/worktrees/feat-branch
        let ws_path = worktrees_dir(&main_repo).join("feat-branch");
        fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                main_repo.to_str().unwrap(),
                "worktree",
                "add",
                ws_path.to_str().unwrap(),
                "-b",
                "feat-branch",
            ])
            .output()
            .unwrap();

        let backend = crate::git::GitBackend::default();
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert!(
            entries.len() >= 2,
            "should have main + worktree, got {}",
            entries.len()
        );

        let main_entry = entries.iter().find(|e| e.is_main).unwrap();
        assert_eq!(main_entry.name, "main-worktree");
        assert_eq!(main_entry.path, main_repo);

        let feat_entry = entries.iter().find(|e| e.name == "feat-branch").unwrap();
        assert!(!feat_entry.is_main);
        assert!(feat_entry.path.ends_with("feat-branch"));
        assert!(feat_entry.bookmarks.contains(&"feat-branch".to_string()));
    }

    #[test]
    fn e2e_git_new_and_delete_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let backend = crate::git::GitBackend::default();
            let deps = WorkspaceDeps {
                backend: Box::new(backend),
                cwd: main_repo.clone(),
                rex: None,
            };

            new_workspace_inner(
                &deps,
                Some("test-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("test-ws");
            assert!(ws_dir.exists(), "workspace dir should exist after creation");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "test-ws"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            delete_workspace_inner(
                &deps3,
                Some("test-ws".to_string()),
                DeleteOutput::Verbose,
                OpenPanes::Refuse,
            )
            .unwrap();
            assert!(!ws_dir.exists());

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps4).unwrap();
            assert!(!entries.iter().any(|e| e.name == "test-ws"));
        });
    }

    #[test]
    fn e2e_git_worktree_with_changes() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let backend = crate::git::GitBackend::default();
            let deps = WorkspaceDeps {
                backend: Box::new(backend),
                cwd: main_repo.clone(),
                rex: None,
            };

            new_workspace_inner(
                &deps,
                Some("feature".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("feature");

            fs::write(ws_dir.join("hello.txt"), "hello world\n").unwrap();
            let ws_str = ws_dir.to_str().unwrap();
            std::process::Command::new("git")
                .args(["-C", ws_str, "add", "hello.txt"])
                .output()
                .unwrap();
            std::process::Command::new("git")
                .args(["-C", ws_str, "commit", "-m", "add hello"])
                .output()
                .unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            let feat = entries.iter().find(|e| e.name == "feature").unwrap();
            assert_eq!(feat.description, "add hello");
            assert!(
                feat.diff_stat.insertions > 0 || feat.diff_stat.files_changed > 0,
                "feature workspace should show changes vs trunk"
            );
        });
    }

    #[test]
    fn e2e_git_rename_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let backend = crate::git::GitBackend::default();
            let deps = WorkspaceDeps {
                backend: Box::new(backend),
                cwd: main_repo.clone(),
                rex: None,
            };

            new_workspace_inner(
                &deps,
                Some("old-name".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let old_path = worktrees_dir(&main_repo).join("old-name");
            assert!(old_path.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            rename_workspace_inner(&deps2, "old-name", "new-name").unwrap();

            assert!(!old_path.exists(), "old dir should be gone");
            assert!(worktrees_dir(&main_repo).join("new-name").exists());

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps3).unwrap();
            assert!(entries.iter().any(|e| e.name == "new-name"));
            assert!(!entries.iter().any(|e| e.name == "old-name"));
        });
    }

    #[test]
    fn e2e_git_rename_redirects_when_inside() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };

            new_workspace_inner(
                &deps,
                Some("my-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_path = worktrees_dir(&main_repo).join("my-ws");
            let subdir = ws_path.join("src");
            fs::create_dir_all(&subdir).unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: subdir,
                rex: None,
            };
            let redirect = rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();
            let redirect = redirect.expect("should redirect when cwd is inside renamed workspace");
            assert_eq!(redirect, worktrees_dir(&main_repo).join("renamed-ws/src"));

            let new_ws = worktrees_dir(&main_repo).join("renamed-ws");
            assert!(new_ws.exists());
            assert!(new_ws.join("src").exists());
        });
    }

    fn jj_available() -> bool {
        std::process::Command::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn init_jj_repo(dir: &Path) -> PathBuf {
        let dir_str = dir.to_str().unwrap();
        std::process::Command::new("jj")
            .args(["git", "init", dir_str])
            .output()
            .unwrap();
        // Create a "main" bookmark so trunk() resolves
        std::process::Command::new("jj")
            .args([
                "--repository",
                dir_str,
                "bookmark",
                "create",
                "main",
                "-r",
                "@-",
            ])
            .output()
            .unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn e2e_jj_list_entries_main_only() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert_eq!(entries.len(), 1, "should have default workspace entry");
        assert!(entries[0].is_main);
        assert_eq!(entries[0].name, "default");
        assert_eq!(entries[0].path, main_repo);
    }

    #[test]
    fn e2e_jj_list_entries_with_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        let ws_path = worktrees_dir(&main_repo).join("feat-ws");
        fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::process::Command::new("jj")
            .args([
                "--repository",
                main_repo.to_str().unwrap(),
                "workspace",
                "add",
                "--name",
                "feat-ws",
                ws_path.to_str().unwrap(),
            ])
            .output()
            .unwrap();

        let backend = crate::jj::JjBackend;
        let deps = WorkspaceDeps {
            backend: Box::new(backend),
            cwd: main_repo.clone(),
            rex: None,
        };

        let entries = list_workspace_entries_inner(&deps).unwrap();
        assert!(
            entries.len() >= 2,
            "should have default + workspace, got {}",
            entries.len()
        );

        let main_entry = entries.iter().find(|e| e.is_main).unwrap();
        assert_eq!(main_entry.name, "default");
        assert_eq!(main_entry.path, main_repo);

        let feat_entry = entries.iter().find(|e| e.name == "feat-ws").unwrap();
        assert!(!feat_entry.is_main);
        assert!(feat_entry.path.ends_with("feat-ws"));
    }

    #[test]
    fn e2e_jj_new_and_delete_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("test-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("test-ws");
            assert!(ws_dir.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "test-ws"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            delete_workspace_inner(
                &deps3,
                Some("test-ws".to_string()),
                DeleteOutput::Verbose,
                OpenPanes::Refuse,
            )
            .unwrap();
            assert!(!ws_dir.exists());

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps4).unwrap();
            assert!(!entries.iter().any(|e| e.name == "test-ws"));
        });
    }

    #[test]
    fn e2e_jj_workspace_with_spaces_in_name() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("my cool feature".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("my cool feature");
            assert!(ws_dir.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            assert!(entries.iter().any(|e| e.name == "my cool feature"));

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let switch_path = switch_workspace_inner(&deps3, "my cool feature").unwrap();
            assert_eq!(switch_path, ws_dir);

            let deps4 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            delete_workspace_inner(
                &deps4,
                Some("my cool feature".to_string()),
                DeleteOutput::Verbose,
                OpenPanes::Refuse,
            )
            .unwrap();
            assert!(!ws_dir.exists());
        });
    }

    #[test]
    fn e2e_jj_workspace_with_changes() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("feature".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("feature");

            fs::write(ws_dir.join("hello.txt"), "hello world\n").unwrap();
            let ws_str = ws_dir.to_str().unwrap();
            std::process::Command::new("jj")
                .args(["--repository", ws_str, "describe", "-m", "add hello"])
                .output()
                .unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            let feat = entries.iter().find(|e| e.name == "feature").unwrap();
            assert_eq!(feat.description.trim(), "add hello");
            assert!(
                feat.diff_stat.insertions > 0 || feat.diff_stat.files_changed > 0,
                "feature workspace should show changes vs trunk: {:?}",
                feat.diff_stat
            );
        });
    }

    /// Regression: the workspace diff was taken from `trunk()` itself, so a
    /// workspace that had simply not been rebased reported every change that
    /// landed on trunk since it branched (inverted) on top of its own.
    #[test]
    fn e2e_jj_diff_stat_ignores_trunk_advancing() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);
        let main_str = main_repo.to_str().unwrap().to_string();

        let jj = |dir: &str, args: &[&str]| {
            let mut full = vec!["--repository", dir];
            full.extend_from_slice(args);
            let out = std::process::Command::new("jj")
                .args(&full)
                .output()
                .unwrap();
            assert!(out.status.success(), "jj {:?} failed: {:?}", args, out);
        };

        // Without a remote, the default `trunk()` resolves to the root commit,
        // which hides the difference this test is about. Point it at the local
        // `main` bookmark instead.
        jj(
            &main_str,
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.\"trunk()\"",
                "main",
            ],
        );

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("feature".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("feature");
            let ws_str = ws_dir.to_str().unwrap().to_string();

            // One line of work in the workspace.
            fs::write(ws_dir.join("hello.txt"), "hello\n").unwrap();
            jj(&ws_str, &["describe", "-m", "add hello"]);

            // Trunk moves on afterwards, touching files the workspace never saw.
            fs::write(main_repo.join("trunk-a.txt"), "one\ntwo\nthree\n").unwrap();
            fs::write(main_repo.join("trunk-b.txt"), "four\nfive\n").unwrap();
            jj(&main_str, &["describe", "-m", "trunk work"]);
            jj(&main_str, &["bookmark", "set", "main", "-r", "@"]);

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps2).unwrap();
            let feat = entries.iter().find(|e| e.name == "feature").unwrap();
            assert_eq!(
                (
                    feat.diff_stat.files_changed,
                    feat.diff_stat.insertions,
                    feat.diff_stat.deletions
                ),
                (1, 1, 0),
                "workspace should report only its own change, got {:?}",
                feat.diff_stat
            );
        });
    }

    #[test]
    fn e2e_jj_rename_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("old-name".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let old_path = worktrees_dir(&main_repo).join("old-name");
            assert!(old_path.exists());

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            rename_workspace_inner(&deps2, "old-name", "new-name").unwrap();

            assert!(!old_path.exists());
            let new_path = worktrees_dir(&main_repo).join("new-name");
            assert!(new_path.exists());

            // jj's own record of the workspace's location must follow the move.
            let out = std::process::Command::new("jj")
                .args(["workspace", "root", "--name", "new-name"])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                PathBuf::from(String::from_utf8(out.stdout).unwrap().trim()),
                fs::canonicalize(&new_path).unwrap()
            );

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps3).unwrap();
            assert!(entries.iter().any(|e| e.name == "new-name"));
            assert!(!entries.iter().any(|e| e.name == "old-name"));
        });
    }

    #[test]
    fn e2e_jj_rename_stale_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("my-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();

            let main_str = main_repo.to_str().unwrap();
            fs::write(main_repo.join("file.txt"), "content\n").unwrap();
            std::process::Command::new("jj")
                .args(["--repository", main_str, "describe", "-m", "advance op log"])
                .output()
                .unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();

            assert!(!worktrees_dir(&main_repo).join("my-ws").exists());
            assert!(worktrees_dir(&main_repo).join("renamed-ws").exists());

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo,
                rex: None,
            };
            let entries = list_workspace_entries_inner(&deps3).unwrap();
            assert!(entries.iter().any(|e| e.name == "renamed-ws"));
            assert!(!entries.iter().any(|e| e.name == "my-ws"));
        });
    }

    #[test]
    fn e2e_git_switch_workspace() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_git_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("switch-target".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("switch-target");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            let path = switch_workspace_inner(&deps2, "switch-target").unwrap();
            assert_eq!(path, ws_dir);

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::git::GitBackend::default()),
                cwd: main_repo.clone(),
                rex: None,
            };
            let path = switch_workspace_inner(&deps3, "main-worktree").unwrap();
            assert_eq!(path, main_repo);
        });
    }

    #[test]
    fn e2e_jj_switch_workspace() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("switch-target".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_dir = worktrees_dir(&main_repo).join("switch-target");

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let path = switch_workspace_inner(&deps2, "switch-target").unwrap();
            assert_eq!(path, ws_dir);

            let deps3 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            let path = switch_workspace_inner(&deps3, "default").unwrap();
            assert_eq!(path, main_repo);
        });
    }

    #[test]
    fn e2e_jj_rename_redirects_when_inside() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repos/myrepo");
        fs::create_dir_all(&repo_path).unwrap();
        let main_repo = init_jj_repo(&repo_path);

        with_data_dir(tmp.path(), || {
            let deps = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: main_repo.clone(),
                rex: None,
            };
            new_workspace_inner(
                &deps,
                Some("my-ws".to_string()),
                None,
                None,
                Placement::Rex,
                &[],
            )
            .unwrap();
            let ws_path = worktrees_dir(&main_repo).join("my-ws");
            let subdir = ws_path.join("src");
            fs::create_dir_all(&subdir).unwrap();

            let deps2 = WorkspaceDeps {
                backend: Box::new(crate::jj::JjBackend),
                cwd: subdir,
                rex: None,
            };
            let redirect = rename_workspace_inner(&deps2, "my-ws", "renamed-ws").unwrap();
            let redirect = redirect.expect("should redirect when cwd is inside renamed workspace");
            assert_eq!(redirect, worktrees_dir(&main_repo).join("renamed-ws/src"));

            let new_ws = worktrees_dir(&main_repo).join("renamed-ws");
            assert!(new_ws.exists());
            assert!(new_ws.join("src").exists());
        });
    }

    // ── prune e2e ─────────────────────────────────────────────────────

    /// Run a command, asserting success, with a fixed identity so commits can
    /// be pushed regardless of the developer's own VCS config.
    fn run_ok(dir: &Path, program: &str, args: &[&str]) -> String {
        let out = std::process::Command::new(program)
            .args(args)
            .current_dir(dir)
            .env("JJ_USER", "dwm test")
            .env("JJ_EMAIL", "dwm@example.com")
            .env("GIT_AUTHOR_NAME", "dwm test")
            .env("GIT_AUTHOR_EMAIL", "dwm@example.com")
            .env("GIT_COMMITTER_NAME", "dwm test")
            .env("GIT_COMMITTER_EMAIL", "dwm@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn verdict_of<'a>(outcome: &'a PruneOutcome, name: &str) -> &'a vcs::PruneVerdict {
        &outcome
            .verdicts
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no verdict for {name}: {:?}", outcome.verdicts))
            .1
    }

    fn assert_keep_contains(outcome: &PruneOutcome, name: &str, needle: &str) {
        match verdict_of(outcome, name) {
            vcs::PruneVerdict::Keep(reason) => {
                assert!(
                    reason.contains(needle),
                    "{name}: {reason:?} lacks {needle:?}"
                )
            }
            other => panic!("{name}: expected Keep({needle}), got {other:?}"),
        }
    }

    #[test]
    fn e2e_jj_prune() {
        assert!(jj_available(), "jj must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        run_ok(
            tmp.path(),
            "git",
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        let repo_path = tmp.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        run_ok(&repo_path, "jj", &["git", "init"]);
        let repo = repo_path.canonicalize().unwrap();
        run_ok(
            &repo,
            "jj",
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
        );
        fs::write(repo.join("base.txt"), "base").unwrap();
        run_ok(&repo, "jj", &["commit", "-m", "base"]);
        run_ok(&repo, "jj", &["bookmark", "create", "main", "-r", "@-"]);
        run_ok(
            &repo,
            "jj",
            &["git", "push", "--bookmark", "main", "--allow-new"],
        );

        let worktrees = worktrees_dir(&repo);
        fs::create_dir_all(&worktrees).unwrap();
        let add = |name: &str| {
            let path = worktrees.join(name);
            run_ok(
                &repo,
                "jj",
                &[
                    "workspace",
                    "add",
                    "--name",
                    name,
                    "-r",
                    "main",
                    path.to_str().unwrap(),
                ],
            );
            path
        };

        // Fresh workspace: nothing to lose.
        add("clean");
        // On-disk edit that no jj command has snapshotted yet.
        let dirty = add("dirty");
        fs::write(dirty.join("wip.txt"), "wip").unwrap();
        // A described commit that only exists locally.
        let local = add("local");
        fs::write(local.join("local.txt"), "local").unwrap();
        run_ok(&local, "jj", &["commit", "-m", "local work"]);
        // The same, but pushed to the remote.
        let pushed = add("pushed");
        fs::write(pushed.join("pushed.txt"), "pushed").unwrap();
        run_ok(&pushed, "jj", &["commit", "-m", "pushed work"]);
        run_ok(&pushed, "jj", &["bookmark", "create", "feat", "-r", "@-"]);
        run_ok(
            &pushed,
            "jj",
            &["git", "push", "--bookmark", "feat", "--allow-new"],
        );
        // Stale: its working-copy commit is rewritten from another workspace,
        // and it has an unsnapshotted edit on disk.
        let stale = add("stale");
        run_ok(&stale, "jj", &["status"]);
        run_ok(&repo, "jj", &["new", "main"]);
        fs::write(repo.join("other.txt"), "x").unwrap();
        run_ok(&repo, "jj", &["commit", "-m", "trunk moves"]);
        run_ok(&repo, "jj", &["rebase", "-r", "stale@", "-d", "@-"]);
        fs::write(stale.join("unsnapshotted.txt"), "precious").unwrap();

        let deps = WorkspaceDeps {
            backend: Box::new(crate::jj::JjBackend),
            cwd: repo.clone(),
            rex: None,
        };
        let dry = prune_workspaces_inner(&deps, PruneMode::DryRun).unwrap();
        assert_eq!(verdict_of(&dry, "clean"), &vcs::PruneVerdict::Prunable);
        assert_eq!(verdict_of(&dry, "pushed"), &vcs::PruneVerdict::Prunable);
        assert_keep_contains(&dry, "dirty", "uncommitted changes");
        assert_keep_contains(&dry, "local", "not on any remote bookmark");
        assert_keep_contains(&dry, "stale", "stale");
        assert!(worktrees.join("clean").exists(), "dry run must not delete");

        let done = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(
            done.deleted,
            vec!["clean".to_string(), "pushed".to_string()]
        );
        for gone in ["clean", "pushed"] {
            assert!(!worktrees.join(gone).exists(), "{gone} should be deleted");
        }
        for kept in ["dirty", "local", "stale"] {
            assert!(worktrees.join(kept).exists(), "{kept} should be kept");
        }
        // prune must never run `jj workspace update-stale`, which could
        // clobber this file.
        assert_eq!(
            fs::read_to_string(stale.join("unsnapshotted.txt")).unwrap(),
            "precious"
        );
        let listed = run_ok(&repo, "jj", &["workspace", "list", "-T", "name ++ \"\\n\""]);
        assert!(
            !listed.lines().any(|l| l == "clean" || l == "pushed"),
            "{listed}"
        );
        assert!(listed.lines().any(|l| l == "default"), "{listed}");
    }

    #[test]
    fn e2e_git_prune() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        run_ok(
            tmp.path(),
            "git",
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        let repo_path = tmp.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        run_ok(&repo_path, "git", &["init", "-q", "-b", "main"]);
        let repo = repo_path.canonicalize().unwrap();
        run_ok(
            &repo,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "base"],
        );
        run_ok(
            &repo,
            "git",
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run_ok(&repo, "git", &["push", "-q", "-u", "origin", "main"]);
        let worktrees = worktrees_dir(&repo);
        fs::create_dir_all(&worktrees).unwrap();
        let add = |name: &str| {
            let path = worktrees.join(name);
            run_ok(
                &repo,
                "git",
                &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
            );
            path
        };

        add("clean");
        let dirty = add("dirty");
        fs::write(dirty.join("untracked.txt"), "wip").unwrap();
        let ignored = add("ignored");
        fs::write(ignored.join(".gitignore"), "target/\n").unwrap();
        run_ok(&ignored, "git", &["add", ".gitignore"]);
        run_ok(&ignored, "git", &["commit", "-q", "-m", "ignore target"]);
        run_ok(&ignored, "git", &["push", "-q", "-u", "origin", "ignored"]);
        fs::create_dir_all(ignored.join("target")).unwrap();
        fs::write(ignored.join("target/build.o"), "artifact").unwrap();
        let local = add("local");
        run_ok(
            &local,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "local work"],
        );
        let ahead = add("ahead");
        run_ok(
            &ahead,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "pushed"],
        );
        run_ok(&ahead, "git", &["push", "-q", "-u", "origin", "ahead"]);
        run_ok(
            &ahead,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "not yet pushed"],
        );

        let deps = WorkspaceDeps {
            backend: Box::new(crate::git::GitBackend::default()),
            cwd: repo.clone(),
            rex: None,
        };
        let dry = prune_workspaces_inner(&deps, PruneMode::DryRun).unwrap();
        assert_eq!(verdict_of(&dry, "clean"), &vcs::PruneVerdict::Prunable);
        assert_eq!(verdict_of(&dry, "ignored"), &vcs::PruneVerdict::Prunable);
        assert_keep_contains(&dry, "dirty", "uncommitted change");
        assert_keep_contains(&dry, "local", "\"local work\"");
        assert_keep_contains(&dry, "ahead", "1 commit not pushed");
        assert!(!dry.verdicts.iter().any(|(n, _)| n == "main-worktree"));

        let done = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
        assert_eq!(
            done.deleted,
            vec!["clean".to_string(), "ignored".to_string()]
        );
        for kept in ["dirty", "local", "ahead"] {
            assert!(worktrees.join(kept).exists(), "{kept} should be kept");
        }
        let listed = run_ok(&repo, "git", &["worktree", "list", "--porcelain"]);
        assert!(!listed.contains("/clean\n"), "{listed}");
    }

    #[test]
    fn e2e_git_lease_reuses_and_resets_members() {
        assert!(git_available(), "git must be installed to run this test");
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        run_ok(
            tmp.path(),
            "git",
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        let repo_path = tmp.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        run_ok(&repo_path, "git", &["init", "-q", "-b", "main"]);
        let repo = repo_path.canonicalize().unwrap();
        run_ok(
            &repo,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "base"],
        );
        run_ok(
            &repo,
            "git",
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run_ok(&repo, "git", &["push", "-q", "-u", "origin", "main"]);
        let deps = WorkspaceDeps {
            backend: Box::new(crate::git::GitBackend::default()),
            cwd: repo.clone(),
            rex: None,
        };
        let lease = || {
            with_data_dir(tmp.path(), || {
                lease_workspace_inner(&deps, "agents", None).unwrap()
            })
        };

        let first = lease();
        assert_eq!(first, worktrees_dir(&repo).join("agents-1"));
        // Work that reached the remote is safe to discard on reuse.
        run_ok(
            &first,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "pushed work"],
        );
        run_ok(&first, "git", &["push", "-q", "-u", "origin", "agents-1"]);
        run_ok(
            &repo,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "trunk moves"],
        );
        release_workspace_inner(&deps, Some("agents-1".into())).unwrap();

        assert_eq!(lease(), first, "the released member is reused");
        let head = run_ok(&first, "git", &["log", "-1", "--format=%s"]);
        assert_eq!(head.trim(), "trunk moves", "reset to the current trunk");
        let branch = run_ok(&first, "git", &["branch", "--show-current"]);
        assert_eq!(branch.trim(), "agents-1", "still on its own branch");

        // An uncommitted change makes a released member unusable.
        fs::write(first.join("wip.txt"), "wip").unwrap();
        release_workspace_inner(&deps, Some("agents-1".into())).unwrap();
        assert_eq!(lease(), worktrees_dir(&repo).join("agents-2"));
        assert!(
            first.join("wip.txt").exists(),
            "the dirty member was left alone"
        );
    }

    // ── Rex integration ──────────────────────────────────────────────

    mod rex_integration {
        use super::*;
        use crate::rex::ProgramState::{Blocked, Idle, Working};
        use crate::rex::tests::{FakeRex, pane};

        /// A repo with workspaces `a` and `b`, and the deps to act on it from
        /// the main checkout with `fake` as the Rex server.
        fn fixture(
            tmp: &Path,
            fake: &FakeRex,
        ) -> (PathBuf, WorkspaceDeps, Arc<Mutex<Vec<MockCall>>>) {
            let main_repo = make_repo_with_vcs(tmp);
            for ws in ["a", "b"] {
                fs::create_dir_all(worktrees_dir(&main_repo).join(ws)).unwrap();
            }
            let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
            let mock = mock.with_prune_verdicts(&[
                ("a", vcs::PruneVerdict::Prunable),
                ("b", vcs::PruneVerdict::Prunable),
            ]);
            let deps = WorkspaceDeps {
                backend: Box::new(mock),
                cwd: main_repo.clone(),
                rex: fake.boxed(),
            };
            (main_repo, deps, calls)
        }

        fn ws(main_repo: &Path, name: &str) -> PathBuf {
            worktrees_dir(main_repo).join(name)
        }

        // switching

        #[test]
        fn switch_focuses_a_pane_already_in_the_workspace() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(a.join("src")).unwrap();
            let fake = FakeRex::new(
                vec![
                    pane("shell", "a", &a.join("src"), &[]),
                    pane("agent", "a", &a, &[Idle]),
                ],
                true,
            )
            .in_block("me");

            let cd = go_to_workspace(Some(&fake), &main_repo, &a, Placement::Rex).unwrap();
            assert_eq!(cd, None, "focusing a pane leaves the shell where it is");
            assert_eq!(fake.calls(), vec!["panes", "focus agent"]);
        }

        #[test]
        fn switch_falls_back_to_cd_when_rex_wont_show_the_session() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(vec![pane("p", "a", &a, &[])], false).in_block("me");

            let cd = go_to_workspace(Some(&fake), &main_repo, &a, Placement::Rex).unwrap();
            assert_eq!(cd, Some(a));
            assert_eq!(fake.calls(), vec!["panes", "focus p"]);
        }

        #[test]
        fn switch_opens_a_session_named_after_the_workspace() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            // A pane in the main checkout doesn't count for workspace `a`.
            let fake = FakeRex::new(vec![pane("p", "repo", &main_repo, &[])], true).in_block("me");

            let cd = go_to_workspace(Some(&fake), &main_repo, &a, Placement::Rex).unwrap();
            assert_eq!(cd, None);
            let opened = fake.opened.borrow();
            assert_eq!(opened.len(), 1);
            assert_eq!(opened[0].label, "a");
            assert_eq!(opened[0].cwd, a);
            assert_eq!(opened[0].create, crate::rex::CreateWhen::OnlyIfShown);
            assert_eq!(opened[0].initial_input, None);
        }

        #[test]
        fn switch_to_main_labels_the_session_after_the_repo() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(vec![], true).in_block("me");

            go_to_workspace(Some(&fake), &a, &main_repo, Placement::Rex).unwrap();
            assert_eq!(fake.opened.borrow()[0].label, "myrepo");
        }

        #[test]
        fn switch_changes_directory_when_this_pane_is_already_there() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(a.join("src")).unwrap();
            let fake = FakeRex::new(vec![pane("other", "a", &a, &[])], true).in_block("me");

            let cd = go_to_workspace(Some(&fake), &a.join("src"), &a, Placement::Rex).unwrap();
            assert_eq!(cd, Some(a));
            assert!(fake.calls().is_empty(), "no need to ask Rex anything");
        }

        #[test]
        fn switch_from_a_throwaway_pane_uses_rex_even_when_already_there() {
            // The palette's floating picker runs in the repo it lists, and
            // closes when dwm exits, so a cd there would do nothing visible.
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let mut fake =
                FakeRex::new(vec![pane("me", "float", &main_repo, &[])], true).in_block("me");
            fake.pane = crate::rex::CurrentPane::Throwaway;

            let cd = go_to_workspace(Some(&fake), &main_repo, &main_repo, Placement::Rex).unwrap();
            assert_eq!(cd, None);
            // The picker's own pane is never the one focused.
            assert_eq!(fake.calls(), vec!["panes", "open myrepo"]);
        }

        #[test]
        fn switch_here_and_outside_rex_change_directory() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(vec![pane("p", "a", &a, &[])], true);

            let cd = go_to_workspace(Some(&fake), &main_repo, &a, Placement::Here).unwrap();
            assert_eq!(cd, Some(a.clone()));
            assert!(fake.calls().is_empty());
            let cd = go_to_workspace(None, &main_repo, &a, Placement::Rex).unwrap();
            assert_eq!(cd, Some(a));
        }

        #[test]
        fn unattended_dwm_never_moves_rex() {
            // Regression guard: an agent running `dwm switch` inside a Rex pane
            // has REX_SERVER set, but must not switch the person's view.
            assert_eq!(
                effective_placement(Placement::Rex, Terminal::Unattended),
                Placement::Here
            );
            assert_eq!(
                effective_placement(Placement::Rex, Terminal::Interactive),
                Placement::Rex
            );
            assert_eq!(
                effective_placement(Placement::Here, Terminal::Interactive),
                Placement::Here
            );
        }

        // creating

        #[test]
        fn new_opens_a_session_instead_of_changing_directory() {
            let tmp = tempfile::tempdir().unwrap();
            let fake = FakeRex::new(vec![], true).in_block("me");
            let (main_repo, deps, _calls) = fixture(tmp.path(), &fake);

            let created = with_data_dir(tmp.path(), || {
                new_workspace_inner(&deps, Some("c".into()), None, None, Placement::Rex, &[])
                    .unwrap()
            });
            assert_eq!(created.name, "c");
            assert_eq!(created.cd, None);
            let opened = fake.opened.borrow();
            assert_eq!(opened[0].label, "c");
            assert_eq!(opened[0].cwd, ws(&main_repo, "c"));
        }

        #[test]
        fn new_with_command_types_it_into_the_session_even_when_unseen() {
            let tmp = tempfile::tempdir().unwrap();
            let fake = FakeRex::new(vec![], false).in_block("me");
            let (main_repo, deps, _calls) = fixture(tmp.path(), &fake);
            let command = vec!["claude".to_string(), "fix it".to_string()];

            let created = with_data_dir(tmp.path(), || {
                new_workspace_inner(
                    &deps,
                    Some("c".into()),
                    None,
                    None,
                    Placement::Rex,
                    &command,
                )
                .unwrap()
            });
            let opened = fake.opened.borrow();
            assert_eq!(
                opened[0].initial_input.as_deref(),
                Some("claude 'fix it'\r")
            );
            assert_eq!(opened[0].create, crate::rex::CreateWhen::Always);
            // The session runs unseen, so the shell still goes to the workspace.
            assert_eq!(created.cd, Some(ws(&main_repo, "c")));
        }

        #[test]
        fn new_with_command_outside_rex_fails_before_creating_anything() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
            let deps = WorkspaceDeps {
                backend: Box::new(mock),
                cwd: main_repo.clone(),
                rex: None,
            };
            let err = new_workspace_inner(
                &deps,
                Some("c".into()),
                None,
                None,
                Placement::Rex,
                &["claude".to_string()],
            )
            .unwrap_err();
            assert!(err.to_string().contains("needs a Rex session"), "{err}");
            assert!(calls.lock().unwrap().is_empty());
            assert!(!ws(&main_repo, "c").exists());
        }

        // deleting

        #[test]
        fn delete_refuses_while_a_pane_is_open_in_the_workspace() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(vec![pane("agent", "a-session", &a, &[Working])], true);
            let (_, deps, calls) = fixture(tmp.path(), &fake);

            let err = delete_workspace_inner(
                &deps,
                Some("a".into()),
                DeleteOutput::Quiet,
                OpenPanes::Refuse,
            )
            .unwrap_err();
            assert!(err.to_string().contains("open in Rex"), "{err}");
            assert!(err.to_string().contains("a-session"), "{err}");
            assert!(calls.lock().unwrap().is_empty(), "nothing removed");
            assert!(a.exists());
        }

        #[test]
        fn delete_can_close_the_panes_first() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(vec![pane("agent", "a", &a, &[Working])], true);
            let (_, deps, calls) = fixture(tmp.path(), &fake);

            delete_workspace_inner(
                &deps,
                Some("a".into()),
                DeleteOutput::Quiet,
                OpenPanes::Close,
            )
            .unwrap();
            assert_eq!(fake.calls(), vec!["panes", "close agent"]);
            assert_eq!(calls.lock().unwrap().len(), 1);
            assert!(!a.exists());
        }

        #[test]
        fn delete_ignores_the_pane_dwm_runs_in() {
            // That pane is sent back to the repo root by the shell wrapper.
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(vec![pane("me", "a", &a, &[])], true).in_block("me");
            let (_, mut deps, _) = fixture(tmp.path(), &fake);
            deps.cwd = a.clone();

            let redirect =
                delete_workspace_inner(&deps, None, DeleteOutput::Quiet, OpenPanes::Refuse)
                    .unwrap();
            assert_eq!(redirect, Some(main_repo));
        }

        // pruning

        #[test]
        fn prune_keeps_workspaces_open_in_rex() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(
                vec![pane("p1", "a", &a, &[Blocked]), pane("p2", "a", &a, &[])],
                true,
            );
            let (_, deps, calls) = fixture(tmp.path(), &fake);

            let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
            assert_eq!(outcome.deleted, vec!["b"]);
            let (_, verdict) = &outcome.verdicts[0];
            match verdict {
                vcs::PruneVerdict::Keep(reason) => {
                    assert_eq!(reason, "open in Rex session 'a' (zsh) and 1 more")
                }
                other => panic!("expected Keep, got {other:?}"),
            }
            // Only `b` reached the backend's delete.
            assert_eq!(calls.lock().unwrap().len(), 1);
            // One look decides every verdict; deleting `b` looks again just before
            // removing it.
            assert_eq!(fake.calls(), vec!["panes", "panes"]);
        }

        // renaming

        #[test]
        fn rename_relabels_the_workspace_session() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let mut renamed_session = pane("p1", "a", &a, &[]);
            renamed_session.session_id = "session:a".into();
            let mut second_pane = pane("p2", "a", &a, &[]);
            second_pane.session_id = "session:a".into();
            let other_session = pane("p3", "something-else", &a, &[]);
            let fake = FakeRex::new(vec![renamed_session, second_pane, other_session], true);
            let (_, deps, _) = fixture(tmp.path(), &fake);

            rename_workspace_inner(&deps, "a", "c").unwrap();
            assert_eq!(fake.calls(), vec!["panes", "rename session:a c"]);
        }

        // listing

        #[test]
        fn list_reports_rex_panes_and_their_agent_status() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(
                vec![
                    pane("p1", "a", &a, &[Blocked]),
                    pane("p2", "a", &a, &[Working]),
                    pane("p3", "repo", &main_repo, &[]),
                ],
                true,
            );
            let (_, deps, _) = fixture(tmp.path(), &fake);

            let entries = list_workspace_entries_inner(&deps).unwrap();
            let a_entry = entries.iter().find(|e| e.name == "a").unwrap();
            let agents = a_entry.agent_status.as_ref().unwrap();
            assert_eq!((agents.waiting, agents.working), (1, 1));
            assert_eq!(a_entry.rex_pane.as_ref().unwrap().block_id, "p1");
            let main = entries.iter().find(|e| e.is_main).unwrap();
            assert_eq!(main.rex_pane.as_ref().unwrap().block_id, "p3");
            assert_eq!(main.agent_status, None, "a plain shell is not an agent");
            let b_entry = entries.iter().find(|e| e.name == "b").unwrap();
            assert!(b_entry.rex_pane.is_none());
        }

        #[test]
        fn status_json_includes_rex_fields() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let a = ws(&main_repo, "a");
            fs::create_dir_all(&a).unwrap();
            let fake = FakeRex::new(vec![pane("p1", "a", &a, &[Working])], true);
            let (_, deps, _) = fixture(tmp.path(), &fake);

            let entries = list_workspace_entries_inner(&deps).unwrap();
            let json = status_json(&entries);
            let list = json["workspaces"].as_array().unwrap();
            let a_json = list.iter().find(|w| w["name"] == "a").unwrap();
            assert_eq!(a_json["rex_pane"]["block_id"], "p1");
            assert_eq!(a_json["agents"]["working"], 1);
            assert_eq!(a_json["session_label"], "a");
            assert_eq!(a_json["is_main"], false);
            let main = list.iter().find(|w| w["is_main"] == true).unwrap();
            assert_eq!(main["session_label"], "myrepo");
            assert!(main["agents"].is_null());
            assert!(main["rex_pane"].is_null());
        }

        // pools

        /// A repo whose workspaces are `names`, each with the given verdict.
        fn pool_fixture(
            tmp: &Path,
            fake: Option<&FakeRex>,
            members: &[(&str, vcs::PruneVerdict)],
        ) -> (PathBuf, WorkspaceDeps, Arc<Mutex<Vec<MockCall>>>) {
            let main_repo = make_repo_with_vcs(tmp);
            for (name, _) in members {
                fs::create_dir_all(worktrees_dir(&main_repo).join(name)).unwrap();
            }
            let (mock, calls) = MockBackend::new(main_repo.clone(), vec![]);
            let deps = WorkspaceDeps {
                backend: Box::new(mock.with_prune_verdicts(members)),
                cwd: main_repo.clone(),
                rex: fake.and_then(|f| f.boxed()),
            };
            (main_repo, deps, calls)
        }

        fn lease(tmp: &Path, deps: &WorkspaceDeps, at: Option<&str>) -> PathBuf {
            with_data_dir(tmp, || lease_workspace_inner(deps, "agents", at).unwrap())
        }

        #[test]
        fn lease_creates_the_first_member_from_trunk() {
            let tmp = tempfile::tempdir().unwrap();
            let fake = FakeRex::new(vec![], true).in_block("block:me");
            let (main_repo, deps, calls) = pool_fixture(tmp.path(), Some(&fake), &[]);

            let path = lease(tmp.path(), &deps, None);
            assert_eq!(path, ws(&main_repo, "agents-1"));
            match &calls.lock().unwrap()[..] {
                [MockCall::Add { name, at, .. }] => {
                    assert_eq!(name, "agents-1");
                    assert_eq!(at.as_deref(), Some("trunk()"));
                }
                other => panic!("expected one Add, got {other:?}"),
            }
            let lease = pool::read_lease(&dwm_dir(&main_repo), "agents-1").unwrap();
            assert_eq!(lease.rex_block.as_deref(), Some("block:me"));
        }

        #[test]
        fn lease_reuses_a_free_member_and_resets_it() {
            let tmp = tempfile::tempdir().unwrap();
            let (main_repo, deps, calls) = pool_fixture(
                tmp.path(),
                None,
                &[
                    ("agents-1", vcs::PruneVerdict::Prunable),
                    ("agents-2", vcs::PruneVerdict::Prunable),
                ],
            );

            let path = lease(tmp.path(), &deps, Some("main"));
            assert_eq!(path, ws(&main_repo, "agents-1"));
            match &calls.lock().unwrap()[..] {
                [MockCall::Reset { name, revision }] => {
                    assert_eq!((name.as_str(), revision.as_str()), ("agents-1", "main"))
                }
                other => panic!("expected one Reset, got {other:?}"),
            }
            assert!(pool::read_lease(&dwm_dir(&main_repo), "agents-1").is_some());

            // The next lease skips the member now on loan.
            let path = lease(tmp.path(), &deps, None);
            assert_eq!(path, ws(&main_repo, "agents-2"));
        }

        #[test]
        fn lease_skips_busy_members_and_creates_the_next() {
            let tmp = tempfile::tempdir().unwrap();
            let main_repo = make_repo_with_vcs(tmp.path());
            let open = ws(&main_repo, "agents-2");
            fs::create_dir_all(&open).unwrap();
            let fake =
                FakeRex::new(vec![pane("block:other", "x", &open, &[])], true).in_block("block:me");
            let (main_repo, deps, calls) = pool_fixture(
                tmp.path(),
                Some(&fake),
                &[
                    ("agents-1", vcs::PruneVerdict::Prunable), // leased below
                    ("agents-2", vcs::PruneVerdict::Prunable), // open in Rex
                    ("agents-3", keep("1 unpushed commit")),
                    ("other-9", vcs::PruneVerdict::Prunable), // not in the pool
                ],
            );
            pool::write_lease(
                &dwm_dir(&main_repo),
                &pool::Lease::new("agents-1", Some("block:other")),
            )
            .unwrap();

            let path = lease(tmp.path(), &deps, None);
            assert_eq!(path, ws(&main_repo, "agents-4"));
            assert!(
                matches!(&calls.lock().unwrap()[..], [MockCall::Add { name, .. }] if name == "agents-4"),
                "nothing reset"
            );
        }

        #[test]
        fn lease_reclaims_a_member_whose_rex_pane_closed() {
            let tmp = tempfile::tempdir().unwrap();
            let fake = FakeRex::new(vec![], true).in_block("block:me");
            let (main_repo, deps, _) = pool_fixture(
                tmp.path(),
                Some(&fake),
                &[("agents-1", vcs::PruneVerdict::Prunable)],
            );
            let dwm = dwm_dir(&main_repo);
            pool::write_lease(&dwm, &pool::Lease::new("agents-1", Some("block:gone"))).unwrap();

            assert_eq!(lease(tmp.path(), &deps, None), ws(&main_repo, "agents-1"));
            let lease = pool::read_lease(&dwm, "agents-1").unwrap();
            assert_eq!(
                lease.rex_block.as_deref(),
                Some("block:me"),
                "re-leased to us"
            );
        }

        #[test]
        fn lease_rejects_bad_pool_names() {
            let tmp = tempfile::tempdir().unwrap();
            let (_, deps, _) = pool_fixture(tmp.path(), None, &[]);
            assert!(lease_workspace_inner(&deps, "a/b", None).is_err());
            assert!(lease_workspace_inner(&deps, "..", None).is_err());
        }

        #[test]
        fn release_ends_a_lease_by_name_or_from_cwd() {
            let tmp = tempfile::tempdir().unwrap();
            let (main_repo, mut deps, _) = pool_fixture(
                tmp.path(),
                None,
                &[("agents-1", vcs::PruneVerdict::Prunable)],
            );
            let dwm = dwm_dir(&main_repo);
            pool::write_lease(&dwm, &pool::Lease::new("agents-1", None)).unwrap();

            deps.cwd = ws(&main_repo, "agents-1");
            release_workspace_inner(&deps, None).unwrap();
            assert!(pool::read_lease(&dwm, "agents-1").is_none());
            // Releasing again is harmless.
            release_workspace_inner(&deps, Some("agents-1".into())).unwrap();
        }

        #[test]
        fn prune_keeps_leased_workspaces() {
            let tmp = tempfile::tempdir().unwrap();
            let (main_repo, deps, _) = pool_fixture(
                tmp.path(),
                None,
                &[
                    ("agents-1", vcs::PruneVerdict::Prunable),
                    ("agents-2", vcs::PruneVerdict::Prunable),
                ],
            );
            pool::write_lease(&dwm_dir(&main_repo), &pool::Lease::new("agents-1", None)).unwrap();

            let outcome = prune_workspaces_inner(&deps, PruneMode::Delete).unwrap();
            assert_eq!(outcome.deleted, vec!["agents-2"]);
            assert!(
                matches!(&outcome.verdicts[0].1, vcs::PruneVerdict::Keep(r) if r.starts_with("leased")),
                "{:?}",
                outcome.verdicts
            );
        }

        #[test]
        fn delete_and_rename_carry_the_lease() {
            let tmp = tempfile::tempdir().unwrap();
            let (main_repo, deps, _) = pool_fixture(
                tmp.path(),
                None,
                &[
                    ("agents-1", vcs::PruneVerdict::Prunable),
                    ("agents-2", vcs::PruneVerdict::Prunable),
                ],
            );
            let dwm = dwm_dir(&main_repo);
            pool::write_lease(&dwm, &pool::Lease::new("agents-1", None)).unwrap();
            pool::write_lease(&dwm, &pool::Lease::new("agents-2", None)).unwrap();

            rename_workspace_inner(&deps, "agents-1", "agents-7").unwrap();
            assert!(pool::read_lease(&dwm, "agents-1").is_none());
            assert!(pool::read_lease(&dwm, "agents-7").is_some());

            delete_workspace_inner(
                &deps,
                Some("agents-2".into()),
                DeleteOutput::Quiet,
                OpenPanes::Refuse,
            )
            .unwrap();
            assert!(pool::read_lease(&dwm, "agents-2").is_none());
        }
    }
}
