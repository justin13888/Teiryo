//! Socket path resolution and spawn-on-demand of `teiryod`.
//!
//! The TUI never manages the daemon's lifecycle beyond this: if the socket is
//! unreachable it spawns `teiryod` detached (tmux's client/server pattern) and
//! retries the connection.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use directories::ProjectDirs;
use tokio::net::UnixStream;

use crate::client::ClientError;

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

/// The daemon's log file, `$XDG_DATA_HOME/teiryo/teiryo.log`. Must match
/// `teiryod`'s own resolution.
fn log_path() -> Option<PathBuf> {
    Some(
        ProjectDirs::from("", "", "teiryo")?
            .data_dir()
            .join("teiryo.log"),
    )
}

/// Where the spawned daemon's stdout/stderr go: the log file, so that a
/// failure *before* the daemon initialises `tracing` — a panic, a linker
/// error — is still recoverable. Falls back to discarding output rather than
/// refusing to start the daemon at all.
fn daemon_output() -> Stdio {
    let opened = log_path().and_then(|path| {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });
    opened.map_or_else(Stdio::null, Stdio::from)
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

/// Wait on `child` in the background. Nothing here waits on the daemon inline —
/// it deliberately outlives the TUI — but a `Child` dropped unwaited leaves the
/// daemon `<defunct>` in our process table for as long as we run, and the
/// reconnect loop spawns one per second per connection while the socket is
/// down. The thread parks in `wait` and dies with the process, which reparents
/// a daemon that is still running to init.
fn reap_in_background(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Spawn `teiryod` detached in its own session-like process group, stdio
/// routed away from the TUI's terminal.
fn spawn_daemon(binary: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(binary);
    cmd.stdin(Stdio::null())
        .stdout(daemon_output())
        .stderr(daemon_output())
        .process_group(0);
    cmd.spawn().map(reap_in_background)
}

/// Connect to the daemon socket, spawning the daemon if it is not running.
///
/// A first straight connect is tried; on failure the daemon is spawned once
/// and the connect retried with a short backoff until it binds.
pub async fn connect_or_spawn() -> Result<UnixStream, ClientError> {
    let path = socket_path();
    if let Ok(stream) = UnixStream::connect(&path).await {
        return Ok(stream);
    }

    let binary = daemon_binary();
    if let Err(e) = spawn_daemon(&binary) {
        return Err(spawn_error(&binary, &e));
    }

    let mut last_err = io::Error::new(io::ErrorKind::ConnectionRefused, "daemon did not start");
    for _ in 0..CONNECT_ATTEMPTS {
        tokio::time::sleep(CONNECT_BACKOFF).await;
        match UnixStream::connect(&path).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = e,
        }
    }
    // The daemon was spawned but never bound: it died on startup, and its
    // output went to the log file.
    Err(ClientError::DaemonStart(match log_path() {
        Some(log) => format!(
            "teiryod started but never bound {} ({last_err}) — see {}",
            path.display(),
            log.display()
        ),
        None => format!(
            "teiryod started but never bound {} ({last_err})",
            path.display()
        ),
    }))
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

    /// A zombie keeps its pid valid for the life of its parent, so polling the
    /// pid is what separates "reaped" from "still sitting in our process
    /// table": without the background `wait` this never reaches `ESRCH`.
    #[test]
    fn reaped_child_leaves_no_zombie() {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("/bin/sh should be spawnable");
        let pid = i32::try_from(child.id()).expect("a pid fits in pid_t");
        reap_in_background(child);

        for _ in 0..200 {
            // SAFETY: signal 0 performs no delivery, only an existence check.
            if unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("pid {pid} still exists: the child was never reaped");
    }

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
