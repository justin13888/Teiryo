//! Wire protocol: versioned handshake, bincode-encoded request/response
//! enums, and length-delimited framing.
//!
//! Connection lifecycle: raw 6-byte [`handshake`] first (never bincode, so it
//! can never itself go stale), then [`codec`]-framed bincode frames carrying
//! [`wire::Request`] / [`wire::Response`].

pub mod codec;
pub mod handshake;
pub mod wire;
