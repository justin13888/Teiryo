//! The tabbed detail pane: Trend, Activity, Health.
//!
//! Unlike the modal screens this replaced, the pane holds no data of its own —
//! it renders whatever the app last loaded, and the event loop reloads that on
//! every daemon update, so the pane stays live rather than freezing at the
//! moment it was opened.

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Wrap};
use ratatui::Frame;

use teiryo_core::domain::QuotaUnit;
use teiryo_core::{PollEvent, ProviderHealth, WindowRollover};

use crate::app::{App, DetailTab, Pane, TimeRange, Trend};
use crate::metrics;
use crate::metrics::{Boundary, BoundaryKind};
use crate::ui::format::{
    format_countdown, format_elapsed, outcome_text, trigger_glyph, truncate, usage_text,
};
use crate::ui::theme;

/// Render the detail pane into `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &mut App, now: DateTime<Utc>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border(app.focus == Pane::Detail))
        .title(tab_bar(app))
        .title_bottom(footer(app, now));
    let viewport = block.inner(area);
    match app.detail {
        DetailTab::Trend => {
            // A chart is drawn to fit, on both axes; only text rows overflow.
            app.set_detail_bounds(0, 0);
            render_trend(frame, area, block, app, now);
        }
        DetailTab::Activity => render_activity(frame, area, block, app, viewport, now),
        DetailTab::Health => render_health(frame, area, block, app, viewport, now),
    }
}

/// Clamp the pane's offsets to what `lines` actually overflow the viewport by,
/// and hand back the (y, x) to draw at. The renderer is the only place that
/// knows both the content and the viewport, so it is where the bounds for the
/// next scroll are established.
fn fit_scroll(app: &mut App, lines: &[Line<'_>], viewport: Rect) -> (u16, u16) {
    let widest = lines.iter().map(Line::width).max().unwrap_or(0);
    app.set_detail_bounds(
        widest.saturating_sub(viewport.width as usize),
        lines.len().saturating_sub(viewport.height as usize),
    );
    (
        app.detail_scroll.min(u16::MAX as usize) as u16,
        app.detail_hscroll.min(u16::MAX as usize) as u16,
    )
}

/// The tab strip, rendered as the pane's title so it costs no extra row.
fn tab_bar(app: &App) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, tab) in DetailTab::ALL.iter().enumerate() {
        let style = if *tab == app.detail {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            theme::dim()
        };
        spans.push(Span::styled(format!("{} {}", i + 1, tab.label()), style));
        spans.push(Span::raw("  "));
    }
    Line::from(spans)
}

/// Per-tab summary line along the pane's bottom border.
fn footer(app: &App, now: DateTime<Utc>) -> Line<'static> {
    match app.detail {
        DetailTab::Trend => trend_footer(app, now),
        DetailTab::Activity => Line::from(Span::styled(
            format!(
                " {} polls · PgUp/PgDn rows · j/k across ",
                app.activity.len()
            ),
            theme::dim(),
        )),
        DetailTab::Health => Line::from(Span::styled(
            format!(" {} provider(s) ", app.health.len()),
            theme::dim(),
        )),
    }
}

