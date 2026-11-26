//! Integration with the [Rex](https://www.superlogical.com/rex) terminal.
//!
//! Inside Rex, dwm can see every terminal pane on the server: the directory
//! its foreground process is in and the status programs in it have reported
//! with the Program Status Protocol (OSC 7501). Claude Code reports its state
//! that way, so dwm gets agent status without installing any hooks. dwm also
//! uses Rex to focus a pane that is already in a workspace, to open a new
//! session for a workspace, and to report its own progress.
//!
//! dwm talks to Rex by piping the embedded `rex/ops.lua` script to `rex do`.
//! Everything that interprets Rex's answers is a pure function in this module
//! so it can be tested without a Rex server.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::agent::AgentStatus;

/// The Lua script that performs every Rex operation dwm needs.
const OPS_SCRIPT: &str = include_str!("rex/ops.lua");

/// App identifier dwm uses in its own OSC 7501 reports.
const APP_ID: &str = "dwm";

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Whether dwm should use Rex, decided from the environment.
///
/// `DWM_REX=0` (or `off`/`false`) turns the integration off. `DWM_REX=1` turns
/// it on even without `REX_SERVER`, which the Rex command palette action uses
/// because programs it runs don't inherit a pane's environment. Otherwise dwm
/// uses Rex when it runs inside a Rex terminal, which sets `REX_SERVER`.
pub fn is_active() -> bool {
    is_active_with(
        std::env::var("DWM_REX").ok().as_deref(),
        std::env::var_os("REX_SERVER").is_some(),
    )
}

fn is_active_with(dwm_rex: Option<&str>, has_rex_server: bool) -> bool {
    match dwm_rex.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "0" | "off" | "false" | "no") => false,
        Some(v) if matches!(v.as_str(), "1" | "on" | "true" | "yes") => true,
        _ => has_rex_server,
    }
}

/// The `rex` executable to run: `DWM_REX_BIN` if set, otherwise `rex` on PATH.
fn rex_bin() -> OsString {
    std::env::var_os("DWM_REX_BIN").unwrap_or_else(|| OsString::from("rex"))
}

/// Return a live Rex client when dwm runs inside Rex.
pub fn connect() -> Option<Box<dyn RexApi>> {
    if is_active() {
        Some(Box::new(LiveRex {
            current_block: std::env::var("REX_BLOCK").ok().filter(|b| !b.is_empty()),
            pane: if std::env::var_os("DWM_REX_THROWAWAY_PANE").is_some() {
                CurrentPane::Throwaway
            } else {
                CurrentPane::Lasting
            },
        }))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// A program's state as reported with OSC 7501.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl ProgramState {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "working" => Some(Self::Working),
            "done" => Some(Self::Done),
            "blocked" => Some(Self::Blocked),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }

    /// How dwm shows this state in its agent column. Both `blocked` and
    /// `error` need the person to do something, which is what "waiting" means.
    pub fn agent_status(self) -> AgentStatus {
        match self {
            Self::Idle => AgentStatus::Idle,
            Self::Working => AgentStatus::Working,
            Self::Done => AgentStatus::Done,
            Self::Blocked | Self::Error => AgentStatus::Waiting,
        }
    }
}

/// One OSC 7501 record from a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRecord {
    pub state: ProgramState,
    pub app: Option<String>,
    /// What the program says it is doing or waiting for, already decoded.
    pub msg: Option<String>,
}

/// A terminal pane on the Rex server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub session_id: String,
    pub session_label: Option<String>,
    pub block_id: String,
    pub window_id: Option<String>,
    /// Where the pane's foreground process is, or its shell if that's unknown.
    pub cwd: Option<PathBuf>,
    /// The foreground program, such as `zsh` or `claude`.
    pub command: Option<String>,
    pub records: Vec<StatusRecord>,
}

impl Pane {
    /// A short name for the pane in messages: its session's label.
    pub fn describe(&self) -> String {
        match (&self.session_label, &self.command) {
            (Some(label), Some(cmd)) => format!("session '{label}' ({cmd})"),
            (Some(label), None) => format!("session '{label}'"),
            (None, Some(cmd)) => format!("a pane running {cmd}"),
            (None, None) => "a pane".to_string(),
        }
    }

