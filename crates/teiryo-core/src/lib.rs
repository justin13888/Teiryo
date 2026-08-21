//! Domain types, wire protocol, adapter traits, and storage for Teiryo.
//!
//! This crate is the only dependency the TUI needs: it defines the domain
//! model, the versioned Unix-socket wire protocol, the provider adapter
//! traits, and the SQLite storage used by the daemon.

pub mod domain;
pub mod error;
pub mod protocol;

pub use domain::{
    Account, AccountId, ClientKind, PollEvent, PollId, PollOutcome, PollTrigger, ProviderId,
    QuotaSnapshot, QuotaUnit, QuotaWindow, ResetKind, WindowId, WindowScope,
};
pub use error::{ErrorKind, HandshakeError, WireError};
pub use protocol::codec::{
    decode_frame, encode_frame, framed, length_delimited_codec, MAX_FRAME_LEN,
};
pub use protocol::handshake::{
    client_handshake, server_handshake, Hello, PROTOCOL_MAGIC, PROTOCOL_VERSION,
};
pub use protocol::wire::{AccountStatus, ProviderHealth, Request, Response};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_ids_are_time_ordered() {
        let a = PollId::generate();
        let b = PollId::generate();
        assert!(PollId::zero() < a);
        assert!(a <= b);
    }

    #[test]
    fn outcome_error_messages() {
        assert_eq!(
            PollOutcome::Success { windows: vec![] }.error_message(),
            None
        );
        assert_eq!(
            PollOutcome::AuthError("expired".into()).error_message(),
            Some("expired")
        );
    }
}
