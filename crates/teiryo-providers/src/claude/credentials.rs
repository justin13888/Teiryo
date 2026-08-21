//! Claude Code local credential discovery.
//!
//! On Linux, Claude Code stores its OAuth tokens in
//! `~/.claude/.credentials.json` under a `claudeAiOauth` key. On macOS the
//! tokens live in the Keychain, which this adapter does not read yet.

use std::path::{Path, PathBuf};

use secrecy::SecretString;
use serde::Deserialize;
use teiryo_core::AuthError;

/// Parsed Claude Code credentials (secret material wrapped in
/// [`SecretString`]; this struct intentionally has no `Debug`).
pub(crate) struct ClaudeCredentials {
    /// OAuth access token.
    pub access_token: SecretString,
    /// Access-token expiry, unix epoch milliseconds.
    pub expires_at_ms: i64,
}

impl ClaudeCredentials {
    /// Whether the access token is past its recorded expiry.
    pub fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp_millis() >= self.expires_at_ms
    }
}

/// On-disk layout of `~/.claude/.credentials.json`. Unknown fields ignored.
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthSection>,
}

#[derive(Deserialize)]
struct OauthSection {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt", default)]
    expires_at: Option<i64>,
}

/// Default credentials path: `~/.claude/.credentials.json` (Linux). On macOS
/// Claude Code uses the Keychain instead, which is not supported yet.
pub(crate) fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".claude").join(".credentials.json")
}

/// Load and parse the Claude Code credentials file.
pub(crate) fn load(path: &Path) -> Result<ClaudeCredentials, AuthError> {
    if cfg!(target_os = "macos") && !path.exists() {
        return Err(AuthError::Store(
            "Claude Code stores credentials in the macOS Keychain, which is not supported yet"
                .into(),
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AuthError::NotLoggedIn(format!(
                "no Claude Code credentials at {}; run `claude` and log in",
                path.display()
            ))
        } else {
            AuthError::Store(format!("reading {}: {e}", path.display()))
        }
    })?;
    let file: CredentialsFile = serde_json::from_slice(&bytes)
        .map_err(|e| AuthError::Store(format!("parsing {}: {e}", path.display())))?;
    let oauth = file.claude_ai_oauth.ok_or_else(|| {
        AuthError::NotLoggedIn("credentials file has no claudeAiOauth section".into())
    })?;
    let access_token = oauth
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AuthError::NotLoggedIn("credentials file has no access token".into()))?;
    Ok(ClaudeCredentials {
        access_token: SecretString::from(access_token),
        // Missing expiry: treat as already expired so the user is told to
        // refresh rather than us sending a stale token.
        expires_at_ms: oauth.expires_at.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn write_creds(dir: &tempfile::TempDir, contents: &str) -> PathBuf {
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// `unwrap_err` needs `Ok: Debug`, which `ClaudeCredentials` deliberately
    /// lacks — extract the error without formatting the credentials.
    fn expect_err(result: Result<ClaudeCredentials, AuthError>) -> AuthError {
        match result {
            Ok(_) => panic!("expected an error, got credentials"),
            Err(e) => e,
        }
    }

    #[test]
    fn loads_valid_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(
            &dir,
            r#"{"claudeAiOauth":{"accessToken":"fake-token-for-test","refreshToken":"fake-refresh","expiresAt":99999999999999,"scopes":["user:inference"],"subscriptionType":"max"}}"#,
        );
        let creds = load(&path).unwrap();
        assert_eq!(creds.access_token.expose_secret(), "fake-token-for-test");
        assert!(!creds.is_expired());
    }

    #[test]
    fn expired_timestamp_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(
            &dir,
            r#"{"claudeAiOauth":{"accessToken":"fake-token-for-test","expiresAt":1}}"#,
        );
        assert!(load(&path).unwrap().is_expired());
    }

    #[test]
    fn missing_file_is_not_logged_in() {
        let dir = tempfile::tempdir().unwrap();
        let err = expect_err(load(&dir.path().join("nope.json")));
        assert!(matches!(err, AuthError::NotLoggedIn(_)), "got {err:?}");
    }

    #[test]
    fn missing_oauth_section_is_not_logged_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(&dir, r#"{"somethingElse":{}}"#);
        let err = expect_err(load(&path));
        assert!(matches!(err, AuthError::NotLoggedIn(_)), "got {err:?}");
    }

    #[test]
    fn malformed_json_is_store_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(&dir, "not json");
        let err = expect_err(load(&path));
        assert!(matches!(err, AuthError::Store(_)), "got {err:?}");
    }
}
