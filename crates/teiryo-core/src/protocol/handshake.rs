//! Hand-decoded, never-changing connection handshake.
//!
//! First 6 bytes on every connection, raw — not bincode, so the handshake
//! itself can never go stale across versions. The daemon replies with a
//! single fixed byte and closes the connection on mismatch.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::HandshakeError;

/// Magic bytes opening every connection.
pub const PROTOCOL_MAGIC: [u8; 4] = *b"TEIR";
/// Current wire protocol version (little-endian u16 on the wire).
pub const PROTOCOL_VERSION: u16 = 4;
/// Daemon reply: handshake accepted, bincode frames may follow.
pub const HELLO_ACCEPTED: u8 = 0x00;
/// Daemon reply: protocol version mismatch, connection will be closed.
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
        HELLO_REJECTED_VERSION => Err(HandshakeError::Rejected(HELLO_REJECTED_VERSION)),
        other => Err(HandshakeError::UnexpectedReply(other)),
    }
}

/// Server side: read the peer's hello, accept or reject it.
///
/// On success the accept byte has been written and bincode frames may follow.
/// On failure the reject byte has been written (where applicable) and the
/// caller must close the connection without decoding anything further.
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
        stream.write_all(&[HELLO_REJECTED_VERSION]).await?;
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
        let mut reply = [0u8; 1];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], HELLO_REJECTED_VERSION);

        match server_task.await.unwrap() {
            Err(HandshakeError::VersionMismatch { ours, theirs }) => {
                assert_eq!(ours, PROTOCOL_VERSION);
                assert_eq!(theirs, 999);
            }
            other => panic!("expected version mismatch, got {other:?}"),
        }
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
}
