//! Claude (subscription) adapter.
//!
//! Usage is exposed as *headroom percentages*, not raw counts: a rolling
//! 5-hour session window and rolling weekly windows (separate Opus/Sonnet
//! buckets on Max plans, plus per-model weekly caps such as Fable's that the
//! server reports only through its `limits[]` rows). Hitting a cap
//! hard-blocks new prompts.

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
    /// Read the macOS Keychain when `credentials_path` does not exist. Only
    /// for the default path: a path someone named explicitly is the one they
    /// meant, and silently reading a different store would hide a typo.
    keychain_fallback: bool,
    base_url: String,
    /// One persistent HTTP client per account — separate accounts must not
    /// share a connection pool; reuse across probes keeps connection fidelity.
    clients: Mutex<HashMap<AccountId, reqwest::Client>>,
}

/// Where an adapter reads credentials from on a given call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialSource {
    File,
    Keychain,
}

/// Whether the Keychain may stand in for a missing credentials file: on
/// macOS, and only when no `TEIRYO_CLAUDE_CREDENTIALS` path was given.
fn keychain_fallback_for(overridden: Option<&std::path::Path>) -> bool {
    cfg!(target_os = "macos") && overridden.is_none()
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeAdapter {
    /// Adapter with default paths, honoring the `TEIRYO_CLAUDE_*` env
    /// overrides.
    ///
    /// On macOS, with no `TEIRYO_CLAUDE_CREDENTIALS` override, credentials
    /// come from `~/.claude/.credentials.json` when it exists and from the
    /// login Keychain otherwise — where Claude Code keeps them by default.
    pub fn new() -> Self {
        let overridden = std::env::var_os(ENV_CREDENTIALS_PATH).map(PathBuf::from);
        let keychain_fallback = keychain_fallback_for(overridden.as_deref());
        let credentials_path = overridden.unwrap_or_else(credentials::default_path);
        let base_url = std::env::var(ENV_BASE_URL).unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        Self {
            keychain_fallback,
            ..Self::with_config(credentials_path, base_url)
        }
    }

    /// Adapter with explicit credentials path and API base URL. Reads that
    /// file only, never the Keychain.
    pub fn with_config(credentials_path: PathBuf, base_url: String) -> Self {
        Self {
            credentials_path,
            keychain_fallback: false,
            base_url,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Load the current credentials from wherever this adapter reads them.
    /// Decided per call: a credentials file that appears later wins from then
    /// on, as it does for Claude Code itself.
    async fn load_credentials(&self) -> Result<credentials::ClaudeCredentials, AuthError> {
        match self.credential_source() {
            CredentialSource::Keychain => credentials::load_keychain().await,
            CredentialSource::File => credentials::load(&self.credentials_path),
        }
    }

    /// Which store [`Self::load_credentials`] reads right now.
    fn credential_source(&self) -> CredentialSource {
        if self.keychain_fallback && !self.credentials_path.exists() {
            CredentialSource::Keychain
        } else {
            CredentialSource::File
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
        self.load_credentials().await?;
        Ok(vec![Account {
            id: AccountId::from("claude:default"),
            provider: PROVIDER_ID.to_owned(),
            label: "default".to_owned(),
        }])
    }

    async fn credential_for(&self, _account: &Account) -> Result<Credential, AuthError> {
        let creds = self.load_credentials().await?;
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

    fn id_is_server_derived(&self, id: &WindowId) -> bool {
        parser::is_server_derived(id)
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
}

impl ProviderAdapter for ClaudeAdapter {
    fn id(&self) -> ProviderId {
        PROVIDER_ID.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str =
        r#"{"claudeAiOauth":{"accessToken":"fake-token-for-test","expiresAt":99999999999999}}"#;

    /// An adapter as `new()` builds it on macOS with no override, pointed at
    /// `path`, so the fallback is exercised on every platform.
    fn with_fallback(path: PathBuf) -> ClaudeAdapter {
        ClaudeAdapter {
            keychain_fallback: true,
            ..ClaudeAdapter::with_config(path, String::new())
        }
    }

    #[tokio::test]
    async fn an_existing_file_is_read_even_with_the_keychain_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, VALID).unwrap();
        let adapter = with_fallback(path);
        assert_eq!(adapter.credential_source(), CredentialSource::File);
        let creds = adapter.load_credentials().await;
        assert!(
            matches!(&creds, Ok(c) if c.access_token.expose_secret() == "fake-token-for-test"),
            "the file's credentials are the ones loaded"
        );
    }

    /// Falsifier for the test above: the fallback does select the Keychain
    /// when the file is missing.
    #[test]
    fn a_missing_file_with_the_fallback_selects_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = with_fallback(dir.path().join("missing.json"));
        assert_eq!(adapter.credential_source(), CredentialSource::Keychain);
    }

    /// The source is decided per call: a file that appears later wins.
    #[test]
    fn a_file_that_appears_later_replaces_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let adapter = with_fallback(path.clone());
        assert_eq!(adapter.credential_source(), CredentialSource::Keychain);
        std::fs::write(&path, VALID).unwrap();
        assert_eq!(adapter.credential_source(), CredentialSource::File);
    }

    #[tokio::test]
    async fn with_config_never_falls_back_to_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = ClaudeAdapter::with_config(dir.path().join("missing.json"), String::new());
        assert_eq!(adapter.credential_source(), CredentialSource::File);
        assert!(matches!(
            adapter.load_credentials().await,
            Err(AuthError::NotLoggedIn(_))
        ));
    }

    #[test]
    fn an_explicit_credentials_path_disables_the_keychain_fallback() {
        assert!(!keychain_fallback_for(Some(std::path::Path::new(
            "/explicit/.credentials.json"
        ))));
        assert_eq!(keychain_fallback_for(None), cfg!(target_os = "macos"));
    }

    #[test]
    fn render_hint_flags_hard_block() {
        let adapter = ClaudeAdapter::with_config(PathBuf::from("/nonexistent"), String::new());
        let hint = adapter.render_hint(&parser::test_window());
        assert_eq!(hint.style, BarStyle::Percent);
        assert!((hint.warn_threshold - 0.8).abs() < f32::EPSILON);
        assert!((hint.critical_threshold - 0.95).abs() < f32::EPSILON);
        assert_eq!(hint.note.as_deref(), Some("Blocks entirely at cap"));
    }

    /// The ordering `group_order` used to declare, asserted where it is now
    /// the only one: the order `parse` emits windows in *is* the display
    /// order, because that is the order the daemon stores and the TUI draws.
    ///
    /// Kept rather than deleted with the method. The guarantee was real; what
    /// was wrong was expressing it through a second channel nothing read.
    #[test]
    fn windows_are_parsed_session_first_then_weekly_buckets() {
        let adapter = ClaudeAdapter::with_config(PathBuf::from("/nonexistent"), String::new());
        let windows = adapter
            .parse(&RawResponse {
                status: 200,
                headers: Vec::new(),
                body: br#"{"five_hour":{"utilization":10},
                           "seven_day":{"utilization":20},
                           "seven_day_opus":{"utilization":30},
                           "seven_day_sonnet":{"utilization":40}}"#
                    .to_vec(),
                fetched_at: chrono::Utc::now(),
            })
            .expect("parses");
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(
            ids,
            ["session_5h", "weekly", "weekly_opus", "weekly_sonnet"]
        );
    }
}
