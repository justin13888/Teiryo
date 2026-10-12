//! The Trend tab's charts: usage and the burn rate on one time axis, the
//! forecast as it stood at every reading, every window together, and what
//! became of past windows.
//!
//! Every chart here is drawn into a plot rect laid out by hand, with its axis
//! labels drawn beside it rather than by ratatui. Given no labels, ratatui's
//! `Chart` draws no axes and plots across its whole area, so the rect the
//! renderer records is exactly the rect the data occupies — which is what
//! lets a click be turned back into the instant under it.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Local, Timelike, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Bar, BarChart, BarGroup, Block, Chart, Dataset, GraphType, Paragraph,
};
use ratatui::Frame;

use teiryo_core::domain::{QuotaUnit, WindowId};
use teiryo_core::rollover::RESET_TOLERANCE;
use teiryo_core::{AccountStatus, QuotaSnapshot, WindowView};

use crate::app::{
    is_session, App, ChartHit, HitAxis, Inspect, Purpose, TimeRange, Trend, TrendView,
};
use crate::metrics::{self, Boundary, BoundaryKind, EffectiveWindow, RatePoint};
use crate::ui::dashboard::rate_interval;
use crate::ui::detail::{
    charted_rollovers, future_lead, placeholder, rule_color, trend_rules, vrule, x_labels, y_bound,
    y_labels,
};
use crate::ui::format::{cells, format_span, truncate, usage_text};
use crate::ui::theme;

/// Rows a chart needs, labels included, before stacking a second one under it
/// is worth more than giving the first the room.
const MIN_CHART_ROWS: u16 = 5;

/// Colours for the windows that are not selected on the all-windows chart.
/// Deliberately none of the severity colours: a second series is not a
/// warning.
const OTHERS: [Color; 4] = [
    Color::Magenta,
    Color::Blue,
    Color::LightMagenta,
    Color::LightBlue,
];

