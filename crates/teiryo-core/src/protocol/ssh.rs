//! The remote transport: an ssh child whose far end runs `teiryod bridge`.
//!
//! `teiryo --ssh HOST` runs `ssh -T -o BatchMode=yes HOST teiryod bridge` once
//! per connection and speaks the protocol over the child's stdin and stdout.
//! The bridge copies bytes between its stdio and the remote daemon's socket
//! without reading them, so the handshake reaches the remote daemon unchanged
//! and that daemon enforces exact `PROTOCOL_VERSION` equality as it does for a
//! local client. Port, user and keys come from the user's ssh config.

use std::ffi::OsString;
use std::io;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, Join, ReadBuf};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

/// The ssh client run when neither `--ssh-command` nor [`SSH_ENV`] names one.
pub const DEFAULT_SSH: &str = "ssh";

/// The environment variable that names the ssh client, below `--ssh-command`.
pub const SSH_ENV: &str = "TEIRYO_SSH";

/// The daemon binary run on the remote host when `--remote-daemon` names none:
/// found on the remote `$PATH`.
pub const DEFAULT_REMOTE_DAEMON: &str = "teiryod";

/// Where and how to reach a remote daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// `[user@]host`, passed to ssh as typed.
    pub host: String,
    /// The ssh client to run.
    pub program: OsString,
    /// The `teiryod` to run on the remote host. The remote shell reads it, so
    /// `~/.cargo/bin/teiryod` works.
    pub remote_daemon: String,
}

impl SshTarget {
    /// A target for `host`, with `program` falling back to `$TEIRYO_SSH` and
    /// then `ssh`, and `remote_daemon` to `teiryod`. Refuses an empty host
    /// and one ssh would read as an option.
    pub fn new(
        host: String,
        program: Option<OsString>,
        remote_daemon: Option<String>,
    ) -> Result<Self, String> {
        if host.is_empty() {
            return Err("the --ssh host is empty".to_owned());
        }
        if host.starts_with('-') {
            return Err(format!(
                "'{host}' is not a host: ssh would read it as an option"
            ));
        }
        let program = program
            .or_else(|| std::env::var_os(SSH_ENV).filter(|p| !p.is_empty()))
            .unwrap_or_else(|| OsString::from(DEFAULT_SSH));
        let remote_daemon = remote_daemon.unwrap_or_else(|| DEFAULT_REMOTE_DAEMON.to_owned());
        if remote_daemon.is_empty() {
            return Err("the --remote-daemon path is empty".to_owned());
        }
        Ok(Self {
            host,
            program,
            remote_daemon,
        })
    }

    /// The arguments ssh is run with: no pseudo-terminal (the stream is
    /// binary), never a password or host-key prompt (nobody can answer one
    /// behind the TUI), the host, and the remote command.
    pub fn args(&self) -> Vec<OsString> {
        [
            "-T",
            "-o",
            "BatchMode=yes",
            self.host.as_str(),
            self.remote_daemon.as_str(),
            "bridge",
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    /// Start ssh. The child dies with the returned stream.
    pub fn connect(&self) -> io::Result<SshStream> {
        let mut cmd = Command::new(&self.program);
        cmd.args(self.args());
        start(cmd)
    }
}

/// Run `cmd` with all three of its stdio piped to an [`SshStream`].
fn start(mut cmd: Command) -> io::Result<SshStream> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let piped = || io::Error::other("ssh child stdio was not piped");
    let stdin = child.stdin.take().ok_or_else(piped)?;
    let stdout = child.stdout.take().ok_or_else(piped)?;
    let stderr = child.stderr.take().ok_or_else(piped)?;
    Ok(SshStream {
        io: tokio::io::join(stdout, stdin),
        stderr: Some(tokio::spawn(last_line(stderr))),
        child,
    })
}

/// Read `stderr` to its end, keeping only its last non-blank line that is not
/// a usage error's closing hint. Draining it for the child's whole life also
/// keeps ssh from blocking on a full pipe.
async fn last_line(stderr: ChildStderr) -> Option<String> {
    let mut lines = BufReader::new(stderr).lines();
    let mut last = None;
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if !line.is_empty() && !is_usage_hint(line) {
            last = Some(line.to_owned());
        }
    }
    last
}

/// The line a usage error ends with (`For more information, try '--help'.`),
/// which says nothing about what went wrong: the line before it does.
fn is_usage_hint(line: &str) -> bool {
    line.starts_with("For more information, try")
}

/// Whether ssh's last word is a remote `teiryod` refusing the `bridge`
/// command: an install that predates it, which no retry can fix.
pub fn lacks_bridge(why: &str) -> bool {
    why.contains("unexpected argument 'bridge'")
}

/// A running ssh child, read and written through its stdout and stdin.
pub struct SshStream {
    io: Join<ChildStdout, ChildStdin>,
    stderr: Option<JoinHandle<Option<String>>>,
    child: Child,
}

