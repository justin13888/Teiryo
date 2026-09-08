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
    use crate::adapter::{BarStyle, RenderHint};
    use crate::domain::*;
    use crate::error::ErrorKind;
    use crate::protocol::wire::{
        AccountHealth, AccountStatus, ConfigEdit, ConfigState, ConfigView, HistoryPage,
        ProviderHealth, ProviderSettings, Request, Response, WindowView,
    };
    use crate::rollover::{ObservedStart, RolloverKind, WindowRollover};

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

    /// One `WindowView` frame, recorded byte for byte at `PROTOCOL_VERSION` 6.
    ///
    /// Every other test in this module encodes and decodes in the same
    /// process, which is self-consistency: rename a field, reorder a variant,
    /// change a representation, and both sides move together and the test
    /// stays green. That is exactly the change a protocol version exists to
    /// announce, and until this fixture existed nothing in the workspace could
    /// see one. The version's own tests cannot either — the mismatch tests use
    /// a literal, and the accepting test asks `Hello::current()` on both sides
    /// — so `PROTOCOL_VERSION` could be reverted to 5 with the whole suite
    /// green.
    ///
    /// `WindowView` is the type this window-anchoring work put on the wire, so
    /// it is the one recorded here.
    ///
    /// **If this fails, the wire format changed.** Do not re-record the bytes
    /// on their own: bump `PROTOCOL_VERSION`, update `docs/protocol.md` as
    /// `AGENTS.md` requires, and then re-record.
    const GOLDEN_WINDOW_VIEW: &[u8] = &[
        0x0f, 0x73, 0x65, 0x73, 0x73, 0x69, 0x6f, 0x6e, 0x5f, 0x35, 0x68, 0x5f, 0x6f, 0x70, 0x75,
        0x73, 0x0f, 0x4f, 0x70, 0x75, 0x73, 0x20, 0xe2, 0x80, 0x94, 0x20, 0x35, 0x20, 0x68, 0x6f,
        0x75, 0x72, 0x01, 0x04, 0x6f, 0x70, 0x75, 0x73, 0x00, 0xfb, 0x50, 0x46, 0x00, 0x03, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x40, 0x45, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x44,
        0x40, 0x01, 0x14, 0x32, 0x30, 0x32, 0x36, 0x2d, 0x30, 0x38, 0x2d, 0x31, 0x35, 0x54, 0x31,
        0x32, 0x3a, 0x30, 0x30, 0x3a, 0x30, 0x30, 0x5a, 0x00, 0xcd, 0xcc, 0x4c, 0x3f, 0x33, 0x33,
        0x73, 0x3f, 0x01, 0x16, 0x62, 0x6c, 0x6f, 0x63, 0x6b, 0x73, 0x20, 0x65, 0x6e, 0x74, 0x69,
        0x72, 0x65, 0x6c, 0x79, 0x20, 0x61, 0x74, 0x20, 0x63, 0x61, 0x70, 0x01, 0x14, 0x32, 0x30,
        0x32, 0x36, 0x2d, 0x30, 0x38, 0x2d, 0x31, 0x35, 0x54, 0x30, 0x37, 0x3a, 0x33, 0x30, 0x3a,
        0x30, 0x30, 0x5a, 0x14, 0x32, 0x30, 0x32, 0x36, 0x2d, 0x30, 0x38, 0x2d, 0x31, 0x35, 0x54,
        0x30, 0x37, 0x3a, 0x34, 0x35, 0x3a, 0x30, 0x30, 0x5a,
    ];

    /// The value `GOLDEN_WINDOW_VIEW` was recorded from.
    ///
    /// `QuotaUnit::Hours` rather than the `Percent` every other fixture uses,
    /// deliberately: `Percent` is the first variant, so bincode encodes it as
    /// zero and reordering the enum leaves the bytes identical. A variant
    /// reordered is exactly the change `docs/protocol.md` says must bump the
    /// version, so the recorded value has to sit somewhere the discriminant
    /// can move.
    fn golden_window_view() -> WindowView {
        WindowView {
            window: QuotaWindow {
                unit: QuotaUnit::Hours,
                limit: Some(40.0),
                ..sample_window()
            },
            hint: RenderHint {
                style: BarStyle::Percent,
                warn_threshold: 0.8,
                critical_threshold: 0.95,
                note: Some("blocks entirely at cap".into()),
            },
            observed_start: Some(ObservedStart {
                not_before: Utc.with_ymd_and_hms(2026, 8, 15, 7, 30, 0).unwrap(),
                not_after: Utc.with_ymd_and_hms(2026, 8, 15, 7, 45, 0).unwrap(),
            }),
        }
    }

    /// Every wire enum's discriminants, recorded.
    ///
    /// One frame can only ever pin the variants it happens to contain, so a
    /// single golden value is not enough: with `QuotaUnit::Hours` recorded,
    /// swapping `Messages` and `Tokens` leaves its bytes identical. Reordering
    /// variants is one of the changes `docs/protocol.md` says must bump the
    /// version, so each one is written down rather than sampled.
    ///
    /// **If this fails, variants were reordered, renumbered, or inserted
    /// mid-list.** Bump `PROTOCOL_VERSION` and update `docs/protocol.md`
    /// before re-recording. Appending a variant at the end is the one change
    /// that leaves these bytes alone.
    #[test]
    fn wire_enum_discriminants_are_recorded_not_derived() {
        use crate::domain::{QuotaUnit, WindowScope};
        use crate::rollover::RolloverKind;

        for (unit, want) in [
            (QuotaUnit::Percent, 0u8),
            (QuotaUnit::Messages, 1),
            (QuotaUnit::Tokens, 2),
            (QuotaUnit::Hours, 3),
        ] {
            assert_eq!(
                encode_frame(&unit).expect("encode").as_ref(),
                [want],
                "QuotaUnit::{unit:?}"
            );
        }
        for (kind, want) in [
            (RolloverKind::Scheduled, 0u8),
            (RolloverKind::Early, 1),
            (RolloverKind::Retracted, 2),
            (RolloverKind::Unannounced, 3),
        ] {
            assert_eq!(
                encode_frame(&kind).expect("encode").as_ref(),
                [want],
                "RolloverKind::{kind:?}"
            );
        }
        assert_eq!(
            encode_frame(&WindowScope::AccountWide)
                .expect("encode")
                .as_ref(),
            [0u8]
        );
        assert_eq!(
            encode_frame(&WindowScope::Model("opus".into()))
                .expect("encode")
                .as_ref(),
            [1u8, 4, b'o', b'p', b'u', b's']
        );
    }

    #[test]
    fn recorded_bytes_still_decode_to_what_they_were_recorded_from() {
        let decoded: WindowView = decode_frame(GOLDEN_WINDOW_VIEW).expect("golden frame decodes");
        assert_eq!(decoded, golden_window_view());
    }

    #[test]
    fn this_build_still_encodes_to_the_recorded_bytes() {
        let bytes = encode_frame(&golden_window_view()).expect("encode");
        assert_eq!(
            bytes.as_ref(),
            GOLDEN_WINDOW_VIEW,
            "the wire format moved; bump PROTOCOL_VERSION and update docs/protocol.md \
             before re-recording"
        );
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
                config_gen: 0,
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
            Request::GetConfig,
            Request::SetConfig(ConfigEdit::GlobalPollInterval(Some(120))),
            Request::SetConfig(ConfigEdit::GlobalPollInterval(None)),
            Request::SetConfig(ConfigEdit::ProviderPollInterval {
                provider: "claude".into(),
                secs: Some(30),
            }),
            Request::SetConfig(ConfigEdit::ProviderEnabled {
                provider: "claude".into(),
                enabled: false,
            }),
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
                windows: vec![WindowView {
                    window: sample_window(),
                    hint: RenderHint {
                        style: BarStyle::Percent,
                        warn_threshold: 0.8,
                        critical_threshold: 0.95,
                        note: Some("blocks entirely at cap".into()),
                    },
                    observed_start: Some(ObservedStart {
                        not_before: Utc::now(),
                        not_after: Utc::now(),
                    }),
                }],
                last_poll: Some(sample_event(PollOutcome::Success {
                    windows: vec![sample_window()],
                })),
                last_success: Some(Utc::now()),
                poll_interval_secs: 60,
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
                    prev_observed_at: Some(Utc::now()),
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
                accounts: vec![AccountHealth {
                    account: AccountId::from("claude:personal"),
                    consecutive_failures: 3,
                    last_error: Some("rate limited".into()),
                    last_poll_ts: Some(Utc::now()),
                    poll_interval_secs: 60,
                }],
                consecutive_failures: 3,
                last_error: Some("rate limited".into()),
            }]),
            Response::Ack,
            Response::Err(ErrorKind::UnknownProvider, "no such provider".into()),
            Response::Config(ConfigState {
                path: "/home/u/.config/teiryo/config.toml".into(),
                generation: 7,
                effective: ConfigView {
                    poll_interval_secs: Some(120),
                    default_poll_interval_secs: 60,
                    min_poll_interval_secs: 10,
                    providers: vec![ProviderSettings {
                        provider: "claude".into(),
                        enabled: true,
                        poll_interval_secs: Some(30),
                        effective_poll_interval_secs: 30,
                    }],
                },
                loaded_at: Utc::now(),
                warnings: vec!["unknown key `retrys` — ignored".into()],
                error: Some("poll_interval_secs must be at least 10".into()),
            }),
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