/// Draw the Trend tab into `area`, inside `block`.
pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    block: Block<'static>,
    app: &mut App,
    now: DateTime<Utc>,
) {
    let Some((status, view)) = app.selected_window() else {
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
    let title = readout(app, now).unwrap_or_else(|| {
        app.charted_trend()
            .map(|t| t.title.clone())
            .unwrap_or_else(|| format!("{} — {}", status.account.label, view.window.label))
    });
    let inspecting = app.inspect.is_some();
    let block = block.title_top(
        Line::from(Span::styled(
            format!(" {} ", truncate(&title, 72)),
            if inspecting {
                Style::default()
                    .fg(theme::CURSOR)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme::heading()
            },
        ))
        .alignment(Alignment::Right),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 || inner.width < 8 {
        return;
    }
    let [strip, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    view_strip(frame, strip, app);

    let mut hits = Vec::new();
    match app.trend_view {
        TrendView::UsagePace | TrendView::Forecast => {
            render_columns(frame, body, app, now, &mut hits)
        }
        TrendView::AllWindows => render_all_windows(frame, body, app, now, &mut hits),
        TrendView::History => render_history(frame, body, app, now, &mut hits),
    }
    app.chart_hits = hits;
}

/// The row of view names along the top of the pane, recorded so a click can
/// pick one.
fn view_strip(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let mut x = area.x + 1;
    let mut tabs = Vec::new();
    let mut spans = vec![Span::raw(" ")];
    for view in TrendView::ALL {
        let label = view.label();
        let width = cells(label) as u16;
        let style = if view == app.trend_view {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            theme::dim()
        };
        if x + width <= area.right() {
            tabs.push((Rect::new(x, area.y, width, 1), view));
        }
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("  "));
        x += width + 2;
    }
    let split = if !app.trend_view.splits() {
        ""
    } else if app.family_split {
        "f split: on"
    } else {
        "f split: off"
    };
    spans.push(Span::styled(format!("· v view  {split}"), theme::dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    app.view_tabs = tabs;
}

/// One column of the splittable views: a window and its loaded series.
struct Column<'a> {
    status: &'a AccountStatus,
    view: &'a WindowView,
    trend: &'a Trend,
}

/// The selected window's column, and with the split on and the sibling's
/// series loaded, the sibling's beside it — session on the left, weekly on
/// the right, whichever of the two is selected.
fn columns(app: &App) -> Vec<Column<'_>> {
    let Some(((status, view), trend)) = app.selected_window().zip(app.charted_trend()) else {
        return Vec::new();
    };
    let mut out = vec![Column {
        status,
        view,
        trend,
    }];
    if app.family_split && app.trend_view.splits() {
        if let Some(((status, view), trend)) = app.sibling_window().zip(app.extra(Purpose::Sibling))
        {
            let sibling = Column {
                status,
                view,
                trend,
            };
            if is_session(view) {
                out.insert(0, sibling);
            } else {
                out.push(sibling);
            }
        }
    }
    out
}

/// How far apart two readings of a charted series may be and still count as
/// one stretch.
///
/// The account's own gap tolerance, widened to the bucket the daemon
/// downsampled the range into: a week drawn in 480 points has a reading
/// every 21 minutes even when the daemon never missed a poll, and calling
/// every one of those a hole would leave the chart with no rate at all.
fn chart_gap(range: TimeRange, poll_interval_secs: u32) -> Duration {
    let bucket = range.duration() / range.max_points() as i32;
    metrics::gap_tolerance(poll_interval_secs).max(bucket * 2)
}

/// The scale a window's raw `used` is charted on: percent points, or the
/// published limit.
fn y_scale(view: &WindowView) -> f64 {
    match (view.window.unit, view.window.limit) {
        (QuotaUnit::Percent, _) => 100.0,
        (_, Some(limit)) if limit > 0.0 => limit,
        _ => 100.0,
    }
}

/// A time axis shared by every chart in one column, so a cursor or a rule
/// lands on the same column of each.
#[derive(Debug, Clone, Copy)]
struct TimeAxis {
    left: DateTime<Utc>,
    span: f64,
    lead: f64,
}

impl TimeAxis {
    /// The axis a trend is drawn on, with the rules that decided whether it
    /// grows to reach the upcoming reset.
    fn of(
        trend: &Trend,
        view: &WindowView,
        live: bool,
        now: DateTime<Utc>,
    ) -> (Self, Vec<Boundary>) {
        let span = trend.range.duration().num_seconds().max(1) as f64;
        let left = trend.until - trend.range.duration();
        let rules = trend_rules(Some(view), &trend.rollovers, left, trend.until, now);
        let x_of = |ts: DateTime<Utc>| (ts - left).num_seconds() as f64;
        let lead = future_lead(&rules, span, x_of, live);
        (Self { left, span, lead }, rules)
    }

    fn x(&self, ts: DateTime<Utc>) -> f64 {
        (ts - self.left).num_milliseconds() as f64 / 1000.0
    }

    fn bound(&self) -> f64 {
        self.span + self.lead
    }

    fn contains(&self, ts: DateTime<Utc>) -> bool {
        (0.0..=self.bound()).contains(&self.x(ts))
    }

    fn hit(&self, plot: Rect, window: &WindowId) -> ChartHit {
        ChartHit {
            plot,
            axis: HitAxis::Time {
                left: self.left,
                secs: self.bound(),
                window: window.clone(),
            },
        }
    }
}

/// One dataset, owned, so a chart's series can be built before ratatui
/// borrows them.
struct Series {
    points: Vec<(f64, f64)>,
    color: Color,
    marker: Marker,
    graph: GraphType,
}

impl Series {
    fn line(points: Vec<(f64, f64)>, color: Color) -> Self {
        Self {
            points,
            color,
            marker: Marker::Braille,
            graph: GraphType::Line,
        }
    }

    fn dotted(points: Vec<(f64, f64)>, color: Color) -> Self {
        Self {
            points,
            color,
            marker: Marker::Dot,
            graph: GraphType::Line,
        }
    }

    fn scatter(points: Vec<(f64, f64)>, color: Color, marker: Marker) -> Self {
        Self {
            points,
            color,
            marker,
            graph: GraphType::Scatter,
        }
    }
}

/// A chart's frame: the plot, and the labels around it.
struct Frame2 {
    plot: Rect,
    y_labels: Rect,
    x_labels: Option<Rect>,
}

/// Carve `area` into a plot with `label_width` columns of y labels to its
/// left and, when asked, a row of x labels under it.
fn carve(area: Rect, label_width: u16, with_x_labels: bool) -> Frame2 {
    let [left, plot] =
        Layout::horizontal([Constraint::Length(label_width), Constraint::Min(0)]).areas(area);
    if with_x_labels && plot.height > 1 {
        let [plot, x] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(plot);
        let [y_labels, _] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(left);
        Frame2 {
            plot,
            y_labels,
            x_labels: Some(x),
        }
    } else {
        Frame2 {
            plot,
            y_labels: left,
            x_labels: None,
        }
    }
}

/// The width a set of y labels needs, with a cell of air before the plot.
fn label_width(labels: &[String]) -> u16 {
    labels.iter().map(|l| cells(l)).max().unwrap_or(0) as u16 + 1
}

/// Draw `series` into `frame.plot` over `x` by `y`, with its labels.
fn draw(
    frame: &mut Frame<'_>,
    chart: &Frame2,
    series: &[Series],
    x: [f64; 2],
    y: [f64; 2],
    y_text: &[String; 3],
    x_text: Option<Vec<Line<'static>>>,
) {
    let datasets: Vec<Dataset<'_>> = series
        .iter()
        .map(|s| {
            Dataset::default()
                .marker(s.marker)
                .graph_type(s.graph)
                .style(Style::default().fg(s.color))
                .data(&s.points)
        })
        .collect();
    frame.render_widget(
        Chart::new(datasets)
            .x_axis(Axis::default().bounds(x))
            .y_axis(Axis::default().bounds(y))
            .legend_position(None),
        chart.plot,
    );
    let area = chart.y_labels;
    if area.height > 0 && area.width > 1 {
        let width = area.width - 1;
        let rows = [area.bottom() - 1, area.y + (area.height - 1) / 2, area.y];
        for (text, row) in y_text.iter().zip(rows) {
            frame.render_widget(
                Paragraph::new(Span::styled(text.clone(), theme::dim()))
                    .alignment(Alignment::Right),
                Rect::new(area.x, row, width, 1),
            );
        }
    }
    if let (Some(area), Some(labels)) = (chart.x_labels, x_text) {
        for (label, alignment) in
            labels
                .into_iter()
                .zip([Alignment::Left, Alignment::Center, Alignment::Right])
        {
            frame.render_widget(Paragraph::new(label).alignment(alignment), area);
        }
    }
}

/// Points of a line, broken wherever a value is missing so the line does not
/// bridge a stretch it knows nothing about.
fn segments(points: impl IntoIterator<Item = Option<(f64, f64)>>) -> Vec<Vec<(f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64)>> = vec![Vec::new()];
    for point in points {
        match point {
            Some(p) => out.last_mut().expect("never empty").push(p),
            None if out.last().is_some_and(|s| !s.is_empty()) => out.push(Vec::new()),
            None => {}
        }
    }
    out.retain(|s| !s.is_empty());
    out
}

/// An upper bound for a rate axis: room for the series, never under 2× so
/// the 1.0 line sits mid-chart on a quiet one, and never past 10× so one
/// young window's spike does not flatten everything else.
fn rate_bound<'a>(values: impl IntoIterator<Item = &'a f64>) -> f64 {
    let peak = values
        .into_iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(0.0_f64, f64::max);
    ((peak * 1.1 / 0.5).ceil() * 0.5).clamp(2.0, 10.0)
}

