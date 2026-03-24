//! ratatui rendering: table, hint bar, modeline, profile picker overlay.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::app::{App, ConfirmAction, Mode};

// Cursor indicator appended to the palette input line.
const CURSOR: &str = "▏";

// ---------------------------------------------------------------------------
// Top-level render entry point
// ---------------------------------------------------------------------------

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    // Vertical split: [table area] [hint bar] [modeline]
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // table (all remaining space)
            Constraint::Length(1), // hint bar
            Constraint::Length(1), // modeline
        ])
        .split(area);

    render_table(f, app, chunks[0]);
    render_hint_bar(f, app, chunks[1]);
    render_modeline(f, app, chunks[2]);

    if app.mode == Mode::ProfilePicker {
        render_profile_picker(f, app, area);
    }
}

// ---------------------------------------------------------------------------
// Container table
// ---------------------------------------------------------------------------

fn render_table(f: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(vec![
        Cell::from("NAME").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("STATUS").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("IMAGE").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("PORTS").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("UPTIME").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .height(1);

    let rows: Vec<Row> = app
        .containers
        .iter()
        .map(|c| {
            let status_style = match c.status.as_str() {
                "running" => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
            };

            let uptime = format_uptime(&c.started_at);

            // Prefix the name with a selection marker when Space-selected.
            let name_cell = if app.selected_names.contains(&c.name) {
                Cell::from(format!("■ {}", c.name)).style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Cell::from(c.name.as_str())
            };

            // Format ports as "8080→80, 443→443".
            let ports_str = c
                .ports
                .iter()
                .map(|s| s.replacen(':', "→", 1))
                .collect::<Vec<_>>()
                .join(", ");

            Row::new(vec![
                name_cell,
                Cell::from(c.status.as_str()).style(status_style),
                Cell::from(c.rootfs.as_str()),
                Cell::from(ports_str).style(Style::default().fg(Color::Cyan)),
                Cell::from(uptime),
            ])
            .height(1)
        })
        .collect();

    // Build a TableState so ratatui knows which row to highlight.
    let mut table_state = TableState::default();
    if !app.containers.is_empty() {
        table_state.select(Some(app.selected));
    }

    let title = if app.show_all {
        " pelagos — all containers "
    } else {
        " pelagos "
    };

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(23), // NAME
            Constraint::Percentage(10), // STATUS
            Constraint::Percentage(37), // IMAGE
            Constraint::Percentage(15), // PORTS
            Constraint::Percentage(15), // UPTIME
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▶ ");

    f.render_stateful_widget(table, area, &mut table_state);

    // Empty state message when no containers are present.
    if app.containers.is_empty() {
        let msg = if app.vm_running {
            if app.show_all {
                "No containers found. Use 'pelagos run' to start one."
            } else {
                "No containers running. Press 'a' to show all, or use 'pelagos run'."
            }
        } else {
            "VM is stopped. Use 'pelagos vm start' to start it."
        };

        // Centre the message inside the table block (subtract borders).
        let inner = Rect {
            x: area.x + 1,
            y: area.y + area.height / 2,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        let p = Paragraph::new(msg).style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, inner);
    }
}

// ---------------------------------------------------------------------------
// Hint bar
// ---------------------------------------------------------------------------

fn render_hint_bar(f: &mut Frame, app: &App, area: Rect) {
    let text = match app.mode {
        Mode::CommandPalette => "  [Enter]run  [Esc]cancel",
        Mode::Confirm => "  confirm action: [y]yes  [any]cancel",
        Mode::ConfirmQuit => "  quit pelagos-tui: [y/q]yes  [any]cancel",
        _ => "  [q]quit  [a]all  [j/k]nav  [Space]sel  [s]stop  [S]restart  [d]rm  [P]prune  [r]run  [p]profile",
    };
    let hints = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(hints, area);
}

// ---------------------------------------------------------------------------
// Modeline
// ---------------------------------------------------------------------------

fn render_modeline(f: &mut Frame, app: &App, area: Rect) {
    // Transient error/status from the last run command.
    if let Some(msg) = &app.status_message {
        let spans = vec![
            Span::styled(
                "  ! ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(msg.as_str(), Style::default().fg(Color::Yellow)),
        ];
        let modeline = Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Black).fg(Color::White));
        f.render_widget(modeline, area);
        return;
    }

    // Quit confirmation prompt.
    if app.mode == Mode::ConfirmQuit {
        let spans = vec![
            Span::styled(
                "  quit pelagos-tui?  ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("[y/q/N] ", Style::default().fg(Color::Yellow)),
        ];
        let modeline = Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Black).fg(Color::White));
        f.render_widget(modeline, area);
        return;
    }

    // In confirm mode the modeline shows the action + target count prompt.
    if app.mode == Mode::Confirm {
        if let Some(action) = &app.confirm_action {
            let count = app.confirm_targets.len();
            let noun = if count == 1 {
                "container"
            } else {
                "containers"
            };
            let action_color = match action {
                ConfirmAction::Remove => Color::Red,
                ConfirmAction::Stop => Color::Yellow,
                ConfirmAction::Restart => Color::Cyan,
            };
            let spans = vec![
                Span::styled(
                    format!("  {} ", action.verb()),
                    Style::default()
                        .fg(action_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{} {}?  ", count, noun),
                    Style::default().fg(Color::White),
                ),
                Span::styled("[y/N] ", Style::default().fg(Color::Yellow)),
            ];
            let modeline = Paragraph::new(Line::from(spans))
                .style(Style::default().bg(Color::Black).fg(Color::White));
            f.render_widget(modeline, area);
            return;
        }
    }

    // In command palette mode the modeline becomes an input field.
    if app.mode == Mode::CommandPalette {
        let spans = vec![
            Span::styled(
                "  run> ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                app.palette_input.as_str(),
                Style::default().fg(Color::White),
            ),
            Span::styled(CURSOR, Style::default().fg(Color::Yellow)),
        ];
        let modeline = Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Black).fg(Color::White));
        f.render_widget(modeline, area);
        return;
    }

    let vm_text = if app.vm_running { "running" } else { "stopped" };
    let vm_color = if app.vm_running {
        Color::Cyan
    } else {
        Color::Red
    };

    let age = app.refresh_age_secs();
    let age_str = if age == 0 {
        "just now".to_string()
    } else if age == 1 {
        "1s ago".to_string()
    } else {
        format!("{}s ago", age)
    };

    let total = app.containers.len();
    let running = app
        .containers
        .iter()
        .filter(|c| c.status == "running")
        .count();
    let container_str = format!("{}/{} running", running, total);
    let container_color = if total == 0 || running == 0 {
        Color::DarkGray
    } else if running == total {
        Color::White
    } else {
        Color::Yellow
    };

    let spans = vec![
        Span::styled(
            format!("  {} ", app.profile),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(Color::White)),
        Span::styled("vm ", Style::default().fg(Color::Gray)),
        Span::styled(vm_text, Style::default().fg(vm_color)),
        Span::styled(" │ ", Style::default().fg(Color::White)),
        Span::styled(container_str, Style::default().fg(container_color)),
        Span::styled(" │ ", Style::default().fg(Color::White)),
        Span::styled(
            format!("↻ {}  ", age_str),
            Style::default().fg(Color::DarkGray),
        ),
    ];

    let modeline =
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black).fg(Color::White));
    f.render_widget(modeline, area);
}

