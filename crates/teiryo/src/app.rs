//! TUI application state and key handling, independent of rendering and I/O.
//!
//! The TUI has a single view: a live quota dashboard whose lower half is a
//! tabbed detail pane. Three transient overlays (help, shutdown confirmation,
//! settings) draw *over* it rather than replacing it, so there is no screen
//! stack and no per-screen keymap to keep straight.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use teiryo_core::domain::{AccountId, WindowId};
use teiryo_core::{
    AccountStatus, ConfigEdit, ConfigState, PollEvent, ProviderHealth, ProviderId, QuotaSnapshot,
    Request, WindowRollover, WindowView,
};

/// Which detail-pane tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    /// Usage history for the selected window.
    Trend,
    /// Recent poll log across all accounts.
    Activity,
    /// Provider and per-account health.
    Health,
}

impl DetailTab {
    /// Tabs in display order.
    pub const ALL: [DetailTab; 3] = [DetailTab::Trend, DetailTab::Activity, DetailTab::Health];

    /// Tab label for the pane header.
    pub fn label(self) -> &'static str {
        match self {
            DetailTab::Trend => "Trend",
            DetailTab::Activity => "Activity",
            DetailTab::Health => "Health",
        }
    }

    fn shift(self, delta: isize) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0) as isize;
        let len = Self::ALL.len() as isize;
        Self::ALL[(i + delta).rem_euclid(len) as usize]
    }
}

/// Which pane the cursor is in.
///
/// The two panes scroll along different axes — the quota list is a tall column
/// of rows, the detail pane is a wide strip of time or of columns too wide for
/// the terminal — so one keypress means "next row" in one and "further along"
/// in the other. Focus is what disambiguates `j`/`k` between them, and the
/// wheel sets it from whichever pane the pointer is over so the two can never
/// end up scrolling different things. The wheel itself needs no such
/// disambiguation: it says which axis the gesture was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The account/window list.
    List,
    /// The tabbed detail pane.
    Detail,
}

/// How far back the trend chart looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeRange {
    /// Last hour.
    H1,
    /// Last six hours.
    H6,
    /// Last day.
    H24,
    /// Last week.
    D7,
}

impl TimeRange {
    /// Ranges in ascending order.
    pub const ALL: [TimeRange; 4] = [TimeRange::H1, TimeRange::H6, TimeRange::H24, TimeRange::D7];

    /// Short label for the range selector.
    pub fn label(self) -> &'static str {
        match self {
            TimeRange::H1 => "1h",
            TimeRange::H6 => "6h",
            TimeRange::H24 => "24h",
            TimeRange::D7 => "7d",
        }
    }

    /// How far back the range reaches.
    pub fn duration(self) -> Duration {
        match self {
            TimeRange::H1 => Duration::hours(1),
            TimeRange::H6 => Duration::hours(6),
            TimeRange::H24 => Duration::hours(24),
            TimeRange::D7 => Duration::days(7),
        }
    }

    /// Points to ask the daemon for. A chart is at most a few hundred columns
    /// wide even on a large terminal, so requesting more only inflates the
    /// frame — the daemon downsamples to this, preserving peaks.
    pub fn max_points(self) -> u32 {
        480
    }

    fn shift(self, delta: isize) -> Self {
        let i = Self::ALL.iter().position(|r| *r == self).unwrap_or(0) as isize;
        let last = Self::ALL.len() as isize - 1;
        Self::ALL[(i + delta).clamp(0, last) as usize]
    }
}

/// A modal drawn over the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// Full keymap.
    Help,
    /// Two-step daemon shutdown confirmation.
    ConfirmShutdown,
    /// Daemon settings: what `config.toml` currently says, and an editor for
    /// it. Stays `Copy` — the cursor lives in [`App::settings_cursor`], the
    /// way the detail pane's data lives in `App` rather than in `DetailTab`.
    Settings,
}

/// One editable line in the settings overlay.
///
/// Derived from [`ConfigState`] on every frame rather than stored, so an
/// external `config.toml` edit that adds or removes a provider cannot leave
/// the cursor pointing at a row that no longer exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    /// The global poll interval.
    GlobalInterval,
    /// Whether a provider is polled, by index into `ConfigView::providers`.
    ProviderEnabled(usize),
    /// A provider's interval override, by the same index.
    ProviderInterval(usize),
}

/// Intervals a keypress moves between, in seconds.
///
/// A ladder rather than ±1s: one press should be a change worth making, and
/// the bottom rung is the daemon's own floor, so the overlay cannot propose a
/// value that would come back rejected.
pub const INTERVAL_RUNGS: [u32; 10] = [10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600];

/// The rung `delta` steps from `current`, clamped at both ends. A value
/// between rungs (hand-written in `config.toml`) moves to the neighbouring
/// rung in the requested direction rather than snapping backwards.
fn step_interval(current: u32, delta: isize) -> u32 {
    let at_or_after = INTERVAL_RUNGS.iter().position(|r| *r >= current);
    let index = match (at_or_after, delta) {
        (Some(i), _) if INTERVAL_RUNGS[i] == current => i as isize + delta,
        // Between rungs: up goes to the next one above, down to the one below.
        (Some(i), d) if d > 0 => i as isize,
        (Some(i), _) => i as isize - 1,
        // Above every rung.
        (None, d) if d > 0 => INTERVAL_RUNGS.len() as isize - 1,
        (None, _) => INTERVAL_RUNGS.len() as isize - 1,
    };
    INTERVAL_RUNGS[index.clamp(0, INTERVAL_RUNGS.len() as isize - 1) as usize]
}

/// One selectable dashboard row: an account header or one of its windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowRef {
    /// Index into [`App::statuses`].
    pub account: usize,
    /// Window index within the account, `None` for the account header row.
    pub window: Option<usize>,
}

/// What is selected, by stable identity rather than row index.
///
/// The daemon may reorder accounts or grow an account's window list between
/// refreshes; an index would silently point at a different row when that
/// happens, so selection is resolved from ids on every frame instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Account owning the selected row.
    pub account: AccountId,
    /// Selected window, or `None` for the account's header row.
    pub window: Option<WindowId>,
}

/// Loaded history for one window.
pub struct Trend {
    /// Account the series belongs to.
    pub account: AccountId,
    /// Window the series belongs to.
    pub window: WindowId,
    /// Display title, "account — window".
    pub title: String,
    /// Snapshots, oldest first.
    pub snapshots: Vec<QuotaSnapshot>,
    /// Rollovers observed over the same interval, oldest first. Recorded at
    /// poll time rather than inferred from `snapshots`, which the daemon
    /// downsamples — a boundary read off a peak-preserving bucket would be
    /// placed up to a bucket's width away from where it happened.
    pub rollovers: Vec<WindowRollover>,
    /// Range the series was fetched for, so the axis can be labeled.
    pub range: TimeRange,
    /// Right edge of the charted interval — `now` unless the view is panned
    /// into the past. The chart maps x against this rather than against the
    /// wall clock, or a panned series would drift rightwards every tick.
    pub until: DateTime<Utc>,
    /// When the daemon's oldest snapshot for this window was taken, whatever
    /// interval was fetched — `None` when it holds none. This is where the
    /// chart stops when it is scrolled backwards: the series cannot be
    /// followed past its own beginning.
    pub earliest: Option<DateTime<Utc>>,
    /// The clock reading this page was fetched against. With `earliest` it
    /// says how much history exists, so the app can bound a pan without
    /// reading the clock itself — the same reason every other function here
    /// takes `now` as an argument.
    pub fetched_at: DateTime<Utc>,
}

/// What the detail pane needs fetched for its current tab.
pub enum DetailQuery {
    /// History for the selected window.
    Trend {
        /// Account to query.
        account: AccountId,
        /// Window to chart.
        window: WindowId,
        /// Display title for the pane.
        title: String,
        /// Range being charted.
        range: TimeRange,
        /// Right edge of the charted interval.
        until: DateTime<Utc>,
        /// The request to send.
        request: Request,
    },
    /// Recent poll log.
    Activity(Request),
    /// Provider health.
    Health(Request),
}

