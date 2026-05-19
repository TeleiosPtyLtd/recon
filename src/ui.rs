use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Row, Table, Paragraph},
};

use crate::app::App;
use crate::session::SessionStatus;

pub fn render(frame: &mut Frame, app: &App) {
    let show_search = app.filter_active || !app.filter_text.is_empty();
    let chunks = if show_search {
        Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area())
    } else {
        Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area())
    };

    render_table(frame, app, chunks[0]);
    if show_search {
        render_search_bar(frame, app, chunks[1]);
        render_footer(frame, app, chunks[2]);
    } else {
        render_footer(frame, app, chunks[1]);
    }
}

fn render_table(frame: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(vec![
        Cell::from(" # "),
        Cell::from("Session"),
        Cell::from("Project"),
        Cell::from("Directory"),
        Cell::from("Status"),
        Cell::from("Model"),
        Cell::from("Context"),
        Cell::from("Last Activity"),
    ])
    .style(
        Style::default()
            .fg(Color::Rgb(0x8A, 0x84, 0x7A))
            .add_modifier(Modifier::BOLD),
    );

    let filtered = app.filtered_indices();
    let rows: Vec<Row> = filtered
        .iter()
        .enumerate()
        .map(|(display_idx, &real_idx)| {
            let session = &app.sessions[real_idx];
            let num = format!(" {} ", real_idx + 1);

            let tmux_name = session
                .tmux_session
                .as_deref()
                .unwrap_or("—");

            // Status: shape + label + color (layered encoding — don't rely on color alone)
            let (status_dot, status_label, status_color) = match session.status {
                SessionStatus::New     => ("○", "New",     Color::Rgb(0x3D, 0x47, 0x59)),
                SessionStatus::Working => ("●", "Working", Color::Rgb(0x4A, 0x55, 0x68)),
                SessionStatus::Idle    => ("●", "Idle",    Color::Gray),
                SessionStatus::Input   => ("▲", "Input",   Color::Rgb(0xDD, 0x6B, 0x20)),
            };

            let token_ratio = session.token_ratio();
            let token_style = if token_ratio > 0.9 {
                Style::default().fg(Color::Red)
            } else if token_ratio > 0.75 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };

            let activity = session
                .last_activity
                .as_deref()
                .map(format_timestamp)
                .unwrap_or_else(|| "—".to_string());

            let cwd_display = shorten_home(&session.cwd);

            // Project: repo::relative_dir::branch
            let project_cell = {
                let mut spans = vec![Span::raw(&session.project_name)];
                if let Some(dir) = &session.relative_dir {
                    spans.push(Span::styled("::", Style::default().fg(Color::DarkGray)));
                    spans.push(Span::raw(dir.clone()));
                }
                if let Some(b) = &session.branch {
                    spans.push(Span::styled("::", Style::default().fg(Color::DarkGray)));
                    spans.push(Span::styled(b, Style::default().fg(Color::Rgb(0x9A, 0x94, 0x8A))));
                }
                Cell::from(Line::from(spans))
            };

            // Status: colored dot + label
            let status_cell = Cell::from(Line::from(vec![
                Span::styled(status_dot, Style::default().fg(status_color)),
                Span::styled(
                    format!(" {status_label}"),
                    Style::default().fg(status_color),
                ),
            ]));

            // Directory: dimmed
            let dir_cell =
                Cell::from(cwd_display).style(Style::default().fg(Color::DarkGray));

            let row = Row::new(vec![
                Cell::from(num),
                Cell::from(tmux_name.to_string()),
                project_cell,
                dir_cell,
                status_cell,
                Cell::from(session.model_display()),
                Cell::from(session.token_display()).style(token_style),
                Cell::from(activity),
            ]);

            // Row tint matches the tmux pane paint so the dashboard and the
            // panes you're switching between speak the same visual language.
            if session.status == SessionStatus::Input {
                row.style(Style::default().bg(Color::Rgb(0x2D, 0x1F, 0x10)))
            } else if display_idx == app.selected {
                row.style(Style::default().bg(Color::Rgb(0x2E, 0x31, 0x48)))
            } else {
                row
            }
        })
        .collect();

    let widths = [
        Constraint::Length(4),   // #
        Constraint::Length(16),  // Session
        Constraint::Min(20),    // Project (repo + branch)
        Constraint::Length(20), // Directory
        Constraint::Length(10), // Status
        Constraint::Length(20), // Model
        Constraint::Length(14), // Context
        Constraint::Length(14), // Last Activity
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" recon — Claude Code Sessions "),
        );

    frame.render_widget(table, area);
}

fn render_search_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled("/", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
        Span::raw(&app.filter_text),
    ];
    if !app.filter_active && !app.filter_text.is_empty() {
        let count = app.filtered_indices().len();
        spans.push(Span::styled(
            format!("  ({} match{})", count, if count == 1 { "" } else { "es" }),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let paragraph = Paragraph::new(Line::from(spans));
    frame.render_widget(paragraph, area);

    if app.filter_active {
        frame.set_cursor_position((area.x + 1 + app.filter_cursor as u16, area.y));
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let spans = if app.filter_active {
        vec![
            Span::styled("Esc", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" clear  "),
            Span::styled("Enter", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" keep filter  "),
            Span::styled("j/k", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" navigate"),
        ]
    } else {
        vec![
            Span::styled("j/k", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" navigate  "),
            Span::styled("1-9", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" jump  "),
            Span::styled("Enter", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" switch  "),
            Span::styled("x", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" kill  "),
            Span::styled("/", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" search  "),
            Span::styled("v", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" view  "),
            Span::styled("i", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" next input  "),
            Span::styled("q", Style::default().fg(Color::Rgb(0xB0, 0xA8, 0x98))),
            Span::raw(" quit"),
        ]
    };
    let footer = Paragraph::new(Line::from(spans));
    frame.render_widget(footer, area);
}

/// Replace home directory prefix with ~.
fn shorten_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Format an ISO timestamp into a relative or short time string.
fn format_timestamp(ts: &str) -> String {
    use chrono::{DateTime, Local, Utc};

    let parsed = ts.parse::<DateTime<Utc>>();
    match parsed {
        Ok(dt) => {
            let now = Utc::now();
            let diff = now - dt;

            if diff.num_seconds() < 60 {
                "< 1m".to_string()
            } else if diff.num_minutes() < 60 {
                format!("{}m ago", diff.num_minutes())
            } else if diff.num_hours() < 24 {
                format!("{}h ago", diff.num_hours())
            } else {
                dt.with_timezone(&Local).format("%b %d %H:%M").to_string()
            }
        }
        Err(_) => ts.to_string(),
    }
}
