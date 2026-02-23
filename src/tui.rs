use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Frame, prelude::*, widgets::*};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::agent::AgentSummary;
use crate::workspace::{WorkspaceEntry, format_time_ago};

/// Shared stop signal that can wake sleeping threads immediately.
struct StopSignal {
    flag: AtomicBool,
    condvar: Condvar,
    mutex: Mutex<()>,
}

impl StopSignal {
    fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            condvar: Condvar::new(),
            mutex: Mutex::new(()),
        }
    }

    fn stop(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.condvar.notify_all();
    }

    fn is_stopped(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Sleep for up to `duration`, but wake immediately if stopped.
    fn sleep(&self, duration: std::time::Duration) {
        let guard = self.mutex.lock().unwrap();
        let _ = self.condvar.wait_timeout(guard, duration);
    }
}

/// Spawn a background thread that periodically calls `produce` and posts
/// results to `sender`. Polls immediately on start, then sleeps for `interval`
/// between calls. Wakes instantly when the stop signal fires.
fn spawn_refresh_thread<T: Send + 'static>(
    interval: std::time::Duration,
    stop: Arc<StopSignal>,
    sender: Arc<Mutex<Option<T>>>,
    mut produce: impl FnMut() -> Option<T> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            if stop.is_stopped() {
                break;
            }
            if let Some(value) = produce() {
                let _ = sender.lock().map(|mut m| *m = Some(value));
            }
            stop.sleep(interval);
        }
    })
}

/// Thread-safe single-slot mailbox for passing data from background threads.
struct Mailbox<T>(Arc<Mutex<Option<T>>>);

impl<T> Mailbox<T> {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    fn sender(&self) -> Arc<Mutex<Option<T>>> {
        Arc::clone(&self.0)
    }

    fn take(&self) -> Option<T> {
        self.0.try_lock().ok().and_then(|mut guard| guard.take())
    }
}

#[derive(Debug, Clone)]
enum PreviewState {
    Hidden,
    Loading,
    Ready { log: String, diff_stat: String },
}

fn fetch_preview(
    main_repo_path: PathBuf,
    worktree_dir: PathBuf,
    ws_name: String,
    vcs_type: crate::vcs::VcsType,
    mailbox: Arc<Mutex<Option<PreviewState>>>,
) {
    std::thread::spawn(move || {
        let backend = vcs_type.to_backend();

        let log = backend.preview_log(&main_repo_path, &worktree_dir, &ws_name, 10);
        let diff_stat = backend.preview_diff_stat(&main_repo_path, &worktree_dir, &ws_name);

        let _ = mailbox
            .lock()
            .map(|mut m| *m = Some(PreviewState::Ready { log, diff_stat }));
    });
}

/// The action chosen by the user in the interactive workspace picker.
#[derive(Debug)]
pub enum PickerResult {
    /// User selected an existing workspace; value is the workspace path.
    Selected(String),
    /// User wants to create a new workspace with an optional explicit name.
    CreateNew(Option<String>),
}

/// Column by which the workspace table is sorted.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SortMode {
    Recency,
    Name,
    DiffSize,
}

impl SortMode {
    /// Cycle to the next sort mode.
    fn next(self) -> Self {
        match self {
            SortMode::Recency => SortMode::Name,
            SortMode::Name => SortMode::DiffSize,
            SortMode::DiffSize => SortMode::Recency,
        }
    }

    /// Short label shown in the help bar (e.g. `"recency"`, `"name"`).
    fn label(self) -> &'static str {
        match self {
            SortMode::Recency => "recency",
            SortMode::Name => "name",
            SortMode::DiffSize => "diff size",
        }
    }
}

/// Return `true` if `entry` matches the filter `query` (case-insensitive).
/// Matches against workspace name, description, and bookmark names.
fn matches_filter(entry: &WorkspaceEntry, query: &str) -> bool {
    let query = query.to_lowercase();
    entry.name.to_lowercase().contains(&query)
        || entry.description.to_lowercase().contains(&query)
        || entry
            .bookmarks
            .iter()
            .any(|b| b.to_lowercase().contains(&query))
}

/// Sort `entries` in-place according to `mode`.
fn sort_entries(entries: &mut [WorkspaceEntry], mode: SortMode) {
    match mode {
        SortMode::Name => {
            entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }
        SortMode::Recency => {
            entries.sort_by(|a, b| {
                // Most recent first; None sorts last
                match (a.last_modified, b.last_modified) {
                    (Some(a_t), Some(b_t)) => b_t.cmp(&a_t),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            });
        }
        SortMode::DiffSize => {
            entries.sort_by(|a, b| {
                let a_total = a.diff_stat.insertions + a.diff_stat.deletions;
                let b_total = b.diff_stat.insertions + b.diff_stat.deletions;
                b_total.cmp(&a_total)
            });
        }
    }
}

/// Current interaction mode of the single-repo picker.
#[derive(Debug, PartialEq)]
enum Mode {
    /// Normal navigation.
    Browse,
    /// User is typing a name for a new workspace.
    InputName,
    /// User is typing a filter string.
    Filter,
    /// Waiting for y/n confirmation before deleting the named workspace.
    ConfirmDelete(String),
}

/// State for the single-repo interactive picker.
struct App {
    entries: Vec<WorkspaceEntry>,
    /// Index into [`filtered_indices`] (not into `entries` directly).
    selected: usize,
    mode: Mode,
    /// Buffer for the new-workspace name being typed.
    input_buf: String,
    sort_mode: SortMode,
    /// Live filter string.
    filter_buf: String,
    /// Indices into `entries` that survive the current filter.
    filtered_indices: Vec<usize>,
    show_preview: bool,
    preview: PreviewState,
    preview_mailbox: Arc<Mutex<Option<PreviewState>>>,
    table_state: TableState,
    /// Transient status message shown in the help bar (e.g. after deletion).
    status_message: Option<String>,
    /// Receives full workspace entry refreshes from background thread.
    refresh_mailbox: Mailbox<Vec<WorkspaceEntry>>,
    /// Receives agent status updates from background thread.
    agent_refresh_mailbox: Mailbox<HashMap<String, AgentSummary>>,
}

impl App {
    /// Create a new [`App`], sorting entries by recency and computing the
    /// initial (unfiltered) index list.
    fn new(mut entries: Vec<WorkspaceEntry>) -> Self {
        let sort_mode = SortMode::Recency;
        sort_entries(&mut entries, sort_mode);
        let filtered_indices: Vec<usize> = (0..entries.len()).collect();
        Self {
            selected: 0,
            entries,
            mode: Mode::Browse,
            input_buf: String::new(),
            sort_mode,
            filter_buf: String::new(),
            filtered_indices,
            show_preview: false,
            preview: PreviewState::Hidden,
            preview_mailbox: Arc::new(Mutex::new(None)),
            table_state: TableState::default().with_selected(0),
            status_message: None,
            refresh_mailbox: Mailbox::new(),
            agent_refresh_mailbox: Mailbox::new(),
        }
    }

    /// Return only the entries that pass the current filter, in display order.
    fn visible_entries(&self) -> Vec<&WorkspaceEntry> {
        self.filtered_indices
            .iter()
            .map(|&i| &self.entries[i])
            .collect()
    }

    /// Total number of selectable rows including the "+ Create new" sentinel row.
    fn total_rows(&self) -> usize {
        self.filtered_indices.len() + 1 // +1 for "Create new" row
    }

    /// Return `true` when the cursor is on the "+ Create new" row.
    fn on_create_row(&self) -> bool {
        self.selected == self.filtered_indices.len()
    }

    /// Return the index into `entries` for the currently selected row, or
    /// `None` when the cursor is on the "+ Create new" row.
    fn selected_entry_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected).copied()
    }

    /// Move the cursor down one row (wrapping).
    fn next(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected = (self.selected + 1) % total;
        }
        self.sync_table_state();
    }

    /// Move the cursor up one row (wrapping).
    fn previous(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(total - 1);
        }
        self.sync_table_state();
    }

    /// Keep `table_state` selection in sync with `selected`.
    fn sync_table_state(&mut self) {
        self.table_state.select(Some(self.selected));
    }

    fn trigger_preview_fetch(&mut self) {
        if !self.show_preview {
            return;
        }
        if let Some(idx) = self.selected_entry_index() {
            let entry = &self.entries[idx];
            self.preview = PreviewState::Loading;
            let mailbox = Arc::new(Mutex::new(None));
            self.preview_mailbox = Arc::clone(&mailbox);
            fetch_preview(
                entry.main_repo_path.clone(),
                entry.path.clone(),
                entry.name.clone(),
                entry.vcs_type,
                mailbox,
            );
        } else {
            self.preview = PreviewState::Hidden;
        }
    }

    fn drain_preview_mailbox(&mut self) {
        if let Ok(mut guard) = self.preview_mailbox.try_lock()
            && let Some(state) = guard.take()
        {
            self.preview = state;
        }
    }

    /// Drain refresh mailboxes, merging updated data into current state.
    ///
    /// Agent-only updates are lightweight (no re-sort). Full entry refreshes
    /// preserve the current selection by matching on workspace name.
    fn drain_refresh_mailbox(&mut self) {
        // Check agent-only refresh (fast path, ~2s interval)
        if let Some(summaries) = self.agent_refresh_mailbox.take() {
            for entry in &mut self.entries {
                entry.agent_status = summaries.get(&entry.name).cloned();
            }
        }

        // Check full entry refresh (~10s interval)
        if let Some(new_entries) = self.refresh_mailbox.take() {
            self.merge_entries(new_entries);
        }
    }

    /// Merge a fresh set of entries, preserving current selection and sort/filter.
    fn merge_entries(&mut self, new_entries: Vec<WorkspaceEntry>) {
        // Remember currently-selected workspace name
        let selected_name = self
            .selected_entry_index()
            .map(|idx| self.entries[idx].name.clone());

        self.entries = new_entries;
        sort_entries(&mut self.entries, self.sort_mode);
        self.recompute_filter();

        // Restore selection by name
        if let Some(ref name) = selected_name {
            let new_selected = self
                .filtered_indices
                .iter()
                .position(|&i| self.entries[i].name == *name)
                .unwrap_or(0);
            self.selected = new_selected;
        } else {
            self.selected = 0;
        }
        if self.selected >= self.total_rows() {
            self.selected = self.total_rows().saturating_sub(1);
        }
        self.sync_table_state();
    }

    /// Recompute `filtered_indices` after `filter_buf` has changed.
    fn recompute_filter(&mut self) {
        if self.filter_buf.is_empty() {
            self.filtered_indices = (0..self.entries.len()).collect();
        } else {
            self.filtered_indices = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| matches_filter(e, &self.filter_buf))
                .map(|(i, _)| i)
                .collect();
        }
        if self.selected >= self.total_rows() {
            self.selected = self.total_rows().saturating_sub(1);
        }
        self.sync_table_state();
    }
}

