//! Claude (subscription) adapter.
//!
//! Usage is exposed as *headroom percentages*, not raw counts: a rolling
//! 5-hour session window and rolling weekly windows (separate Opus/Sonnet
//! buckets on Max plans, plus per-model weekly caps such as Fable's that the
//! server reports only through its `limits[]` rows). Hitting a cap
//! hard-blocks new prompts.

mod credentials;
mod parser;
mod refresh;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use teiryo_core::{
    Account, AccountId, AuthError, BarStyle, Credential, ParseError, ProbeError, Prober,
    ProviderAdapter, ProviderId, QuotaParser, QuotaWindow, RawResponse, RenderHint, WindowId,
    WindowPresenter,
};
use tokio::sync::Mutex;

pub use parser::ASSUMED_SCHEMA;
pub use refresh::DEFAULT_TOKEN_URL;

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

/// Env var overriding the OAuth token endpoint (mainly for tests).
pub const ENV_TOKEN_URL: &str = "TEIRYO_CLAUDE_TOKEN_URL";

/// How long a refresh waits for another teiryod process to finish its own
/// refresh of the same credentials file before giving up for this attempt.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Claude subscription adapter: discovers the local Claude Code login,
/// renews it when its access token has expired, probes the OAuth usage
/// endpoint, and parses headroom percentages.
pub struct ClaudeAdapter {
    credentials_path: PathBuf,
    /// Read the macOS Keychain when `credentials_path` does not exist. Only
    /// for the default path: a path someone named explicitly is the one they
    /// meant, and silently reading a different store would hide a typo.
    keychain_fallback: bool,
    base_url: String,
    /// Where expired tokens are renewed.
    token_url: String,
    /// Serializes refreshes inside this process: two accounts, or a manual
    /// poll racing a scheduled one, must not both spend the same refresh
    /// token. Across processes the lock file does the same job.
    refresh_lock: Mutex<RefreshState>,
    /// One persistent HTTP client per account — separate accounts must not
    /// share a connection pool; reuse across probes keeps connection fidelity.
    clients: Mutex<HashMap<AccountId, reqwest::Client>>,
}

