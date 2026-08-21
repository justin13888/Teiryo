//! Error types shared across the workspace.

use serde::{Deserialize, Serialize};

/// Handshake failure (before any bincode bytes are exchanged).
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// Peer sent something that is not a Teiryo hello.
    #[error("bad handshake magic {0:?}")]
    BadMagic([u8; 4]),
    /// Peer speaks a different protocol version.
    #[error("protocol version mismatch: ours v{ours}, peer v{theirs} — restart the daemon")]
    VersionMismatch {
        /// Our protocol version.
        ours: u16,
        /// The peer's protocol version.
        theirs: u16,
    },
    /// Daemon replied with a reject code (client side).
    #[error(
        "daemon rejected handshake (code {0:#04x}): daemon and client protocol versions differ — restart the daemon"
    )]
    Rejected(u8),
    /// Daemon replied with an unknown code.
    #[error("unexpected handshake reply {0:#04x}")]
    UnexpectedReply(u8),
    /// I/O failure during the handshake.
    #[error("handshake i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Frame (de)serialization failure.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// bincode encoding failed.
    #[error("frame encode: {0}")]
    Encode(#[from] bincode::error::EncodeError),
    /// bincode decoding failed.
    #[error("frame decode: {0}")]
    Decode(#[from] bincode::error::DecodeError),
}

/// Machine-readable error category carried in `Response::Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ErrorKind {
    /// Request named a provider the daemon does not know.
    #[error("unknown provider")]
    UnknownProvider,
    /// Request named an account the daemon does not know.
    #[error("unknown account")]
    UnknownAccount,
    /// Request was structurally valid but semantically bad.
    #[error("bad request")]
    BadRequest,
    /// Storage layer failure.
    #[error("storage error")]
    Storage,
    /// Anything else that went wrong inside the daemon.
    #[error("internal error")]
    Internal,
}
