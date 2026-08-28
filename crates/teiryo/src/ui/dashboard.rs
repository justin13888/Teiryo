//! The header strip and the scrollable quota gauge list.

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

use teiryo_core::{AccountStatus, WindowView};

use crate::app::{App, Pane, RowRef};
use crate::metrics;
use crate::ui::format::{
    format_countdown, format_elapsed, outcome_text, text_bar_fine, truncate, usage_short,
};
use crate::ui::theme;

/// Render the two-line identity/connection header.
pub fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App, now: DateTime<Utc>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::BORDER));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(inner);

    frame.render_widget(
        Paragraph::new(vec![identity_line(app), connection_line(app, now)]),
        left,
    );
    frame.render_widget(
        Paragraph::new(vec![headroom_line(app), notice_line(app, now)]).alignment(Alignment::Right),
        right,
    );
}

/// "Teiryō · claude/default · 4 windows"
fn identity_line(app: &App) -> Line<'static> {
    let mut spans = vec![Span::styled("Teiryō", theme::heading())];
    let accounts = app.statuses.len();
    let windows: usize = app.statuses.iter().map(|s| s.windows.len()).sum();
    if accounts == 0 {
        spans.push(Span::styled("  waiting for the daemon", theme::dim()));
        return Line::from(spans);
    }
    let names: Vec<String> = app
        .statuses
        .iter()
        .map(|s| format!("{}/{}", s.account.provider, s.account.label))
        .collect();
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        truncate(&names.join(", "), 40),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!("  {windows} window{}", if windows == 1 { "" } else { "s" }),
        theme::dim(),
    ));
    Line::from(spans)
}

/// Connection dot, staleness, and the next scheduled poll.
fn connection_line(app: &App, now: DateTime<Utc>) -> Line<'static> {
    if app.disconnected {
        return Line::from(Span::styled(
            "◌ daemon unreachable — reconnecting…",
            Style::default().fg(theme::WARN),
        ));
    }
    let mut spans = vec![
        Span::styled("● ", Style::default().fg(theme::OK)),
        Span::styled("live", theme::dim()),
    ];
    if !app.polling.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{} polling", theme::spinner_frame(app.spinner)),
            Style::default().fg(theme::ACCENT),
        ));
    }
    match app.last_update {
        Some(ts) => spans.push(Span::styled(
            format!("  updated {}", format_elapsed(ts, now)),
            theme::dim(),
        )),
        None => spans.push(Span::styled("  no updates yet", theme::dim())),
    }
    if let Some(next) = next_poll_at(app, now) {
        spans.push(Span::styled(
            format!("  next ~{}", format_countdown(next, now)),
            theme::dim(),
        ));
    }
    Line::from(spans)
}

/// The soonest expected next poll across all accounts. Approximate: the
/// scheduler jitters each cycle by ±10%, so this is a hint, not a promise.
fn next_poll_at(app: &App, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    app.statuses
        .iter()
        .filter(|s| s.poll_interval_secs > 0)
        .filter_map(|s| {
            let last = s.last_poll.as_ref()?.ts;
            Some(last + chrono::Duration::seconds(i64::from(s.poll_interval_secs)))
        })
        .filter(|ts| *ts > now)
        .min()
}