fn trend_footer(app: &App, now: DateTime<Utc>) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for range in TimeRange::ALL {
        let style = if range == app.range {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };
        spans.push(Span::styled(range.label(), style));
        spans.push(Span::raw(" "));
    }
    // A panned chart is no longer showing the present, which the series alone
    // cannot say — the line simply stops earlier than usual.
    let until = app.trend_until(now);
    if until < now {
        spans.push(Span::styled(
            format!("· ends {} · g for now ", format_elapsed(until, now)),
            Style::default().fg(theme::WARN),
        ));
    }
    // Scrolling back stops at the first stored point rather than running on
    // into empty time; without this the chart would just refuse to move, which
    // reads as a stuck key.
    if app.trend_at_oldest() {
        spans.push(Span::styled("· start of history ", theme::dim()));
    }
    // The same series the chart draws, so the counts cannot describe a window
    // the pane is no longer showing.
    if let Some(trend) = app.charted_trend() {
        let peak = trend
            .snapshots
            .iter()
            .map(|s| s.used)
            .fold(f64::NEG_INFINITY, f64::max);
        if peak.is_finite() {
            spans.push(Span::styled(
                format!("· {} pts · peak {peak:.0}", trend.snapshots.len()),
                theme::dim(),
            ));
        }
        // The rules are easy to miss on a busy chart, and a surprise reset is
        // worth stating in words rather than leaving to a color.
        let resets = charted_rollovers(trend).count();
        if resets > 0 {
            spans.push(Span::styled(format!(" · {resets} reset(s)"), theme::dim()));
            let surprises = charted_rollovers(trend)
                .filter(|r| r.kind.is_surprise())
                .count();
            if surprises > 0 {
                spans.push(Span::styled(
                    format!(" · {surprises} unexpected"),
                    Style::default().fg(theme::BOUNDARY_SURPRISE),
                ));
            }
        }
    }
    if let Some((status, view)) = app.selected_window() {
        spans.push(Span::styled(
            format!(" · now {}", usage_text(&view.window)),
            theme::dim(),
        ));
        if let Some(pace) = metrics::pace(&view.window, now) {
            spans.push(Span::styled(format!(" · pace {pace:.2}×"), theme::dim()));
        }
        // Named for what separates it from the pace beside it: that one is the
        // average since the window opened, this one only the recent end of it.
        let points = app.recent_points(&status.account.id, &view.window.id);
        if let Some(recent) = metrics::recent_pace(&view.window, points, now) {
            spans.push(Span::styled(
                format!(" · lately {recent:.2}×"),
                theme::dim(),
            ));
        }
        if let Some(note) = &view.hint.note {
            spans.push(Span::styled(
                format!(" · {}", truncate(note, 32)),
                Style::default().fg(theme::WARN),
            ));
        }
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn render_trend(
    frame: &mut Frame<'_>,
    area: Rect,
    block: Block<'static>,
    app: &App,
    now: DateTime<Utc>,
) {
    // Only chart a series that belongs to the current selection: a reload is
    // in flight while the selection moves, and the previous window's history
    // under the new window's title would be a lie.
    let Some(trend) = app.charted_trend() else {
        // "Select a window" is only actionable advice when there *are*
        // windows; with none it reads as a broken chart rather than as a
        // daemon that has not reported a successful poll yet.
        let text = if app.statuses.iter().all(|s| s.windows.is_empty()) {
            "no quota data yet — waiting for the first successful poll"
        } else {
            "select a window with j/k to chart its history"
        };
        frame.render_widget(placeholder(text, block), area);
        return;
    };
    if trend.snapshots.len() < 2 {
        frame.render_widget(
            placeholder(
                &format!(
                    "not enough history for {} in the last {}",
                    trend.title,
                    trend.range.label()
                ),
                block,
            ),
            area,
        );
        return;
    }

    // X is seconds before the right edge of the charted interval, so the
    // series always ends there regardless of when the daemon last polled — and
    // a panned chart stays put instead of sliding right on every tick.
    let span = trend.range.duration().num_seconds().max(1) as f64;
    let until = trend.until;
    let x_of = |ts: DateTime<Utc>| span - (until - ts).num_seconds() as f64;
    let y_max = y_bound(trend);

    let series: Vec<(f64, f64)> = trend
        .snapshots
        .iter()
        .map(|s| (x_of(s.ts), s.used))
        .collect();
    let critical = app
        .selected_window()
        .map(|(_, view)| f64::from(view.hint.critical_threshold) * y_max)
        .unwrap_or(y_max);

    // Where the window began and where it ends. Empty for a provider that
    // publishes no reset instant and has never been seen to roll over.
    let rules = app
        .selected_window()
        .map(|(_, view)| {
            metrics::boundaries(
                &view.window,
                &trend.rollovers,
                until - trend.range.duration(),
                until,
            )
        })
        .unwrap_or_default();
    // The upcoming reset is the one boundary that lies to the right of the
    // series, so the axis has to grow to hold it.
    let lead = future_lead(&rules, span, x_of, app.trend_is_live());
    let x_bound = span + lead;
    let threshold = vec![(0.0, critical), (x_bound, critical)];

    // Owned first, borrowed into datasets after: `Dataset::data` holds a slice.
    let rule_points: Vec<(BoundaryKind, Vec<(f64, f64)>)> = rules
        .iter()
        .filter(|b| b.kind != BoundaryKind::UpcomingReset || lead > 0.0)
        .map(|b| (b.kind, vrule(x_of(b.at), y_max)))
        .collect();
    // An unannounced drop is not a boundary — inferred from `used` alone, it is
    // not trustworthy enough to draw a rule through the chart — but it is still
    // worth a mark at the reading that raised it.
    let drops: Vec<(f64, f64)> = charted_rollovers(trend)
        .filter(|r| !r.kind.is_boundary())
        .map(|r| (x_of(r.observed_at), r.new_used))
        .collect();

    // Rules first: later datasets draw over earlier ones, and the series is
    // what the pane is for.
    let mut datasets: Vec<Dataset<'_>> = rule_points
        .iter()
        .map(|(kind, points)| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(rule_color(*kind)))
                .data(points)
        })
        .collect();
    datasets.push(
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(theme::ACCENT))
            .data(&series),
    );
    // The same points again, as a scatter in a coarser marker. Everything
    // between two readings is interpolation the daemon never measured, and at
    // a 3-minute cadence a wide range draws far more interpolated cells than
    // real ones. Braille for the line and a full-cell dot for the readings
    // makes the difference visible: the fat dots are what was actually probed.
    datasets.push(
        Dataset::default()
            .marker(Marker::Dot)
            .graph_type(GraphType::Scatter)
            .style(Style::default().fg(theme::ACCENT))
            .data(&series),
    );
    datasets.push(
        Dataset::default()
            .marker(Marker::Dot)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(theme::CRIT))
            .data(&threshold),
    );
    if !drops.is_empty() {
        datasets.push(
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(theme::BOUNDARY_SURPRISE))
                .data(&drops),
        );
    }

    let chart = Chart::new(datasets)
        .block(
            block.title_top(
                Line::from(Span::styled(
                    format!(" {} ", truncate(&trend.title, 40)),
                    theme::heading(),
                ))
                .alignment(Alignment::Right),
            ),
        )
        .x_axis(
            Axis::default()
                .style(theme::dim())
                .bounds([0.0, x_bound])
                .labels(x_labels(until - trend.range.duration(), x_bound, now)),
        )
        .y_axis(
            Axis::default()
                .style(theme::dim())
                .bounds([0.0, y_max])
                .labels(y_labels(y_max)),
        )
        .legend_position(None);
    frame.render_widget(chart, area);
}