/// What the event loop should do after a key press.
#[derive(Debug)]
pub enum Action {
    /// Nothing.
    None,
    /// Leave the TUI (daemon keeps running).
    Quit,
    /// Send requests whose replies only matter as acks/errors.
    Send(Vec<Request>),
    /// Refetch whatever the detail pane is currently showing.
    ReloadDetail,
    /// Fetch the daemon's current settings into [`App::config`].
    LoadConfig,
    /// Ask the daemon to make one settings change. Distinct from
    /// [`Action::Send`] because the reply carries the resulting settings, and
    /// a rejection is the only way the user learns the edit did not take.
    EditConfig(ConfigEdit),
    /// Confirmed: stop the daemon, then quit.
    ShutdownDaemon,
}

/// Application state.
pub struct App {
    /// Latest account statuses, sorted by (provider, label).
    pub statuses: Vec<AccountStatus>,
    /// Selected row, by stable identity.
    selection: Option<Selection>,
    /// Whether the user has moved the selection themselves. Until they have,
    /// the selection is re-derived on every refresh.
    user_moved: bool,
    /// Scroll/highlight state for the quota list.
    pub list_state: ListState,
    /// Which pane the cursor is in, and so which axis a scroll moves along.
    pub focus: Pane,
    /// Active detail tab.
    pub detail: DetailTab,
    /// Whether the detail pane is shown (also suppressed on short terminals).
    pub detail_visible: bool,
    /// Range for the trend chart.
    pub range: TimeRange,
    /// Vertical scroll offset, in rows, within the Activity/Health tabs.
    pub detail_scroll: usize,
    /// Horizontal scroll offset, in columns, for Activity and Health — their
    /// rows are wider than most terminals and would otherwise be cut off with
    /// no way to read the rest.
    pub detail_hscroll: usize,
    /// Largest useful [`App::detail_hscroll`], recorded by the renderer, which
    /// is the only place that knows both the content width and the viewport.
    /// Zero until the first frame, which merely means the first press cannot
    /// overshoot — it clamps on the next one.
    pub detail_hscroll_max: usize,
    /// Largest useful [`App::detail_scroll`], recorded by the same pass and
    /// for the same reason: without it a page-down runs off the end of the log
    /// into blank rows the user then has to scroll back through.
    pub detail_scroll_max: usize,
    /// How far the trend chart is panned into the past, in seconds. Zero means
    /// the chart ends at "now" and keeps following it. Kept in seconds rather
    /// than in steps so the wheel and the keyboard can move by different
    /// amounts along one axis, and so the clamp can land exactly on the oldest
    /// stored point rather than on the nearest step to it.
    pub trend_pan_secs: i64,
    /// Where the quota list was drawn, for hit-testing the pointer.
    pub quotas_area: Rect,
    /// Where the detail pane was drawn; empty while it is hidden.
    pub detail_area: Rect,
    /// Loaded trend series, if any.
    pub trend: Option<Trend>,
    /// Recent history per window, for the burn rates the rows draw. Keyed by
    /// window rather than held on [`Trend`] because every row needs its own,
    /// not just the selected one.
    recent: HashMap<(AccountId, WindowId), Vec<QuotaSnapshot>>,
    /// Loaded poll log.
    pub activity: Vec<PollEvent>,
    /// Loaded provider health.
    pub health: Vec<ProviderHealth>,
    /// Modal drawn over the dashboard, if any.
    pub overlay: Option<Overlay>,
    /// The daemon's settings and how its last read of `config.toml` went.
    /// `None` until the first `GetConfig` reply lands.
    pub config: Option<ConfigState>,
    /// Cursor into [`App::settings_rows`], clamped on every use.
    pub settings_cursor: usize,
    /// Accounts with a manual poll in flight, for the spinner.
    pub polling: HashSet<AccountId>,
    /// Spinner frame counter, advanced once per tick.
    pub spinner: usize,
    /// Status-line error, if any.
    pub error: Option<String>,
    /// True when the daemon connection is lost and reconnection is pending.
    pub disconnected: bool,
    /// Time of the most recent update, for the header.
    pub last_update: Option<DateTime<Utc>>,
}

impl App {
    /// Fresh app with no data yet.
    pub fn new() -> Self {
        Self {
            statuses: Vec::new(),
            selection: None,
            user_moved: false,
            list_state: ListState::default(),
            focus: Pane::List,
            detail: DetailTab::Trend,
            detail_visible: true,
            range: TimeRange::H24,
            detail_scroll: 0,
            detail_hscroll: 0,
            detail_hscroll_max: 0,
            detail_scroll_max: 0,
            trend_pan_secs: 0,
            quotas_area: Rect::ZERO,
            detail_area: Rect::ZERO,
            trend: None,
            recent: HashMap::new(),
            activity: Vec::new(),
            health: Vec::new(),
            overlay: None,
            config: None,
            settings_cursor: 0,
            polling: HashSet::new(),
            spinner: 0,
            error: None,
            disconnected: false,
            last_update: None,
        }
    }

    /// Replace statuses (sorted for stable display), keeping the selection
    /// pinned to the same account/window where it still exists.
    pub fn set_statuses(&mut self, mut statuses: Vec<AccountStatus>) {
        statuses.sort_by(|a, b| {
            (&a.account.provider, &a.account.label).cmp(&(&b.account.provider, &b.account.label))
        });
        let previous_index = self.selected_index();
        self.statuses = statuses;
        if self.user_moved {
            if self.selected_index().is_none() {
                // The selected row is gone: fall back to the nearest row at
                // the old position rather than jumping to the top.
                let len = self.rows().len();
                self.selection = (len > 0)
                    .then(|| self.selection_at(previous_index.unwrap_or(0).min(len - 1)))
                    .flatten();
            }
        } else {
            // Until the user navigates, keep re-deriving the default: land on
            // a window rather than an account header so the trend pane has
            // something to chart. This must be re-evaluated on every refresh,
            // not just the first — the TUI usually connects before the
            // daemon's startup poll lands, so the initial `Status` has an
            // account with *no* windows yet. Pinning to the header row then
            // would leave it selected once the windows finally arrive, since
            // the header itself never stops resolving.
            self.selection = self.selection_at(self.first_window_row().unwrap_or(0));
        }
        self.sync_list_state();
    }

    fn first_window_row(&self) -> Option<usize> {
        self.rows().iter().position(|row| row.window.is_some())
    }

    /// Flattened selectable rows: each account header followed by its windows.
    pub fn rows(&self) -> Vec<RowRef> {
        let mut rows = Vec::new();
        for (ai, status) in self.statuses.iter().enumerate() {
            rows.push(RowRef {
                account: ai,
                window: None,
            });
            for wi in 0..status.windows.len() {
                rows.push(RowRef {
                    account: ai,
                    window: Some(wi),
                });
            }
        }
        rows
    }

    /// Row index of the current selection, if it still resolves.
    pub fn selected_index(&self) -> Option<usize> {
        let selection = self.selection.as_ref()?;
        self.rows().into_iter().position(|row| {
            let status = &self.statuses[row.account];
            status.account.id == selection.account
                && row.window.map(|wi| &status.windows[wi].window.id) == selection.window.as_ref()
        })
    }

    /// The currently selected row, if any rows exist.
    pub fn selected_row(&self) -> Option<RowRef> {
        let index = self.selected_index()?;
        self.rows().get(index).copied()
    }

    /// The selected window and its owning account, when a window row (not an
    /// account header) is selected.
    pub fn selected_window(&self) -> Option<(&AccountStatus, &WindowView)> {
        let row = self.selected_row()?;
        let status = &self.statuses[row.account];
        Some((status, status.windows.get(row.window?)?))
    }

    fn selection_at(&self, index: usize) -> Option<Selection> {
        let row = self.rows().get(index).copied()?;
        let status = &self.statuses[row.account];
        Some(Selection {
            account: status.account.id.clone(),
            window: row.window.map(|wi| status.windows[wi].window.id.clone()),
        })
    }

    /// Move the selection in response to a key press, which also stops
    /// [`App::set_statuses`] from re-deriving its default.
    fn select(&mut self, index: usize) {
        let target = self.selection_at(index);
        if target != self.selection {
            // A pan belongs to the series it was made on. Another window has
            // its own history, which may not even reach back that far, so
            // carrying the offset across would open the new chart on empty
            // time rather than on its data.
            self.trend_pan_secs = 0;
        }
        self.selection = target;
        self.user_moved = true;
        self.sync_list_state();
    }