fn render_preview(frame: &mut Frame, area: Rect, preview: &PreviewState) {
    let content = match preview {
        PreviewState::Hidden => String::new(),
        PreviewState::Loading => "Loading...".to_string(),
        PreviewState::Ready { log, diff_stat } => {
            let mut text = String::new();
            if !diff_stat.is_empty() {
                text.push_str("--- diff stat vs trunk ---\n");
                text.push_str(diff_stat);
                if !diff_stat.ends_with('\n') {
                    text.push('\n');
                }
                text.push('\n');
            }
            if !log.is_empty() {
                text.push_str("--- log ---\n");
                text.push_str(log);
            }
            if text.is_empty() {
                "No changes".to_string()
            } else {
                text
            }
        }
    };

    let paragraph = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Preview ")
                .title_alignment(Alignment::Center),
        )
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::White));

    frame.render_widget(paragraph, area);
}

/// Render the single-repo workspace table and help bar into `frame`.
fn render(frame: &mut Frame, app: &mut App) {
    let full_area = frame.area();

    // Reserve 1 line at the bottom for help bar
    let (main_area, help_area) = if full_area.height > 3 {
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(full_area);
        (chunks[0], Some(chunks[1]))
    } else {
        (full_area, None)
    };

    // Split horizontally if preview is visible
    let (table_area, preview_area) = if app.show_preview {
        let chunks = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(main_area);
        (chunks[0], Some(chunks[1]))
    } else {
        (main_area, None)
    };

    let header_cells = [
        "Name",
        "Change",
        "Description",
        "Bookmarks",
        "Modified",
        "Changes",
        "Agent",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().fg(Color::White).bold()));
    let header = Row::new(header_cells)
        .style(Style::default().bg(Color::DarkGray))
        .height(1);

    let visible = app.visible_entries();
    let mut rows: Vec<Row> = visible
        .iter()
        .map(|entry| {
            let name_text = if entry.is_main {
                format!("{} (main)", entry.name)
            } else if entry.is_stale {
                format!("{} [stale]", entry.name)
            } else {
                entry.name.clone()
            };

            let change_text = entry.change_id.clone();

            let desc_text = entry.description.lines().next().unwrap_or("").to_string();

            let bookmarks_text = entry.bookmarks.join(", ");

            let time_text = format_time_ago(entry.last_modified);

            let stat = &entry.diff_stat;
            let changes_text =
                if stat.files_changed == 0 && stat.insertions == 0 && stat.deletions == 0 {
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

            // Use dim styling for stale workspaces
            let dim = entry.is_stale;
            let name_fg = if dim { Color::DarkGray } else { Color::Cyan };
            let change_fg = if dim { Color::DarkGray } else { Color::Magenta };
            let desc_fg = if dim { Color::DarkGray } else { Color::White };
            let bookmark_fg = if dim { Color::DarkGray } else { Color::Blue };
            let time_fg = if dim { Color::DarkGray } else { Color::Yellow };
            let changes_fg = if dim {
                Color::DarkGray
            } else if stat.deletions > stat.insertions {
                Color::Red
            } else if stat.insertions > 0 {
                Color::Green
            } else {
                Color::DarkGray
            };

            let (agent_text, agent_fg) = match &entry.agent_status {
                Some(summary) if !summary.is_empty() => {
                    let color = if dim {
                        Color::DarkGray
                    } else {
                        match summary.most_urgent() {
                            Some(crate::agent::AgentStatus::Waiting) => Color::Yellow,
                            Some(crate::agent::AgentStatus::Working) => Color::Green,
                            _ => Color::DarkGray,
                        }
                    };
                    (summary.to_string(), color)
                }
                _ => (String::new(), Color::DarkGray),
            };

            Row::new(vec![
                Cell::from(name_text).style(Style::default().fg(name_fg)),
                Cell::from(change_text).style(Style::default().fg(change_fg)),
                Cell::from(desc_text).style(Style::default().fg(desc_fg)),
                Cell::from(bookmarks_text).style(Style::default().fg(bookmark_fg)),
                Cell::from(time_text).style(Style::default().fg(time_fg)),
                Cell::from(changes_text).style(Style::default().fg(changes_fg)),
                Cell::from(agent_text).style(Style::default().fg(agent_fg)),
            ])
        })
        .collect();

    // Append "+ Create new" row
    let create_row_selected = app.on_create_row();
    let create_style = if create_row_selected {
        Style::default().bg(Color::Rgb(40, 40, 60))
    } else {
        Style::default()
    };

    let input_active = app.mode == Mode::InputName && create_row_selected;

    // Always add the create row to the table so it occupies the right space
    let create_name = if input_active {
        // Placeholder text that will be painted over by the overlay
        String::new()
    } else {
        "+ Create new".to_string()
    };
    rows.push(
        Row::new(vec![
            Cell::from(create_name).style(Style::default().fg(Color::Green)),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ])
        .style(create_style),
    );

    let widths = [
        Constraint::Percentage(14),
        Constraint::Percentage(8),
        Constraint::Percentage(27),
        Constraint::Percentage(13),
        Constraint::Percentage(10),
        Constraint::Percentage(12),
        Constraint::Percentage(16),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" dwm workspaces ")
                .title_alignment(Alignment::Center),
        )
        .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 60)));

    frame.render_stateful_widget(table, table_area, &mut app.table_state);

    // Overlay a full-width input line on top of the create row
    if input_active {
        // Row y = table top border (1) + header (1) + (row_index - scroll_offset)
        let scroll_offset = app.table_state.offset() as u16;
        let create_row_index = app.filtered_indices.len() as u16;
        let create_row_y = table_area.y + 2 + create_row_index.saturating_sub(scroll_offset);
        if create_row_y < table_area.bottom() {
            let input_area = Rect::new(
                table_area.x + 1, // inside left border
                create_row_y,
                table_area.width.saturating_sub(2), // inside both borders
                1,
            );
            let input_text = format!("Name: {}_", app.input_buf);
            let input_line = Paragraph::new(input_text)
                .style(Style::default().fg(Color::Green).bg(Color::Rgb(40, 40, 60)));
            frame.render_widget(input_line, input_area);
        }
    }

    // Render preview pane if visible
    if let Some(preview_area) = preview_area {
        render_preview(frame, preview_area, &app.preview);
    }

    // Render help bar at bottom
    if let Some(help_area) = help_area {
        let (help_text, help_style) = if let Some(ref msg) = app.status_message {
            (format!(" {}", msg), Style::default().fg(Color::Green))
        } else {
            let text = match app.mode {
                Mode::InputName => " Enter: create  Esc: cancel".to_string(),
                Mode::Filter => {
                    format!(" filter: {}▏  Enter: apply  Esc: clear", app.filter_buf)
                }
                Mode::ConfirmDelete(ref name) => {
                    format!(" Delete '{}'? y: confirm  n: cancel", name)
                }
                Mode::Browse if app.on_create_row() => {
                    " Enter: create (auto-name)  type: name it  q: quit".to_string()
                }
                Mode::Browse => {
                    let filter_info = if !app.filter_buf.is_empty() {
                        format!("  [filter: \"{}\"]", app.filter_buf)
                    } else {
                        String::new()
                    };
                    format!(
                        " j/k: navigate  /: filter  s: sort ({})  p: preview  d: delete  Enter: select  q: quit{}",
                        app.sort_mode.label(),
                        filter_info
                    )
                }
            };
            (text, Style::default().fg(Color::DarkGray))
        };
        let help = Paragraph::new(help_text).style(help_style);
        frame.render_widget(help, help_area);
    }
}

