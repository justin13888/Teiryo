//! Typed protocol client: handshake, framed request/response, and the
//! `AwaitUpdate` long-poll loop.
//!
//! The TUI uses two connections: one for interactive requests (never blocked
//! longer than a normal round-trip) and one dedicated to the long-poll loop,
//! which may legitimately sit idle for `AWAIT_TIMEOUT_MS` at a time.
//!
//! Each connection reaches the daemon one of two ways ([`Target`]): this
//! host's socket, or an ssh child running `teiryod bridge` on another host.
//! The handshake and the frames are the same over both.

use std::future::Future;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use teiryo_core::protocol::codec::{decode_frame, encode_frame, framed};
use teiryo_core::protocol::handshake::{client_handshake, PROTOCOL_VERSION};
use teiryo_core::protocol::ssh::{lacks_bridge, SshTarget};
use teiryo_core::protocol::transport::{BoxedTransport, Transport};
use teiryo_core::{
    AccountStatus, ConfigState, HandshakeError, PollEvent, PollId, Request, Response, WireError,
};

use crate::spawn::connect_or_spawn;

/// Where the daemon is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// This host's daemon, spawned on demand.
    Local,
    /// A daemon on another host, reached by running `teiryod bridge` there
    /// over ssh, once per connection.
    Ssh(SshTarget),
}

impl Target {
    /// The remote host, for the header; `None` for the local daemon.
    pub fn host(&self) -> Option<&str> {
        match self {
            Target::Local => None,
            Target::Ssh(ssh) => Some(&ssh.host),
        }
    }
}

/// How long an ssh that ended the stream before the handshake completed has
/// to exit and close its stderr, before its failure is reported as the
/// handshake's own error instead of ssh's last word.
const SSH_EXIT_WAIT: Duration = Duration::from_secs(2);

/// How long the daemon may hold one `AwaitUpdate` open.
pub const AWAIT_TIMEOUT_MS: u32 = 25_000;

/// How long a daemon that accepted the connection has to answer the
/// handshake. One that bound its socket but never reached its accept loop
/// would otherwise hang the TUI before the terminal is even set up.
///
/// The connect succeeds as soon as the daemon has bound, but it answers only
/// once it reaches its accept loop, after opening the database and running
/// account discovery. Discovery may read the macOS Keychain through
/// `/usr/bin/security` under a 5 s bound, which a pending access prompt runs
/// out, so a freshly spawned daemon can legitimately take that long before
/// answering. This bound sits well above that worst case: a slow
/// first launch waits instead of failing.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// First wait before a reconnect attempt; it doubles on each failure.
const RECONNECT_BASE: Duration = Duration::from_secs(1);
/// Ceiling on the wait between reconnect attempts.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Client-side connection/protocol failures.
#[derive(Debug)]
pub enum ClientError {
    /// Socket-level failure (connect, read, write).
    Io(std::io::Error),
    /// The daemon could not be started: its binary is missing, or it was
    /// spawned and died before binding. Carries a message that already names
    /// the log file, so it reads as a complete sentence to the user.
    DaemonStart(String),
    /// Handshake failed; `VersionMismatch`-shaped rejections mean a stale
    /// daemon from a previous install is still running.
    Handshake(HandshakeError),
    /// Frame (de)serialization failed.
    Wire(WireError),
    /// The daemon accepted the connection but never answered the handshake.
    HandshakeTimeout(Duration),
    /// ssh was still running when the handshake ran out of time, so nothing
    /// shows the remote daemon was ever reached: the network, ssh, the
    /// bridge, or the remote daemon may be the one not answering.
    SshTimeout {
        /// The `--ssh` host.
        host: String,
        /// The handshake's limit.
        limit: Duration,
    },
    /// The remote `teiryod` has no `bridge` command: it predates `--ssh`,
    /// and only installing a matching one there fixes it.
    NoBridge {
        /// The `--ssh` host.
        host: String,
    },
    /// ssh could not be run, or ended before the handshake completed: the
    /// host, and ssh's (or the remote bridge's) last word on why.
    Ssh {
        /// The `--ssh` host.
        host: String,
        /// The reason, usually ssh's last stderr line.
        why: String,
    },
    /// The daemon closed the connection.
    Closed,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "socket error: {e}"),
            ClientError::DaemonStart(message) => f.write_str(message),
            ClientError::Handshake(e) => write!(f, "handshake failed: {e}"),
            ClientError::Wire(e) => write!(f, "wire error: {e}"),
            ClientError::HandshakeTimeout(limit) => write!(
                f,
                "the daemon accepted the connection but did not answer the handshake within {}s",
                limit.as_secs()
            ),
            ClientError::Ssh { host, why } => {
                write!(f, "cannot reach the daemon on {host} over ssh: {why}")
            }
            ClientError::SshTimeout { host, limit } => write!(
                f,
                "no handshake reply from {host} over ssh within {}s: ssh is still running, \
                 so the network, ssh, the remote bridge, or the remote daemon is not answering",
                limit.as_secs()
            ),
            ClientError::NoBridge { host } => write!(
                f,
                "the teiryod on {host} has no `bridge` command, so it is older than this \
                 client — install the matching teiryod there and relaunch teiryo"
            ),
            ClientError::Closed => f.write_str("daemon closed the connection"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e)
    }
}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> Self {
        ClientError::Wire(e)
    }
}

