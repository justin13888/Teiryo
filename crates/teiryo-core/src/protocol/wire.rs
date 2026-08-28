//! Post-handshake request/response enums, bincode-encoded inside
//! length-delimited frames.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::adapter::RenderHint;
use crate::domain::{
    Account, AccountId, PollEvent, PollId, ProviderId, QuotaSnapshot, QuotaWindow, WindowId,
};
use crate::error::ErrorKind;
use crate::rollover::WindowRollover;

/// Client → daemon request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Current status; `None` filters mean "all".
    Status {
        /// Restrict to one provider.
        provider: Option<ProviderId>,
        /// Restrict to one account.
        account: Option<AccountId>,
    },
    /// Trigger an immediate poll; `None` means all accounts on the provider.
    PollNow {
        /// Provider to poll.
        provider: ProviderId,
        /// Account to poll, or all of the provider's accounts.
        account: Option<AccountId>,
    },
    /// Long-poll for the next completed poll newer than `since`, **or** for a
    /// config reload newer than `config_gen` — whichever lands first.
    AwaitUpdate {
        /// Newest poll id the client has already seen.
        since: PollId,
        /// Newest [`ConfigState::generation`] the client has already seen.
        /// One long-poll therefore covers both of the daemon's clocks; a
        /// client that does not care about config passes `u64::MAX`.
        config_gen: u64,
        /// How long the daemon may hold the request open.
        timeout_ms: u32,
    },
    /// Historical snapshots for one account.
    History {
        /// Account to query.
        account: AccountId,
        /// Restrict to one window, or all windows.
        window: Option<WindowId>,
        /// Only snapshots at or after this instant.
        since: DateTime<Utc>,
        /// Only snapshots at or before this instant; `None` means "now".
        until: Option<DateTime<Utc>>,
        /// Downsample each window's series to at most this many points.
        /// `None` still applies the daemon's own cap — see
        /// [`crate::storage::MAX_HISTORY_POINTS`].
        max_points: Option<u32>,
    },
    /// The most recent poll events, newest first.
    RecentPolls {
        /// Maximum number of events to return.
        limit: u32,
    },
    /// Provider/account health overview.
    Providers,
    /// Ask the daemon to flush and exit.
    Shutdown,
    /// The settings currently in effect and how the last config load went.
    GetConfig,
    /// Apply one settings change: the daemon validates it, rewrites
    /// `config.toml`, and puts it into effect. New variants go **after** this
    /// one — bincode is positional.
    SetConfig(ConfigEdit),
}

/// Daemon → client response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    /// Reply to `Status`.
    Status(Vec<AccountStatus>),
    /// `PollNow` was queued; the result arrives via `AwaitUpdate`.
    PollAccepted {
        /// Id is assigned when the poll completes; this echoes the newest
        /// known poll id so clients can `AwaitUpdate { since }` from it.
        poll_id: PollId,
    },
    /// `AwaitUpdate` resolved with new data.
    Update(PollEvent),
    /// `AwaitUpdate` timed out with nothing new.
    NoUpdate,
    /// Reply to `History`.
    History(HistoryPage),
    /// Reply to `RecentPolls`.
    RecentPolls(Vec<PollEvent>),
    /// Reply to `Providers`.
    Providers(Vec<ProviderHealth>),
    /// Generic acknowledgement (e.g. `Shutdown`).
    Ack,
    /// Request failed.
    Err(ErrorKind, String),
    /// Reply to `GetConfig` and `SetConfig`, and how a config-woken
    /// `AwaitUpdate` resolves. New variants go **after** this one — bincode is
    /// positional.
    Config(ConfigState),
}

/// One page of history: the slice that was asked for, plus where the stored
/// series actually starts.
///
/// The slice alone can never say whether anything lies before it — a page that
/// begins at its own `since` looks identical whether that is the start of the
/// history or merely the start of the query — so a client scrolling backwards
/// through time would have to probe blindly to find the far end. `earliest` is
/// what lets it clamp the scroll to the data instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPage {
    /// Snapshots inside the requested interval, oldest first, downsampled to
    /// the requested budget.
    pub snapshots: Vec<QuotaSnapshot>,
    /// Timestamp of the oldest snapshot stored for the queried account and
    /// window, regardless of the interval asked for. `None` when the query
    /// matches nothing at all.
    pub earliest: Option<DateTime<Utc>>,
    /// Window rollovers observed inside the same interval, oldest first.
    ///
    /// Carried here rather than behind their own request so a chart's
    /// boundaries can never describe a different interval from its series.
    /// Never downsampled — see [`crate::storage::Storage::rollovers`].
    pub rollovers: Vec<WindowRollover>,
}