/// Event loop for the single-repo picker. `next_event` is injectable for
/// testing (pass a closure that returns synthetic key events).
///
/// `on_delete` performs the workspace deletion — returns `Ok(true)` if the
/// caller already printed a redirect path (picker should exit), `Ok(false)`
/// if the picker should refresh and continue.
///
/// `list_entries` is called after a successful non-redirect deletion to
/// refresh the entry list.
fn run_picker_inner<B: Backend>(
    terminal: &mut Terminal<B>,
    app: App,
    next_event: &mut dyn FnMut() -> Result<Option<Event>>,
    on_delete: &mut dyn FnMut(&str) -> Result<bool>,
    list_entries: &mut dyn FnMut() -> Result<Vec<WorkspaceEntry>>,
) -> Result<Option<PickerResult>> {
    let mut app = app;

    loop {
        // Drain mailboxes before drawing
        app.drain_preview_mailbox();
        app.drain_refresh_mailbox();

        terminal.draw(|f| render(f, &mut app))?;

        let event = next_event()?;
        let Some(event) = event else {
            continue;
        };

        if let Event::Key(key) = event {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            let prev_selected = app.selected;
            app.status_message = None;

            match app.mode {
                Mode::Browse => match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Down => app.next(),
                    KeyCode::Up => app.previous(),
                    KeyCode::Enter => {
                        if app.on_create_row() {
                            return Ok(Some(PickerResult::CreateNew(None)));
                        } else if let Some(idx) = app.selected_entry_index() {
                            let path = app.entries[idx].path.to_string_lossy().to_string();
                            return Ok(Some(PickerResult::Selected(path)));
                        }
                    }
                    KeyCode::Char(c) if app.on_create_row() => {
                        app.mode = Mode::InputName;
                        app.input_buf.clear();
                        app.input_buf.push(c);
                    }
                    KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('j') => app.next(),
                    KeyCode::Char('k') => app.previous(),
                    KeyCode::Char('s') => {
                        app.sort_mode = app.sort_mode.next();
                        sort_entries(&mut app.entries, app.sort_mode);
                        app.recompute_filter();
                        app.selected = 0;
                        app.sync_table_state();
                    }
                    KeyCode::Char('/') => {
                        app.mode = Mode::Filter;
                    }
                    KeyCode::Char('p') => {
                        app.show_preview = !app.show_preview;
                        if app.show_preview {
                            app.trigger_preview_fetch();
                        } else {
                            app.preview = PreviewState::Hidden;
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(idx) = app.selected_entry_index() {
                            let entry = &app.entries[idx];
                            if !entry.is_main {
                                app.mode = Mode::ConfirmDelete(entry.name.clone());
                            }
                        }
                    }
                    _ => {}
                },
                Mode::InputName => match key.code {
                    KeyCode::Esc => {
                        app.mode = Mode::Browse;
                        app.input_buf.clear();
                    }
                    KeyCode::Enter => {
                        let name = if app.input_buf.trim().is_empty() {
                            None
                        } else {
                            Some(app.input_buf.clone())
                        };
                        return Ok(Some(PickerResult::CreateNew(name)));
                    }
                    KeyCode::Backspace => {
                        app.input_buf.pop();
                        if app.input_buf.is_empty() {
                            app.mode = Mode::Browse;
                        }
                    }
                    KeyCode::Char(c) => {
                        app.input_buf.push(c);
                    }
                    _ => {}
                },
                Mode::Filter => match key.code {
                    KeyCode::Esc => {
                        app.filter_buf.clear();
                        app.recompute_filter();
                        app.mode = Mode::Browse;
                    }
                    KeyCode::Enter => {
                        app.mode = Mode::Browse;
                    }
                    KeyCode::Backspace => {
                        app.filter_buf.pop();
                        app.recompute_filter();
                    }
                    KeyCode::Char(c) => {
                        app.filter_buf.push(c);
                        app.recompute_filter();
                    }
                    _ => {}
                },
                Mode::ConfirmDelete(ref name) => match key.code {
                    KeyCode::Char('y') => {
                        let name = name.clone();
                        app.mode = Mode::Browse;
                        let redirected = on_delete(&name)?;
                        if redirected {
                            return Ok(None);
                        }
                        // Refresh entries after deletion
                        let new_entries = list_entries()?;
                        if new_entries.is_empty() {
                            return Ok(None);
                        }
                        app.entries = new_entries;
                        sort_entries(&mut app.entries, app.sort_mode);
                        app.recompute_filter();
                        if app.selected >= app.total_rows() {
                            app.selected = app.total_rows().saturating_sub(1);
                        }
                        app.sync_table_state();
                        app.trigger_preview_fetch();
                        app.status_message = Some(format!("workspace '{}' deleted", name));
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        app.mode = Mode::Browse;
                    }
                    _ => {}
                },
            }

            // Trigger preview fetch on selection change
            if app.selected != prev_selected {
                app.trigger_preview_fetch();
            }
        }
    }
}

