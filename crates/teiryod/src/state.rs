//! Shared single-threaded daemon state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use teiryo_core::{
    rollover, Account, AccountId, AccountStatus, BarStyle, PollEvent, PollOutcome, PollTrigger,
    ProviderAdapter, ProviderHealth, ProviderId, QuotaWindow, RenderHint, Storage, WindowView,
};
use tokio::sync::{mpsc, watch};

/// Rolling health of one (provider, account) poll task.
#[derive(Debug, Default, Clone)]
pub struct AccountHealth {
    /// Consecutive failed polls (0 = healthy).
    pub consecutive_failures: u32,
    /// Most recent error message, if the last poll failed.
    pub last_error: Option<String>,
}

/// Mutable daemon state. Lives behind `Rc<RefCell<..>>` on the
/// `current_thread` runtime — never hold a borrow across an `await`.
pub struct SharedState {
    /// Persistent storage.
    pub storage: Storage,
    /// Accounts discovered at startup, in registry order.
    pub accounts: Vec<Account>,
    /// Latest completed poll per account (any outcome).
    pub latest_poll: HashMap<AccountId, PollEvent>,
    /// Latest *successful* poll per account — the windows `Status` serves.
    pub latest_success: HashMap<AccountId, PollEvent>,
    /// Health per (provider, account).
    pub health: HashMap<(ProviderId, AccountId), AccountHealth>,
    /// Manual-trigger senders into each poll task.
    pub pollers: HashMap<(ProviderId, AccountId), mpsc::UnboundedSender<PollTrigger>>,
    /// Effective scheduler cadence per account, so clients can show how long
    /// until the next poll without knowing the daemon's config.
    pub poll_intervals: HashMap<AccountId, Duration>,
    /// Adapters kept for their [`teiryo_core::WindowPresenter`] impl: `Status`
    /// attaches each window's render hint so the TUI never hardcodes
    /// provider-specific thresholds.
    pub presenters: HashMap<ProviderId, Rc<dyn ProviderAdapter>>,
}

/// Cadence in whole seconds. `0` means "no next poll to expect", which is
/// what a client needs to draw no countdown.
fn interval_secs(interval: Option<&Duration>) -> u32 {
    interval.map_or(0, |d| d.as_secs().min(u64::from(u32::MAX)) as u32)
}

/// Fallback for an account whose adapter is not registered — it cannot happen
/// for a scheduled account, but `Status` must still render something sane.
fn default_hint() -> RenderHint {
    RenderHint {
        style: BarStyle::Percent,
        warn_threshold: 0.8,
        critical_threshold: 0.95,
        note: None,
    }
}

/// The windows a previous poll reported, or none when it was a failure — a
/// failed poll carries no windows, and treating that as "everything vanished"
/// would manufacture rollovers out of an outage.
fn previous_windows(event: Option<&PollEvent>) -> &[QuotaWindow] {
    match event.map(|e| &e.outcome) {
        Some(PollOutcome::Success { windows }) => windows,
        _ => &[],
    }
}

/// Cheap-to-clone handle bundling state and the daemon-wide channels.
#[derive(Clone)]
pub struct Daemon {
    /// Shared mutable state.
    pub state: Rc<RefCell<SharedState>>,
    /// Publishes every completed poll; `AwaitUpdate` long-polls subscribe here.
    pub watch_tx: watch::Sender<Option<PollEvent>>,
    /// Broadcast shutdown flag.
    pub shutdown_tx: watch::Sender<bool>,
}

