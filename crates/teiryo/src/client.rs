//! Typed protocol client: handshake, framed request/response, and the
//! `AwaitUpdate` long-poll loop.
//!
//! The TUI uses two connections: one for interactive requests (never blocked
//! longer than a normal round-trip) and one dedicated to the long-poll loop,
//! which may legitimately sit idle for `AWAIT_TIMEOUT_MS` at a time.

use std::future::Future;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use teiryo_core::protocol::codec::{decode_frame, encode_frame, framed};
use teiryo_core::protocol::handshake::{client_handshake, PROTOCOL_VERSION};
use teiryo_core::{
    AccountStatus, ConfigState, HandshakeError, PollEvent, PollId, Request, Response, WireError,
};

use crate::spawn::connect_or_spawn;

/// How long the daemon may hold one `AwaitUpdate` open.
pub const AWAIT_TIMEOUT_MS: u32 = 25_000;

/// How long a daemon that accepted the connection has to answer the
/// handshake. A healthy daemon answers in microseconds; one that bound its
/// socket but never reached its accept loop would otherwise hang the TUI
/// before the terminal is even set up.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Whether a handshake failure is the "stale daemon of a different protocol
/// version" case that the user must resolve by letting the daemon restart.
pub fn is_version_mismatch(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::Handshake(HandshakeError::Rejected(_))
            | ClientError::Handshake(HandshakeError::VersionMismatch { .. })
    )
}

/// What the user is told once a reconnect finds a daemon of another protocol
/// version. Retrying cannot fix it — the stale daemon keeps the socket — so it
/// stays on screen until the user acts.
pub fn version_mismatch_message() -> String {
    format!(
        "daemon speaks another protocol than this client (v{PROTOCOL_VERSION}) — \
         stop it (`pkill teiryod`) and relaunch teiryo"
    )
}

/// One handshaken, framed connection to the daemon.
pub struct Client {
    framed: Framed<UnixStream, LengthDelimitedCodec>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    /// Connect (spawning the daemon if needed) and complete the handshake.
    pub async fn connect() -> Result<Self, ClientError> {
        let stream = connect_or_spawn().await?;
        Self::handshake(stream, HANDSHAKE_TIMEOUT).await
    }

    /// Complete the handshake on a connected stream, giving up after `limit`.
    async fn handshake(mut stream: UnixStream, limit: Duration) -> Result<Self, ClientError> {
        match tokio::time::timeout(limit, client_handshake(&mut stream)).await {
            Ok(result) => result.map_err(ClientError::Handshake)?,
            Err(_elapsed) => return Err(ClientError::HandshakeTimeout(limit)),
        }
        Ok(Self {
            framed: framed(stream),
        })
    }

    /// Send one request and await its response.
    pub async fn request(&mut self, req: &Request) -> Result<Response, ClientError> {
        self.framed.send(encode_frame(req)?).await?;
        let frame = self.framed.next().await.ok_or(ClientError::Closed)??;
        Ok(decode_frame(&frame)?)
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
    /// The daemon now on the socket speaks another protocol version.
    /// Reconnecting has stopped: only the user can replace that daemon.
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
/// on a version mismatch, or when the TUI is gone. A fresh task starts the
/// backoff over, so a success resets it.
pub fn spawn_reconnect(tx: mpsc::UnboundedSender<NetEvent>) -> JoinHandle<()> {
    tokio::spawn(reconnect_loop(Client::connect, reconnect_delay, tx))
}

async fn reconnect_loop<F, Fut>(
    mut connect: F,
    delay: impl Fn(u32) -> Duration,
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
            Err(e) if is_version_mismatch(&e) => {
                let _ = tx.send(NetEvent::Incompatible(version_mismatch_message()));
                return;
            }
            Err(e) => {
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

    /// The falsifier for the timeout: a daemon that does answer is not cut
    /// off by it.
    #[tokio::test]
    async fn answering_daemon_completes_the_handshake() {
        let path = temp_socket("answer");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            teiryo_core::protocol::handshake::server_handshake(&mut stream)
                .await
                .unwrap();
            stream
        });

        let stream = UnixStream::connect(&path).await.unwrap();
        let result = Client::handshake(stream, Duration::from_secs(2)).await;
        assert!(result.is_ok(), "{result:?}");
        let _ = server.await;
        let _ = std::fs::remove_file(&path);
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<NetEvent>) -> Vec<NetEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
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
                        Err(ClientError::Handshake(HandshakeError::Rejected(1)))
                    }
                }
            },
            |_| Duration::ZERO,
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
            message.contains(&format!("v{PROTOCOL_VERSION}")),
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
                        Err(ClientError::Handshake(HandshakeError::Rejected(1)))
                    }
                }
            },
            |attempt| {
                asked.borrow_mut().push(attempt);
                Duration::ZERO
            },
            tx,
        )
        .await;
        assert_eq!(*asked.borrow(), [0, 1, 2, 3]);
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
            tx,
        )
        .await;
        assert_eq!(calls.get(), 1);
    }
}
