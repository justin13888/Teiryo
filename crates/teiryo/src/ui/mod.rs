//! Rendering. One view — header, quota gauges, tabbed detail pane, key bar —
//! with overlays drawn on top of it.

pub mod charts;
pub mod dashboard;
pub mod detail;
pub mod format;
pub mod overlay;
pub mod theme;

use chrono::Utc;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, DetailTab, Overlay, Pane};

/// Smallest detail pane still worth drawing: a chart needs axes plus a few
/// rows of plot area to say anything.
const MIN_DETAIL_HEIGHT: u16 = 13;

/// Tallest the detail pane grows to; beyond this the quota list benefits more
/// from the rows than the chart does.
const MAX_DETAIL_HEIGHT: u16 = 32;

/// Terminal height below which the detail pane is suppressed, so the quota
/// list itself does not get squeezed to nothing.
const MIN_HEIGHT_FOR_DETAIL: u16 = 26;

/// Top-level render entry point.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let now = Utc::now();
    let area = frame.area();
    // Whatever was clickable last frame is gone unless this one draws it
    // again; a hidden pane must not answer clicks on where it used to be.
    app.chart_hits.clear();
    app.view_tabs.clear();
    let detail_height = if app.detail_visible && area.height >= MIN_HEIGHT_FOR_DETAIL {
        (area.height / 2).clamp(MIN_DETAIL_HEIGHT, MAX_DETAIL_HEIGHT)
    } else {
        0
    };

    let [header, quotas, detail, keys] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(4),
        Constraint::Length(detail_height),
        Constraint::Length(1),
    ])
    .areas(area);

    // A pane the terminal is too short to show cannot hold the cursor, or
    // `j`/`k` would scroll something invisible.
    if detail_height == 0 {
        app.focus = Pane::List;
    }
    // Remembered so the pointer can be hit-tested against the same geometry
    // the user is looking at.
    app.quotas_area = quotas;
    app.detail_area = if detail_height > 0 {
        detail
    } else {
        Rect::ZERO
    };

    dashboard::render_header(frame, header, app, now);
    dashboard::render_quotas(frame, quotas, app, now);
    if detail_height > 0 {
        detail::render(frame, detail, app, now);
    }
    render_key_bar(frame, keys, app);

    if app.overlay.is_some() {
        overlay::render(frame, area, app);
    }
}

