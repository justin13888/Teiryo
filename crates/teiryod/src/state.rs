//! Shared single-threaded daemon state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use teiryo_core::{
    rollover, Account, AccountHealth, AccountId, AccountStatus, BarStyle, ConfigState, PollEvent,
    PollOutcome, PollTrigger, ProviderAdapter, ProviderHealth, ProviderId, QuotaWindow, RenderHint,
    Storage, WindowView,
};
use tokio::sync::{mpsc, watch};

use crate::config::{Config, LoadedConfig};
use crate::scheduler::Schedule;

/// Rolling health counters for one (provider, account) poll task. The
/// wire-facing view is [`teiryo_core::AccountHealth`], assembled in
/// [`Daemon::provider_health`].
#[derive(Debug, Default, Clone)]
pub struct HealthCounters {
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
    pub health: HashMap<(ProviderId, AccountId), HealthCounters>,
    /// Manual-trigger senders into each poll task.
    pub pollers: HashMap<(ProviderId, AccountId), mpsc::UnboundedSender<PollTrigger>>,
    /// Live schedule senders into each poll task. A config reload publishes
    /// here rather than respawning tasks.
    pub schedules: HashMap<(ProviderId, AccountId), watch::Sender<Schedule>>,
    /// Effective scheduler cadence per account, so clients can show how long
    /// until the next poll without knowing the daemon's config. Zero when the
    /// account's provider is disabled — there is no next poll to count down to.
    pub poll_intervals: HashMap<AccountId, Duration>,
    /// Settings currently in effect.
    pub config: Config,
    /// Wire-facing snapshot of `config`, plus how the last read of the file
    /// went. Rebuilt on every load attempt.
    pub config_state: ConfigState,
    /// Compiled-in provider ids, so [`Config::view`] can offer a row for a
    /// provider the config file never mentions.
    pub known_providers: Vec<ProviderId>,
    /// Adapters kept for their [`teiryo_core::WindowPresenter`] impl: `Status`
    /// attaches each window's render hint so the TUI never hardcodes
    /// provider-specific thresholds.
    pub presenters: HashMap<ProviderId, Rc<dyn ProviderAdapter>>,
}

/// Cadence in whole seconds. `0` means "no next poll to expect": either the
/// account has no poller registered, or its provider is disabled in config.
/// Clients already treat `0` as "draw no countdown", which is exactly right
/// for a paused provider.
fn interval_secs(interval: Option<&Duration>) -> u32 {
    interval.map_or(0, |d| d.as_secs().min(u64::from(u32::MAX)) as u32)
}

/// What one provider's settings resolve to for its poll tasks.
fn schedule(config: &Config, provider: &ProviderId) -> Schedule {
    Schedule {
        enabled: config.provider_enabled(provider),
        interval: config.poll_interval(provider),
    }
}

/// The cadence to report to clients: zero while disabled, per [`interval_secs`].
fn reported(schedule: Schedule) -> Duration {
    if schedule.enabled {
        schedule.interval
    } else {
        Duration::ZERO
    }
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
    /// Publishes [`ConfigState::generation`] after every config load attempt.
    /// The same `AwaitUpdate` that waits on `watch_tx` also waits here, so a
    /// client learns about a `config.toml` edit without a second connection or
    /// a polling timer of its own.
    pub config_tx: watch::Sender<u64>,
    /// Broadcast shutdown flag.
    pub shutdown_tx: watch::Sender<bool>,
}

impl Daemon {
    /// Fresh daemon state around an opened storage. `config_path` is the file
    /// the daemon reads, writes, and watches; `known_providers` is the
    /// compiled-in registry.
    pub fn new(storage: Storage, config_path: PathBuf, known_providers: Vec<ProviderId>) -> Self {
        let (watch_tx, _) = watch::channel(None);
        let (config_tx, _) = watch::channel(0);
        let (shutdown_tx, _) = watch::channel(false);
        let config = Config::default();
        let config_state = ConfigState {
            path: config_path.to_string_lossy().into_owned(),
            generation: 0,
            effective: config.view(&known_providers),
            loaded_at: chrono::Utc::now(),
            warnings: Vec::new(),
            error: None,
        };
        Self {
            state: Rc::new(RefCell::new(SharedState {
                storage,
                accounts: Vec::new(),
                latest_poll: HashMap::new(),
                latest_success: HashMap::new(),
                health: HashMap::new(),
                pollers: HashMap::new(),
                schedules: HashMap::new(),
                poll_intervals: HashMap::new(),
                config,
                config_state,
                known_providers,
                presenters: HashMap::new(),
            })),
            watch_tx,
            config_tx,
            shutdown_tx,
        }
    }