fn rate_labels(y_max: f64) -> [String; 3] {
    [
        "0×".to_owned(),
        format!("{:.1}×", y_max / 2.0),
        format!("{y_max:.1}×"),
    ]
}

fn percent_labels(y_max: f64) -> [String; 3] {
    let labels: Vec<String> = y_labels(y_max).iter().map(|l| l.to_string()).collect();
    [labels[0].clone(), labels[1].clone(), labels[2].clone()]
}

/// The rules of `rules` as series over `y_max`, the upcoming reset only when
/// the axis grew to hold it.
fn rule_series(rules: &[Boundary], axis: &TimeAxis, y_max: f64) -> Vec<Series> {
    rules
        .iter()
        .filter(|b| b.kind != BoundaryKind::UpcomingReset || axis.lead > 0.0)
        .map(|b| {
            Series::scatter(
                vrule(axis.x(b.at), y_max),
                rule_color(b.kind),
                Marker::Braille,
            )
        })
        .collect()
}

/// The cursor, as a rule in the terminal's own foreground colour — visible
/// on any theme, and the one colour nothing else on a chart uses.
fn cursor_series(app: &App, axis: &TimeAxis, y_max: f64) -> Option<Series> {
    let Some(Inspect::Reading { ts, .. }) = &app.inspect else {
        return None;
    };
    axis.contains(*ts)
        .then(|| Series::scatter(vrule(axis.x(*ts), y_max), theme::CURSOR, Marker::Braille))
}

/// Everything a column's charts derive from its series, computed once.
struct Derived {
    effective: Option<EffectiveWindow>,
    points: Vec<RatePoint>,
}

fn derive(column: &Column<'_>, now: DateTime<Utc>) -> Derived {
    let effective = metrics::effective_window(column.view, now);
    let span = column
        .view
        .window
        .span()
        .unwrap_or_else(|| Duration::hours(5));
    let points = metrics::rate_series(
        &column.view.window.id,
        span,
        effective.as_ref(),
        &column.trend.snapshots,
        chart_gap(column.trend.range, column.status.poll_interval_secs),
    );
    Derived { effective, points }
}

/// The two splittable views, one column per window.
fn render_columns(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: DateTime<Utc>,
    hits: &mut Vec<ChartHit>,
) {
    let columns = columns(app);
    let Some(primary) = columns.first() else {
        let text = "loading history…";
        frame.render_widget(
            Paragraph::new(Span::styled(format!(" {text}"), theme::dim())),
            area,
        );
        return;
    };
    if columns.iter().all(|c| c.trend.snapshots.len() < 2) {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    " not enough history for {} in the last {}",
                    primary.trend.title,
                    primary.trend.range.label()
                ),
                theme::dim(),
            )),
            area,
        );
        return;
    }
    let constraints = vec![Constraint::Ratio(1, columns.len() as u32); columns.len()];
    let rects = Layout::horizontal(constraints).spacing(1).split(area);
    for (column, rect) in columns.iter().zip(rects.iter()) {
        let mut rect = *rect;
        if columns.len() > 1 {
            // Name each column: two charts of the same shape side by side
            // are otherwise told apart only by their numbers.
            let [caption, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(rect);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate(&column.view.window.label, caption.width as usize),
                    theme::heading(),
                ))
                .alignment(Alignment::Center),
                caption,
            );
            rect = rest;
        }
        match app.trend_view {
            TrendView::Forecast => forecast_stack(frame, rect, app, column, now, hits),
            _ => usage_pace_stack(frame, rect, app, column, now, hits),
        }
    }
}

/// Split `area` into a top chart, and a bottom one when there is room for
/// both to be legible.
fn stack(area: Rect) -> (Rect, Option<Rect>) {
    if area.height >= MIN_CHART_ROWS * 2 {
        let [top, bottom] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
        (top, Some(bottom))
    } else {
        (area, None)
    }
}

