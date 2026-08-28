//! Rendering and pure formatting helpers.

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Sparkline};
use ratatui::Frame;

use teiryo_core::domain::{PollOutcome, QuotaUnit, QuotaWindow};
use teiryo_core::PollEvent;

use crate::app::{App, RowRef, View};

/// Utilization ratio in `0.0..=1.0`, when computable from the window's data.
pub fn utilization(window: &QuotaWindow) -> Option<f64> {
    match (window.unit, window.limit) {
        (QuotaUnit::Percent, _) => Some((window.used / 100.0).clamp(0.0, 1.0)),
        (_, Some(limit)) if limit > 0.0 => Some((window.used / limit).clamp(0.0, 1.0)),
        _ => None,
    }
}

/// Humanized countdown to a reset instant: "2h 05m", "3d 4h", "41s", "due".
pub fn format_countdown(reset_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (reset_at - now).num_seconds();
    if secs <= 0 {
        return "due".to_owned();
    }
    let (d, h, m, s) = (
        secs / 86_400,
        (secs / 3_600) % 24,
        (secs / 60) % 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Fixed-width text bar: `ratio` filled with `█`, the rest `░`.
pub fn text_bar(ratio: f64, width: usize) -> String {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    let mut bar = String::with_capacity(width * 3);
    for _ in 0..filled {
        bar.push('█');
    }
    for _ in filled..width {
        bar.push('░');
    }
    bar
}

/// Usage text for a window, e.g. "42% used", "37/80 messages".
pub fn usage_text(window: &QuotaWindow) -> String {
    let unit = match window.unit {
        QuotaUnit::Percent => "%",
        QuotaUnit::Messages => " messages",
        QuotaUnit::Tokens => " tokens",
        QuotaUnit::Hours => " hours",
    };
    match (window.unit, window.limit) {
        (QuotaUnit::Percent, _) => format!("{:.0}% used", window.used),
        (_, Some(limit)) => format!("{:.0}/{:.0}{unit}", window.used, limit),
        (_, None) => format!("{:.0}{unit} used", window.used),
    }
}

fn ratio_color(ratio: f64) -> Color {
    if ratio >= 0.95 {
        Color::Red
    } else if ratio >= 0.8 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn poll_outcome_text(event: &PollEvent) -> (String, Color) {
    match &event.outcome {
        PollOutcome::Success { .. } => ("ok".into(), Color::Green),
        other => (
            other.error_message().unwrap_or("error").to_owned(),
            Color::Red,
        ),
    }
}

/// Top-level render entry point.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let [body, status_line] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    match &app.view {
        View::Dashboard => render_dashboard(frame, body, app),
        View::History { title, snapshots } => render_history(frame, body, title, snapshots),
        View::RecentPolls(events) => render_recent(frame, body, events),
        View::Providers(health) => render_providers(frame, body, health),
        View::ConfirmShutdown => render_confirm(frame, body),
    }
    render_status_line(frame, status_line, app);
}

fn render_dashboard(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let now = Utc::now();
    let rows = app.rows();
    let mut items: Vec<ListItem<'_>> = Vec::with_capacity(rows.len().max(1));
    let mut last_provider: Option<&str> = None;

    for (i, row) in rows.iter().enumerate() {
        let status = &app.statuses[row.account];
        let selected = i == app.selected;
        let line = match row {
            RowRef { window: None, .. } => {
                let mut spans = Vec::new();
                if last_provider != Some(status.account.provider.as_str()) {
                    last_provider = Some(status.account.provider.as_str());
                }
                spans.push(Span::styled(
                    format!("{} / {}", status.account.provider, status.account.label),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                match &status.last_poll {
                    Some(event) => {
                        let (text, color) = poll_outcome_text(event);
                        spans.push(Span::raw("  — last poll "));
                        spans.push(Span::styled(text, Style::default().fg(color)));
                        spans.push(Span::raw(format!(
                            " ({} ago)",
                            format_countdown(now, event.ts) // elapsed: reversed args
                        )));
                    }
                    None => spans.push(Span::styled(
                        "  — not polled yet",
                        Style::default().fg(Color::DarkGray),
                    )),
                }
                Line::from(spans)
            }
            RowRef {
                window: Some(wi), ..
            } => {
                let window = &status.windows[*wi].window;
                let mut spans = vec![Span::raw("  ")];
                match utilization(window) {
                    Some(ratio) => {
                        spans.push(Span::styled(
                            text_bar(ratio, 20),
                            Style::default().fg(ratio_color(ratio)),
                        ));
                    }
                    None => spans.push(Span::styled(
                        "····················",
                        Style::default().fg(Color::DarkGray),
                    )),
                }
                spans.push(Span::raw(format!(
                    "  {} — {}",
                    window.label,
                    usage_text(window)
                )));
                if let Some(reset_at) = window.reset_at {
                    spans.push(Span::styled(
                        format!("  resets in {}", format_countdown(reset_at, now)),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                Line::from(spans)
            }
        };
        let item = if selected {
            ListItem::new(line).style(Style::default().bg(Color::Rgb(40, 40, 60)))
        } else {
            ListItem::new(line)
        };
        items.push(item);
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "no accounts discovered yet — waiting for the daemon",
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Teiryō — subscription quotas "),
    );
    frame.render_widget(list, area);
}

fn render_history(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    snapshots: &[teiryo_core::QuotaSnapshot],
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" History — {title} (Esc to close) "));
    if snapshots.is_empty() {
        frame.render_widget(
            Paragraph::new("no history in the last 24h").block(block),
            area,
        );
        return;
    }
    // Sparkline needs u64s; scale used values into 0..=100.
    let max = snapshots.iter().map(|s| s.used).fold(f64::MIN, f64::max);
    let scale = if max > 0.0 { 100.0 / max } else { 1.0 };
    let data: Vec<u64> = snapshots
        .iter()
        .map(|s| (s.used * scale).round() as u64)
        .collect();
    let latest = snapshots.last().expect("non-empty");
    let sparkline = Sparkline::default()
        .block(block.title_bottom(format!(
            " {} points, latest {} max {:.0} ",
            snapshots.len(),
            usage_text_snapshot(latest),
            max
        )))
        .data(&data)
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(sparkline, area);
}

fn usage_text_snapshot(snapshot: &teiryo_core::QuotaSnapshot) -> String {
    match snapshot.limit {
        Some(limit) => format!("{:.0}/{:.0}", snapshot.used, limit),
        None => format!("{:.0}", snapshot.used),
    }
}

fn render_recent(frame: &mut Frame<'_>, area: Rect, events: &[PollEvent]) {
    let items: Vec<ListItem<'_>> = events
        .iter()
        .map(|event| {
            let (text, color) = poll_outcome_text(event);
            ListItem::new(Line::from(vec![
                Span::raw(format!(
                    "{}  {}/{}  ",
                    event.ts.format("%H:%M:%S"),
                    event.provider,
                    event.account
                )),
                Span::styled(text, Style::default().fg(color)),
                Span::styled(
                    format!("  {}ms", event.latency_ms),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Recent polls (Esc to close) "),
    );
    frame.render_widget(list, area);
}

fn render_providers(frame: &mut Frame<'_>, area: Rect, health: &[teiryo_core::ProviderHealth]) {
    let items: Vec<ListItem<'_>> = health
        .iter()
        .map(|h| {
            let color = if h.consecutive_failures == 0 {
                Color::Green
            } else {
                Color::Red
            };
            let mut text = format!(
                "{}  accounts: {}  consecutive failures: {}",
                h.provider,
                h.accounts.len(),
                h.consecutive_failures
            );
            if let Some(err) = &h.last_error {
                text.push_str(&format!("  last error: {err}"));
            }
            ListItem::new(Line::from(Span::styled(text, Style::default().fg(color))))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Provider health (Esc to close) "),
    );
    frame.render_widget(list, area);
}

fn render_confirm(frame: &mut Frame<'_>, area: Rect) {
    let paragraph = Paragraph::new("Stop the daemon? Quota polling will halt until the next launch.\n\nPress y to confirm, any other key to cancel.")
        .block(Block::default().borders(Borders::ALL).title(" Stop daemon "));
    frame.render_widget(paragraph, area);
}

fn render_status_line(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let line = if let Some(error) = &app.error {
        Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(Color::Red),
        ))
    } else if app.disconnected {
        Line::from(Span::styled(
            " daemon unreachable — reconnecting…",
            Style::default().fg(Color::Yellow),
        ))
    } else {
        let updated = app
            .last_update
            .map(|ts| format!("updated {} ago", format_countdown(Utc::now(), ts)))
            .unwrap_or_else(|| "no updates yet".to_owned());
        Line::from(vec![
            Span::styled(
                " r poll  R poll all  h history  l log  p providers  q quit  Q stop daemon ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(format!("| {updated}"), Style::default().fg(Color::DarkGray)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use teiryo_core::domain::{ResetKind, WindowId, WindowScope};

    fn window(unit: QuotaUnit, used: f64, limit: Option<f64>) -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from("w"),
            label: "w".into(),
            scope: WindowScope::AccountWide,
            reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(3600)),
            unit,
            used,
            limit,
            reset_at: None,
        }
    }

    #[test]
    fn utilization_from_percent_and_limits() {
        assert_eq!(
            utilization(&window(QuotaUnit::Percent, 42.0, None)),
            Some(0.42)
        );
        assert_eq!(
            utilization(&window(QuotaUnit::Messages, 30.0, Some(60.0))),
            Some(0.5)
        );
        assert_eq!(utilization(&window(QuotaUnit::Tokens, 30.0, None)), None);
        // Overuse clamps rather than overflowing the bar.
        assert_eq!(
            utilization(&window(QuotaUnit::Percent, 150.0, None)),
            Some(1.0)
        );
    }

    #[test]
    fn countdown_formats() {
        let now = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap();
        let at = |secs: i64| now + chrono::Duration::seconds(secs);
        assert_eq!(format_countdown(at(30), now), "30s");
        assert_eq!(format_countdown(at(125), now), "2m 05s");
        assert_eq!(format_countdown(at(2 * 3600 + 300), now), "2h 05m");
        assert_eq!(format_countdown(at(3 * 86_400 + 4 * 3600), now), "3d 4h");
        assert_eq!(format_countdown(at(-5), now), "due");
    }

    #[test]
    fn text_bar_fills_proportionally() {
        assert_eq!(text_bar(0.0, 4), "░░░░");
        assert_eq!(text_bar(0.5, 4), "██░░");
        assert_eq!(text_bar(1.0, 4), "████");
        assert_eq!(text_bar(2.0, 4), "████"); // clamped
    }

    #[test]
    fn usage_text_by_unit() {
        assert_eq!(
            usage_text(&window(QuotaUnit::Percent, 42.4, None)),
            "42% used"
        );
        assert_eq!(
            usage_text(&window(QuotaUnit::Messages, 30.0, Some(60.0))),
            "30/60 messages"
        );
        assert_eq!(
            usage_text(&window(QuotaUnit::Hours, 3.0, None)),
            "3 hours used"
        );
    }
}