    fn sync_list_state(&mut self) {
        self.list_state.select(self.selected_index());
    }

    /// Move the selection by `delta` rows, clamped to the list.
    fn move_selection(&mut self, delta: isize) {
        let len = self.rows().len();
        if len == 0 {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        self.select((current + delta).clamp(0, len as isize - 1) as usize);
    }

    /// Right edge of the charted interval: `now`, less however far the user
    /// has panned back.
    pub fn trend_until(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now - Duration::seconds(self.trend_pan_secs)
    }

    /// Whether the chart's right edge is the present rather than a point the
    /// user has panned back to. Only then does the window's *upcoming* reset
    /// belong just off that edge.
    pub fn trend_is_live(&self) -> bool {
        self.trend_pan_secs == 0
    }

    /// Seconds one `j`/`k` press moves the chart: a fixed fraction of the
    /// visible range, so a press covers the same visible distance at every
    /// zoom level.
    fn trend_key_step_secs(&self) -> i64 {
        (self.range.duration().num_seconds() / TREND_KEY_STEPS_PER_RANGE).max(1)
    }

    /// Seconds one wheel notch moves the chart: one column of the plot. A
    /// notch is a nudge, not a jump — a wheel gesture arrives as a stream of
    /// them, and a keypress-sized step per notch overshoots the whole range in
    /// one flick of the finger.
    fn trend_wheel_step_secs(&self) -> i64 {
        let columns = (i64::from(self.detail_area.width) - TREND_AXIS_COLUMNS).max(1);
        (self.range.duration().num_seconds() / columns).max(1)
    }

    /// The loaded series, but only when it belongs to the row the cursor is
    /// on. A reload is in flight while the selection moves, and the previous
    /// window's history is not an answer about this one — neither for drawing
    /// it, nor for deciding how far back it can be scrolled.
    pub fn charted_trend(&self) -> Option<&Trend> {
        let (status, view) = self.selected_window()?;
        self.trend
            .as_ref()
            .filter(|t| t.account == status.account.id && t.window == view.window.id)
    }

    /// Furthest the chart may pan back, in seconds: far enough to bring the
    /// oldest stored point to the left edge of the plot, and no further.
    ///
    /// Zero whenever the stored history is narrower than the range — including
    /// before the first page has landed. The chart then stays pinned to the
    /// right edge, following the clock as new points arrive, instead of
    /// sliding off into time that nothing was ever recorded in.
    fn max_trend_pan_secs(&self) -> i64 {
        let Some(trend) = self.charted_trend() else {
            return 0;
        };
        let Some(earliest) = trend.earliest else {
            return 0;
        };
        ((trend.fetched_at - earliest).num_seconds() - self.range.duration().num_seconds()).max(0)
    }

    /// Install a freshly fetched series, re-clamping the pan to the history it
    /// reports. Between the request and the reply the extent may have changed
    /// — a different window, a shorter history — and the view must not be left
    /// stranded past the end of it.
    ///
    /// A view already at the left stop is re-pinned there rather than merely
    /// clamped: the pan is measured back from *now*, so holding the number
    /// still would walk the oldest point off the left edge by however long the
    /// refresh took. Pan zero pins the view to the present in exactly the same
    /// way, at the other end.
    pub fn set_trend(&mut self, trend: Trend) {
        let pinned = self.trend_at_oldest();
        self.trend = Some(trend);
        let max = self.max_trend_pan_secs();
        self.trend_pan_secs = if pinned {
            max
        } else {
            self.trend_pan_secs.clamp(0, max)
        };
    }

    /// Install one account's recent history, bucketed per window.
    ///
    /// Replaces everything held for that account, so a window the daemon has
    /// stopped reporting leaves with its series rather than freezing a rate
    /// that no longer has data behind it.
    pub fn set_recent(&mut self, account: &AccountId, snapshots: Vec<QuotaSnapshot>) {
        self.recent.retain(|(a, _), _| a != account);
        for snapshot in snapshots {
            self.recent
                .entry((account.clone(), snapshot.window.clone()))
                .or_default()
                .push(snapshot);
        }
    }

    /// One window's recent readings, oldest first; empty until the first
    /// history reply for its account lands.
    pub fn recent_points(&self, account: &AccountId, window: &WindowId) -> &[QuotaSnapshot] {
        self.recent
            .get(&(account.clone(), window.clone()))
            .map_or(&[], Vec::as_slice)
    }

    /// Pan the chart by `secs`, positive being *later*, clipped at both ends:
    /// the present on the right, the oldest stored point on the left.
    fn pan_trend(&mut self, secs: i64) -> Action {
        let panned = (self.trend_pan_secs - secs).clamp(0, self.max_trend_pan_secs());
        if panned == self.trend_pan_secs {
            return Action::None;
        }
        self.trend_pan_secs = panned;
        // The visible interval moved, so the series must be refetched.
        Action::ReloadDetail
    }

    /// Whether the chart is showing the oldest history there is, and so cannot
    /// be scrolled back any further.
    pub fn trend_at_oldest(&self) -> bool {
        let max = self.max_trend_pan_secs();
        max > 0 && self.trend_pan_secs >= max
    }

    /// Move the cursor to `pane`, revealing the detail pane if that is where it
    /// is going. Returns whatever the move needs fetched.
    fn focus_pane(&mut self, pane: Pane) -> Action {
        if pane == Pane::Detail && !self.detail_visible {
            self.detail_visible = true;
            self.focus = pane;
            return Action::ReloadDetail;
        }
        self.focus = pane;
        Action::None
    }

    /// Scroll the detail pane along its own axis in response to `j`/`k`:
    /// later/earlier for the trend chart, right/left for the Activity and
    /// Health columns. Positive `delta` moves the view towards the right —
    /// later in time, further along a row.
    fn scroll_detail(&mut self, delta: isize) -> Action {
        match self.detail {
            DetailTab::Trend => self.pan_trend(delta as i64 * self.trend_key_step_secs()),
            DetailTab::Activity | DetailTab::Health => {
                self.scroll_detail_columns(delta * DETAIL_HSCROLL_STEP);
                Action::None
            }
        }
    }

    /// Move the text tabs sideways, bounded by what the renderer last measured
    /// as overflowing — scrolling into blank columns is not reading anything,
    /// and every one of them has to be scrolled back through.
    fn scroll_detail_columns(&mut self, delta: isize) {
        let max = self.detail_hscroll_max as isize;
        self.detail_hscroll = (self.detail_hscroll as isize + delta).clamp(0, max) as usize;
    }

    /// Move the text tabs up or down, bounded the same way.
    fn scroll_detail_rows(&mut self, delta: isize) {
        let max = self.detail_scroll_max as isize;
        self.detail_scroll = (self.detail_scroll as isize + delta).clamp(0, max) as usize;
    }

    /// Put the detail pane's horizontal view back at its origin: the left edge
    /// of a row, the present moment on the chart.
    fn reset_detail_scroll(&mut self) -> Action {
        let panned = self.trend_pan_secs > 0;
        self.trend_pan_secs = 0;
        self.detail_hscroll = 0;
        if panned {
            Action::ReloadDetail
        } else {
            Action::None
        }
    }

    /// Record how far the detail pane's content overruns its viewport, in
    /// columns and in rows. Called by the renderer, which is the only place
    /// that knows both the content and the viewport it is drawn into.
    pub fn set_detail_bounds(&mut self, columns: usize, rows: usize) {
        self.detail_hscroll_max = columns;
        self.detail_scroll_max = rows;
        self.detail_hscroll = self.detail_hscroll.min(columns);
        self.detail_scroll = self.detail_scroll.min(rows);
    }

    /// Route a mouse event by the pane the pointer is over, which also moves
    /// the cursor there — the wheel and `j`/`k` should never disagree about
    /// which pane they are scrolling.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        // Overlays cover the panes below them; scrolling what cannot be seen
        // would be action at a distance.
        if self.overlay.is_some() {
            return Action::None;
        }
        // One notch, one line. A wheel gesture arrives as a stream of these
        // events, so anything coarser turns a flick of the finger into a jump
        // across the whole pane.
        let (rows, columns) = match mouse.kind {
            MouseEventKind::ScrollDown => (1, 0),
            MouseEventKind::ScrollUp => (-1, 0),
            MouseEventKind::ScrollRight => (0, 1),
            MouseEventKind::ScrollLeft => (0, -1),
            _ => return Action::None,
        };
        let at = Position::new(mouse.column, mouse.row);
        if self.detail_area.contains(at) {
            self.focus = Pane::Detail;
            self.wheel_detail(rows, columns)
        } else if self.quotas_area.contains(at) {
            self.focus = Pane::List;
            // The list is one column of rows; a sideways gesture has nowhere
            // to go in it.
            if rows == 0 {
                return Action::None;
            }
            self.move_selection(rows);
            Action::ReloadDetail
        } else {
            Action::None
        }
    }

