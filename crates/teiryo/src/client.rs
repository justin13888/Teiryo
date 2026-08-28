//! Typed protocol client: handshake, framed request/response, and the
//! `AwaitUpdate` long-poll loop.
//!
//! The TUI uses two connections: one for interactive requests (never blocked
//! longer than a normal round-trip) and one dedicated to the long-poll loop,
//! which may legitimately sit idle for `AWAIT_TIMEOUT_MS` at a time.

use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use teiryo_core::protocol::codec::{decode_frame, encode_frame, framed};
use teiryo_core::protocol::handshake::client_handshake;
use teiryo_core::{
    AccountStatus, ConfigState, HandshakeError, PollEvent, PollId, Request, Response, WireError,
};

use crate::spawn::connect_or_spawn;

/// How long the daemon may hold one `AwaitUpdate` open.
pub const AWAIT_TIMEOUT_MS: u32 = 25_000;

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

/// One handshaken, framed connection to the daemon.
pub struct Client {
    framed: Framed<UnixStream, LengthDelimitedCodec>,
}

impl Client {
    /// Connect (spawning the daemon if needed) and complete the handshake.
    pub async fn connect() -> Result<Self, ClientError> {
        let mut stream = connect_or_spawn().await?;
        client_handshake(&mut stream)
            .await
            .map_err(ClientError::Handshake)?;
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
}
