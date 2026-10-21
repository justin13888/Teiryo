//! `teiryod` — headless daemon. Takes the instance lock and binds the Unix
//! socket, polls providers, and serves the wire protocol.

use std::io::{IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;

use anyhow::Context;
use teiryo_core::version::{Action, Flag};
use teiryo_core::{ProviderAdapter, Storage};
use teiryod::paths::{bind_socket, BindOutcome, Paths};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// The one-line description `--help` prints under the version.
const ABOUT: &str = "Teiryo daemon: polls LLM subscription quotas and serves them \
                     over a Unix socket. Normally started by teiryo; runs in the foreground.";

/// Lets scripts such as `mise run install` find the socket without restating
/// the resolution rule.
const PRINT_SOCKET_PATH: Flag = Flag {
    long: "--print-socket-path",
    help: "Print the socket path the daemon binds and exit",
};

/// The log's counterpart to [`PRINT_SOCKET_PATH`]: where the daemon logs
/// differs by platform, and a script that guesses it points the user at a
/// file that does not exist.
const PRINT_LOG_PATH: Flag = Flag {
    long: "--print-log-path",
    help: "Print the log file path the daemon writes and exit",
};

fn main() -> anyhow::Result<()> {
    // Before path resolution and the bind: `-V` must not start a daemon.
    match teiryo_core::version::handle_args_with(
        "teiryod",
        ABOUT,
        &teiryo_core::build_info!(),
        &[PRINT_SOCKET_PATH, PRINT_LOG_PATH],
    ) {
        Action::Run => {}
        Action::Exit(code) => std::process::exit(code),
        Action::Flag(long) if long == PRINT_LOG_PATH.long => {
            // Resolving creates the data directory, so a caller may open the
            // printed path for appending before the daemon has ever run.
            let paths = Paths::resolve().context("resolving XDG paths")?;
            return print_path(&paths.log);
        }
        Action::Flag(_) => return print_path(&teiryo_core::paths::socket_path()),
    }
    let paths = Paths::resolve().context("resolving XDG paths")?;
    init_logging(&paths)?;

    // The runtime exists before the bind so the signal handlers can be
    // installed first: a signal between the bind and their installation would
    // take its default action, killing the daemon with the socket left behind.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;
    let mut signals = {
        let _entered = runtime.enter();
        Signals::install().context("installing signal handlers")?
    };

    // Config is loaded inside `run`, not here: a malformed config.toml must
    // not stop the daemon from binding, or the user would have no client to
    // see the error in and no way to fix it short of reading the log.
    //
    // `_socket` holds the instance lock and unlinks the socket when `main`
    // returns, on the error paths below as much as on a clean stop.
    let (listener, _socket) = match bind_socket(&paths.socket).context("binding socket")? {
        BindOutcome::Bound(listener, guard) => (listener, guard),
        BindOutcome::AlreadyRunning => {
            tracing::info!(socket = %paths.socket.display(), "daemon already running, exiting");
            return Ok(());
        }
    };
    tracing::info!(socket = %paths.socket.display(), "teiryod listening");

    let storage = Storage::open(&paths.db).context("opening database")?;
    let adapters: Vec<Rc<dyn ProviderAdapter>> = teiryo_providers::registry()
        .into_iter()
        .map(Rc::from)
        .collect();

    let local = tokio::task::LocalSet::new();
    let served = local.block_on(&runtime, async {
        // Registering the listener with the reactor must happen here, inside
        // the runtime — `from_std` panics if there is none.
        let listener = tokio::net::UnixListener::from_std(listener)
            .context("registering the socket with the runtime")?;
        let serve = teiryod::run(listener, storage, adapters, paths.config.clone());
        tokio::pin!(serve);
        let daemon = tokio::select! {
            daemon = &mut serve => Some(daemon),
            signal = signals.recv() => {
                tracing::info!(signal, "signal received, shutting down");
                None
            }
        };
        // If a signal interrupted serve, we have no handle — nothing more to
        // flush (storage writes are synchronous); just fall through.
        drop(daemon);
        anyhow::Ok(())
    });

    match &served {
        Ok(()) => tracing::info!("teiryod stopped"),
        Err(e) => tracing::error!(error = %e, "teiryod stopped on error"),
    }
    served
}

/// The `--print-*-path` flags: the path's raw bytes and a newline, so a
/// script gets exactly what the daemon uses even where it is not UTF-8.
fn print_path(path: &std::path::Path) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(path.as_os_str().as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// The shutdown signals, registered with the runtime as soon as it exists.
/// A signal that arrives before the daemon starts serving is held until
/// [`Signals::recv`] is first polled, rather than killing the process.
struct Signals {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
}

impl Signals {
    /// Must be called inside the runtime's context.
    fn install() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Resolves on SIGINT, SIGTERM or SIGHUP, with the signal's name.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.hangup.recv() => "SIGHUP",
        }
    }
}

/// Log to `teiryo.log` (always), filtered by `TEIRYOD_LOG`, and to stderr too
/// when it is a terminal. A daemon the TUI spawns has its stderr redirected
/// into that same log file, so a second layer there would write every line
/// twice, once with ANSI escapes; panics still reach the log through it.
fn init_logging(paths: &Paths) -> anyhow::Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)?;
    let filter = EnvFilter::try_from_env("TEIRYOD_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || file.try_clone().expect("clone log handle")),
        )
        .with(
            std::io::stderr()
                .is_terminal()
                .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr)),
        )
        .init();
    Ok(())
}