/// Usage over pace, with the coverage strip under them.
fn usage_pace_stack(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    column: &Column<'_>,
    now: DateTime<Utc>,
    hits: &mut Vec<ChartHit>,
) {
    let (charts, strip) = if area.height > MIN_CHART_ROWS {
        let [charts, strip] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        (charts, Some(strip))
    } else {
        (area, None)
    };
    let (top, bottom) = stack(charts);
    let live = app.trend_is_live();
    let (axis, rules) = TimeAxis::of(column.trend, column.view, live, now);
    let derived = derive(column, now);

    // Pace first: its labels are the wider, and both charts must share one
    // label column or their plots — and so their time axes — would not line
    // up.
    let pace_values: Vec<f64> = derived
        .points
        .iter()
        .filter(|p| axis.contains(p.ts))
        .flat_map(|p| {
            let average = metrics::pace(&p.window, p.ts);
            p.rate.map(|r| r.high).into_iter().chain(average)
        })
        .collect();
    let pace_max = rate_bound(&pace_values);
    let pace_text = rate_labels(pace_max);
    let usage_max = y_bound(column.trend);
    let usage_text = percent_labels(usage_max);
    let width = label_width(&pace_text).max(label_width(&usage_text));
    let labels = x_labels(axis.left, axis.bound(), now);

    let usage = carve(top, width, bottom.is_none());
    usage_chart(
        frame,
        &usage,
        app,
        column,
        &axis,
        &rules,
        &derived,
        (usage_max, &usage_text),
        now,
        bottom.is_none().then(|| labels.clone()),
    );
    hits.push(axis.hit(usage.plot, &column.view.window.id));

    if let Some(bottom) = bottom {
        let pace = carve(bottom, width, true);
        let mut series = rule_series(&rules, &axis, pace_max);
        series.push(Series::line(
            vec![(0.0, 1.0), (axis.bound(), 1.0)],
            theme::OK,
        ));
        let visible: Vec<&RatePoint> = derived
            .points
            .iter()
            .filter(|p| axis.contains(p.ts))
            .collect();
        let at = |p: &RatePoint, v: f64| Some((axis.x(p.ts), v));
        for band in [
            segments(visible.iter().map(|p| p.rate.and_then(|r| at(p, r.low)))),
            segments(visible.iter().map(|p| p.rate.and_then(|r| at(p, r.high)))),
        ] {
            series.extend(band.into_iter().map(|s| Series::line(s, theme::DIM)));
        }
        series.extend(
            segments(
                visible
                    .iter()
                    .map(|p| metrics::pace(&p.window, p.ts).and_then(|v| at(p, v))),
            )
            .into_iter()
            .map(|s| Series::line(s, theme::SECONDARY)),
        );
        series.extend(
            segments(visible.iter().map(|p| p.rate.and_then(|r| at(p, r.pace))))
                .into_iter()
                .map(|s| Series::line(s, theme::ACCENT)),
        );
        series.extend(cursor_series(app, &axis, pace_max));
        draw(
            frame,
            &pace,
            &series,
            [0.0, axis.bound()],
            [0.0, pace_max],
            &pace_text,
            Some(labels),
        );
        legend(
            frame,
            pace.plot,
            &[
                ("now", theme::ACCENT),
                ("80%", theme::DIM),
                ("avg", theme::SECONDARY),
            ],
        );
        hits.push(axis.hit(pace.plot, &column.view.window.id));
    }

    if let Some(strip) = strip {
        let [label, cells] =
            Layout::horizontal([Constraint::Length(width), Constraint::Min(0)]).areas(strip);
        frame.render_widget(
            Paragraph::new(Span::styled("data", theme::dim())).alignment(Alignment::Left),
            label,
        );
        let gap = chart_gap(column.trend.range, column.status.poll_interval_secs);
        frame.render_widget(
            Paragraph::new(coverage_line(
                &axis,
                cells.width,
                &column.trend.snapshots,
                gap,
                now,
            )),
            cells,
        );
    }
}

/// Usage: the series, its readings, the threshold, the rules, the drops the
/// detector inferred, and — on a live chart — the projection to the reset.
#[allow(clippy::too_many_arguments)]
fn usage_chart(
    frame: &mut Frame<'_>,
    chart: &Frame2,
    app: &App,
    column: &Column<'_>,
    axis: &TimeAxis,
    rules: &[Boundary],
    derived: &Derived,
    (y_max, y_text): (f64, &[String; 3]),
    now: DateTime<Utc>,
    x_text: Option<Vec<Line<'static>>>,
) {
    let trend = column.trend;
    let readings: Vec<(f64, f64)> = trend
        .snapshots
        .iter()
        .map(|s| (axis.x(s.ts), s.used))
        .collect();
    let critical = f64::from(column.view.hint.critical_threshold) * y_max;

    // Rules first: later datasets draw over earlier ones, and the series is
    // what the pane is for.
    let mut series = rule_series(rules, axis, y_max);
    series.extend(projection_cone(app, column, axis, derived, now));
    series.push(Series::line(readings.clone(), theme::ACCENT));
    // The same points again, as a scatter in a coarser marker. Everything
    // between two readings is interpolation the daemon never measured, and at
    // a 3-minute cadence a wide range draws far more interpolated cells than
    // real ones. Braille for the line and a full-cell dot for the readings
    // makes the difference visible: the fat dots are what was actually
    // probed.
    series.push(Series::scatter(readings, theme::ACCENT, Marker::Dot));
    series.push(Series::dotted(
        vec![(0.0, critical), (axis.bound(), critical)],
        theme::CRIT,
    ));
    // An unannounced drop is not a boundary — inferred from `used` alone, it
    // is not trustworthy enough to draw a rule through the chart — but it is
    // still worth a mark at the reading that raised it.
    let drops: Vec<(f64, f64)> = charted_rollovers(trend)
        .filter(|r| !r.kind.is_boundary())
        .map(|r| (axis.x(r.observed_at), r.new_used))
        .collect();
    if !drops.is_empty() {
        series.push(Series::scatter(
            drops,
            theme::BOUNDARY_SURPRISE,
            Marker::Block,
        ));
    }
    series.extend(cursor_series(app, axis, y_max));
    draw(
        frame,
        chart,
        &series,
        [0.0, axis.bound()],
        [0.0, y_max],
        y_text,
        x_text,
    );
}

