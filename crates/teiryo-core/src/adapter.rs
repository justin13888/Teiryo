//! Provider adapter traits, split by concern so the TUI never needs
//! provider internals: authentication, probing, parsing, and presentation.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use secrecy::SecretString;

use crate::domain::{Account, ProviderId, QuotaWindow, WindowId};
use crate::error::{AuthError, ParseError, ProbeError};

/// A provider credential. Wraps [`SecretString`] so secrets are zeroized on
/// drop and never leak through `Debug`/`Display` — `Debug` prints only the
/// variant name.
pub enum Credential {
    /// OAuth access token.
    OAuthToken(SecretString),
    /// Plain API key.
    ApiKey(SecretString),
    /// Serialized cookie jar (e.g. a browser session).
    CookieJar(SecretString),
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Credential::OAuthToken(_) => "OAuthToken",
            Credential::ApiKey(_) => "ApiKey",
            Credential::CookieJar(_) => "CookieJar",
        };
        write!(f, "Credential::{name}([REDACTED])")
    }
}

/// Raw bytes of one usage probe, before provider-specific parsing.
#[derive(Debug, Clone)]
pub struct RawResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers (name, value), as received.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
    /// When the probe completed.
    pub fetched_at: DateTime<Utc>,
}

/// Discovers accounts and resolves their credentials.
#[async_trait]
pub trait Authenticator: Send + Sync {
    /// Enumerate locally-available accounts; enables multi-account from day one.
    async fn discover_accounts(&self) -> Result<Vec<Account>, AuthError>;

    /// Resolve the credential for one discovered account.
    async fn credential_for(&self, account: &Account) -> Result<Credential, AuthError>;
}

/// Performs one usage probe against the provider.
#[async_trait]
pub trait Prober: Send + Sync {
    /// Fetch the raw usage payload for `account` using `cred`.
    async fn probe(&self, account: &Account, cred: &Credential) -> Result<RawResponse, ProbeError>;
}

/// Parses a raw probe response into quota windows.
pub trait QuotaParser: Send + Sync {
    /// Parse all quota windows out of one raw response.
    fn parse(&self, raw: &RawResponse) -> Result<Vec<QuotaWindow>, ParseError>;
}

/// Provider-specific rendering rules, so the TUI stays provider-agnostic.
pub trait WindowPresenter: Send + Sync {
    /// How one window should be rendered.
    fn render_hint(&self, window: &QuotaWindow) -> RenderHint;

    /// Display grouping/order of the provider's windows.
    fn group_order(&self) -> &[WindowId];
}

/// A complete provider: all four concerns plus a stable id.
pub trait ProviderAdapter: Authenticator + Prober + QuotaParser + WindowPresenter {
    /// Stable provider id, e.g. `"claude"`.
    fn id(&self) -> ProviderId;
}

/// How the TUI should draw a quota window.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderHint {
    /// Gauge style.
    pub style: BarStyle,
    /// Fraction (0.0–1.0) at which to warn.
    pub warn_threshold: f32,
    /// Fraction (0.0–1.0) at which to alarm.
    pub critical_threshold: f32,
    /// Provider UX quirk surfaced to the user, e.g. "blocks entirely at cap".
    pub note: Option<String>,
}

/// Gauge style for a quota window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarStyle {
    /// Percentage bar (limit unpublished).
    Percent,
    /// `used / limit` fraction bar.
    FractionOfLimit,
    /// Plain count, no bar.
    CountOnly,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_never_leaks_secret() {
        let cred = Credential::ApiKey(SecretString::from("sk-super-secret"));
        let debug = format!("{cred:?}");
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("REDACTED"));
    }
}