/// The bottom key bar, listing only what is bound in the current context —
/// which now includes where the cursor is, since `j`/`k` change axis with it.
fn render_key_bar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let keys: &str = match (app.overlay, app.detail_visible, app.focus, app.detail) {
        (Some(Overlay::Settings), ..) => {
            "j/k move  +/- adjust  Enter toggle  Backspace inherit  r reload  Esc close"
        }
        (Some(Overlay::Help | Overlay::ConfirmShutdown), ..) => {
            "any key closes  ·  y confirms a shutdown"
        }
        (None, false, ..) => "j/k move  r poll  R all  d show detail  s settings  ? help  q quit",
        (None, true, Pane::List, _) => {
            "j/k move  l detail  r poll  R all  Tab tab  [ ] range  d hide  s settings  ? help  q quit"
        }
        (None, true, Pane::Detail, DetailTab::Trend) => {
            "j/k pan  v view  f split  click/Enter inspect  , . step  g now  h list  [ ] range  ? help"
        }
        (None, true, Pane::Detail, DetailTab::Activity) => {
            "j/k select  Enter/click details  < > across  g/G ends  PgUp/PgDn page  h list  ? help"
        }
        (None, true, Pane::Detail, DetailTab::Health) => {
            "j/k scroll across  g/G ends  PgUp/PgDn · wheel rows  h list  Tab tab  ? help  q quit"
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {keys}"), theme::dim()))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Color;
    use ratatui::Terminal;

    use teiryo_core::domain::{
        AccountId, ClientKind, PollOutcome, PollTrigger, QuotaUnit, QuotaWindow, ResetKind,
        WindowId, WindowScope,
    };
    use teiryo_core::{
        Account, AccountHealth, AccountStatus, BarStyle, ConfigState, ConfigView, ObservedStart,
        PollEvent, PollId, ProviderHealth, ProviderSettings, QuotaSnapshot, RenderHint,
        RolloverKind, WindowRollover, WindowView,
    };

    use crate::app::{DetailTab, HitAxis, Inspect, Overlay, Purpose, Trend, TrendView};
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    fn view(id: &str, used: f64, note: Option<&str>) -> WindowView {
        WindowView {
            window: QuotaWindow {
                id: WindowId::from(id),
                label: format!("Window {id} with a fairly long label"),
                scope: WindowScope::Model("opus".into()),
                reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(5 * 3600)),
                unit: QuotaUnit::Percent,
                used,
                limit: Some(100.0),
                reset_at: Some(Utc::now() + Duration::hours(2)),
            },
            hint: RenderHint {
                style: BarStyle::Percent,
                warn_threshold: 0.8,
                critical_threshold: 0.95,
                note: note.map(str::to_owned),
            },
            observed_start: None,
        }
    }

    fn event(failed: bool) -> PollEvent {
        PollEvent {
            id: PollId::generate(),
            ts: Utc::now() - Duration::seconds(20),
            provider: "claude".into(),
            account: AccountId::from("claude:default"),
            trigger: PollTrigger::Manual {
                client: ClientKind::Tui,
            },
            outcome: if failed {
                PollOutcome::AuthError("token expired — run `claude` to refresh".into())
            } else {
                PollOutcome::Success { windows: vec![] }
            },
            latency_ms: 341,
        }
    }

    fn populated() -> App {
        let mut app = App::new();
        app.set_statuses(vec![AccountStatus {
            account: Account {
                id: AccountId::from("claude:default"),
                provider: "claude".into(),
                label: "default".into(),
            },
            windows: vec![
                view("session_5h", 62.0, Some("Blocks entirely at cap")),
                view("weekly", 97.0, Some("Blocks entirely at cap")),
                view("weekly_opus", 3.0, None),
            ],
            last_poll: Some(event(true)),
            last_success: Some(Utc::now() - Duration::minutes(9)),
            poll_interval_secs: 60,
        }]);
        app.activity = vec![event(false), event(true)];
        app.health = vec![ProviderHealth {
            provider: "claude".into(),
            accounts: vec![AccountHealth {
                account: AccountId::from("claude:default"),
                consecutive_failures: 2,
                last_error: Some("token expired — run `claude` to refresh".into()),
                last_poll_ts: Some(Utc::now()),
                poll_interval_secs: 60,
            }],
            consecutive_failures: 2,
            last_error: Some("token expired".into()),
        }];
        app.last_update = Some(Utc::now() - Duration::seconds(4));
        app.config = Some(config_state(None));
        app
    }

    /// Settings as the daemon reports them, optionally with a rejected load.
    fn config_state(error: Option<&str>) -> ConfigState {
        ConfigState {
            path: "/home/u/.config/teiryo/config.toml".into(),
            generation: 4,
            effective: ConfigView {
                poll_interval_secs: Some(120),
                default_poll_interval_secs: 60,
                min_poll_interval_secs: 10,
                providers: vec![
                    ProviderSettings {
                        provider: "claude".into(),
                        enabled: true,
                        poll_interval_secs: Some(30),
                        effective_poll_interval_secs: 30,
                    },
                    ProviderSettings {
                        provider: "openai".into(),
                        enabled: false,
                        poll_interval_secs: None,
                        effective_poll_interval_secs: 120,
                    },
                ],
            },
            loaded_at: Utc::now(),
            warnings: vec!["unknown key `providers.claude.retrys` — ignored".into()],
            error: error.map(str::to_owned),
        }
    }

    fn trend_for(app: &App, points: usize) -> Trend {
        let (status, view) = app.selected_window().expect("a window is selected");
        Trend {
            account: status.account.id.clone(),
            window: view.window.id.clone(),
            title: "default — Session".into(),
            range: app.range,
            until: app.trend_until(Utc::now()),
            // Enough history behind the series that a panned view is a view of
            // real data rather than one the app would clamp back.
            earliest: Some(Utc::now() - Duration::days(30)),
            fetched_at: Utc::now(),
            // One of each severity, so both rule colors and the unannounced
            // marker are exercised by every render below.
            rollovers: [
                (RolloverKind::Scheduled, 20 * 60),
                (RolloverKind::Early, 9 * 60),
                (RolloverKind::Unannounced, 60),
            ]
            .into_iter()
            .map(|(kind, minutes_ago)| WindowRollover {
                account: status.account.id.clone(),
                window: view.window.id.clone(),
                poll: PollId::generate(),
                observed_at: Utc::now() - Duration::minutes(minutes_ago),
                kind,
                prev_reset_at: Some(Utc::now() + Duration::hours(2)),
                new_reset_at: Some(Utc::now() + Duration::hours(3)),
                prev_used: 90.0,
                new_used: 2.0,
                prev_observed_at: Some(Utc::now() - Duration::minutes(minutes_ago + 3)),
            })
            .collect(),
            snapshots: (0..points)
                .map(|i| QuotaSnapshot {
                    poll_id: PollId::generate(),
                    ts: Utc::now() - Duration::minutes((points - i) as i64),
                    window: view.window.id.clone(),
                    label: "Session".into(),
                    unit: QuotaUnit::Percent,
                    used: (i as f64) * 1.5,
                    limit: Some(100.0),
                    // A rollover partway through, so the marker dataset runs.
                    reset_at: Some(
                        Utc::now() + Duration::hours(if i > points / 2 { 3 } else { 2 }),
                    ),
                })
                .collect(),
        }
    }

    fn draw(app: &mut App, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("render must not panic");
    }

    fn rendered(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("render must not panic");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Columns of the detail pane holding at least [`RULE_RUN`] cells of
    /// `color` — i.e. something spanning the plot rather than incidental.
    ///
    /// Counting tall columns, rather than testing for the color's mere
    /// presence, is what separates a rule from everything else painted in the
    /// same palette: `BOUNDARY` is the border and axis color too, and the
    /// warn-colored provider note sits in the footer. Both of those are at most
    /// a few cells in any one column; a rule spans the whole plot.
    fn rule_columns(app: &mut App, width: u16, height: u16, color: Color) -> usize {
        // Border, x-axis and label rows contribute 3 to every column, so the
        // threshold has to clear that with room to spare.
        const RULE_RUN: usize = 6;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("render must not panic");
        let pane = app.detail_area;
        assert!(pane.height > 0, "the detail pane must be on screen");
        let buffer = terminal.backend().buffer().clone();
        (pane.x..pane.x + pane.width)
            .filter(|&x| {
                (pane.y..pane.y + pane.height)
                    .filter(|&y| buffer[(x, y)].fg == color && buffer[(x, y)].symbol() != " ")
                    .count()
                    >= RULE_RUN
            })
            .count()
    }

    /// A trend pane wide enough that the four rules land in distinct columns.
    const TREND_FRAME: (u16, u16) = (100, 40);

    #[test]
    fn the_trend_chart_rules_off_every_window_boundary() {
        let mut app = populated();
        app.detail = DetailTab::Trend;
        app.trend = Some(trend_for(&app, 90));

        // Exactly one early rollover in the fixture, and it gets a rule.
        //
        // Only the surprise color is asserted on: `BOUNDARY` is `DIM`, which is
        // also the y-axis and its labels, and `BOUNDARY_LIVE` is `ACCENT`,
        // which is the series itself — neither can be told from the chrome by
        // color alone. `detail::tests` covers those two directly instead.
        assert_eq!(
            rule_columns(
                &mut app,
                TREND_FRAME.0,
                TREND_FRAME.1,
                theme::BOUNDARY_SURPRISE
            ),
            1
        );

        let frame = rendered(&mut app, TREND_FRAME.0, TREND_FRAME.1);
        // The reset is ~2h out on a 24h chart, so the axis grows past the
        // present to hold its rule and the right label runs forwards.
        assert!(
            frame.contains("+1h"),
            "the axis reaches the upcoming reset:\n{frame}"
        );
        // And the count is stated in words, not left to the colors alone.
        assert!(frame.contains("3 reset(s)"), "{frame}");
        assert!(frame.contains("2 unexpected"), "{frame}");
    }

    /// The whole feature is conditional on the provider publishing a reset
    /// instant. One that does not — and has never been seen to roll over —
    /// must chart exactly as it did before any of this existed.
    #[test]
    fn a_window_with_no_cap_draws_no_rules_and_no_lead() {
        let mut app = populated();
        app.detail = DetailTab::Trend;
        for status in &mut app.statuses {
            for window in &mut status.windows {
                window.window.reset_at = None;
            }
        }
        let mut trend = trend_for(&app, 90);
        trend.rollovers.clear();
        for snapshot in &mut trend.snapshots {
            snapshot.reset_at = None;
        }
        app.trend = Some(trend);

        assert_eq!(
            rule_columns(
                &mut app,
                TREND_FRAME.0,
                TREND_FRAME.1,
                theme::BOUNDARY_SURPRISE
            ),
            0,
            "no rules without a published reset"
        );

        let frame = rendered(&mut app, TREND_FRAME.0, TREND_FRAME.1);
        assert!(
            frame.contains("now") && !frame.contains("+"),
            "the axis still ends at the present:\n{frame}"
        );
        assert!(!frame.contains("reset(s)"), "{frame}");
    }

    /// A panned chart is not showing the present, so the *current* window's
    /// reset does not belong just off its right edge.
    #[test]
    fn a_panned_chart_does_not_reach_for_the_upcoming_reset() {
        let mut app = populated();
        app.detail = DetailTab::Trend;
        app.focus = Pane::Detail;
        app.trend = Some(trend_for(&app, 90));
        // Pan back; `trend_for` rebuilds the series against the new right edge.
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert!(!app.trend_is_live(), "precondition: the view moved");
        app.trend = Some(trend_for(&app, 90));

        let frame = rendered(&mut app, TREND_FRAME.0, TREND_FRAME.1);
        assert!(!frame.contains("+1h"), "{frame}");
    }

    /// An account whose windows have not arrived yet has nothing selectable,
    /// so telling the user to "select a window" is advice they cannot follow.
    #[test]
    fn empty_trend_pane_distinguishes_no_data_from_no_selection() {
        let mut app = App::new();
        app.set_statuses(vec![AccountStatus {
            account: Account {
                id: AccountId::from("claude:default"),
                provider: "claude".into(),
                label: "default".into(),
            },
            windows: Vec::new(),
            last_poll: Some(event(true)),
            last_success: None,
            poll_interval_secs: 60,
        }]);
        assert!(rendered(&mut app, 100, 40).contains("no quota data yet"));

        // With windows present but the header row selected, the original
        // advice is the right one.
        let mut app = populated();
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert!(rendered(&mut app, 100, 40).contains("select a window with j/k"));
    }

    /// Every tab, at sizes from absurdly small to very large. The row layout
    /// does width arithmetic that would panic on underflow, and the chart
    /// divides by a time span — both need real geometry to exercise.
    #[test]
    fn renders_at_every_size_and_tab() {
        const SIZES: [(u16, u16); 6] = [
            (20, 5),   // absurdly small
            (40, 10),  // too short for the detail pane
            (80, 24),  // classic terminal: pane must auto-collapse
            (80, 40),  // pane fits
            (120, 30), // wide
            (240, 80), // very large
        ];
        for (width, height) in SIZES {
            for tab in DetailTab::ALL {
                let mut app = populated();
                app.detail = tab;
                app.trend = Some(trend_for(&app, 90));
                draw(&mut app, width, height);
            }
        }
    }

    /// Empty state: connected but the daemon has discovered nothing yet.
    /// Anchor every window on a restart the daemon saw between `oldest` and
    /// `newest` minutes ago, bracketed exactly that widely.
    ///
    /// `view` hard-codes `observed_start: None`, and so does every other
    /// construction site outside `metrics`' own tests — which left the whole
    /// render path unexercised against an anchored window, including the claim
    /// that the chart's rule and the row's numbers describe the same one.
    fn anchor_every_window(app: &mut App, oldest: i64, newest: i64) {
        for status in &mut app.statuses {
            for window in &mut status.windows {
                window.observed_start = Some(ObservedStart {
                    not_before: Utc::now() - Duration::minutes(oldest),
                    not_after: Utc::now() - Duration::minutes(newest),
                });
            }
        }
    }

    /// The row's numbers are measured against the window that actually ran.
    ///
    /// Every fixture window publishes a reset two hours out on a 5-hour span,
    /// so the provider's arithmetic starts them three hours back. A restart
    /// seen an hour ago makes the effective window three hours long and one
    /// hour in — a third elapsed, where the published span said five sixths.
    #[test]
    fn an_anchored_row_measures_against_the_window_that_actually_ran() {
        let mut app = populated();
        anchor_every_window(&mut app, 61, 59);
        let out = rendered(&mut app, 120, 40);

        // 62% spent a third of the way in is 1.86×, against the 0.74× the
        // published span would have given.
        assert!(
            out.contains("1.86× pace"),
            "expected the anchored pace:\n{out}"
        );
        assert!(
            !out.contains("0.74× pace"),
            "the nominal start is gone:\n{out}"
        );
        // A two-minute bracket on a three-hour window is precise, so none of
        // the fields measured from the start is marked as an estimate. Checked
        // per field rather than on the frame: the header's `next ~39s`
        // countdown carries a `~` of its own and always will.
        assert!(
            !out.contains("~1.86× pace"),
            "a tight bracket needs no mark:\n{out}"
        );
        assert!(!out.contains("cap in ~"), "{out}");
        assert!(!out.contains("→~"), "{out}");
    }

    /// The same number, resting on a bracket wide enough to matter, says so.
    ///
    /// `start_uncertainty` was computed, carried across the whole metrics
    /// surface, and read by nothing: a pace anchored to a bracket minutes wide
    /// and one anchored to a bracket comparable to the window itself printed
    /// identically.
    #[test]
    fn a_row_marks_the_numbers_that_rest_on_a_wide_bracket() {
        let mut app = populated();
        // Restart seen somewhere in a whole hour, on a three-hour window.
        anchor_every_window(&mut app, 90, 30);
        let out = rendered(&mut app, 120, 40);

        // Same midpoint, so the same pace as the tight bracket above — the
        // mark is the only difference, which is the point.
        assert!(
            out.contains("~1.86× pace"),
            "expected a marked pace:\n{out}"
        );
        assert!(out.contains("cap in ~"), "expected a marked runway:\n{out}");
        assert!(out.contains("→~"), "expected a marked projection:\n{out}");
        // `afford` divides the remaining budget by the time left to the reset
        // and never touches the start, so it is not an estimate and is not
        // marked. Without this the mark could be blanket-applied and pass.
        assert!(
            out.contains("afford 0."),
            "afford rests on reset_at, not on the start:\n{out}"
        );
    }

    #[test]
    fn every_row_carries_its_derived_numbers_when_the_pane_is_tall_enough() {
        let mut app = populated();
        let out = rendered(&mut app, 120, 40);

        // The gauge line no longer carries pace; the continuation line does,
        // together with the runway and the pace still affordable.
        assert!(out.contains("pace"), "expected a pace field:\n{out}");
        assert!(out.contains("cap in"), "expected a runway field:\n{out}");
        assert!(
            out.contains("afford"),
            "expected an affordable pace:\n{out}"
        );
        assert!(out.contains("at reset"), "expected a projection:\n{out}");

        // The 97% window is 5 minutes from its cap and 2 hours from its reset,
        // so the cap is what binds; the 3% window's runway runs days past the
        // reset and is reported anyway.
        assert!(out.contains("cap in 5m"), "expected a near cap:\n{out}");
        assert!(
            out.contains("cap in 4d"),
            "expected a runway past the reset:\n{out}"
        );
    }

    /// Readings of `id` for the populated account, oldest first.
    fn recent_series(id: &str, readings: &[(i64, f64)]) -> Vec<QuotaSnapshot> {
        readings
            .iter()
            .map(|&(minutes_ago, used)| QuotaSnapshot {
                poll_id: PollId::generate(),
                ts: Utc::now() - Duration::minutes(minutes_ago),
                window: WindowId::from(id),
                label: "Session".into(),
                unit: QuotaUnit::Percent,
                used,
                limit: Some(100.0),
                reset_at: Some(Utc::now() + Duration::hours(2)),
            })
            .collect()
    }

    /// Readings every two minutes over the last `minutes`, climbing linearly
    /// from `from` to `to` — an account polling on cadence with nothing
    /// dropped, which is what a rate labelled "now" is allowed to be read from.
    fn recent_ramp(id: &str, minutes: i64, from: f64, to: f64) -> Vec<QuotaSnapshot> {
        let steps = minutes / 2;
        let readings: Vec<(i64, f64)> = (0..=steps)
            .map(|k| {
                (
                    minutes - k * 2,
                    from + (to - from) * k as f64 / steps as f64,
                )
            })
            .collect();
        recent_series(id, &readings)
    }

    #[test]
    fn a_row_reports_a_recent_burst_beside_the_average_that_hides_it() {
        let mut app = populated();
        // The 5-hour window is 3 hours in at 62%, an unremarkable 1.03×
        // average — but 20 of those points went in the last 20 minutes.
        app.set_recent(
            &AccountId::from("claude:default"),
            recent_ramp("session_5h", 20, 42.0, 62.0),
        );
        let out = rendered(&mut app, 120, 40);

        assert!(out.contains("1.03× pace"), "expected the average:\n{out}");
        assert!(
            out.contains("3.00× now ("),
            "expected the recent rate with its interval:\n{out}"
        );
        // The runway is the recent rate's, not the average's: the 38% left
        // at a point a minute lasts 38 minutes, where 1.03× would give hours.
        assert!(
            out.contains("cap in 38m 00s ("),
            "expected a runway at the recent rate:\n{out}"
        );
        // And so is the projection, which no longer just restates the pace.
        assert!(!out.contains("→103%"), "{out}");
        // Windows with no history behind them simply omit the field.
        assert_eq!(out.matches("× now").count(), 1, "only one series:\n{out}");
    }

    #[test]
    fn a_row_drops_the_now_rate_rather_than_printing_a_stale_one() {
        let mut app = populated();
        // The same burst, but the daemon stopped half an hour ago — well past
        // four missed polls at this account's 60-second cadence. The average
        // since the window opened is still a fact; the current rate is not.
        let stale: Vec<QuotaSnapshot> = recent_ramp("session_5h", 20, 42.0, 62.0)
            .into_iter()
            .map(|mut p| {
                p.ts -= Duration::minutes(30);
                p
            })
            .collect();
        app.set_recent(&AccountId::from("claude:default"), stale);
        let out = rendered(&mut app, 120, 40);

        assert!(out.contains("1.03× pace"), "expected the average:\n{out}");
        assert_eq!(
            out.matches("× now").count(),
            0,
            "a rate measured half an hour ago is not a rate now:\n{out}"
        );
    }

    #[test]
    fn a_short_pane_keeps_the_bars_and_drops_the_derived_line() {
        let mut app = populated();
        let out = rendered(&mut app, 120, 12);

        // Every window still has a gauge, and pace falls back to its column on
        // the gauge line rather than vanishing with the continuation line.
        assert!(out.contains("62%"), "expected the bars to survive:\n{out}");
        assert!(
            out.contains("pace"),
            "expected pace on the gauge line:\n{out}"
        );
        assert!(
            !out.contains("afford"),
            "the derived line should be gone:\n{out}"
        );
    }

    #[test]
    fn a_narrow_pane_sheds_derived_fields_from_the_right() {
        let mut app = populated();
        let out = rendered(&mut app, 60, 40);

        // Pace is the last field to go, so it survives where the rest cannot.
        assert!(out.contains("pace"), "expected pace to survive:\n{out}");
        assert!(
            !out.contains("at reset"),
            "the projection should be gone:\n{out}"
        );
    }

    #[test]
    fn the_help_overlay_says_what_the_row_numbers_mean() {
        let mut app = populated();
        app.overlay = Some(Overlay::Help);
        let out = rendered(&mut app, 100, 50);

        assert!(out.contains("What the numbers on a row mean"), "{out}");
        assert!(out.contains("burn rate since the window opened"), "{out}");
        // The legend sits below the keymap, so nothing may push it out through
        // the bottom border — the box sizes itself from the line count.
        assert!(out.contains("press any key to close"), "{out}");
    }

    #[test]
    fn renders_before_any_data_arrives() {
        for tab in DetailTab::ALL {
            let mut app = App::new();
            app.detail = tab;
            draw(&mut app, 80, 40);
        }
    }

    #[test]
    fn renders_overlays_and_error_states() {
        for overlay in [Overlay::Help, Overlay::ConfirmShutdown, Overlay::Settings] {
            let mut app = populated();
            app.overlay = Some(overlay);
            // Overlays must also fit terminals smaller than they are.
            draw(&mut app, 30, 8);
            draw(&mut app, 100, 40);
        }
        let mut app = populated();
        app.disconnected = true;
        app.error = Some("daemon unreachable".into());
        draw(&mut app, 80, 40);
    }

    /// The visibility complaint this feature exists to fix: the overlay has to
    /// say what each value is *and* where it came from, or "60s" is ambiguous
    /// between a default and a deliberate choice.
    #[test]
    fn settings_overlay_shows_values_with_their_provenance() {
        let mut app = populated();
        app.overlay = Some(Overlay::Settings);
        let screen = rendered(&mut app, 100, 40);

        assert!(screen.contains("config.toml"), "{screen}");
        assert!(screen.contains("Poll interval"), "{screen}");
        assert!(screen.contains("2m"), "the global 120s value: {screen}");
        assert!(screen.contains("set here"), "{screen}");
        // A provider override versus one that inherits.
        assert!(screen.contains("claude · interval"), "{screen}");
        assert!(screen.contains("override"), "{screen}");
        assert!(screen.contains("inherited"), "{screen}");
        // Disabled providers read as off rather than simply being absent.
        assert!(screen.contains("openai · polling"), "{screen}");
        assert!(screen.contains("⚠"), "the unknown-key warning: {screen}");
    }

    /// A rejected config must be visible from the dashboard, not only inside
    /// the overlay — otherwise the daemon looks like it is ignoring the file.
    #[test]
    fn a_rejected_config_shows_on_the_dashboard_and_in_the_overlay() {
        let mut app = populated();
        app.config = Some(config_state(Some(
            "`poll_interval_secs` must not be negative",
        )));
        assert!(rendered(&mut app, 100, 40).contains("config not applied"));

        app.overlay = Some(Overlay::Settings);
        let screen = rendered(&mut app, 100, 40);
        assert!(screen.contains("not applied"), "{screen}");
        assert!(screen.contains("still what is running"), "{screen}");
    }

    /// Opened before the first `GetConfig` reply lands, which is exactly what
    /// happens if the daemon is slow to answer.
    #[test]
    fn settings_overlay_renders_without_a_config_yet() {
        let mut app = populated();
        app.config = None;
        app.overlay = Some(Overlay::Settings);
        draw(&mut app, 30, 8);
        assert!(rendered(&mut app, 100, 40).contains("waiting for the daemon"));
    }

    /// A one- or two-point series cannot be charted; it must fall back to a
    /// message rather than dividing by a zero-width span.
    #[test]
    fn renders_degenerate_trend_series() {
        for points in [0, 1, 2] {
            let mut app = populated();
            app.trend = Some(trend_for(&app, points));
            draw(&mut app, 100, 40);
        }
    }

    /// Scroll routing needs geometry only the renderer knows: where each pane
    /// landed, and how far the widest row overruns it.
    #[test]
    fn rendering_records_the_geometry_scrolling_depends_on() {
        let mut app = populated();
        app.detail = DetailTab::Activity;
        draw(&mut app, 60, 40);
        assert!(app.quotas_area.height > 0);
        assert!(app.detail_area.height > 0);
        assert!(
            app.detail_hscroll_max > 0,
            "an activity row is wider than 60 columns"
        );
        assert_eq!(
            app.detail_scroll_max, 0,
            "two poll events fit the pane, so there is nothing to scroll down to"
        );

        // …and both bounds move with the content: a log longer than the pane
        // is what makes a vertical scroll mean anything.
        app.activity = (0..40).map(|_| event(false)).collect();
        draw(&mut app, 60, 40);
        assert!(app.detail_scroll_max > 0);

        // A hidden pane cannot be pointed at, so it holds neither the cursor
        // nor a rectangle for the pointer to land in.
        app.detail_visible = false;
        app.focus = Pane::Detail;
        draw(&mut app, 60, 40);
        assert_eq!(app.detail_area, Rect::ZERO);
        assert_eq!(app.focus, Pane::List);
    }

    #[test]
    fn scrolling_activity_sideways_reveals_the_columns_cut_off_the_right() {
        let mut app = populated();
        app.detail = DetailTab::Activity;
        let screen = rendered(&mut app, 60, 40);
        assert!(
            !screen.contains("341ms"),
            "latency is off-screen at 60: {screen}"
        );

        app.detail_hscroll = 40;
        let scrolled = rendered(&mut app, 60, 40);
        assert!(scrolled.contains("341ms"), "{scrolled}");
    }

    /// A chart panned into the past must not go on claiming it ends at "now".
    #[test]
    fn a_panned_trend_says_where_it_ends() {
        let mut app = populated();
        app.trend = Some(trend_for(&app, 90));
        assert!(rendered(&mut app, 100, 40).contains("now"));

        app.trend_pan_secs = Duration::hours(12).num_seconds();
        app.trend = Some(trend_for(&app, 90));
        let screen = rendered(&mut app, 100, 40);
        assert!(screen.contains("g for now"), "{screen}");
    }

    /// Panned all the way back, the footer has to say why the chart stopped
    /// moving — otherwise a clipped scroll reads as an unresponsive key.
    #[test]
    fn a_trend_at_the_end_of_its_history_says_so() {
        let mut app = populated();
        let mut trend = trend_for(&app, 90);
        // A day of stored history against the default 24h range: the view is
        // already showing all of it and cannot go back further.
        trend.earliest = Some(Utc::now() - Duration::hours(24));
        app.trend = Some(trend);
        let screen = rendered(&mut app, 100, 40);
        assert!(!screen.contains("start of history"), "{screen}");

        let mut trend = trend_for(&app, 90);
        trend.earliest = Some(Utc::now() - Duration::hours(30));
        app.trend = Some(trend);
        app.trend_pan_secs = Duration::hours(6).num_seconds();
        let screen = rendered(&mut app, 100, 40);
        assert!(screen.contains("start of history"), "{screen}");
    }

    /// A window with no published limit has no ratio to draw.
    #[test]
    fn renders_windows_without_a_limit_or_reset() {
        let mut app = populated();
        for status in &mut app.statuses {
            for view in &mut status.windows {
                view.window.unit = QuotaUnit::Tokens;
                view.window.limit = None;
                view.window.reset_at = None;
            }
        }
        draw(&mut app, 100, 40);
    }

    /// A frame of the Trend tab with every auxiliary series loaded, so each
    /// view has what it draws.
    fn charted() -> App {
        let mut app = populated();
        app.detail = DetailTab::Trend;
        let trend = trend_for(&app, 90);
        // Every window of the account over the same interval, for the
        // all-windows view: the selected one's readings under each id.
        let mut all = trend_for(&app, 90);
        for id in ["weekly", "weekly_opus"] {
            all.snapshots
                .extend(trend.snapshots.iter().map(|s| QuotaSnapshot {
                    window: WindowId::from(id),
                    used: s.used / 2.0,
                    ..s.clone()
                }));
        }
        app.set_extra(Purpose::AllWindows, all);
        app.set_extra(Purpose::Outcomes, trend_for(&app, 2));
        app.set_extra(Purpose::Burn, trend_for(&app, 90));
        app.set_recent(
            &AccountId::from("claude:default"),
            recent_ramp("session_5h", 20, 42.0, 62.0),
        );
        app.trend = Some(trend);
        app
    }

    fn click(app: &mut App, column: u16, row: u16) -> crate::app::Action {
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// The plot column a reading at `ts` lands in, by the geometry recorded
    /// for the chart.
    fn column_of(app: &App, chart: usize, ts: chrono::DateTime<Utc>) -> u16 {
        let hit = &app.chart_hits[chart];
        let HitAxis::Time { left, secs, .. } = &hit.axis else {
            panic!("not a time chart");
        };
        let fraction = (ts - *left).num_milliseconds() as f64 / 1000.0 / secs;
        hit.plot.x + (fraction * f64::from(hit.plot.width)) as u16
    }

    #[test]
    fn the_usage_view_stacks_a_rate_chart_and_the_coverage_under_usage() {
        let mut app = charted();
        let out = rendered(&mut app, 120, 60);
        assert_eq!(app.chart_hits.len(), 2, "usage and pace:\n{out}");
        let (usage, pace) = (&app.chart_hits[0].plot, &app.chart_hits[1].plot);
        assert!(usage.y < pace.y, "usage above pace");
        // One label column for both, so their time axes line up.
        assert_eq!((usage.x, usage.width), (pace.x, pace.width));
        for legend in ["now", "80%", "avg", "data", "Usage·Pace", "Forecast"] {
            assert!(out.contains(legend), "expected {legend:?}:\n{out}");
        }
        // The coverage strip marks readings.
        assert!(out.contains("•"), "{out}");
    }

    #[test]
    fn every_view_renders_with_its_data_and_records_what_it_drew() {
        for (view, hits, expect) in [
            (TrendView::Forecast, 2, "afford"),
            (TrendView::AllWindows, 1, "Window weekly_opus"),
            (TrendView::History, 2, "burn by hour of day"),
        ] {
            let mut app = charted();
            app.trend_view = view;
            let out = rendered(&mut app, 120, 60);
            assert_eq!(app.chart_hits.len(), hits, "{view:?}:\n{out}");
            assert!(out.contains(expect), "{view:?} lacks {expect:?}:\n{out}");
            // And at every size, without panicking.
            for (w, h) in [(20, 5), (60, 30), (80, 40), (240, 80)] {
                draw(&mut app, w, h);
            }
        }
    }

    #[test]
    fn a_short_pane_keeps_the_usage_chart_and_drops_the_rate_chart() {
        let mut app = charted();
        // 26 rows is the smallest terminal that shows the pane at all.
        draw(&mut app, 120, 26);
        assert_eq!(app.chart_hits.len(), 1, "usage alone");
    }

    #[test]
    fn a_click_on_a_chart_reads_out_the_reading_under_it_and_again_puts_it_away() {
        let mut app = charted();
        rendered(&mut app, 120, 60);
        let target = app.trend.as_ref().unwrap().snapshots[60].clone();
        let column = column_of(&app, 1, target.ts);
        let row = app.chart_hits[1].plot.y + 1;

        click(&mut app, column, row);
        assert_eq!(app.focus, Pane::Detail, "a click moves the cursor there");
        let Some(Inspect::Reading { ts, .. }) = app.inspect.clone() else {
            panic!("expected a reading, got {:?}", app.inspect);
        };
        // Snapped to a reading within a column's width of the one aimed at:
        // a column of a 24-hour chart is a quarter of an hour wide.
        let HitAxis::Time { secs, .. } = &app.chart_hits[1].axis else {
            panic!("a time chart");
        };
        let cell = secs / f64::from(app.chart_hits[1].plot.width);
        assert!(
            ((ts - target.ts).num_seconds().abs() as f64) <= cell,
            "{ts} vs {}",
            target.ts
        );
        let snapped = app
            .trend
            .as_ref()
            .unwrap()
            .snapshots
            .iter()
            .find(|s| s.ts == ts)
            .unwrap()
            .used;
        let out = rendered(&mut app, 120, 60);
        assert!(
            out.contains(&format!("{snapped:.0}% used")),
            "readout:\n{out}"
        );

        // The same point again.
        rendered(&mut app, 120, 60);
        let again = column_of(&app, 1, ts);
        click(&mut app, again, row);
        assert_eq!(app.inspect, None);
    }

    #[test]
    fn the_keyboard_walks_the_chart_cursor_reading_by_reading() {
        let mut app = charted();
        app.focus = Pane::Detail;
        rendered(&mut app, 120, 60);
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Enter));
        let times: Vec<_> = app
            .trend
            .as_ref()
            .unwrap()
            .snapshots
            .iter()
            .map(|s| s.ts)
            .collect();
        let at = |app: &App| match &app.inspect {
            Some(Inspect::Reading { ts, .. }) => Some(*ts),
            _ => None,
        };
        assert_eq!(
            at(&app),
            times.last().copied(),
            "Enter starts at the newest"
        );
        app.handle_key(key(KeyCode::Char('.')));
        assert_eq!(at(&app), times.last().copied(), "and stops there");
        app.handle_key(key(KeyCode::Char(',')));
        assert_eq!(at(&app), Some(times[times.len() - 2]));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.inspect, None, "Esc puts it away");
        assert_eq!(app.focus, Pane::Detail, "before leaving the pane");
    }

    #[test]
    fn a_click_on_a_bar_reads_out_what_it_stands_for() {
        let mut app = charted();
        app.trend_view = TrendView::History;
        rendered(&mut app, 120, 60);
        let burn = app.chart_hits[1].clone();
        let HitAxis::Bars { width, gap, .. } = burn.axis else {
            panic!("bars");
        };
        // The fourth bar: hour 03.
        click(
            &mut app,
            burn.plot.x + 3 * (width + gap),
            burn.plot.bottom() - 2,
        );
        let out = rendered(&mut app, 120, 60);
        assert!(out.contains("03:00–04:00"), "{out}");
    }

    #[test]
    fn clicking_a_view_name_switches_to_it() {
        let mut app = charted();
        rendered(&mut app, 120, 60);
        let (rect, view) = app.view_tabs[2];
        assert_eq!(view, TrendView::AllWindows);
        assert!(matches!(
            click(&mut app, rect.x, rect.y),
            crate::app::Action::ReloadDetail
        ));
        assert_eq!(app.trend_view, TrendView::AllWindows);
    }

    #[test]
    fn the_split_sets_the_session_window_beside_the_weekly_one() {
        let mut app = charted();
        // Make the selected window's neighbour a weekly one.
        for status in &mut app.statuses {
            status.windows[1].window.reset_kind =
                ResetKind::Rolling(std::time::Duration::from_secs(7 * 24 * 3600));
        }
        app.family_split = true;
        let (_, sibling) = app.sibling_window().expect("a weekly sibling");
        assert_eq!(sibling.window.id, WindowId::from("weekly"));
        let mut weekly = trend_for(&app, 90);
        weekly.window = WindowId::from("weekly");
        for s in &mut weekly.snapshots {
            s.window = WindowId::from("weekly");
        }
        app.set_extra(Purpose::Sibling, weekly);
        let out = rendered(&mut app, 160, 60);
        assert_eq!(app.chart_hits.len(), 4, "two stacks:\n{out}");
        // Session on the left.
        assert!(app.chart_hits[0].plot.x < app.chart_hits[2].plot.x);
        assert!(out.contains("Window weekly with"), "{out}");
    }

    fn on_activity() -> App {
        let mut app = populated();
        app.detail = DetailTab::Activity;
        app.focus = Pane::Detail;
        app
    }

    #[test]
    fn the_keyboard_selects_a_poll_and_enter_toggles_its_details() {
        let mut app = on_activity();
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.activity_index(),
            Some(0),
            "the first press lands on the newest"
        );
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.activity_index(), Some(1));

        app.handle_key(key(KeyCode::Enter));
        let out = rendered(&mut app, 120, 50);
        assert!(app.activity_details_area.width > 0, "{out}");
        for field in [
            "Details",
            "latency",
            "341 ms",
            "manual, from the TUI",
            "auth error",
        ] {
            assert!(out.contains(field), "expected {field:?}:\n{out}");
        }
        // The full error, not the column-truncated one.
        assert!(
            out.contains("token expired — run `claude` to refresh"),
            "{out}"
        );
        // Wide enough to sit beside the log, which stays readable.
        assert!(app.activity_list_area.width > 0);

        app.handle_key(key(KeyCode::Enter));
        rendered(&mut app, 120, 50);
        assert_eq!(
            app.activity_details_area,
            Rect::ZERO,
            "Enter again shuts it"
        );
    }

    #[test]
    fn clicking_a_poll_selects_it_and_clicking_it_again_toggles_its_details() {
        let mut app = on_activity();
        rendered(&mut app, 120, 50);
        let list = app.activity_list_area;
        click(&mut app, list.x + 3, list.y + 1);
        assert_eq!(app.activity_index(), Some(1));
        assert!(!app.activity_details, "a first click only selects");

        click(&mut app, list.x + 3, list.y + 1);
        assert!(app.activity_details, "the same row again opens it");
        rendered(&mut app, 120, 50);
        let list = app.activity_list_area;
        click(&mut app, list.x + 3, list.y + 1);
        assert!(!app.activity_details, "and again shuts it");

        // A click past the last row selects nothing.
        rendered(&mut app, 120, 50);
        click(&mut app, list.x + 3, list.y + 10);
        assert_eq!(app.activity_index(), Some(1));
    }

    #[test]
    fn the_cursor_stays_on_its_poll_when_newer_ones_arrive() {
        let mut app = on_activity();
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let chosen = app.activity_selected().unwrap().id;
        // A refresh: a new poll at the top, the old ones shifted down.
        let mut log = vec![event(false)];
        log.extend(app.activity.clone());
        app.activity = log;
        assert_eq!(app.activity_index(), Some(1));
        assert_eq!(app.activity_selected().unwrap().id, chosen);
    }

    #[test]
    fn a_narrow_pane_shows_details_in_place_of_the_log() {
        let mut app = on_activity();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let out = rendered(&mut app, 80, 50);
        assert_eq!(app.activity_list_area, Rect::ZERO, "{out}");
        assert!(out.contains("Details"), "{out}");
        // Esc shuts it before it steps out of the pane.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.activity_details);
        assert_eq!(app.focus, Pane::Detail);
    }

    #[test]
    fn a_successful_poll_lists_every_window_it_returned() {
        let mut app = on_activity();
        app.activity[0].outcome = PollOutcome::Success {
            windows: app.statuses[0]
                .windows
                .iter()
                .map(|w| w.window.clone())
                .collect(),
        };
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let out = rendered(&mut app, 160, 60);
        assert!(out.contains("ok · 3 windows"), "{out}");
        assert!(out.contains("session_5h"), "{out}");
        assert!(out.contains("model opus · rolling 5h"), "{out}");
    }

    /// The all-windows page reads every window in one poll, so a timestamp
    /// alone names several readings; the readout must be the selected one's.
    #[test]
    fn the_all_windows_readout_is_the_selected_windows_own_reading() {
        let mut app = charted();
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            app.selected_window().unwrap().1.window.id,
            WindowId::from("weekly")
        );
        // Session readings first in the page, weekly ones at half of them,
        // taken by the same polls.
        let mut all = trend_for(&app, 90);
        all.window = WindowId::from("weekly");
        for s in &mut all.snapshots {
            s.window = WindowId::from("session_5h");
        }
        let session = all.snapshots.clone();
        all.snapshots.extend(session.iter().map(|s| QuotaSnapshot {
            window: WindowId::from("weekly"),
            used: s.used / 2.0,
            ..s.clone()
        }));
        let snapshots = all.snapshots.clone();
        app.set_extra(Purpose::AllWindows, all);
        app.trend_view = TrendView::AllWindows;
        rendered(&mut app, 120, 60);
        let target = &snapshots[90 + 80];
        let column = column_of(&app, 0, target.ts);
        let row = app.chart_hits[0].plot.y + 1;
        click(&mut app, column, row);
        let Some(Inspect::Reading { ts, .. }) = app.inspect.clone() else {
            panic!("a reading");
        };
        let weekly = snapshots
            .iter()
            .find(|s| s.ts == ts && s.window == WindowId::from("weekly"))
            .unwrap();
        let session = snapshots
            .iter()
            .find(|s| s.ts == ts && s.window == WindowId::from("session_5h"))
            .unwrap();
        assert_ne!(
            format!("{:.0}", weekly.used),
            format!("{:.0}", session.used)
        );
        let out = rendered(&mut app, 120, 60);
        assert!(
            out.contains(&format!(" · {:.0}% used", weekly.used)),
            "expected weekly's {:.0}%, not session's {:.0}%:\n{out}",
            weekly.used,
            session.used
        );
    }

    /// The daemon's buckets move with every live refetch, so the reading the
    /// cursor was on may not come back; it moves to the nearest that does.
    #[test]
    fn the_chart_cursor_follows_a_refetch_and_leaves_with_its_reading() {
        let mut app = charted();
        app.focus = Pane::Detail;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Some(Inspect::Reading { ts, .. }) = app.inspect.clone() else {
            panic!("a reading");
        };
        // The same series, every reading 20 seconds later.
        let mut moved = trend_for(&app, 90);
        for s in &mut moved.snapshots {
            s.ts += Duration::seconds(20);
        }
        let expected = moved
            .snapshots
            .iter()
            .map(|s| s.ts)
            .min_by_key(|t| (*t - ts).num_milliseconds().abs())
            .unwrap();
        app.set_trend(moved);
        assert_eq!(
            app.inspect,
            Some(Inspect::Reading {
                window: WindowId::from("session_5h"),
                ts: expected
            })
        );
        let out = rendered(&mut app, 120, 60);
        assert!(out.contains("% used ·"), "the readout survives:\n{out}");
        app.handle_key(KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE));
        assert_ne!(
            app.inspect,
            Some(Inspect::Reading {
                window: WindowId::from("session_5h"),
                ts: expected
            }),
            "and it still steps"
        );

        // A series that now begins after the cursor's instant: it scrolled
        // off the left edge, and the cursor goes with it.
        for _ in 0..60 {
            app.handle_key(KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE));
        }
        let mut later = trend_for(&app, 90);
        later
            .snapshots
            .retain(|s| s.ts > Utc::now() - Duration::minutes(10));
        app.set_trend(later);
        assert_eq!(app.inspect, None);
    }

    /// A reading published without a reset has no rate behind it, but its
    /// time and usage are still worth reading out.
    #[test]
    fn a_reading_without_a_reset_still_reads_out_its_time_and_usage() {
        let mut app = charted();
        app.focus = Pane::Detail;
        let trend = app.trend.as_mut().unwrap();
        let last = trend.snapshots.last_mut().unwrap();
        last.reset_at = None;
        let (ts, used) = (last.ts, last.used);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.inspect,
            Some(Inspect::Reading {
                window: WindowId::from("session_5h"),
                ts
            })
        );
        let out = rendered(&mut app, 120, 60);
        let time = ts
            .with_timezone(&chrono::Local)
            .format("%a %H:%M")
            .to_string();
        assert!(
            out.contains(&format!("{time} · {used:.0}% used")),
            "readout:\n{out}"
        );
    }

    #[test]
    fn returning_to_the_present_puts_the_chart_cursor_away() {
        let mut app = charted();
        app.focus = Pane::Detail;
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert!(!app.trend_is_live());
        app.trend = Some(trend_for(&app, 90));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.inspect.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.inspect, None);
    }

    #[test]
    fn esc_on_another_tab_does_not_spend_itself_on_an_unseen_chart_cursor() {
        let mut app = charted();
        app.focus = Pane::Detail;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.inspect.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.activity_details);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !app.activity_details,
            "the first Esc shuts what is on screen"
        );
    }

    #[test]
    fn a_flat_recent_stretch_still_says_how_soon_the_cap_could_come() {
        let mut app = populated();
        app.set_recent(
            &AccountId::from("claude:default"),
            recent_ramp("session_5h", 20, 62.0, 62.0),
        );
        let out = rendered(&mut app, 140, 40);
        assert!(out.contains("0.00× now ("), "{out}");
        assert!(out.contains("cap in ∞ ("), "{out}");
    }

    #[test]
    fn a_narrow_row_drops_an_interval_before_the_value_it_belongs_to() {
        let mut app = populated();
        app.set_recent(
            &AccountId::from("claude:default"),
            recent_ramp("session_5h", 20, 42.0, 62.0),
        );
        let out = rendered(&mut app, 36, 40);
        assert!(out.contains("3.00× now"), "{out}");
        assert!(!out.contains("3.00× now ("), "{out}");
    }

    #[test]
    fn without_a_recent_rate_the_projections_say_they_are_the_averages() {
        let mut app = populated();
        let out = rendered(&mut app, 140, 40);
        assert!(out.contains("avg cap in"), "{out}");
        assert!(out.contains("avg →"), "{out}");
        assert!(!out.contains("× now"), "{out}");
    }
}