/// Where usage goes from here at the recent rate: a centre line and the two
/// ends of its interval, each running to the reset or to the cap, whichever
/// comes first, and clipped at the axis.
///
/// Measured from the same recent history the row uses, so the cone and the
/// row's `cap in` can never disagree. Only on a live chart — a panned one is
/// not showing the present for a projection to start from.
fn projection_cone(
    app: &App,
    column: &Column<'_>,
    axis: &TimeAxis,
    derived: &Derived,
    now: DateTime<Utc>,
) -> Vec<Series> {
    if !app.trend_is_live() {
        return Vec::new();
    }
    let Some(window) = derived.effective.as_ref() else {
        return Vec::new();
    };
    let Some(used) = window.used else {
        return Vec::new();
    };
    let points = app.recent_points(&column.status.account.id, &column.view.window.id);
    let max_gap = metrics::gap_tolerance(column.status.poll_interval_secs);
    let Some(rate) = metrics::estimate_rate(window, points, max_gap, now) else {
        return Vec::new();
    };
    let Some(projection) = metrics::project(window, &rate, now) else {
        return Vec::new();
    };
    let scale = y_scale(column.view);
    let start = (axis.x(now), used * scale);
    let edge = |runway: Option<Duration>, at_reset: f64| {
        let (ts, value) = match runway {
            Some(r) if now + r < window.reset_at => (now + r, 1.0),
            _ => (window.reset_at, at_reset),
        };
        let end = (axis.x(ts), value * scale);
        // Clip at the right edge, along the line.
        let bound = axis.bound();
        if end.0 > bound && end.0 > start.0 {
            let t = (bound - start.0) / (end.0 - start.0);
            vec![start, (bound, start.1 + (end.1 - start.1) * t)]
        } else {
            vec![start, end]
        }
    };
    vec![
        Series::line(
            edge(projection.runway.low, projection.at_reset.high),
            theme::DIM,
        ),
        Series::line(
            edge(projection.runway.high, projection.at_reset.low),
            theme::DIM,
        ),
        Series::line(
            edge(projection.runway.mid, projection.at_reset.mid),
            theme::SECONDARY,
        ),
    ]
}

/// What the readings cover, one cell per plot column: `•` a column holding a
/// reading, `─` one between two readings close enough to be one stretch,
/// `░` a hole the rate cannot be measured across, blank for the future.
fn coverage_line(
    axis: &TimeAxis,
    width: u16,
    snapshots: &[QuotaSnapshot],
    gap: Duration,
    now: DateTime<Utc>,
) -> Line<'static> {
    let mut times: Vec<DateTime<Utc>> = snapshots.iter().map(|s| s.ts).collect();
    times.sort();
    times.dedup();
    let secs = axis.bound();
    let at = |i: u16| {
        axis.left
            + Duration::milliseconds(
                (secs * f64::from(i) / f64::from(width.max(1)) * 1000.0) as i64,
            )
    };
    let mut spans = Vec::new();
    for i in 0..width {
        let (from, to) = (at(i), at(i + 1));
        let glyph = if from > now {
            Span::raw(" ")
        } else if times.iter().any(|t| *t >= from && *t < to) {
            Span::styled("•", Style::default().fg(theme::ACCENT))
        } else {
            let before = times.iter().rev().find(|t| **t < from);
            let after = times.iter().find(|t| **t >= to).copied().or_else(|| {
                // After the newest reading, the present is the far side.
                (to <= now).then_some(now)
            });
            match (before, after) {
                (Some(b), Some(a)) if a - *b <= gap => Span::styled("─", theme::dim()),
                _ => Span::styled("░", Style::default().fg(theme::WARN)),
            }
        };
        spans.push(glyph);
    }
    Line::from(spans)
}

/// A one-row key in the top-right corner of a plot.
fn legend(frame: &mut Frame<'_>, plot: Rect, entries: &[(&str, Color)]) {
    let mut spans = Vec::new();
    for (label, color) in entries {
        spans.push(Span::styled("━", Style::default().fg(*color)));
        spans.push(Span::styled(format!("{label} "), theme::dim()));
    }
    let line = Line::from(spans);
    let width = line.width() as u16;
    if plot.width > width + 2 && plot.height > 2 {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(plot.right() - width, plot.y, width, 1),
        );
    }
}

