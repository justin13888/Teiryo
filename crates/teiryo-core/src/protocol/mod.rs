//! Wire protocol: versioned handshake, bincode-encoded request/response
//! enums, and length-delimited framing.
//!
//! Connection lifecycle: raw [`handshake`] first (the 6-byte version preamble,
//! then each side's build commit; never bincode, so the version check can
//! never itself go stale), then [`codec`]-framed bincode frames carrying
//! [`wire::Request`] / [`wire::Response`].
//!
//! All of it runs over any byte stream ([`transport::Transport`]): the daemon's
//! Unix socket, or an ssh child's stdio whose far end is `teiryod bridge`
//! ([`ssh`]).

pub mod codec;
pub mod handshake;
pub mod ssh;
pub mod transport;
pub mod wire;

/// The bump guard: the whole wire format, hashed and pinned to the
/// `PROTOCOL_VERSION` it shipped under.
///
/// The golden fixtures in `handshake.rs` and `codec.rs` record a few values
/// byte for byte. This test covers everything else: one canonical value of
/// every variant of every enum that crosses the wire, the hello, the accept
/// and reject replies, and one frame's length prefix. Their encodings are
/// hashed into one fingerprint, and `(PROTOCOL_VERSION, fingerprint)` must
/// equal [`WIRE_FINGERPRINT`](wire_fingerprint::WIRE_FINGERPRINT). Change any
/// byte on the wire without bumping the version and it fails; bump the version
/// without repinning and it fails too, so the pair can only move together.
#[cfg(test)]
mod wire_fingerprint {
    use std::fmt::Debug;
    use std::time::Duration;

    use bytes::{Bytes, BytesMut};
    use chrono::{DateTime, TimeZone, Utc};
    use serde::de::{self, DeserializeOwned, Visitor};
    use serde::{Deserializer, Serialize};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::codec::Encoder;
    use ulid::Ulid;

    use super::codec::{encode_frame, length_delimited_codec, MAX_FRAME_LEN};
    use super::handshake::{
        encode_commit, hello_message, server_handshake, Hello, PROTOCOL_MAGIC, PROTOCOL_VERSION,
    };
    use super::wire::{
        AccountHealth, AccountStatus, ConfigEdit, ConfigState, ConfigView, HistoryPage,
        ProviderHealth, ProviderSettings, Request, Response, WindowView,
    };
    use crate::adapter::{BarStyle, RenderHint};
    use crate::domain::{
        Account, AccountId, ClientKind, PollEvent, PollId, PollOutcome, PollTrigger, QuotaSnapshot,
        QuotaUnit, QuotaWindow, ResetKind, WindowId, WindowScope,
    };
    use crate::error::ErrorKind;
    use crate::rollover::{ObservedStart, RolloverKind, WindowRollover};

    /// The protocol version and the wire fingerprint it was pinned with.
    ///
    /// **If the fingerprint test fails, the bytes on the wire changed.** Do
    /// not repin the hash on its own: bump `PROTOCOL_VERSION` by 1, update
    /// `docs/protocol.md` and the golden hello bytes in `handshake.rs`, and
    /// then repin both halves of this pair to what the failure prints.
    const WIRE_FINGERPRINT: (u16, u64) = (9, 0x45f8_706f_f871_faa5);