impl Daemon {
    /// Fresh daemon state around an opened storage.
    pub fn new(storage: Storage) -> Self {
        let (watch_tx, _) = watch::channel(None);
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            state: Rc::new(RefCell::new(SharedState {
                storage,
                accounts: Vec::new(),
                latest_poll: HashMap::new(),
                latest_success: HashMap::new(),
                health: HashMap::new(),
                pollers: HashMap::new(),
                poll_intervals: HashMap::new(),
                presenters: HashMap::new(),
            })),
            watch_tx,
            shutdown_tx,
        }
    }

    /// Record a completed poll: persist, update caches/health, publish.
    pub fn record_event(&self, event: &PollEvent) {
        let mut st = self.state.borrow_mut();
        let windows = match &event.outcome {
            PollOutcome::Success { windows } => windows.clone(),
            _ => Vec::new(),
        };
        // Detect against the last *successful* poll, not the last poll: a run
        // of failures in between leaves the windows untouched, and comparing
        // against an empty failure payload would invent a rollover. This runs
        // before `latest_success` is replaced below.
        let rollovers = rollover::detect(
            &event.account,
            previous_windows(st.latest_success.get(&event.account)),
            &windows,
            event.id,
            event.ts,
        );
        for r in rollovers.iter().filter(|r| r.kind.is_surprise()) {
            tracing::info!(
                account = %r.account, window = %r.window, kind = r.kind.as_str(),
                prev_reset_at = ?r.prev_reset_at, new_reset_at = ?r.new_reset_at,
                prev_used = r.prev_used, new_used = r.new_used,
                "quota window reset unexpectedly"
            );
        }
        if let Err(e) = st.storage.record_poll(event, &windows, &rollovers) {
            tracing::error!(error = %e, poll = %event.id, "failed to persist poll event");
        }
        let key = (event.provider.clone(), event.account.clone());
        let health = st.health.entry(key).or_default();
        match event.outcome.error_message() {
            None => {
                health.consecutive_failures = 0;
                health.last_error = None;
                st.latest_success
                    .insert(event.account.clone(), event.clone());
            }
            Some(msg) => {
                health.consecutive_failures += 1;
                health.last_error = Some(msg.to_owned());
            }
        }
        st.latest_poll.insert(event.account.clone(), event.clone());
        drop(st);
        self.watch_tx.send_replace(Some(event.clone()));
    }

    /// Reload an account's last poll, last success, and health counters from
    /// storage.
    ///
    /// The caches this fills are in-memory only, so without it a freshly
    /// started daemon serves an empty `Status` — no windows, nothing for the
    /// TUI to select, and therefore an empty trend chart — until its own
    /// first poll *succeeds*. That can be a long wait when the provider is
    /// rate limiting, even though the history is already on disk.
    pub fn hydrate_account(&self, account: &Account) {
        let mut st = self.state.borrow_mut();
        match st.storage.latest_poll_for(&account.id) {
            Ok(Some(event)) => {
                st.latest_poll.insert(account.id.clone(), event);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(account = %account.id, error = %e, "failed to restore last poll")
            }
        }
        match st.storage.latest_success_for(&account.id) {
            Ok(Some(event)) => {
                st.latest_success.insert(account.id.clone(), event);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(account = %account.id, error = %e, "failed to restore last success")
            }
        }
        let failures = match st.storage.consecutive_failures_for(&account.id) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(account = %account.id, error = %e, "failed to restore failure count");
                0
            }
        };
        // Keep the error message and the counter agreeing: both describe the
        // stretch of failures since the last success, or neither is set.
        let last_error = st
            .latest_poll
            .get(&account.id)
            .and_then(|e| e.outcome.error_message().map(str::to_owned));
        let health = st
            .health
            .entry((account.provider.clone(), account.id.clone()))
            .or_default();
        health.consecutive_failures = failures;
        health.last_error = last_error;
    }

    /// Assemble `Status` payload, optionally filtered.
    pub fn status(
        &self,
        provider: Option<&ProviderId>,
        account: Option<&AccountId>,
    ) -> Vec<AccountStatus> {
        let st = self.state.borrow();
        st.accounts
            .iter()
            .filter(|a| provider.is_none_or(|p| &a.provider == p))
            .filter(|a| account.is_none_or(|id| &a.id == id))
            .map(|a| {
                let success = st.latest_success.get(&a.id);
                let presenter = st.presenters.get(&a.provider);
                let windows = match success.map(|e| &e.outcome) {
                    Some(PollOutcome::Success { windows }) => windows
                        .iter()
                        .map(|window| WindowView {
                            hint: presenter
                                .map_or_else(default_hint, |adapter| adapter.render_hint(window)),
                            window: window.clone(),
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                AccountStatus {
                    account: a.clone(),
                    windows,
                    last_poll: st.latest_poll.get(&a.id).cloned(),
                    last_success: success.map(|e| e.ts),
                    poll_interval_secs: interval_secs(st.poll_intervals.get(&a.id)),
                }
            })
            .collect()
    }

    /// Assemble `Providers` health payload.
    pub fn provider_health(&self) -> Vec<ProviderHealth> {
        let st = self.state.borrow();
        let mut by_provider: HashMap<&str, ProviderHealth> = HashMap::new();
        for account in &st.accounts {
            let entry = by_provider
                .entry(account.provider.as_str())
                .or_insert_with(|| ProviderHealth {
                    provider: account.provider.clone(),
                    accounts: Vec::new(),
                    consecutive_failures: 0,
                    last_error: None,
                });
            entry.accounts.push(account.id.clone());
            if let Some(h) = st
                .health
                .get(&(account.provider.clone(), account.id.clone()))
            {
                entry.consecutive_failures = entry.consecutive_failures.max(h.consecutive_failures);
                if entry.last_error.is_none() {
                    entry.last_error = h.last_error.clone();
                }
            }
        }
        let mut list: Vec<_> = by_provider.into_values().collect();
        list.sort_by(|a, b| a.provider.cmp(&b.provider));
        list
    }

    /// The newest poll id published so far (zero if none yet).
    pub fn newest_poll_id(&self) -> teiryo_core::PollId {
        self.watch_tx
            .borrow()
            .as_ref()
            .map(|e| e.id)
            .unwrap_or_else(teiryo_core::PollId::zero)
    }
}
