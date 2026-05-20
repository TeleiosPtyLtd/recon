use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Row, Table, Paragraph},
};

use crate::app::App;
use crate::paint;
use crate::session::SessionStatus;

pub fn render(frame: &mut Frame, app: &App) {
    // Paint a dark surface across the whole frame so the dashboard never
    // inherits a white terminal background.
    let bg = Block::default().style(Style::default().bg(Color::Rgb(0x0F, 0x11, 0x17)));
    frame.render_widget(bg, frame.area());

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
        Cell::from("Title"),
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

            // Tmux-active pane: a neon-green ▌ in the # column mirrors the
            // pane-active-border color used in flow mode, so the dashboard row
            // matches the green border the user sees on the focused pane.
            let num_cell = if session.tmux_active {
                Cell::from(Line::from(vec![
                    Span::styled("▌", Style::default().fg(Color::Rgb(0x39, 0xFF, 0x14))),
                    Span::raw(format!("{} ", real_idx + 1)),
                ]))
            } else {
                Cell::from(format!(" {} ", real_idx + 1))
            };

            // Prefer the pane title (Claude TUI writes a live task summary
            // there), falling back to the tmux session name for panes that
            // haven't set one — brand-new claude or non-claude shells.
            let title_text = session
                .pane_title
                .as_deref()
                .filter(|t| !t.is_empty())
                .or(session.tmux_session.as_deref())
                .unwrap_or("—")
                .to_string();

            // --dangerously-skip-permissions marker: prepend a red warning
            // glyph so unsupervised panes are unmistakable in the list.
            let session_cell = if session.dangerous {
                Cell::from(Line::from(vec![
                    Span::styled("⚠ ", Style::default().fg(Color::Rgb(0xE5, 0x3E, 0x3E)).add_modifier(Modifier::BOLD)),
                    Span::raw(title_text),
                ]))
            } else {
                Cell::from(title_text)
            };

            // Layered encoding (shape + label + color) so the dashboard is
            // legible under colorblind palettes and grayscale terminals.
            let (status_dot, status_label, status_color) = match session.status {
                SessionStatus::New     => ("○", "New",     Color::Rgb(0x5A, 0x5A, 0x5E)),
                SessionStatus::Working => ("●", "Working", Color::Rgb(0xB8, 0xB0, 0xA4)),
                SessionStatus::Idle    => ("●", "Idle",    Color::Rgb(0xE8, 0xE2, 0xD6)),
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

            let status_cell = Cell::from(Line::from(vec![
                Span::styled(status_dot, Style::default().fg(status_color)),
                Span::styled(
                    format!(" {status_label}"),
                    Style::default().fg(status_color),
                ),
            ]));

            let dir_cell =
                Cell::from(cwd_display).style(Style::default().fg(Color::DarkGray));

            let row = Row::new(vec![
                num_cell,
                session_cell,
                project_cell,
                dir_cell,
                status_cell,
                Cell::from(session.model_display()),
                Cell::from(session.token_display()).style(token_style),
                Cell::from(activity),
            ]);

            // Row tint mirrors the per-pane palette in paint.rs so the
            // dashboard and the panes you switch between speak the same
            // visual language. Working rows sink (near-black bg, dim base
            // fg); Idle rows rise (lighter bg, crisp base fg). Cells with
            // their own explicit fg (status dot, branch, token warnings)
            // keep their colour on top of the row base.
            //
            // Precedence: Input (red-warm) > Selected (blue tint) > status,
            // so attention-demanding panes always win the eye.
            if session.status == SessionStatus::Input {
                row.style(Style::default().bg(Color::Rgb(0x2D, 0x1F, 0x10)))
            } else if display_idx == app.selected {
                row.style(Style::default().bg(Color::Rgb(0x2E, 0x31, 0x48)))
            } else {
                // bg mirrors the pane paint (single source of truth in paint.rs).
                // fg is dashboard-local — smaller text wants its own contrast.
                let fg = match session.status {
                    SessionStatus::Working => Some(Color::Rgb(0x3A, 0x3E, 0x47)),
                    SessionStatus::Idle => Some(Color::Rgb(0xD8, 0xDA, 0xE2)),
                    _ => None,
                };
                match (paint::row_bg(&session.status), fg) {
                    (Some((r, g, b)), Some(fg)) => {
                        row.style(Style::default().bg(Color::Rgb(r, g, b)).fg(fg))
                    }
                    _ => row,
                }
            }
        })
        .collect();

    let widths = [
        Constraint::Length(4),   // #
        Constraint::Min(20),     // Title (Claude's live task summary, varies in length)
        Constraint::Length(22),  // Project (repo + branch)
        Constraint::Length(20),  // Directory
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