/// One quota window paired with the provider's rendering rules for it.
/// Bundling them keeps the two from drifting apart the way parallel `Vec`s
/// would, and keeps the TUI free of provider-specific thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowView {
    /// The window's current reading.
    pub window: QuotaWindow,
    /// How the provider wants it drawn.
    pub hint: RenderHint,
}

/// Live status of one account: its windows and the poll that produced them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountStatus {
    /// The account.
    pub account: Account,
    /// Latest known windows for the account, each with its render hint.
    pub windows: Vec<WindowView>,
    /// The most recent poll of any outcome, which may be a failure.
    pub last_poll: Option<PollEvent>,
    /// When the *successful* poll backing `windows` completed. Distinct from
    /// `last_poll.ts`: a later failure leaves the windows stale but valid, and
    /// only this field says how stale.
    pub last_success: Option<DateTime<Utc>>,
    /// Scheduler cadence for this account. Actual polls jitter ±10% around it.
    pub poll_interval_secs: u32,
}

/// Health of one account's poll task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountHealth {
    /// The account.
    pub account: AccountId,
    /// Consecutive failed polls (0 = healthy).
    pub consecutive_failures: u32,
    /// Most recent error message, if the last poll failed.
    pub last_error: Option<String>,
    /// When the last poll of any outcome completed.
    pub last_poll_ts: Option<DateTime<Utc>>,
    /// Scheduler cadence for this account.
    pub poll_interval_secs: u32,
}

/// The daemon's view of `config.toml`: what is in effect, and what its last
/// read of the file made of it.
///
/// The daemon reloads the file whenever it changes on disk, so `effective` and
/// the file can disagree: a rejected load leaves the previously applied
/// settings running and reports why in `error`. Keeping both in one struct is
/// what lets a client say "this is running, that edit did not take" rather
/// than silently showing one or the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigState {
    /// Absolute path of the file the daemon reads and writes.
    pub path: String,
    /// Bumped on every load attempt, successful or not. Clients pass the last
    /// value back in `AwaitUpdate` to be woken on the next one.
    pub generation: u64,
    /// The settings actually in effect.
    pub effective: ConfigView,
    /// When the daemon last read the file.
    pub loaded_at: DateTime<Utc>,
    /// Keys the file contained that the daemon does not know. Ignored — the
    /// rest of the file still applied.
    pub warnings: Vec<String>,
    /// Why the last load was rejected, if it was. `effective` then describes
    /// the last config that *did* apply, not the file on disk.
    pub error: Option<String>,
}

/// The settings in effect, with enough context for a client to label each
/// value's provenance and to avoid proposing one the daemon would reject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigView {
    /// Global poll interval override; `None` means the compiled-in default.
    pub poll_interval_secs: Option<u32>,
    /// The compiled-in default, so a client can render "180s (default)".
    pub default_poll_interval_secs: u32,
    /// Smallest interval the daemon accepts, so a client can clamp before
    /// sending rather than round-tripping a rejection.
    pub min_poll_interval_secs: u32,
    /// Per-provider settings, sorted by provider id.
    pub providers: Vec<ProviderSettings>,
}

/// One provider's settings and what they resolve to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSettings {
    /// The provider.
    pub provider: ProviderId,
    /// Whether the provider is polled at all.
    pub enabled: bool,
    /// Interval override; `None` inherits [`ConfigView::poll_interval_secs`].
    pub poll_interval_secs: Option<u32>,
    /// What the scheduler actually uses, after both fallbacks.
    pub effective_poll_interval_secs: u32,
}

/// One atomic settings change, as sent in `SetConfig`.
///
/// Deliberately one edit rather than a whole `ConfigView`: a client that
/// submitted the full document would clobber concurrent external edits between
/// its last `GetConfig` and its write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConfigEdit {
    /// Set or (with `None`) clear the global interval.
    GlobalPollInterval(Option<u32>),
    /// Set or (with `None`) clear one provider's interval override.
    ProviderPollInterval {
        /// Provider to change.
        provider: ProviderId,
        /// New override, or `None` to inherit the global value.
        secs: Option<u32>,
    },
    /// Enable or disable polling for one provider.
    ProviderEnabled {
        /// Provider to change.
        provider: ProviderId,
        /// Whether to poll it.
        enabled: bool,
    },
}

/// Health of one provider across its accounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealth {
    /// The provider.
    pub provider: ProviderId,
    /// Per-account health, in registry order.
    pub accounts: Vec<AccountHealth>,
    /// Worst `consecutive_failures` across the provider's accounts.
    pub consecutive_failures: u32,
    /// First error found across the provider's accounts, if any.
    pub last_error: Option<String>,
}
