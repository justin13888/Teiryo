//! Claude (subscription) adapter.
//!
//! Usage is exposed as *headroom percentages*, not raw counts: a rolling
//! 5-hour session window and rolling weekly windows (separate Opus/Sonnet
//! buckets on Max plans). Hitting a cap hard-blocks new prompts.

mod credentials;
mod parser;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use secrecy::ExposeSecret;
use teiryo_core::{
    Account, AccountId, AuthError, BarStyle, Credential, ParseError, ProbeError, Prober,
    ProviderAdapter, ProviderId, QuotaParser, QuotaWindow, RawResponse, RenderHint, WindowId,
    WindowPresenter,
};
use tokio::sync::Mutex;

pub use parser::ASSUMED_SCHEMA;

/// Stable provider id.
pub const PROVIDER_ID: &str = "claude";

/// Default API base URL (the host Claude Code itself talks to).
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Usage endpoint path, relative to the base URL.
pub const USAGE_PATH: &str = "/api/oauth/usage";

/// OAuth beta header required by the usage endpoint.
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// User agent mirroring the real Claude Code CLI.
const USER_AGENT: &str = "claude-cli/1.0.0 (external, cli)";

/// Env var overriding the credentials file path (mainly for tests/config).
pub const ENV_CREDENTIALS_PATH: &str = "TEIRYO_CLAUDE_CREDENTIALS";

/// Env var overriding the API base URL (mainly for tests/config).
pub const ENV_BASE_URL: &str = "TEIRYO_CLAUDE_BASE_URL";

/// Claude subscription adapter: discovers the local Claude Code login,
/// probes the OAuth usage endpoint, and parses headroom percentages.
pub struct ClaudeAdapter {
    credentials_path: PathBuf,
    base_url: String,
    /// One persistent HTTP client per account — separate accounts must not
    /// share a connection pool; reuse across probes keeps connection fidelity.
    clients: Mutex<HashMap<AccountId, reqwest::Client>>,
    group_order: Vec<WindowId>,
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeAdapter {
    /// Adapter with default paths, honoring the `TEIRYO_CLAUDE_*` env
    /// overrides.
    pub fn new() -> Self {
        let credentials_path = std::env::var_os(ENV_CREDENTIALS_PATH)
            .map(PathBuf::from)
            .unwrap_or_else(credentials::default_path);
        let base_url = std::env::var(ENV_BASE_URL).unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        Self::with_config(credentials_path, base_url)
    }

    /// Adapter with explicit credentials path and API base URL.
    pub fn with_config(credentials_path: PathBuf, base_url: String) -> Self {
        Self {
            credentials_path,
            base_url,
            clients: Mutex::new(HashMap::new()),
            group_order: parser::group_order(),
        }
    }

    /// The persistent client for `account`, built on first use.
    async fn client_for(&self, account: &AccountId) -> Result<reqwest::Client, ProbeError> {
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(account) {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ProbeError::Network(format!("building http client: {e}")))?;
        clients.insert(account.clone(), client.clone());
        Ok(client)
    }
}

#[async_trait]
impl teiryo_core::Authenticator for ClaudeAdapter {
    async fn discover_accounts(&self) -> Result<Vec<Account>, AuthError> {
        credentials::load(&self.credentials_path)?;
        Ok(vec![Account {
            id: AccountId::from("claude:default"),
            provider: PROVIDER_ID.to_owned(),
            label: "default".to_owned(),
        }])
    }

    async fn credential_for(&self, _account: &Account) -> Result<Credential, AuthError> {
        let creds = credentials::load(&self.credentials_path)?;
        if creds.is_expired() {
            return Err(AuthError::Expired(
                "Claude Code access token is past its expiry; run `claude` to refresh".into(),
            ));
        }
        Ok(Credential::OAuthToken(creds.access_token))
    }
}

#[async_trait]
impl Prober for ClaudeAdapter {
    async fn probe(&self, account: &Account, cred: &Credential) -> Result<RawResponse, ProbeError> {
        let token = match cred {
            Credential::OAuthToken(token) => token,
            other => {
                return Err(ProbeError::Auth(format!(
                    "claude adapter needs an OAuth token, got {other:?}"
                )));
            }
        };
        let client = self.client_for(&account.id).await?;
        let url = format!("{}{}", self.base_url, USAGE_PATH);
        let response = client
            .get(&url)
            .bearer_auth(token.expose_secret())
            .header("anthropic-beta", OAUTH_BETA_HEADER)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ProbeError::Network(e.to_string()))?;

        let status = response.status();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into()))
            .collect();

        match status.as_u16() {
            401 | 403 => Err(ProbeError::Auth(format!(
                "usage endpoint returned {status}"
            ))),
            429 => {
                let retry_after = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
                    .and_then(|(_, v)| v.parse::<u64>().ok())
                    .map(Duration::from_secs);
                Err(ProbeError::RateLimited { retry_after })
            }
            code if !status.is_success() => Err(ProbeError::Provider(format!(
                "usage endpoint returned {code}"
            ))),
            _ => {
                let body = response
                    .bytes()
                    .await
                    .map_err(|e| ProbeError::Network(e.to_string()))?
                    .to_vec();
                Ok(RawResponse {
                    status: status.as_u16(),
                    headers,
                    body,
                    fetched_at: chrono::Utc::now(),
                })
            }
        }
    }
}

impl QuotaParser for ClaudeAdapter {
    fn parse(&self, raw: &RawResponse) -> Result<Vec<QuotaWindow>, ParseError> {
        parser::parse(raw)
    }
}

impl WindowPresenter for ClaudeAdapter {
    fn render_hint(&self, _window: &QuotaWindow) -> RenderHint {
        RenderHint {
            style: BarStyle::Percent,
            warn_threshold: 0.8,
            critical_threshold: 0.95,
            note: Some("Blocks entirely at cap".to_owned()),
        }
    }

    fn group_order(&self) -> &[WindowId] {
        &self.group_order
    }
}

impl ProviderAdapter for ClaudeAdapter {
    fn id(&self) -> ProviderId {
        PROVIDER_ID.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_hint_flags_hard_block() {
        let adapter = ClaudeAdapter::with_config(PathBuf::from("/nonexistent"), String::new());
        let hint = adapter.render_hint(&parser::test_window());
        assert_eq!(hint.style, BarStyle::Percent);
        assert!((hint.warn_threshold - 0.8).abs() < f32::EPSILON);
        assert!((hint.critical_threshold - 0.95).abs() < f32::EPSILON);
        assert_eq!(hint.note.as_deref(), Some("Blocks entirely at cap"));
    }

    #[test]
    fn group_order_lists_session_before_weekly() {
        let adapter = ClaudeAdapter::with_config(PathBuf::from("/nonexistent"), String::new());
        let order = adapter.group_order();
        let pos = |id: &str| {
            order
                .iter()
                .position(|w| w.0 == id)
                .unwrap_or_else(|| panic!("{id} missing from group order"))
        };
        assert!(pos("session_5h") < pos("weekly"));
        assert!(pos("weekly") < pos("weekly_opus"));
    }
}
