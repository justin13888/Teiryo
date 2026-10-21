//! Spawn-on-demand of `teiryod`.
//!
//! The TUI never manages the daemon's lifecycle beyond this: if the socket is
//! unreachable it spawns `teiryod` detached (tmux's client/server pattern) and
//! retries the connection. The socket path comes from
//! [`teiryo_core::paths::socket_path`], the rule the daemon binds by, and the
//! log the spawned daemon writes to from [`teiryo_core::paths::log_path`].

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use teiryo_core::paths::{log_path, socket_path};
use tokio::net::UnixStream;

use crate::client::ClientError;

/// Attempts made while waiting for a freshly spawned daemon to bind.
const CONNECT_ATTEMPTS: u32 = 25;
/// Delay between connection attempts.
const CONNECT_BACKOFF: Duration = Duration::from_millis(200);

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
/// reconnect loop may spawn one per backoff step for as long as the socket is
/// down. The thread parks in `wait` and dies with the process, which reparents
/// a daemon that is still running to init.
fn reap_in_background(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// The variables a spawned daemon keeps from the TUI's environment, besides
/// every `TEIRYO_*` one ([`INHERITED_PREFIX`]). The daemon is shared by every
/// later TUI, from any shell, for as long as it runs, so it must not carry
/// whatever the first TUI's shell happened to hold: per-directory mise
/// `[env]` values, proxy and certificate settings, `cargo run`'s variables.
///
/// `HOME`, `TMPDIR` and the `XDG_*` base directories are what the socket,
/// database, config and log paths resolve from; dropping one would make the
/// daemon bind or write somewhere other than where the TUI looks.
const INHERITED: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "TMPDIR",
    "LANG",
    "TEIRYOD_LOG",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
];

/// The prefix of the daemon's own settings (`TEIRYO_CLAUDE_CREDENTIALS`,
/// `TEIRYO_CLAUDE_BASE_URL`, …), all of which a spawned daemon keeps.
const INHERITED_PREFIX: &[u8] = b"TEIRYO_";

/// Whether a spawned daemon keeps the variable `name`.
fn inherited(name: &OsStr) -> bool {
    let name = name.as_bytes();
    name.starts_with(INHERITED_PREFIX) || INHERITED.iter().any(|kept| kept.as_bytes() == name)
}

/// The command that starts `binary` as the daemon: [`detached`], with an
/// environment of only the variables of `env` that [`inherited`] keeps.
/// Production passes [`std::env::vars_os`]; tests pass their own. Stdio is
/// the caller's to set.
fn daemon_command(binary: &Path, env: impl IntoIterator<Item = (OsString, OsString)>) -> Command {
    let mut cmd = detached(binary);
    cmd.env_clear()
        .envs(env.into_iter().filter(|(name, _)| inherited(name)));
    cmd
}

/// Spawn `teiryod` detached: in a session of its own, so it has no
/// controlling terminal and the TUI's terminal closing cannot signal it; with
/// `/` as its working directory, so it never pins the directory the TUI was
/// launched from (and with it a volume that could otherwise be ejected); with
/// only the environment [`daemon_command`] keeps; and with stdio routed away
/// from the TUI's terminal.
fn spawn_daemon(binary: &Path) -> io::Result<()> {
    let mut cmd = daemon_command(binary, std::env::vars_os());
    cmd.stdin(Stdio::null())
        .stdout(daemon_output())
        .stderr(daemon_output());
    cmd.spawn().map(reap_in_background)
}

/// `binary` as a command that starts in a new session with `/` as its
/// working directory. Stdio is the caller's to set.
fn detached(binary: &Path) -> Command {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(binary);
    cmd.current_dir("/");
    // SAFETY: runs in the forked child before exec, where only
    // async-signal-safe calls are allowed; setsid is one, and nothing here
    // allocates.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
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

    /// The spawned daemon leads a session of its own and runs from `/`, so it
    /// neither shares the TUI's terminal session nor pins its directory.
    #[test]
    fn detached_child_has_its_own_session_and_root_cwd() {
        use std::io::BufRead;

        let mut child = detached(Path::new("/bin/sh"))
            .arg("-c")
            .arg("pwd; exec sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("/bin/sh should be spawnable");
        let mut cwd = String::new();
        io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut cwd)
            .unwrap();
        let pid = i32::try_from(child.id()).expect("a pid fits in pid_t");
        // SAFETY: getsid only reads the process table.
        let sid = unsafe { libc::getsid(pid) };
        // SAFETY: getsid(0) is the caller's own session.
        let ours = unsafe { libc::getsid(0) };
        child.kill().ok();
        child.wait().ok();

        assert_eq!(cwd.trim_end(), "/");
        assert_eq!(sid, pid, "the child leads a new session");
        assert_ne!(sid, ours);
    }

    /// A daemon spawned from a shell with arbitrary extra variables starts
    /// with only the allowlisted ones and the `TEIRYO_*` settings: the child
    /// is `env` itself, so its output is exactly the environment it got.
    #[test]
    fn daemon_command_passes_only_the_allowlist() {
        let parent = [
            ("HOME", "/home/u"),
            ("PATH", "/usr/bin:/bin"),
            ("USER", "u"),
            ("TMPDIR", "/tmp/u"),
            ("LANG", "C.UTF-8"),
            ("TEIRYOD_LOG", "debug"),
            ("XDG_CONFIG_HOME", "/home/u/.config"),
            ("XDG_DATA_HOME", "/home/u/.local/share"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("TEIRYO_CLAUDE_BASE_URL", "http://127.0.0.1:1"),
            ("HTTPS_PROXY", "http://proxy:3128"),
            ("SSL_CERT_FILE", "/etc/custom.pem"),
            ("CARGO_MANIFEST_DIR", "/src/teiryo"),
            ("MISE_ENV_VALUE", "x"),
            // Neither a prefix match nor a case-insensitive one.
            ("TEIRYO", "bare"),
            ("teiryo_lower", "x"),
            ("HOMEDIR", "/elsewhere"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));

        let output = daemon_command(Path::new("/usr/bin/env"), parent)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("/usr/bin/env should be spawnable");
        assert!(output.status.success());
        let mut got: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        got.sort();

        let mut want = vec![
            "HOME=/home/u",
            "LANG=C.UTF-8",
            "PATH=/usr/bin:/bin",
            "TEIRYOD_LOG=debug",
            "TEIRYO_CLAUDE_BASE_URL=http://127.0.0.1:1",
            "TMPDIR=/tmp/u",
            "USER=u",
            "XDG_CONFIG_HOME=/home/u/.config",
            "XDG_DATA_HOME=/home/u/.local/share",
            "XDG_RUNTIME_DIR=/run/user/1000",
        ];
        want.sort_unstable();
        assert_eq!(got, want);
    }

    /// The falsifier for the test above: the test process's own environment
    /// (cargo sets `CARGO_*` for every test run) never leaks past the clear.
    #[test]
    fn daemon_command_does_not_inherit_the_real_environment() {
        let output = daemon_command(Path::new("/usr/bin/env"), std::iter::empty())
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("/usr/bin/env should be spawnable");
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
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
