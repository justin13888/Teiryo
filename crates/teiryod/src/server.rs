//! IPC server: handshake-gated, length-delimited bincode frames over UDS.

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use teiryo_core::{
    decode_frame, encode_frame, framed, server_handshake, AccountId, ClientKind, ErrorKind,
    HistoryPage, PollEvent, PollId, PollTrigger, Request, Response,
};
use tokio::net::{UnixListener, UnixStream};

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
        Request::AwaitUpdate { since, timeout_ms } => await_update(daemon, since, timeout_ms).await,
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
    drop(st);
    if !provider_known {
        return Response::Err(
            ErrorKind::UnknownProvider,
            format!("no provider {provider:?}"),
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

/// Resolve immediately if a poll newer than `since` is already published,
/// otherwise wait for the next one, bounded by `timeout_ms`.
async fn await_update(daemon: &Daemon, since: PollId, timeout_ms: u32) -> Response {
    let mut rx = daemon.watch_tx.subscribe();
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
        match tokio::time::timeout_at(deadline, rx.changed()).await {
            Ok(Ok(())) => continue,
            _ => return Response::NoUpdate, // timeout or daemon dropped
        }
    }
}