// ---------------------------------------------------------------------------
// Profile picker overlay
// ---------------------------------------------------------------------------

fn render_profile_picker(f: &mut Frame, app: &App, area: Rect) {
    // Determine popup dimensions.
    let max_name_len = app.profiles.iter().map(|p| p.len()).max().unwrap_or(10);
    let popup_width = (max_name_len as u16 + 6).max(24).min(area.width - 4);
    let popup_height = (app.profiles.len() as u16 + 4).min(area.height - 4);

    // Centre the popup.
    let popup_x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    // Erase the area under the popup before drawing.
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Switch profile ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Render profile list rows.
    let rows: Vec<Row> = app
        .profiles
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let style = if i == app.profile_picker_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if name == &app.profile {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new(vec![Cell::from(name.as_str())]).style(style)
        })
        .collect();

    let mut picker_state = TableState::default();
    picker_state.select(Some(app.profile_picker_selected));

    let picker_table = Table::new(rows, [Constraint::Percentage(100)])
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(picker_table, inner, &mut picker_state);
}

// ---------------------------------------------------------------------------
// Uptime formatting
// ---------------------------------------------------------------------------

/// Format an ISO 8601 timestamp as a human-readable uptime string.
///
/// Parses the subset `YYYY-MM-DDTHH:MM:SSZ` that `pelagos` always emits.
/// Falls back to the raw string on any parse error.
fn format_uptime(started_at: &str) -> String {
    if let Some(secs) = parse_iso8601_secs(started_at) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let elapsed = now.saturating_sub(secs);
        format_duration(elapsed)
    } else {
        started_at.to_string()
    }
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` into Unix seconds.  Returns None on any error.
fn parse_iso8601_secs(s: &str) -> Option<u64> {
    // Expected: 20 chars minimum: "YYYY-MM-DDTHH:MM:SSZ"
    let s = s.trim_end_matches('Z');
    let (date_part, time_part) = s.split_once('T')?;
    let mut d = date_part.split('-');
    let year: u64 = d.next()?.parse().ok()?;
    let month: u64 = d.next()?.parse().ok()?;
    let day: u64 = d.next()?.parse().ok()?;

    let mut t = time_part.split(':');
    let hour: u64 = t.next()?.parse().ok()?;
    let min: u64 = t.next()?.parse().ok()?;
    let sec: u64 = t.next()?.parse().ok()?;

    // Days from epoch to start of year (Gregorian calendar, no chrono needed).
    let days = days_from_epoch(year, month, day)?;
    let secs = days * 86400 + hour * 3600 + min * 60 + sec;
    Some(secs)
}

/// Convert a Gregorian date to days since 1970-01-01.
fn days_from_epoch(year: u64, month: u64, day: u64) -> Option<u64> {
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days in each month (non-leap year).
    const DAYS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let mut days: u64 = (year - 1970) * 365;
    // Leap years between 1970 and year-1.
    days += leap_years_between(1970, year);
    for m in 1..month {
        days += DAYS[(m - 1) as usize];
        // Add a day for February if current year is a leap year.
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;
    Some(days)
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

/// Count leap years in [from, to).
fn leap_years_between(from: u64, to: u64) -> u64 {
    if to <= from {
        return 0;
    }
    let count = |y: u64| -> u64 { y / 4 - y / 100 + y / 400 };
    count(to - 1) - count(from - 1)
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {:02}m", h, m)
    } else {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        format!("{}d {:02}h", d, h)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3599), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600), "1h 00m");
        assert_eq!(format_duration(3661), "1h 01m");
    }

    #[test]
    fn format_duration_days() {
        assert_eq!(format_duration(86400), "1d 00h");
        assert_eq!(format_duration(90061), "1d 01h");
    }

    #[test]
    fn parse_iso8601_known_epoch() {
        // 1970-01-01T00:00:00Z = 0
        assert_eq!(parse_iso8601_secs("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_iso8601_known_date() {
        // 2026-01-01T00:00:00Z — pre-computed: 20454 days from epoch.
        let secs = parse_iso8601_secs("2026-01-01T00:00:00Z").expect("parse");
        let days = secs / 86400;
        assert_eq!(days, 20454);
    }

    #[test]
    fn parse_iso8601_bad_input() {
        assert!(parse_iso8601_secs("not-a-date").is_none());
        assert!(parse_iso8601_secs("").is_none());
    }
}
