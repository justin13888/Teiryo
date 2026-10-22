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
        base.join(teiryo_core::paths::DATA_DIR_NAME)
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

    mod bridge {
        use super::*;
        use futures::{SinkExt, StreamExt};
        use teiryo_core::protocol::handshake::encode_commit;
        use teiryo_core::protocol::ssh::SshTarget;
        use teiryo_core::{
            client_handshake, decode_frame, encode_frame, framed, Request, Response,
            PROTOCOL_MAGIC, PROTOCOL_VERSION,
        };
        use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

        /// The commit these test clients announce.
        const CLIENT_COMMIT: &str = "0123456789ab";

        /// Bounded: a hung bridge or daemon fails the test rather than
        /// hanging it.
        async fn bounded<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
            tokio::time::timeout(Duration::from_secs(20), fut)
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        }

        /// `teiryod bridge` in the scratch environment, its stdio piped.
        fn bridge(scratch: &Scratch) -> tokio::process::Child {
            let mut cmd = tokio::process::Command::from(scratch.command());
            cmd.arg("bridge")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            cmd.spawn().unwrap()
        }

        /// The bridge child's stdout and stdin as one stream.
        fn stdio(
            child: &mut tokio::process::Child,
        ) -> tokio::io::Join<tokio::process::ChildStdout, tokio::process::ChildStdin> {
            tokio::io::join(child.stdout.take().unwrap(), child.stdin.take().unwrap())
        }

        /// A whole session over `stream`: the handshake, a request, and the
        /// `Shutdown` that stops the daemon the bridge spawned.
        async fn session<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, scratch: &Scratch) {
            let daemon_commit = bounded(
                "the handshake",
                client_handshake(&mut stream, CLIENT_COMMIT),
            )
            .await
            .expect("the daemon accepts a client of its own version");
            assert!(!daemon_commit.is_empty());
            let mut framed = framed(stream);
            framed
                .send(
                    encode_frame(&Request::Status {
                        provider: None,
                        account: None,
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
            let frame = bounded("Status", framed.next()).await.unwrap().unwrap();
            assert!(
                matches!(decode_frame(&frame).unwrap(), Response::Status(_)),
                "Status answers with the daemon's statuses"
            );
            framed
                .send(encode_frame(&Request::Shutdown).unwrap())
                .await
                .unwrap();
            let socket = scratch.socket();
            wait_for("the daemon to stop", || !socket.exists());
        }

        /// Stop the daemon a bridge spawned, over a direct connection.
        async fn stop_daemon(scratch: &Scratch) {
            let stream = tokio::net::UnixStream::connect(scratch.socket())
                .await
                .unwrap();
            session(stream, scratch).await;
        }

        /// Piped stdio in, the daemon's socket out: the bridge spawns the
        /// daemon nothing was running, relays a session to it, and exits
        /// cleanly once the daemon closes.
        #[tokio::test]
        async fn bridge_relays_a_session_to_the_daemon_it_spawns() {
            let scratch = Scratch::new();
            assert!(!scratch.socket().exists());
            let mut child = bridge(&scratch);
            session(stdio(&mut child), &scratch).await;

            let status = bounded("the bridge to exit", child.wait()).await.unwrap();
            assert!(status.success(), "exit status {status:?}");
        }

        /// The bridge never reads the stream, so the daemon behind it is
        /// what refuses a client of another version, in the reject layout
        /// that names its own version, and the bridge ends cleanly with it.
        #[tokio::test]
        async fn a_version_mismatch_passes_through_the_bridge() {
            let scratch = Scratch::new();
            let mut child = bridge(&scratch);
            let mut stream = stdio(&mut child);

            let mut hello = PROTOCOL_MAGIC.to_vec();
            hello.extend_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
            hello.extend_from_slice(&encode_commit(CLIENT_COMMIT));
            stream.write_all(&hello).await.unwrap();
            let mut reply = [0u8; 3];
            bounded("the reject", stream.read_exact(&mut reply))
                .await
                .unwrap();
            assert_eq!(reply[0], 0x01, "rejected");
            assert_eq!(u16::from_le_bytes([reply[1], reply[2]]), PROTOCOL_VERSION);
            // The daemon left our commit unread, which its socket reports as
            // a reset; the bridge turns that into a plain end of stream.
            let mut rest = Vec::new();
            bounded("the end of stream", stream.read_to_end(&mut rest))
                .await
                .unwrap();
            assert!(rest.is_empty(), "{rest:?}");
            let status = bounded("the bridge to exit", child.wait()).await.unwrap();
            assert!(status.success(), "exit status {status:?}");

            stop_daemon(&scratch).await;
        }

        /// A stand-in for ssh: checks ssh's own options, drops them and the
        /// host, and runs the remote command through a shell as sshd would,
        /// in the scratch environment.
        fn fake_ssh(scratch: &Scratch) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;

            let path = scratch.0.join("fake-ssh");
            let script = format!(
                "#!/bin/sh\n\
                 [ \"$1 $2 $3 $4\" = '-T -o BatchMode=yes box' ] || {{ echo \"fake-ssh: $*\" >&2; exit 64; }}\n\
                 shift 4\n\
                 exec env -i HOME={home} XDG_RUNTIME_DIR={run} XDG_DATA_HOME={data} \
                 XDG_CONFIG_HOME={config} /bin/sh -c \"$*\"\n",
                home = scratch.0.join("home").display(),
                run = scratch.runtime_dir().display(),
                data = scratch.0.join("data").display(),
                config = scratch.0.join("config").display(),
            );
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        /// The client's ssh transport, over a fake ssh that runs `teiryod
        /// bridge` locally: what `teiryo --ssh box` does end to end.
        #[tokio::test]
        async fn a_client_over_ssh_reaches_the_remote_daemon() {
            let scratch = Scratch::new();
            let target = SshTarget::new(
                "box".into(),
                Some(fake_ssh(&scratch).into_os_string()),
                Some(TEIRYOD.into()),
            )
            .unwrap();
            let stream = target.connect().unwrap();
            session(stream, &scratch).await;
        }
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
