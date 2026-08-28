//! IPC server: handshake-gated, length-delimited bincode frames over UDS.

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use teiryo_core::{
    decode_frame, encode_frame, framed, server_handshake, AccountId, ClientKind, ConfigEdit,
    ErrorKind, HistoryPage, PollEvent, PollId, PollTrigger, Request, Response,
};
use tokio::net::{UnixListener, UnixStream};

use crate::config;
use crate::state::Daemon;

/// Accept connections until shutdown. Must run inside a `LocalSet`.
pub async fn serve(listener: UnixListener, daemon: Daemon) {
    let mut shutdown_rx = daemon.shutdown_tx.subscribe();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let daemon = daemon.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(e) = handle_conn(stream, daemon).await {
                            tracing::debug!(error = %e, "connection ended with error");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            },
            _ = shutdown_rx.changed() => break,
        }
    }
}

/// One client connection: handshake first, then a request/response loop.
/// A failed handshake or a malformed frame ends only this connection.
async fn handle_conn(mut stream: UnixStream, daemon: Daemon) -> anyhow::Result<()> {
    if let Err(e) = server_handshake(&mut stream).await {
        tracing::info!(error = %e, "handshake rejected");
        return Ok(());
    }
    let mut framed = framed(stream);
    let mut shutdown_rx = daemon.shutdown_tx.subscribe();
    loop {
        let frame = tokio::select! {
            frame = framed.next() => match frame {
                Some(f) => f?,
                None => return Ok(()), // client hung up
            },
            _ = shutdown_rx.changed() => return Ok(()),
        };
        let response = match decode_frame::<Request>(&frame) {
            Ok(request) => {
                let shutdown = matches!(request, Request::Shutdown);
                let response = handle_request(request, &daemon).await;
                framed.send(encode_frame(&response)?).await?;
                if shutdown {
                    daemon.shutdown_tx.send_replace(true);
                    return Ok(());
                }
                continue;
            }
            Err(e) => Response::Err(ErrorKind::BadRequest, format!("undecodable request: {e}")),
        };
        framed.send(encode_frame(&response)?).await?;
    }
}

/// Dispatch one request. Only `AwaitUpdate` actually awaits.
async fn handle_request(request: Request, daemon: &Daemon) -> Response {
    match request {
        Request::Status { provider, account } => {
            Response::Status(daemon.status(provider.as_ref(), account.as_ref()))
        }
        Request::PollNow { provider, account } => poll_now(daemon, &provider, account.as_ref()),
        Request::AwaitUpdate {
            since,
            config_gen,
            timeout_ms,
        } => await_update(daemon, since, config_gen, timeout_ms).await,
        Request::History {
            account,
            window,
            since,
            until,
            max_points,
        } => {
            let st = daemon.state.borrow();
            let page = st
                .storage
                .history(&account, window.as_ref(), since, until, max_points)
                .and_then(|snapshots| {
                    // Where the series starts is a property of the stored
                    // history, not of the slice asked for, so it is queried
                    // separately — a client scrolling back through time has no
                    // other way to know when to stop.
                    let earliest = st.storage.earliest_snapshot(&account, window.as_ref())?;
                    // The same interval the series covers, resolving `until:
                    // None` the way `history` already did, so the boundaries
                    // and the points they annotate can never disagree.
                    let rollovers = st.storage.rollovers(
                        &account,
                        window.as_ref(),
                        since,
                        until.unwrap_or_else(Utc::now),
                    )?;
                    Ok(HistoryPage {
                        snapshots,
                        earliest,
                        rollovers,
                    })
                });
            match page {
                Ok(page) => Response::History(page),
                Err(e) => Response::Err(ErrorKind::Storage, e.to_string()),
            }
        }
        Request::RecentPolls { limit } => {
            let st = daemon.state.borrow();
            match st.storage.recent_polls(limit) {
                Ok(events) => Response::RecentPolls(events),
                Err(e) => Response::Err(ErrorKind::Storage, e.to_string()),
            }
        }
        Request::Providers => Response::Providers(daemon.provider_health()),
        Request::Shutdown => Response::Ack,
        Request::GetConfig => Response::Config(daemon.config_state()),
        Request::SetConfig(edit) => set_config(daemon, &edit),
    }
}