/// What the adapter remembers between refreshes.
#[derive(Default)]
struct RefreshState {
    /// The refresh token the OAuth server last refused. Asking again with it
    /// can never succeed, so it is not sent again; a new login replaces the
    /// token in the file and lifts this by no longer matching it.
    rejected: Option<SecretString>,
    /// The client token-endpoint requests go through, built on first use.
    client: Option<reqwest::Client>,
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
        let token_url =
            std::env::var(ENV_TOKEN_URL).unwrap_or_else(|_| DEFAULT_TOKEN_URL.to_owned());
        Self {
            keychain_fallback,
            ..Self::with_config(credentials_path, base_url).with_token_url(token_url)
        }
    }

    /// Adapter with explicit credentials path and API base URL. Reads that
    /// file only, never the Keychain.
    pub fn with_config(credentials_path: PathBuf, base_url: String) -> Self {
        Self {
            credentials_path,
            keychain_fallback: false,
            base_url,
            token_url: DEFAULT_TOKEN_URL.to_owned(),
            refresh_lock: Mutex::new(RefreshState::default()),
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The same adapter, renewing expired tokens at `token_url` instead.
    #[must_use]
    pub fn with_token_url(mut self, token_url: String) -> Self {
        self.token_url = token_url;
        self
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

    async fn refresh_credential(
        &self,
        _account: &Account,
        rejected: Option<&Credential>,
    ) -> Result<Credential, AuthError> {
        if self.credential_source() == CredentialSource::Keychain {
            return Err(AuthError::Expired(
                "Claude Code access token is past its expiry and teiryo renews only a \
                 credentials file, not the macOS Keychain; run `claude` to refresh"
                    .into(),
            ));
        }
        let rejected = match rejected {
            Some(Credential::OAuthToken(token)) => Some(token),
            _ => None,
        };
        let mut state = self.refresh_lock.lock().await;
        let _flock = FileLock::acquire(&lock_path(&self.credentials_path)).await?;

        // Read under the lock: Claude Code, or another teiryod, may have
        // renewed the login since the caller's read. Using that is free;
        // rotating again would be a second spend of a token for nothing.
        let current = credentials::load(&self.credentials_path)?;
        let was_rejected =
            rejected.is_some_and(|r| r.expose_secret() == current.access_token.expose_secret());
        if !current.is_expired() && !was_rejected {
            return Ok(Credential::OAuthToken(current.access_token));
        }
        let Some(refresh_token) = current.refresh_token else {
            return Err(AuthError::Expired(
                "Claude Code access token is past its expiry and the credentials file holds \
                 no refresh token; run `claude` to log in again"
                    .into(),
            ));
        };
        if state
            .rejected
            .as_ref()
            .is_some_and(|r| r.expose_secret() == refresh_token.expose_secret())
        {
            return Err(AuthError::Expired(
                "the Claude login could not be renewed earlier and has not changed since; \
                 run `claude` to log in again"
                    .into(),
            ));
        }
        let client = match &state.client {
            Some(client) => client.clone(),
            None => {
                let client = reqwest::Client::builder()
                    .user_agent(USER_AGENT)
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|e| AuthError::Store(format!("building http client: {e}")))?;
                state.client = Some(client.clone());
                client
            }
        };
        let renewed = match refresh::exchange(&client, &self.token_url, &refresh_token).await {
            Ok(renewed) => renewed,
            Err(refresh::Failure::Rejected(message)) => {
                state.rejected = Some(refresh_token);
                return Err(AuthError::Expired(message));
            }
            Err(refresh::Failure::Transient(message)) => return Err(AuthError::Store(message)),
        };
        match credentials::store_renewed(&self.credentials_path, &renewed, &refresh_token)? {
            credentials::Stored::Written => tracing::info!(
                path = %self.credentials_path.display(),
                expires_in_secs = (renewed.expires_at_ms - chrono::Utc::now().timestamp_millis()) / 1000,
                "renewed the Claude access token and wrote it back"
            ),
            // The token we hold is valid either way; only the file's pair is
            // Claude Code's to keep.
            credentials::Stored::Superseded => tracing::warn!(
                path = %self.credentials_path.display(),
                "the Claude login was renewed elsewhere while teiryo renewed it; kept the file's"
            ),
        }
        Ok(Credential::OAuthToken(renewed.access_token))
    }
}

/// The lock file serializing refreshes of `credentials` across processes —
/// a development daemon and the installed one may share one login.
fn lock_path(credentials: &std::path::Path) -> PathBuf {
    let mut name = credentials
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".teiryo.lock");
    credentials.with_file_name(name)
}

/// An exclusive `flock`, released when dropped.
struct FileLock {
    _file: std::fs::File,
}