    /// Wheel handling for the detail pane, where each tab has its own axes:
    /// the chart has only time, the text tabs have rows down and columns
    /// across. Unlike `j`/`k`, which must pick one axis per pane, the wheel
    /// reports which one the user asked for.
    fn wheel_detail(&mut self, rows: isize, columns: isize) -> Action {
        match self.detail {
            DetailTab::Trend => {
                // Time is the chart's only axis, so both gestures travel along
                // it — one plot column per notch.
                let delta = if columns != 0 { columns } else { rows };
                self.pan_trend(delta as i64 * self.trend_wheel_step_secs())
            }
            DetailTab::Activity | DetailTab::Health => {
                self.scroll_detail_rows(rows);
                self.scroll_detail_columns(columns);
                Action::None
            }
        }
    }

    /// Distinct providers currently displayed.
    fn providers(&self) -> Vec<ProviderId> {
        let mut providers: Vec<ProviderId> = self
            .statuses
            .iter()
            .map(|s| s.account.provider.clone())
            .collect();
        providers.sort();
        providers.dedup();
        providers
    }

    /// What the detail pane needs fetched right now, if anything.
    ///
    /// Called both on the key presses that change the pane and on every
    /// daemon update, which is what keeps the pane live instead of frozen at
    /// whatever it held when it was opened.
    pub fn detail_query(&self, now: DateTime<Utc>) -> Option<DetailQuery> {
        match self.detail {
            DetailTab::Trend => {
                let (status, view) = self.selected_window()?;
                let until = self.trend_until(now);
                Some(DetailQuery::Trend {
                    account: status.account.id.clone(),
                    window: view.window.id.clone(),
                    title: format!("{} — {}", status.account.label, view.window.label),
                    range: self.range,
                    until,
                    request: Request::History {
                        account: status.account.id.clone(),
                        window: Some(view.window.id.clone()),
                        since: until - self.range.duration(),
                        // `None` means "now" to the daemon, which is what an
                        // unpanned chart wants: it keeps following the clock.
                        until: (self.trend_pan_secs > 0).then_some(until),
                        max_points: Some(self.range.max_points()),
                    },
                })
            }
            DetailTab::Activity => Some(DetailQuery::Activity(Request::RecentPolls {
                limit: RECENT_POLL_LIMIT,
            })),
            DetailTab::Health => Some(DetailQuery::Health(Request::Providers)),
        }
    }

    /// Note that a poll was requested for `account`, so the header can spin.
    fn mark_polling(&mut self, account: AccountId) {
        self.polling.insert(account);
    }

    /// Clear the spinner for an account whose poll has landed.
    pub fn poll_settled(&mut self, account: &AccountId) {
        self.polling.remove(account);
    }

    /// Clear the spinner for every account a refused `PollNow` targeted.
    ///
    /// Without this the spinner runs forever: it is normally stopped by the
    /// poll arriving, and a poll the daemon refused — a disabled provider, say
    /// — never arrives.
    pub fn poll_refused(&mut self, provider: &ProviderId, account: Option<&AccountId>) {
        let targeted: Vec<AccountId> = self
            .statuses
            .iter()
            .map(|s| &s.account)
            .filter(|a| &a.provider == provider)
            .filter(|a| account.is_none_or(|id| &a.id == id))
            .map(|a| a.id.clone())
            .collect();
        for id in targeted {
            self.polling.remove(&id);
        }
    }

    /// The settings overlay's editable lines, in display order. Empty until
    /// the daemon's config has arrived.
    pub fn settings_rows(&self) -> Vec<SettingsRow> {
        let Some(config) = &self.config else {
            return Vec::new();
        };
        let mut rows = vec![SettingsRow::GlobalInterval];
        for i in 0..config.effective.providers.len() {
            rows.push(SettingsRow::ProviderEnabled(i));
            rows.push(SettingsRow::ProviderInterval(i));
        }
        rows
    }

    /// The row under the cursor, if any.
    pub fn settings_row(&self) -> Option<SettingsRow> {
        let rows = self.settings_rows();
        rows.get(self.settings_cursor.min(rows.len().saturating_sub(1)))
            .copied()
    }

    /// Whether the daemon last rejected `config.toml`. Surfaced on the
    /// dashboard too, so a bad hand edit is visible without opening Settings —
    /// otherwise the daemon would look like it was simply ignoring the file.
    pub fn config_error(&self) -> Option<&str> {
        self.config.as_ref()?.error.as_deref()
    }

