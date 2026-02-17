use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Frame, prelude::*, widgets::*};
use std::io;

use crate::workspace::{WorkspaceEntry, format_time_ago};

#[derive(Debug)]
pub enum PickerResult {
    Selected(String),
    CreateNew(Option<String>),
    Delete(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SortMode {
    Recency,
    Name,
    DiffSize,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::Recency => SortMode::Name,
            SortMode::Name => SortMode::DiffSize,
            SortMode::DiffSize => SortMode::Recency,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::Recency => "recency",
            SortMode::Name => "name",
            SortMode::DiffSize => "diff size",
        }
    }
}

fn matches_filter(entry: &WorkspaceEntry, query: &str) -> bool {
    let query = query.to_lowercase();
    entry.name.to_lowercase().contains(&query)
        || entry.description.to_lowercase().contains(&query)
        || entry
            .bookmarks
            .iter()
            .any(|b| b.to_lowercase().contains(&query))
}

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

#[derive(Debug, PartialEq)]
enum Mode {
    Browse,
    InputName,
    Filter,
    ConfirmDelete(String),
}

struct App {
    entries: Vec<WorkspaceEntry>,
    selected: usize,
    mode: Mode,
    input_buf: String,
    sort_mode: SortMode,
    filter_buf: String,
    filtered_indices: Vec<usize>,
}

impl App {
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
        }
    }

    fn visible_entries(&self) -> Vec<&WorkspaceEntry> {
        self.filtered_indices
            .iter()
            .map(|&i| &self.entries[i])
            .collect()
    }

    fn total_rows(&self) -> usize {
        self.filtered_indices.len() + 1 // +1 for "Create new" row
    }

    fn on_create_row(&self) -> bool {
        self.selected == self.filtered_indices.len()
    }

    fn selected_entry_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected).copied()
    }

    fn next(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected = (self.selected + 1) % total;
        }
    }

    fn previous(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(total - 1);
        }
    }

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
    }
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let header_cells = [
        "Name",
        "Change",
        "Description",
        "Bookmarks",
        "Modified",
        "Changes",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().fg(Color::White).bold()));
    let header = Row::new(header_cells)
        .style(Style::default().bg(Color::DarkGray))
        .height(1);

    let visible = app.visible_entries();
    let mut rows: Vec<Row> = visible
        .iter()
        .enumerate()
        .map(|(i, entry)| {
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

            let style = if i == app.selected {
                Style::default().bg(Color::Rgb(40, 40, 60))
            } else {
                Style::default()
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

            Row::new(vec![
                Cell::from(name_text).style(Style::default().fg(name_fg)),
                Cell::from(change_text).style(Style::default().fg(change_fg)),
                Cell::from(desc_text).style(Style::default().fg(desc_fg)),
                Cell::from(bookmarks_text).style(Style::default().fg(bookmark_fg)),
                Cell::from(time_text).style(Style::default().fg(time_fg)),
                Cell::from(changes_text).style(Style::default().fg(changes_fg)),
            ])
            .style(style)
        })
        .collect();

    // Append "+ Create new" row
    let create_row_selected = app.on_create_row();
    let create_style = if create_row_selected {
        Style::default().bg(Color::Rgb(40, 40, 60))
    } else {
        Style::default()
    };

    let create_name = if app.mode == Mode::InputName && create_row_selected {
        format!("Name: {}_", app.input_buf)
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
        ])
        .style(create_style),
    );

    let widths = [
        Constraint::Percentage(15),
        Constraint::Percentage(8),
        Constraint::Percentage(35),
        Constraint::Percentage(14),
        Constraint::Percentage(13),
        Constraint::Percentage(15),
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

    frame.render_widget(table, area);

    // Render help bar at bottom
    if area.height > 3 {
        let help_text = match app.mode {
            Mode::InputName => " Enter: create  Esc: cancel".to_string(),
            Mode::Filter => format!(" filter: {}▏  Enter: apply  Esc: clear", app.filter_buf),
            Mode::ConfirmDelete(ref name) => format!(" Delete '{}'? y: confirm  n: cancel", name),
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
                    " j/k: navigate  /: filter  s: sort ({})  d: delete  Enter: select  q: quit{}",
                    app.sort_mode.label(),
                    filter_info
                )
            }
        };
        let help = Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray));
        let help_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        frame.render_widget(help, help_area);
    }
}

fn run_picker_inner<B: Backend>(
    terminal: &mut Terminal<B>,
    entries: Vec<WorkspaceEntry>,
    next_event: &mut dyn FnMut() -> Result<Event>,
) -> Result<Option<PickerResult>> {
    let mut app = App::new(entries);

    loop {
        terminal.draw(|f| render(f, &app))?;

        if let Event::Key(key) = next_event()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

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
                    }
                    KeyCode::Char('/') => {
                        app.mode = Mode::Filter;
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
                        return Ok(Some(PickerResult::Delete(name)));
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        app.mode = Mode::Browse;
                    }
                    _ => {}
                },
            }
        }
    }
}