/// The daemon's protocol version when a handshake failure is the "stale
/// daemon of a different protocol version" case that the user must resolve
/// by letting the daemon restart: `Some(Some(v))` names it, `Some(None)` is a
/// daemon older than v8, which does not say. `None` for any other failure.
pub fn version_mismatch(err: &ClientError) -> Option<Option<u16>> {
    match err {
        ClientError::Handshake(HandshakeError::Rejected { daemon }) => Some(*daemon),
        ClientError::Handshake(HandshakeError::VersionMismatch { theirs, .. }) => {
            Some(Some(*theirs))
        }
        _ => None,
    }
}

/// How a mismatched daemon's version reads to the user: `v9`, or what is
/// known about a daemon that did not say.
pub fn daemon_version_label(daemon: Option<u16>) -> String {
    match daemon {
        Some(v) => format!("v{v}"),
        None => "older than v8".to_owned(),
    }
}

/// What the user is told once a reconnect finds a daemon of another protocol
/// version. Retrying cannot fix it — the stale daemon keeps the socket — so it
/// stays on screen until the user acts. A remote one may also be a remote
/// install of another version, which only reinstalling there fixes.
pub fn version_mismatch_message(daemon: Option<u16>, target: &Target) -> String {
    let daemon = daemon_version_label(daemon);
    match target.host() {
        None => format!(
            "daemon speaks protocol {daemon}, this client v{PROTOCOL_VERSION} — \
             stop it (`pkill teiryod`) and relaunch teiryo"
        ),
        Some(host) => format!(
            "daemon on {host} speaks protocol {daemon}, this client v{PROTOCOL_VERSION} — \
             install the matching teiryod there, or stop its stale daemon \
             (`ssh {host} pkill teiryod`), and relaunch teiryo"
        ),
    }
}

/// What the user is told when `err` is one no reconnect can fix: a daemon
/// of another protocol version, or a remote install too old to bridge.
/// `None` for a failure worth retrying.
fn incompatible_message(err: &ClientError, target: &Target) -> Option<String> {
    match err {
        ClientError::NoBridge { .. } => Some(err.to_string()),
        _ => version_mismatch(err).map(|daemon| version_mismatch_message(daemon, target)),
    }
}

/// The commit this client was built from, as `-V` prints it.
pub const CLIENT_COMMIT: &str = env!("TEIRYO_COMMIT_HASH");

