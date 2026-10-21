//! Hand-decoded, never-changing connection handshake.
//!
//! First 6 bytes on every connection, raw — not bincode, so the handshake
//! itself can never go stale across versions. The daemon replies with a
//! single fixed byte; on mismatch it follows it with its own version (u16
//! little-endian, since v8) and closes the connection.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::HandshakeError;

/// Magic bytes opening every connection.
pub const PROTOCOL_MAGIC: [u8; 4] = *b"TEIR";
/// Current wire protocol version (little-endian u16 on the wire).
pub const PROTOCOL_VERSION: u16 = 8;
/// Daemon reply: handshake accepted, bincode frames may follow.
pub const HELLO_ACCEPTED: u8 = 0x00;
/// Daemon reply: protocol version mismatch. Since v8 the daemon's own
/// version follows as a little-endian u16, then the connection is closed.
pub const HELLO_REJECTED_VERSION: u8 = 0x01;

/// The 6-byte hello: magic followed by the protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// Must equal [`PROTOCOL_MAGIC`].
    pub magic: [u8; 4],
    /// Sender's protocol version.
    pub protocol_version: u16,
}

impl Hello {
    /// Hello for the running build.
    pub fn current() -> Self {
        Self {
            magic: PROTOCOL_MAGIC,
            protocol_version: PROTOCOL_VERSION,
        }
    }

    /// Raw wire representation.
    pub fn to_bytes(self) -> [u8; 6] {
        let mut buf = [0u8; 6];
        buf[..4].copy_from_slice(&self.magic);
        buf[4..].copy_from_slice(&self.protocol_version.to_le_bytes());
        buf
    }

    /// Parse the raw wire representation.
    pub fn from_bytes(buf: [u8; 6]) -> Self {
        Self {
            magic: [buf[0], buf[1], buf[2], buf[3]],
            protocol_version: u16::from_le_bytes([buf[4], buf[5]]),
        }
    }
}

/// Client side: send our hello, await the daemon's verdict.
pub async fn client_handshake<S>(stream: &mut S) -> Result<(), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(&Hello::current().to_bytes()).await?;
    stream.flush().await?;
    let mut reply = [0u8; 1];
    stream.read_exact(&mut reply).await?;
    match reply[0] {
        HELLO_ACCEPTED => Ok(()),
        HELLO_REJECTED_VERSION => Err(HandshakeError::Rejected {
            daemon: read_daemon_version(stream).await,
        }),
        other => Err(HandshakeError::UnexpectedReply(other)),
    }
}

/// The version a rejecting daemon sends after the reject byte, or `None` where
/// it closed without one. A daemon older than v8 sends only the byte, and the
/// reject is already known, so a failed read means "unknown" rather than an
/// error.
async fn read_daemon_version<S>(stream: &mut S) -> Option<u16>
where
    S: AsyncRead + Unpin,
{
    let mut version = [0u8; 2];
    stream.read_exact(&mut version).await.ok()?;
    Some(u16::from_le_bytes(version))
}

