//! Binary-level tests: run the built `teiryod` with `HOME` and every XDG
//! directory pointed into a scratch directory, so nothing touches the user's
//! data, config, or running daemon.
//!
//! Only `--print-socket-path` and `--print-log-path` run on macOS: there the
//! socket lives in the
//! per-user `confstr` temp dir, which no environment variable redirects, so a
//! test that binds would share the real daemon's socket and lock. The tests
//! that bind run where `$XDG_RUNTIME_DIR` isolates the socket.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

const TEIRYOD: &str = env!("CARGO_BIN_EXE_teiryod");

/// A scratch directory per test, short enough that the socket inside it
/// stays under the bindable length instead of falling back to `/tmp`.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/teiryod-bin-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for sub in ["home", "run", "data", "config"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        Self(dir)
    }

    fn runtime_dir(&self) -> PathBuf {
        self.0.join("run")
    }

    // Used only by the tests that bind, which macOS does not run.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn socket(&self) -> PathBuf {
        self.runtime_dir().join(teiryo_core::paths::SOCKET_NAME)
    }

    /// Where `Paths::resolve` puts the database and log. macOS ignores the
    /// XDG variables and uses `$HOME/Library/Application Support`.
    fn data_dir(&self) -> PathBuf {
        #[cfg(target_os = "macos")]
        let base = self.0.join("home/Library/Application Support");
        #[cfg(not(target_os = "macos"))]
        let base = self.0.join("data");
        base.join("teiryo")
    }

    /// `teiryod` with an environment holding only the scratch directories.
    fn command(&self) -> Command {
        let mut cmd = Command::new(TEIRYOD);
        cmd.env_clear()
            .env("HOME", self.0.join("home"))
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().next().is_none()
}

#[test]
fn print_socket_path_prints_the_resolved_path_and_starts_nothing() {
    let scratch = Scratch::new();
    let out = scratch
        .command()
        .arg("--print-socket-path")
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "exit status {:?}", out.status);

    #[cfg(target_os = "macos")]
    let expected = teiryo_core::paths::socket_path();
    #[cfg(not(target_os = "macos"))]
    let expected = scratch.socket();
    let mut line = expected.as_os_str().as_bytes().to_vec();
    line.push(b'\n');
    assert_eq!(out.stdout, line);

    // It exits before resolving the data dirs: no log, no database, no bind.
    for sub in ["home", "run", "data", "config"] {
        assert!(is_empty_dir(&scratch.0.join(sub)), "{sub} was written");
    }
}

#[test]
fn print_log_path_prints_the_log_path_and_creates_its_directory() {
    let scratch = Scratch::new();
    let out = scratch
        .command()
        .arg("--print-log-path")
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "exit status {:?}", out.status);

    let log = scratch.data_dir().join("teiryo.log");
    let mut line = log.as_os_str().as_bytes().to_vec();
    line.push(b'\n');
    assert_eq!(out.stdout, line);

    // The directory exists so a script can append to the log before the
    // daemon has ever run, but nothing was logged, opened, or bound.
    assert!(scratch.data_dir().is_dir(), "data dir not created");
    assert!(is_empty_dir(&scratch.data_dir()), "data dir was written");
    assert!(is_empty_dir(&scratch.runtime_dir()), "run was written");
}

#[cfg(not(target_os = "macos"))]
mod binding {
    use super::*;
    use std::time::{Duration, Instant};

    /// Bounded: a daemon that never binds fails the test rather than hanging it.
    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn log(scratch: &Scratch) -> String {
        std::fs::read_to_string(scratch.data_dir().join("teiryo.log")).unwrap_or_default()
    }

    #[test]
    fn sighup_stops_the_daemon_and_unlinks_its_socket() {
        let scratch = Scratch::new();
        let mut child = scratch.command().spawn().unwrap();
        let socket = scratch.socket();
        wait_for("the socket", || socket.exists());

        // SAFETY: kill has no memory-safety preconditions; the pid is our child.
        let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) };
        assert_eq!(rc, 0);
        let mut status = None;
        wait_for("the daemon to exit", || {
            status = child.try_wait().unwrap();
            status.is_some()
        });
        assert!(status.unwrap().success(), "exit status {status:?}");

        assert!(!socket.exists(), "socket left behind");
        let log = log(&scratch);
        assert!(
            log.contains("signal received, shutting down signal=\"SIGHUP\""),
            "{log}"
        );
        assert!(log.contains("teiryod stopped"), "{log}");
    }

    #[test]
    fn an_early_error_after_the_bind_still_unlinks_the_socket() {
        let scratch = Scratch::new();
        // A directory where the database belongs makes `Storage::open` fail
        // after the bind, on one of `main`'s early `?` exits.
        std::fs::create_dir_all(scratch.data_dir().join("teiryo.db")).unwrap();

        let status = scratch.command().status().unwrap();
        assert!(!status.success(), "exit status {status:?}");

        let log = log(&scratch);
        assert!(log.contains("teiryod listening"), "never bound: {log}");
        assert!(!scratch.socket().exists(), "socket left behind");
    }
}