/// Upper bound of the y axis: the published limit where there is one, else
/// enough headroom above the peak to keep the line off the top border.
fn y_bound(trend: &Trend) -> f64 {
    let peak = trend
        .snapshots
        .iter()
        .map(|s| s.used)
        .fold(f64::NEG_INFINITY, f64::max);
    let limit = trend.snapshots.last().and_then(|s| s.limit);
    // Never below the peak: a provider reporting over its own limit should
    // show the overshoot rather than have the line silently clipped flat.
    let headroom = if peak.is_finite() && peak > 0.0 {
        peak * 1.05
    } else {
        0.0
    };
    match (trend.snapshots.last().map(|s| s.unit), limit) {
        (Some(QuotaUnit::Percent), _) => 100.0_f64.max(headroom),
        (_, Some(limit)) if limit > 0.0 => limit.max(headroom),
        _ if headroom > 0.0 => headroom,
        _ => 1.0,
    }
}

/// The loaded page's rollovers that belong to the window being charted.
///
/// `Request::History` accepts `window: None`, in which case the daemon answers
/// for every window on the account. The pane only ever asks about one, but
/// filtering here keeps the markers and the footer count agreeing with
/// [`metrics::boundaries`], which does the same.
fn charted_rollovers(trend: &Trend) -> impl Iterator<Item = &WindowRollover> {
    trend.rollovers.iter().filter(|r| r.window == trend.window)
}