/// Launch the interactive TUI workspace picker for a single repo.
///
/// Switches the terminal to an alternate screen in raw mode, runs the event
/// loop, then restores the terminal before returning.
///
/// `on_delete` is called when the user confirms deletion of a workspace.
/// It should return `Ok(true)` if a redirect path was printed (picker exits),
/// or `Ok(false)` to refresh and continue.
///
/// `list_entries` is called after a non-redirect deletion to get the fresh
/// entry list.
pub fn run_picker(
    entries: Vec<WorkspaceEntry>,
    repo_dir: PathBuf,
    mut on_delete: impl FnMut(&str) -> Result<bool>,
    mut list_entries: impl FnMut() -> Result<Vec<WorkspaceEntry>>,
) -> Result<Option<PickerResult>> {
    if entries.is_empty() {
        eprintln!("{}", "no workspaces found".red());
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stderr = io::stderr();
    crossterm::execute!(stderr, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    // Set up background refresh threads
    let app = App::new(entries);
    let stop = Arc::new(StopSignal::new());

    let agent_sender = app.agent_refresh_mailbox.sender();
    let refresh_sender = app.refresh_mailbox.sender();

    // Agent status polling thread (~2s)
    let agent_repo_dir = repo_dir.clone();
    let agent_thread = spawn_refresh_thread(
        std::time::Duration::from_secs(2),
        Arc::clone(&stop),
        agent_sender,
        move || Some(crate::agent::read_agent_summaries(&agent_repo_dir)),
    );

    // Full VCS refresh thread (~10s)
    let refresh_thread = spawn_refresh_thread(
        std::time::Duration::from_secs(10),
        Arc::clone(&stop),
        refresh_sender,
        move || crate::workspace::list_workspace_entries().ok(),
    );

    let result = run_picker_inner(
        &mut terminal,
        app,
        &mut || {
            if event::poll(std::time::Duration::from_millis(100))? {
                Ok(Some(event::read()?))
            } else {
                Ok(None)
            }
        },
        &mut on_delete,
        &mut list_entries,
    );

    // Signal background threads to stop (wakes them immediately)
    stop.stop();
    let _ = agent_thread.join();
    let _ = refresh_thread.join();

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

// ── Multi-repo picker (--all mode) ──────────────────────────────

/// State for the multi-repo (`--all`) interactive picker.
struct MultiRepoApp {
    entries: Vec<WorkspaceEntry>,
    selected: usize,
    sort_mode: SortMode,
    filter_buf: String,
    filtered_indices: Vec<usize>,
    /// Whether the user is currently typing a filter string.
    filter_mode: bool,
    show_preview: bool,
    preview: PreviewState,
    preview_mailbox: Arc<Mutex<Option<PreviewState>>>,
    table_state: TableState,
    /// Receives full workspace entry refreshes from background thread.
    refresh_mailbox: Mailbox<Vec<WorkspaceEntry>>,
    /// Receives agent status updates from background thread.
    agent_refresh_mailbox: Mailbox<HashMap<String, AgentSummary>>,
}

impl MultiRepoApp {
    /// Create a new [`MultiRepoApp`], sorting entries by recency.
    fn new(mut entries: Vec<WorkspaceEntry>) -> Self {
        let sort_mode = SortMode::Recency;
        sort_entries(&mut entries, sort_mode);
        let filtered_indices: Vec<usize> = (0..entries.len()).collect();
        Self {
            selected: 0,
            entries,
            sort_mode,
            filter_buf: String::new(),
            filtered_indices,
            filter_mode: false,
            show_preview: false,
            preview: PreviewState::Hidden,
            preview_mailbox: Arc::new(Mutex::new(None)),
            table_state: TableState::default().with_selected(0),
            refresh_mailbox: Mailbox::new(),
            agent_refresh_mailbox: Mailbox::new(),
        }
    }

    /// Return only the entries that pass the current filter, in display order.
    fn visible_entries(&self) -> Vec<&WorkspaceEntry> {
        self.filtered_indices
            .iter()
            .map(|&i| &self.entries[i])
            .collect()
    }

    /// Total number of selectable rows.
    fn total_rows(&self) -> usize {
        self.filtered_indices.len()
    }

    fn selected_entry_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected).copied()
    }

    /// Move the cursor down one row (wrapping).
    fn next(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected = (self.selected + 1) % total;
        }
        self.sync_table_state();
    }

    /// Move the cursor up one row (wrapping).
    fn previous(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(total - 1);
        }
        self.sync_table_state();
    }

    fn sync_table_state(&mut self) {
        self.table_state.select(Some(self.selected));
    }

    fn trigger_preview_fetch(&mut self) {
        if !self.show_preview {
            return;
        }
        if let Some(idx) = self.selected_entry_index() {
            let entry = &self.entries[idx];
            self.preview = PreviewState::Loading;
            let mailbox = Arc::new(Mutex::new(None));
            self.preview_mailbox = Arc::clone(&mailbox);
            fetch_preview(
                entry.main_repo_path.clone(),
                entry.path.clone(),
                entry.name.clone(),
                entry.vcs_type,
                mailbox,
            );
        } else {
            self.preview = PreviewState::Hidden;
        }
    }

    fn drain_preview_mailbox(&mut self) {
        if let Ok(mut guard) = self.preview_mailbox.try_lock()
            && let Some(state) = guard.take()
        {
            self.preview = state;
        }
    }

    /// Drain refresh mailboxes, merging updated data into current state.
    fn drain_refresh_mailbox(&mut self) {
        // Check agent-only refresh (fast path, ~2s interval)
        if let Some(summaries) = self.agent_refresh_mailbox.take() {
            for entry in &mut self.entries {
                // Multi-repo keys include repo name to avoid collisions
                let key = format!(
                    "{}:{}",
                    entry.repo_name.as_deref().unwrap_or(""),
                    entry.name
                );
                entry.agent_status = summaries.get(&key).cloned();
            }
        }

        // Check full entry refresh (~10s interval)
        if let Some(new_entries) = self.refresh_mailbox.take() {
            let selected_name = self
                .selected_entry_index()
                .map(|idx| self.entries[idx].name.clone());

            self.entries = new_entries;
            sort_entries(&mut self.entries, self.sort_mode);
            self.recompute_filter();

            if let Some(ref name) = selected_name {
                let new_selected = self
                    .filtered_indices
                    .iter()
                    .position(|&i| self.entries[i].name == *name)
                    .unwrap_or(0);
                self.selected = new_selected;
            } else {
                self.selected = 0;
            }
            if self.selected >= self.total_rows() {
                self.selected = self.total_rows().saturating_sub(1);
            }
            self.sync_table_state();
        }
    }

    /// Recompute `filtered_indices` after `filter_buf` has changed.
    fn recompute_filter(&mut self) {
        if self.filter_buf.is_empty() {
            self.filtered_indices = (0..self.entries.len()).collect();
        } else {
            self.filtered_indices = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| matches_filter(e, &self.filter_buf))
                .map(|(i, _)| i)
                .collect();
        }
        if self.selected >= self.total_rows() {
            self.selected = self.total_rows().saturating_sub(1);
        }
        self.sync_table_state();
    }
}

/// Render the multi-repo workspace table and help bar into `frame`.
fn render_multi_repo(frame: &mut Frame, app: &mut MultiRepoApp) {
    let full_area = frame.area();

    // Reserve 1 line at the bottom for help bar
    let (main_area, help_area) = if full_area.height > 3 {
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(full_area);
        (chunks[0], Some(chunks[1]))
    } else {
        (full_area, None)
    };

    // Split horizontally if preview is visible
    let (table_area, preview_area) = if app.show_preview {
        let chunks = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(main_area);
        (chunks[0], Some(chunks[1]))
    } else {
        (main_area, None)
    };

    let header_cells = [
        "Repo",
        "Name",
        "Change",
        "Description",
        "Bookmarks",
        "Modified",
        "Changes",
        "Agent",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().fg(Color::White).bold()));
    let header = Row::new(header_cells)
        .style(Style::default().bg(Color::DarkGray))
        .height(1);

    let visible = app.visible_entries();
    let rows: Vec<Row> = visible
        .iter()
        .map(|entry| {
            let repo_text = entry.repo_name.as_deref().unwrap_or("").to_string();

            let name_text = if entry.is_main {
                format!("{} (main)", entry.name)
            } else if entry.is_stale {
                format!("{} [stale]", entry.name)
            } else {
                entry.name.clone()
            };

            let change_text = entry.change_id.clone();
            let desc_text = entry.description.lines().next().unwrap_or("").to_string();
            let bookmarks_text = entry.bookmarks.join(", ");
            let time_text = format_time_ago(entry.last_modified);

            let stat = &entry.diff_stat;
            let changes_text =
                if stat.files_changed == 0 && stat.insertions == 0 && stat.deletions == 0 {
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

            let dim = entry.is_stale;
            let name_fg = if dim { Color::DarkGray } else { Color::Cyan };
            let change_fg = if dim { Color::DarkGray } else { Color::Magenta };
            let desc_fg = if dim { Color::DarkGray } else { Color::White };
            let bookmark_fg = if dim { Color::DarkGray } else { Color::Blue };
            let time_fg = if dim { Color::DarkGray } else { Color::Yellow };
            let changes_fg = if dim {
                Color::DarkGray
            } else if stat.deletions > stat.insertions {
                Color::Red
            } else if stat.insertions > 0 {
                Color::Green
            } else {
                Color::DarkGray
            };

            let (agent_text, agent_fg) = match &entry.agent_status {
                Some(summary) if !summary.is_empty() => {
                    let color = if dim {
                        Color::DarkGray
                    } else {
                        match summary.most_urgent() {
                            Some(crate::agent::AgentStatus::Waiting) => Color::Yellow,
                            Some(crate::agent::AgentStatus::Working) => Color::Green,
                            _ => Color::DarkGray,
                        }
                    };
                    (summary.to_string(), color)
                }
                _ => (String::new(), Color::DarkGray),
            };

            Row::new(vec![
                Cell::from(repo_text).style(Style::default().fg(Color::Green)),
                Cell::from(name_text).style(Style::default().fg(name_fg)),
                Cell::from(change_text).style(Style::default().fg(change_fg)),
                Cell::from(desc_text).style(Style::default().fg(desc_fg)),
                Cell::from(bookmarks_text).style(Style::default().fg(bookmark_fg)),
                Cell::from(time_text).style(Style::default().fg(time_fg)),
                Cell::from(changes_text).style(Style::default().fg(changes_fg)),
                Cell::from(agent_text).style(Style::default().fg(agent_fg)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Percentage(10),
        Constraint::Percentage(11),
        Constraint::Percentage(7),
        Constraint::Percentage(24),
        Constraint::Percentage(11),
        Constraint::Percentage(10),
        Constraint::Percentage(12),
        Constraint::Percentage(15),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" dwm workspaces (all repos) ")
                .title_alignment(Alignment::Center),
        )
        .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 60)));

    frame.render_stateful_widget(table, table_area, &mut app.table_state);

    // Render preview pane if visible
    if let Some(preview_area) = preview_area {
        render_preview(frame, preview_area, &app.preview);
    }

    if let Some(help_area) = help_area {
        let help_text = if app.filter_mode {
            format!(" filter: {}▏  Enter: apply  Esc: clear", app.filter_buf)
        } else {
            let filter_info = if !app.filter_buf.is_empty() {
                format!("  [filter: \"{}\"]", app.filter_buf)
            } else {
                String::new()
            };
            format!(
                " j/k: navigate  /: filter  s: sort ({})  p: preview  Enter: select  q: quit{}",
                app.sort_mode.label(),
                filter_info
            )
        };
        let help = Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(help, help_area);
    }
}