    fn move_settings_cursor(&mut self, delta: isize) {
        let len = self.settings_rows().len();
        if len == 0 {
            return;
        }
        self.settings_cursor =
            (self.settings_cursor as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    /// Step the interval under the cursor by one rung, or toggle the boolean.
    fn edit_settings(&mut self, delta: isize) -> Action {
        let Some(config) = &self.config else {
            return Action::None;
        };
        let Some(row) = self.settings_row() else {
            return Action::None;
        };
        let view = &config.effective;
        match row {
            SettingsRow::GlobalInterval => {
                let current = view
                    .poll_interval_secs
                    .unwrap_or(view.default_poll_interval_secs);
                Action::EditConfig(ConfigEdit::GlobalPollInterval(Some(step_interval(
                    current, delta,
                ))))
            }
            SettingsRow::ProviderEnabled(i) => {
                let provider = &view.providers[i];
                Action::EditConfig(ConfigEdit::ProviderEnabled {
                    provider: provider.provider.clone(),
                    enabled: !provider.enabled,
                })
            }
            SettingsRow::ProviderInterval(i) => {
                let provider = &view.providers[i];
                Action::EditConfig(ConfigEdit::ProviderPollInterval {
                    provider: provider.provider.clone(),
                    secs: Some(step_interval(provider.effective_poll_interval_secs, delta)),
                })
            }
        }
    }

    /// Drop the override under the cursor, falling back to the inherited
    /// value. Has no meaning for the enabled flag, which always has a value.
    fn clear_setting(&mut self) -> Action {
        let Some(config) = &self.config else {
            return Action::None;
        };
        match self.settings_row() {
            Some(SettingsRow::GlobalInterval) => {
                Action::EditConfig(ConfigEdit::GlobalPollInterval(None))
            }
            Some(SettingsRow::ProviderInterval(i)) => {
                Action::EditConfig(ConfigEdit::ProviderPollInterval {
                    provider: config.effective.providers[i].provider.clone(),
                    secs: None,
                })
            }
            _ => Action::None,
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => {
                self.overlay = None;
                Action::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_settings_cursor(1);
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_settings_cursor(-1);
                Action::None
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('l') | KeyCode::Right => {
                self.edit_settings(1)
            }
            KeyCode::Char('-') | KeyCode::Char('_') | KeyCode::Char('h') | KeyCode::Left => {
                self.edit_settings(-1)
            }
            // Only meaningful on a boolean row; on an interval it would have to
            // pick a direction arbitrarily.
            KeyCode::Enter | KeyCode::Char(' ') => match self.settings_row() {
                Some(SettingsRow::ProviderEnabled(_)) => self.edit_settings(1),
                _ => Action::None,
            },
            KeyCode::Backspace | KeyCode::Delete => self.clear_setting(),
            // Recover from a rejected external edit without leaving the overlay.
            KeyCode::Char('r') => Action::LoadConfig,
            _ => Action::None,
        }
    }

    /// Translate a key press into an [`Action`], mutating view state.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        // Raw mode delivers Ctrl-C as an ordinary key event; without this it
        // would be swallowed and the TUI would feel stuck.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        match self.overlay {
            Some(Overlay::ConfirmShutdown) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Action::ShutdownDaemon,
                _ => {
                    self.overlay = None;
                    Action::None
                }
            },
            Some(Overlay::Help) => {
                self.overlay = None;
                Action::None
            }
            Some(Overlay::Settings) => self.handle_settings_key(key),
            None => self.handle_dashboard_key(key),
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('Q') => {
                self.overlay = Some(Overlay::ConfirmShutdown);
                Action::None
            }
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                Action::None
            }
            KeyCode::Char('s') => {
                self.overlay = Some(Overlay::Settings);
                self.settings_cursor = 0;
                // Refetch rather than trusting the cached copy: the file may
                // have changed while a previous TUI session held it.
                Action::LoadConfig
            }
            KeyCode::Esc => {
                // Step back out of the detail pane first; only once the cursor
                // is already in the list does Esc mean "get out of the way".
                if self.focus == Pane::Detail {
                    self.focus = Pane::List;
                } else {
                    self.detail_visible = false;
                }
                Action::None
            }
            // The scroll keys keep their gesture and change their axis: down
            // the list of windows, or along the detail pane.
            KeyCode::Char('j') | KeyCode::Down => {
                if self.detail_focused() {
                    return self.scroll_detail(1);
                }
                self.move_selection(1);
                Action::ReloadDetail
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.detail_focused() {
                    return self.scroll_detail(-1);
                }
                self.move_selection(-1);
                Action::ReloadDetail
            }
            KeyCode::Char('h') | KeyCode::Left => self.focus_pane(Pane::List),
            KeyCode::Char('l') | KeyCode::Right => self.focus_pane(Pane::Detail),
            KeyCode::Char('g') | KeyCode::Home => {
                if self.detail_focused() {
                    return self.reset_detail_scroll();
                }
                self.select(0);
                Action::ReloadDetail
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.detail_focused() {
                    // A row has a last column to jump to; the chart's far end
                    // is simply the present, which is where `g` lands too —
                    // history has no known far edge to offer instead.
                    if self.detail == DetailTab::Trend {
                        return self.reset_detail_scroll();
                    }
                    self.detail_hscroll = self.detail_hscroll_max;
                    return Action::None;
                }
                let len = self.rows().len();
                if len > 0 {
                    self.select(len - 1);
                }
                Action::ReloadDetail
            }
            KeyCode::PageDown => {
                self.scroll_detail_rows(DETAIL_PAGE);
                Action::None
            }
            KeyCode::PageUp => {
                self.scroll_detail_rows(-DETAIL_PAGE);
                Action::None
            }
            KeyCode::Tab => self.set_tab(self.detail.shift(1)),
            KeyCode::BackTab => self.set_tab(self.detail.shift(-1)),
            KeyCode::Char('1') => self.set_tab(DetailTab::Trend),
            KeyCode::Char('2') => self.set_tab(DetailTab::Activity),
            KeyCode::Char('3') => self.set_tab(DetailTab::Health),
            // A pan is measured in fractions of the range, so it means
            // something different either side of a zoom; start from the
            // present rather than from a distance that no longer applies.
            KeyCode::Char('[') => {
                self.range = self.range.shift(-1);
                self.trend_pan_secs = 0;
                Action::ReloadDetail
            }
            KeyCode::Char(']') => {
                self.range = self.range.shift(1);
                self.trend_pan_secs = 0;
                Action::ReloadDetail
            }
            KeyCode::Char('d') => {
                self.detail_visible = !self.detail_visible;
                if self.detail_visible {
                    Action::ReloadDetail
                } else {
                    // Nothing to point at any more.
                    self.focus = Pane::List;
                    Action::None
                }
            }
            KeyCode::Char('r') | KeyCode::Enter => match self.selected_row() {
                Some(row) => {
                    let account = self.statuses[row.account].account.clone();
                    self.mark_polling(account.id.clone());
                    Action::Send(vec![Request::PollNow {
                        provider: account.provider,
                        account: Some(account.id),
                    }])
                }
                None => Action::None,
            },
            KeyCode::Char('R') => {
                for status in &self.statuses {
                    self.polling.insert(status.account.id.clone());
                }
                Action::Send(
                    self.providers()
                        .into_iter()
                        .map(|provider| Request::PollNow {
                            provider,
                            account: None,
                        })
                        .collect(),
                )
            }
            _ => Action::None,
        }
    }

    fn set_tab(&mut self, tab: DetailTab) -> Action {
        self.detail = tab;
        self.detail_scroll = 0;
        // Columns are a different thing per tab; carrying the offset across
        // would land the new tab mid-word. The trend's pan is a viewport onto
        // time and survives, so switching away and back does not lose the
        // interval the user navigated to.
        self.detail_hscroll = 0;
        self.detail_hscroll_max = 0;
        self.detail_scroll_max = 0;
        self.detail_visible = true;
        Action::ReloadDetail
    }

    /// Whether the cursor is in a detail pane that is actually on screen.
    fn detail_focused(&self) -> bool {
        self.focus == Pane::Detail && self.detail_visible
    }
}

/// Poll events fetched for the Activity tab.
pub const RECENT_POLL_LIMIT: u32 = 100;

/// Rows PageUp/PageDown move the detail pane by.
const DETAIL_PAGE: isize = 5;

/// Columns one `j`/`k` press moves the Activity and Health tabs by. Wide
/// enough that a press visibly advances, narrow enough to stop inside a column
/// rather than skipping past it. The wheel moves by one column instead — a
/// notch is a nudge, a keypress is a stride.
const DETAIL_HSCROLL_STEP: isize = 6;

/// Pan steps that make up one full trend range for a keypress, so a press
/// always moves the same visible distance regardless of the zoom level.
const TREND_KEY_STEPS_PER_RANGE: i64 = 8;