impl FileLock {
    /// Take the lock at `path`, polling rather than blocking: the daemon runs
    /// on one thread, and a blocking `flock` would stall every client.
    async fn acquire(path: &std::path::Path) -> Result<Self, AuthError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|e| AuthError::Store(format!("opening {}: {e}", path.display())))?;
        let deadline = tokio::time::Instant::now() + REFRESH_LOCK_WAIT;
        loop {
            // SAFETY: `file` owns a valid descriptor for the whole call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { _file: file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                return Err(AuthError::Store(format!(
                    "locking {}: {err}",
                    path.display()
                )));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AuthError::Store(format!(
                    "another teiryod has been renewing the Claude login for over {}s",
                    REFRESH_LOCK_WAIT.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
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

/// Token refresh, end to end against a stand-in token endpoint: what is sent,
/// what is written back, and when nothing is sent at all.
#[cfg(test)]
mod refresh_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use teiryo_core::Authenticator;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// An access token that expired long ago, a refresh token, and fields
    /// teiryo does not know about, inside the section and beside it.
    const EXPIRED: &str = r#"{"claudeAiOauth":{"accessToken":"old-access","refreshToken":"old-refresh","expiresAt":1,"scopes":["user:inference"],"subscriptionType":"max","refreshTokenExpiresAt":4102444800000},"other":{"keep":true}}"#;

    const RENEWED: &str =
        r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":28800}"#;

    fn account() -> Account {
        Account {
            id: AccountId::from("claude:default"),
            provider: PROVIDER_ID.into(),
            label: "default".into(),
        }
    }

    fn token(cred: &Credential) -> &str {
        match cred {
            Credential::OAuthToken(t) => t.expose_secret(),
            other => panic!("expected an OAuth token, got {other:?}"),
        }
    }

    fn expect_err(result: Result<Credential, AuthError>) -> AuthError {
        match result {
            Ok(cred) => panic!("expected an error, got {cred:?}"),
            Err(e) => e,
        }
    }

    /// A one-route HTTP server answering every request with `status` and
    /// `body`, after running `before_reply` (which may tamper with the file).
    /// Returns the URL, a request counter, and the last request body seen.
    async fn endpoint(
        status: u16,
        body: &'static str,
        before_reply: impl Fn() + Send + Sync + 'static,
    ) -> (String, Arc<AtomicUsize>, Arc<std::sync::Mutex<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/oauth/token", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let (h, s) = (hits.clone(), seen.clone());
        let before_reply = Arc::new(before_reply);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                h.fetch_add(1, Ordering::SeqCst);
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the head, then exactly Content-Length more bytes.
                let request = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break text[end + 4..end + 4 + len].to_owned();
                        }
                    }
                    if n == 0 {
                        break String::new();
                    }
                };
                *s.lock().unwrap() = request;
                before_reply();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(reply.as_bytes()).await.unwrap();
                let _ = sock.shutdown().await;
            }
        });
        (url, hits, seen)
    }

    fn adapter_for(path: &std::path::Path, token_url: String) -> ClaudeAdapter {
        ClaudeAdapter::with_config(path.to_path_buf(), String::new()).with_token_url(token_url)
    }

    fn written(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn an_expired_token_is_renewed_and_written_back_with_every_other_field_kept() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, EXPIRED).unwrap();
        let inode_before = std::fs::metadata(&path).unwrap().ino();
        let (url, hits, seen) = endpoint(200, RENEWED, || {}).await;
        let adapter = adapter_for(&path, url);

        let cred = adapter.refresh_credential(&account(), None).await.unwrap();

        assert_eq!(token(&cred), "new-access");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let sent: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()).unwrap();
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["refresh_token"], "old-refresh");
        assert_eq!(sent["client_id"], refresh::CLIENT_ID);

        let doc = written(&path);
        let oauth = &doc["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], "new-access");
        assert_eq!(oauth["refreshToken"], "new-refresh");
        let expires = oauth["expiresAt"].as_i64().unwrap();
        let in_secs = (expires - chrono::Utc::now().timestamp_millis()) / 1000;
        assert!(
            (28_700..=28_800).contains(&in_secs),
            "expires in {in_secs}s"
        );
        assert_eq!(oauth["scopes"], serde_json::json!(["user:inference"]));
        assert_eq!(oauth["subscriptionType"], "max");
        assert_eq!(oauth["refreshTokenExpiresAt"], 4_102_444_800_000_i64);
        assert_eq!(doc["other"]["keep"], true);

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_ne!(
            meta.ino(),
            inode_before,
            "replaced by rename, not rewritten in place"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");

        // And the cheap path now resolves it without the network.
        let cred = adapter.credential_for(&account()).await.unwrap();
        assert_eq!(token(&cred), "new-access");
    }

    /// A server that does not rotate the refresh token keeps the old one.
    #[tokio::test]
    async fn a_reply_without_a_refresh_token_keeps_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, EXPIRED).unwrap();
        let (url, _, _) = endpoint(
            200,
            r#"{"access_token":"new-access","expires_in":60}"#,
            || {},
        )
        .await;
        adapter_for(&path, url)
            .refresh_credential(&account(), None)
            .await
            .unwrap();
        assert_eq!(
            written(&path)["claudeAiOauth"]["refreshToken"],
            "old-refresh"
        );
    }

    /// A login that became usable since the caller looked — Claude Code ran —
    /// is handed back as is: refreshing it would spend a token for nothing.
    #[tokio::test]
    async fn a_usable_stored_token_is_returned_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"fresh","refreshToken":"r","expiresAt":99999999999999}}"#,
        )
        .unwrap();
        let (url, hits, _) = endpoint(200, RENEWED, || {}).await;
        let cred = adapter_for(&path, url)
            .refresh_credential(&account(), None)
            .await
            .unwrap();
        assert_eq!(token(&cred), "fresh");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// Falsifier for the test above: an unexpired token the provider just
    /// refused is not handed back again; it is renewed.
    #[tokio::test]
    async fn a_token_the_provider_rejected_is_renewed_even_before_its_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"refused","refreshToken":"old-refresh","expiresAt":99999999999999}}"#,
        )
        .unwrap();
        let (url, hits, _) = endpoint(200, RENEWED, || {}).await;
        let refused = Credential::OAuthToken(SecretString::from("refused"));
        let cred = adapter_for(&path, url)
            .refresh_credential(&account(), Some(&refused))
            .await
            .unwrap();
        assert_eq!(token(&cred), "new-access");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// A refused refresh token is asked about once: every later attempt with
    /// the same token fails without a request, until the login changes.
    #[tokio::test]
    async fn a_rejected_refresh_token_is_not_sent_again_until_the_login_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, EXPIRED).unwrap();
        let (url, hits, _) = endpoint(400, r#"{"error":"invalid_grant"}"#, || {}).await;
        let adapter = adapter_for(&path, url);

        let first = expect_err(adapter.refresh_credential(&account(), None).await);
        let AuthError::Expired(message) = first else {
            panic!("a refused refresh token is an expired login, got {first:?}");
        };
        assert!(message.contains("invalid_grant"), "{message}");
        assert!(message.contains("run `claude`"), "{message}");
        let again = expect_err(adapter.refresh_credential(&account(), None).await);
        assert!(matches!(again, AuthError::Expired(_)), "got {again:?}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the dead token was sent again"
        );
        assert_eq!(written(&path)["claudeAiOauth"]["accessToken"], "old-access");

        // A re-login writes a different refresh token, which is tried.
        std::fs::write(&path, EXPIRED.replace("old-refresh", "relogin-refresh")).unwrap();
        let _ = adapter.refresh_credential(&account(), None).await;
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// An outage is not a verdict on the token: a store error, retried later.
    #[tokio::test]
    async fn a_server_error_is_transient_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, EXPIRED).unwrap();
        let (url, hits, _) = endpoint(503, "{}", || {}).await;
        let adapter = adapter_for(&path, url);
        let err = expect_err(adapter.refresh_credential(&account(), None).await);
        assert!(matches!(err, AuthError::Store(_)), "got {err:?}");
        let _ = adapter.refresh_credential(&account(), None).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "a transient failure is retried"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EXPIRED);
    }

    /// Claude Code renewing the login while teiryo's own renewal is in flight
    /// wins: its pair stays in the file, and teiryo still polls with the token
    /// it was issued.
    #[tokio::test]
    async fn a_login_renewed_elsewhere_meanwhile_is_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, EXPIRED).unwrap();
        let theirs = EXPIRED
            .replace("old-refresh", "claude-code-refresh")
            .replace("old-access", "claude-code-access");
        let (p, t) = (path.clone(), theirs.clone());
        let (url, _, _) = endpoint(200, RENEWED, move || std::fs::write(&p, &t).unwrap()).await;
        let cred = adapter_for(&path, url)
            .refresh_credential(&account(), None)
            .await
            .unwrap();
        assert_eq!(token(&cred), "new-access");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
    }

    #[tokio::test]
    async fn a_file_without_a_refresh_token_is_expired_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":1}}"#,
        )
        .unwrap();
        let (url, hits, _) = endpoint(200, RENEWED, || {}).await;
        let err = expect_err(
            adapter_for(&path, url)
                .refresh_credential(&account(), None)
                .await,
        );
        assert!(matches!(err, AuthError::Expired(_)), "got {err:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// The Keychain is read, never written: a Keychain login is not renewed.
    #[tokio::test]
    async fn a_keychain_login_is_not_renewed() {
        let dir = tempfile::tempdir().unwrap();
        let (url, hits, _) = endpoint(200, RENEWED, || {}).await;
        let adapter = ClaudeAdapter {
            keychain_fallback: true,
            ..adapter_for(&dir.path().join("missing.json"), url)
        };
        let err = expect_err(adapter.refresh_credential(&account(), None).await);
        let AuthError::Expired(message) = err else {
            panic!("got {err:?}");
        };
        assert!(message.contains("Keychain"), "{message}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_lock_file_sits_beside_the_credentials_it_guards() {
        assert_eq!(
            lock_path(std::path::Path::new("/h/.claude/.credentials.json")),
            PathBuf::from("/h/.claude/.credentials.json.teiryo.lock")
        );
    }
}