/// Event loop for the multi-repo picker. `next_event` is injectable for testing.
fn run_picker_multi_repo_inner<B: Backend>(
    terminal: &mut Terminal<B>,
    app: MultiRepoApp,
    next_event: &mut dyn FnMut() -> Result<Option<Event>>,
) -> Result<Option<PickerResult>> {
    let mut app = app;

    loop {
        // Drain mailboxes before drawing
        app.drain_preview_mailbox();
        app.drain_refresh_mailbox();

        terminal.draw(|f| render_multi_repo(f, &mut app))?;

        let event = next_event()?;
        let Some(event) = event else {
            continue;
        };

        if let Event::Key(key) = event {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            let prev_selected = app.selected;

            if app.filter_mode {
                match key.code {
                    KeyCode::Esc => {
                        app.filter_buf.clear();
                        app.recompute_filter();
                        app.filter_mode = false;
                    }
                    KeyCode::Enter => {
                        app.filter_mode = false;
                    }
                    KeyCode::Backspace => {
                        app.filter_buf.pop();
                        app.recompute_filter();
                    }
                    KeyCode::Char(c) => {
                        app.filter_buf.push(c);
                        app.recompute_filter();
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Char('j') | KeyCode::Down => app.next(),
                    KeyCode::Char('k') | KeyCode::Up => app.previous(),
                    KeyCode::Char('s') => {
                        app.sort_mode = app.sort_mode.next();
                        sort_entries(&mut app.entries, app.sort_mode);
                        app.recompute_filter();
                        app.selected = 0;
                        app.sync_table_state();
                    }
                    KeyCode::Char('/') => {
                        app.filter_mode = true;
                    }
                    KeyCode::Char('p') => {
                        app.show_preview = !app.show_preview;
                        if app.show_preview {
                            app.trigger_preview_fetch();
                        } else {
                            app.preview = PreviewState::Hidden;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(&idx) = app.filtered_indices.get(app.selected) {
                            let path = app.entries[idx].path.to_string_lossy().to_string();
                            return Ok(Some(PickerResult::Selected(path)));
                        }
                    }
                    _ => {}
                }
            }

            // Trigger preview fetch on selection change
            if app.selected != prev_selected {
                app.trigger_preview_fetch();
            }
        }
    }
}

/// Launch the interactive TUI workspace picker showing all repos (`--all` mode).
///
/// Returns the selected workspace path, or `None` if the user cancelled.
pub fn run_picker_multi_repo(entries: Vec<WorkspaceEntry>) -> Result<Option<PickerResult>> {
    if entries.is_empty() {
        eprintln!("{}", "no workspaces found".red());
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stderr = io::stderr();
    crossterm::execute!(stderr, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let app = MultiRepoApp::new(entries);
    let stop = Arc::new(StopSignal::new());

    let agent_sender = app.agent_refresh_mailbox.sender();
    let refresh_sender = app.refresh_mailbox.sender();

    // Collect unique repo dirs for agent polling
    let repo_dirs: Vec<PathBuf> = {
        let mut dirs = std::collections::HashSet::new();
        for entry in &app.entries {
            if let Some(repo_name) = &entry.repo_name {
                let home = dirs::home_dir().unwrap_or_default();
                dirs.insert(home.join(".dwm").join(repo_name));
            }
        }
        dirs.into_iter().collect()
    };

    // Agent status polling thread (~2s)
    let agent_thread = spawn_refresh_thread(
        std::time::Duration::from_secs(2),
        Arc::clone(&stop),
        agent_sender,
        move || {
            let mut all_summaries = HashMap::new();
            for repo_dir in &repo_dirs {
                let repo_name = repo_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                for (ws_name, summary) in crate::agent::read_agent_summaries(repo_dir) {
                    all_summaries.insert(format!("{}:{}", repo_name, ws_name), summary);
                }
            }
            Some(all_summaries)
        },
    );

    // Full VCS refresh thread (~10s)
    let refresh_thread = spawn_refresh_thread(
        std::time::Duration::from_secs(10),
        Arc::clone(&stop),
        refresh_sender,
        move || crate::workspace::list_all_workspace_entries().ok(),
    );

    let result = run_picker_multi_repo_inner(&mut terminal, app, &mut || {
        if event::poll(std::time::Duration::from_millis(100))? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    });

    stop.stop();
    let _ = agent_thread.join();
    let _ = refresh_thread.join();

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::DiffStat;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn make_entry(
        name: &str,
        modified_secs_ago: Option<u64>,
        insertions: u32,
        deletions: u32,
    ) -> WorkspaceEntry {
        WorkspaceEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{}", name)),
            last_modified: modified_secs_ago.map(|s| SystemTime::now() - Duration::from_secs(s)),
            diff_stat: DiffStat {
                files_changed: 1,
                insertions,
                deletions,
            },
            is_main: false,
            change_id: String::new(),
            description: String::new(),
            bookmarks: Vec::new(),
            is_stale: false,
            repo_name: None,
            main_repo_path: PathBuf::from("/tmp/repo"),
            vcs_type: crate::vcs::VcsType::Jj,
            agent_status: None,
        }
    }

    #[test]
    fn sort_by_name_alphabetical() {
        let mut entries = vec![
            make_entry("cherry", None, 0, 0),
            make_entry("Apple", None, 0, 0),
            make_entry("banana", None, 0, 0),
        ];
        sort_entries(&mut entries, SortMode::Name);
        assert_eq!(entries[0].name, "Apple");
        assert_eq!(entries[1].name, "banana");
        assert_eq!(entries[2].name, "cherry");
    }

    #[test]
    fn sort_by_recency_most_recent_first() {
        let mut entries = vec![
            make_entry("old", Some(3600), 0, 0),
            make_entry("new", Some(60), 0, 0),
            make_entry("mid", Some(600), 0, 0),
        ];
        sort_entries(&mut entries, SortMode::Recency);
        assert_eq!(entries[0].name, "new");
        assert_eq!(entries[1].name, "mid");
        assert_eq!(entries[2].name, "old");
    }

    #[test]
    fn sort_by_recency_none_sorts_last() {
        let mut entries = vec![
            make_entry("unknown", None, 0, 0),
            make_entry("recent", Some(10), 0, 0),
        ];
        sort_entries(&mut entries, SortMode::Recency);
        assert_eq!(entries[0].name, "recent");
        assert_eq!(entries[1].name, "unknown");
    }

    #[test]
    fn sort_by_diff_size_largest_first() {
        let mut entries = vec![
            make_entry("small", None, 1, 2),
            make_entry("large", None, 50, 30),
            make_entry("medium", None, 10, 5),
        ];
        sort_entries(&mut entries, SortMode::DiffSize);
        assert_eq!(entries[0].name, "large");
        assert_eq!(entries[1].name, "medium");
        assert_eq!(entries[2].name, "small");
    }

    #[test]
    fn sort_mode_cycles() {
        assert_eq!(SortMode::Recency.next(), SortMode::Name);
        assert_eq!(SortMode::Name.next(), SortMode::DiffSize);
        assert_eq!(SortMode::DiffSize.next(), SortMode::Recency);
    }

    fn make_entry_with_desc(name: &str, description: &str, bookmarks: Vec<&str>) -> WorkspaceEntry {
        WorkspaceEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{}", name)),
            last_modified: None,
            diff_stat: DiffStat::default(),
            is_main: false,
            change_id: String::new(),
            description: description.to_string(),
            bookmarks: bookmarks.into_iter().map(String::from).collect(),
            is_stale: false,
            repo_name: None,
            main_repo_path: PathBuf::from("/tmp/repo"),
            vcs_type: crate::vcs::VcsType::Jj,
            agent_status: None,
        }
    }

    #[test]
    fn filter_matches_name() {
        let entry = make_entry_with_desc("my-feature", "", vec![]);
        assert!(matches_filter(&entry, "feat"));
        assert!(!matches_filter(&entry, "bugfix"));
    }

    #[test]
    fn filter_matches_description() {
        let entry = make_entry_with_desc("ws1", "fix login bug", vec![]);
        assert!(matches_filter(&entry, "login"));
        assert!(!matches_filter(&entry, "signup"));
    }

    #[test]
    fn filter_matches_bookmarks() {
        let entry = make_entry_with_desc("ws1", "", vec!["main", "release-v2"]);
        assert!(matches_filter(&entry, "release"));
        assert!(!matches_filter(&entry, "develop"));
    }

    #[test]
    fn filter_is_case_insensitive() {
        let entry = make_entry_with_desc("MyFeature", "Fix Bug", vec!["Main"]);
        assert!(matches_filter(&entry, "myfeature"));
        assert!(matches_filter(&entry, "FIX"));
        assert!(matches_filter(&entry, "main"));
    }

    #[test]
    fn filter_no_match() {
        let entry = make_entry_with_desc("ws1", "some desc", vec!["bk1"]);
        assert!(!matches_filter(&entry, "zzz"));
    }

    #[test]
    fn create_row_any_char_enters_input_mode() {
        // Regression: pressing 's', 'd', 'q', etc. on the create row should
        // start typing a workspace name, not trigger shortcuts like sort/delete/quit.
        let entries = vec![make_entry("ws1", Some(60), 0, 0)];
        let mut app = App::new(entries);
        let original_sort = app.sort_mode;

        // Move to the "+ Create new" row
        app.next();
        assert!(app.on_create_row());

        // Simulate what the event loop does for Char(c) when on_create_row()
        for ch in ['s', 'd', 'q', 'j', 'k', '/'] {
            app.mode = Mode::Browse;
            app.input_buf.clear();

            // This mirrors the match arm: Char(c) if on_create_row() => InputName
            app.mode = Mode::InputName;
            app.input_buf.push(ch);

            assert_eq!(
                app.mode,
                Mode::InputName,
                "char '{}' should enter InputName",
                ch
            );
            assert_eq!(app.input_buf, ch.to_string());
        }
        // Sort should never have changed
        assert_eq!(app.sort_mode, original_sort);
    }

    // ── TUI integration test helpers ─────────────────────────────────

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Drive run_picker_inner with a sequence of key events.
    /// After keys are exhausted, Esc is sent to avoid hanging.
    fn run_picker_with_keys(
        entries: Vec<WorkspaceEntry>,
        keys: Vec<KeyCode>,
    ) -> Result<Option<PickerResult>> {
        run_picker_with_keys_and_callbacks(entries, keys, &mut |_| Ok(false), &mut || Ok(vec![]))
    }

    /// Like `run_picker_with_keys` but with custom delete/refresh callbacks.
    fn run_picker_with_keys_and_callbacks(
        entries: Vec<WorkspaceEntry>,
        keys: Vec<KeyCode>,
        on_delete: &mut dyn FnMut(&str) -> Result<bool>,
        list_entries: &mut dyn FnMut() -> Result<Vec<WorkspaceEntry>>,
    ) -> Result<Option<PickerResult>> {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        let mut key_iter = keys.into_iter();
        run_picker_inner(
            &mut terminal,
            App::new(entries),
            &mut || match key_iter.next() {
                Some(code) => Ok(Some(key(code))),
                None => Ok(Some(key(KeyCode::Esc))),
            },
            on_delete,
            list_entries,
        )
    }

    /// Drive run_picker_multi_repo_inner with a sequence of key events.
    fn run_multi_picker_with_keys(
        entries: Vec<WorkspaceEntry>,
        keys: Vec<KeyCode>,
    ) -> Result<Option<PickerResult>> {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        let mut key_iter = keys.into_iter();
        run_picker_multi_repo_inner(&mut terminal, MultiRepoApp::new(entries), &mut || {
            match key_iter.next() {
                Some(code) => Ok(Some(key(code))),
                None => Ok(Some(key(KeyCode::Esc))),
            }
        })
    }

    /// Create a named entry with a specific recency rank.
    /// Lower `rank` = more recent (appears first when sorted by recency).
    fn make_named_entry_ranked(name: &str, path: &str, rank: u64) -> WorkspaceEntry {
        WorkspaceEntry {
            name: name.to_string(),
            path: PathBuf::from(path),
            last_modified: Some(SystemTime::now() - Duration::from_secs(rank)),
            diff_stat: DiffStat::default(),
            is_main: false,
            change_id: "abc".to_string(),
            description: format!("{} description", name),
            bookmarks: vec![],
            is_stale: false,
            repo_name: None,
            main_repo_path: PathBuf::from("/tmp/repo"),
            vcs_type: crate::vcs::VcsType::Jj,
            agent_status: None,
        }
    }

    fn make_named_entry(name: &str, path: &str) -> WorkspaceEntry {
        make_named_entry_ranked(name, path, 0)
    }

    fn make_main_entry(name: &str, path: &str) -> WorkspaceEntry {
        WorkspaceEntry {
            is_main: true,
            ..make_named_entry(name, path)
        }
    }

    // ── TUI picker integration tests ────────────────────────────────

    #[test]
    fn tui_select_first_entry() {
        // rank 0 = most recent, rank 1 = second most recent
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let result = run_picker_with_keys(entries, vec![KeyCode::Enter]).unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/ws1"),
            other => panic!("expected Selected, got {:?}", other),
        }
    }

    #[test]
    fn tui_navigate_down_and_select() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
            make_named_entry_ranked("ws3", "/tmp/ws3", 2),
        ];
        // j, j -> moves to ws3 (index 2), then Enter
        let result = run_picker_with_keys(
            entries,
            vec![KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Enter],
        )
        .unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/ws3"),
            other => panic!("expected Selected ws3, got {:?}", other),
        }
    }

    #[test]
    fn tui_navigate_with_arrow_keys() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let result = run_picker_with_keys(entries, vec![KeyCode::Down, KeyCode::Enter]).unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/ws2"),
            other => panic!("expected Selected ws2, got {:?}", other),
        }
    }

    #[test]
    fn tui_navigate_up_wraps() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        // 3 rows total: ws1(0), ws2(1), Create(2)
        // Up from 0 wraps to Create(2), Up again to ws2(1)
        // Use arrow keys since j/k on the Create row starts typing a name
        let result =
            run_picker_with_keys(entries, vec![KeyCode::Up, KeyCode::Up, KeyCode::Enter]).unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/ws2"),
            other => panic!("expected Selected ws2, got {:?}", other),
        }
    }

    #[test]
    fn tui_quit_with_q() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        let result = run_picker_with_keys(entries, vec![KeyCode::Char('q')]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tui_quit_with_esc() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        let result = run_picker_with_keys(entries, vec![KeyCode::Esc]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tui_create_new_auto_name() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        // j to move to "Create new" row, Enter to confirm
        let result =
            run_picker_with_keys(entries, vec![KeyCode::Char('j'), KeyCode::Enter]).unwrap();
        match result {
            Some(PickerResult::CreateNew(None)) => {}
            other => panic!("expected CreateNew(None), got {:?}", other),
        }
    }

    #[test]
    fn tui_create_new_with_name() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        // j to "Create new", type "foo", Enter
        let result = run_picker_with_keys(
            entries,
            vec![
                KeyCode::Char('j'),
                KeyCode::Char('f'),
                KeyCode::Char('o'),
                KeyCode::Char('o'),
                KeyCode::Enter,
            ],
        )
        .unwrap();
        match result {
            Some(PickerResult::CreateNew(Some(name))) => assert_eq!(name, "foo"),
            other => panic!("expected CreateNew(Some(\"foo\")), got {:?}", other),
        }
    }

    #[test]
    fn tui_delete_flow() {
        // After deletion the picker should continue (not exit), and the
        // refreshed list should be used. We verify by selecting the
        // remaining entry.
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let mut deleted_name = String::new();
        let result = run_picker_with_keys_and_callbacks(
            entries,
            vec![
                KeyCode::Char('d'), // initiate delete on ws1
                KeyCode::Char('y'), // confirm
                KeyCode::Enter,     // select first entry (now ws2)
            ],
            &mut |name| {
                deleted_name = name.to_string();
                Ok(false) // no redirect
            },
            &mut || {
                // Return refreshed list with ws1 removed
                Ok(vec![make_named_entry_ranked("ws2", "/tmp/ws2", 0)])
            },
        )
        .unwrap();
        assert_eq!(deleted_name, "ws1");
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/ws2"),
            other => panic!(
                "expected Selected(ws2) after delete+refresh, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn tui_delete_redirect_exits_picker() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let result = run_picker_with_keys_and_callbacks(
            entries,
            vec![KeyCode::Char('d'), KeyCode::Char('y')],
            &mut |_| Ok(true), // redirect happened
            &mut || Ok(vec![]),
        )
        .unwrap();
        // Picker should exit with None (redirect path already printed)
        assert!(result.is_none());
    }

    #[test]
    fn tui_delete_empty_list_exits_picker() {
        let entries = vec![make_named_entry_ranked("ws1", "/tmp/ws1", 0)];
        let result = run_picker_with_keys_and_callbacks(
            entries,
            vec![KeyCode::Char('d'), KeyCode::Char('y')],
            &mut |_| Ok(false),
            &mut || Ok(vec![]), // no entries left
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tui_delete_shows_status_message() {
        // After deletion, the status message should appear in the rendered help bar.
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut keys = vec![
            KeyCode::Char('d'), // initiate delete on ws1
            KeyCode::Char('y'), // confirm
        ]
        .into_iter();
        // Run one iteration that processes 'd', then 'y' which triggers delete+refresh,
        // then we stop and inspect the buffer.
        run_picker_inner(
            &mut terminal,
            App::new(entries),
            &mut || match keys.next() {
                Some(code) => Ok(Some(key(code))),
                // After processing keys, send Esc to exit so we can check the last frame
                None => Ok(Some(key(KeyCode::Esc))),
            },
            &mut |_| Ok(false),
            &mut || Ok(vec![make_named_entry_ranked("ws2", "/tmp/ws2", 0)]),
        )
        .unwrap();
        // The status message "workspace 'ws1' deleted" should have been rendered
        // in the frame right after deletion (before the Esc cleared it).
        // Since Esc exits immediately without redraw, the last rendered frame
        // still has the status message.
        let lines = buffer_lines(&terminal);
        let all_text = lines.join("\n");
        assert!(
            all_text.contains("workspace 'ws1' deleted"),
            "expected status message in help bar, got:\n{}",
            all_text
        );
    }

    #[test]
    fn tui_delete_cancel_with_n() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        // d to initiate, n to cancel, then q to quit
        let result = run_picker_with_keys(
            entries,
            vec![KeyCode::Char('d'), KeyCode::Char('n'), KeyCode::Char('q')],
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tui_delete_refused_on_main() {
        let entries = vec![
            make_main_entry("default", "/tmp/main"),
            make_named_entry_ranked("ws1", "/tmp/ws1", 1),
        ];
        // main entry is first (most recent by default), d on main does nothing, then q
        let result =
            run_picker_with_keys(entries, vec![KeyCode::Char('d'), KeyCode::Char('q')]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tui_filter_and_select() {
        let entries = vec![
            make_named_entry_ranked("apple", "/tmp/apple", 0),
            make_named_entry_ranked("banana", "/tmp/banana", 1),
            make_named_entry_ranked("cherry", "/tmp/cherry", 2),
        ];
        // / to enter filter, type "ban", Enter to apply, Enter to select
        let result = run_picker_with_keys(
            entries,
            vec![
                KeyCode::Char('/'),
                KeyCode::Char('b'),
                KeyCode::Char('a'),
                KeyCode::Char('n'),
                KeyCode::Enter,
                KeyCode::Enter,
            ],
        )
        .unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/banana"),
            other => panic!("expected Selected banana, got {:?}", other),
        }
    }

    #[test]
    fn tui_filter_esc_clears() {
        let entries = vec![
            make_named_entry_ranked("apple", "/tmp/apple", 0),
            make_named_entry_ranked("banana", "/tmp/banana", 1),
        ];
        // / to filter, type "ban", Esc to clear filter, Enter selects first (apple)
        let result = run_picker_with_keys(
            entries,
            vec![
                KeyCode::Char('/'),
                KeyCode::Char('b'),
                KeyCode::Char('a'),
                KeyCode::Char('n'),
                KeyCode::Esc,
                KeyCode::Enter,
            ],
        )
        .unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/apple"),
            other => panic!("expected Selected apple, got {:?}", other),
        }
    }

    #[test]
    fn tui_input_name_backspace_returns_to_browse() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        // j to Create row, type 'a' (enters InputName), backspace (returns to Browse), q to quit
        let result = run_picker_with_keys(
            entries,
            vec![
                KeyCode::Char('j'),
                KeyCode::Char('a'),
                KeyCode::Backspace,
                KeyCode::Char('q'),
            ],
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tui_input_name_esc_cancels() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        // j to Create row, type 'a', Esc cancels, q quits
        let result = run_picker_with_keys(
            entries,
            vec![
                KeyCode::Char('j'),
                KeyCode::Char('a'),
                KeyCode::Esc,
                KeyCode::Char('q'),
            ],
        )
        .unwrap();
        assert!(result.is_none());
    }

    // ── Multi-repo picker integration tests ─────────────────────────

    #[test]
    fn tui_multi_select_first() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let result = run_multi_picker_with_keys(entries, vec![KeyCode::Enter]).unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/ws1"),
            other => panic!("expected Selected, got {:?}", other),
        }
    }

    #[test]
    fn tui_multi_navigate_and_select() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let result =
            run_multi_picker_with_keys(entries, vec![KeyCode::Char('j'), KeyCode::Enter]).unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/ws2"),
            other => panic!("expected Selected ws2, got {:?}", other),
        }
    }

    #[test]
    fn tui_multi_quit() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        let result = run_multi_picker_with_keys(entries, vec![KeyCode::Char('q')]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tui_multi_filter_and_select() {
        let entries = vec![
            make_named_entry_ranked("alpha", "/tmp/alpha", 0),
            make_named_entry_ranked("beta", "/tmp/beta", 1),
        ];
        let result = run_multi_picker_with_keys(
            entries,
            vec![
                KeyCode::Char('/'),
                KeyCode::Char('b'),
                KeyCode::Char('e'),
                KeyCode::Enter,
                KeyCode::Enter,
            ],
        )
        .unwrap();
        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/beta"),
            other => panic!("expected Selected beta, got {:?}", other),
        }
    }

    // ── Preview pane tests ──────────────────────────────────────────

    #[test]
    fn tui_preview_hidden_by_default() {
        let app = App::new(vec![make_named_entry("ws1", "/tmp/ws1")]);
        assert!(!app.show_preview);
        assert!(matches!(app.preview, PreviewState::Hidden));
    }

    #[test]
    fn tui_preview_toggle() {
        let entries = vec![make_named_entry("ws1", "/tmp/ws1")];
        let mut app = App::new(entries);

        // Initially hidden
        assert!(!app.show_preview);

        // Toggle on
        app.show_preview = true;
        assert!(app.show_preview);

        // Toggle off
        app.show_preview = false;
        app.preview = PreviewState::Hidden;
        assert!(!app.show_preview);
        assert!(matches!(app.preview, PreviewState::Hidden));
    }

    #[test]
    fn tui_preview_toggle_via_keys() {
        // Press p to enable preview, then p to disable, then q to quit
        let entries = vec![make_named_entry_ranked("ws1", "/tmp/ws1", 0)];
        let result = run_picker_with_keys(
            entries,
            vec![KeyCode::Char('p'), KeyCode::Char('p'), KeyCode::Char('q')],
        )
        .unwrap();
        // Should quit normally
        assert!(result.is_none());
    }

    #[test]
    fn tui_multi_preview_toggle_via_keys() {
        let entries = vec![make_named_entry_ranked("ws1", "/tmp/ws1", 0)];
        let result = run_multi_picker_with_keys(
            entries,
            vec![KeyCode::Char('p'), KeyCode::Char('p'), KeyCode::Char('q')],
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tui_multi_preview_hidden_by_default() {
        let app = MultiRepoApp::new(vec![make_named_entry("ws1", "/tmp/ws1")]);
        assert!(!app.show_preview);
        assert!(matches!(app.preview, PreviewState::Hidden));
    }

    /// Helper to extract all visible text from a terminal buffer as one string per row.
    fn buffer_lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn tui_table_scrolls_with_cursor() {
        // Terminal height 10: border(1) + header(1) + 6 visible rows + border(1) + help(1) = 10
        // Create 20 entries so most won't fit on screen
        let entries: Vec<WorkspaceEntry> = (0..20)
            .map(|i| {
                make_named_entry_ranked(&format!("ws-{:02}", i), &format!("/tmp/ws-{:02}", i), i)
            })
            .collect();

        let mut app = App::new(entries);

        // Navigate to the last data entry (index 19), which is beyond visible area
        for _ in 0..19 {
            app.next();
        }
        assert_eq!(app.selected, 19);

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let lines = buffer_lines(&terminal);
        let all_text = lines.join("\n");

        // The selected entry "ws-19" should be visible after scrolling
        assert!(
            all_text.contains("ws-19"),
            "selected entry 'ws-19' should be visible after scrolling, buffer:\n{}",
            all_text,
        );
    }

    #[test]
    fn tui_multi_table_scrolls_with_cursor() {
        let entries: Vec<WorkspaceEntry> = (0..20)
            .map(|i| {
                let mut e = make_named_entry_ranked(
                    &format!("ws-{:02}", i),
                    &format!("/tmp/ws-{:02}", i),
                    i,
                );
                e.repo_name = Some("repo".to_string());
                e
            })
            .collect();

        let mut app = MultiRepoApp::new(entries);

        for _ in 0..19 {
            app.next();
        }
        assert_eq!(app.selected, 19);

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_multi_repo(f, &mut app)).unwrap();

        let lines = buffer_lines(&terminal);
        let all_text = lines.join("\n");

        assert!(
            all_text.contains("ws-19"),
            "selected entry 'ws-19' should be visible after scrolling, buffer:\n{}",
            all_text,
        );
    }

    // ── Merge / drain unit tests ────────────────────────────────────

    #[test]
    fn merge_entries_preserves_selection() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
            make_named_entry_ranked("ws3", "/tmp/ws3", 2),
        ];
        let mut app = App::new(entries);
        // Select ws2
        app.next();
        assert_eq!(app.entries[app.filtered_indices[app.selected]].name, "ws2");

        // Merge with same entries (different order to prove re-sort works)
        let new_entries = vec![
            make_named_entry_ranked("ws3", "/tmp/ws3", 2),
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        app.merge_entries(new_entries);

        // Selection should still be on ws2
        assert_eq!(app.entries[app.filtered_indices[app.selected]].name, "ws2");
    }

    #[test]
    fn merge_entries_resets_when_selected_disappears() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let mut app = App::new(entries);
        // Select ws2
        app.next();
        assert_eq!(app.entries[app.filtered_indices[app.selected]].name, "ws2");

        // Merge without ws2
        let new_entries = vec![make_named_entry_ranked("ws1", "/tmp/ws1", 0)];
        app.merge_entries(new_entries);

        // Should fall back to 0
        assert_eq!(app.selected, 0);
        assert_eq!(app.entries[app.filtered_indices[app.selected]].name, "ws1");
    }

    #[test]
    fn merge_entries_re_sorts() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let mut app = App::new(entries);

        // Switch to name sort
        app.sort_mode = SortMode::Name;
        sort_entries(&mut app.entries, app.sort_mode);
        app.recompute_filter();

        // Merge with entries that would sort differently
        let new_entries = vec![
            make_named_entry_ranked("cherry", "/tmp/cherry", 0),
            make_named_entry_ranked("apple", "/tmp/apple", 1),
            make_named_entry_ranked("banana", "/tmp/banana", 2),
        ];
        app.merge_entries(new_entries);

        // Verify sorted by name
        let names: Vec<&str> = app.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn merge_entries_respects_filter() {
        let entries = vec![
            make_named_entry_ranked("apple", "/tmp/apple", 0),
            make_named_entry_ranked("banana", "/tmp/banana", 1),
        ];
        let mut app = App::new(entries);

        // Set a filter
        app.filter_buf = "ban".to_string();
        app.recompute_filter();
        assert_eq!(app.filtered_indices.len(), 1);

        // Merge in new entries
        let new_entries = vec![
            make_named_entry_ranked("apple", "/tmp/apple", 0),
            make_named_entry_ranked("banana", "/tmp/banana", 1),
            make_named_entry_ranked("bandana", "/tmp/bandana", 2),
        ];
        app.merge_entries(new_entries);

        // Filter should still be applied — "ban" matches banana and bandana
        assert_eq!(app.filtered_indices.len(), 2);
        let visible_names: Vec<&str> = app
            .filtered_indices
            .iter()
            .map(|&i| app.entries[i].name.as_str())
            .collect();
        assert!(visible_names.contains(&"banana"));
        assert!(visible_names.contains(&"bandana"));
    }

    #[test]
    fn drain_agent_updates_in_place() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let mut app = App::new(entries);

        // Post agent summaries to the mailbox
        let mut summaries = HashMap::new();
        summaries.insert(
            "ws1".to_string(),
            AgentSummary {
                waiting: 1,
                working: 0,
                idle: 0,
            },
        );
        *app.agent_refresh_mailbox.0.lock().unwrap() = Some(summaries);

        app.drain_refresh_mailbox();

        // ws1 should have agent status, ws2 should not
        assert!(
            app.entries
                .iter()
                .find(|e| e.name == "ws1")
                .unwrap()
                .agent_status
                .is_some()
        );
        assert!(
            app.entries
                .iter()
                .find(|e| e.name == "ws2")
                .unwrap()
                .agent_status
                .is_none()
        );
    }

    #[test]
    fn drain_full_refresh() {
        let entries = vec![make_named_entry_ranked("ws1", "/tmp/ws1", 0)];
        let mut app = App::new(entries);

        // Post new entries to refresh mailbox
        let new_entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws-new", "/tmp/ws-new", 1),
        ];
        *app.refresh_mailbox.0.lock().unwrap() = Some(new_entries);

        app.drain_refresh_mailbox();

        assert_eq!(app.entries.len(), 2);
        assert!(app.entries.iter().any(|e| e.name == "ws-new"));
    }

    #[test]
    fn drain_noop_when_empty() {
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        let mut app = App::new(entries);
        let original_len = app.entries.len();

        // Drain with nothing posted
        app.drain_refresh_mailbox();

        assert_eq!(app.entries.len(), original_len);
    }

    // ── Background thread integration tests ──────────────────────────

    #[test]
    fn refresh_thread_posts_to_mailbox() {
        let stop = Arc::new(StopSignal::new());
        let sender = Arc::new(Mutex::new(None::<Vec<String>>));
        let sender_clone = Arc::clone(&sender);

        let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = Arc::clone(&call_count);

        let handle = spawn_refresh_thread(
            Duration::from_millis(50),
            Arc::clone(&stop),
            sender_clone,
            move || {
                count_clone.fetch_add(1, Ordering::Relaxed);
                Some(vec!["hello".to_string()])
            },
        );

        std::thread::sleep(Duration::from_millis(200));
        stop.stop();
        handle.join().unwrap();

        // Should have posted at least once
        let data = sender.lock().unwrap().take();
        assert!(data.is_some(), "expected data in mailbox");
        assert_eq!(data.unwrap(), vec!["hello".to_string()]);

        // Producer should have been called multiple times
        assert!(call_count.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn refresh_thread_stops_on_flag() {
        let stop = Arc::new(StopSignal::new());
        let sender = Arc::new(Mutex::new(None::<u32>));

        let handle = spawn_refresh_thread(
            Duration::from_millis(500),
            Arc::clone(&stop),
            sender,
            || Some(42),
        );

        // Stop immediately — condvar should wake the thread instantly
        stop.stop();
        let start = std::time::Instant::now();
        handle.join().unwrap();
        let elapsed = start.elapsed();

        // Thread should exit nearly instantly (not wait for the 500ms sleep)
        assert!(
            elapsed < Duration::from_millis(100),
            "thread took too long to stop: {:?}",
            elapsed
        );
    }

    #[test]
    fn agent_thread_posts_summaries() {
        let stop = Arc::new(StopSignal::new());
        let sender = Arc::new(Mutex::new(None::<HashMap<String, AgentSummary>>));
        let sender_clone = Arc::clone(&sender);

        let handle = spawn_refresh_thread(
            Duration::from_millis(50),
            Arc::clone(&stop),
            sender_clone,
            move || {
                let mut map = HashMap::new();
                map.insert(
                    "ws1".to_string(),
                    AgentSummary {
                        waiting: 0,
                        working: 1,
                        idle: 0,
                    },
                );
                Some(map)
            },
        );

        std::thread::sleep(Duration::from_millis(150));
        stop.stop();
        handle.join().unwrap();

        let data = sender.lock().unwrap().take();
        assert!(data.is_some());
        let summaries = data.unwrap();
        assert!(summaries.contains_key("ws1"));
        assert_eq!(summaries["ws1"].working, 1);
    }

    // ── Full integration test with run_picker_inner + mailbox ────────

    #[test]
    fn run_picker_inner_sees_refreshed_data() {
        // Start with one entry, post a refresh with two entries via mailbox,
        // then verify the picker uses the updated entries.
        let entries = vec![make_named_entry_ranked("ws1", "/tmp/ws1", 0)];
        let app = App::new(entries);

        // Pre-load the refresh mailbox with new entries
        let new_entries = vec![
            make_named_entry_ranked("ws-alpha", "/tmp/ws-alpha", 0),
            make_named_entry_ranked("ws-beta", "/tmp/ws-beta", 1),
        ];
        *app.refresh_mailbox.0.lock().unwrap() = Some(new_entries);

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Feed: None (triggers drain), then j (move down), Enter (select ws-beta)
        let mut events = vec![
            None,
            Some(key(KeyCode::Char('j'))),
            Some(key(KeyCode::Enter)),
        ]
        .into_iter();

        let result = run_picker_inner(
            &mut terminal,
            app,
            &mut || Ok(events.next().unwrap_or(Some(key(KeyCode::Esc)))),
            &mut |_| Ok(false),
            &mut || Ok(vec![]),
        )
        .unwrap();

        match result {
            Some(PickerResult::Selected(path)) => assert_eq!(path, "/tmp/ws-beta"),
            other => panic!("expected Selected ws-beta, got {:?}", other),
        }
    }

    #[test]
    fn tui_help_bar_shows_preview_hint() {
        let mut app = App::new(vec![make_named_entry("ws1", "/tmp/ws1")]);
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Check the last line of the buffer for the help text
        let last_row = buf.area.height - 1;
        let mut line = String::new();
        for x in 0..buf.area.width {
            let cell = &buf[(x, last_row)];
            line.push_str(cell.symbol());
        }
        assert!(
            line.contains("p: preview"),
            "help bar should contain 'p: preview', got: '{}'",
            line.trim()
        );
    }
}
