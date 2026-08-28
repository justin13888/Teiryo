//! Length-delimited framing and bincode frame (de)serialization.
//!
//! Both sides must configure the codec identically: u32 little-endian length
//! prefix, frames capped at [`MAX_FRAME_LEN`] so a garbage or oversized frame
//! from a misbehaving peer is rejected instead of buffered unbounded.

use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::error::WireError;

/// Maximum frame payload size (1 MiB).
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// The canonical codec configuration. Daemon and TUI must both use this.
pub fn length_delimited_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_type::<u32>()
        .little_endian()
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

/// Wrap a stream with the canonical framing.
pub fn framed<T>(io: T) -> Framed<T, LengthDelimitedCodec>
where
    T: AsyncRead + AsyncWrite,
{
    Framed::new(io, length_delimited_codec())
}

/// Encode a value into a frame payload (bincode 2, serde mode, standard config).
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Bytes, WireError> {
    let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())?;
    Ok(Bytes::from(bytes))
}

/// Decode a frame payload produced by [`encode_frame`].
pub fn decode_frame<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, WireError> {
    let (value, _len) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{TimeZone, Utc};
    use futures::{SinkExt, StreamExt};

    use super::*;
    use crate::domain::*;
    use crate::error::ErrorKind;
    use crate::protocol::wire::{AccountStatus, HistoryPage, ProviderHealth, Request, Response};
    use crate::rollover::{RolloverKind, WindowRollover};

    fn sample_window() -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from("session_5h_opus"),
            label: "Opus — 5 hour".into(),
            scope: WindowScope::Model("opus".into()),
            reset_kind: ResetKind::Rolling(Duration::from_secs(5 * 3600)),
            unit: QuotaUnit::Percent,
            used: 42.5,
            limit: None,
            reset_at: Some(Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap()),
        }
    }

    fn sample_event(outcome: PollOutcome) -> PollEvent {
        PollEvent {
            id: PollId::generate(),
            ts: Utc::now(),
            provider: "claude".into(),
            account: AccountId::from("claude:personal"),
            trigger: PollTrigger::Manual {
                client: ClientKind::Tui,
            },
            outcome,
            latency_ms: 128,
        }
    }

    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
        let bytes = encode_frame(value).expect("encode");
        let back: T = decode_frame(&bytes).expect("decode");
        assert_eq!(&back, value);
    }

    #[test]
    fn request_variants_roundtrip() {
        let requests = [
            Request::Status {
                provider: Some("claude".into()),
                account: Some(AccountId::from("claude:personal")),
            },
            Request::PollNow {
                provider: "claude".into(),
                account: None,
            },
            Request::AwaitUpdate {
                since: PollId::zero(),
                timeout_ms: 30_000,
            },
            Request::History {
                account: AccountId::from("claude:personal"),
                window: Some(WindowId::from("weekly_all")),
                since: Utc::now(),
                until: Some(Utc::now()),
                max_points: Some(240),
            },
            Request::History {
                account: AccountId::from("claude:personal"),
                window: None,
                since: Utc::now(),
                until: None,
                max_points: None,
            },
            Request::RecentPolls { limit: 50 },
            Request::Providers,
            Request::Shutdown,
        ];
        for request in &requests {
            roundtrip(request);
        }
    }

    #[test]
    fn response_variants_roundtrip() {
        let responses = [
            Response::Status(vec![AccountStatus {
                account: Account {
                    id: AccountId::from("claude:personal"),
                    provider: "claude".into(),
                    label: "personal".into(),
                },
                windows: vec![sample_window()],
                last_poll: Some(sample_event(PollOutcome::Success {
                    windows: vec![sample_window()],
                })),
            }]),
            Response::PollAccepted {
                poll_id: PollId::generate(),
            },
            Response::Update(sample_event(PollOutcome::RateLimited {
                retry_after: Some(Duration::from_secs(60)),
            })),
            Response::NoUpdate,
            Response::History(HistoryPage {
                snapshots: vec![QuotaSnapshot {
                    poll_id: PollId::generate(),
                    ts: Utc::now(),
                    window: WindowId::from("session_5h_opus"),
                    label: "Opus — 5 hour".into(),
                    unit: QuotaUnit::Percent,
                    used: 61.0,
                    limit: None,
                    reset_at: None,
                }],
                earliest: Some(Utc::now()),
                rollovers: vec![WindowRollover {
                    account: AccountId::from("claude:personal"),
                    window: WindowId::from("session_5h_opus"),
                    poll: PollId::generate(),
                    observed_at: Utc::now(),
                    kind: RolloverKind::Early,
                    prev_reset_at: Some(Utc::now()),
                    new_reset_at: Some(Utc::now()),
                    prev_used: 88.0,
                    new_used: 1.0,
                }],
            }),
            Response::History(HistoryPage {
                snapshots: Vec::new(),
                earliest: None,
                rollovers: Vec::new(),
            }),
            Response::RecentPolls(vec![
                sample_event(PollOutcome::AuthError("token expired".into())),
                sample_event(PollOutcome::NetworkError("connection refused".into())),
                sample_event(PollOutcome::SchemaDrift("missing field".into())),
            ]),
            Response::Providers(vec![ProviderHealth {
                provider: "claude".into(),
                accounts: vec![AccountId::from("claude:personal")],
                consecutive_failures: 3,
                last_error: Some("rate limited".into()),
            }]),
            Response::Ack,
            Response::Err(ErrorKind::UnknownProvider, "no such provider".into()),
        ];
        for response in &responses {
            roundtrip(response);
        }
    }

    #[tokio::test]
    async fn framed_roundtrip_over_duplex() {
        let (client, server) = tokio::io::duplex(MAX_FRAME_LEN);
        let mut client = framed(client);
        let mut server = framed(server);

        let request = Request::RecentPolls { limit: 10 };
        client.send(encode_frame(&request).unwrap()).await.unwrap();
        let frame = server.next().await.unwrap().unwrap();
        let decoded: Request = decode_frame(&frame).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        // A frame header claiming more than MAX_FRAME_LEN must error, not buffer.
        use tokio_util::codec::Decoder;
        let mut codec = length_delimited_codec();
        let mut buf = bytes::BytesMut::new();
        buf.extend_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_le_bytes());
        buf.extend_from_slice(&[0u8; 16]);
        assert!(codec.decode(&mut buf).is_err());
    }
}
