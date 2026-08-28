//! Hot reload: watch `config.toml` and put every valid edit into effect.
//!
//! The daemon is the only writer clients go through, but it is not the only
//! writer — `$EDITOR` is, too. Watching the file makes both paths identical:
//! `SetConfig` writes and the watcher applies, exactly as a hand edit does.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::config;
use crate::state::Daemon;

/// How long to let filesystem events settle before re-reading. One save can
/// produce several events (truncate, write, rename), and re-reading between
/// them would parse a half-written file and report a spurious error.
const SETTLE: Duration = Duration::from_millis(200);

/// Watch `path` and apply every change until shutdown. `seen` is the file text
/// already read at startup. Must run inside a `tokio::task::LocalSet`.
///
/// The watcher must be kept alive for the duration — dropping it unregisters
/// the inotify watch — so this owns it and only returns on shutdown.
pub async fn watch_config(daemon: Daemon, path: PathBuf, seen: String) {
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();

    // Editors replace a config file by writing a temp file and renaming over
    // it, which leaves the original inode — and any watch on it — pointing at
    // a file nothing will ever touch again. Watching the directory survives
    // that; the filter below keeps the other files in it out of the way.
    let dir = match path.parent() {
        Some(dir) => dir.to_path_buf(),
        None => {
            tracing::warn!(path = %path.display(), "config path has no parent, not watching");
            return;
        }
    };
    let name = path.file_name().map(std::ffi::OsStr::to_os_string);
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        let touched = event.paths.iter().any(|p| p.file_name() == name.as_deref());
        if touched {
            // The receiver is gone once the daemon is shutting down; nothing
            // to report in that case.
            let _ = tx.send(());
        }
    });
    let mut watcher: RecommendedWatcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "cannot create a config watcher; hot reload is off");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        tracing::warn!(dir = %dir.display(), error = %e, "cannot watch the config directory; hot reload is off");
        return;
    }
    tracing::debug!(path = %path.display(), "watching config for changes");

    let mut shutdown_rx = daemon.shutdown_tx.subscribe();
    let mut seen = seen;
    loop {
        tokio::select! {
            event = rx.recv() => {
                if event.is_none() {
                    return; // watcher dropped
                }
            }
            _ = shutdown_rx.changed() => return,
        }
        // Coalesce the rest of the burst.
        tokio::time::sleep(SETTLE).await;
        while rx.try_recv().is_ok() {}

        if let Some(text) = reload(&daemon, &path, &seen) {
            seen = text;
        }
    }
}

/// Re-read and apply the file. Returns the new text when the file had actually
/// changed, so the caller can keep comparing against it.
///
/// Comparing text rather than tracking who wrote last is what makes the
/// daemon's own `SetConfig` writes free: they arrive here identical to the
/// config already running, and a no-op save from an editor does too. A
/// rename-replace also lands as more than one event, and without this it would
/// report the same rejection twice.
fn reload(daemon: &Daemon, path: &Path, seen: &str) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // A removed config means "no overrides", the same as never having one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read config.toml");
            daemon.reject_config(format!("cannot read the file: {e}"));
            return None;
        }
    };
    if text == seen {
        return None;
    }
    match config::parse(&text) {
        Ok(loaded) => {
            for warning in &loaded.warnings {
                tracing::warn!(config = %path.display(), "{warning}");
            }
            tracing::info!(config = %path.display(), "config reloaded");
            daemon.apply_config(loaded);
        }
        Err(e) => {
            tracing::error!(config = %path.display(), error = %e, "config rejected, keeping the previous settings");
            daemon.reject_config(e.to_string());
        }
    }
    // Recorded either way. A rejection stays visible in `ConfigState.error`
    // until a load succeeds, so re-reporting an unchanged broken file would
    // add nothing but noise.
    Some(text)
}

/// Read and apply the config at startup. A bad file is reported rather than
/// fatal — a daemon that refuses to start over a typo leaves the user with no
/// way to see the typo. Returns the text that was read, for the watcher's
/// already-applied comparison.
pub fn load_initial(daemon: &Daemon, path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    match config::load(path) {
        Ok(loaded) => {
            for warning in &loaded.warnings {
                tracing::warn!(config = %path.display(), "{warning}");
            }
            daemon.apply_config(loaded);
        }
        Err(e) => {
            tracing::error!(config = %path.display(), error = %e, "config rejected, starting on defaults");
            daemon.reject_config(e.to_string());
        }
    }
    text
}
