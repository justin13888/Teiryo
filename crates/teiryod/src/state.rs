//! Shared single-threaded daemon state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use teiryo_core::{
    is_collapse, rollover, Account, AccountHealth, AccountId, AccountStatus, BarStyle, ConfigState,
    ObservedStart, PollEvent, PollOutcome, PollTrigger, ProviderAdapter, ProviderHealth,
    ProviderId, QuotaWindow, RenderHint, RolloverKind, Storage, WindowId, WindowRollover,
    WindowView,
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
    /// Most recent observed restart per window, published on `Status`.
    ///
    /// Held here rather than recomputed per request because deriving it means
    /// walking the stored rollovers, and because the client cannot derive it
    /// at all: the reset that anchors a weekly window is routinely older than
    /// any history a dashboard fetches.
    pub observed_starts: HashMap<(AccountId, WindowId), ObservedStart>,
}

/// How far back [`Daemon::hydrate_account`] looks for the reset that anchors a
/// window. Longer than the longest window any adapter publishes (7 days), so
/// the anchor for a weekly quota survives a restart, and bounded so a database
/// kept for months is not scanned on every startup.
const ANCHOR_LOOKBACK: chrono::Duration = chrono::Duration::days(21);

/// Whether a rollover is one the provider failed to announce, and so the only
/// kind that can say anything `reset_at` does not already say.
///
/// Every other kind moved `reset_at`, which makes `reset_at - span` the
/// provider's own statement of where the new window began — exact, and better
/// than anything inferred here. Anchoring on those would actively make things
/// worse: a restart seen across a weekend outage carries a bracket days wide,
/// and its midpoint would override a start the provider had told us precisely.
fn unannounced(rollover: &WindowRollover) -> bool {
    matches!(rollover.kind, RolloverKind::Unannounced)
}

/// Drop anchors that can no longer describe a window currently running.
///
/// An anchor is retired directly only by a later *announced* rollover, which
/// is a signal a provider that never moves `reset_at` never sends — and that
/// provider is exactly the one this feature exists for. `effective_window`'s
/// `estimate() > reset_at - span` guard is no substitute: that boundary moves
/// only when `reset_at` does. Without a second bound the map keeps every
/// bracket it has ever recorded, including for windows the payload has since
/// stopped carrying, for as long as the process runs.
///
/// The bound is the window's own length. A rolling window of span `S` that
/// began at `T` has ended by `T + S`, so a bracket older than that describes
/// an instance that is over.
///
/// A window missing from this payload is *not* judged. It has no span left to
/// ask for, and borrowing another window's would delete a live anchor: an
/// account with a 5-hour and a weekly window, three days into an observed
/// weekly restart, loses that anchor the first time one payload omits `weekly`
/// — after which the pace falls back to `reset_at - 7d`, an instant inside a
/// window that already ended, and stays there until the next observed restart.
/// A window absent from a payload is evidence about the payload, which is
/// exactly what `rollover::detect` says about the same case.
///
/// `docs/domain.md` calls the related risk "bounded by the window's length",
/// of a restart missed entirely. This is the same bound applied to the anchor
/// map's own contents, which nothing previously expressed at all.
fn prune_anchors(
    st: &mut SharedState,
    account: &AccountId,
    windows: &[QuotaWindow],
    now: chrono::DateTime<chrono::Utc>,
) {
    let mut spans: HashMap<&WindowId, chrono::Duration> = HashMap::new();
    let mut longest = chrono::Duration::zero();
    for w in windows {
        if let Some(span) = w.span() {
            spans.insert(&w.id, span);
            longest = longest.max(span);
        }
    }
    if longest <= chrono::Duration::zero() {
        return;
    }
    st.observed_starts.retain(|(owner, window), start| {
        if owner != account {
            return true;
        }
        match spans.get(window) {
            Some(span) => now - start.estimate() <= *span,
            None => true,
        }
    });
}

