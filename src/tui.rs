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

struct App {
    entries: Vec<WorkspaceEntry>,
    selected: usize,
}

impl App {
    fn new(entries: Vec<WorkspaceEntry>) -> Self {
        Self {
            selected: 0,
            entries,
        }
    }

    fn next(&mut self) {
        if !self.entries.is_empty() {
            self.selected = (self.selected + 1) % self.entries.len();
        }
    }

    fn previous(&mut self) {
        if !self.entries.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.entries.len() - 1);
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

    let rows: Vec<Row> = app
        .entries
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
        let help = Paragraph::new(" j/k: navigate  Enter: select  q: quit")
            .style(Style::default().fg(Color::DarkGray));
        let help_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        frame.render_widget(help, help_area);
    }
}

pub fn run_picker(entries: Vec<WorkspaceEntry>) -> Result<Option<String>> {
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
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    result = None;
                    break;
                }
                KeyCode::Char('j') | KeyCode::Down => app.next(),
                KeyCode::Char('k') | KeyCode::Up => app.previous(),
                KeyCode::Enter => {
                    let path = app.entries[app.selected].path.to_string_lossy().to_string();
                    result = Some(path);
                    break;
                }
                _ => {}
            }
        }
    }

    // Cleanup
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(result)
}
