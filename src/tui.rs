use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame,
    prelude::*,
    widgets::*,
};
use std::io;
use std::time::SystemTime;

use crate::workspace::WorkspaceEntry;

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
        || entry.bookmarks.iter().any(|b| b.to_lowercase().contains(&query))
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

#[derive(PartialEq)]
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
        self.filtered_indices.iter().map(|&i| &self.entries[i]).collect()
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
            self.filtered_indices = self.entries.iter().enumerate()
                .filter(|(_, e)| matches_filter(e, &self.filter_buf))
                .map(|(i, _)| i)
                .collect();
        }
        if self.selected >= self.total_rows() {
            self.selected = self.total_rows().saturating_sub(1);
        }
    }
}

fn format_time_ago(time: Option<SystemTime>) -> String {
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

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let header_cells = ["Name", "Change", "Description", "Bookmarks", "Modified", "Changes"]
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
            } else {
                entry.name.clone()
            };

            let change_text = entry.change_id.clone();

            let desc_text = entry.description
                .lines()
                .next()
                .unwrap_or("")
                .to_string();

            let bookmarks_text = entry.bookmarks.join(", ");

            let time_text = format_time_ago(entry.last_modified);

            let stat = &entry.diff_stat;
            let changes_text = if stat.files_changed == 0 && stat.insertions == 0 && stat.deletions == 0 {
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

            Row::new(vec![
                Cell::from(name_text).style(Style::default().fg(Color::Cyan)),
                Cell::from(change_text).style(Style::default().fg(Color::Magenta)),
                Cell::from(desc_text).style(Style::default().fg(Color::White)),
                Cell::from(bookmarks_text).style(Style::default().fg(Color::Blue)),
                Cell::from(time_text).style(Style::default().fg(Color::Yellow)),
                Cell::from(changes_text).style(Style::default().fg(if stat.deletions > stat.insertions {
                    Color::Red
                } else if stat.insertions > 0 {
                    Color::Green
                } else {
                    Color::DarkGray
                })),
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
                .title(" jjws workspaces ")
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
                format!(" j/k: navigate  /: filter  s: sort ({})  d: delete  Enter: select  q: quit{}", app.sort_mode.label(), filter_info)
            }
        };
        let help = Paragraph::new(help_text)
            .style(Style::default().fg(Color::DarkGray));
        let help_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        frame.render_widget(help, help_area);
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

    let mut app = App::new(entries);
    let result;

    loop {
        terminal.draw(|f| render(f, &app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match app.mode {
                Mode::Browse => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        result = None;
                        break;
                    }
                    KeyCode::Char('j') | KeyCode::Down => app.next(),
                    KeyCode::Char('k') | KeyCode::Up => app.previous(),
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
                    KeyCode::Enter => {
                        if app.on_create_row() {
                            result = Some(PickerResult::CreateNew(None));
                            break;
                        } else if let Some(idx) = app.selected_entry_index() {
                            let path = app.entries[idx].path.to_string_lossy().to_string();
                            result = Some(PickerResult::Selected(path));
                            break;
                        }
                    }
                    KeyCode::Char(c) if app.on_create_row() => {
                        app.mode = Mode::InputName;
                        app.input_buf.clear();
                        app.input_buf.push(c);
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
                        result = Some(PickerResult::CreateNew(name));
                        break;
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
                        result = Some(PickerResult::Delete(name));
                        break;
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        app.mode = Mode::Browse;
                    }
                    _ => {}
                },
            }
        }
    }

    // Cleanup
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::DiffStat;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn make_entry(name: &str, modified_secs_ago: Option<u64>, insertions: u32, deletions: u32) -> WorkspaceEntry {
        WorkspaceEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{}", name)),
            last_modified: modified_secs_ago.map(|s| SystemTime::now() - Duration::from_secs(s)),
            diff_stat: DiffStat { files_changed: 1, insertions, deletions },
            is_main: false,
            change_id: String::new(),
            description: String::new(),
            bookmarks: Vec::new(),
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
}