/// Whether a *stored* row still reads as an unannounced reset under the rule
/// compiled in here.
///
/// [`unannounced`] asks only what the row says it is, which is the right
/// question for a rollover this process detected a moment ago: the detector
/// wrote it under this rule. It is the wrong question for a row read back out
/// of the database, because rows outlive the rule that classified them.
///
/// Rows written before the ratio guard existed were judged by a bare
/// `prev - new > 0.25` drop. A provider correcting 90% → 60% satisfies that,
/// and [`is_collapse`] — deliberately — does not: 60 is not far enough below
/// 90 to be a restart rather than a correction. Replaying such a row installs
/// an anchor for an instant nothing restarted at, and nothing downstream
/// rejects it: the bracket sits *inside* the live window, so it clears every
/// filter `effective_window` applies. The window then reads as far shorter
/// than it ran, and `pace`, `cap in` and the projection are all measured
/// against a span that never happened — the over-reporting direction, which is
/// the costly one, and it survives until the next announced rollover retires
/// it. For a provider that does not move `reset_at`, that is days.
///
/// `window` supplies the scale. [`is_collapse`] is stated over *utilizations*
/// — it needs a ratio to judge "far enough" against — while the row stores
/// both readings in the window's own unit and carries neither the unit nor the
/// limit. Feeding it `prev_used`/`new_used` raw would compare a percentage
/// against a threshold meant for a fraction, so a 0.08% → 0.03% wobble would
/// read as a restart. The live window's unit and limit are the provider's
/// schema and do not move between polls, so they are the right scale to
/// re-judge an old row with; without one, no row replays.
fn replays_as_a_reset(rollover: &WindowRollover, window: &QuotaWindow) -> bool {
    if !unannounced(rollover) {
        return false;
    }
    let scaled = |used: f64| {
        let mut scale = window.clone();
        scale.used = used;
        scale.utilization()
    };
    match (scaled(rollover.prev_used), scaled(rollover.new_used)) {
        (Some(prev), Some(new)) => is_collapse(prev, new),
        // No scale, so no judgement — and `detect` would not have written the
        // row either, since its own rule needs both utilizations too.
        _ => false,
    }
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
                observed_starts: HashMap::new(),
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
            st.latest_success.get(&event.account).map(|e| e.ts),
            event.ts,
        );
        // An unannounced restart is where the current window began, and the
        // only evidence there is for it. It counts here even though it is
        // deliberately not a chart boundary: the two questions are different.
        // Whether to break a drawn series on a `used`-only signal is about
        // drawing; where the window a rate is measured against began is about
        // arithmetic, and on that one the evidence is good enough.
        //
        // An announced rollover does the opposite — it retires the anchor,
        // because `reset_at` has moved and the provider's own arithmetic is
        // now both correct and more precise than any bracket.
        //
        // Applied whatever the write below does. A failed `record_poll` costs
        // durability, not truth: the restart was still observed, and this is
        // the only poll pair that can see it. Withholding the anchor until the
        // row lands looks tidier and is worse — `latest_success` is replaced
        // regardless a few lines down, so the next poll compares against a
        // payload that was never persisted, the collapse is never detected
        // again, and the anchor is lost for the rest of the window instance
        // rather than for one poll. Nor would it make memory agree with
        // storage, for the same reason.
        for r in rollovers.iter().filter(|r| r.kind.is_surprise()) {
            tracing::info!(
                account = %r.account, window = %r.window, kind = r.kind.as_str(),
                prev_reset_at = ?r.prev_reset_at, new_reset_at = ?r.new_reset_at,
                prev_used = r.prev_used, new_used = r.new_used,
                "quota window reset unexpectedly"
            );
        }
        for r in &rollovers {
            let key = (r.account.clone(), r.window.clone());
            if unannounced(r) {
                st.observed_starts
                    .insert(key, ObservedStart::from_rollover(r));
            } else {
                st.observed_starts.remove(&key);
            }
        }
        prune_anchors(&mut st, &event.account, &windows, event.ts);
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
        // Restore the anchor too: without it the first status after a restart
        // would fall back to the provider's arithmetic and report a pace the
        // daemon already knew to be wrong.
        let now = chrono::Utc::now();
        // The scale each stored row has to be re-judged against, taken from the
        // newest successful poll restored just above: a rollover row carries
        // two readings but not the unit or limit that make them a ratio.
        let scales: HashMap<WindowId, QuotaWindow> = match st
            .latest_success
            .get(&account.id)
            .map(|event| &event.outcome)
        {
            Some(PollOutcome::Success { windows }) => {
                windows.iter().map(|w| (w.id.clone(), w.clone())).collect()
            }
            _ => HashMap::new(),
        };
        match st
            .storage
            .rollovers(&account.id, None, now - ANCHOR_LOOKBACK, now)
        {
            // Oldest first, replayed in order, so an announced rollover after
            // an unannounced one retires it exactly as it did when live.
            Ok(found) => {
                for r in &found {
                    let key = (r.account.clone(), r.window.clone());
                    let replay = scales
                        .get(&r.window)
                        .is_some_and(|scale| replays_as_a_reset(r, scale));
                    if replay {
                        st.observed_starts
                            .insert(key, ObservedStart::from_rollover(r));
                    } else if unannounced(r) {
                        // Recorded as unannounced by an older rule, and not a
                        // reset under this one. Skipped rather than retired:
                        // the current detector would not have written the row
                        // at all, so replaying it must neither anchor the
                        // window nor clear an anchor an earlier row set.
                        tracing::debug!(
                            account = %r.account, window = %r.window,
                            prev_used = r.prev_used, new_used = r.new_used,
                            "ignoring a stored unannounced rollover that is not a reset under the current rule"
                        );
                    } else {
                        st.observed_starts.remove(&key);
                    }
                }
                let live: Vec<QuotaWindow> = scales.values().cloned().collect();
                prune_anchors(&mut st, &account.id, &live, now);
            }
            Err(e) => {
                tracing::warn!(account = %account.id, error = %e, "failed to restore window anchors")
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
                            observed_start: st
                                .observed_starts
                                .get(&(a.id.clone(), window.id.clone()))
                                .copied(),
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

    /// Write a rollover row straight to storage, the way an *older* binary
    /// would have.
    ///
    /// Deliberately not via `record_event`: that runs the detector compiled in
    /// here, which by construction will not produce the rows these tests are
    /// about. A pre-upgrade database is exactly a set of rows the current rule
    /// would not have written.
    fn store_rollover(
        daemon: &Daemon,
        kind: RolloverKind,
        prev_used: f64,
        new_used: f64,
        minutes_ago: i64,
    ) {
        store_rollover_of(daemon, window(), kind, prev_used, new_used, minutes_ago);
    }

    /// As [`store_rollover`], for a window whose span the test chooses.
    fn store_rollover_of(
        daemon: &Daemon,
        window: QuotaWindow,
        kind: RolloverKind,
        prev_used: f64,
        new_used: f64,
        minutes_ago: i64,
    ) {
        let observed_at = chrono::Utc::now() - chrono::Duration::minutes(minutes_ago);
        let ev = event(PollOutcome::Success {
            windows: vec![window.clone()],
        });
        let rollover = WindowRollover {
            account: account().id,
            window: window.id.clone(),
            poll: ev.id,
            observed_at,
            kind,
            prev_reset_at: None,
            new_reset_at: None,
            prev_used,
            new_used,
            prev_observed_at: Some(observed_at - chrono::Duration::minutes(5)),
        };
        daemon
            .state
            .borrow_mut()
            .storage
            .record_poll(&ev, std::slice::from_ref(&window), &[rollover])
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }

    fn anchors(daemon: &Daemon) -> Vec<(AccountId, WindowId)> {
        let mut keys: Vec<_> = daemon
            .state
            .borrow()
            .observed_starts
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// The upgrade hazard: rows written under the retired rule must not be
    /// replayed as anchors under the new one.
    ///
    /// The old binary called any drop over 0.25 an unannounced reset, with no
    /// ratio guard, so a provider correcting 90% → 60% was recorded as one.
    /// This rule rejects that as a correction, and its own test at
    /// `rollover.rs` pins `detected(window(90), window(50)) == None`. Replaying
    /// the stored row anyway would anchor the window to an instant nothing
    /// restarted at — and nothing downstream would catch it, because the
    /// bracket sits inside the live window and clears every filter
    /// `effective_window` applies.
    #[test]
    fn a_stored_rollover_the_current_rule_rejects_is_not_replayed_as_an_anchor() {
        let dir = std::env::temp_dir().join(format!("teiryod-replay-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("teiryo.db");

        {
            let old = seeded(&path);
            store_rollover(&old, RolloverKind::Unannounced, 90.0, 60.0, 60);
        }

        let restarted = seeded(&path);
        restarted.hydrate_account(&account());
        assert!(
            anchors(&restarted).is_empty(),
            "a 90% → 60% correction is not a restart under this rule"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The other half of the same judgement: a row that *is* a reset under the
    /// current rule still hydrates, or the re-check would have thrown out the
    /// feature along with the bad rows.
    #[test]
    fn a_stored_rollover_the_current_rule_accepts_is_replayed_as_an_anchor() {
        let dir = std::env::temp_dir().join(format!("teiryod-replay-ok-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("teiryo.db");

        {
            let old = seeded(&path);
            store_rollover(&old, RolloverKind::Unannounced, 90.0, 2.0, 60);
        }

        let restarted = seeded(&path);
        restarted.hydrate_account(&account());
        assert_eq!(anchors(&restarted), vec![(account().id, window().id)]);
        // And it carries the bracket the row recorded, not the instant of the
        // poll that noticed: five minutes wide, per `store_rollover`.
        let st = restarted.state.borrow();
        let start = st.observed_starts[&(account().id, window().id)];
        assert_eq!(start.uncertainty(), chrono::Duration::minutes(5));
        drop(st);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Replay is oldest-first so it lands where the live path would have: an
    /// announced rollover *after* an unannounced one retires it, and the same
    /// two rows in the other order leave the anchor standing.
    ///
    /// Both halves are needed. Without the second, deleting the retire branch
    /// entirely still passes, since nothing would have been anchored anyway.
    #[test]
    fn replay_retires_an_anchor_in_the_order_the_rollovers_happened() {
        let dir = std::env::temp_dir().join(format!("teiryod-order-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();

        let retired = dir.join("retired.db");
        {
            let old = seeded(&retired);
            store_rollover(&old, RolloverKind::Unannounced, 90.0, 2.0, 60);
            store_rollover(&old, RolloverKind::Early, 40.0, 1.0, 30);
        }
        let restarted = seeded(&retired);
        restarted.hydrate_account(&account());
        assert!(
            anchors(&restarted).is_empty(),
            "the later announced rollover retires the anchor"
        );

        let standing = dir.join("standing.db");
        {
            let old = seeded(&standing);
            store_rollover(&old, RolloverKind::Early, 40.0, 1.0, 60);
            store_rollover(&old, RolloverKind::Unannounced, 90.0, 2.0, 30);
        }
        let restarted = seeded(&standing);
        restarted.hydrate_account(&account());
        assert_eq!(
            anchors(&restarted),
            vec![(account().id, window().id)],
            "the announced rollover came first and retires nothing after it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The live counterpart of the replay above: an announced rollover retires
    /// an anchor that an unannounced one set.
    ///
    /// Deleting the retire branch left the whole suite green. The existing
    /// coverage reaches the announced arm only in scenarios where no anchor
    /// had ever been set, so there was never anything there for it to remove —
    /// the branch was executed and asserted by nothing.
    #[test]
    fn an_announced_rollover_retires_a_live_anchor() {
        let dir = std::env::temp_dir().join(format!("teiryod-retire-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let daemon = seeded(&dir.join("teiryo.db"));

        let reset = chrono::Utc::now() + chrono::Duration::hours(2);
        let reading = |used: f64, reset_at| {
            let mut w = window();
            w.used = used;
            w.reset_at = Some(reset_at);
            w
        };

        daemon.record_event(&event(PollOutcome::Success {
            windows: vec![reading(90.0, reset)],
        }));
        std::thread::sleep(Duration::from_millis(2));
        // `reset_at` held still and usage collapsed: an unannounced restart,
        // and the only evidence there is for where this window began.
        daemon.record_event(&event(PollOutcome::Success {
            windows: vec![reading(2.0, reset)],
        }));
        assert_eq!(
            anchors(&daemon),
            vec![(account().id, window().id)],
            "precondition: the silent restart anchored the window"
        );

        std::thread::sleep(Duration::from_millis(2));
        // Now `reset_at` moves. The provider has stated where the new window
        // ends, so `reset_at - span` is its own account of where that window
        // began — exact, and better than any bracket. The anchor must go.
        daemon.record_event(&event(PollOutcome::Success {
            windows: vec![reading(5.0, reset + chrono::Duration::hours(5))],
        }));
        assert!(
            anchors(&daemon).is_empty(),
            "an announced rollover must retire the anchor, not sit beside it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bound `docs/domain.md` describes — "bounded by the window's
    /// length" — enforced on the anchor map rather than left to prose.
    ///
    /// Nothing else expresses it. An anchor is retired directly only by a
    /// later announced rollover, and the provider this feature targets never
    /// sends one; `effective_window`'s `estimate() > reset_at - span` guard
    /// moves only when `reset_at` does, which for that provider is never.
    #[test]
    fn an_anchor_older_than_its_window_is_pruned() {
        let dir = std::env::temp_dir().join(format!("teiryod-prune-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();

        // `window()` is a 5-hour window, so a bracket six hours old describes
        // an instance that has already ended.
        let expired = dir.join("expired.db");
        {
            let old = seeded(&expired);
            store_rollover(&old, RolloverKind::Unannounced, 90.0, 2.0, 6 * 60);
        }
        let restarted = seeded(&expired);
        restarted.hydrate_account(&account());
        assert!(anchors(&restarted).is_empty(), "older than its own span");

        // Four hours old is still inside the window it anchors, and survives —
        // without this half, pruning everything would also pass.
        let live = dir.join("live.db");
        {
            let old = seeded(&live);
            store_rollover(&old, RolloverKind::Unannounced, 90.0, 2.0, 4 * 60);
        }
        let restarted = seeded(&live);
        restarted.hydrate_account(&account());
        assert_eq!(anchors(&restarted), vec![(account().id, window().id)]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `ANCHOR_LOOKBACK` bounds the replay. A reset older than it belongs to a
    /// window instance long finished, and reading further back would scan a
    /// database kept for months on every startup.
    ///
    /// The fixture window is 30 days long on purpose. With the 5-hour one,
    /// `prune_anchors` drops the anchor for being older than its own window
    /// before the lookback is ever consulted — so widening the query to ten
    /// years left this test green and it pinned nothing. The row has to be
    /// older than the lookback and younger than the window it anchors.
    #[test]
    fn replay_ignores_rollovers_older_than_the_anchor_lookback() {
        let dir = std::env::temp_dir().join(format!("teiryod-lookback-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("teiryo.db");
        let long = || {
            let mut w = window();
            w.reset_kind = ResetKind::Rolling(Duration::from_secs(30 * 24 * 3600));
            w
        };

        {
            let old = seeded(&path);
            let past = ANCHOR_LOOKBACK.num_minutes() + 60;
            store_rollover_of(&old, long(), RolloverKind::Unannounced, 90.0, 2.0, past);
        }
        let restarted = seeded(&path);
        restarted.hydrate_account(&account());
        assert!(anchors(&restarted).is_empty(), "older than the lookback");

        // A day inside the lookback, and well inside the 30-day window, so the
        // lookback is the only thing that could have excluded the one above.
        let inside = dir.join("inside.db");
        {
            let old = seeded(&inside);
            let recent = ANCHOR_LOOKBACK.num_minutes() - 24 * 60;
            store_rollover_of(&old, long(), RolloverKind::Unannounced, 90.0, 2.0, recent);
        }
        let restarted = seeded(&inside);
        restarted.hydrate_account(&account());
        assert_eq!(anchors(&restarted), vec![(account().id, window().id)]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The live pruning path, which only `hydrate_account`'s was covering —
    /// and the live one is the whole point: the doc is about a process that
    /// keeps running, which is exactly where hydration never happens again.
    #[test]
    fn a_running_daemon_prunes_an_anchor_its_window_outlived() {
        let dir = std::env::temp_dir().join(format!("teiryod-liveprune-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let daemon = seeded(&dir.join("teiryo.db"));

        let at = |used: f64, reset_at| {
            let mut w = window();
            w.used = used;
            w.reset_at = Some(reset_at);
            w
        };
        let reset = chrono::Utc::now() + chrono::Duration::hours(2);
        daemon.record_event(&event(PollOutcome::Success {
            windows: vec![at(90.0, reset)],
        }));
        std::thread::sleep(Duration::from_millis(2));
        daemon.record_event(&event(PollOutcome::Success {
            windows: vec![at(2.0, reset)],
        }));
        assert_eq!(
            anchors(&daemon),
            vec![(account().id, window().id)],
            "precondition: the silent restart anchored the window"
        );

        // Six hours on, past the 5-hour window the anchor belongs to. No
        // announced rollover ever arrives — the provider does not move
        // `reset_at`, which is the case this bound exists for.
        std::thread::sleep(Duration::from_millis(2));
        let mut later = event(PollOutcome::Success {
            windows: vec![at(3.0, reset)],
        });
        later.ts = chrono::Utc::now() + chrono::Duration::hours(6);
        daemon.record_event(&later);
        assert!(
            anchors(&daemon).is_empty(),
            "a running daemon must not keep an anchor its window outlived"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A window missing from one payload must not cost its anchor.
    ///
    /// Judging it against the longest span the account still publishes deletes
    /// a live anchor: three days into an observed weekly restart, the first
    /// payload that omits `weekly` leaves only the 5-hour span to compare
    /// against, and the weekly anchor is older than that. The pace then falls
    /// back to an instant inside a window that already ended — the exact
    /// under-reporting this branch exists to fix.
    #[test]
    fn a_window_absent_from_one_payload_keeps_its_anchor() {
        let dir = std::env::temp_dir().join(format!("teiryod-partial-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let daemon = seeded(&dir.join("teiryo.db"));

        let weekly = |used: f64| {
            let mut w = window();
            w.id = WindowId::from("weekly");
            w.reset_kind = ResetKind::Rolling(Duration::from_secs(7 * 24 * 3600));
            w.used = used;
            w.reset_at = Some(chrono::Utc::now() + chrono::Duration::days(4));
            w
        };
        daemon.record_event(&event(PollOutcome::Success {
            windows: vec![window(), weekly(90.0)],
        }));
        std::thread::sleep(Duration::from_millis(2));
        daemon.record_event(&event(PollOutcome::Success {
            windows: vec![window(), weekly(2.0)],
        }));
        let anchored = (account().id, WindowId::from("weekly"));
        assert!(anchors(&daemon).contains(&anchored), "precondition");

        // Three days on, one payload omits the weekly window entirely.
        std::thread::sleep(Duration::from_millis(2));
        let mut partial = event(PollOutcome::Success {
            windows: vec![window()],
        });
        partial.ts = chrono::Utc::now() + chrono::Duration::days(3);
        daemon.record_event(&partial);
        assert!(
            anchors(&daemon).contains(&anchored),
            "a payload omitting a window says nothing about that window's anchor"
        );

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