/// One handshaken, framed connection to the daemon.
pub struct Client {
    framed: Framed<BoxedTransport, LengthDelimitedCodec>,
    /// The commit the daemon said it was built from.
    daemon_commit: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    /// Connect to `target` (spawning a local daemon if needed, or running ssh
    /// for a remote one) and complete the handshake.
    pub async fn connect(target: &Target) -> Result<Self, ClientError> {
        match target {
            Target::Local => {
                let stream = connect_or_spawn().await?;
                Self::handshake(stream, HANDSHAKE_TIMEOUT).await
            }
            Target::Ssh(ssh) => Self::connect_ssh(ssh, HANDSHAKE_TIMEOUT).await,
        }
    }

    /// Run ssh to `ssh`'s host and complete the handshake over its stdio,
    /// giving up after `limit`. The remote daemon judges the protocol version
    /// as it would a local client's, so its reject comes back as is. Any other
    /// failure while ssh has exited is reported in ssh's own words: a refused
    /// key or an unknown host reaches the user as more than "end of stream".
    async fn connect_ssh(ssh: &SshTarget, limit: Duration) -> Result<Self, ClientError> {
        let failed = |why: String| ClientError::Ssh {
            host: ssh.host.clone(),
            why,
        };
        let mut stream = ssh
            .connect()
            .map_err(|e| failed(format!("cannot run {}: {e}", ssh.program.to_string_lossy())))?;
        match exchange(&mut stream, limit).await {
            Ok(daemon_commit) => Ok(Self {
                framed: framed(Box::new(stream)),
                daemon_commit,
            }),
            Err(e) if version_mismatch(&e).is_some() => Err(e),
            Err(e) => match stream.failure(SSH_EXIT_WAIT).await {
                Some(why) if lacks_bridge(&why) => Err(ClientError::NoBridge {
                    host: ssh.host.clone(),
                }),
                Some(why) => Err(failed(why)),
                None => match e {
                    ClientError::HandshakeTimeout(limit) => Err(ClientError::SshTimeout {
                        host: ssh.host.clone(),
                        limit,
                    }),
                    e => Err(e),
                },
            },
        }
    }

    /// Complete the handshake on a connected stream, giving up after `limit`.
    async fn handshake<S: Transport + 'static>(
        mut stream: S,
        limit: Duration,
    ) -> Result<Self, ClientError> {
        let daemon_commit = exchange(&mut stream, limit).await?;
        Ok(Self {
            framed: framed(Box::new(stream)),
            daemon_commit,
        })
    }

    /// `client / daemon` commits when this connection's daemon is not
    /// provably the same build as this client; `None` when it is. Never a
    /// reason to refuse the connection: the protocol versions already match.
    pub fn build_mismatch(&self) -> Option<String> {
        teiryo_core::version::build_mismatch(CLIENT_COMMIT, &self.daemon_commit)
    }

    /// Send one request and await its response.
    pub async fn request(&mut self, req: &Request) -> Result<Response, ClientError> {
        self.framed.send(encode_frame(req)?).await?;
        let frame = self.framed.next().await.ok_or(ClientError::Closed)??;
        Ok(decode_frame(&frame)?)
    }
}

/// Send our hello on `stream` and read the daemon's verdict within `limit`:
/// the daemon's build commit on accept.
async fn exchange<S: Transport>(stream: &mut S, limit: Duration) -> Result<String, ClientError> {
    match tokio::time::timeout(limit, client_handshake(stream, CLIENT_COMMIT)).await {
        Ok(result) => result.map_err(ClientError::Handshake),
        Err(_elapsed) => Err(ClientError::HandshakeTimeout(limit)),
    }
}