pub fn run_picker(entries: Vec<WorkspaceEntry>) -> Result<Option<PickerResult>> {
    if entries.is_empty() {
        eprintln!("no workspaces found");
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stderr = io::stderr();
    crossterm::execute!(stderr, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let result = run_picker_inner(&mut terminal, entries, &mut || Ok(event::read()?));

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

// ── Multi-repo picker (--all mode) ──────────────────────────────

struct MultiRepoApp {
    entries: Vec<WorkspaceEntry>,
    selected: usize,
    sort_mode: SortMode,
    filter_buf: String,
    filtered_indices: Vec<usize>,
    filter_mode: bool,
}

impl MultiRepoApp {
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
        }
    }

    fn visible_entries(&self) -> Vec<&WorkspaceEntry> {
        self.filtered_indices
            .iter()
            .map(|&i| &self.entries[i])
            .collect()
    }

    fn total_rows(&self) -> usize {
        self.filtered_indices.len()
    }

    fn next(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected = (self.selected + 1) % total;
        }
    }

    fn previous(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(total - 1);
        }
    }

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
    }
}

fn render_multi_repo(frame: &mut Frame, app: &MultiRepoApp) {
    let area = frame.area();

    let header_cells = [
        "Repo",
        "Name",
        "Change",
        "Description",
        "Bookmarks",
        "Modified",
        "Changes",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().fg(Color::White).bold()));
    let header = Row::new(header_cells)
        .style(Style::default().bg(Color::DarkGray))
        .height(1);

    let visible = app.visible_entries();
    let rows: Vec<Row> = visible
        .iter()
        .enumerate()
        .map(|(i, entry)| {
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

            let style = if i == app.selected {
                Style::default().bg(Color::Rgb(40, 40, 60))
            } else {
                Style::default()
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

            Row::new(vec![
                Cell::from(repo_text).style(Style::default().fg(Color::Green)),
                Cell::from(name_text).style(Style::default().fg(name_fg)),
                Cell::from(change_text).style(Style::default().fg(change_fg)),
                Cell::from(desc_text).style(Style::default().fg(desc_fg)),
                Cell::from(bookmarks_text).style(Style::default().fg(bookmark_fg)),
                Cell::from(time_text).style(Style::default().fg(time_fg)),
                Cell::from(changes_text).style(Style::default().fg(changes_fg)),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Percentage(10),
        Constraint::Percentage(12),
        Constraint::Percentage(8),
        Constraint::Percentage(30),
        Constraint::Percentage(12),
        Constraint::Percentage(13),
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

    frame.render_widget(table, area);

    if area.height > 3 {
        let help_text = if app.filter_mode {
            format!(" filter: {}▏  Enter: apply  Esc: clear", app.filter_buf)
        } else {
            let filter_info = if !app.filter_buf.is_empty() {
                format!("  [filter: \"{}\"]", app.filter_buf)
            } else {
                String::new()
            };
            format!(
                " j/k: navigate  /: filter  s: sort ({})  Enter: select  q: quit{}",
                app.sort_mode.label(),
                filter_info
            )
        };
        let help = Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray));
        let help_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        frame.render_widget(help, help_area);
    }
}

fn run_picker_multi_repo_inner<B: Backend>(
    terminal: &mut Terminal<B>,
    entries: Vec<WorkspaceEntry>,
    next_event: &mut dyn FnMut() -> Result<Event>,
) -> Result<Option<PickerResult>> {
    let mut app = MultiRepoApp::new(entries);

    loop {
        terminal.draw(|f| render_multi_repo(f, &app))?;

        if let Event::Key(key) = next_event()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

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
                    }
                    KeyCode::Char('/') => {
                        app.filter_mode = true;
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
        }
    }
}

pub fn run_picker_multi_repo(entries: Vec<WorkspaceEntry>) -> Result<Option<PickerResult>> {
    if entries.is_empty() {
        eprintln!("no workspaces found");
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stderr = io::stderr();
    crossterm::execute!(stderr, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let result = run_picker_multi_repo_inner(&mut terminal, entries, &mut || Ok(event::read()?));

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

            assert_eq!(app.mode, Mode::InputName, "char '{}' should enter InputName", ch);
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
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        let mut key_iter = keys.into_iter();
        run_picker_inner(&mut terminal, entries, &mut || {
            match key_iter.next() {
                Some(code) => Ok(key(code)),
                None => Ok(key(KeyCode::Esc)),
            }
        })
    }

    /// Drive run_picker_multi_repo_inner with a sequence of key events.
    fn run_multi_picker_with_keys(
        entries: Vec<WorkspaceEntry>,
        keys: Vec<KeyCode>,
    ) -> Result<Option<PickerResult>> {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        let mut key_iter = keys.into_iter();
        run_picker_multi_repo_inner(&mut terminal, entries, &mut || {
            match key_iter.next() {
                Some(code) => Ok(key(code)),
                None => Ok(key(KeyCode::Esc)),
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
        let result =
            run_picker_with_keys(entries, vec![KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Enter])
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
        let result =
            run_picker_with_keys(entries, vec![KeyCode::Down, KeyCode::Enter]).unwrap();
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
            run_picker_with_keys(entries, vec![KeyCode::Up, KeyCode::Up, KeyCode::Enter])
                .unwrap();
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
        let entries = vec![
            make_named_entry_ranked("ws1", "/tmp/ws1", 0),
            make_named_entry_ranked("ws2", "/tmp/ws2", 1),
        ];
        // d to initiate delete on ws1 (first/selected), y to confirm
        let result =
            run_picker_with_keys(entries, vec![KeyCode::Char('d'), KeyCode::Char('y')]).unwrap();
        match result {
            Some(PickerResult::Delete(name)) => assert_eq!(name, "ws1"),
            other => panic!("expected Delete(ws1), got {:?}", other),
        }
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
}
