//! Spawn-on-demand of a local `teiryod`.
//!
//! The TUI never manages the daemon's lifecycle beyond this: if the socket is
//! unreachable it spawns `teiryod` detached (tmux's client/server pattern) and
//! retries the connection. How is [`teiryo_core::protocol::transport`]'s, which
//! `teiryod bridge` shares; which binary, and how a failure reads, is this
//! module's.

use std::io;
use std::path::{Path, PathBuf};

use teiryo_core::protocol::transport::{self, SpawnError};
use tokio::net::UnixStream;

use crate::client::ClientError;

/// Where `teiryod` sits next to the current executable — the common
/// cargo/packaging layout.
fn sibling_daemon() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("teiryod"))
}

/// Locate the `teiryod` binary: next to the current executable first, then
/// `$PATH`.
fn daemon_binary() -> PathBuf {
    match sibling_daemon() {
        Some(sibling) if sibling.is_file() => sibling,
        _ => PathBuf::from("teiryod"),
    }
}

/// Why the daemon could not even be launched. A missing binary is the common
/// case and is actionable, so it gets its own wording.
fn spawn_error(binary: &Path, e: &io::Error) -> ClientError {
    if e.kind() != io::ErrorKind::NotFound {
        return ClientError::DaemonStart(format!("cannot start {}: {e}", binary.display()));
    }
    let looked = sibling_daemon().map_or_else(
        || "$PATH".to_owned(),
        |p| format!("{} and $PATH", p.display()),
    );
    ClientError::DaemonStart(format!(
        "cannot find the teiryod binary (looked in {looked}) — \
         install it with `mise run install`, or build it with \
         `cargo build -p teiryod` when running from a checkout"
    ))
}

/// Connect to the daemon socket, spawning the daemon if it is not running:
/// [`transport::connect_or_spawn`] with the `teiryod` this TUI finds, and its
/// failures worded for the TUI.
pub async fn connect_or_spawn() -> Result<UnixStream, ClientError> {
    let binary = daemon_binary();
    transport::connect_or_spawn(&binary)
        .await
        .map_err(|e| match e {
            SpawnError::Launch(e) => spawn_error(&binary, &e),
            SpawnError::NeverBound(message) => ClientError::DaemonStart(message),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_daemon_error_names_both_ways_to_get_it() {
        let missing = io::Error::from(io::ErrorKind::NotFound);
        let ClientError::DaemonStart(message) =
            spawn_error(Path::new("/nowhere/teiryod"), &missing)
        else {
            panic!("a missing binary is a start failure");
        };
        // Spelled out end to end: the remedy spans two escaped line
        // continuations, and a botched one would only show up as stray
        // whitespace in the sentence the user actually reads.
        assert!(
            message.ends_with(
                "install it with `mise run install`, or build it with \
                 `cargo build -p teiryod` when running from a checkout"
            ),
            "{message}"
        );
    }

    /// The falsifier for the test above: only `NotFound` means "we could not
    /// find it", so any other failure keeps naming the binary it tried.
    #[test]
    fn other_spawn_failures_name_the_binary() {
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let ClientError::DaemonStart(message) = spawn_error(Path::new("/nowhere/teiryod"), &denied)
        else {
            panic!("a failed start is a start failure");
        };
        assert!(message.contains("/nowhere/teiryod"), "{message}");
        assert!(!message.contains("mise run install"), "{message}");
    }
}
