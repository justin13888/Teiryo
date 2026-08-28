//! End-to-end IPC test: a real UnixListener, a stub provider adapter, and a
//! client speaking the actual handshake + framed bincode protocol.

use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use secrecy::SecretString;
use teiryo_core::{
    client_handshake, decode_frame, encode_frame, framed, Account, AccountId, AuthError,
    Authenticator, BarStyle, Credential, ParseError, PollId, PollOutcome, PollTrigger, ProbeError,
    Prober, ProviderAdapter, QuotaParser, QuotaUnit, QuotaWindow, RawResponse, RenderHint, Request,
    ResetKind, Response, Storage, WindowId, WindowPresenter, WindowScope, PROTOCOL_MAGIC,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

struct StubAdapter;

fn stub_account() -> Account {
    Account {
        id: AccountId::from("stub:one"),
        provider: "stub".into(),
        label: "one".into(),
    }
}

fn stub_window() -> QuotaWindow {
    QuotaWindow {
        id: WindowId::from("session"),
        label: "Session".into(),
        scope: WindowScope::AccountWide,
        reset_kind: ResetKind::Rolling(Duration::from_secs(5 * 3600)),
        unit: QuotaUnit::Percent,
        used: 37.0,
        limit: None,
        reset_at: None,
    }
}

#[async_trait]
impl Authenticator for StubAdapter {
    async fn discover_accounts(&self) -> Result<Vec<Account>, AuthError> {
        Ok(vec![stub_account()])
    }
    async fn credential_for(&self, _account: &Account) -> Result<Credential, AuthError> {
        Ok(Credential::ApiKey(SecretString::from("stub-key")))
    }
}

#[async_trait]
impl Prober for StubAdapter {
    async fn probe(
        &self,
        _account: &Account,
        _cred: &Credential,
    ) -> Result<RawResponse, ProbeError> {
        Ok(RawResponse {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
            fetched_at: chrono::Utc::now(),
        })
    }
}

impl QuotaParser for StubAdapter {
    fn parse(&self, _raw: &RawResponse) -> Result<Vec<QuotaWindow>, ParseError> {
        Ok(vec![stub_window()])
    }
}

impl WindowPresenter for StubAdapter {
    fn render_hint(&self, _window: &QuotaWindow) -> RenderHint {
        RenderHint {
            style: BarStyle::Percent,
            warn_threshold: 0.8,
            critical_threshold: 0.95,
            note: Some("stub caveat".into()),
        }
    }
    fn group_order(&self) -> &[WindowId] {
        &[]
    }
}

impl ProviderAdapter for StubAdapter {
    fn id(&self) -> String {
        "stub".into()
    }
}

fn test_socket_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("teiryod-test-{}.sock", ulid::Ulid::new()))
}

async fn send_request(
    framed: &mut futures::stream::SplitSink<
        tokio_util::codec::Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
        bytes::Bytes,
    >,
    request: &Request,
) {
    framed.send(encode_frame(request).unwrap()).await.unwrap();
}

async fn recv_response(
    framed: &mut futures::stream::SplitStream<
        tokio_util::codec::Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    >,
) -> Response {
    let frame = framed.next().await.expect("frame").expect("io");
    decode_frame(&frame).expect("decode")
}

