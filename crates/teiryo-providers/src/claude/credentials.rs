//! Claude Code local credential discovery.
//!
//! Claude Code keeps its OAuth tokens in one of two places, as the same
//! `{"claudeAiOauth": {...}}` JSON either way:
//!
//! - `~/.claude/.credentials.json` — on Linux, and on macOS when that file
//!   exists;
//! - the macOS login Keychain, generic password `Claude Code-credentials` —
//!   where Claude Code keeps them on macOS by default.
//!
//! The Keychain is read through `/usr/bin/security` rather than the Security
//! framework. Claude Code writes the item with that same tool, which leaves it
//! on the item's access list, so the read is prompt-free; a read made by
//! `teiryod` itself would not be on that list and would raise an access
//! prompt, which `Authenticator::credential_for` forbids.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
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

/// Layout of the credentials JSON, from the file or the Keychain. Unknown
/// fields ignored.
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

/// Default credentials path: `~/.claude/.credentials.json`.
pub(crate) fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".claude").join(".credentials.json")
}

/// Load and parse the Claude Code credentials file.
pub(crate) fn load(path: &Path) -> Result<ClaudeCredentials, AuthError> {
    let bytes = std::fs::read(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            AuthError::NotLoggedIn(format!(
                "no Claude Code credentials at {}; run `claude` and log in",
                path.display()
            ))
        } else {
            AuthError::Store(format!("reading {}: {e}", path.display()))
        }
    })?;
    parse(&bytes, &path.display().to_string())
}

/// Parse the credentials JSON; `origin` names where it came from in errors.
fn parse(bytes: &[u8], origin: &str) -> Result<ClaudeCredentials, AuthError> {
    let file: CredentialsFile = serde_json::from_slice(bytes)
        .map_err(|e| AuthError::Store(format!("parsing {origin}: {e}")))?;
    let oauth = file
        .claude_ai_oauth
        .ok_or_else(|| AuthError::NotLoggedIn(format!("{origin} has no claudeAiOauth section")))?;
    let access_token = oauth
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AuthError::NotLoggedIn(format!("{origin} has no access token")))?;
    Ok(ClaudeCredentials {
        access_token: SecretString::from(access_token),
        // Missing expiry: treat as already expired so the user is told to
        // refresh rather than us sending a stale token.
        expires_at_ms: oauth.expires_at.unwrap_or(0),
    })
}

/// The generic-password service Claude Code stores its login under.
pub(crate) const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// The tool that reads it, by absolute path, so nothing on `$PATH` (a shim, a
/// same-named script) is ever handed the secret.
const SECURITY: &str = "/usr/bin/security";

/// How long a Keychain read may take. One still running by then is almost
/// always `security` sitting behind an access prompt nobody is there to
/// answer; the daemon polls unattended, so it gives up rather than wait.
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// `security`'s exit status for `errSecItemNotFound`.
const ITEM_NOT_FOUND: i32 = 44;

/// What one run of the Keychain reader produced. The secret lives only inside
/// [`SecretString`], and this type deliberately has no `Debug`.
pub(crate) enum KeychainRead {
    /// Exit 0; stdout is the stored password.
    Found(SecretString),
    /// Non-zero exit (`None` when killed by a signal) and its stderr, which
    /// never carries the password.
    Exited { code: Option<i32>, stderr: String },
    /// Still running at the deadline; it was killed.
    TimedOut,
    /// Could not be started, or its output could not be collected.
    Spawn(io::Error),
}

/// Read Claude Code's credentials from the macOS Keychain.
pub(crate) async fn load_keychain() -> Result<ClaudeCredentials, AuthError> {
    let read = run_reader(
        Path::new(SECURITY),
        &["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"],
        KEYCHAIN_TIMEOUT,
    )
    .await;
    interpret(read)
}