/// Server side: read the peer's hello, accept or reject it.
///
/// On success the accept byte has been written and bincode frames may follow.
/// On failure the reject byte and our version have been written (where
/// applicable) and the caller must close the connection without decoding
/// anything further.
pub async fn server_handshake<S>(stream: &mut S) -> Result<(), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = [0u8; 6];
    stream.read_exact(&mut buf).await?;
    let hello = Hello::from_bytes(buf);
    if hello.magic != PROTOCOL_MAGIC {
        return Err(HandshakeError::BadMagic(hello.magic));
    }
    if hello.protocol_version != PROTOCOL_VERSION {
        let [lo, hi] = PROTOCOL_VERSION.to_le_bytes();
        stream.write_all(&[HELLO_REJECTED_VERSION, lo, hi]).await?;
        stream.flush().await?;
        return Err(HandshakeError::VersionMismatch {
            ours: PROTOCOL_VERSION,
            theirs: hello.protocol_version,
        });
    }
    stream.write_all(&[HELLO_ACCEPTED]).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip() {
        let hello = Hello::current();
        assert_eq!(Hello::from_bytes(hello.to_bytes()), hello);
        assert_eq!(&hello.to_bytes()[..4], b"TEIR");
    }

    #[tokio::test]
    async fn handshake_accepts_matching_version() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move { server_handshake(&mut server).await });
        client_handshake(&mut client)
            .await
            .expect("client accepted");
        server_task.await.expect("join").expect("server accepted");
    }

    #[tokio::test]
    async fn handshake_rejects_version_mismatch() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move { server_handshake(&mut server).await });

        // Hand-roll a v999 hello.
        let stale = Hello {
            magic: PROTOCOL_MAGIC,
            protocol_version: 999,
        };
        client.write_all(&stale.to_bytes()).await.unwrap();
        let mut reply = [0u8; 3];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], HELLO_REJECTED_VERSION);
        assert_eq!(u16::from_le_bytes([reply[1], reply[2]]), PROTOCOL_VERSION);

        match server_task.await.unwrap() {
            Err(HandshakeError::VersionMismatch { ours, theirs }) => {
                assert_eq!(ours, PROTOCOL_VERSION);
                assert_eq!(theirs, 999);
            }
            other => panic!("expected version mismatch, got {other:?}"),
        }
    }

    /// The six bytes this build opens a connection with, recorded rather than
    /// recomputed.
    ///
    /// Every other test here compares the version against itself:
    /// `handshake_accepts_matching_version` asks `Hello::current()` on both
    /// sides, and the mismatch tests use a literal `999`. So the constant that
    /// exists to announce a wire change could be reverted with the whole suite
    /// green — the one thing it must never be able to do quietly.
    ///
    /// **If this fails, the protocol version moved.** That is correct and
    /// expected when the wire actually changed: update these bytes and
    /// `docs/protocol.md` together, as `AGENTS.md` requires. On its own it is
    /// a revert nobody meant.
    #[test]
    fn the_hello_bytes_are_recorded_not_recomputed() {
        assert_eq!(Hello::current().to_bytes(), [b'T', b'E', b'I', b'R', 8, 0]);
    }

    #[tokio::test]
    async fn handshake_rejects_bad_magic() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move { server_handshake(&mut server).await });
        client.write_all(b"NOPE\x01\x00").await.unwrap();
        match server_task.await.unwrap() {
            Err(HandshakeError::BadMagic(m)) => assert_eq!(&m, b"NOPE"),
            other => panic!("expected bad magic, got {other:?}"),
        }
    }

    /// A daemon that rejects us with `reply` and then closes; what the client
    /// makes of it.
    async fn client_against_rejecting_daemon(reply: &'static [u8]) -> HandshakeError {
        let (mut client, mut daemon) = tokio::io::duplex(64);
        let daemon_task = tokio::spawn(async move {
            let mut hello = [0u8; 6];
            daemon.read_exact(&mut hello).await.unwrap();
            daemon.write_all(reply).await.unwrap();
            // Dropping `daemon` closes the connection, as a real reject does.
        });
        let err = client_handshake(&mut client)
            .await
            .expect_err("the daemon rejected us");
        daemon_task.await.unwrap();
        err
    }

    /// The acceptance case: a daemon one version ahead, and one behind, is
    /// reported with both versions named.
    #[tokio::test]
    async fn a_reject_names_the_daemon_version_and_ours() {
        const AHEAD: [u8; 3] = {
            let [lo, hi] = (PROTOCOL_VERSION + 1).to_le_bytes();
            [HELLO_REJECTED_VERSION, lo, hi]
        };
        const BEHIND: [u8; 3] = {
            let [lo, hi] = (PROTOCOL_VERSION - 1).to_le_bytes();
            [HELLO_REJECTED_VERSION, lo, hi]
        };
        for (reply, daemon) in [
            (&AHEAD, PROTOCOL_VERSION + 1),
            (&BEHIND, PROTOCOL_VERSION - 1),
        ] {
            let err = client_against_rejecting_daemon(reply).await;
            assert!(
                matches!(err, HandshakeError::Rejected { daemon: Some(v) } if v == daemon),
                "{err:?}"
            );
            let message = err.to_string();
            assert!(
                message.contains(&format!(
                    "daemon is v{daemon}, client is v{PROTOCOL_VERSION}"
                )),
                "{message}"
            );
        }
    }

    /// A daemon from before v8 sends the reject byte alone. The client still
    /// reports a mismatch, says the daemon's version is unknown, and names
    /// its own.
    #[tokio::test]
    async fn a_pre_v8_reject_without_a_version_is_still_a_reject() {
        let err = client_against_rejecting_daemon(&[HELLO_REJECTED_VERSION]).await;
        assert!(
            matches!(err, HandshakeError::Rejected { daemon: None }),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("older than v8"), "{message}");
        assert!(
            message.contains(&format!("client is v{PROTOCOL_VERSION}")),
            "{message}"
        );
    }
}