    /// The most urgent status reported in this pane, as an agent status.
    pub fn agent_status(&self) -> Option<AgentStatus> {
        self.records
            .iter()
            .map(|r| r.state.agent_status())
            .max_by_key(|s| s.urgency())
    }

    /// The message of the most urgent record that has one, such as what an
    /// agent is waiting for.
    pub fn agent_note(&self) -> Option<crate::agent::AgentNote> {
        let mut records: Vec<_> = self.records.iter().collect();
        // Most urgent first; among equals, keep the protocol's order.
        records.sort_by_key(|r| std::cmp::Reverse(r.state.agent_status().urgency()));
        records.into_iter().find_map(|r| {
            let text = r.msg.as_deref()?.trim();
            (!text.is_empty()).then(|| crate::agent::AgentNote {
                status: r.state.agent_status(),
                text: text.to_string(),
            })
        })
    }

    fn is_inside(&self, dir: &Path) -> bool {
        self.cwd.as_deref().is_some_and(|cwd| cwd.starts_with(dir))
    }
}

/// Whether the Rex app showed what dwm asked it to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    Yes,
    /// The app didn't switch, usually because it doesn't accept actions from
    /// scripts. Holds Rex's reason.
    No(String),
}

/// When [`RexApi::open_session`] should create the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateWhen {
    /// Only when the app can show it. Otherwise nothing is created and the
    /// caller falls back to changing directory.
    OnlyIfShown,
    /// Always, because the session runs something worth having even unseen.
    Always,
}

/// A request to open a new Rex session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSession {
    pub label: String,
    pub cwd: PathBuf,
    /// Text typed at the new shell's prompt, such as a command and `\r`.
    pub initial_input: Option<String>,
    pub create: CreateWhen,
}

/// What [`RexApi::open_session`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub created: bool,
    pub shown: Shown,
}

/// The Rex operations dwm uses. [`LiveRex`] runs them against the server;
/// tests substitute a fake.
pub trait RexApi {
    /// Every terminal pane on the server.
    fn panes(&self) -> Result<Vec<Pane>>;
    /// The pane dwm itself runs in, if any.
    fn current_block(&self) -> Option<&str>;
    /// Whether the pane dwm runs in closes when dwm exits.
    fn current_pane(&self) -> CurrentPane {
        CurrentPane::Lasting
    }
    /// Focus a pane and ask the app to show its session.
    fn focus(&self, pane: &Pane) -> Result<Shown>;
    /// Create a session with one shell pane and ask the app to show it.
    fn open_session(&self, req: &OpenSession) -> Result<Opened>;
    /// Close a pane, ending the program in it.
    fn close_pane(&self, pane: &Pane) -> Result<()>;
    /// Set a session's label.
    fn rename_session(&self, session_id: &str, label: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Parsing what ops.lua returns
// ---------------------------------------------------------------------------

/// Deserialize a list that Lua may have encoded as `{}`.
///
/// Rex encodes an empty Lua table as a JSON object, so an empty list arrives
/// as `{}` rather than `[]`. `null` and a missing field are empty too.
fn lua_list<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(d)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Object(m)) if m.is_empty() => Ok(Vec::new()),
        Some(v @ serde_json::Value::Array(_)) => {
            serde_json::from_value(v).map_err(serde::de::Error::custom)
        }
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected a list, got {other}"
        ))),
    }
}

#[derive(Deserialize)]
struct RawScan {
    #[serde(default, deserialize_with = "lua_list")]
    panes: Vec<RawPane>,
}

#[derive(Deserialize)]
struct RawPane {
    session_id: String,
    session_label: Option<String>,
    block_id: String,
    window_id: Option<String>,
    cwd: Option<String>,
    command: Option<String>,
    #[serde(default, deserialize_with = "lua_list")]
    records: Vec<RawRecord>,
}

#[derive(Deserialize)]
struct RawRecord {
    state: Option<String>,
    app: Option<String>,
    effective_app: Option<String>,
    msg: Option<String>,
}

