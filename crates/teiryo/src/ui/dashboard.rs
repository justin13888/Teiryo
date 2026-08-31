//! The header strip and the scrollable quota gauge list.

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

use teiryo_core::{AccountStatus, QuotaSnapshot, WindowView};

use crate::app::{App, Pane, RowRef};
use crate::metrics;
use crate::ui::format::{
    format_countdown, format_elapsed, format_span, outcome_text, text_bar_fine, truncate,
    usage_short,
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
        Paragraph::new(vec![headroom_line(app), notice_line(app)]).alignment(Alignment::Right),
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
fn notice_line(app: &App) -> Line<'static> {
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
        // The projected cap used to live here, where it competed with the
        // provider's caveat for one line and won. Every row now carries its
        // own runway, so the caveat gets the line back.
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
    let inner = block.inner(area);
    let inner_width = inner.width as usize;

    let rows = app.rows();
    // The derived numbers get a line of their own only while every row can
    // have one: bars survive, continuation lines go first.
    let window_rows = rows.iter().filter(|r| r.window.is_some()).count();
    let two_line = rows.len() + window_rows <= inner.height as usize;
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
                    RowRef { window: None, .. } => vec![account_line(status, now)],
                    RowRef {
                        window: Some(wi), ..
                    } => {
                        let view = &status.windows[*wi];
                        let points = app.recent_points(&status.account.id, &view.window.id);
                        window_lines(view, points, inner_width, now, two_line)
                    }
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

/// One window row: the gauge line, and under it — when the pane is tall enough
/// — the line of derived numbers.
fn window_lines(
    view: &WindowView,
    points: &[QuotaSnapshot],
    width: usize,
    now: DateTime<Utc>,
    two_line: bool,
) -> Vec<Line<'static>> {
    // With a line of its own for the derived numbers, the gauge line has no
    // pace column to reserve and the bar keeps those columns instead.
    let mut lines = vec![gauge_line(view, width, now, !two_line)];
    if two_line {
        lines.extend(derived_line(view, points, width, now));
    }
    lines
}

/// The gauge line: label, bar, usage, reset countdown, and — only when the
/// derived numbers have no line of their own — pace.
fn gauge_line(
    view: &WindowView,
    width: usize,
    now: DateTime<Utc>,
    with_pace: bool,
) -> Line<'static> {
    const INDENT: usize = 2;
    const LABEL: usize = 24;
    const USAGE: usize = 7;
    const RESET: usize = 11;
    const PACE: usize = 15;

    let window = &view.window;
    // Give the bar whatever the fixed columns do not need, and drop the pace
    // column entirely before letting the bar shrink into illegibility.
    let fixed = INDENT + LABEL + USAGE + RESET;
    let show_pace = with_pace && width >= fixed + PACE + MIN_BAR;
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

/// The continuation line: what the gauge cannot say — how fast the window is
/// going, how long that lasts, and how fast it could afford to go.
///
/// Fields are appended left to right only while they fit, so a narrow terminal
/// sheds them from the right. The order is by how much each one adds: the two
/// burn rates first and together, since the whole point of the recent one is
/// being read against the average; then the runway that says what the pace
/// costs; then the pace still affordable; and last the projection — which is
/// *numerically identical* to pace (projected use at reset is
/// `u + (u/E)(1-E) = u/E`) and so is the field worth losing first.
///
/// `None` when the window publishes no `reset_at`: every one of these is
/// derived from the window's own start, so there is nothing to say.
fn derived_line(
    view: &WindowView,
    points: &[QuotaSnapshot],
    width: usize,
    now: DateTime<Utc>,
) -> Option<Line<'static>> {
    const INDENT: usize = 4;
    const SEPARATOR: &str = " · ";

    let window = &view.window;
    let mut fields: Vec<(String, Style)> = Vec::new();

    if let Some(pace) = metrics::pace(window, now) {
        let (glyph, color) = pace_style(pace);
        fields.push((
            format!("{glyph} {pace:.2}× pace"),
            Style::default().fg(color),
        ));
    }
    if let Some(recent) = metrics::recent_pace(window, points, now) {
        let (glyph, color) = pace_style(recent);
        fields.push((
            format!("{glyph} {recent:.2}× now"),
            Style::default().fg(color),
        ));
    }
    if let Some(runway) = metrics::runway(window, now) {
        // Warn only when the cap is what actually arrives first. A runway
        // longer than the window has left is information, not an alarm — the
        // rollover gets there before the cap does.
        let binding = metrics::eta_to_cap(window, now).is_some();
        let text = if runway <= chrono::Duration::zero() {
            "at cap".to_owned()
        } else {
            format!("cap in {}", format_span(runway))
        };
        let color = if binding { theme::WARN } else { theme::DIM };
        fields.push((text, Style::default().fg(color)));
    }
    if let Some(afford) = metrics::affordable_pace(window, now) {
        fields.push((format!("afford {afford:.2}×"), theme::dim()));
    }
    if let Some(pace) = metrics::pace(window, now) {
        fields.push((format!("→{:.0}% at reset", pace * 100.0), theme::dim()));
    }

    let mut spans = vec![Span::raw(" ".repeat(INDENT))];
    let mut used = INDENT;
    for (text, style) in fields {
        // Counted in cells, not bytes: the separator's "·" is two bytes wide
        // and one column, and so are the glyphs inside the fields.
        let separator = if used > INDENT {
            SEPARATOR.chars().count()
        } else {
            0
        };
        let cost = text.chars().count() + separator;
        if used + cost > width {
            break;
        }
        if used > INDENT {
            spans.push(Span::styled(SEPARATOR, theme::dim()));
        }
        used += cost;
        spans.push(Span::styled(text, style));
    }
    (spans.len() > 1).then(|| Line::from(spans))
}

/// Glyph and colour for a burn rate, whether it is the average since the
/// window opened or a rate measured over a shorter stretch.
fn pace_style(pace: f64) -> (&'static str, ratatui::style::Color) {
    if pace >= 1.5 {
        ("▲", theme::CRIT)
    } else if pace >= 1.05 {
        ("▲", theme::WARN)
    } else if pace <= 0.8 {
        ("▼", theme::OK)
    } else {
        ("=", theme::DIM)
    }
}

/// "▲ 1.3× pace" / "▼ 0.5× pace" — usage measured against the clock.
fn pace_span(view: &WindowView, now: DateTime<Utc>) -> Span<'static> {
    match metrics::pace(&view.window, now) {
        Some(pace) => {
            let (glyph, color) = pace_style(pace);
            Span::styled(
                format!("  {glyph} {pace:.2}× pace"),
                Style::default().fg(color),
            )
        }
        None => Span::raw(""),
    }
}