    /// 64-bit FNV-1a. Inline rather than a dependency: it only has to be
    /// deterministic across builds and platforms, not collision resistant.
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
            (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// Every encoding, each behind its own length, so that moving a byte from
    /// one encoding to the next still changes the hash.
    fn fingerprint(encodings: &[Vec<u8>]) -> u64 {
        let mut stream = Vec::new();
        for encoding in encodings {
            let len = u32::try_from(encoding.len()).expect("encoding fits a u32");
            stream.extend_from_slice(&len.to_le_bytes());
            stream.extend_from_slice(encoding);
        }
        fnv1a(&stream)
    }

    /// A deserializer that only answers one question: which variants does
    /// this enum declare? serde's derive passes the full list to
    /// `deserialize_enum`, so the canonical values below are checked against
    /// the enum itself rather than against a list kept by hand. That is what
    /// catches an appended variant, which leaves every existing encoding
    /// unchanged and still breaks an older peer that receives it.
    struct VariantNames<'a>(&'a mut &'static [&'static str]);

    impl<'de> Deserializer<'de> for VariantNames<'_> {
        type Error = de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not an enum"))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = variants;
            Err(de::Error::custom("variant names recorded"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct identifier ignored_any
        }
    }

    fn declared_variants<T: DeserializeOwned>() -> &'static [&'static str] {
        let mut names: &'static [&'static str] = &[];
        let _ = T::deserialize(VariantNames(&mut names));
        assert!(
            !names.is_empty(),
            "{} is not an enum",
            std::any::type_name::<T>()
        );
        names
    }

    /// The variant a value is, read off its derived `Debug`.
    fn variant_of<T: Debug>(value: &T) -> String {
        format!("{value:?}")
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    }

    /// Encode `values`, first asserting they are exactly one per variant `T`
    /// declares, in declaration order.
    fn every_variant<T: Serialize + DeserializeOwned + Debug>(values: &[T]) -> Vec<Vec<u8>> {
        let declared = declared_variants::<T>();
        let given: Vec<String> = values.iter().map(variant_of).collect();
        assert_eq!(
            given,
            declared,
            "{} needs one canonical value per variant, in declaration order. A \
             variant added, removed, or reordered is a wire change: add its \
             value here, bump PROTOCOL_VERSION, and repin WIRE_FINGERPRINT",
            std::any::type_name::<T>()
        );
        values
            .iter()
            .map(|value| encode_frame(value).expect("encode").to_vec())
            .collect()
    }

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 15, hour, minute, 0).unwrap()
    }

    fn poll_id(n: u128) -> PollId {
        PollId(Ulid(0x0198_aa00_0000_0000_0000_0000_0000_0000 + n))
    }

    fn account() -> AccountId {
        AccountId::from("claude:personal")
    }

    fn window() -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from("session_5h_opus"),
            label: "Opus — 5 hour".into(),
            scope: WindowScope::Model("opus".into()),
            reset_kind: ResetKind::Rolling(Duration::from_secs(5 * 3600)),
            unit: QuotaUnit::Hours,
            used: 42.5,
            limit: Some(40.0),
            reset_at: Some(at(12, 0)),
        }
    }

    fn event(trigger: PollTrigger, outcome: PollOutcome) -> PollEvent {
        PollEvent {
            id: poll_id(1),
            ts: at(7, 45),
            provider: "claude".into(),
            account: account(),
            trigger,
            outcome,
            latency_ms: 128,
        }
    }

    fn requests() -> Vec<Request> {
        vec![
            Request::Status {
                provider: Some("claude".into()),
                account: Some(account()),
            },
            Request::PollNow {
                provider: "claude".into(),
                account: Some(account()),
            },
            Request::AwaitUpdate {
                since: poll_id(1),
                config_gen: 7,
                timeout_ms: 30_000,
            },
            Request::History {
                account: account(),
                window: Some(WindowId::from("weekly_all")),
                since: at(0, 0),
                until: Some(at(12, 0)),
                max_points: Some(240),
            },
            Request::RecentPolls { limit: 50 },
            Request::Providers,
            Request::Shutdown,
            Request::GetConfig,
            Request::SetConfig(ConfigEdit::GlobalPollInterval(Some(120))),
        ]
    }

    fn responses() -> Vec<Response> {
        vec![
            Response::Status(vec![AccountStatus {
                account: Account {
                    id: account(),
                    provider: "claude".into(),
                    label: "personal".into(),
                },
                windows: vec![WindowView {
                    window: window(),
                    hint: RenderHint {
                        style: BarStyle::FractionOfLimit,
                        warn_threshold: 0.8,
                        critical_threshold: 0.95,
                        note: Some("blocks entirely at cap".into()),
                    },
                    observed_start: Some(ObservedStart {
                        not_before: at(7, 30),
                        not_after: at(7, 45),
                    }),
                }],
                last_poll: Some(event(
                    PollTrigger::Scheduled,
                    PollOutcome::Success {
                        windows: vec![window()],
                    },
                )),
                last_success: Some(at(7, 45)),
                poll_interval_secs: 60,
            }]),
            Response::PollAccepted {
                poll_id: poll_id(2),
            },
            Response::Update(event(
                PollTrigger::Manual {
                    client: ClientKind::Tui,
                },
                PollOutcome::RateLimited {
                    retry_after: Some(Duration::from_millis(60_500)),
                },
            )),
            Response::NoUpdate,
            Response::History(HistoryPage {
                snapshots: vec![QuotaSnapshot {
                    poll_id: poll_id(3),
                    ts: at(7, 45),
                    window: WindowId::from("session_5h_opus"),
                    label: "Opus — 5 hour".into(),
                    unit: QuotaUnit::Messages,
                    used: 61.0,
                    limit: Some(100.0),
                    reset_at: Some(at(12, 0)),
                }],
                earliest: Some(at(0, 0)),
                rollovers: vec![WindowRollover {
                    account: account(),
                    window: WindowId::from("session_5h_opus"),
                    poll: poll_id(4),
                    observed_at: at(7, 45),
                    kind: RolloverKind::Early,
                    prev_reset_at: Some(at(8, 0)),
                    new_reset_at: Some(at(12, 45)),
                    prev_used: 88.0,
                    new_used: 1.0,
                    prev_observed_at: Some(at(7, 30)),
                }],
            }),
            Response::RecentPolls(vec![event(
                PollTrigger::Startup,
                PollOutcome::AuthError("token expired".into()),
            )]),
            Response::Providers(vec![ProviderHealth {
                provider: "claude".into(),
                accounts: vec![AccountHealth {
                    account: account(),
                    consecutive_failures: 3,
                    last_error: Some("rate limited".into()),
                    last_poll_ts: Some(at(7, 45)),
                    poll_interval_secs: 60,
                }],
                consecutive_failures: 3,
                last_error: Some("rate limited".into()),
                discovery_error: Some("no Claude Code login in the Keychain".into()),
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
                loaded_at: at(7, 0),
                warnings: vec!["unknown key `retrys` — ignored".into()],
                error: Some("poll_interval_secs must be at least 10".into()),
            }),
        ]
    }

    /// The canonical build commits the handshake carries. Fixed rather than
    /// this build's own, which changes with every commit and would move the
    /// fingerprint with it.
    const CLIENT_COMMIT: &str = "0123456789ab-dirty";
    const DAEMON_COMMIT: &str = "ba9876543210";

    /// The bytes a daemon at this version replies with to `hello` followed by
    /// [`CLIENT_COMMIT`], read off the real [`server_handshake`] rather than
    /// restated.
    async fn daemon_reply(hello: Hello, len: usize) -> Vec<u8> {
        let (mut client, mut daemon) = tokio::io::duplex(64);
        let daemon_task = tokio::spawn(async move {
            let _ = server_handshake(&mut daemon, DAEMON_COMMIT).await;
        });
        let mut opening = hello.to_bytes().to_vec();
        opening.extend_from_slice(&encode_commit(CLIENT_COMMIT));
        client.write_all(&opening).await.unwrap();
        let mut reply = vec![0u8; len];
        client.read_exact(&mut reply).await.unwrap();
        daemon_task.await.unwrap();
        reply
    }

    /// Everything that reaches the wire, encoded, in a fixed order.
    async fn wire_encodings() -> Vec<Vec<u8>> {
        let mut encodings = vec![hello_message(CLIENT_COMMIT)];
        encodings.push(daemon_reply(Hello::current(), 3 + DAEMON_COMMIT.len()).await);
        let stale = Hello {
            magic: PROTOCOL_MAGIC,
            protocol_version: PROTOCOL_VERSION.wrapping_add(1),
        };
        encodings.push(daemon_reply(stale, 3).await);

        let mut frame = BytesMut::new();
        length_delimited_codec()
            .encode(Bytes::from_static(b"frame"), &mut frame)
            .expect("frame encodes");
        encodings.push(frame.to_vec());
        encodings.push((MAX_FRAME_LEN as u64).to_le_bytes().to_vec());

        encodings.extend(every_variant(&requests()));
        encodings.extend(every_variant(&responses()));
        // Every variant of every enum nested inside them, so that reordering
        // or appending one of those is seen as well.
        encodings.extend(every_variant(&[
            ConfigEdit::GlobalPollInterval(None),
            ConfigEdit::ProviderPollInterval {
                provider: "claude".into(),
                secs: Some(30),
            },
            ConfigEdit::ProviderEnabled {
                provider: "claude".into(),
                enabled: false,
            },
        ]));
        encodings.extend(every_variant(&[
            ErrorKind::UnknownProvider,
            ErrorKind::UnknownAccount,
            ErrorKind::BadRequest,
            ErrorKind::Storage,
            ErrorKind::Internal,
        ]));
        encodings.extend(every_variant(&[
            PollTrigger::Scheduled,
            PollTrigger::Manual {
                client: ClientKind::Tui,
            },
            PollTrigger::Startup,
        ]));
        encodings.extend(every_variant(&[
            ClientKind::Tui,
            ClientKind::Other("cli".into()),
        ]));
        encodings.extend(every_variant(&[
            PollOutcome::Success {
                windows: vec![window()],
            },
            PollOutcome::AuthError("token expired".into()),
            PollOutcome::NetworkError("connection refused".into()),
            PollOutcome::SchemaDrift("missing field".into()),
            PollOutcome::RateLimited { retry_after: None },
        ]));
        encodings.extend(every_variant(&[
            WindowScope::AccountWide,
            WindowScope::Model("opus".into()),
        ]));
        encodings.extend(every_variant(&[ResetKind::Rolling(Duration::from_secs(
            7 * 24 * 3600,
        ))]));
        encodings.extend(every_variant(&[
            QuotaUnit::Percent,
            QuotaUnit::Messages,
            QuotaUnit::Tokens,
            QuotaUnit::Hours,
        ]));
        encodings.extend(every_variant(&[
            BarStyle::Percent,
            BarStyle::FractionOfLimit,
            BarStyle::CountOnly,
        ]));
        encodings.extend(every_variant(&[
            RolloverKind::Scheduled,
            RolloverKind::Early,
            RolloverKind::Retracted,
            RolloverKind::Unannounced,
        ]));
        encodings
    }

    #[tokio::test]
    async fn the_wire_format_is_pinned_to_the_protocol_version() {
        let actual = (PROTOCOL_VERSION, fingerprint(&wire_encodings().await));
        assert_eq!(
            actual, WIRE_FINGERPRINT,
            "the wire format no longer matches the version it was pinned to.\n\
             Any change to the bytes on the wire bumps PROTOCOL_VERSION by 1 \
             (docs/protocol.md, \"Versioning policy\"). Bump PROTOCOL_VERSION \
             in handshake.rs, update docs/protocol.md and the golden hello \
             bytes, rerun this test, and only then repin WIRE_FINGERPRINT in \
             protocol/mod.rs to the pair it prints. This build's pair: \
             ({}, {:#018x})",
            actual.0, actual.1
        );
    }

    /// The guard is only as good as its ability to see a change: one byte
    /// different anywhere must move the fingerprint.
    #[tokio::test]
    async fn any_changed_byte_moves_the_fingerprint() {
        let encodings = wire_encodings().await;
        let pinned = fingerprint(&encodings);
        for i in 0..encodings.len() {
            for j in 0..encodings[i].len() {
                let mut changed = encodings.clone();
                changed[i][j] ^= 0x01;
                assert_ne!(fingerprint(&changed), pinned, "encoding {i}, byte {j}");
            }
            let mut appended = encodings.clone();
            appended[i].push(0);
            assert_ne!(fingerprint(&appended), pinned, "encoding {i}, appended");
        }
    }

    #[test]
    fn variant_names_are_read_off_the_enum() {
        assert_eq!(
            declared_variants::<QuotaUnit>(),
            ["Percent", "Messages", "Tokens", "Hours"]
        );
        assert_eq!(
            variant_of(&Request::RecentPolls { limit: 1 }),
            "RecentPolls"
        );
        assert_eq!(variant_of(&Response::Ack), "Ack");
    }
}