/// Events flowing into the TUI event loop from the update connection.
#[derive(Debug)]
pub enum NetEvent {
    /// A poll completed daemon-side; refresh status.
    Update(PollEvent),
    /// The daemon reloaded `config.toml` — from a `SetConfig` of ours or from
    /// someone editing the file — and the settings may have changed.
    Config(Box<ConfigState>),
    /// The update connection died; the TUI should reconnect.
    Disconnected(String),
    /// A background reconnect succeeded; this replaces the command
    /// connection.
    Reconnected(Box<Client>),
    /// A background reconnect attempt failed; another follows after backoff.
    ReconnectFailed(String),
    /// The daemon now on the socket speaks another protocol version, or the
    /// remote install has no `bridge`. Reconnecting has stopped: only the
    /// user can replace that daemon.
    Incompatible(String),
}

/// The wait before reconnect attempt `attempt` (0-based): one second,
/// doubling per failure, capped at thirty.
///
/// Each attempt may spawn a daemon, so this is also the ceiling on how fast a
/// daemon that keeps dying on startup is respawned.
pub fn reconnect_delay(attempt: u32) -> Duration {
    RECONNECT_BASE
        .checked_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX))
        .map_or(RECONNECT_MAX, |d| d.min(RECONNECT_MAX))
}

/// Reconnect in the background, so the event loop keeps drawing and handling
/// keys while the daemon is down. Reports through `tx` and ends on success,
/// on a version mismatch or a remote install without `bridge`, or when the
/// TUI is gone. A fresh task starts the
/// backoff over, so a success resets it. A remote target reruns ssh on each
/// attempt.
pub fn spawn_reconnect(target: Target, tx: mpsc::UnboundedSender<NetEvent>) -> JoinHandle<()> {
    let connect = {
        let target = target.clone();
        move || {
            let target = target.clone();
            async move { Client::connect(&target).await }
        }
    };
    tokio::spawn(reconnect_loop(connect, reconnect_delay, target, tx))
}

