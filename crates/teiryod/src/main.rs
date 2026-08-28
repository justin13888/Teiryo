//! `teiryod` — headless daemon. Binds the Unix socket (the bind is the
//! single-instance lock), polls providers, and serves the wire protocol.

use std::rc::Rc;

use anyhow::Context;
use teiryo_core::{ProviderAdapter, Storage};
use teiryod::paths::{bind_socket, BindOutcome, Paths};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    let paths = Paths::resolve().context("resolving XDG paths")?;
    init_logging(&paths)?;

    // Config is loaded inside `run`, not here: a malformed config.toml must
    // not stop the daemon from binding, or the user would have no client to
    // see the error in and no way to fix it short of reading the log.
    let listener = match bind_socket(&paths.socket).context("binding socket")? {
        BindOutcome::Bound(l) => l,
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

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
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
            _ = shutdown_signal() => {
                tracing::info!("signal received, shutting down");
                None
            }
        };
        // If a signal interrupted serve, we have no handle — nothing more to
        // flush (storage writes are synchronous); just fall through.
        drop(daemon);
        anyhow::Ok(())
    });

    std::fs::remove_file(&paths.socket).ok();
    match &served {
        Ok(()) => tracing::info!("teiryod stopped"),
        Err(e) => tracing::error!(error = %e, "teiryod stopped on error"),
    }
    served
}

/// Resolves on SIGINT or SIGTERM.
async fn shutdown_signal() {
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("sigterm");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

/// Log to `teiryo.log` (always) and stderr, filtered by `TEIRYOD_LOG`.
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
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
    Ok(())
}