impl std::fmt::Debug for SshStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshStream").finish_non_exhaustive()
    }
}

impl SshStream {
    /// Why ssh failed, once it has exited: its last stderr line, or its exit
    /// status when it printed nothing. `None` when it is still running after
    /// `wait`, which means the failure was not ssh's. Meant for a connection
    /// that ended before the handshake did; asked twice, it answers `None`.
    pub async fn failure(&mut self, wait: Duration) -> Option<String> {
        let stderr = self.stderr.take()?;
        let deadline = tokio::time::Instant::now() + wait;
        // Stderr closes when ssh exits (or hands it to a child that does).
        let line = tokio::time::timeout_at(deadline, stderr).await.ok()?.ok()?;
        let status = tokio::time::timeout_at(deadline, self.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        Some(describe(line, status))
    }
}

/// The user-facing reason for a failed ssh: what it said, else how it exited.
fn describe(line: Option<String>, status: Option<ExitStatus>) -> String {
    match (line, status) {
        (Some(line), _) => line,
        (None, Some(status)) => format!("ssh exited ({status}) without a message"),
        (None, None) => "ssh closed the connection without a message".to_owned(),
    }
}

impl AsyncRead for SshStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn target(host: &str) -> Result<SshTarget, String> {
        SshTarget::new(host.to_owned(), Some("ssh".into()), None)
    }

    #[test]
    fn ssh_runs_the_bridge_without_a_tty_or_prompts() {
        let target = SshTarget::new("me@box".into(), Some("ssh".into()), None).unwrap();
        assert_eq!(
            target.args(),
            ["-T", "-o", "BatchMode=yes", "me@box", "teiryod", "bridge"].map(OsString::from)
        );
        let custom = SshTarget::new(
            "box".into(),
            Some("/opt/ssh".into()),
            Some("~/.cargo/bin/teiryod".into()),
        )
        .unwrap();
        assert_eq!(custom.program, "/opt/ssh");
        assert_eq!(custom.args()[4], "~/.cargo/bin/teiryod");
    }

    /// A host ssh would parse as an option is refused before ssh ever runs.
    #[test]
    fn option_shaped_and_empty_hosts_are_refused() {
        assert!(target("-oProxyCommand=x").is_err());
        assert!(target("").is_err());
        assert!(target("box").is_ok());
        assert!(SshTarget::new("box".into(), None, Some(String::new())).is_err());
    }

    /// A fake ssh: a shell running `script`, with stdio piped as ssh's is.
    fn fake(script: &str) -> SshStream {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        start(cmd).unwrap()
    }

    #[tokio::test]
    async fn bytes_cross_the_child_both_ways() {
        let mut stream = fake("exec cat");
        stream.write_all(b"TEIR").await.unwrap();
        let mut back = [0u8; 4];
        stream.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"TEIR");
    }

    /// What the user reads when ssh fails is ssh's own last word.
    #[tokio::test]
    async fn a_failed_ssh_reports_its_last_stderr_line() {
        let mut stream = fake(
            "echo 'Warning: something' >&2; \
             echo 'ssh: Could not resolve hostname box: Name or service not known' >&2; \
             exit 255",
        );
        let mut sink = Vec::new();
        stream.read_to_end(&mut sink).await.unwrap();
        assert_eq!(
            stream.failure(Duration::from_secs(5)).await.as_deref(),
            Some("ssh: Could not resolve hostname box: Name or service not known")
        );
    }

    /// A remote teiryod without `bridge` ends its usage error with a hint;
    /// the line kept is the one naming `bridge`, which marks the old install.
    #[tokio::test]
    async fn a_remote_usage_error_reports_the_line_naming_bridge() {
        let mut stream = fake(
            "printf \"teiryod: unexpected argument 'bridge'\\n\\n\
             For more information, try '--help'.\\n\" >&2; exit 2",
        );
        let failure = stream.failure(Duration::from_secs(5)).await.unwrap();
        assert_eq!(failure, "teiryod: unexpected argument 'bridge'");
        assert!(lacks_bridge(&failure));
        assert!(!lacks_bridge(
            "ssh: connect to host box port 22: Connection refused"
        ));
    }

    #[tokio::test]
    async fn a_silent_ssh_reports_its_exit_status() {
        let mut stream = fake("exit 255");
        let failure = stream.failure(Duration::from_secs(5)).await.unwrap();
        assert!(failure.contains("255"), "{failure}");
    }

    /// The falsifier: an ssh still running is not blamed.
    #[tokio::test]
    async fn a_running_ssh_is_not_a_failure() {
        let mut stream = fake("exec sleep 30");
        assert_eq!(stream.failure(Duration::from_millis(100)).await, None);
    }
}