/// The account-wide worst window, as a compact gauge — the one number worth
/// seeing without reading the list.
fn headroom_line(app: &App) -> Line<'static> {
    let Some((view, ratio)) = worst_window(app) else {
        return Line::from(Span::styled("no quota data yet", theme::dim()));
    };
    Line::from(vec![
        Span::styled(
            format!("{} ", truncate(&view.window.label, 22)),
            theme::dim(),
        ),
        Span::styled(
            text_bar_fine(ratio, 12),
            Style::default().fg(theme::severity(ratio, &view.hint)),
        ),
        Span::styled(
            format!(" {:>4}", usage_short(&view.window)),
            Style::default()
                .fg(theme::severity(ratio, &view.hint))
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Highest-utilization window across every account.
fn worst_window(app: &App) -> Option<(&WindowView, f64)> {
    app.statuses
        .iter()
        .flat_map(|s| s.windows.iter())
        .filter_map(|view| metrics::utilization(&view.window).map(|r| (view, r)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// Error text, else the selected window's provider caveat.
fn notice_line(app: &App, now: DateTime<Utc>) -> Line<'static> {
    if let Some(error) = &app.error {
        return Line::from(Span::styled(
            truncate(error, 60),
            Style::default().fg(theme::CRIT),
        ));
    }
    // A rejected config.toml has to be visible here, not only inside the
    // Settings overlay: otherwise the daemon looks like it is quietly ignoring
    // the file the user just edited.
    if let Some(error) = app.config_error() {
        return Line::from(Span::styled(
            truncate(&format!("✗ config not applied: {error} — press s"), 60),
            Style::default().fg(theme::CRIT),
        ));
    }
    if let Some((_, view)) = app.selected_window() {
        if let Some(eta) = metrics::eta_to_cap(&view.window, now) {
            return Line::from(Span::styled(
                format!("⚡ projected to cap in {}", format_countdown(eta, now)),
                Style::default().fg(theme::WARN),
            ));
        }
        if let Some(note) = &view.hint.note {
            return Line::from(Span::styled(truncate(note, 60), theme::dim()));
        }
    }
    Line::from(Span::raw(""))
}

/// Render the account/window list with per-window gauges.
pub fn render_quotas(frame: &mut Frame<'_>, area: Rect, app: &mut App, now: DateTime<Utc>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border(app.focus == Pane::List))
        .title(Span::styled(" Quotas ", theme::heading()));
    let inner_width = block.inner(area).width as usize;

    let rows = app.rows();
    let items: Vec<ListItem<'_>> = if rows.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no accounts discovered yet — waiting for the daemon",
            theme::dim(),
        )))]
    } else {
        rows.iter()
            .map(|row| {
                let status = &app.statuses[row.account];
                ListItem::new(match row {
                    RowRef { window: None, .. } => account_line(status, now),
                    RowRef {
                        window: Some(wi), ..
                    } => window_line(&status.windows[*wi], inner_width, now),
                })
            })
            .collect()
    };

    let list = List::new(items)
        .block(block)
        .highlight_style(theme::selected());
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

/// Account header row: identity, last poll outcome, staleness of the data.
fn account_line(status: &AccountStatus, now: DateTime<Utc>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{}/{}", status.account.provider, status.account.label),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    match &status.last_poll {
        Some(event) => {
            let (text, failed) = outcome_text(event);
            let color = if failed { theme::CRIT } else { theme::OK };
            spans.push(Span::styled("  last poll ", theme::dim()));
            spans.push(Span::styled(
                truncate(&text, 44),
                Style::default().fg(color),
            ));
            spans.push(Span::styled(
                format!("  {}", format_elapsed(event.ts, now)),
                theme::dim(),
            ));
            spans.push(Span::styled(
                format!("  {}ms", event.latency_ms),
                theme::dim(),
            ));
            // When the newest poll failed, the gauges below it are from an
            // older successful one — say so rather than showing them as live.
            if failed {
                if let Some(ts) = status.last_success {
                    spans.push(Span::styled(
                        format!("  · showing data from {}", format_elapsed(ts, now)),
                        Style::default().fg(theme::WARN),
                    ));
                }
            }
        }
        None => spans.push(Span::styled("  not polled yet", theme::dim())),
    }
    Line::from(spans)
}

/// One window row: label, gauge, usage, reset countdown, pace.
fn window_line(view: &WindowView, width: usize, now: DateTime<Utc>) -> Line<'static> {
    const INDENT: usize = 2;
    const LABEL: usize = 24;
    const USAGE: usize = 7;
    const RESET: usize = 11;
    const PACE: usize = 15;

    let window = &view.window;
    // Give the bar whatever the fixed columns do not need, and drop the pace
    // column entirely before letting the bar shrink into illegibility.
    let fixed = INDENT + LABEL + USAGE + RESET;
    let show_pace = width >= fixed + PACE + MIN_BAR;
    let bar_width = width
        .saturating_sub(fixed + if show_pace { PACE } else { 0 })
        .clamp(MIN_BAR, 48);

    let mut spans = vec![Span::raw(" ".repeat(INDENT))];
    spans.push(Span::raw(format!(
        "{:<LABEL$}",
        truncate(&window.label, LABEL - 1)
    )));

    match metrics::utilization(window) {
        Some(ratio) => {
            let color = theme::severity(ratio, &view.hint);
            spans.push(Span::styled(
                text_bar_fine(ratio, bar_width),
                Style::default().fg(color),
            ));
            spans.push(Span::styled(
                format!("{:>USAGE$}", usage_short(window)),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
        None => {
            // No limit published: there is no ratio to draw, only a count.
            spans.push(Span::styled("·".repeat(bar_width), theme::dim()));
            spans.push(Span::styled(
                format!("{:>USAGE$}", usage_short(window)),
                theme::dim(),
            ));
        }
    }

    match window.reset_at {
        Some(reset_at) => spans.push(Span::styled(
            format!(
                "{:>RESET$}",
                format!("⟳ {}", format_countdown(reset_at, now))
            ),
            theme::dim(),
        )),
        None => spans.push(Span::raw(" ".repeat(RESET))),
    }

    if show_pace {
        spans.push(pace_span(view, now));
    }
    Line::from(spans)
}

/// Narrowest bar still worth drawing.
const MIN_BAR: usize = 8;

/// "▲ 1.3× pace" / "▼ 0.5× pace" — usage measured against the clock.
fn pace_span(view: &WindowView, now: DateTime<Utc>) -> Span<'static> {
    match metrics::pace(&view.window, now) {
        Some(pace) => {
            let (glyph, color) = if pace >= 1.5 {
                ("▲", theme::CRIT)
            } else if pace >= 1.05 {
                ("▲", theme::WARN)
            } else if pace <= 0.8 {
                ("▼", theme::OK)
            } else {
                ("=", theme::DIM)
            };
            Span::styled(
                format!("  {glyph} {pace:.2}× pace"),
                Style::default().fg(color),
            )
        }
        None => Span::raw(""),
    }
}
