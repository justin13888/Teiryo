//! `teiryo` — thin TUI client for the Teiryo daemon. No HTTP, no DB, no
//! scheduling: it only speaks the Unix-socket wire protocol.

mod app;
mod client;
mod spawn;
mod ui;

use chrono::Utc;
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use teiryo_core::protocol::handshake::PROTOCOL_VERSION;
use teiryo_core::{ErrorKind, Request, Response};

use app::{Action, App, View};
use client::{is_version_mismatch, newest_poll_id, spawn_update_loop, Client, NetEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match run().await {
        Ok(()) => {}
        Err(err) => {
            eprintln!("teiryo: {err}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Connect (spawning the daemon if needed) before touching the terminal,
    // so connection errors print normally.
    let mut command = match Client::connect().await {
        Ok(c) => c,
        Err(e) if is_version_mismatch(&e) => {
            eprintln!(
                "teiryo: the running daemon speaks a different protocol version \
                 (this client is v{PROTOCOL_VERSION}).\n\
                 Stop the stale daemon (e.g. `pkill teiryod`) and relaunch — \
                 teiryo will respawn the matching daemon automatically."
            );
            std::process::exit(2);
        }
        Err(e) => return Err(e.into()),
    };

    let mut app = App::new();
    refresh_status(&mut command, &mut app).await;

    let (net_tx, mut net_rx) = mpsc::unbounded_channel();
    let mut update_task = start_update_loop(&app, net_tx.clone()).await;

    let terminal = ratatui::init();
    let result = event_loop(terminal, &mut app, &mut command, &mut net_rx, &net_tx).await;
    ratatui::restore();
    if let Some(task) = update_task.take() {
        task.abort();
    }
    result
}

/// Open the dedicated update connection and start the long-poll loop.
async fn start_update_loop(
    app: &App,
    tx: mpsc::UnboundedSender<NetEvent>,
) -> Option<JoinHandle<()>> {
    match Client::connect().await {
        Ok(client) => Some(spawn_update_loop(client, newest_poll_id(&app.statuses), tx)),
        Err(e) => {
            let _ = tx.send(NetEvent::Disconnected(e.to_string()));
            None
        }
    }
}

async fn event_loop(
    mut terminal: ratatui::DefaultTerminal,
    app: &mut App,
    command: &mut Client,
    net_rx: &mut mpsc::UnboundedReceiver<NetEvent>,
    net_tx: &mpsc::UnboundedSender<NetEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut update_task: Option<JoinHandle<()>> = None;

    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        tokio::select! {
            maybe_event = term_events.next() => {
                let Some(Ok(crossterm::event::Event::Key(key))) = maybe_event else {
                    if maybe_event.is_none() { return Ok(()); }
                    continue;
                };
                match app.handle_key(key) {
                    Action::None => {}
                    Action::Quit => return Ok(()),
                    Action::Send(requests) => {
                        app.error = None;
                        for request in &requests {
                            send_checked(command, app, request).await;
                        }
                    }
                    Action::OpenHistory { account, window, title } => {
                        let since = Utc::now() - chrono::Duration::hours(24);
                        let request = Request::History {
                            account,
                            window: Some(window),
                            since,
                            until: None,
                            max_points: None,
                        };
                        match command.request(&request).await {
                            Ok(Response::History(page)) => {
                                app.view = View::History { title, snapshots: page.snapshots };
                            }
                            other => note_unexpected(app, other),
                        }
                    }
                    Action::OpenRecent => {
                        match command.request(&Request::RecentPolls { limit: 50 }).await {
                            Ok(Response::RecentPolls(events)) => {
                                app.view = View::RecentPolls(events);
                            }
                            other => note_unexpected(app, other),
                        }
                    }
                    Action::OpenProviders => {
                        match command.request(&Request::Providers).await {
                            Ok(Response::Providers(health)) => {
                                app.view = View::Providers(health);
                            }
                            other => note_unexpected(app, other),
                        }
                    }
                    Action::ShutdownDaemon => {
                        let _ = command.request(&Request::Shutdown).await;
                        return Ok(());
                    }
                }
            }
            Some(net_event) = net_rx.recv() => match net_event {
                NetEvent::Update(event) => {
                    app.last_update = Some(event.ts);
                    app.disconnected = false;
                    refresh_status(command, app).await;
                }
                NetEvent::Disconnected(message) => {
                    app.disconnected = true;
                    app.error = Some(message);
                }
            },
            _ = tick.tick() => {
                if app.disconnected {
                    // Reconnect both connections; Client::connect respawns the
                    // daemon if its socket is gone.
                    if let Ok(client) = Client::connect().await {
                        *command = client;
                        app.disconnected = false;
                        app.error = None;
                        refresh_status(command, app).await;
                        if let Some(task) = update_task.take() {
                            task.abort();
                        }
                        update_task = start_update_loop(app, net_tx.clone()).await;
                    }
                }
                // Otherwise the tick just redraws countdowns.
            }
        }
    }
}

/// Fetch a full `Status` and install it in the app; flags disconnection on
/// socket errors instead of crashing.
async fn refresh_status(command: &mut Client, app: &mut App) {
    let request = Request::Status {
        provider: None,
        account: None,
    };
    match command.request(&request).await {
        Ok(Response::Status(statuses)) => app.set_statuses(statuses),
        other => note_unexpected(app, other),
    }
}

/// Send a request where only errors are interesting (e.g. `PollNow`).
async fn send_checked(command: &mut Client, app: &mut App, request: &Request) {
    match command.request(request).await {
        Ok(Response::Err(kind, message)) => {
            app.error = Some(format!("{}: {message}", error_kind_text(kind)));
        }
        Ok(_) => {}
        Err(e) => {
            app.disconnected = true;
            app.error = Some(e.to_string());
        }
    }
}

fn note_unexpected(app: &mut App, response: Result<Response, client::ClientError>) {
    match response {
        Ok(Response::Err(kind, message)) => {
            app.error = Some(format!("{}: {message}", error_kind_text(kind)));
        }
        Ok(other) => app.error = Some(format!("unexpected daemon reply: {other:?}")),
        Err(e) => {
            app.disconnected = true;
            app.error = Some(e.to_string());
        }
    }
}

fn error_kind_text(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::UnknownProvider => "unknown provider",
        ErrorKind::UnknownAccount => "unknown account",
        ErrorKind::BadRequest => "bad request",
        ErrorKind::Storage => "storage error",
        ErrorKind::Internal => "daemon error",
    }
}