/// The forecast as it stood at each reading, over the pace it could afford
/// against the pace it ran at.
fn forecast_stack(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    column: &Column<'_>,
    now: DateTime<Utc>,
    hits: &mut Vec<ChartHit>,
) {
    let (top, bottom) = stack(area);
    let (axis, rules) = TimeAxis::of(column.trend, column.view, false, now);
    let derived = derive(column, now);
    let visible: Vec<&RatePoint> = derived
        .points
        .iter()
        .filter(|p| axis.contains(p.ts))
        .collect();
    let projections: Vec<Option<metrics::Projection>> = visible
        .iter()
        .map(|p| p.rate.and_then(|r| metrics::project(&p.window, &r, p.ts)))
        .collect();
    let affords: Vec<Option<f64>> = visible
        .iter()
        .map(|p| metrics::affordable_pace(&p.window, p.ts))
        .collect();

    let at_reset_peak = projections
        .iter()
        .flatten()
        .map(|p| p.at_reset.high * 100.0)
        .fold(0.0_f64, f64::max);
    let forecast_max = ((at_reset_peak * 1.05 / 50.0).ceil() * 50.0).clamp(120.0, 400.0);
    let forecast_text = [
        "0%".to_owned(),
        format!("{:.0}%", forecast_max / 2.0),
        format!("{forecast_max:.0}%"),
    ];
    let rate_values: Vec<f64> = visible
        .iter()
        .filter_map(|p| p.rate.map(|r| r.high))
        .chain(affords.iter().flatten().copied())
        .collect();
    let rate_max = rate_bound(&rate_values);
    let rate_text = rate_labels(rate_max);
    let width = label_width(&forecast_text).max(label_width(&rate_text));
    let labels = x_labels(axis.left, axis.bound(), now);

    let forecast = carve(top, width, bottom.is_none());
    let mut series = rule_series(&rules, &axis, forecast_max);
    series.push(Series::line(
        vec![(0.0, 100.0), (axis.bound(), 100.0)],
        theme::CRIT,
    ));
    let x = |p: &RatePoint| axis.x(p.ts);
    let band = |pick: fn(&metrics::Projection) -> f64| {
        segments(
            visible
                .iter()
                .zip(&projections)
                .map(|(p, proj)| proj.as_ref().map(|j| (x(p), pick(j) * 100.0))),
        )
    };
    for s in band(|j| j.at_reset.low)
        .into_iter()
        .chain(band(|j| j.at_reset.high))
    {
        series.push(Series::line(s, theme::DIM));
    }
    for s in band(|j| j.at_reset.mid) {
        series.push(Series::line(s, theme::ACCENT));
    }
    series.extend(cursor_series(app, &axis, forecast_max));
    draw(
        frame,
        &forecast,
        &series,
        [0.0, axis.bound()],
        [0.0, forecast_max],
        &forecast_text,
        bottom.is_none().then(|| labels.clone()),
    );
    legend(
        frame,
        forecast.plot,
        &[("→ at reset", theme::ACCENT), ("80%", theme::DIM)],
    );
    hits.push(axis.hit(forecast.plot, &column.view.window.id));

    if let Some(bottom) = bottom {
        let chart = carve(bottom, width, true);
        let mut series = rule_series(&rules, &axis, rate_max);
        series.push(Series::line(
            vec![(0.0, 1.0), (axis.bound(), 1.0)],
            theme::DIM,
        ));
        for s in segments(visible.iter().map(|p| p.rate.map(|r| (x(p), r.low))))
            .into_iter()
            .chain(segments(
                visible.iter().map(|p| p.rate.map(|r| (x(p), r.high))),
            ))
        {
            series.push(Series::line(s, theme::DIM));
        }
        for s in segments(
            visible
                .iter()
                .zip(&affords)
                .map(|(p, a)| a.map(|a| (x(p), a))),
        ) {
            series.push(Series::line(s, theme::OK));
        }
        for s in segments(visible.iter().map(|p| p.rate.map(|r| (x(p), r.pace)))) {
            series.push(Series::line(s, theme::ACCENT));
        }
        series.extend(cursor_series(app, &axis, rate_max));
        draw(
            frame,
            &chart,
            &series,
            [0.0, axis.bound()],
            [0.0, rate_max],
            &rate_text,
            Some(labels),
        );
        legend(
            frame,
            chart.plot,
            &[("now", theme::ACCENT), ("afford", theme::OK)],
        );
        hits.push(axis.hit(chart.plot, &column.view.window.id));
    }
}

/// Every window of the account on one 0–100% axis: the selected one in
/// accent, the rest each in a colour of its own, each line broken at its
/// rollovers so no reset is drawn as a descent.
fn render_all_windows(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: DateTime<Utc>,
    hits: &mut Vec<ChartHit>,
) {
    let (Some((status, selected)), Some(all)) =
        (app.selected_window(), app.extra(Purpose::AllWindows))
    else {
        frame.render_widget(
            Paragraph::new(Span::styled(" loading history…", theme::dim())),
            area,
        );
        return;
    };
    let axis = TimeAxis {
        left: all.until - all.range.duration(),
        span: all.range.duration().num_seconds().max(1) as f64,
        lead: 0.0,
    };
    let text = percent_labels(100.0);
    let width = label_width(&text);
    let [chart_area, key] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let chart = carve(chart_area, width, true);

    let mut by_window: HashMap<&WindowId, Vec<&QuotaSnapshot>> = HashMap::new();
    for s in &all.snapshots {
        by_window.entry(&s.window).or_default().push(s);
    }
    let mut series = Vec::new();
    let mut key_spans = Vec::new();
    let mut others = OTHERS.iter().cycle();
    // The selected window last, so it draws over the rest.
    let mut order: Vec<&WindowView> = status.windows.iter().collect();
    order.sort_by_key(|w| w.window.id == selected.window.id);
    for view in order {
        let color = if view.window.id == selected.window.id {
            theme::ACCENT
        } else {
            *others.next().expect("cycled")
        };
        let mut readings = by_window.remove(&view.window.id).unwrap_or_default();
        readings.sort_by_key(|s| s.ts);
        let mut previous: Option<DateTime<Utc>> = None;
        let line = segments(readings.iter().flat_map(|s| {
            // A rollover is where `reset_at` moved: break the line there.
            let broken = match (previous, s.reset_at) {
                (Some(p), Some(r)) => (r - p).abs() > RESET_TOLERANCE,
                _ => false,
            };
            previous = s.reset_at.or(previous);
            let point = s.utilization().map(|u| (axis.x(s.ts), u * 100.0));
            [broken.then_some(None), Some(point)].into_iter().flatten()
        }));
        series.extend(line.into_iter().map(|s| Series::line(s, color)));
        key_spans.push(Span::styled("━ ", Style::default().fg(color)));
        key_spans.push(Span::styled(
            format!("{}  ", view.window.label),
            theme::dim(),
        ));
    }
    series.push(Series::line(
        vec![(0.0, 100.0), (axis.bound(), 100.0)],
        theme::CRIT,
    ));
    series.extend(cursor_series(app, &axis, 100.0));
    draw(
        frame,
        &chart,
        &series,
        [0.0, axis.bound()],
        [0.0, 100.0],
        &text,
        Some(x_labels(axis.left, axis.bound(), now)),
    );
    frame.render_widget(Paragraph::new(Line::from(key_spans)), key);
    hits.push(axis.hit(chart.plot, &selected.window.id));
}

