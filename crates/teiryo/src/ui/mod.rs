//! Rendering. One view — header, quota gauges, tabbed detail pane, key bar —
//! with overlays drawn on top of it.

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
const MAX_DETAIL_HEIGHT: u16 = 22;

/// Terminal height below which the detail pane is suppressed, so the quota
/// list itself does not get squeezed to nothing.
const MIN_HEIGHT_FOR_DETAIL: u16 = 26;

/// Top-level render entry point.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let now = Utc::now();
    let area = frame.area();
    let detail_height = if app.detail_visible && area.height >= MIN_HEIGHT_FOR_DETAIL {
        (area.height / 3).clamp(MIN_DETAIL_HEIGHT, MAX_DETAIL_HEIGHT)
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
            "j/k pan time  wheel nudges  g now  h list  Tab tab  [ ] range  d hide  ? help  q quit"
        }
        (None, true, Pane::Detail, DetailTab::Activity | DetailTab::Health) => {
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
        Account, AccountHealth, AccountStatus, BarStyle, ConfigState, ConfigView, PollEvent,
        PollId, ProviderHealth, ProviderSettings, QuotaSnapshot, RenderHint, RolloverKind,
        WindowRollover, WindowView,
    };

    use crate::app::{DetailTab, Overlay, Trend};

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

    #[test]
    fn a_row_reports_a_recent_burst_beside_the_average_that_hides_it() {
        let mut app = populated();
        // The 5-hour window is 3 hours in at 62%, an unremarkable 1.03×
        // average — but 20 of those points went in the last 20 minutes.
        app.set_recent(
            &AccountId::from("claude:default"),
            recent_series("session_5h", &[(20, 42.0), (0, 62.0)]),
        );
        let out = rendered(&mut app, 120, 40);

        assert!(out.contains("1.03× pace"), "expected the average:\n{out}");
        assert!(
            out.contains("3.00× now"),
            "expected the recent rate:\n{out}"
        );
        // Windows with no history behind them simply omit the field.
        assert_eq!(out.matches("× now").count(), 1, "only one series:\n{out}");
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
}
