//! Hand-decoded connection handshake.
//!
//! First 6 bytes on every connection, raw — not bincode, so the version check
//! itself can never go stale across versions. Since v9 the client follows them
//! with its build commit. The daemon replies with a single fixed byte: on
//! accept it follows it with its own build commit (since v9); on mismatch it
//! follows it with its own version (u16 little-endian, since v8) and closes
//! the connection. A commit is a little-endian u16 byte length and that many
//! bytes of UTF-8.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::HandshakeError;

/// Magic bytes opening every connection.
pub const PROTOCOL_MAGIC: [u8; 4] = *b"TEIR";
/// Current wire protocol version (little-endian u16 on the wire).
pub const PROTOCOL_VERSION: u16 = 9;
/// Daemon reply: handshake accepted. Since v9 the daemon's build commit
/// follows, then bincode frames may follow.
pub const HELLO_ACCEPTED: u8 = 0x00;
/// Daemon reply: protocol version mismatch. Since v8 the daemon's own
/// version follows as a little-endian u16, then the connection is closed.
pub const HELLO_REJECTED_VERSION: u8 = 0x01;
/// Longest build commit either side sends or accepts, in bytes. A build's
/// commit is a short hash, so this only bounds what a misbehaving peer can
/// make the other allocate; a longer one is truncated before sending.
pub const MAX_COMMIT_LEN: usize = 256;