/// Columns of the detail pane the trend chart does *not* plot into: its two
/// borders and the y-axis labels. An estimate — the axis is as wide as its
/// widest label — but it only sizes a wheel notch, where being a column out is
/// imperceptible.
const TREND_AXIS_COLUMNS: i64 = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use teiryo_core::domain::{QuotaUnit, QuotaWindow, ResetKind, WindowScope};
    use teiryo_core::{Account, BarStyle, RenderHint};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn view(id: &str) -> WindowView {
        WindowView {
            window: QuotaWindow {
                id: WindowId::from(id),
                label: id.to_owned(),
                scope: WindowScope::AccountWide,
                reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(3600)),
                unit: QuotaUnit::Percent,
                used: 10.0,
                limit: None,
                reset_at: None,
            },
            hint: RenderHint {
                style: BarStyle::Percent,
                warn_threshold: 0.8,
                critical_threshold: 0.95,
                note: None,
            },
        }
    }

    fn status(provider: &str, label: &str, windows: usize) -> AccountStatus {
        AccountStatus {
            account: Account {
                id: AccountId::from(format!("{provider}:{label}").as_str()),
                provider: provider.into(),
                label: label.into(),
            },
            windows: (0..windows).map(|i| view(&format!("w{i}"))).collect(),
            last_poll: None,
            last_success: None,
            poll_interval_secs: 60,
        }
    }

    #[test]
    fn rows_flatten_accounts_and_windows() {
        let mut app = App::new();
        app.set_statuses(vec![
            status("claude", "personal", 2),
            status("openai", "p", 1),
        ]);
        let rows = app.rows();
        assert_eq!(rows.len(), 5); // 2 headers + 3 windows
        assert_eq!(rows[0].window, None);
        assert_eq!(rows[1].window, Some(0));
    }

    #[test]
    fn first_load_selects_a_window_not_the_account_header() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 2)]);
        assert_eq!(app.selected_index(), Some(1));
        assert!(app.selected_window().is_some());

        // An account with no windows has only its header to land on.
        let mut empty = App::new();
        empty.set_statuses(vec![status("claude", "a", 0)]);
        assert_eq!(empty.selected_index(), Some(0));
    }

    /// The TUI normally connects before the daemon's startup poll lands, so
    /// the first `Status` carries an account with no windows yet. The default
    /// selection must follow the windows in when they appear rather than
    /// staying stuck on the account header.
    #[test]
    fn default_selection_follows_windows_arriving_late() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 0)]);
        assert!(app.selected_window().is_none());

        app.set_statuses(vec![status("claude", "a", 2)]);
        assert_eq!(app.selected_index(), Some(1));
        assert!(app.selected_window().is_some());
    }

    /// …but once the user picks a row, refreshes must leave it alone.
    #[test]
    fn explicit_selection_survives_refreshes() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 3)]);
        app.handle_key(code(KeyCode::Home)); // deliberately the header row
        assert_eq!(app.selected_index(), Some(0));

        app.set_statuses(vec![status("claude", "a", 3)]);
        assert_eq!(app.selected_index(), Some(0), "refresh overrode the user");
    }

    #[test]
    fn selection_clamps_on_shrink() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 3)]);
        app.handle_key(code(KeyCode::End));
        assert_eq!(app.selected_index(), Some(3));
        app.set_statuses(vec![status("claude", "a", 0)]);
        assert_eq!(app.selected_index(), Some(0));
    }

    #[test]
    fn selection_follows_the_window_across_a_reorder() {
        let mut app = App::new();
        app.set_statuses(vec![status("aaa", "first", 1), status("zzz", "second", 2)]);
        // Select the second window of the "zzz" account (row index 4).
        app.handle_key(code(KeyCode::End));
        let before = app.selected_window().unwrap().1.window.id.clone();
        assert_eq!(before, WindowId::from("w1"));

        // A new account sorts ahead of both, shifting every row index down.
        app.set_statuses(vec![
            status("zzz", "second", 2),
            status("aaa", "first", 1),
            status("aaa", "extra", 4),
        ]);
        let (status, view) = app.selected_window().expect("selection survived");
        assert_eq!(status.account.id, AccountId::from("zzz:second"));
        assert_eq!(view.window.id, WindowId::from("w1"));
    }

    #[test]
    fn shutdown_needs_confirmation() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 1)]);
        assert!(matches!(app.handle_key(key('Q')), Action::None));
        assert_eq!(app.overlay, Some(Overlay::ConfirmShutdown));
        assert!(matches!(app.handle_key(key('n')), Action::None));
        assert_eq!(app.overlay, None);
        app.handle_key(key('Q'));
        assert!(matches!(app.handle_key(key('y')), Action::ShutdownDaemon));
    }

    #[test]
    fn help_overlay_closes_on_any_key() {
        let mut app = App::new();
        app.handle_key(key('?'));
        assert_eq!(app.overlay, Some(Overlay::Help));
        app.handle_key(key('j'));
        assert_eq!(app.overlay, None);
    }

    /// A poll the daemon refuses never lands, so nothing else would clear its
    /// spinner and the account would appear to be polling forever.
    #[test]
    fn a_refused_poll_stops_its_spinner() {
        let mut app = App::new();
        app.set_statuses(vec![
            status("claude", "a", 1),
            status("claude", "b", 1),
            status("openai", "c", 1),
        ]);
        app.handle_key(key('R'));
        assert_eq!(app.polling.len(), 3);

        // Refusing one provider leaves the other's spinner alone.
        app.poll_refused(&"claude".to_owned(), None);
        assert_eq!(app.polling.len(), 1);
        app.poll_refused(&"openai".to_owned(), Some(&AccountId::from("openai:c")));
        assert!(app.polling.is_empty());
    }

    #[test]
    fn poll_all_targets_each_provider_once() {
        let mut app = App::new();
        app.set_statuses(vec![
            status("claude", "a", 1),
            status("claude", "b", 1),
            status("openai", "c", 1),
        ]);
        match app.handle_key(key('R')) {
            Action::Send(reqs) => assert_eq!(reqs.len(), 2),
            _ => panic!("expected Send"),
        }
        // Every account spins until its poll lands.
        assert_eq!(app.polling.len(), 3);
        app.poll_settled(&AccountId::from("claude:a"));
        assert_eq!(app.polling.len(), 2);
    }

    #[test]
    fn tabs_cycle_both_ways_and_reset_scroll() {
        let mut app = App::new();
        app.detail_scroll = 12;
        app.handle_key(code(KeyCode::Tab));
        assert_eq!(app.detail, DetailTab::Activity);
        assert_eq!(app.detail_scroll, 0);
        app.handle_key(code(KeyCode::Tab));
        assert_eq!(app.detail, DetailTab::Health);
        app.handle_key(code(KeyCode::Tab));
        assert_eq!(app.detail, DetailTab::Trend); // wraps
        app.handle_key(code(KeyCode::BackTab));
        assert_eq!(app.detail, DetailTab::Health);
        app.handle_key(key('2'));
        assert_eq!(app.detail, DetailTab::Activity);
    }

    #[test]
    fn range_clamps_at_both_ends() {
        let mut app = App::new();
        assert_eq!(app.range, TimeRange::H24);
        app.handle_key(key(']'));
        assert_eq!(app.range, TimeRange::D7);
        app.handle_key(key(']'));
        assert_eq!(app.range, TimeRange::D7); // no wrap past the widest range
        for _ in 0..5 {
            app.handle_key(key('['));
        }
        assert_eq!(app.range, TimeRange::H1);
    }

    #[test]
    fn detail_query_follows_the_active_tab() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 2)]);
        let now = Utc::now();

        // An account header row has no window to chart.
        app.handle_key(code(KeyCode::Home));
        assert!(app.detail_query(now).is_none());

        app.handle_key(key('j'));
        match app.detail_query(now) {
            Some(DetailQuery::Trend { request, .. }) => assert!(matches!(
                request,
                Request::History {
                    max_points: Some(_),
                    ..
                }
            )),
            _ => panic!("expected a Trend query"),
        }

        app.handle_key(key('3'));
        assert!(matches!(
            app.detail_query(now),
            Some(DetailQuery::Health(Request::Providers))
        ));
    }

    fn config_state() -> ConfigState {
        use teiryo_core::{ConfigView, ProviderSettings};
        ConfigState {
            path: "/home/u/.config/teiryo/config.toml".into(),
            generation: 3,
            effective: ConfigView {
                poll_interval_secs: None,
                default_poll_interval_secs: 60,
                min_poll_interval_secs: 10,
                providers: vec![ProviderSettings {
                    provider: "claude".into(),
                    enabled: true,
                    poll_interval_secs: Some(30),
                    effective_poll_interval_secs: 30,
                }],
            },
            loaded_at: Utc::now(),
            warnings: vec!["unknown key `retrys` — ignored".into()],
            error: None,
        }
    }

    fn with_config() -> App {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 1)]);
        app.handle_key(key('s'));
        app.config = Some(config_state());
        app
    }

    #[test]
    fn settings_opens_and_fetches() {
        let mut app = App::new();
        assert!(matches!(app.handle_key(key('s')), Action::LoadConfig));
        assert_eq!(app.overlay, Some(Overlay::Settings));
        // Nothing to edit until the reply lands, and no panic for trying.
        assert!(app.settings_rows().is_empty());
        assert!(matches!(app.handle_key(key('+')), Action::None));

        app.config = Some(config_state());
        assert_eq!(app.settings_rows().len(), 3); // global + enabled + interval
        assert!(matches!(app.handle_key(code(KeyCode::Esc)), Action::None));
        assert_eq!(app.overlay, None);
    }

    /// Stepping starts from the *effective* value, so the first press changes
    /// what the user is looking at rather than jumping from an unset override.
    #[test]
    fn adjusting_steps_along_the_rung_ladder() {
        let mut app = with_config();
        // Global row, showing the 60s default because nothing overrides it.
        match app.handle_key(key('+')) {
            Action::EditConfig(ConfigEdit::GlobalPollInterval(Some(secs))) => {
                assert_eq!(secs, 120);
            }
            other => panic!("expected a global interval edit, got {other:?}"),
        }
        match app.handle_key(key('-')) {
            Action::EditConfig(ConfigEdit::GlobalPollInterval(Some(secs))) => {
                assert_eq!(secs, 30);
            }
            other => panic!("expected a global interval edit, got {other:?}"),
        }

        // Down twice: past the enabled row onto the provider's interval.
        app.handle_key(key('j'));
        app.handle_key(key('j'));
        match app.handle_key(key('+')) {
            Action::EditConfig(ConfigEdit::ProviderPollInterval { provider, secs }) => {
                assert_eq!(provider, "claude");
                assert_eq!(secs, Some(60));
            }
            other => panic!("expected a provider interval edit, got {other:?}"),
        }
        // Backspace drops the override rather than picking a value.
        match app.handle_key(code(KeyCode::Backspace)) {
            Action::EditConfig(ConfigEdit::ProviderPollInterval { secs: None, .. }) => {}
            other => panic!("expected the override to be cleared, got {other:?}"),
        }
    }

    /// The floor and ceiling of the ladder hold; a keypress can never propose
    /// a value the daemon would reject.
    #[test]
    fn the_rung_ladder_clamps_at_both_ends() {
        assert_eq!(step_interval(INTERVAL_RUNGS[0], -1), INTERVAL_RUNGS[0]);
        let top = INTERVAL_RUNGS[INTERVAL_RUNGS.len() - 1];
        assert_eq!(step_interval(top, 1), top);
        // A hand-written value between rungs moves to the neighbour in the
        // requested direction, not backwards past it.
        assert_eq!(step_interval(45, 1), 60);
        assert_eq!(step_interval(45, -1), 30);
        // And one above every rung still comes down.
        assert_eq!(step_interval(7_200, -1), top);
    }

    #[test]
    fn toggling_enabled_flips_it() {
        let mut app = with_config();
        app.handle_key(key('j')); // onto the provider's enabled row
        match app.handle_key(code(KeyCode::Enter)) {
            Action::EditConfig(ConfigEdit::ProviderEnabled { provider, enabled }) => {
                assert_eq!(provider, "claude");
                assert!(!enabled, "toggling an enabled provider must disable it");
            }
            other => panic!("expected an enabled toggle, got {other:?}"),
        }
        // Enter is meaningless on an interval row — it has no "other" value.
        app.handle_key(key('j'));
        assert!(matches!(app.handle_key(code(KeyCode::Enter)), Action::None));
    }

    /// A cursor left pointing past the end by an external edit must not panic
    /// or silently edit the wrong row.
    #[test]
    fn a_shrinking_config_cannot_strand_the_cursor() {
        let mut app = with_config();
        app.handle_key(code(KeyCode::Down));
        app.handle_key(code(KeyCode::Down));
        assert_eq!(app.settings_row(), Some(SettingsRow::ProviderInterval(0)));

        let mut shrunk = config_state();
        shrunk.effective.providers.clear();
        app.config = Some(shrunk);
        assert_eq!(app.settings_row(), Some(SettingsRow::GlobalInterval));
    }

    #[test]
    fn a_rejected_config_is_reported() {
        let mut app = with_config();
        assert_eq!(app.config_error(), None);
        let mut bad = config_state();
        bad.error = Some("`poll_interval_secs` must not be negative".into());
        app.config = Some(bad);
        assert!(app.config_error().is_some());
    }

    /// The point of the focus concept: one gesture, two axes. In the list
    /// `j`/`k` walk the rows; in the detail pane they move along it instead,
    /// and must leave the selection — and so the charted window — alone.
    #[test]
    fn the_scroll_keys_change_axis_with_the_cursor() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 3)]);
        app.detail = DetailTab::Activity;
        app.detail_hscroll_max = 40;

        app.handle_key(key('j'));
        assert_eq!(app.selected_index(), Some(2), "the list still scrolls");

        app.handle_key(key('l'));
        assert_eq!(app.focus, Pane::Detail);
        app.handle_key(key('j'));
        app.handle_key(key('j'));
        assert_eq!(app.detail_hscroll, 2 * DETAIL_HSCROLL_STEP as usize);
        assert_eq!(app.selected_index(), Some(2), "the selection was dragged");

        app.handle_key(key('k'));
        assert_eq!(app.detail_hscroll, DETAIL_HSCROLL_STEP as usize);
        // Esc steps back out of the pane before it collapses it.
        app.handle_key(code(KeyCode::Esc));
        assert_eq!(app.focus, Pane::List);
        assert!(app.detail_visible);
        app.handle_key(code(KeyCode::Esc));
        assert!(!app.detail_visible);
    }

    /// Only the renderer knows how far the content overruns the viewport, so
    /// the offsets are bounded by what it last measured rather than running
    /// off into empty space the user then has to scroll back through.
    #[test]
    fn detail_scrolling_stops_at_the_measured_edges() {
        let mut app = App::new();
        app.detail = DetailTab::Health;
        app.focus = Pane::Detail;
        app.set_detail_bounds(10, 7);
        for _ in 0..20 {
            app.handle_key(key('j'));
        }
        assert_eq!(app.detail_hscroll, 10);
        app.handle_key(key('g'));
        assert_eq!(app.detail_hscroll, 0);
        app.handle_key(code(KeyCode::End));
        assert_eq!(app.detail_hscroll, 10);

        // Paging down stops at the last row rather than scrolling the content
        // off the top into blank rows.
        for _ in 0..5 {
            app.handle_key(code(KeyCode::PageDown));
        }
        assert_eq!(app.detail_scroll, 7);
        for _ in 0..5 {
            app.handle_key(code(KeyCode::PageUp));
        }
        assert_eq!(app.detail_scroll, 0);

        // A narrower frame, or a shorter log, pulls both offsets back with it.
        app.handle_key(code(KeyCode::End));
        app.handle_key(code(KeyCode::PageDown));
        app.set_detail_bounds(4, 2);
        assert_eq!(app.detail_hscroll, 4);
        assert_eq!(app.detail_scroll, 2);
    }

    /// A loaded series for the selected window, with `history` of stored
    /// history behind it — which is what bounds how far back it can be
    /// scrolled.
    fn trend_with_history(app: &App, history: Duration, now: DateTime<Utc>) -> Trend {
        let (status, view) = app.selected_window().expect("a window is selected");
        Trend {
            account: status.account.id.clone(),
            window: view.window.id.clone(),
            title: "a — w0".into(),
            snapshots: Vec::new(),
            rollovers: Vec::new(),
            range: app.range,
            until: app.trend_until(now),
            earliest: Some(now - history),
            fetched_at: now,
        }
    }

    /// Panning the chart is a horizontal scroll through time: it moves the
    /// requested interval, not just the drawing.
    #[test]
    fn panning_the_trend_moves_the_requested_interval() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 1)]);
        app.focus = Pane::Detail;
        let now = Utc::now();
        // A week of stored history behind the default 24h range.
        app.set_trend(trend_with_history(&app, Duration::days(7), now));

        // Unpanned, `until` stays open so the chart keeps following the clock.
        match app.detail_query(now) {
            Some(DetailQuery::Trend { request, .. }) => {
                assert!(matches!(request, Request::History { until: None, .. }))
            }
            _ => panic!("expected a Trend query"),
        }

        app.handle_key(key('k')); // one step back
        app.handle_key(key('k'));
        let step = Duration::seconds(app.range.duration().num_seconds() / 8);
        match app.detail_query(now) {
            Some(DetailQuery::Trend { until, request, .. }) => {
                assert_eq!(until, now - step * 2);
                match request {
                    Request::History { since, until, .. } => {
                        assert_eq!(until, Some(now - step * 2));
                        assert_eq!(since, now - step * 2 - app.range.duration());
                    }
                    other => panic!("expected History, got {other:?}"),
                }
            }
            _ => panic!("expected a Trend query"),
        }

        // Forward again, and no further: the chart cannot pan past the present.
        for _ in 0..5 {
            app.handle_key(key('j'));
        }
        assert_eq!(app.trend_pan_secs, 0);
        assert_eq!(app.trend_until(now), now);

        // Backwards it stops with the oldest stored point on the left edge,
        // rather than scrolling on into time nothing was recorded in.
        for _ in 0..50 {
            app.handle_key(key('k'));
        }
        let oldest = Duration::days(7) - app.range.duration();
        assert_eq!(app.trend_pan_secs, oldest.num_seconds());
        assert!(app.trend_at_oldest());

        // The pan is measured back from *now*, so a view parked at the oldest
        // point has to be re-pinned as the clock moves on, or five minutes of
        // updates would walk that point five minutes off the left edge.
        let later = now + Duration::minutes(5);
        app.set_trend(trend_with_history(
            &app,
            Duration::days(7) + Duration::minutes(5),
            later,
        ));
        assert!(app.trend_at_oldest());
        assert_eq!(
            app.trend_pan_secs,
            (oldest + Duration::minutes(5)).num_seconds()
        );
        assert_eq!(
            app.trend_until(later) - app.range.duration(),
            now - Duration::days(7),
            "the oldest point stayed on the left edge"
        );

        // `g` is the way back, and a zoom starts from the present again.
        app.handle_key(key('g'));
        assert_eq!(app.trend_pan_secs, 0);
        app.handle_key(key('k'));
        app.handle_key(key(']'));
        assert_eq!(app.trend_pan_secs, 0);
    }

    /// History narrower than the range has nothing to scroll through: the view
    /// stays pinned to the right edge, which is also what keeps it following
    /// the clock as new points land.
    #[test]
    fn a_short_history_pins_the_chart_to_the_present() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 1)]);
        app.focus = Pane::Detail;
        app.detail_area = Rect::new(0, 16, 80, 16);
        let now = Utc::now();
        // Two hours of history against the default 24h range.
        app.set_trend(trend_with_history(&app, Duration::hours(2), now));

        for _ in 0..10 {
            app.handle_key(key('k'));
            app.handle_mouse(wheel(MouseEventKind::ScrollUp, 10, 20));
        }
        assert_eq!(app.trend_pan_secs, 0);
        assert_eq!(app.trend_until(now), now);
        assert!(
            !app.trend_at_oldest(),
            "there is nothing to be at the end of"
        );
        // The request keeps `until` open, so each reply reaches the newest
        // point instead of the one the view was pinned at.
        match app.detail_query(now) {
            Some(DetailQuery::Trend { request, .. }) => {
                assert!(matches!(request, Request::History { until: None, .. }))
            }
            _ => panic!("expected a Trend query"),
        }

        // No history at all is the same story.
        let mut trend = trend_with_history(&app, Duration::hours(2), now);
        trend.earliest = None;
        app.set_trend(trend);
        app.handle_key(key('k'));
        assert_eq!(app.trend_pan_secs, 0);
    }

    /// A page reporting less history than the view is panned into pulls it
    /// back. Otherwise the chart sits on empty time, and only a keypress in
    /// the right direction gets out of it.
    #[test]
    fn a_shorter_history_pulls_a_panned_chart_back() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 1)]);
        app.focus = Pane::Detail;
        let now = Utc::now();
        app.set_trend(trend_with_history(&app, Duration::days(7), now));
        for _ in 0..4 {
            app.handle_key(key('k'));
        }
        assert_eq!(app.trend_pan_secs, Duration::hours(12).num_seconds());

        app.set_trend(trend_with_history(&app, Duration::hours(25), now));
        assert_eq!(app.trend_pan_secs, Duration::hours(1).num_seconds());
    }

    /// A pan belongs to the window it was made on: the next window's history
    /// may not reach back nearly as far, so its chart opens on the present.
    #[test]
    fn moving_the_selection_returns_the_chart_to_the_present() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 2)]);
        app.focus = Pane::Detail;
        let now = Utc::now();
        app.set_trend(trend_with_history(&app, Duration::days(7), now));
        app.handle_key(key('k'));
        assert!(app.trend_pan_secs > 0);

        app.focus = Pane::List;
        app.handle_key(key('j'));
        assert_eq!(app.trend_pan_secs, 0);
    }

    fn wheel(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The wheel scrolls whatever it is pointing at, and moves the cursor
    /// there — otherwise the next `j` would scroll a different pane than the
    /// notch just did. Unlike `j`/`k` it also says which axis it moved along,
    /// so in the text tabs it scrolls rows rather than columns.
    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 3)]);
        app.detail = DetailTab::Activity;
        app.set_detail_bounds(40, 20);
        app.quotas_area = Rect::new(0, 4, 80, 12);
        app.detail_area = Rect::new(0, 16, 80, 16);

        app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 6));
        assert_eq!(app.focus, Pane::List);
        assert_eq!(app.selected_index(), Some(2));

        app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 20));
        assert_eq!(app.focus, Pane::Detail);
        assert_eq!(app.detail_scroll, 1);
        assert_eq!(app.detail_hscroll, 0, "a vertical notch is not sideways");
        assert_eq!(app.selected_index(), Some(2), "the list must not follow");
        // A trackpad's sideways gesture is the one that moves columns.
        app.handle_mouse(wheel(MouseEventKind::ScrollRight, 10, 20));
        assert_eq!(app.detail_hscroll, 1);
        app.handle_mouse(wheel(MouseEventKind::ScrollLeft, 10, 20));
        assert_eq!(app.detail_hscroll, 0);
        // The list has no sideways axis for that gesture to move.
        app.handle_mouse(wheel(MouseEventKind::ScrollRight, 10, 6));
        assert_eq!(app.selected_index(), Some(2));

        // Outside both panes, and behind an overlay, the wheel does nothing.
        app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 0));
        assert_eq!(app.selected_index(), Some(2));
        app.overlay = Some(Overlay::Help);
        app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 6));
        assert_eq!(app.selected_index(), Some(2));
    }

    /// One notch, one line, in either direction — and never past the end of
    /// the content. A wheel gesture arrives as a stream of these events, so a
    /// larger step per notch is a jump across the pane.
    #[test]
    fn the_wheel_moves_one_line_per_notch_and_stops_at_the_ends() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 3)]);
        app.detail = DetailTab::Activity;
        app.detail_area = Rect::new(0, 16, 80, 16);
        app.quotas_area = Rect::new(0, 4, 80, 12);
        app.set_detail_bounds(40, 3);

        for expected in 1..=3 {
            app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 20));
            assert_eq!(app.detail_scroll, expected);
        }
        for _ in 0..5 {
            app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 20));
        }
        assert_eq!(app.detail_scroll, 3, "the log has no more rows to reveal");
        for _ in 0..10 {
            app.handle_mouse(wheel(MouseEventKind::ScrollUp, 10, 20));
        }
        assert_eq!(app.detail_scroll, 0);

        // The list moves one row per notch too, in both directions, and stops
        // on the last row. It opens on the first window, at index 1.
        for expected in [2, 3] {
            app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 6));
            assert_eq!(app.selected_index(), Some(expected));
        }
        app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 6));
        assert_eq!(app.selected_index(), Some(3), "one header plus three rows");
        app.handle_mouse(wheel(MouseEventKind::ScrollUp, 10, 6));
        assert_eq!(app.selected_index(), Some(2));
    }

    /// A notch on the chart is a nudge — about one column of the plot — where
    /// a keypress is a stride of an eighth of the range.
    #[test]
    fn the_wheel_pans_the_chart_by_a_single_column() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 1)]);
        app.detail_area = Rect::new(0, 16, 80, 16);
        let now = Utc::now();
        app.set_trend(trend_with_history(&app, Duration::days(7), now));

        app.handle_mouse(wheel(MouseEventKind::ScrollUp, 10, 20));
        let notch = app.trend_pan_secs;
        let range = app.range.duration().num_seconds();
        assert_eq!(notch, range / (80 - TREND_AXIS_COLUMNS));
        assert!(notch > 0);

        app.handle_key(key('k'));
        assert_eq!(
            app.trend_pan_secs - notch,
            range / TREND_KEY_STEPS_PER_RANGE,
            "a keypress moves further than a notch"
        );

        // Forward again by exactly what the notch moved back.
        let before = app.trend_pan_secs;
        app.handle_mouse(wheel(MouseEventKind::ScrollDown, 10, 20));
        assert_eq!(before - app.trend_pan_secs, notch);
        // A sideways gesture travels the same axis: the chart has only one.
        app.handle_mouse(wheel(MouseEventKind::ScrollLeft, 10, 20));
        assert_eq!(app.trend_pan_secs, before);
    }

    #[test]
    fn ctrl_c_quits_from_anywhere() {
        let mut app = App::new();
        app.overlay = Some(Overlay::Help);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(app.handle_key(ctrl_c), Action::Quit));
    }
}
