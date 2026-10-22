//! `teiryod bridge`: the far end of `teiryo --ssh HOST`.
//!
//! ssh runs the bridge on the remote host with the client's stream as its
//! stdin and stdout. The bridge connects to this host's daemon socket —
//! spawning the daemon if nothing is listening, exactly as a local TUI would —
//! and copies bytes both ways until either side closes. It never reads what
//! it copies, so the client's handshake reaches the daemon unchanged and the
//! daemon alone decides whether the client's protocol version is its own.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Copy `input` to `socket` and `socket` to `output` until either side
/// closes.
///
/// When the daemon closes, the bridge ends at once, so ssh exits and the
/// client sees the end of its stream. When the client closes (or its side
/// fails), the daemon is told (its read side ends) and whatever it still sends
/// is delivered before the bridge ends.
pub async fn relay<I, O, S>(mut input: I, mut output: O, socket: S) -> io::Result<()>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut from_daemon, mut to_daemon) = tokio::io::split(socket);
    // Chunk by chunk, each flushed before the next read: a read error must
    // never strand bytes already read, and the client is waiting on each one.
    let down = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = match from_daemon.read(&mut buf).await {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                // A daemon that closes with bytes of ours unread — the commit
                // after a rejected hello — is reported as a reset, after
                // everything it sent. That is its end of stream.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Ok(()),
                Err(e) => return Err(e),
            };
            output.write_all(&buf[..n]).await?;
            output.flush().await?;
        }
    };
    let up = async {
        // Whether the client closed or its side failed, the daemon is told
        // the same way, and its reply still goes out below.
        let _ = tokio::io::copy(&mut input, &mut to_daemon).await;
        let _ = to_daemon.shutdown().await;
    };
    tokio::pin!(down);
    tokio::select! {
        done = &mut down => return done,
        () = up => {}
    }
    down.await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// Both directions carry their bytes unchanged, and the daemon closing
    /// ends the relay even though the client has not closed.
    #[tokio::test]
    async fn bytes_cross_both_ways_and_the_daemon_closing_ends_it() {
        let (client_in, input) = duplex(64);
        let (output, mut client_out) = duplex(64);
        let (socket, mut daemon) = duplex(64);
        let relay = tokio::spawn(relay(input, output, socket));

        let mut client_in = client_in;
        client_in.write_all(b"hello").await.unwrap();
        let mut got = [0u8; 5];
        daemon.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello");

        daemon.write_all(b"reply").await.unwrap();
        drop(daemon);
        let mut back = Vec::new();
        client_out.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"reply");
        relay.await.unwrap().unwrap();
        drop(client_in);
    }

    /// The client closing is passed on to the daemon, whose last bytes still
    /// reach the client.
    #[tokio::test]
    async fn the_client_closing_reaches_the_daemon_and_its_reply_still_arrives() {
        let (client_in, input) = duplex(64);
        let (output, mut client_out) = duplex(64);
        let (socket, mut daemon) = duplex(64);
        let relay = tokio::spawn(relay(input, output, socket));

        drop(client_in);
        let mut rest = Vec::new();
        daemon.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        daemon.write_all(b"bye").await.unwrap();
        drop(daemon);

        let mut back = Vec::new();
        client_out.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"bye");
        relay.await.unwrap().unwrap();
    }
}