/// Widest slice of empty future the chart will grow to show the upcoming
/// reset, as a fraction of the visible range.
///
/// Without a ceiling a weekly window would squash a `1h` chart's whole history
/// into the leftmost column to make room for a reset three days out. Past that
/// point the countdown on the window's own row is the better answer, and the
/// rule is simply dropped.
const FUTURE_LEAD_MAX: f64 = 0.20;

/// How far past the right edge the axis should reach, in x units, to fit the
/// upcoming reset — `0.0` when there is none, when it is too far out to be
/// worth the room, or when the chart is panned.
///
/// A panned view is deliberately excluded: it is not showing the present, so
/// the *current* window's reset is not the boundary next to its right edge, and
/// drawing it there would place a future instant in the middle of the past.
fn future_lead(
    rules: &[Boundary],
    span: f64,
    x_of: impl Fn(DateTime<Utc>) -> f64,
    live: bool,
) -> f64 {
    if !live {
        return 0.0;
    }
    rules
        .iter()
        .find(|b| b.kind == BoundaryKind::UpcomingReset)
        .map(|b| x_of(b.at) - span)
        .filter(|lead| *lead > 0.0 && *lead <= span * FUTURE_LEAD_MAX)
        .unwrap_or(0.0)
}

/// A vertical rule at `x`, as points a scatter dataset can draw.
///
/// ratatui has no vline primitive. Braille dots spaced up the y axis read as a
/// dotted rule, which keeps it distinct from both the solid series and the
/// dotted horizontal threshold.
fn vrule(x: f64, y_max: f64) -> Vec<(f64, f64)> {
    const DOTS: usize = 24;
    (0..=DOTS)
        .map(|i| (x, y_max * i as f64 / DOTS as f64))
        .collect()
}

/// Color for a boundary rule. Scheduled rollovers recede into the background;
/// the live window's own edges match its series; anything the provider did not
/// advertise is flagged.
fn rule_color(kind: BoundaryKind) -> Color {
    match kind {
        BoundaryKind::Rollover => theme::BOUNDARY,
        BoundaryKind::Surprise => theme::BOUNDARY_SURPRISE,
        BoundaryKind::CurrentStart | BoundaryKind::UpcomingReset => theme::BOUNDARY_LIVE,
    }
}

/// Axis labels for an x extent of `total` seconds starting at `left`, counted
/// against *now* rather than against the edges — on a panned chart the two
/// differ, and "now" on the right of a view that ends three days ago would be
/// a lie. When the axis has been extended to reach an upcoming reset the right
/// label runs forwards instead, as `+1h`.
fn x_labels(left: DateTime<Utc>, total: f64, now: DateTime<Utc>) -> Vec<Line<'static>> {
    let at = |fraction: f64| {
        let ts = left + chrono::Duration::seconds((total * fraction) as i64);
        let delta = (ts - now).num_seconds();
        let text = match delta {
            0 => "now".to_owned(),
            d if d < 0 => format!(
                "-{}",
                format_countdown(now - chrono::Duration::seconds(d), now)
            ),
            d => format!(
                "+{}",
                format_countdown(now + chrono::Duration::seconds(d), now)
            ),
        };
        Line::from(Span::styled(text, theme::dim()))
    };
    vec![at(0.0), at(0.5), at(1.0)]
}

fn y_labels(y_max: f64) -> Vec<Line<'static>> {
    [0.0, y_max / 2.0, y_max]
        .into_iter()
        .map(|v| Line::from(Span::styled(format!("{v:.0}"), theme::dim())))
        .collect()
}

