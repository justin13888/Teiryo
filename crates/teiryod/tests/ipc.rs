//! End-to-end IPC test: a real UnixListener, a stub provider adapter, and a
//! client speaking the actual handshake + framed bincode protocol.

use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use secrecy::SecretString;
use teiryo_core::{
    client_handshake, decode_frame, encode_frame, framed, Account, AccountId, AuthError,
    Authenticator, BarStyle, ConfigEdit, ConfigState, Credential, ErrorKind, ParseError, PollId,
    PollOutcome, PollTrigger, ProbeError, Prober, ProviderAdapter, QuotaParser, QuotaUnit,
    QuotaWindow, RawResponse, RenderHint, Request, ResetKind, Response, Storage, WindowId,
    WindowPresenter, WindowScope, PROTOCOL_MAGIC,
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
            // Distinctive so the Status assertion proves the hint really
            // crosses the wire rather than being reconstructed client-side.
            note: Some("stub caveat".to_owned()),
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

/// A private directory holding this test's `config.toml`, seeded with `body`.
/// Its own directory because the daemon watches the *parent*, and a shared one
/// would make concurrently running tests wake each other's watchers.
fn test_config(body: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("teiryod-test-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, body).unwrap();
    path
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

type Sink = futures::stream::SplitSink<
    tokio_util::codec::Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    bytes::Bytes,
>;
type Source = futures::stream::SplitStream<
    tokio_util::codec::Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
>;

/// Long-poll until a config reload newer than `config_gen` arrives, skipping
/// polls that land first — one `AwaitUpdate` serves both clocks, so which one
/// fires first is a genuine race and not something to assert on.
async fn await_config(
    sink: &mut Sink,
    source: &mut Source,
    since: &mut PollId,
    config_gen: u64,
) -> ConfigState {
    loop {
        send_request(
            sink,
            &Request::AwaitUpdate {
                since: *since,
                config_gen,
                // Generous: the watcher debounces before re-reading.
                timeout_ms: 10_000,
            },
        )
        .await;
        match recv_response(source).await {
            Response::Config(state) => return state,
            Response::Update(event) => *since = event.id,
            other => panic!("expected a config wake-up, got {other:?}"),
        }
    }
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
        let config = test_config("poll_interval_secs = 3600\n");

        let server =
            tokio::task::spawn_local(teiryod::run(listener, storage, adapters, config.clone()));

        // Connect and handshake.
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        client_handshake(&mut stream).await.expect("handshake");
        let (mut sink, mut source) = framed(stream).split();

        // The startup poll arrives via AwaitUpdate from PollId::zero().
        send_request(
            &mut sink,
            &Request::AwaitUpdate {
                since: PollId::zero(),
                config_gen: u64::MAX,
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
        //
        // Spaced off the startup poll first: `AwaitUpdate { since }` orders by
        // ULID, and two ids minted inside one millisecond tie-break randomly,
        // so without this the manual poll can sort *below* `since` and the
        // wait times out instead of delivering it.
        tokio::time::sleep(Duration::from_millis(5)).await;
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
                config_gen: u64::MAX,
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
        std::fs::remove_dir_all(config.parent().unwrap()).ok();
    });
}

/// Settings over the wire, end to end: read them, change them, watch the
/// change reach the scheduler and the file, and see a bad hand edit reported
/// without disturbing what is running.
#[test]
fn settings_round_trip_and_hot_reload() {
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
        let config = test_config("# hand-written\npoll_interval_secs = 3600\n");

        let server =
            tokio::task::spawn_local(teiryod::run(listener, storage, adapters, config.clone()));

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        client_handshake(&mut stream).await.expect("handshake");
        let (mut sink, mut source) = framed(stream).split();

        // What the file says, plus the bounds a client needs to stay inside.
        send_request(&mut sink, &Request::GetConfig).await;
        let initial = match recv_response(&mut source).await {
            Response::Config(state) => state,
            other => panic!("expected Config, got {other:?}"),
        };
        assert_eq!(initial.path, config.to_string_lossy());
        assert_eq!(initial.effective.poll_interval_secs, Some(3600));
        assert_eq!(
            initial.effective.default_poll_interval_secs,
            teiryod::config::DEFAULT_POLL_INTERVAL.as_secs() as u32
        );
        assert_eq!(initial.effective.min_poll_interval_secs, 10);
        assert_eq!(initial.error, None);
        // A compiled-in provider gets a row even with nothing in the file, or
        // there would be no way to configure it from a client.
        let stub = &initial.effective.providers[0];
        assert_eq!(stub.provider, "stub");
        assert!(stub.enabled);
        assert_eq!(stub.poll_interval_secs, None);
        assert_eq!(stub.effective_poll_interval_secs, 3600);

        // A change reaches the reply, the file, and the scheduler alike.
        send_request(
            &mut sink,
            &Request::SetConfig(ConfigEdit::GlobalPollInterval(Some(30))),
        )
        .await;
        let applied = match recv_response(&mut source).await {
            Response::Config(state) => state,
            other => panic!("expected Config, got {other:?}"),
        };
        assert_eq!(applied.effective.poll_interval_secs, Some(30));
        assert!(applied.generation > initial.generation);
        let on_disk = std::fs::read_to_string(&config).unwrap();
        assert!(on_disk.contains("poll_interval_secs = 30"), "{on_disk}");
        assert!(
            on_disk.contains("# hand-written"),
            "comment lost: {on_disk}"
        );
        send_request(
            &mut sink,
            &Request::Status {
                provider: None,
                account: None,
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::Status(statuses) => assert_eq!(statuses[0].poll_interval_secs, 30),
            other => panic!("expected Status, got {other:?}"),
        }

        // Below the floor is refused, and refusing must not touch the file.
        let before = std::fs::read_to_string(&config).unwrap();
        send_request(
            &mut sink,
            &Request::SetConfig(ConfigEdit::GlobalPollInterval(Some(5))),
        )
        .await;
        match recv_response(&mut source).await {
            Response::Err(ErrorKind::BadRequest, message) => {
                assert!(message.contains("at least 10"), "{message}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);

        // Disabling parks the provider: the reported cadence goes to zero, and
        // a manual poll says why rather than silently queueing.
        send_request(
            &mut sink,
            &Request::SetConfig(ConfigEdit::ProviderEnabled {
                provider: "stub".into(),
                enabled: false,
            }),
        )
        .await;
        let disabled = match recv_response(&mut source).await {
            Response::Config(state) => state,
            other => panic!("expected Config, got {other:?}"),
        };
        assert!(!disabled.effective.providers[0].enabled);
        send_request(
            &mut sink,
            &Request::Status {
                provider: None,
                account: None,
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::Status(statuses) => assert_eq!(statuses[0].poll_interval_secs, 0),
            other => panic!("expected Status, got {other:?}"),
        }
        send_request(
            &mut sink,
            &Request::PollNow {
                provider: "stub".into(),
                account: None,
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::Err(ErrorKind::BadRequest, message) => {
                assert!(message.contains("disabled"), "{message}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }

        // The whole hot-reload contract: someone edits the file badly, the
        // daemon wakes the long-poll to say so, and what is running does not
        // change.
        let running = disabled.effective.clone();
        let mut since = PollId::zero();
        std::fs::write(&config, "poll_interval_secs = -1\n").unwrap();
        let rejected = await_config(&mut sink, &mut source, &mut since, disabled.generation).await;
        assert!(rejected.error.is_some(), "a bad file must be reported");
        assert_eq!(
            rejected.effective, running,
            "a rejected file must not change what is running"
        );

        // And a good edit from outside takes effect.
        std::fs::write(&config, "poll_interval_secs = 45\n").unwrap();
        let reloaded = await_config(&mut sink, &mut source, &mut since, rejected.generation).await;
        assert_eq!(reloaded.error, None);
        assert_eq!(reloaded.effective.poll_interval_secs, Some(45));
        // Re-enabled, because the new file drops the disable.
        assert!(reloaded.effective.providers[0].enabled);

        send_request(&mut sink, &Request::Shutdown).await;
        assert!(matches!(recv_response(&mut source).await, Response::Ack));
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server stopped")
            .expect("join");

        std::fs::remove_file(&socket).ok();
        std::fs::remove_dir_all(config.parent().unwrap()).ok();
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
        let _server =
            tokio::task::spawn_local(teiryod::run(listener, storage, adapters, test_config("")));

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

/// An adapter whose window rolls over *early* on its second poll: the first
/// reading's reset is two hours out, and the next one replaces it anyway.
struct RollingAdapter {
    /// Atomic rather than a `Cell`: the adapter traits are `Sync`.
    polls: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl Authenticator for RollingAdapter {
    async fn discover_accounts(&self) -> Result<Vec<Account>, AuthError> {
        Ok(vec![stub_account()])
    }
    async fn credential_for(&self, _account: &Account) -> Result<Credential, AuthError> {
        Ok(Credential::ApiKey(SecretString::from("stub-key")))
    }
}

#[async_trait]
impl Prober for RollingAdapter {
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

impl QuotaParser for RollingAdapter {
    fn parse(&self, _raw: &RawResponse) -> Result<Vec<QuotaWindow>, ParseError> {
        let nth = self
            .polls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = chrono::Utc::now();
        let mut window = stub_window();
        if nth == 0 {
            window.used = 88.0;
            window.reset_at = Some(now + chrono::Duration::hours(2));
        } else {
            window.used = 1.0;
            window.reset_at = Some(now + chrono::Duration::hours(7));
        }
        Ok(vec![window])
    }
}

impl WindowPresenter for RollingAdapter {
    fn render_hint(&self, _window: &QuotaWindow) -> RenderHint {
        RenderHint {
            style: BarStyle::Percent,
            warn_threshold: 0.8,
            critical_threshold: 0.95,
            note: None,
        }
    }
    fn group_order(&self) -> &[WindowId] {
        &[]
    }
}

impl ProviderAdapter for RollingAdapter {
    fn id(&self) -> String {
        "stub".into()
    }
}

/// A window that resets before the provider said it would is recorded, and
/// reaches the client on the same history page as the series it annotates.
#[test]
fn an_early_rollover_reaches_the_client_with_its_history() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async {
        let socket = test_socket_path();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let adapters: Vec<Rc<dyn ProviderAdapter>> = vec![Rc::new(RollingAdapter {
            polls: std::sync::atomic::AtomicU32::new(0),
        })];
        let config = test_config("poll_interval_secs = 3600\n");
        let server =
            tokio::task::spawn_local(teiryod::run(listener, storage, adapters, config.clone()));

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        client_handshake(&mut stream).await.expect("handshake");
        let (mut sink, mut source) = framed(stream).split();

        // Poll 1 — the startup poll, which has nothing to compare against.
        send_request(
            &mut sink,
            &Request::AwaitUpdate {
                since: PollId::zero(),
                config_gen: u64::MAX,
                timeout_ms: 5_000,
            },
        )
        .await;
        let first = match recv_response(&mut source).await {
            Response::Update(event) => event,
            other => panic!("expected startup Update, got {other:?}"),
        };

        // Poll 2 — the reset moves while the old one was still two hours out.
        //
        // Spaced off the startup poll for the ULID reason noted above.
        tokio::time::sleep(Duration::from_millis(5)).await;
        send_request(
            &mut sink,
            &Request::PollNow {
                provider: "stub".into(),
                account: None,
            },
        )
        .await;
        assert!(matches!(
            recv_response(&mut source).await,
            Response::PollAccepted { .. }
        ));
        send_request(
            &mut sink,
            &Request::AwaitUpdate {
                since: first.id,
                config_gen: u64::MAX,
                timeout_ms: 5_000,
            },
        )
        .await;
        let second = match recv_response(&mut source).await {
            Response::Update(event) => event,
            other => panic!("expected manual Update, got {other:?}"),
        };

        send_request(
            &mut sink,
            &Request::History {
                account: stub_account().id,
                window: Some(WindowId::from("session")),
                since: chrono::Utc::now() - chrono::Duration::hours(1),
                until: None,
                max_points: None,
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::History(page) => {
                assert!(page.snapshots.len() >= 2, "both readings are stored");
                assert_eq!(page.rollovers.len(), 1, "one boundary, on one page");
                let rollover = &page.rollovers[0];
                assert_eq!(rollover.kind, teiryo_core::RolloverKind::Early);
                assert!(rollover.kind.is_surprise() && rollover.kind.is_boundary());
                assert_eq!(rollover.window, WindowId::from("session"));
                assert_eq!(rollover.account, stub_account().id);
                // Attributed to the poll that revealed it, with both readings.
                assert_eq!(rollover.poll, second.id);
                assert_eq!(rollover.prev_used, 88.0);
                assert_eq!(rollover.new_used, 1.0);
                assert!(rollover.new_reset_at > rollover.prev_reset_at);
            }
            other => panic!("expected History, got {other:?}"),
        }

        // A window the rollover does not belong to must not inherit it.
        send_request(
            &mut sink,
            &Request::History {
                account: stub_account().id,
                window: Some(WindowId::from("nope")),
                since: chrono::Utc::now() - chrono::Duration::hours(1),
                until: None,
                max_points: None,
            },
        )
        .await;
        match recv_response(&mut source).await {
            Response::History(page) => assert!(page.rollovers.is_empty()),
            other => panic!("expected History, got {other:?}"),
        }

        send_request(&mut sink, &Request::Shutdown).await;
        let _ = recv_response(&mut source).await;
        let _ = server.await;
        std::fs::remove_file(&socket).ok();
        std::fs::remove_dir_all(config.parent().unwrap()).ok();
    });
}
