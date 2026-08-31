//! Core domain model: providers, accounts, quota windows, and poll events.
//!
//! `std::time::Duration` fields serialize via serde's standard `(secs, nanos)`
//! representation; `chrono` timestamps serialize as RFC 3339 strings.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Provider identifier, e.g. `"claude"`, `"openai"`. Small open set, no enum.
pub type ProviderId = String;

/// Stable, provider-derived account identifier (stable across daemon restarts).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AccountId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Provider-defined quota window identifier, e.g. `"session_5h_opus"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowId(pub String);

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for WindowId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Core-generated, sortable poll identifier (ULID).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PollId(pub Ulid);

impl PollId {
    /// Generate a fresh, time-ordered id.
    pub fn generate() -> Self {
        Self(Ulid::new())
    }

    /// The zero id — sorts before every generated id. Useful as an
    /// `AwaitUpdate { since }` starting point.
    pub fn zero() -> Self {
        Self(Ulid::nil())
    }
}

impl fmt::Display for PollId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One provider account, e.g. a personal and a work Claude login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Stable provider-derived id.
    pub id: AccountId,
    /// Owning provider.
    pub provider: ProviderId,
    /// Human label, e.g. `"personal"`, `"work"`.
    pub label: String,
}

/// What a quota window applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowScope {
    /// Applies to the whole account.
    AccountWide,
    /// Applies to a single model, e.g. `Model("opus")`.
    Model(String),
}

/// How a quota window resets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetKind {
    /// Anchored rolling window of the given length (e.g. 5 hours, 7 days).
    Rolling(Duration),
}

/// Unit the provider reports usage in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuotaUnit {
    /// Usage as a percentage of an unpublished limit.
    Percent,
    /// Message counts.
    Messages,
    /// Token counts.
    Tokens,
    /// Hours of usage.
    Hours,
}

/// One quota window for one account, as reported by a single poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Provider-defined window id.
    pub id: WindowId,
    /// Adapter-supplied display label, e.g. `"Opus — 5 hour"`.
    pub label: String,
    /// What the window applies to.
    pub scope: WindowScope,
    /// How the window resets.
    pub reset_kind: ResetKind,
    /// Reporting unit.
    pub unit: QuotaUnit,
    /// Amount used, in `unit`.
    pub used: f64,
    /// Limit in `unit`; `None` where the provider only exposes % remaining.
    pub limit: Option<f64>,
    /// When the window resets, if known.
    pub reset_at: Option<DateTime<Utc>>,
}

/// Utilization ratio in `0.0..=1.0` from a reading's own three fields, when
/// computable.
///
/// `Percent` readings are self-describing; anything else needs a published
/// limit, which not every provider gives (see `docs/providers.md`). Shared by
/// [`QuotaWindow`] and [`QuotaSnapshot`] so a live row and its own history can
/// never disagree about what a reading means.
fn utilization_of(unit: QuotaUnit, used: f64, limit: Option<f64>) -> Option<f64> {
    match (unit, limit) {
        (QuotaUnit::Percent, _) => Some((used / 100.0).clamp(0.0, 1.0)),
        (_, Some(limit)) if limit > 0.0 => Some((used / limit).clamp(0.0, 1.0)),
        _ => None,
    }
}

impl QuotaWindow {
    /// Utilization ratio in `0.0..=1.0`, when computable from the window's own
    /// fields.
    pub fn utilization(&self) -> Option<f64> {
        utilization_of(self.unit, self.used, self.limit)
    }

    /// How long the window rolls over, as a `chrono` duration.
    pub fn span(&self) -> Option<chrono::Duration> {
        match self.reset_kind {
            ResetKind::Rolling(d) => chrono::Duration::from_std(d).ok(),
        }
    }

    /// When the current window began: `reset_at` less its own length. `None`
    /// when the provider did not publish `reset_at`, since without it the
    /// window's start is unknowable.
    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        Some(self.reset_at? - self.span()?)
    }
}

/// What caused a poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PollTrigger {
    /// The scheduler's interval fired.
    Scheduled,
    /// A client requested it via `PollNow`.
    Manual {
        /// The client that asked.
        client: ClientKind,
    },
    /// Initial poll on daemon startup.
    Startup,
}