fn render_activity(
    frame: &mut Frame<'_>,
    area: Rect,
    block: Block<'static>,
    app: &mut App,
    viewport: Rect,
    now: DateTime<Utc>,
) {
    if app.activity.is_empty() {
        app.set_detail_bounds(0, 0);
        frame.render_widget(placeholder("no polls recorded yet", block), area);
        return;
    }
    // A Paragraph rather than a List: the rows are wider than most terminals,
    // and only a Paragraph can be scrolled sideways to reach the rest of them.
    // Nothing is lost — the log has no selection to highlight.
    let lines: Vec<Line<'static>> = app
        .activity
        .iter()
        .map(|event| activity_line(event, now))
        .collect();
    let scroll = fit_scroll(app, &lines, viewport);
    frame.render_widget(Paragraph::new(lines).block(block).scroll(scroll), area);
}

fn activity_line(event: &PollEvent, now: DateTime<Utc>) -> Line<'static> {
    let (text, failed) = outcome_text(event);
    Line::from(vec![
        Span::styled(format!(" {} ", trigger_glyph(&event.trigger)), theme::dim()),
        Span::styled(event.ts.format("%H:%M:%S").to_string(), theme::dim()),
        Span::raw(format!(
            "  {:<22}",
            truncate(&event.account.to_string(), 21)
        )),
        Span::styled(
            format!("{:<40}", truncate(&text, 39)),
            Style::default().fg(if failed { theme::CRIT } else { theme::OK }),
        ),
        Span::styled(format!("{:>7}ms", event.latency_ms), theme::dim()),
        Span::styled(format!("  {}", format_elapsed(event.ts, now)), theme::dim()),
    ])
}

fn render_health(
    frame: &mut Frame<'_>,
    area: Rect,
    block: Block<'static>,
    app: &mut App,
    viewport: Rect,
    now: DateTime<Utc>,
) {
    if app.health.is_empty() {
        app.set_detail_bounds(0, 0);
        frame.render_widget(placeholder("no provider health reported", block), area);
        return;
    }
    let mut lines = Vec::new();
    for provider in &app.health {
        lines.push(provider_line(provider));
        lines.extend(account_lines(provider, now));
    }
    // Unwrapped, unlike before: a wrapped line has no columns to the right, so
    // wrapping and a sideways scroll cannot both be right. Scrolling is the
    // better of the two here — it reaches the tail of a long provider error
    // without reflowing the provider→account indentation that carries the
    // hierarchy, and it works the same way as the Activity tab next door.
    let scroll = fit_scroll(app, &lines, viewport);
    frame.render_widget(Paragraph::new(lines).block(block).scroll(scroll), area);
}

fn provider_line(provider: &ProviderHealth) -> Line<'static> {
    let healthy = provider.consecutive_failures == 0;
    Line::from(vec![
        Span::styled(
            if healthy { " ● " } else { " ▲ " },
            Style::default().fg(if healthy { theme::OK } else { theme::CRIT }),
        ),
        Span::styled(
            provider.provider.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} account{}",
                provider.accounts.len(),
                if provider.accounts.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            theme::dim(),
        ),
    ])
}

fn account_lines(provider: &ProviderHealth, now: DateTime<Utc>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for account in &provider.accounts {
        let healthy = account.consecutive_failures == 0;
        let mut spans = vec![
            Span::raw("     "),
            Span::styled(
                if healthy { "ok  " } else { "fail" },
                Style::default().fg(if healthy { theme::OK } else { theme::CRIT }),
            ),
            Span::raw(format!(
                "  {:<24}",
                truncate(&account.account.to_string(), 23)
            )),
        ];
        if !healthy {
            spans.push(Span::styled(
                format!("{} consecutive  ", account.consecutive_failures),
                Style::default().fg(theme::CRIT),
            ));
        }
        if account.poll_interval_secs > 0 {
            spans.push(Span::styled(
                format!("every {}s", account.poll_interval_secs),
                theme::dim(),
            ));
        }
        if let Some(ts) = account.last_poll_ts {
            spans.push(Span::styled(
                format!("  last {}", format_elapsed(ts, now)),
                theme::dim(),
            ));
        }
        lines.push(Line::from(spans));
        // Errors get their own wrapped line rather than being truncated off
        // the right edge of the row above.
        if let Some(error) = &account.last_error {
            lines.push(Line::from(Span::styled(
                format!("       {error}"),
                Style::default().fg(theme::CRIT),
            )));
        }
    }
    lines
}

