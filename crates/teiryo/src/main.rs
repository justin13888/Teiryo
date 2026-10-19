//! `teiryo` — thin TUI client for the Teiryo daemon. No HTTP, no DB, no
//! scheduling: it only speaks the Unix-socket wire protocol.

mod app;
mod client;
mod metrics;
mod spawn;
mod ui;

use chrono::Utc;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, MouseEventKind,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use teiryo_core::domain::AccountId;
use teiryo_core::protocol::handshake::PROTOCOL_VERSION;
use teiryo_core::{ErrorKind, Request, Response};

use app::{Action, App, DetailQuery, Trend};
use client::{is_version_mismatch, newest_poll_id, spawn_update_loop, Client, NetEvent};

/// The one-line description `--help` prints under the version.
const ABOUT: &str = "Live dashboard for LLM subscription quotas. \
                     Starts teiryod on demand when it is not running.";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Before anything touches the socket or the terminal: `-V` must not spawn
    // a daemon or open the dashboard.
    if let Some(code) =
        teiryo_core::version::handle_args("teiryo", ABOUT, &teiryo_core::build_info!())
    {
        std::process::exit(code);
    }
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
    refresh_recent(&mut command, &mut app).await;
    reload_detail(&mut command, &mut app).await;
    // Primed before the first frame so a rejected config.toml shows in the
    // header immediately, rather than only once the user opens Settings.
    load_config(&mut command, &mut app).await;

    let (net_tx, mut net_rx) = mpsc::unbounded_channel();
    let mut update_task = start_update_loop(&app, net_tx.clone()).await;

    let terminal = ratatui::init();
    // The wheel scrolls the pane the pointer is over, which the terminal only
    // reports while the mouse is captured. The cost is the terminal's own
    // click-to-select, which most emulators still offer under Shift.
    let mouse = crossterm::execute!(std::io::stdout(), EnableMouseCapture).is_ok();
    let result = event_loop(terminal, &mut app, &mut command, &mut net_rx, &net_tx).await;
    if mouse {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
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
        Ok(client) => Some(spawn_update_loop(
            client,
            newest_poll_id(&app.statuses),
            app.config.as_ref().map_or(0, |c| c.generation),
            tx,
        )),
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

    // Whatever happens below, the reconnect-created long-poll task must not
    // outlive this function.
    let result = drive(
        &mut terminal,
        app,
        command,
        net_rx,
        net_tx,
        &mut term_events,
        &mut tick,
        &mut update_task,
    )
    .await;
    if let Some(task) = update_task.take() {
        task.abort();
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    command: &mut Client,
    net_rx: &mut mpsc::UnboundedReceiver<NetEvent>,
    net_tx: &mpsc::UnboundedSender<NetEvent>,
    term_events: &mut EventStream,
    tick: &mut tokio::time::Interval,
    update_task: &mut Option<JoinHandle<()>>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        tokio::select! {
            maybe_event = term_events.next() => {
                let action = match maybe_event {
                    Some(Ok(Event::Key(key))) => app.handle_key(key),
                    // With mouse capture on, every movement of the pointer is
                    // an event, and redrawing the charts for each one is what
                    // a moving pointer would otherwise cost. Nothing here
                    // responds to movement, so it is dropped before the draw.
                    Some(Ok(Event::Mouse(mouse)))
                        if matches!(mouse.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) =>
                    {
                        continue
                    }
                    Some(Ok(Event::Mouse(mouse))) => app.handle_mouse(mouse),
                    None => return Ok(()),
                    _ => continue,
                };
                match action {
                    Action::None => {}
                    Action::Quit => return Ok(()),
                    Action::Send(requests) => {
                        app.error = None;
                        for request in &requests {
                            send_checked(command, app, request).await;
                        }
                    }
                    Action::ReloadDetail => reload_detail(command, app).await,
                    Action::LoadConfig => load_config(command, app).await,
                    Action::EditConfig(edit) => {
                        app.error = None;
                        edit_config(command, app, edit).await;
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
                    app.poll_settled(&event.account);
                    refresh_status(command, app).await;
                    refresh_recent(command, app).await;
                    // The detail pane tracks the same data, so it refreshes
                    // with the dashboard rather than freezing at whatever it
                    // held when the tab was opened.
                    reload_detail(command, app).await;
                }
                NetEvent::Config(state) => {
                    app.disconnected = false;
                    app.config = Some(*state);
                    // Cadences just changed, and `Status` is what carries them.
                    refresh_status(command, app).await;
                    refresh_recent(command, app).await;
                }
                NetEvent::Disconnected(message) => {
                    app.disconnected = true;
                    app.error = Some(message);
                }
            },
            _ = tick.tick() => {
                app.spinner = app.spinner.wrapping_add(1);
                if app.disconnected {
                    // Reconnect both connections; Client::connect respawns the
                    // daemon if its socket is gone.
                    if let Ok(client) = Client::connect().await {
                        *command = client;
                        app.disconnected = false;
                        app.error = None;
                        refresh_status(command, app).await;
                        refresh_recent(command, app).await;
                        reload_detail(command, app).await;
                        // The daemon may have restarted onto a different
                        // config; a stale generation would also make the new
                        // long-poll miss the next reload.
                        load_config(command, app).await;
                        if let Some(task) = update_task.take() {
                            task.abort();
                        }
                        *update_task = start_update_loop(app, net_tx.clone()).await;
                    }
                }
                // Otherwise the tick just redraws countdowns and the spinner.
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

/// How far back the rows' burn rates may look: two doublings past the longest
/// lookback `metrics::estimate_rate` starts from, which is twelve hours.
///
/// Twelve alone left a weekly window nothing to widen into. One rounding step
/// over twelve hours of a week is already wider than the precision target, so
/// the row stopped at its first pass with the widest interval there is while
/// the charts, fetching more, widened past it — and the two disagreed. At the
/// default three-minute cadence two days is 960 readings per window, inside
/// `RECENT_MAX_POINTS`, so the daemon still returns them undownsampled and the
/// rate is measured from real endpoints.
const RECENT_SPAN_HOURS: i64 = 48;

/// Point budget for one window's slice of that history.
const RECENT_MAX_POINTS: u32 = 1_000;

/// Fetch the recent history the rows measure their burn rates from.
///
/// One request per account rather than per window: `History` takes
/// `window: None` to mean all of them, and the daemon buckets the reply by
/// window on the way out.
async fn refresh_recent(command: &mut Client, app: &mut App) {
    let since = Utc::now() - chrono::Duration::hours(RECENT_SPAN_HOURS);
    let accounts: Vec<AccountId> = app.statuses.iter().map(|s| s.account.id.clone()).collect();
    for account in accounts {
        let request = Request::History {
            account: account.clone(),
            window: None,
            since,
            until: None,
            max_points: Some(RECENT_MAX_POINTS),
        };
        match command.request(&request).await {
            Ok(Response::History(page)) => app.set_recent(&account, page.snapshots),
            // One unreachable account means the socket is gone; the rest will
            // fail the same way, so stop rather than pile up identical errors.
            other => return note_unexpected(app, other),
        }
    }
}

/// Fetch whatever the detail pane's current tab needs.
async fn reload_detail(command: &mut Client, app: &mut App) {
    // One clock reading for the whole exchange: the trend's pan is measured
    // back from it, and the reply is bounded by how much history exists as of
    // it, so the two must agree.
    let now = Utc::now();
    let Some(query) = app.detail_query(now) else {
        // Nothing selected to chart; drop any stale series so the pane says
        // so rather than showing the previous window's history.
        app.trend = None;
        return;
    };
    match query {
        DetailQuery::Trend {
            account,
            window,
            title,
            range,
            until,
            request,
            extras,
        } => {
            match command.request(&request).await {
                Ok(Response::History(page)) => {
                    app.set_trend(Trend {
                        account,
                        window,
                        title,
                        snapshots: page.snapshots,
                        rollovers: page.rollovers,
                        range,
                        until,
                        earliest: page.earliest,
                        fetched_at: now,
                    });
                }
                other => return note_unexpected(app, other),
            }
            // Whatever else the view charts. Each reply is checked against
            // the selection when it is drawn, not here, so one landing after
            // the cursor moved is simply never shown.
            for extra in extras {
                match command.request(&extra.request).await {
                    Ok(Response::History(page)) => app.set_extra(
                        extra.purpose,
                        Trend {
                            account: extra.account,
                            window: extra.window,
                            title: extra.title,
                            snapshots: page.snapshots,
                            rollovers: page.rollovers,
                            range: extra.range,
                            until: extra.until,
                            earliest: page.earliest,
                            fetched_at: now,
                        },
                    ),
                    other => return note_unexpected(app, other),
                }
            }
        }
        DetailQuery::Activity(request) => match command.request(&request).await {
            Ok(Response::RecentPolls(events)) => app.activity = events,
            other => note_unexpected(app, other),
        },
        DetailQuery::Health(request) => match command.request(&request).await {
            Ok(Response::Providers(health)) => app.health = health,
            other => note_unexpected(app, other),
        },
    }
}

/// Fetch the daemon's current settings.
async fn load_config(command: &mut Client, app: &mut App) {
    match command.request(&Request::GetConfig).await {
        Ok(Response::Config(state)) => app.config = Some(state),
        other => note_unexpected(app, other),
    }
}

/// Apply one settings change. The reply carries the resulting settings, so the
/// overlay redraws from what the daemon actually did rather than from what the
/// keypress asked for.
async fn edit_config(command: &mut Client, app: &mut App, edit: teiryo_core::ConfigEdit) {
    match command.request(&Request::SetConfig(edit)).await {
        Ok(Response::Config(state)) => {
            app.config = Some(state);
            // The cadence shown on the dashboard comes from `Status`.
            refresh_status(command, app).await;
        }
        other => note_unexpected(app, other),
    }
}

/// Send a request where only errors are interesting (e.g. `PollNow`).
async fn send_checked(command: &mut Client, app: &mut App, request: &Request) {
    match command.request(request).await {
        Ok(Response::Err(kind, message)) => {
            app.error = Some(format!("{}: {message}", error_kind_text(kind)));
            // A refused poll never lands, so nothing else would stop its
            // spinner.
            if let Request::PollNow { provider, account } = request {
                app.poll_refused(provider, account.as_ref());
            }
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