/// Kind of client that triggered a manual poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKind {
    /// The `teiryo` TUI.
    Tui,
    /// A future non-TUI caller, e.g. a CLI or HTTP shim.
    Other(String),
}

/// Result of one poll of one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PollOutcome {
    /// Probe and parse succeeded; all windows the account reported.
    Success {
        /// Windows returned by this poll (usually several).
        windows: Vec<QuotaWindow>,
    },
    /// Credentials missing, invalid, or expired.
    AuthError(String),
    /// Transport-level failure.
    NetworkError(String),
    /// Response arrived but no longer matches the expected schema.
    SchemaDrift(String),
    /// Provider rate-limited the probe itself.
    RateLimited {
        /// Provider-suggested retry delay, if any.
        retry_after: Option<Duration>,
    },
}

impl PollOutcome {
    /// Error message, if this outcome is a failure.
    pub fn error_message(&self) -> Option<&str> {
        match self {
            PollOutcome::Success { .. } => None,
            PollOutcome::AuthError(m)
            | PollOutcome::NetworkError(m)
            | PollOutcome::SchemaDrift(m) => Some(m),
            PollOutcome::RateLimited { .. } => Some("rate limited"),
        }
    }
}

/// One completed poll: trigger, outcome, and timing, for one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollEvent {
    /// Sortable poll id.
    pub id: PollId,
    /// When the poll completed.
    pub ts: DateTime<Utc>,
    /// Provider polled.
    pub provider: ProviderId,
    /// Account polled.
    pub account: AccountId,
    /// What caused the poll.
    pub trigger: PollTrigger,
    /// What happened.
    pub outcome: PollOutcome,
    /// Wall-clock probe latency in milliseconds.
    pub latency_ms: u32,
}

/// One historical quota reading: a `QuotaWindow` measurement tied to the poll
/// that produced it. Scope/reset kind are not persisted — history is about
/// `used`/`limit` over time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    /// Poll that produced this reading.
    pub poll_id: PollId,
    /// When the poll completed.
    pub ts: DateTime<Utc>,
    /// Window measured.
    pub window: WindowId,
    /// Display label at the time of the poll.
    pub label: String,
    /// Reporting unit.
    pub unit: QuotaUnit,
    /// Amount used, in `unit`.
    pub used: f64,
    /// Limit in `unit`, if the provider publishes one.
    pub limit: Option<f64>,
    /// When the window resets, if known.
    pub reset_at: Option<DateTime<Utc>>,
}

impl QuotaSnapshot {
    /// Utilization ratio in `0.0..=1.0` at the moment of this reading, when
    /// computable from the fields the snapshot carries.
    ///
    /// The same rule [`QuotaWindow::utilization`] applies, so a rate measured
    /// across two snapshots is in the same units as the live row's gauge.
    pub fn utilization(&self) -> Option<f64> {
        utilization_of(self.unit, self.used, self.limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(unit: QuotaUnit, used: f64, limit: Option<f64>) -> QuotaSnapshot {
        QuotaSnapshot {
            poll_id: PollId::generate(),
            ts: Utc::now(),
            window: WindowId::from("w"),
            label: "w".to_owned(),
            unit,
            used,
            limit,
            reset_at: None,
        }
    }

    #[test]
    fn a_snapshot_reads_its_utilization_the_way_its_window_does() {
        assert_eq!(
            snapshot(QuotaUnit::Percent, 42.0, Some(100.0)).utilization(),
            Some(0.42)
        );
        assert_eq!(
            snapshot(QuotaUnit::Messages, 30.0, Some(60.0)).utilization(),
            Some(0.5)
        );
        // No published limit: a count alone says nothing about headroom.
        assert_eq!(
            snapshot(QuotaUnit::Messages, 30.0, None).utilization(),
            None
        );
        // Overuse clamps rather than reporting more than a full window.
        assert_eq!(
            snapshot(QuotaUnit::Percent, 150.0, None).utilization(),
            Some(1.0)
        );
    }
}