/// What became of past instances of the selected window, over when in the
/// day its quota goes.
fn render_history(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: DateTime<Utc>,
    hits: &mut Vec<ChartHit>,
) {
    let Some((_, view)) = app.selected_window() else {
        return;
    };
    let (top, bottom) = stack(area);
    match app.extra(Purpose::Outcomes) {
        Some(outcomes) => outcome_bars(frame, top, app, view, outcomes, now, hits),
        None => frame.render_widget(Paragraph::new(Span::styled(" loading…", theme::dim())), top),
    }
    if let Some(bottom) = bottom {
        match app.extra(Purpose::Burn) {
            Some(burn) => burn_bars(frame, bottom, app, view, burn, hits),
            None => frame.render_widget(
                Paragraph::new(Span::styled(" loading…", theme::dim())),
                bottom,
            ),
        }
    }
}

/// A caption row over a bar chart, and the rect left for the bars.
fn captioned(frame: &mut Frame<'_>, area: Rect, caption: String) -> Rect {
    let [caption_row, rest] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(Span::styled(caption, theme::dim())),
        caption_row,
    );
    rest
}

/// Highlight for the bar under the cursor.
fn bar_style(app: &App, plot: Rect, index: usize, color: Color) -> Style {
    match &app.inspect {
        Some(Inspect::Bar {
            plot: p, index: i, ..
        }) if *p == plot && *i == index => Style::default().fg(theme::CURSOR),
        _ => Style::default().fg(color),
    }
}

/// One bar per window instance the series saw end: where usage stood when it
/// rolled over — the peak it reached — and how it ended. The running
/// instance closes the row, at its current reading.
fn outcome_bars(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    view: &WindowView,
    outcomes: &Trend,
    now: DateTime<Utc>,
    hits: &mut Vec<ChartHit>,
) {
    let scale = y_scale(view);
    let mut ended: Vec<(DateTime<Utc>, f64, &'static str)> = charted_rollovers(outcomes)
        .map(|r| (r.observed_at, r.prev_used / scale, r.kind.as_str()))
        .collect();
    ended.sort_by_key(|(at, ..)| *at);
    let plot = captioned(
        frame,
        area,
        format!(
            " peak of each past window, last {}d — {} ended · click a bar",
            crate::app::OUTCOMES_SPAN_DAYS,
            ended.len()
        ),
    );
    if plot.height < 2 {
        return;
    }
    let mut bars: Vec<(String, f64, Color)> = ended
        .iter()
        .map(|(at, used, kind)| {
            let color = if *kind == "scheduled" {
                theme::severity(*used, &view.hint)
            } else {
                theme::BOUNDARY_SURPRISE
            };
            (
                format!(
                    "ended {} at {:.0}% · {kind}",
                    at.with_timezone(&Local).format("%a %m-%d %H:%M"),
                    used * 100.0
                ),
                *used,
                color,
            )
        })
        .collect();
    if let Some(used) = view.window.utilization() {
        bars.push((
            format!(
                "running now at {:.0}% · {}",
                used * 100.0,
                view.window
                    .reset_at
                    .map(|r| format!("resets in {}", format_span(r - now)))
                    .unwrap_or_default()
            ),
            used,
            theme::ACCENT,
        ));
    }
    let (width, gap) = (2u16, 1u16);
    let fit = usize::from(plot.width / (width + gap)).max(1);
    let shown = &bars[bars.len().saturating_sub(fit)..];
    let drawn: Vec<Bar<'_>> = shown
        .iter()
        .enumerate()
        .map(|(i, (_, used, color))| {
            Bar::default()
                .value((used * 100.0).round() as u64)
                .text_value(String::new())
                .style(bar_style(app, plot, i, *color))
        })
        .collect();
    frame.render_widget(
        BarChart::default()
            .data(BarGroup::default().bars(&drawn))
            .bar_width(width)
            .bar_gap(gap)
            .max(100),
        plot,
    );
    hits.push(ChartHit {
        plot,
        axis: HitAxis::Bars {
            width,
            gap,
            readouts: shown.iter().map(|(r, ..)| r.clone()).collect(),
        },
    });
}

/// Usage burnt in each hour of the local day over the last week.
///
/// A cumulative counter only says how much went between two readings, not
/// when inside them, so each step is credited to the hour of the reading
/// that closed it — and steps longer than an hour, which cannot be placed
/// in one, are left out rather than guessed at. Falls are restarts or
/// revisions, not negative burn, and are skipped too.
fn burn_bars(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    view: &WindowView,
    burn: &Trend,
    hits: &mut Vec<ChartHit>,
) {
    let scale = y_scale(view);
    let mut readings: Vec<&QuotaSnapshot> = burn
        .snapshots
        .iter()
        .filter(|s| s.window == view.window.id)
        .collect();
    readings.sort_by_key(|s| s.ts);
    let mut hours = [0.0_f64; 24];
    for pair in readings.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let same_window = match (a.reset_at, b.reset_at) {
            (Some(x), Some(y)) => (x - y).abs() <= RESET_TOLERANCE,
            _ => true,
        };
        let step = b.used - a.used;
        if same_window && step > 0.0 && b.ts - a.ts <= Duration::hours(1) {
            hours[b.ts.with_timezone(&Local).hour() as usize] += step / scale * 100.0;
        }
    }
    let total: f64 = hours.iter().sum();
    let plot = captioned(
        frame,
        area,
        format!(
            " burn by hour of day, last {}d, local time — {total:.0} points in all",
            crate::app::BURN_SPAN_DAYS
        ),
    );
    if plot.height < 3 {
        return;
    }
    let width = (plot.width / 24).saturating_sub(1).clamp(1, 4);
    let gap = 1;
    let peak = hours.iter().copied().fold(0.0_f64, f64::max).max(1.0);
    let bars: Vec<Bar<'_>> = hours
        .iter()
        .enumerate()
        .map(|(hour, points)| {
            Bar::default()
                // Tenths, so an hour with a fraction of a point still shows.
                .value((points * 10.0).round() as u64)
                .text_value(String::new())
                .label(Line::from(if width >= 2 {
                    format!("{hour:02}")
                } else {
                    String::new()
                }))
                .style(bar_style(app, plot, hour, theme::ACCENT))
        })
        .collect();
    frame.render_widget(
        BarChart::default()
            .data(BarGroup::default().bars(&bars))
            .bar_width(width)
            .bar_gap(gap)
            .max((peak * 10.0).ceil() as u64)
            .label_style(theme::dim()),
        plot,
    );
    let days = crate::app::BURN_SPAN_DAYS as f64;
    hits.push(ChartHit {
        plot,
        axis: HitAxis::Bars {
            width,
            gap,
            readouts: hours
                .iter()
                .enumerate()
                .map(|(hour, points)| {
                    format!(
                        "{hour:02}:00–{:02}:00 · {points:.1} points over {days:.0}d · {:.1}/day",
                        (hour + 1) % 24,
                        points / days
                    )
                })
                .collect(),
        },
    });
}