/// Write one settings change to `config.toml` and apply it.
///
/// The daemon writes the file rather than the client so that validation, the
/// write, and the apply happen in one place — and so the client learns about a
/// rejection as a reply rather than having to notice it later.
fn set_config(daemon: &Daemon, edit: &ConfigEdit) -> Response {
    let path = std::path::PathBuf::from(&daemon.config_state().path);
    let text = match config::write_edit(&path, edit) {
        Ok(text) => text,
        Err(e) => return Response::Err(ErrorKind::BadRequest, e.to_string()),
    };
    // Apply straight away instead of leaving it to the watcher: the reply
    // should describe settings that are already in effect, not ones that will
    // be a filesystem event later.
    match config::parse(&text) {
        Ok(loaded) => {
            for warning in &loaded.warnings {
                tracing::warn!(config = %path.display(), "{warning}");
            }
            daemon.apply_config(loaded);
            Response::Config(daemon.config_state())
        }
        // Only reachable if the file was already invalid for a reason the edit
        // did not touch, since `write_edit` validates what it writes.
        Err(e) => {
            daemon.reject_config(e.to_string());
            Response::Err(ErrorKind::BadRequest, e.to_string())
        }
    }
}

/// Inject a manual trigger into every matching poll task. The returned
/// `poll_id` echoes the newest already-published poll id, so the client can
/// `AwaitUpdate { since: poll_id }` and receive exactly the polls this
/// request caused (or anything newer).
fn poll_now(daemon: &Daemon, provider: &str, account: Option<&AccountId>) -> Response {
    let st = daemon.state.borrow();
    let targets: Vec<_> = st
        .pollers
        .iter()
        .filter(|((p, _), _)| p == provider)
        .filter(|((_, a), _)| account.is_none_or(|id| a == id))
        .map(|(_, tx)| tx.clone())
        .collect();
    let provider_known = st.pollers.keys().any(|(p, _)| p == provider);
    let enabled = st.config.provider_enabled(provider);
    drop(st);
    if !provider_known {
        return Response::Err(
            ErrorKind::UnknownProvider,
            format!("no provider {provider:?}"),
        );
    }
    // A disabled provider's poll tasks are parked, so a trigger sent now would
    // sit in the channel until it was re-enabled and then fire as a surprise.
    // Refusing says what actually happened.
    if !enabled {
        return Response::Err(
            ErrorKind::BadRequest,
            format!("provider {provider:?} is disabled in config"),
        );
    }
    if targets.is_empty() {
        return Response::Err(
            ErrorKind::UnknownAccount,
            format!("no matching account on {provider:?}"),
        );
    }
    for tx in targets {
        let _ = tx.send(PollTrigger::Manual {
            client: ClientKind::Tui,
        });
    }
    Response::PollAccepted {
        poll_id: daemon.newest_poll_id(),
    }
}

/// Resolve immediately if a poll newer than `since` or a config load newer
/// than `config_gen` is already published, otherwise wait for whichever comes
/// first, bounded by `timeout_ms`.
///
/// Both of the daemon's clocks share one long-poll so the client needs neither
/// a third connection nor a timer of its own — the daemon stays the only clock
/// that matters.
async fn await_update(
    daemon: &Daemon,
    since: PollId,
    config_gen: u64,
    timeout_ms: u32,
) -> Response {
    let mut rx = daemon.watch_tx.subscribe();
    let mut config_rx = daemon.config_tx.subscribe();
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.into());
    loop {
        let newer: Option<PollEvent> = rx
            .borrow_and_update()
            .as_ref()
            .filter(|e| e.id > since)
            .cloned();
        if let Some(event) = newer {
            return Response::Update(event);
        }
        if *config_rx.borrow_and_update() > config_gen {
            return Response::Config(daemon.config_state());
        }
        let woken = tokio::time::timeout_at(deadline, async {
            tokio::select! {
                r = rx.changed() => r,
                r = config_rx.changed() => r,
            }
        })
        .await;
        match woken {
            Ok(Ok(())) => continue,
            _ => return Response::NoUpdate, // timeout or daemon dropped
        }
    }
}
