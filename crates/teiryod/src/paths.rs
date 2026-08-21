//! XDG path resolution and Unix-socket binding with single-instance handling.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use directories::{BaseDirs, ProjectDirs};

/// Resolved filesystem locations for one daemon run.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Unix socket the daemon listens on.
    pub socket: PathBuf,
    /// SQLite database file.
    pub db: PathBuf,
    /// Log file.
    pub log: PathBuf,
    /// Config file (may not exist).
    pub config: PathBuf,
}

impl Paths {
    /// Resolve per §13: `$XDG_RUNTIME_DIR/teiryo.sock` (fallback
    /// `/tmp/teiryo-$UID.sock`), `$XDG_DATA_HOME/teiryo/{teiryo.db,teiryo.log}`,
    /// `$XDG_CONFIG_HOME/teiryo/config.toml`. Creates the data/config dirs.
    pub fn resolve() -> io::Result<Self> {
        let dirs = ProjectDirs::from("", "", "teiryo")
            .ok_or_else(|| io::Error::other("cannot resolve home directory"))?;
        let data = dirs.data_dir().to_path_buf();
        let config_dir = dirs.config_dir().to_path_buf();
        std::fs::create_dir_all(&data)?;
        std::fs::create_dir_all(&config_dir)?;
        Ok(Self {
            socket: socket_path(),
            db: data.join("teiryo.db"),
            log: data.join("teiryo.log"),
            config: config_dir.join("config.toml"),
        })
    }
}

/// Socket path: `$XDG_RUNTIME_DIR/teiryo.sock`, else `/tmp/teiryo-$UID.sock`.
pub fn socket_path() -> PathBuf {
    if let Some(base) = BaseDirs::new() {
        if let Some(runtime) = base.runtime_dir() {
            return runtime.join("teiryo.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/teiryo-{uid}.sock"))
}

/// Outcome of trying to become the single daemon instance.
pub enum BindOutcome {
    /// We own the socket and may serve.
    Bound(tokio::net::UnixListener),
    /// A live daemon already owns the socket; exit 0.
    AlreadyRunning,
}

/// Bind the daemon socket. The bind *is* the single-instance lock: on
/// `EADDRINUSE` we try connecting — success means another daemon is live;
/// `ECONNREFUSED` means the socket is stale from a crash, so unlink and
/// rebind. The socket file is chmod'd to 0600.
pub fn bind_socket(path: &std::path::Path) -> io::Result<BindOutcome> {
    let listener = match std::os::unix::net::UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => return Ok(BindOutcome::AlreadyRunning),
                Err(ce) if ce.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?;
                    std::os::unix::net::UnixListener::bind(path)?
                }
                Err(ce) => return Err(ce),
            }
        }
        Err(e) => return Err(e),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(BindOutcome::Bound(tokio::net::UnixListener::from_std(
        listener,
    )?))
}