/// Run `program` with `args` and classify the outcome: everything about a
/// Keychain read except the Keychain, which is what the tests drive with a
/// stand-in program.
pub(crate) async fn run_reader(program: &Path, args: &[&str], timeout: Duration) -> KeychainRead {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Abandoning the wait at the deadline must not leave `security`
        // behind, still holding its prompt open.
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => return KeychainRead::Spawn(e),
    };
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return KeychainRead::Spawn(e),
        Err(_) => return KeychainRead::TimedOut,
    };
    if output.status.success() {
        // Moved, not copied, into the secret. Invalid UTF-8 is dropped here
        // and reported without its contents.
        return match String::from_utf8(output.stdout) {
            Ok(text) => KeychainRead::Found(SecretString::from(text)),
            Err(_) => KeychainRead::Exited {
                code: Some(0),
                stderr: "the stored password is not UTF-8".to_owned(),
            },
        };
    }
    KeychainRead::Exited {
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

/// Map one reader outcome to credentials, or to the error the daemon reports.
pub(crate) fn interpret(read: KeychainRead) -> Result<ClaudeCredentials, AuthError> {
    let origin = format!("the Keychain item \"{KEYCHAIN_SERVICE}\"");
    match read {
        KeychainRead::Found(secret) => parse(secret.expose_secret().trim().as_bytes(), &origin),
        KeychainRead::Exited {
            code: Some(ITEM_NOT_FOUND),
            ..
        } => Err(AuthError::NotLoggedIn(format!(
            "no Claude Code login in the macOS Keychain ({origin}); run `claude` and log in"
        ))),
        KeychainRead::Exited { code, stderr } => {
            let status = code.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}"));
            let detail = stderr.lines().next().unwrap_or("").trim();
            Err(AuthError::Store(if detail.is_empty() {
                format!("reading {origin}: {SECURITY} exited with {status}")
            } else {
                format!("reading {origin}: {SECURITY} exited with {status}: {detail}")
            }))
        }
        KeychainRead::TimedOut => Err(AuthError::Store(format!(
            "reading {origin} timed out after {}s; macOS is probably waiting for you \
             to allow {SECURITY} to access it",
            KEYCHAIN_TIMEOUT.as_secs()
        ))),
        KeychainRead::Spawn(e) => Err(AuthError::Store(format!("cannot run {SECURITY}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    const VALID: &str = r#"{"claudeAiOauth":{"accessToken":"fake-token-for-test","refreshToken":"fake-refresh","expiresAt":99999999999999,"scopes":["user:inference"],"subscriptionType":"max"}}"#;

    #[test]
    fn loads_valid_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(&dir, VALID);
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

    /// On every platform, macOS included: a missing file is a missing login,
    /// not a refusal to look. Whether to try the Keychain instead is the
    /// adapter's decision, not this function's.
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

    /// `security -w` ends the password with a newline; the JSON inside is the
    /// same document the file holds.
    #[test]
    fn keychain_password_parses_like_the_file() {
        let read = KeychainRead::Found(SecretString::from(format!("{VALID}\n")));
        let creds = interpret(read).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(creds.access_token.expose_secret(), "fake-token-for-test");
        assert!(!creds.is_expired());
    }

    #[test]
    fn missing_keychain_item_is_not_logged_in() {
        let err = expect_err(interpret(KeychainRead::Exited {
            code: Some(ITEM_NOT_FOUND),
            stderr: "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.".into(),
        }));
        let AuthError::NotLoggedIn(message) = err else {
            panic!("item-not-found is a missing login, got {err:?}");
        };
        assert!(message.contains("run `claude` and log in"), "{message}");
    }

    /// The falsifier for the test above: only status 44 means "no login", so
    /// any other failure is a store error that keeps `security`'s own reason.
    #[test]
    fn other_security_failures_are_store_errors_with_the_reason() {
        let err = expect_err(interpret(KeychainRead::Exited {
            code: Some(51),
            stderr: "security: user interaction is not allowed.\n".into(),
        }));
        let AuthError::Store(message) = err else {
            panic!("got {err:?}");
        };
        assert!(message.contains("status 51"), "{message}");
        assert!(
            message.contains("user interaction is not allowed"),
            "{message}"
        );
    }

    #[test]
    fn a_timed_out_read_names_the_prompt() {
        let err = expect_err(interpret(KeychainRead::TimedOut));
        let AuthError::Store(message) = err else {
            panic!("got {err:?}");
        };
        assert!(message.contains("timed out"), "{message}");
        assert!(message.contains("allow"), "{message}");
    }

    #[test]
    fn a_keychain_item_without_a_token_is_not_logged_in() {
        let read = KeychainRead::Found(SecretString::from(r#"{"claudeAiOauth":{}}"#.to_owned()));
        let err = expect_err(interpret(read));
        assert!(matches!(err, AuthError::NotLoggedIn(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn reader_captures_stdout_of_a_successful_run() {
        let read = run_reader(
            Path::new("/bin/sh"),
            &["-c", "printf '%s\\n' 'hello'"],
            Duration::from_secs(5),
        )
        .await;
        let KeychainRead::Found(secret) = read else {
            panic!("a zero exit is a found item");
        };
        assert_eq!(secret.expose_secret(), "hello\n");
    }

    #[tokio::test]
    async fn reader_reports_exit_status_and_stderr() {
        let read = run_reader(
            Path::new("/bin/sh"),
            &["-c", "echo 'no such item' >&2; exit 44"],
            Duration::from_secs(5),
        )
        .await;
        let KeychainRead::Exited { code, stderr } = read else {
            panic!("a non-zero exit is reported as such");
        };
        assert_eq!(code, Some(44));
        assert_eq!(stderr, "no such item");
    }

    /// A reader still running at the deadline is abandoned, and killed with
    /// it: the timeout must come back long before the child would have ended.
    #[tokio::test]
    async fn reader_times_out_and_returns_promptly() {
        let started = std::time::Instant::now();
        let read = run_reader(
            Path::new("/bin/sh"),
            &["-c", "sleep 30"],
            Duration::from_millis(200),
        )
        .await;
        assert!(matches!(read, KeychainRead::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The real Keychain, on a Mac with Claude Code logged in. Ignored because
    /// CI has neither; run it by hand with `--ignored --nocapture`. Prints the
    /// expiry only, never anything read from the item.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "reads the real login Keychain"]
    async fn reads_the_real_keychain() {
        match load_keychain().await {
            Ok(creds) => {
                let minutes =
                    (creds.expires_at_ms - chrono::Utc::now().timestamp_millis()) / 60_000;
                println!("ok, expires in {minutes} min");
            }
            Err(e) => panic!("{e}"),
        }
    }

    #[tokio::test]
    async fn reader_reports_a_missing_program() {
        let read = run_reader(
            Path::new("/nonexistent/security"),
            &[],
            Duration::from_secs(5),
        )
        .await;
        let KeychainRead::Spawn(e) = read else {
            panic!("an unstartable program is a spawn failure");
        };
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }
}
