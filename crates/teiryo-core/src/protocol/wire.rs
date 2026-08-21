//! Post-handshake request/response enums, bincode-encoded inside
//! length-delimited frames.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{
    Account, AccountId, PollEvent, PollId, ProviderId, QuotaSnapshot, QuotaWindow, WindowId,
};
use crate::error::ErrorKind;

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
    /// Long-poll for the next completed poll newer than `since`.
    AwaitUpdate {
        /// Newest poll id the client has already seen.
        since: PollId,
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
    History(Vec<QuotaSnapshot>),
    /// Reply to `RecentPolls`.
    RecentPolls(Vec<PollEvent>),
    /// Reply to `Providers`.
    Providers(Vec<ProviderHealth>),
    /// Generic acknowledgement (e.g. `Shutdown`).
    Ack,
    /// Request failed.
    Err(ErrorKind, String),
}

/// Live status of one account: its windows and the poll that produced them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountStatus {
    /// The account.
    pub account: Account,
    /// Latest known windows for the account.
    pub windows: Vec<QuotaWindow>,
    /// The poll those windows came from, if any poll has completed yet.
    pub last_poll: Option<PollEvent>,
}

/// Health of one provider across its accounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealth {
    /// The provider.
    pub provider: ProviderId,
    /// Accounts discovered for it.
    pub accounts: Vec<AccountId>,
    /// Consecutive failed polls (0 = healthy).
    pub consecutive_failures: u32,
    /// Most recent error message, if the last poll failed.
    pub last_error: Option<String>,
}