/// `commit` as it goes on the wire: its byte length as a little-endian u16,
/// then its UTF-8 bytes, cut to [`MAX_COMMIT_LEN`] at a character boundary.
pub fn encode_commit(commit: &str) -> Vec<u8> {
    let mut end = commit.len().min(MAX_COMMIT_LEN);
    while !commit.is_char_boundary(end) {
        end -= 1;
    }
    let bytes = &commit.as_bytes()[..end];
    let len = u16::try_from(bytes.len()).expect("MAX_COMMIT_LEN fits a u16");
    let mut buf = Vec::with_capacity(2 + bytes.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(bytes);
    buf
}

/// Read one commit written by [`encode_commit`]. Bytes that are not UTF-8
/// are replaced rather than refused: the commit is a diagnostic, and a garbled
/// one must not cost the connection.
async fn read_commit<S>(stream: &mut S) -> Result<String, HandshakeError>
where
    S: AsyncRead + Unpin,
{
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await?;
    let len = u16::from_le_bytes(len);
    if usize::from(len) > MAX_COMMIT_LEN {
        return Err(HandshakeError::CommitTooLong(len));
    }
    let mut bytes = vec![0u8; usize::from(len)];
    stream.read_exact(&mut bytes).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The 6-byte hello preamble: magic followed by the protocol version. Since
/// v9 the client sends its commit right after it.
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

/// Everything a client opens a connection with: the hello preamble, then
/// `commit`.
pub fn hello_message(commit: &str) -> Vec<u8> {
    let mut buf = Hello::current().to_bytes().to_vec();
    buf.extend_from_slice(&encode_commit(commit));
    buf
}

/// Client side: send our hello and our build `commit`, await the daemon's
/// verdict. On accept, returns the daemon's build commit.
pub async fn client_handshake<S>(stream: &mut S, commit: &str) -> Result<String, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(&hello_message(commit)).await?;
    stream.flush().await?;
    let mut reply = [0u8; 1];
    stream.read_exact(&mut reply).await?;
    match reply[0] {
        HELLO_ACCEPTED => read_commit(stream).await,
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

/// Server side: read the peer's hello, accept or reject it. On accept, returns
/// the client's build commit.
///
/// On success the accept byte and our build `commit` have been written and
/// bincode frames may follow. On failure the reject byte and our version have
/// been written (where applicable) and the caller must close the connection
/// without decoding anything further. The client's commit is read only once
/// its version matched: a client of another version may not send one, and the
/// reject must reach it all the same.
pub async fn server_handshake<S>(stream: &mut S, commit: &str) -> Result<String, HandshakeError>
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
    let client_commit = read_commit(stream).await?;
    let mut reply = vec![HELLO_ACCEPTED];
    reply.extend_from_slice(&encode_commit(commit));
    stream.write_all(&reply).await?;
    stream.flush().await?;
    Ok(client_commit)
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

    /// Each side learns the other's commit, and only the other's.
    #[tokio::test]
    async fn handshake_accepts_matching_version_and_swaps_commits() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task =
            tokio::spawn(async move { server_handshake(&mut server, "def456").await });
        let daemon_commit = client_handshake(&mut client, "abc123-dirty")
            .await
            .expect("client accepted");
        let client_commit = server_task.await.expect("join").expect("server accepted");
        assert_eq!(daemon_commit, "def456");
        assert_eq!(client_commit, "abc123-dirty");
    }

    /// An empty commit is a commit like any other: two zero length bytes.
    #[tokio::test]
    async fn an_empty_commit_round_trips() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, "").await });
        assert_eq!(client_handshake(&mut client, "").await.unwrap(), "");
        assert_eq!(server_task.await.unwrap().unwrap(), "");
    }

    #[tokio::test]
    async fn handshake_rejects_version_mismatch() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task =
            tokio::spawn(async move { server_handshake(&mut server, "def456").await });

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
        assert_eq!(Hello::current().to_bytes(), [b'T', b'E', b'I', b'R', 9, 0]);
    }

    /// The whole v9 opening and the accept reply, recorded the same way: the
    /// commit is a little-endian u16 length and its UTF-8 bytes.
    #[test]
    fn the_commit_bytes_are_recorded_not_recomputed() {
        assert_eq!(
            hello_message("abc"),
            [b'T', b'E', b'I', b'R', 9, 0, 3, 0, b'a', b'b', b'c']
        );
        assert_eq!(encode_commit(""), [0, 0]);
    }

    /// A commit past the bound is cut at a character boundary, never inside
    /// one, so what is sent still decodes as the text it started with.
    #[test]
    fn a_long_commit_is_cut_at_a_character_boundary() {
        let ascii = "a".repeat(MAX_COMMIT_LEN + 10);
        let encoded = encode_commit(&ascii);
        assert_eq!(
            usize::from(u16::from_le_bytes([encoded[0], encoded[1]])),
            MAX_COMMIT_LEN
        );
        assert_eq!(encoded.len(), 2 + MAX_COMMIT_LEN);

        // "é" is two bytes, so a cut at MAX_COMMIT_LEN would split one.
        let wide = format!("a{}", "é".repeat(MAX_COMMIT_LEN));
        let encoded = encode_commit(&wide);
        let len = usize::from(u16::from_le_bytes([encoded[0], encoded[1]]));
        assert_eq!(len, MAX_COMMIT_LEN - 1);
        assert!(std::str::from_utf8(&encoded[2..]).is_ok());
    }

    /// A peer that announces a commit longer than the bound is refused before
    /// anything is allocated for it, and is not accepted.
    #[tokio::test]
    async fn an_oversized_commit_is_refused() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, "d").await });
        let mut hello = Hello::current().to_bytes().to_vec();
        let too_long = u16::try_from(MAX_COMMIT_LEN + 1).unwrap();
        hello.extend_from_slice(&too_long.to_le_bytes());
        client.write_all(&hello).await.unwrap();
        match server_task.await.unwrap() {
            Err(HandshakeError::CommitTooLong(len)) => assert_eq!(len, too_long),
            other => panic!("expected an oversized commit, got {other:?}"),
        }
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty(), "no accept byte was sent: {rest:?}");
    }

    /// A commit that is not UTF-8 is a garbled diagnostic, not a reason to
    /// drop the connection.
    #[tokio::test]
    async fn a_commit_that_is_not_utf8_is_replaced_not_refused() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, "d").await });
        let mut hello = Hello::current().to_bytes().to_vec();
        hello.extend_from_slice(&[2, 0, b'a', 0xff]);
        client.write_all(&hello).await.unwrap();
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [HELLO_ACCEPTED, 1, 0, b'd']);
        assert_eq!(server_task.await.unwrap().unwrap(), "a\u{fffd}");
    }

    /// A v8 client sends the six preamble bytes and no commit. The daemon
    /// rejects it on the version alone, without waiting for a commit that
    /// will never come, and in the layout a v8 client reads.
    #[tokio::test]
    async fn a_v8_client_without_a_commit_is_still_rejected() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, "d").await });
        let old = Hello {
            magic: PROTOCOL_MAGIC,
            protocol_version: 8,
        };
        client.write_all(&old.to_bytes()).await.unwrap();
        let mut reply = [0u8; 3];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], HELLO_REJECTED_VERSION);
        assert_eq!(u16::from_le_bytes([reply[1], reply[2]]), PROTOCOL_VERSION);
        assert!(matches!(
            server_task.await.unwrap(),
            Err(HandshakeError::VersionMismatch { theirs: 8, .. })
        ));
    }

    #[tokio::test]
    async fn handshake_rejects_bad_magic() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, "d").await });
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
        let err = client_handshake(&mut client, "abc123")
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
