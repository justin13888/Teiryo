//! TUI application state and key handling, independent of rendering and I/O.

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use teiryo_core::domain::{AccountId, WindowId};
use teiryo_core::{AccountStatus, PollEvent, ProviderHealth, ProviderId, QuotaSnapshot, Request};

/// Which screen is showing.
pub enum View {
    /// Live dashboard of all accounts and windows.
    Dashboard,
    /// History for one window.
    History {
        /// Human title ("account — window").
        title: String,
        /// Snapshots, oldest first.
        snapshots: Vec<QuotaSnapshot>,
    },
    /// Recent poll log.
    RecentPolls(Vec<PollEvent>),
    /// Provider/account health.
    Providers(Vec<ProviderHealth>),
    /// Two-step daemon shutdown confirmation.
    ConfirmShutdown,
}

/// One selectable dashboard row: an account header or one of its windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowRef {
    /// Index into `App::statuses`.
    pub account: usize,
    /// Window index within the account, `None` for the account header row.
    pub window: Option<usize>,
}

/// What the event loop should do after a key press.
pub enum Action {
    /// Nothing.
    None,
    /// Leave the TUI (daemon keeps running).
    Quit,
    /// Send requests whose replies only matter as acks/errors.
    Send(Vec<Request>),
    /// Fetch and open history for one window.
    OpenHistory {
        /// Account owning the window.
        account: AccountId,
        /// Window to chart.
        window: WindowId,
        /// Display title for the view.
        title: String,
    },
    /// Fetch and open the recent poll log.
    OpenRecent,
    /// Fetch and open provider health.
    OpenProviders,
    /// Confirmed: stop the daemon, then quit.
    ShutdownDaemon,
}

/// Application state.
pub struct App {
    /// Latest account statuses, sorted by (provider, label).
    pub statuses: Vec<AccountStatus>,
    /// Selected row index into [`App::rows`].
    pub selected: usize,
    /// Current screen.
    pub view: View,
    /// Status-line error, if any.
    pub error: Option<String>,
    /// True when the daemon connection is lost and reconnection is pending.
    pub disconnected: bool,
    /// Time of the most recent update, for the status line.
    pub last_update: Option<DateTime<Utc>>,
}

impl App {
    /// Fresh app with no data yet.
    pub fn new() -> Self {
        Self {
            statuses: Vec::new(),
            selected: 0,
            view: View::Dashboard,
            error: None,
            disconnected: false,
            last_update: None,
        }
    }

    /// Replace statuses (sorted for stable display) and clamp the selection.
    pub fn set_statuses(&mut self, mut statuses: Vec<AccountStatus>) {
        statuses.sort_by(|a, b| {
            (&a.account.provider, &a.account.label).cmp(&(&b.account.provider, &b.account.label))
        });
        self.statuses = statuses;
        let len = self.rows().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
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

    /// The currently selected row, if any rows exist.
    pub fn selected_row(&self) -> Option<RowRef> {
        self.rows().get(self.selected).copied()
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

    /// Translate a key press into an [`Action`], mutating view/selection state.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        match &self.view {
            View::ConfirmShutdown => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Action::ShutdownDaemon,
                _ => {
                    self.view = View::Dashboard;
                    Action::None
                }
            },
            View::History { .. } | View::RecentPolls(_) | View::Providers(_) => match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                    self.view = View::Dashboard;
                    Action::None
                }
                _ => Action::None,
            },
            View::Dashboard => self.handle_dashboard_key(key),
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('Q') => {
                self.view = View::ConfirmShutdown;
                Action::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let len = self.rows().len();
                if len > 0 {
                    self.selected = (self.selected + 1).min(len - 1);
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Char('r') => match self.selected_row() {
                Some(row) => {
                    let account = &self.statuses[row.account].account;
                    Action::Send(vec![Request::PollNow {
                        provider: account.provider.clone(),
                        account: Some(account.id.clone()),
                    }])
                }
                None => Action::None,
            },
            KeyCode::Char('R') => Action::Send(
                self.providers()
                    .into_iter()
                    .map(|provider| Request::PollNow {
                        provider,
                        account: None,
                    })
                    .collect(),
            ),
            KeyCode::Char('h') | KeyCode::Enter => match self.selected_row() {
                Some(RowRef {
                    account,
                    window: Some(wi),
                }) => {
                    let status = &self.statuses[account];
                    let win = &status.windows[wi];
                    Action::OpenHistory {
                        account: status.account.id.clone(),
                        window: win.window.id.clone(),
                        title: format!("{} — {}", status.account.label, win.window.label),
                    }
                }
                _ => Action::None,
            },
            KeyCode::Char('l') => Action::OpenRecent,
            KeyCode::Char('p') => Action::OpenProviders,
            _ => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use teiryo_core::domain::{QuotaUnit, QuotaWindow, ResetKind, WindowScope};
    use teiryo_core::{Account, BarStyle, RenderHint, WindowView};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn window(id: &str) -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from(id),
            label: id.to_owned(),
            scope: WindowScope::AccountWide,
            reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(3600)),
            unit: QuotaUnit::Percent,
            used: 10.0,
            limit: None,
            reset_at: None,
        }
    }

    fn status(provider: &str, label: &str, windows: usize) -> AccountStatus {
        AccountStatus {
            account: Account {
                id: AccountId::from(format!("{provider}:{label}").as_str()),
                provider: provider.into(),
                label: label.into(),
            },
            windows: (0..windows)
                .map(|i| WindowView {
                    window: window(&format!("w{i}")),
                    hint: RenderHint {
                        style: BarStyle::Percent,
                        warn_threshold: 0.8,
                        critical_threshold: 0.95,
                        note: None,
                    },
                })
                .collect(),
            last_success: None,
            poll_interval_secs: 60,
            last_poll: None,
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
    fn selection_clamps_on_shrink() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 3)]);
        app.selected = 3;
        app.set_statuses(vec![status("claude", "a", 0)]);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn shutdown_needs_confirmation() {
        let mut app = App::new();
        app.set_statuses(vec![status("claude", "a", 1)]);
        assert!(matches!(app.handle_key(key('Q')), Action::None));
        assert!(matches!(app.view, View::ConfirmShutdown));
        assert!(matches!(app.handle_key(key('n')), Action::None));
        assert!(matches!(app.view, View::Dashboard));
        app.handle_key(key('Q'));
        assert!(matches!(app.handle_key(key('y')), Action::ShutdownDaemon));
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
    }
}