/// The pane title while the cursor is out: the numbers at it.
///
/// For a reading, everything the row would have said at that instant —
/// usage, the recent rate with its interval, the average, the projection
/// and the pace it could afford — so a point on any chart can be read off
/// against all the others.
pub fn readout(app: &App, now: DateTime<Utc>) -> Option<String> {
    match app.inspect.as_ref()? {
        Inspect::Bar { readout, .. } => Some(readout.clone()),
        Inspect::Reading { window, ts } => {
            let (status, view, snapshots) = series_of(app, window)?;
            let reading = snapshots.iter().find(|s| s.ts == *ts)?;
            let mut parts = vec![
                ts.with_timezone(&Local).format("%a %H:%M").to_string(),
                usage_text(&teiryo_core::domain::QuotaWindow {
                    used: reading.used,
                    limit: reading.limit,
                    unit: reading.unit,
                    ..view.window.clone()
                }),
            ];
            let range = app.charted_trend().map_or(app.range, |t| t.range);
            let gap = chart_gap(range, status.poll_interval_secs);
            let span = view.window.span().unwrap_or_else(|| Duration::hours(5));
            let effective = metrics::effective_window(view, now);
            let point = metrics::rate_series(window, span, effective.as_ref(), snapshots, gap)
                .into_iter()
                .find(|p| p.ts == *ts)?;
            if let Some(rate) = point.rate {
                parts.push(format!("now {:.2}× {}", rate.pace, rate_interval(&rate)));
            }
            if let Some(average) = metrics::pace(&point.window, point.ts) {
                parts.push(format!("avg {average:.2}×"));
            }
            if let Some(projection) = point
                .rate
                .and_then(|r| metrics::project(&point.window, &r, point.ts))
            {
                let at = projection.at_reset;
                parts.push(format!(
                    "→{:.0}% ({:.0}–{:.0}%)",
                    at.mid * 100.0,
                    at.low * 100.0,
                    at.high * 100.0
                ));
            }
            if let Some(afford) = metrics::affordable_pace(&point.window, point.ts) {
                parts.push(format!("afford {afford:.2}×"));
            }
            Some(parts.join(" · "))
        }
    }
}

/// Whose readings a window id names on the charts, and the readings.
fn series_of<'a>(
    app: &'a App,
    window: &WindowId,
) -> Option<(&'a AccountStatus, &'a WindowView, &'a [QuotaSnapshot])> {
    let (status, _) = app.selected_window()?;
    let view = status.windows.iter().find(|w| &w.window.id == window)?;
    let series = app.charted_series(window)?;
    Some((status, view, &series.snapshots))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_breaks_where_its_values_stop() {
        let got = segments([
            Some((0.0, 1.0)),
            Some((1.0, 1.0)),
            None,
            None,
            Some((3.0, 2.0)),
        ]);
        assert_eq!(got, vec![vec![(0.0, 1.0), (1.0, 1.0)], vec![(3.0, 2.0)]]);
        assert!(segments([None, None]).is_empty());
    }

    #[test]
    fn a_rate_axis_keeps_the_affordable_line_in_view_and_spikes_in_check() {
        assert_eq!(rate_bound(&[0.3, 0.9]), 2.0);
        assert_eq!(rate_bound(&[2.6]), 3.0);
        assert_eq!(rate_bound(&[40.0, f64::INFINITY]), 10.0);
    }

    #[test]
    fn a_downsampled_week_is_not_one_long_hole() {
        // 480 points over a week is a reading every 21 minutes; a 3-minute
        // cadence's own tolerance would call every step an outage.
        assert!(chart_gap(TimeRange::D7, 180) >= Duration::minutes(42));
        assert_eq!(chart_gap(TimeRange::H1, 180), metrics::gap_tolerance(180));
    }
}