async fn reconnect_loop<F, Fut>(
    mut connect: F,
    delay: impl Fn(u32) -> Duration,
    target: Target,
    tx: mpsc::UnboundedSender<NetEvent>,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Client, ClientError>>,
{
    let mut attempt: u32 = 0;
    loop {
        tokio::time::sleep(delay(attempt)).await;
        match connect().await {
            Ok(client) => {
                let _ = tx.send(NetEvent::Reconnected(Box::new(client)));
                return;
            }
            Err(e) => {
                if let Some(message) = incompatible_message(&e, &target) {
                    let _ = tx.send(NetEvent::Incompatible(message));
                    return;
                }
                if tx.send(NetEvent::ReconnectFailed(e.to_string())).is_err() {
                    return; // TUI is gone
                }
            }
        }
        attempt = attempt.saturating_add(1);
    }
}

/// The newest poll id visible in a status snapshot; the starting point for
/// `AwaitUpdate { since }`.
pub fn newest_poll_id(statuses: &[AccountStatus]) -> PollId {
    statuses
        .iter()
        .filter_map(|s| s.last_poll.as_ref().map(|p| p.id))
        .max()
        .unwrap_or_else(PollId::zero)
}

/// Run the long-poll loop on a dedicated connection: each `Update` advances
/// `since`, each `Config` advances `config_gen`, and both are forwarded;
/// `NoUpdate` just re-arms.
///
/// One request covers both of the daemon's clocks, so a `config.toml` edit
/// reaches the TUI as promptly as a completed poll does — without a third
/// connection or a polling timer on this side.
pub fn spawn_update_loop(
    mut client: Client,
    mut since: PollId,
    mut config_gen: u64,
    tx: mpsc::UnboundedSender<NetEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let req = Request::AwaitUpdate {
                since,
                config_gen,
                timeout_ms: AWAIT_TIMEOUT_MS,
            };
            match client.request(&req).await {
                Ok(Response::Update(event)) => {
                    since = since.max(event.id);
                    if tx.send(NetEvent::Update(event)).is_err() {
                        return; // TUI is gone
                    }
                }
                Ok(Response::Config(state)) => {
                    config_gen = config_gen.max(state.generation);
                    if tx.send(NetEvent::Config(Box::new(state))).is_err() {
                        return; // TUI is gone
                    }
                }
                Ok(Response::NoUpdate) => {}
                Ok(other) => {
                    let _ = tx.send(NetEvent::Disconnected(format!(
                        "unexpected reply to AwaitUpdate: {other:?}"
                    )));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Disconnected(e.to_string()));
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use teiryo_core::domain::{AccountId, ClientKind, PollOutcome, PollTrigger};
    use teiryo_core::Account;
    use tokio::net::UnixStream;

    fn status_with_poll(id: Option<PollId>) -> AccountStatus {
        AccountStatus {
            account: Account {
                id: AccountId::from("claude:x"),
                provider: "claude".into(),
                label: "x".into(),
            },
            windows: vec![],
            last_poll: id.map(|id| PollEvent {
                id,
                ts: chrono::Utc::now(),
                provider: "claude".into(),
                account: AccountId::from("claude:x"),
                trigger: PollTrigger::Manual {
                    client: ClientKind::Tui,
                },
                outcome: PollOutcome::Success { windows: vec![] },
                latency_ms: 1,
            }),
            last_success: None,
            poll_interval_secs: 60,
        }
    }

    #[test]
    fn newest_poll_id_empty_is_zero() {
        assert_eq!(newest_poll_id(&[]), PollId::zero());
        assert_eq!(newest_poll_id(&[status_with_poll(None)]), PollId::zero());
    }

    #[test]
    fn newest_poll_id_takes_max() {
        let a = PollId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = PollId::generate();
        let statuses = [
            status_with_poll(Some(a)),
            status_with_poll(None),
            status_with_poll(Some(b)),
        ];
        assert_eq!(newest_poll_id(&statuses), b);
    }

    #[test]
    fn reconnect_backoff_doubles_to_a_thirty_second_cap() {
        let secs: Vec<u64> = (0..8).map(|n| reconnect_delay(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
        // Far past the point where the shift itself would overflow.
        assert_eq!(reconnect_delay(31), Duration::from_secs(30));
        assert_eq!(reconnect_delay(32), Duration::from_secs(30));
        assert_eq!(reconnect_delay(u32::MAX), Duration::from_secs(30));
    }

    /// A unique socket path per test, outside the real runtime dir.
    fn temp_socket(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("teiryo-client-{tag}-{}.sock", std::process::id()))
    }

    /// The hang this bound exists for: a daemon that accepts (or merely has
    /// the connection queued in its backlog) but never answers.
    #[tokio::test]
    async fn silent_daemon_times_out_the_handshake() {
        let path = temp_socket("silent");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let held = tokio::spawn(async move {
            // Accept and hold the connection open without ever replying.
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        });

        let stream = UnixStream::connect(&path).await.unwrap();
        let started = std::time::Instant::now();
        let result = Client::handshake(stream, Duration::from_millis(100)).await;
        assert!(
            matches!(result, Err(ClientError::HandshakeTimeout(_))),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        held.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// A daemon on `tag`'s socket that answers the handshake as built from
    /// `daemon_commit`, and what the client made of it.
    async fn client_against(tag: &str, daemon_commit: &'static str) -> Result<Client, ClientError> {
        let path = temp_socket(tag);
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let client_commit =
                teiryo_core::protocol::handshake::server_handshake(&mut stream, daemon_commit)
                    .await
                    .unwrap();
            (stream, client_commit)
        });

        let stream = UnixStream::connect(&path).await.unwrap();
        let result = Client::handshake(stream, Duration::from_secs(2)).await;
        let (_stream, client_commit) = server.await.unwrap();
        assert_eq!(
            client_commit, CLIENT_COMMIT,
            "the client sends its own commit"
        );
        let _ = std::fs::remove_file(&path);
        result
    }

    /// The falsifier for the timeout: a daemon that does answer is not cut
    /// off by it.
    #[tokio::test]
    async fn answering_daemon_completes_the_handshake() {
        let result = client_against("answer", "def456").await;
        assert!(result.is_ok(), "{result:?}");
    }

    /// A daemon built from another commit is accepted all the same, and the
    /// client keeps both commits for the header.
    #[tokio::test]
    async fn another_build_connects_and_is_reported() {
        let client = client_against("other-build", "def456").await.unwrap();
        assert_eq!(
            client.build_mismatch(),
            Some(format!("{CLIENT_COMMIT} / def456"))
        );
    }

    /// The same build reports nothing, unless this build is one that can
    /// never vouch for itself.
    #[tokio::test]
    async fn the_same_build_is_reported_only_when_it_cannot_vouch_for_itself() {
        let client = client_against("same-build", CLIENT_COMMIT).await.unwrap();
        let unprovable = CLIENT_COMMIT.ends_with("-dirty") || CLIENT_COMMIT == "unknown";
        assert_eq!(client.build_mismatch().is_some(), unprovable);
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<NetEvent>) -> Vec<NetEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// A reject carries the daemon's version through to what the user reads;
    /// a pre-v8 daemon that sent none is still a mismatch, named as such.
    #[test]
    fn a_mismatch_names_both_versions() {
        let ahead = ClientError::Handshake(HandshakeError::Rejected {
            daemon: Some(PROTOCOL_VERSION + 1),
        });
        assert_eq!(version_mismatch(&ahead), Some(Some(PROTOCOL_VERSION + 1)));
        assert!(
            version_mismatch_message(Some(PROTOCOL_VERSION + 1), &Target::Local).contains(
                &format!(
                    "daemon speaks protocol v{}, this client v{PROTOCOL_VERSION}",
                    PROTOCOL_VERSION + 1
                )
            )
        );

        let old = ClientError::Handshake(HandshakeError::Rejected { daemon: None });
        assert_eq!(version_mismatch(&old), Some(None));
        assert!(
            version_mismatch_message(None, &Target::Local).contains(&format!(
                "daemon speaks protocol older than v8, this client v{PROTOCOL_VERSION}"
            ))
        );

        assert_eq!(version_mismatch(&ClientError::Closed), None);
    }

    /// A remote daemon's mismatch names the host, and the remedies that
    /// work from here: reinstalling there, or stopping it over ssh.
    #[test]
    fn a_remote_mismatch_names_the_host() {
        let target = Target::Ssh(SshTarget::new("box".into(), Some("ssh".into()), None).unwrap());
        let message = version_mismatch_message(Some(PROTOCOL_VERSION + 1), &target);
        assert!(
            message.starts_with(&format!(
                "daemon on box speaks protocol v{}, this client v{PROTOCOL_VERSION}",
                PROTOCOL_VERSION + 1
            )),
            "{message}"
        );
        assert!(message.contains("`ssh box pkill teiryod`"), "{message}");
    }

    /// An executable stand-in for ssh that runs `script`, ignoring the
    /// arguments ssh would get. It deletes itself once started, so it must
    /// be run exactly once.
    fn fake_ssh_target(script: &str) -> Target {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("teiryo-fake-ssh-{}-{n}", std::process::id()));
        write_executable(&path, &format!("#!/bin/sh\nrm -f \"$0\"\n{script}\n"));
        Target::Ssh(SshTarget::new("box".into(), Some(path.into_os_string()), None).unwrap())
    }

    /// Write `contents` to a new executable at `path` through a child shell,
    /// so this process never holds the file open for writing. A parallel test
    /// that forks while it did would carry that descriptor into its child
    /// until the child execs, and running the file meanwhile fails with
    /// `ETXTBSY` ("Text file busy").
    fn write_executable(path: &std::path::Path, contents: &str) {
        use std::io::Write;

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "writing {}: {status:?}", path.display());
    }

    /// ssh refusing to connect reaches the user in ssh's own words, naming
    /// the host, rather than as a bare end of stream.
    #[tokio::test]
    async fn a_failed_ssh_is_reported_in_its_own_words() {
        let target = fake_ssh_target(
            "echo 'ssh: connect to host box port 22: Connection refused' >&2; exit 255",
        );
        let err = Client::connect(&target).await.unwrap_err();
        assert!(
            matches!(&err, ClientError::Ssh { host, .. } if host == "box"),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "cannot reach the daemon on box over ssh: \
             ssh: connect to host box port 22: Connection refused"
        );
        assert_eq!(version_mismatch(&err), None, "an ssh failure is retried");
    }

    /// A remote teiryod too old to have `bridge` is named as such, and the
    /// reconnect loop stops on it as it does on a mismatch: no retry fixes it.
    #[tokio::test]
    async fn a_remote_install_without_bridge_is_named_and_not_retried() {
        let usage = teiryo_core::version::usage_error("teiryod", "bridge");
        let target = fake_ssh_target(&format!(
            "printf '%s\\n' '{}' >&2; exit 2",
            usage.replace('\'', r"'\''")
        ));
        let err = Client::connect(&target).await.unwrap_err();
        assert!(
            matches!(&err, ClientError::NoBridge { host } if host == "box"),
            "{err:?}"
        );
        let message = incompatible_message(&err, &target).expect("not retried");
        assert!(message.contains("has no `bridge` command"), "{message}");
        assert!(
            message.contains("install the matching teiryod"),
            "{message}"
        );
    }

    /// ssh still running when the handshake times out says nothing about
    /// the remote daemon, so the error does not claim it was reached.
    #[tokio::test]
    async fn a_running_ssh_that_never_answers_is_not_blamed_on_the_daemon() {
        let Target::Ssh(ssh) = fake_ssh_target("exec sleep 30") else {
            unreachable!()
        };
        let err = Client::connect_ssh(&ssh, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ClientError::SshTimeout { host, .. } if host == "box"),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(!message.contains("accepted the connection"), "{message}");
        assert!(message.contains("ssh is still running"), "{message}");
        assert_eq!(incompatible_message(&err, &Target::Ssh(ssh)), None);
    }

    /// The falsifier for the test above: a remote daemon's reject is a
    /// version mismatch, not an ssh failure, even though ssh exits right
    /// after it — so reconnecting stops, as it does locally.
    #[tokio::test]
    async fn a_remote_reject_stays_a_version_mismatch() {
        let [lo, hi] = (PROTOCOL_VERSION + 1).to_le_bytes();
        let target = fake_ssh_target(&format!(
            "printf '\\001\\{lo:03o}\\{hi:03o}'; echo 'bridge done' >&2"
        ));
        let err = Client::connect(&target).await.unwrap_err();
        assert_eq!(
            version_mismatch(&err),
            Some(Some(PROTOCOL_VERSION + 1)),
            "{err:?}"
        );
    }

    /// A remote daemon that accepts is a working connection, and its build
    /// is the one the header compares against.
    #[tokio::test]
    async fn a_remote_accept_connects_and_reports_the_remote_build() {
        let target = fake_ssh_target("printf '\\000\\006\\000def456'; exec cat >/dev/null");
        let client = Client::connect(&target).await.unwrap();
        assert_eq!(
            client.build_mismatch(),
            Some(format!("{CLIENT_COMMIT} / def456"))
        );
    }

    /// Ordinary failures retry; a version mismatch ends the loop, because
    /// every further attempt would hit the same stale daemon — and spawn
    /// another daemon that exits at once — for as long as the TUI runs.
    #[tokio::test]
    async fn mismatch_stops_reconnecting() {
        let calls = std::cell::Cell::new(0u32);
        let (tx, mut rx) = mpsc::unbounded_channel();
        reconnect_loop(
            || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move {
                    if n < 3 {
                        Err(ClientError::Io(
                            std::io::ErrorKind::ConnectionRefused.into(),
                        ))
                    } else {
                        Err(ClientError::Handshake(HandshakeError::Rejected {
                            daemon: Some(PROTOCOL_VERSION + 1),
                        }))
                    }
                }
            },
            |_| Duration::ZERO,
            Target::Local,
            tx,
        )
        .await;

        assert_eq!(calls.get(), 3, "no attempt after the mismatch");
        let events = drain(&mut rx);
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(events[0], NetEvent::ReconnectFailed(_)));
        assert!(matches!(events[1], NetEvent::ReconnectFailed(_)));
        let NetEvent::Incompatible(message) = &events[2] else {
            panic!("the last event names the mismatch: {events:?}");
        };
        assert!(
            message.contains(&format!(
                "daemon speaks protocol v{}, this client v{PROTOCOL_VERSION}",
                PROTOCOL_VERSION + 1
            )),
            "{message}"
        );
    }

    /// Each attempt waits out its own step of the schedule, in order.
    #[tokio::test]
    async fn reconnect_waits_by_attempt_number() {
        let asked = std::cell::RefCell::new(Vec::new());
        let calls = std::cell::Cell::new(0u32);
        let (tx, _rx) = mpsc::unbounded_channel();
        reconnect_loop(
            || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move {
                    if n < 4 {
                        Err(ClientError::Closed)
                    } else {
                        Err(ClientError::Handshake(HandshakeError::Rejected {
                            daemon: None,
                        }))
                    }
                }
            },
            |attempt| {
                asked.borrow_mut().push(attempt);
                Duration::ZERO
            },
            Target::Local,
            tx,
        )
        .await;
        assert_eq!(*asked.borrow(), [0, 1, 2, 3]);
    }

    /// A successful attempt hands its connection to the TUI and ends the loop:
    /// one `Reconnected`, and no attempt after it.
    #[tokio::test]
    async fn success_reports_reconnected_and_stops() {
        let path = temp_socket("reconnected");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            teiryo_core::protocol::handshake::server_handshake(&mut stream, "def456")
                .await
                .unwrap();
            stream
        });
        let stream = UnixStream::connect(&path).await.unwrap();
        let client = Client::handshake(stream, Duration::from_secs(2))
            .await
            .unwrap();
        let _server_side = server.await.unwrap();

        let mut ready = Some(client);
        let calls = std::cell::Cell::new(0u32);
        let (tx, mut rx) = mpsc::unbounded_channel();
        reconnect_loop(
            || {
                calls.set(calls.get() + 1);
                // First attempt fails; the second gets the real connection.
                // A third would get a mismatch, which ends the loop, so a
                // loop that kept going after success fails the count below
                // instead of hanging.
                let next = if calls.get() == 1 {
                    Err(ClientError::Closed)
                } else {
                    ready
                        .take()
                        .ok_or(ClientError::Handshake(HandshakeError::Rejected {
                            daemon: None,
                        }))
                };
                async move { next }
            },
            |_| Duration::ZERO,
            Target::Local,
            tx,
        )
        .await;

        assert_eq!(calls.get(), 2, "no attempt after the success");
        let events = drain(&mut rx);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(events[0], NetEvent::ReconnectFailed(_)));
        assert!(matches!(events[1], NetEvent::Reconnected(_)), "{events:?}");
        let _ = std::fs::remove_file(&path);
    }

    /// A TUI that has gone away stops the loop instead of retrying forever.
    #[tokio::test]
    async fn reconnect_stops_when_the_tui_is_gone() {
        let calls = std::cell::Cell::new(0u32);
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        reconnect_loop(
            || {
                calls.set(calls.get() + 1);
                async { Err(ClientError::Closed) }
            },
            |_| Duration::ZERO,
            Target::Local,
            tx,
        )
        .await;
        assert_eq!(calls.get(), 1);
    }
}