/// Parse the output of the `scan` operation.
///
/// Records with a state dwm doesn't know are dropped, as the protocol asks.
fn parse_scan(json: &str) -> Result<Vec<Pane>> {
    let raw: RawScan = serde_json::from_str(json).context("unexpected Rex scan output")?;
    Ok(raw
        .panes
        .into_iter()
        .map(|p| Pane {
            session_id: p.session_id,
            session_label: p.session_label.filter(|l| !l.is_empty()),
            block_id: p.block_id,
            window_id: p.window_id,
            cwd: p.cwd.filter(|c| !c.is_empty()).map(PathBuf::from),
            command: p.command.filter(|c| !c.is_empty()),
            records: p
                .records
                .into_iter()
                .filter_map(|r| {
                    Some(StatusRecord {
                        state: ProgramState::parse(r.state.as_deref()?)?,
                        app: r.effective_app.or(r.app),
                        msg: r.msg.filter(|m| !m.is_empty()),
                    })
                })
                .collect(),
        })
        .collect())
}

#[derive(Deserialize)]
struct RawShown {
    shown: bool,
    reason: Option<String>,
    #[serde(default)]
    created: bool,
}

impl RawShown {
    fn shown(&self) -> Shown {
        if self.shown {
            Shown::Yes
        } else {
            Shown::No(
                self.reason
                    .clone()
                    .unwrap_or_else(|| "Rex didn't say why".to_string()),
            )
        }
    }
}

fn parse_shown(json: &str) -> Result<RawShown> {
    serde_json::from_str(json).context("unexpected Rex output")
}

// ---------------------------------------------------------------------------
// Live client
// ---------------------------------------------------------------------------

/// Runs operations against the Rex server with `rex do`.
pub struct LiveRex {
    current_block: Option<String>,
    pane: CurrentPane,
}

/// Whether the pane dwm runs in outlives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurrentPane {
    /// An ordinary pane, which `cd` can move into a workspace.
    #[default]
    Lasting,
    /// A pane that closes when dwm exits, such as the floating picker the
    /// command palette opens (`DWM_REX_THROWAWAY_PANE`). Changing its
    /// directory achieves nothing, so dwm always uses Rex from it.
    Throwaway,
}

impl LiveRex {
    fn run(&self, args: serde_json::Value) -> Result<String> {
        let args = serde_json::to_string(&args)?;
        let mut child = Command::new(rex_bin())
            .args(["do", "-", "--args", &args])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not run `rex`; is it on PATH?")?;
        child
            .stdin
            .take()
            .context("rex stdin unavailable")?
            .write_all(OPS_SCRIPT.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.trim().trim_start_matches("Error: ");
            bail!("rex: {}", if msg.is_empty() { "failed" } else { msg });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl RexApi for LiveRex {
    fn panes(&self) -> Result<Vec<Pane>> {
        parse_scan(&self.run(serde_json::json!({ "op": "scan" }))?)
    }

    fn current_block(&self) -> Option<&str> {
        self.current_block.as_deref()
    }

    fn current_pane(&self) -> CurrentPane {
        self.pane
    }

    fn focus(&self, pane: &Pane) -> Result<Shown> {
        let out = self.run(serde_json::json!({
            "op": "focus",
            "session_id": pane.session_id,
            "block_id": pane.block_id,
            "window_id": pane.window_id,
        }))?;
        Ok(parse_shown(&out)?.shown())
    }

    fn open_session(&self, req: &OpenSession) -> Result<Opened> {
        let out = self.run(serde_json::json!({
            "op": "open",
            "label": req.label,
            "cwd": req.cwd,
            "initial_input": req.initial_input,
            "always_create": req.create == CreateWhen::Always,
        }))?;
        let raw = parse_shown(&out)?;
        Ok(Opened {
            created: raw.created,
            shown: raw.shown(),
        })
    }

    fn close_pane(&self, pane: &Pane) -> Result<()> {
        self.run(serde_json::json!({
            "op": "close",
            "session_id": pane.session_id,
            "block_id": pane.block_id,
        }))?;
        Ok(())
    }

    fn rename_session(&self, session_id: &str, label: &str) -> Result<()> {
        self.run(serde_json::json!({
            "op": "rename",
            "session_id": session_id,
            "label": label,
        }))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Resolve symlinks in `path` when it exists, so it compares equal to the
/// real paths Rex reports for processes.
pub fn real_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The panes whose working directory is `dir` or inside it, leaving out the
/// pane dwm runs in.
pub fn panes_inside<'a>(
    panes: &'a [Pane],
    dir: &Path,
    current_block: Option<&str>,
) -> Vec<&'a Pane> {
    let dir = real_path(dir);
    panes
        .iter()
        .filter(|p| Some(p.block_id.as_str()) != current_block)
        .filter(|p| p.is_inside(&dir))
        .collect()
}

/// Choose the pane to focus for a workspace: one where a program reports a
/// status (usually an agent), otherwise the first.
pub fn pick_pane<'a>(candidates: &[&'a Pane]) -> Option<&'a Pane> {
    candidates
        .iter()
        .find(|p| !p.records.is_empty())
        .or_else(|| candidates.first())
        .copied()
}