#[test]
fn full_ipc_roundtrip() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async {
        let socket = test_socket_path();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let adapters: Vec<Rc<dyn ProviderAdapter>> = vec![Rc::new(StubAdapter)];
        // Long base interval: only the startup poll and manual polls fire.
        let config: teiryod::Config = toml::from_str("poll_interval_secs = 3600").unwrap();

        let server = tokio::task::spawn_local(teiryod::run(listener, storage, adapters, config));

        // Connect and handshake.
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        client_handshake(&mut stream).await.expect("handshake");
        let (mut sink, mut source) = framed(stream).split();

        // The startup poll arrives via AwaitUpdate from PollId::zero().
        send_request(
            &mut sink,
            &Request::AwaitUpdate {
                since: PollId::zero(),
                timeout_ms: 5_000,
            },
        )
        .await;
        let startup = match recv_response(&mut source).await {
            Response::Update(event) => event,
            other => panic!("expected startup Update, got {other:?}"),
        };
        assert_eq!(startup.provider, "stub");
        assert_eq!(startup.trigger, PollTrigger::Startup);
        assert!(matches!(startup.outcome, PollOutcome::Success { .. }));

        // Status now shows the account with its windows.
        send_request(
            &mut sink,
            &Request::Status {
                provider: None,
                account: None,
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::Status(statuses) => {
                assert_eq!(statuses.len(), 1);
                assert_eq!(statuses[0].account, stub_account());
                assert_eq!(statuses[0].windows.len(), 1);
                assert_eq!(statuses[0].windows[0].window, stub_window());
                // The adapter's render hint reaches the client intact.
                assert_eq!(
                    statuses[0].windows[0].hint.note.as_deref(),
                    Some("stub caveat")
                );
                assert_eq!(statuses[0].windows[0].hint.critical_threshold, 0.95);
                assert_eq!(statuses[0].last_poll.as_ref().unwrap().id, startup.id);
                // The successful poll backing those windows is timestamped
                // separately from `last_poll`, and the cadence is exposed.
                assert_eq!(statuses[0].last_success, Some(startup.ts));
                assert_eq!(statuses[0].poll_interval_secs, 3600);
            }
            other => panic!("expected Status, got {other:?}"),
        }

        // Manual poll: PollAccepted echoes the newest published id; the
        // triggered poll then arrives via AwaitUpdate { since: that id }.
        send_request(
            &mut sink,
            &Request::PollNow {
                provider: "stub".into(),
                account: None,
            },
        )
        .await;
        let since = match recv_response(&mut source).await {
            Response::PollAccepted { poll_id } => {
                assert_eq!(poll_id, startup.id);
                poll_id
            }
            other => panic!("expected PollAccepted, got {other:?}"),
        };
        send_request(
            &mut sink,
            &Request::AwaitUpdate {
                since,
                timeout_ms: 5_000,
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::Update(event) => {
                assert!(event.id > since);
                assert!(matches!(event.trigger, PollTrigger::Manual { .. }));
            }
            other => panic!("expected manual Update, got {other:?}"),
        }

        // Unknown provider is a clean error.
        send_request(
            &mut sink,
            &Request::PollNow {
                provider: "nope".into(),
                account: None,
            },
        )
        .await;
        assert!(matches!(
            recv_response(&mut source).await,
            Response::Err(teiryo_core::ErrorKind::UnknownProvider, _)
        ));

        // History and RecentPolls reach storage.
        send_request(
            &mut sink,
            &Request::History {
                account: stub_account().id,
                window: None,
                since: chrono::Utc::now() - chrono::Duration::hours(1),
                until: None,
                max_points: None,
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::History(page) => {
                assert!(page.snapshots.len() >= 2);
                // The page also says where the whole series starts, which is
                // what lets a client stop scrolling at the end of the data
                // rather than probing for it.
                let earliest = page.earliest.expect("stored snapshots have a start");
                assert!(page.snapshots.iter().all(|s| s.ts >= earliest));
            }
            other => panic!("expected History, got {other:?}"),
        }

        // The same query, downsampled: at most one point per window, and it
        // must be the newest reading rather than an arbitrary bucket peak.
        send_request(
            &mut sink,
            &Request::History {
                account: stub_account().id,
                window: Some(WindowId::from("session")),
                since: chrono::Utc::now() - chrono::Duration::hours(1),
                until: None,
                max_points: Some(1),
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::History(page) => {
                assert_eq!(page.snapshots.len(), 1);
                assert_eq!(page.snapshots[0].window, WindowId::from("session"));
                // Downsampling bounds the page, never the reported extent.
                assert!(page.earliest.is_some());
            }
            other => panic!("expected bounded History, got {other:?}"),
        }
        send_request(&mut sink, &Request::RecentPolls { limit: 10 }).await;
        match recv_response(&mut source).await {
            Response::RecentPolls(events) => assert!(events.len() >= 2),
            other => panic!("expected RecentPolls, got {other:?}"),
        }

        // Provider health.
        send_request(&mut sink, &Request::Providers).await;
        match recv_response(&mut source).await {
            Response::Providers(health) => {
                assert_eq!(health.len(), 1);
                assert_eq!(health[0].provider, "stub");
                assert_eq!(health[0].consecutive_failures, 0);
                // Per-account rows, not just the provider-wide rollup.
                assert_eq!(health[0].accounts.len(), 1);
                let account = &health[0].accounts[0];
                assert_eq!(account.account, stub_account().id);
                assert_eq!(account.consecutive_failures, 0);
                assert_eq!(account.last_error, None);
                assert_eq!(account.poll_interval_secs, 3600);
                assert!(account.last_poll_ts.is_some());
            }
            other => panic!("expected Providers, got {other:?}"),
        }

        // Shutdown: Ack, then the server future completes.
        send_request(&mut sink, &Request::Shutdown).await;
        assert!(matches!(recv_response(&mut source).await, Response::Ack));
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server stopped")
            .expect("join");

        std::fs::remove_file(&socket).ok();
    });
}

#[test]
fn version_mismatch_is_rejected_with_0x01() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async {
        let socket = test_socket_path();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let adapters: Vec<Rc<dyn ProviderAdapter>> = vec![];
        let _server = tokio::task::spawn_local(teiryod::run(
            listener,
            storage,
            adapters,
            teiryod::Config::default(),
        ));

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        // Hand-rolled hello with a future protocol version.
        let mut hello = [0u8; 6];
        hello[..4].copy_from_slice(&PROTOCOL_MAGIC);
        hello[4..].copy_from_slice(&999u16.to_le_bytes());
        stream.write_all(&hello).await.unwrap();
        let mut reply = [0u8; 1];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x01);
        // Daemon closes the connection after rejecting.
        let mut rest = Vec::new();
        let n = stream.read_to_end(&mut rest).await.unwrap();
        assert_eq!(n, 0);

        std::fs::remove_file(&socket).ok();
    });
}