fn placeholder(text: &str, block: Block<'static>) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(format!(" {text}"), theme::dim())))
        .block(block)
        .wrap(Wrap { trim: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap()
    }

    /// A 24h chart ending at `now`, mapped the way `render_trend` maps it.
    const DAY: f64 = 24.0 * 3600.0;

    fn x_of(ts: DateTime<Utc>) -> f64 {
        DAY - (now() - ts).num_seconds() as f64
    }

    fn upcoming(hours: i64) -> Vec<Boundary> {
        vec![Boundary {
            at: now() + chrono::Duration::hours(hours),
            kind: BoundaryKind::UpcomingReset,
        }]
    }

    #[test]
    fn the_axis_grows_just_far_enough_to_hold_a_near_reset() {
        // 2h out of a 24h range is inside the 20% ceiling.
        let lead = future_lead(&upcoming(2), DAY, x_of, true);
        assert_eq!(lead, 2.0 * 3600.0);
        assert!(lead <= DAY * FUTURE_LEAD_MAX);
    }

    #[test]
    fn a_distant_reset_is_dropped_rather_than_squashing_the_history() {
        // A weekly window on a 24h chart: 3 days out is far past the ceiling.
        assert_eq!(future_lead(&upcoming(72), DAY, x_of, true), 0.0);
        // And exactly at the ceiling it still fits.
        assert!(future_lead(&upcoming(4), DAY, x_of, true) > 0.0);
    }

    #[test]
    fn a_panned_chart_never_grows() {
        assert_eq!(future_lead(&upcoming(2), DAY, x_of, false), 0.0);
    }

    #[test]
    fn without_an_upcoming_reset_there_is_nothing_to_make_room_for() {
        let past = vec![Boundary {
            at: now() - chrono::Duration::hours(3),
            kind: BoundaryKind::CurrentStart,
        }];
        assert_eq!(future_lead(&past, DAY, x_of, true), 0.0);
        assert_eq!(future_lead(&[], DAY, x_of, true), 0.0);
    }

    #[test]
    fn a_rule_is_vertical_and_spans_the_plot() {
        let points = vrule(42.0, 100.0);
        assert!(points.iter().all(|(x, _)| *x == 42.0));
        assert_eq!(points.first().unwrap().1, 0.0);
        assert_eq!(points.last().unwrap().1, 100.0);
        // Dense enough that no plot row is left with a gap in the rule.
        assert!(points.len() > 16);
    }

    #[test]
    fn each_boundary_reads_at_its_own_volume() {
        // Scheduled rollovers recede; the live window matches its series;
        // anything unadvertised is flagged.
        assert_eq!(rule_color(BoundaryKind::Rollover), theme::BOUNDARY);
        assert_eq!(rule_color(BoundaryKind::Surprise), theme::BOUNDARY_SURPRISE);
        assert_eq!(rule_color(BoundaryKind::CurrentStart), theme::BOUNDARY_LIVE);
        assert_eq!(
            rule_color(BoundaryKind::UpcomingReset),
            theme::BOUNDARY_LIVE
        );
        assert_ne!(rule_color(BoundaryKind::Rollover), theme::BOUNDARY_SURPRISE);
    }

    #[test]
    fn axis_labels_run_backwards_to_the_left_and_forwards_past_the_present() {
        let left = now() - chrono::Duration::hours(24);
        // Extended by 2h to reach an upcoming reset.
        let labels = x_labels(left, DAY + 2.0 * 3600.0, now());
        let text: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
        assert!(text[0].starts_with('-'), "{text:?}");
        assert!(text[2].starts_with('+'), "{text:?}");

        // Unextended, the right edge is the present itself.
        let plain = x_labels(left, DAY, now());
        assert_eq!(plain[2].to_string(), "now");
    }
}