/// Quote `args` as one POSIX shell command line.
pub fn shell_command_line(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%^".contains(c));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

// ---------------------------------------------------------------------------
// Reporting dwm's own status (OSC 7501)
// ---------------------------------------------------------------------------

/// Longest message the protocol accepts, in bytes before encoding.
const MAX_MSG_BYTES: usize = 2048;

/// Standard base64 with padding.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Build an OSC 7501 report for dwm's root record.
///
/// The protocol refuses a whole report whose message has a control
/// character, so those are replaced with spaces, and long messages are cut at
/// a character boundary to fit.
fn osc_status(state: ProgramState, msg: &str) -> String {
    let mut clean: String = msg
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.len() > MAX_MSG_BYTES {
        let mut end = MAX_MSG_BYTES;
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        clean.truncate(end);
    }
    format!(
        "\x1b]7501;state={}:app={APP_ID}:msg={}\x1b\\",
        state.as_str(),
        base64(clean.as_bytes())
    )
}

/// Tells Rex what a long-running dwm command is doing, on the tab of the pane
/// it runs in. Reports `working` when started and `done` from [`Self::done`];
/// dropping it without calling `done` reports `error`.
///
/// Reports go to stderr, because stdout carries paths for the shell wrapper,
/// and only when stderr is a terminal inside Rex.
pub struct StatusReport {
    enabled: bool,
    finished: bool,
}

impl StatusReport {
    pub fn start(msg: &str) -> Self {
        let enabled = is_active()
            && std::env::var_os("REX_SERVER").is_some()
            && std::io::stderr().is_terminal();
        let report = Self {
            enabled,
            finished: false,
        };
        report.emit(ProgramState::Working, msg);
        report
    }

    fn emit(&self, state: ProgramState, msg: &str) {
        if self.enabled {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(osc_status(state, msg).as_bytes());
            let _ = err.flush();
        }
    }

    pub fn done(mut self, msg: &str) {
        self.emit(ProgramState::Done, msg);
        self.finished = true;
    }
}

impl Drop for StatusReport {
    fn drop(&mut self) {
        if !self.finished {
            self.emit(ProgramState::Error, "dwm failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Command palette integration (`dwm setup`)
// ---------------------------------------------------------------------------

/// The line `dwm setup` adds to Rex's `init.lua`.
const REQUIRE_LINE: &str = r#"require("dwm")"#;

/// Rex's configuration file: `REX_CONFIG`, or `rex/init.lua` in the config
/// directory (`$XDG_CONFIG_HOME` or `~/.config` on macOS and Linux).
fn rex_config_file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("REX_CONFIG") {
        return Some(PathBuf::from(p));
    }
    if cfg!(windows) {
        return dirs::config_dir().map(|d| d.join("rex").join("init.lua"));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("rex").join("init.lua"))
}

/// Render `dwm.lua`, the module that adds dwm's command palette action.
///
/// Programs Rex starts get the server's minimal PATH rather than the one a
/// login shell sets up, so the module runs dwm and rex by absolute path and
/// hands dwm `path` (the PATH of the shell running setup) to find jj and git.
pub fn palette_module(dwm: &Path, rex: &Path, path: &str) -> String {
    // A JSON string is a valid Lua string literal for any path text.
    let lua_str = |s: &str| serde_json::to_string(s).expect("a string always serializes");
    include_str!("rex/palette.lua")
        .replace("\"@DWM@\"", &lua_str(&dwm.to_string_lossy()))
        .replace("\"@REX@\"", &lua_str(&rex.to_string_lossy()))
        .replace("\"@PATH@\"", &lua_str(path))
}

/// Whether `init` already loads the dwm module.
fn has_require_line(init: &str) -> bool {
    init.lines().any(|l| {
        let l = l.trim();
        l == REQUIRE_LINE || l == "require('dwm')" || l == "require \"dwm\""
    })
}

/// Return `init` with the require line appended.
fn add_require_line(init: &str) -> String {
    let mut out = init.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("-- dwm workspaces in the command palette (added by `dwm setup`)\n");
    out.push_str(REQUIRE_LINE);
    out.push('\n');
    out
}

/// Find an executable on PATH.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Offer to add dwm's workspace picker to Rex's command palette.
pub fn setup_palette_interactive() -> Result<()> {
    use owo_colors::OwoColorize;

    let rex = std::env::var_os("DWM_REX_BIN")
        .map(PathBuf::from)
        .or_else(|| which("rex"));
    let Some(rex) = rex else {
        eprintln!("  {} Rex not found on PATH; skipping", "-".dimmed());
        return Ok(());
    };
    let Some(init_path) = rex_config_file() else {
        eprintln!("  {} Could not find Rex's config directory", "-".dimmed());
        return Ok(());
    };
    let dir = init_path
        .parent()
        .context("Rex config file has no directory")?
        .to_path_buf();
    let module_path = dir.join("dwm.lua");
    let dwm = std::env::current_exe().context("could not locate the dwm executable")?;
    let path = std::env::var("PATH").unwrap_or_default();
    let module = palette_module(&real_path(&dwm), &real_path(&rex), &path);

    let init = std::fs::read_to_string(&init_path).unwrap_or_default();
    let display = crate::agent::display_path(&init_path);
    if has_require_line(&init) {
        // Keep the module's paths current, for example after reinstalling dwm.
        if std::fs::read_to_string(&module_path).ok().as_deref() != Some(module.as_str()) {
            std::fs::write(&module_path, &module)?;
            reload_config(&rex);
        }
        eprintln!(
            "  {} Command palette action installed in {}",
            "✓".green(),
            display.dimmed()
        );
        return Ok(());
    }

    eprint!(
        "  {} Add a \"dwm: Switch Workspace\" action to Rex's command palette ({})? [y/N] ",
        "?".bold().cyan(),
        display.bold()
    );
    if !crate::agent::confirm_from_tty()? {
        return Ok(());
    }

    std::fs::create_dir_all(&dir)?;
    std::fs::write(&module_path, &module)?;
    std::fs::write(&init_path, add_require_line(&init))?;
    reload_config(&rex);
    eprintln!(
        "  {} Installed; find it in the command palette, or bind it with {}",
        "✓".green(),
        r#"rex.bind("cmd+shift+w", "dwm_switch")"#.bold()
    );
    Ok(())
}

fn reload_config(rex: &Path) {
    let _ = Command::new(rex)
        .args(["config", "reload"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A pane's link to Rex, in `dwm status --json`.
#[derive(Debug, Serialize)]
pub struct PaneRef {
    pub session_id: String,
    pub session_label: Option<String>,
    pub block_id: String,
    pub window_id: Option<String>,
}

impl From<&Pane> for PaneRef {
    fn from(p: &Pane) -> Self {
        Self {
            session_id: p.session_id.clone(),
            session_label: p.session_label.clone(),
            block_id: p.block_id.clone(),
            window_id: p.window_id.clone(),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A Rex server held in memory, recording what dwm asks of it. The logs
    /// are shared so a test can read them after boxing the fake.
    #[derive(Default, Clone)]
    pub struct FakeRex {
        pub panes: Vec<Pane>,
        pub current: Option<String>,
        pub pane: CurrentPane,
        /// Whether the app accepts requests to show a session.
        pub can_show: bool,
        pub calls: Rc<RefCell<Vec<String>>>,
        pub opened: Rc<RefCell<Vec<OpenSession>>>,
    }

    impl FakeRex {
        pub fn new(panes: Vec<Pane>, can_show: bool) -> Self {
            Self {
                panes,
                can_show,
                ..Default::default()
            }
        }

        pub fn in_block(mut self, block: &str) -> Self {
            self.current = Some(block.to_string());
            self
        }

        pub fn boxed(&self) -> Option<Box<dyn RexApi>> {
            Some(Box::new(self.clone()))
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn shown(&self) -> Shown {
            if self.can_show {
                Shown::Yes
            } else {
                Shown::No("Remote Control is off".into())
            }
        }
    }

    impl RexApi for FakeRex {
        fn panes(&self) -> Result<Vec<Pane>> {
            self.calls.borrow_mut().push("panes".into());
            Ok(self.panes.clone())
        }
        fn current_block(&self) -> Option<&str> {
            self.current.as_deref()
        }
        fn current_pane(&self) -> CurrentPane {
            self.pane
        }
        fn focus(&self, pane: &Pane) -> Result<Shown> {
            self.calls
                .borrow_mut()
                .push(format!("focus {}", pane.block_id));
            Ok(self.shown())
        }
        fn open_session(&self, req: &OpenSession) -> Result<Opened> {
            self.calls.borrow_mut().push(format!("open {}", req.label));
            self.opened.borrow_mut().push(req.clone());
            let created = self.can_show || req.create == CreateWhen::Always;
            Ok(Opened {
                created,
                shown: self.shown(),
            })
        }
        fn close_pane(&self, pane: &Pane) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("close {}", pane.block_id));
            Ok(())
        }
        fn rename_session(&self, session_id: &str, label: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("rename {session_id} {label}"));
            Ok(())
        }
    }

    /// A pane in `cwd` with the given OSC 7501 states.
    pub fn pane(block: &str, session_label: &str, cwd: &Path, states: &[ProgramState]) -> Pane {
        Pane {
            session_id: format!("session:{block}"),
            session_label: Some(session_label.to_string()),
            block_id: block.to_string(),
            window_id: Some(format!("window:{block}")),
            cwd: Some(real_path(cwd)),
            command: Some("zsh".into()),
            records: states
                .iter()
                .map(|&state| StatusRecord {
                    state,
                    app: Some("claude-code".into()),
                    msg: None,
                })
                .collect(),
        }
    }

    // ── detection ───────────────────────────────────────────────────

    #[test]
    fn active_inside_rex_by_default() {
        assert!(is_active_with(None, true));
        assert!(!is_active_with(None, false));
    }

    #[test]
    fn dwm_rex_env_overrides_detection() {
        assert!(!is_active_with(Some("0"), true));
        assert!(!is_active_with(Some("off"), true));
        assert!(is_active_with(Some("1"), false));
        assert!(is_active_with(Some(" ON "), false));
        // Anything else falls back to detection.
        assert!(is_active_with(Some("maybe"), true));
        assert!(!is_active_with(Some(""), false));
    }

    // ── parsing ─────────────────────────────────────────────────────

    /// Output recorded from a real Rex server (trimmed), including a pane
    /// whose `records` Rex encoded as `{}` because the Lua table was empty.
    const SCAN_OUTPUT: &str = r#"{"panes":[
        {"block_id":"block:a","cwd":"/Users/me/dev/repo.dwm/worktrees/foo","command":"claude",
         "records":[{"app":"claude-code","effective_app":"claude-code","msg":"Reading files","owner":{"name":"2.1.332","pid":57409},"state":"working","updated_at":"2026-11-26T15:04:43.9115Z"}],
         "session_id":"session:1","session_label":"foo","window_id":"window:1"},
        {"block_id":"block:b","cwd":"/Users/me/dev/repo","command":"-zsh","records":{},
         "session_id":"session:2","session_label":"repo","window_id":"window:2"}
    ]}"#;

    #[test]
    fn parse_scan_reads_panes_and_records() {
        let panes = parse_scan(SCAN_OUTPUT).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].block_id, "block:a");
        assert_eq!(panes[0].session_label.as_deref(), Some("foo"));
        assert_eq!(
            panes[0].cwd.as_deref(),
            Some(Path::new("/Users/me/dev/repo.dwm/worktrees/foo"))
        );
        assert_eq!(
            panes[0].records,
            vec![StatusRecord {
                state: ProgramState::Working,
                app: Some("claude-code".into()),
                msg: Some("Reading files".into()),
            }]
        );
    }

    #[test]
    fn parse_scan_accepts_empty_lua_tables_as_lists() {
        // Regression guard: Rex encodes an empty Lua list as `{}`.
        let panes = parse_scan(SCAN_OUTPUT).unwrap();
        assert!(panes[1].records.is_empty());
        assert!(parse_scan(r#"{"panes":{}}"#).unwrap().is_empty());
        assert!(parse_scan(r#"{}"#).unwrap().is_empty());
    }

    #[test]
    fn parse_scan_drops_unknown_states() {
        let json = r#"{"panes":[{"session_id":"s","block_id":"b","records":[
            {"state":"thinking","app":"x"},{"state":"blocked","app":"x"},{"app":"x"}]}]}"#;
        let panes = parse_scan(json).unwrap();
        assert_eq!(panes[0].records.len(), 1);
        assert_eq!(panes[0].records[0].state, ProgramState::Blocked);
    }

    #[test]
    fn parse_scan_treats_empty_strings_as_missing() {
        let json = r#"{"panes":[{"session_id":"s","block_id":"b","cwd":"","command":"","session_label":""}]}"#;
        let p = &parse_scan(json).unwrap()[0];
        assert_eq!(p.cwd, None);
        assert_eq!(p.command, None);
        assert_eq!(p.session_label, None);
    }

    #[test]
    fn parse_scan_rejects_garbage() {
        assert!(parse_scan("not json").is_err());
        assert!(parse_scan(r#"{"panes":"nope"}"#).is_err());
    }

    #[test]
    fn parse_shown_reports_reason() {
        let raw =
            parse_shown(r#"{"shown":false,"reason":"Remote Control is off","created":false}"#)
                .unwrap();
        assert_eq!(raw.shown(), Shown::No("Remote Control is off".into()));
        assert!(!raw.created);
        let raw = parse_shown(r#"{"shown":true,"created":true,"session_id":"s"}"#).unwrap();
        assert_eq!(raw.shown(), Shown::Yes);
        assert!(raw.created);
    }

    // ── state mapping ───────────────────────────────────────────────

    #[test]
    fn program_states_map_to_agent_statuses() {
        assert_eq!(ProgramState::Working.agent_status(), AgentStatus::Working);
        assert_eq!(ProgramState::Idle.agent_status(), AgentStatus::Idle);
        assert_eq!(ProgramState::Done.agent_status(), AgentStatus::Done);
        assert_eq!(ProgramState::Blocked.agent_status(), AgentStatus::Waiting);
        assert_eq!(ProgramState::Error.agent_status(), AgentStatus::Waiting);
    }

    #[test]
    fn pane_status_is_most_urgent_record() {
        let p = pane(
            "b",
            "s",
            Path::new("/x"),
            &[
                ProgramState::Idle,
                ProgramState::Blocked,
                ProgramState::Working,
            ],
        );
        assert_eq!(p.agent_status(), Some(AgentStatus::Waiting));
        let p = pane(
            "b",
            "s",
            Path::new("/x"),
            &[ProgramState::Done, ProgramState::Idle],
        );
        assert_eq!(p.agent_status(), Some(AgentStatus::Done));
        assert_eq!(pane("b", "s", Path::new("/x"), &[]).agent_status(), None);
    }

    #[test]
    fn agent_note_takes_the_most_urgent_message() {
        let mut p = pane("b", "s", Path::new("/x"), &[]);
        let rec = |state, msg: Option<&str>| StatusRecord {
            state,
            app: None,
            msg: msg.map(String::from),
        };
        p.records = vec![
            rec(ProgramState::Working, Some("Running tests")),
            rec(ProgramState::Blocked, Some("Allow edit to src/foo.rs?")),
            rec(ProgramState::Idle, Some("Ready")),
        ];
        let note = p.agent_note().unwrap();
        assert_eq!(note.status, AgentStatus::Waiting);
        assert_eq!(note.text, "Allow edit to src/foo.rs?");

        // A more urgent record without a message gives way to one with.
        p.records = vec![
            rec(ProgramState::Blocked, None),
            rec(ProgramState::Working, Some("  Running tests  ")),
        ];
        let note = p.agent_note().unwrap();
        assert_eq!(
            (note.status, note.text.as_str()),
            (AgentStatus::Working, "Running tests")
        );

        p.records = vec![rec(ProgramState::Working, Some("   "))];
        assert_eq!(p.agent_note(), None);
    }

    // ── pane selection ──────────────────────────────────────────────

    #[test]
    fn panes_inside_matches_subdirectories_and_skips_current() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let sub = ws.join("src");
        let other = tmp.path().join("ws-other");
        for d in [&sub, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        let panes = vec![
            pane("in-root", "a", &ws, &[]),
            pane("in-sub", "b", &sub, &[]),
            pane("sibling", "c", &other, &[]),
            pane("me", "d", &ws, &[]),
        ];
        let inside: Vec<_> = panes_inside(&panes, &ws, Some("me"))
            .into_iter()
            .map(|p| p.block_id.as_str())
            .collect();
        // `ws-other` shares a string prefix with `ws` but isn't inside it.
        assert_eq!(inside, vec!["in-root", "in-sub"]);
    }

    #[test]
    fn panes_inside_resolves_symlinked_workspace_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let panes = vec![pane("a", "a", &real, &[])];
        assert_eq!(panes_inside(&panes, &link, None).len(), 1);
    }

    #[test]
    fn pick_pane_prefers_one_reporting_status() {
        let plain = pane("plain", "a", Path::new("/w"), &[]);
        let agent = pane("agent", "b", Path::new("/w"), &[ProgramState::Idle]);
        assert_eq!(pick_pane(&[&plain, &agent]).unwrap().block_id, "agent");
        assert_eq!(pick_pane(&[&plain]).unwrap().block_id, "plain");
        assert!(pick_pane(&[]).is_none());
    }

    // ── shell quoting ───────────────────────────────────────────────

    #[test]
    fn shell_command_line_quotes_only_when_needed() {
        let args: Vec<String> = ["claude", "fix the bug", "it's", "--flag=x", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            shell_command_line(&args),
            r#"claude 'fix the bug' 'it'\''s' --flag=x ''"#
        );
    }

    #[test]
    fn shell_quote_handles_metacharacters() {
        for arg in ["$(rm -rf /)", "a;b", "a|b", "`x`", "*", "~", "a\nb"] {
            assert!(shell_quote(arg).starts_with('\''), "{arg:?} must be quoted");
        }
    }

    // ── OSC 7501 ────────────────────────────────────────────────────

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, want) in cases {
            assert_eq!(base64(input.as_bytes()), want, "base64({input:?})");
        }
    }

    #[test]
    fn osc_status_formats_a_report() {
        assert_eq!(
            osc_status(ProgramState::Working, "foo"),
            "\x1b]7501;state=working:app=dwm:msg=Zm9v\x1b\\"
        );
    }

    #[test]
    fn osc_status_strips_control_characters() {
        // The protocol discards a whole report whose text has one.
        let report = osc_status(ProgramState::Done, "a\nb\tc\x1b");
        assert!(report.ends_with(&format!("msg={}\x1b\\", base64(b"a b c "))));
    }

    #[test]
    fn osc_status_truncates_long_messages_on_char_boundary() {
        let msg = "é".repeat(MAX_MSG_BYTES); // 2 bytes each
        let report = osc_status(ProgramState::Done, &msg);
        let encoded = report
            .split("msg=")
            .nth(1)
            .unwrap()
            .trim_end_matches("\x1b\\");
        // 2048 bytes encodes to 2732 characters, the protocol's limit.
        assert!(encoded.len() <= 2732, "{} > 2732", encoded.len());
    }

    // ── palette module and init.lua ─────────────────────────────────

    #[test]
    fn palette_module_embeds_quoted_paths() {
        let m = palette_module(
            Path::new("/opt/my \"dwm\""),
            Path::new("/usr/bin/rex"),
            "/opt/homebrew/bin:/usr/bin",
        );
        assert!(m.contains(r#"local DWM = "/opt/my \"dwm\"""#), "{m}");
        assert!(m.contains(r#"local REX = "/usr/bin/rex""#), "{m}");
        assert!(
            m.contains(r#"local PATH = "/opt/homebrew/bin:/usr/bin""#),
            "{m}"
        );
        assert!(!m.contains("@DWM@") && !m.contains("@REX@") && !m.contains("@PATH@"));
    }

    #[test]
    fn require_line_detection() {
        assert!(has_require_line("rex.bind('a','b')\nrequire(\"dwm\")\n"));
        assert!(has_require_line("  require('dwm')"));
        assert!(!has_require_line("-- require(\"dwm\")"));
        assert!(!has_require_line("require(\"dwm_extra\")"));
    }

    #[test]
    fn add_require_line_appends_once_with_spacing() {
        assert_eq!(
            add_require_line(""),
            "-- dwm workspaces in the command palette (added by `dwm setup`)\nrequire(\"dwm\")\n"
        );
        let out = add_require_line("rex.bind(\"a\", \"b\")");
        assert!(out.starts_with("rex.bind(\"a\", \"b\")\n\n-- dwm"));
        assert!(has_require_line(&out));
    }
}