    /// The current settings and how the last file read went.
    pub fn config_state(&self) -> ConfigState {
        self.state.borrow().config_state.clone()
    }

    /// Put a freshly parsed config into effect: republish every account's
    /// schedule, refresh the cadences clients see, and wake the long-polls.
    ///
    /// This is the *only* apply path — startup, `SetConfig`, and the file
    /// watcher all funnel through it, so there is one place where a setting
    /// becomes real and no way for the three to drift.
    pub fn apply_config(&self, loaded: LoadedConfig) {
        self.install_config(Some(loaded), None);
    }

    /// Record that a load was rejected. The previously applied config keeps
    /// running; only the reported error and generation change.
    pub fn reject_config(&self, error: String) {
        self.install_config(None, Some(error));
    }

    fn install_config(&self, loaded: Option<LoadedConfig>, error: Option<String>) {
        let mut st = self.state.borrow_mut();
        let warnings = match loaded {
            Some(loaded) => {
                st.config = loaded.config;
                loaded.warnings
            }
            // A rejected file tells us nothing new about unknown keys, so the
            // warnings from the last file that *did* apply are still the
            // accurate ones.
            None => st.config_state.warnings.clone(),
        };

        let updates: Vec<((ProviderId, AccountId), Schedule)> = {
            let state = &*st;
            state
                .accounts
                .iter()
                .map(|a| {
                    (
                        (a.provider.clone(), a.id.clone()),
                        schedule(&state.config, &a.provider),
                    )
                })
                .collect()
        };
        for (key, next) in updates {
            st.poll_intervals.insert(key.1.clone(), reported(next));
            if let Some(tx) = st.schedules.get(&key) {
                tx.send_replace(next);
            }
        }

        let generation = st.config_state.generation + 1;
        st.config_state = ConfigState {
            path: st.config_state.path.clone(),
            generation,
            effective: st.config.view(&st.known_providers),
            loaded_at: chrono::Utc::now(),
            warnings,
            error,
        };
        drop(st);
        self.config_tx.send_replace(generation);
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
        // before `latest_success` is replaced below, and `hydrate_account`
        // restores that cache at startup, so detection also survives a restart.
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

    /// Assemble `Providers` health payload. Per-account rows are carried
    /// alongside the per-provider rollup so a client can point at the account
    /// that is actually failing.
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
            let counters = st
                .health
                .get(&(account.provider.clone(), account.id.clone()))
                .cloned()
                .unwrap_or_default();
            entry.consecutive_failures = entry
                .consecutive_failures
                .max(counters.consecutive_failures);
            if entry.last_error.is_none() {
                entry.last_error = counters.last_error.clone();
            }
            entry.accounts.push(AccountHealth {
                account: account.id.clone(),
                consecutive_failures: counters.consecutive_failures,
                last_error: counters.last_error,
                last_poll_ts: st.latest_poll.get(&account.id).map(|e| e.ts),
                poll_interval_secs: interval_secs(st.poll_intervals.get(&account.id)),
            });
        }
        let mut list: Vec<_> = by_provider.into_values().collect();
        list.sort_by(|a, b| a.provider.cmp(&b.provider));
        list
    }

    /// Register a poll task for `account`: open its live schedule channel,
    /// record the cadence clients see, and keep the adapter for its presenter.
    /// Returns the receiver to hand to [`crate::scheduler::spawn_poller`].
    pub fn register_poller(
        &self,
        account: &Account,
        adapter: Rc<dyn ProviderAdapter>,
    ) -> watch::Receiver<Schedule> {
        let mut st = self.state.borrow_mut();
        let initial = schedule(&st.config, &account.provider);
        let (tx, rx) = watch::channel(initial);
        st.schedules
            .insert((account.provider.clone(), account.id.clone()), tx);
        st.poll_intervals
            .insert(account.id.clone(), reported(initial));
        st.presenters.insert(account.provider.clone(), adapter);
        rx
    }

    /// Record the cadence a poll task is *actually* running at, which while a
    /// provider is rate limiting us is longer than the configured one.
    ///
    /// A config reload overwrites this with the configured value, which is
    /// correct rather than racy: the reload wakes every poll task, and each
    /// republishes its own cadence as it re-arms.
    pub fn set_reported_interval(&self, account: &AccountId, interval: Duration) {
        self.state
            .borrow_mut()
            .poll_intervals
            .insert(account.clone(), interval);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use teiryo_core::domain::{PollId, QuotaUnit, QuotaWindow, ResetKind, WindowId, WindowScope};

    use super::*;

    fn account() -> Account {
        Account {
            id: AccountId::from("stub:one"),
            provider: "stub".into(),
            label: "one".into(),
        }
    }

    fn window() -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from("session"),
            label: "Session".into(),
            scope: WindowScope::AccountWide,
            reset_kind: ResetKind::Rolling(Duration::from_secs(5 * 3600)),
            unit: QuotaUnit::Percent,
            used: 37.0,
            limit: Some(100.0),
            reset_at: None,
        }
    }

    fn event(outcome: PollOutcome) -> PollEvent {
        PollEvent {
            id: PollId::generate(),
            ts: chrono::Utc::now(),
            provider: "stub".into(),
            account: account().id,
            trigger: PollTrigger::Scheduled,
            outcome,
            latency_ms: 12,
        }
    }

    fn seeded(path: &std::path::Path) -> Daemon {
        let storage = Storage::open(path).expect("storage");
        let daemon = Daemon::new(
            storage,
            path.with_file_name("config.toml"),
            vec!["stub".to_owned()],
        );
        daemon
            .state
            .borrow_mut()
            .storage
            .upsert_account(&account())
            .unwrap();
        daemon.state.borrow_mut().accounts.push(account());
        daemon
    }

    /// A `Daemon` with a registered poll task, so schedule delivery can be
    /// observed the way the scheduler sees it.
    fn scheduled(path: &std::path::Path) -> (Daemon, watch::Receiver<Schedule>) {
        struct NoAdapter;
        // Only `id`/`render_hint` are reachable here; `register_poller` keeps
        // the adapter solely for its presenter.
        impl teiryo_core::WindowPresenter for NoAdapter {
            fn render_hint(&self, _: &teiryo_core::QuotaWindow) -> RenderHint {
                default_hint()
            }
            fn group_order(&self) -> &[teiryo_core::WindowId] {
                &[]
            }
        }
        impl teiryo_core::QuotaParser for NoAdapter {
            fn parse(
                &self,
                _: &teiryo_core::RawResponse,
            ) -> Result<Vec<teiryo_core::QuotaWindow>, teiryo_core::ParseError> {
                Ok(Vec::new())
            }
        }
        #[async_trait::async_trait]
        impl teiryo_core::Authenticator for NoAdapter {
            async fn discover_accounts(&self) -> Result<Vec<Account>, teiryo_core::AuthError> {
                Ok(Vec::new())
            }
            async fn credential_for(
                &self,
                _: &Account,
            ) -> Result<teiryo_core::Credential, teiryo_core::AuthError> {
                Err(teiryo_core::AuthError::NotLoggedIn("stub".into()))
            }
        }
        #[async_trait::async_trait]
        impl teiryo_core::Prober for NoAdapter {
            async fn probe(
                &self,
                _: &Account,
                _: &teiryo_core::Credential,
            ) -> Result<teiryo_core::RawResponse, teiryo_core::ProbeError> {
                Err(teiryo_core::ProbeError::Network("stub".into()))
            }
        }
        impl ProviderAdapter for NoAdapter {
            fn id(&self) -> ProviderId {
                "stub".into()
            }
        }

        let daemon = seeded(path);
        let rx = daemon.register_poller(&account(), Rc::new(NoAdapter));
        (daemon, rx)
    }

    /// The point of the whole feature: a config change reaches a *running*
    /// poll task, and the cadence clients see moves with it.
    #[test]
    fn applying_config_republishes_schedules() {
        let dir = std::env::temp_dir().join(format!("teiryod-state-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let (daemon, mut rx) = scheduled(&dir.join("teiryo.db"));

        assert_eq!(
            *rx.borrow_and_update(),
            Schedule {
                enabled: true,
                interval: crate::config::DEFAULT_POLL_INTERVAL,
            }
        );
        assert_eq!(
            daemon.status(None, None)[0].poll_interval_secs,
            crate::config::DEFAULT_POLL_INTERVAL.as_secs() as u32
        );

        daemon.apply_config(crate::config::parse("poll_interval_secs = 300").unwrap());
        assert_eq!(rx.borrow_and_update().interval, Duration::from_secs(300));
        assert_eq!(daemon.status(None, None)[0].poll_interval_secs, 300);
        assert_eq!(
            daemon.config_state().effective.poll_interval_secs,
            Some(300)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Disabling reports a cadence of zero, which is already how clients spell
    /// "no next poll to count down to".
    #[test]
    fn disabling_a_provider_parks_it_and_zeroes_the_reported_cadence() {
        let dir = std::env::temp_dir().join(format!("teiryod-state-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let (daemon, mut rx) = scheduled(&dir.join("teiryo.db"));

        daemon.apply_config(crate::config::parse("[providers.stub]\nenabled = false").unwrap());
        assert!(!rx.borrow_and_update().enabled);
        assert_eq!(daemon.status(None, None)[0].poll_interval_secs, 0);
        assert_eq!(
            daemon.provider_health()[0].accounts[0].poll_interval_secs,
            0
        );

        daemon.apply_config(crate::config::parse("").unwrap());
        assert!(rx.borrow_and_update().enabled);
        assert_eq!(
            daemon.status(None, None)[0].poll_interval_secs,
            crate::config::DEFAULT_POLL_INTERVAL.as_secs() as u32
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A rejected file must not disturb what is running — only what is
    /// reported. This is the difference between "your edit did not take" and
    /// "your daemon silently reverted to defaults".
    #[test]
    fn rejecting_a_config_keeps_the_previous_settings_running() {
        let dir = std::env::temp_dir().join(format!("teiryod-state-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let (daemon, mut rx) = scheduled(&dir.join("teiryo.db"));

        daemon.apply_config(crate::config::parse("poll_interval_secs = 300").unwrap());
        let applied = daemon.config_state();

        daemon.reject_config("`poll_interval_secs` must not be negative, got -1".into());
        let rejected = daemon.config_state();
        assert_eq!(rejected.effective, applied.effective, "settings changed");
        assert_eq!(rx.borrow_and_update().interval, Duration::from_secs(300));
        assert!(rejected.error.is_some());
        // Still a new generation, or a client would never learn about it.
        assert!(rejected.generation > applied.generation);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of hydration: a daemon that restarts into a provider
    /// outage still serves the windows its previous run recorded, so the TUI
    /// has something to select and chart.
    #[test]
    fn restart_restores_windows_from_the_last_success() {
        let dir = std::env::temp_dir().join(format!("teiryod-state-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("teiryo.db");

        {
            let first = seeded(&path);
            first.record_event(&event(PollOutcome::Success {
                windows: vec![window()],
            }));
            std::thread::sleep(Duration::from_millis(2));
            first.record_event(&event(PollOutcome::RateLimited { retry_after: None }));
            assert_eq!(first.status(None, None)[0].windows.len(), 1);
        }

        // A fresh daemon over the same database, before any poll of its own.
        let restarted = seeded(&path);
        assert!(
            restarted.status(None, None)[0].windows.is_empty(),
            "precondition: caches start empty"
        );

        restarted.hydrate_account(&account());
        let status = &restarted.status(None, None)[0];
        assert_eq!(status.windows.len(), 1);
        assert_eq!(status.windows[0].window.used, 37.0);
        assert!(status.last_success.is_some());
        // The failure on top of it is still what `last_poll` reports.
        assert!(matches!(
            status.last_poll.as_ref().map(|e| &e.outcome),
            Some(PollOutcome::RateLimited { .. })
        ));

        let health = &restarted.provider_health()[0].accounts[0];
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.last_error.as_deref(), Some("rate limited"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A window whose `reset_at` moves while the old one was still in the
    /// future rolled over early, and that has to be recorded — including
    /// across the failures and the restart that separate the two polls, which
    /// is the case an in-memory-only detector would lose.
    #[test]
    fn an_early_rollover_is_recorded_across_failures_and_a_restart() {
        let dir = std::env::temp_dir().join(format!("teiryod-rollover-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("teiryo.db");
        let now = chrono::Utc::now();
        let since = now - chrono::Duration::hours(1);

        let mut before = window();
        before.used = 88.0;
        before.reset_at = Some(now + chrono::Duration::hours(2));
        {
            let first = seeded(&path);
            first.record_event(&event(PollOutcome::Success {
                windows: vec![before],
            }));
            std::thread::sleep(Duration::from_millis(2));
            // A failure in between must not itself look like a rollover.
            first.record_event(&event(PollOutcome::RateLimited { retry_after: None }));
            let st = first.state.borrow();
            assert!(st
                .storage
                .rollovers(&account().id, None, since, chrono::Utc::now())
                .unwrap()
                .is_empty());
        }

        let restarted = seeded(&path);
        restarted.hydrate_account(&account());
        let mut after = window();
        after.used = 1.0;
        // The old reset was still two hours out when this one appeared.
        after.reset_at = Some(now + chrono::Duration::hours(7));
        restarted.record_event(&event(PollOutcome::Success {
            windows: vec![after],
        }));

        let st = restarted.state.borrow();
        let found = st
            .storage
            .rollovers(&account().id, None, since, chrono::Utc::now())
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, teiryo_core::RolloverKind::Early);
        assert_eq!(found[0].window, WindowId::from("session"));
        assert_eq!(found[0].prev_used, 88.0);
        assert_eq!(found[0].new_used, 1.0);
        drop(st);

        std::fs::remove_dir_all(&dir).ok();
    }
}
