//! OAuth refresh-token exchange against the endpoint Claude Code itself uses.
//!
//! Claude Code renews its own access token only while it runs. A daemon that
//! polls overnight outlives that token, so it renews it the same way: POST the
//! refresh token, receive a new access token and (usually) a rotated refresh
//! token. Rotation is why the result must be written back into Claude Code's
//! store — the token spent here no longer works for Claude Code.

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use super::credentials::Renewed;

/// Claude Code's OAuth token endpoint.
pub const DEFAULT_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Claude Code's public OAuth client id.
pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Why an exchange produced no token pair.
pub(crate) enum Failure {
    /// The server refused the refresh token itself: it is spent, revoked, or
    /// expired, and asking again with it can never succeed.
    Rejected(String),
    /// Anything else — the network, a 5xx, a rate limit, a body we could not
    /// read. Worth retrying later with the same token.
    Transient(String),
}

/// The token endpoint's success body. Secrets deserialize straight into
/// [`SecretString`]; deliberately no `Debug`.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: SecretString,
    #[serde(default)]
    refresh_token: Option<SecretString>,
    /// Lifetime of the access token, seconds.
    #[serde(default)]
    expires_in: Option<i64>,
}

/// The token endpoint's error body (RFC 6749 §5.2). Carries no secret.
#[derive(Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: Option<String>,
}

/// Lifetime assumed when the server omits `expires_in`: short enough that an
/// unknown lifetime is renewed early rather than sent stale.
const ASSUMED_LIFETIME_SECS: i64 = 3600;

/// Exchange `refresh_token` for a new token pair at `token_url`.
pub(crate) async fn exchange(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &SecretString,
) -> Result<Renewed, Failure> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token.expose_secret(),
        "client_id": CLIENT_ID,
    });
    let response = client
        .post(token_url)
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| Failure::Transient(format!("token refresh request failed: {e}")))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| Failure::Transient(format!("reading the token refresh reply: {e}")))?;
    if status.is_success() {
        let parsed: TokenResponse = serde_json::from_slice(&bytes)
            .map_err(|e| Failure::Transient(format!("parsing the token refresh reply: {e}")))?;
        let lifetime = parsed.expires_in.unwrap_or(ASSUMED_LIFETIME_SECS).max(0);
        return Ok(Renewed {
            access_token: parsed.access_token,
            refresh_token: parsed.refresh_token,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + lifetime * 1000,
        });
    }
    let code = serde_json::from_slice::<ErrorResponse>(&bytes)
        .ok()
        .and_then(|e| e.error);
    let detail = code
        .as_deref()
        .map_or_else(String::new, |c| format!(" ({c})"));
    // A 403 with no OAuth error body is a proxy or firewall talking, not the
    // authorization server: it says nothing about the token.
    let oauth_refusal = matches!(status.as_u16(), 400 | 401) || code.is_some();
    match status.as_u16() {
        // `invalid_grant` and friends: the refresh token is no good. A 400 for
        // any other reason is still not something a retry fixes.
        400 | 401 | 403 if oauth_refusal => Err(Failure::Rejected(format!(
            "the Claude login could not be renewed: token endpoint returned {status}{detail}; \
             run `claude` to log in again"
        ))),
        _ => Err(Failure::Transient(format!(
            "token endpoint returned {status}{detail}"
        ))),
    }
}
