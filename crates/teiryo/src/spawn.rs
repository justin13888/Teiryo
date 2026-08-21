//! Socket path resolution and spawn-on-demand of `teiryod`.
//!
//! The TUI never manages the daemon's lifecycle beyond this: if the socket is
//! unreachable it spawns `teiryod` detached (tmux's client/server pattern) and
//! retries the connection.

use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::net::UnixStream;

/// Attempts made while waiting for a freshly spawned daemon to bind.
const CONNECT_ATTEMPTS: u32 = 25;
/// Delay between connection attempts.
const CONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// The daemon socket path: `$XDG_RUNTIME_DIR/teiryo.sock`, falling back to
/// `/tmp/teiryo-$UID.sock`. Must match the daemon's resolution exactly.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("teiryo.sock"),
        _ => PathBuf::from(format!("/tmp/teiryo-{}.sock", uid())),
    }
}

fn uid() -> u32 {
    // SAFETY: getuid is always safe to call.
    unsafe { libc::getuid() }
}

/// Locate the `teiryod` binary: next to the current executable first (the
/// common cargo/packaging layout), then `$PATH`.
fn daemon_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("teiryod");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("teiryod")
}

/// Spawn `teiryod` detached in its own session-like process group, stdio
/// routed away from the TUI's terminal (the daemon logs to its own file).
fn spawn_daemon() -> io::Result<()> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(daemon_binary());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    cmd.spawn().map(drop)
}

/// Connect to the daemon socket, spawning the daemon if it is not running.
///
/// A first straight connect is tried; on failure the daemon is spawned once
/// and the connect retried with a short backoff until it binds.
pub async fn connect_or_spawn() -> io::Result<UnixStream> {
    let path = socket_path();
    if let Ok(stream) = UnixStream::connect(&path).await {
        return Ok(stream);
    }
    spawn_daemon()?;
    let mut last_err = io::Error::new(io::ErrorKind::ConnectionRefused, "daemon did not start");
    for _ in 0..CONNECT_ATTEMPTS {
        tokio::time::sleep(CONNECT_BACKOFF).await;
        match UnixStream::connect(&path).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env mutation is process-global; keep it inside one test to avoid races.
    #[test]
    fn socket_path_resolution() {
        let orig = std::env::var_os("XDG_RUNTIME_DIR");

        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(
            socket_path(),
            PathBuf::from("/run/user/1000/teiryo.sock"),
            "XDG_RUNTIME_DIR takes precedence"
        );

        std::env::remove_var("XDG_RUNTIME_DIR");
        let fallback = socket_path();
        assert_eq!(
            fallback,
            PathBuf::from(format!("/tmp/teiryo-{}.sock", uid())),
            "falls back to /tmp with the real uid"
        );

        std::env::set_var("XDG_RUNTIME_DIR", "");
        assert_eq!(socket_path(), fallback, "empty XDG_RUNTIME_DIR is ignored");

        match orig {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }
}
