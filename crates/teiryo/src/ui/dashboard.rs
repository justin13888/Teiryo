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
    cells, format_countdown, format_elapsed, format_span, outcome_text, pad_to_cells,
    text_bar_fine, truncate, usage_short,
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
                        window_lines(
                            view,
                            points,
                            status.poll_interval_secs,
                            inner_width,
                            now,
                            two_line,
                        )
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
    poll_interval_secs: u32,
    width: usize,
    now: DateTime<Utc>,
    two_line: bool,
) -> Vec<Line<'static>> {
    // With a line of its own for the derived numbers, the gauge line has no
    // pace column to reserve and the bar keeps those columns instead.
    let mut lines = vec![gauge_line(view, width, now, !two_line)];
    if two_line {
        lines.extend(derived_line(view, points, poll_interval_secs, width, now));
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
    // Padded by cells, not by `format!`'s char count: a label carrying any
    // wide glyph is otherwise padded too little, and every column after it on
    // the row shifts right by the difference.
    spans.push(Span::raw(pad_to_cells(
        &truncate(&window.label, LABEL - 1),
        LABEL,
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
    poll_interval_secs: u32,
    width: usize,
    now: DateTime<Utc>,
) -> Option<Line<'static>> {
    const INDENT: usize = 4;
    const SEPARATOR: &str = " · ";

    let window = &metrics::effective_window(view, now)?;
    let mut fields: Vec<(String, Style)> = Vec::new();

    // Marks the fields measured from the window's start when that start is
    // itself only bracketed — a restart seen across an outage can be days
    // wide. Marked rather than withheld: the figure is still the best there
    // is, and hiding it would lose the signal a wide bracket is usually
    // reporting. `afford` and `now` carry no mark because neither is measured
    // from the start.
    let mark = if metrics::start_is_uncertain(window) {
        "~"
    } else {
        ""
    };

    if let Some(pace) = metrics::pace(window, now) {
        let (glyph, color) = pace_style(pace);
        fields.push((
            format!("{glyph} {mark}{pace:.2}× pace"),
            Style::default().fg(color),
        ));
    }
    let max_gap = metrics::gap_tolerance(poll_interval_secs);
    if let Some(recent) = metrics::recent_pace(window, points, max_gap, now) {
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
            format!("cap in {mark}{}", format_span(runway))
        };
        let color = if binding { theme::WARN } else { theme::DIM };
        fields.push((text, Style::default().fg(color)));
    }
    if let Some(afford) = metrics::affordable_pace(window, now) {
        fields.push((format!("afford {afford:.2}×"), theme::dim()));
    }
    if let Some(pace) = metrics::pace(window, now) {
        fields.push((
            format!("→{mark}{:.0}% at reset", pace * 100.0),
            theme::dim(),
        ));
    }

    let mut spans = vec![Span::raw(" ".repeat(INDENT))];
    let mut used = INDENT;
    for (text, style) in fields {
        // Counted in cells, for consistency with every other budget here
        // rather than to fix a live defect: every field on this line is
        // formatted locally from numbers, and `▲ ▼ × → ·` are all one cell, so
        // today a char count gives the same answer. An earlier pass moved this
        // off `str::len`, which did overstate the separator by a byte. Chars
        // are simply the wrong unit for a budget ratatui charges in cells, and
        // the day a field here carries provider text — the label column
        // already does — the units have to already agree.
        let separator = if used > INDENT { cells(SEPARATOR) } else { 0 };
        let cost = cells(&text) + separator;
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
    match metrics::effective_window(view, now).and_then(|w| metrics::pace(&w, now)) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use teiryo_core::domain::{QuotaUnit, QuotaWindow, ResetKind, WindowId, WindowScope};
    use teiryo_core::rollover::ObservedStart;
    use teiryo_core::{BarStyle, RenderHint};

    use crate::ui::format::cells;

    fn view(label: &str) -> WindowView {
        WindowView {
            window: QuotaWindow {
                id: WindowId::from("weekly_model"),
                label: label.to_owned(),
                scope: WindowScope::Model("model".into()),
                reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(7 * 24 * 3600)),
                unit: QuotaUnit::Percent,
                used: 62.0,
                limit: Some(100.0),
                reset_at: Some(Utc::now() + chrono::Duration::hours(2)),
            },
            hint: RenderHint {
                style: BarStyle::Percent,
                warn_threshold: 0.8,
                critical_threshold: 0.95,
                note: None,
            },
            observed_start: None,
        }
    }

    /// The same window, but with its start known only to a twelve-hour
    /// bracket — wide enough against a seven-day span for
    /// `metrics::start_is_uncertain`, so every field measured from the start
    /// carries the `~` mark and costs a cell more than the plain view's.
    fn uncertain_view(label: &str) -> WindowView {
        WindowView {
            observed_start: Some(ObservedStart {
                not_before: Utc::now() - chrono::Duration::hours(20),
                not_after: Utc::now() - chrono::Duration::hours(8),
            }),
            ..view(label)
        }
    }

    /// The label column is exactly its budget whatever the label is made of.
    ///
    /// Asserted against `gauge_line` rather than against the two helpers it
    /// composes: measuring `pad_to_cells(&truncate(..))` inline proves the
    /// helpers and leaves the call site free to use either one wrongly, which
    /// is what it did — reverting this line to `format!("{:<LABEL$}", ..)` left
    /// every test in the crate green.
    #[test]
    fn the_label_column_holds_its_width_for_any_label() {
        const LABEL: usize = 24;
        for label in [
            "Weekly",
            "Weekly — a fairly long model name that will not fit",
            // A server-supplied model name is the reason any of this matters,
            // and these are the three shapes a char count gets wrong.
            "Weekly — Opus 東京東京東京東京",
            "Weekly — \u{2764}\u{FE0F}\u{2764}\u{FE0F}\u{2764}\u{FE0F}\u{2764}\u{FE0F}",
            "Weekly — Ope\u{301}ra",
        ] {
            let line = gauge_line(&view(label), 120, Utc::now(), false);
            let drawn = cells(&line.spans[1].content);
            assert_eq!(
                drawn, LABEL,
                "label {label:?} drew {drawn} cells in a {LABEL}-cell column"
            );
        }
    }

    /// The derived line's own budget is in cells too, so a wide field costs
    /// what it draws rather than what it counts.
    #[test]
    fn the_derived_line_budgets_in_cells() {
        // Wide enough for every field, then narrow enough that the budget has
        // to shed some: the line must never exceed the width it was given.
        // Both marked and unmarked, since the `~` on a bracketed start costs
        // a cell per field that the budget still has to fit.
        for view in [view("Weekly"), uncertain_view("Weekly")] {
            for width in [120, 60, 44, 30] {
                let line = derived_line(&view, &[], 60, width, Utc::now())
                    .expect("a window with a reset instant has derived numbers");
                let drawn: usize = line.spans.iter().map(|s| cells(&s.content)).sum();
                assert!(drawn <= width, "drew {drawn} cells into {width}");
            }
        }
    }
}
