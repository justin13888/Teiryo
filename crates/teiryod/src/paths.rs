//! XDG path resolution and Unix-socket binding with single-instance handling.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use teiryo_core::paths::{lock_path, socket_path, DATA_DIR_NAME};

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
    /// Resolve the socket through [`teiryo_core::paths::socket_path`], which
    /// the TUI shares, `{teiryo.db,teiryo.log}` under
    /// [`DATA_DIR_NAME`] in the platform's data dir (`teiryo`, or
    /// `teiryo-dev` in a debug build), and `teiryo/config.toml` in its config
    /// dir, which every build shares. Creates the data/config dirs.
    pub fn resolve() -> io::Result<Self> {
        let home = || io::Error::other("cannot resolve home directory");
        let data = ProjectDirs::from("", "", DATA_DIR_NAME)
            .ok_or_else(home)?
            .data_dir()
            .to_path_buf();
        let config_dir = ProjectDirs::from("", "", "teiryo")
            .ok_or_else(home)?
            .config_dir()
            .to_path_buf();
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

/// Outcome of trying to become the single daemon instance.
pub enum BindOutcome {
    /// We own the socket and may serve. The listener is already non-blocking,
    /// so the caller hands it to `tokio::net::UnixListener::from_std` — from
    /// *inside* the runtime, since that call panics without a reactor. The
    /// guard holds the instance lock and unlinks the socket when dropped, so
    /// it must live as long as the daemon does.
    Bound(std::os::unix::net::UnixListener, SocketGuard),
    /// Another daemon holds the instance lock or answers on the socket; exit 0.
    AlreadyRunning,
}

/// Ownership of the daemon socket for the life of the process.
///
/// Holds the exclusive `flock` on the lock file beside the socket, and on drop
/// unlinks the socket only while it is still the inode this process bound:
/// an orphan that outlived its socket must not delete the live daemon's.
/// Dropping the guard releases the lock after the unlink, never before.
pub struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
    _lock: File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.dev() == self.dev && meta.ino() == self.ino {
                std::fs::remove_file(&self.path).ok();
            }
        }
    }
}

/// Take the instance lock beside `socket`, without blocking. `None` means
/// another process holds it. The file is opened `O_NOFOLLOW`, so a symlink
/// planted at the lock's name cannot redirect the create elsewhere.
fn acquire_lock(socket: &Path) -> io::Result<Option<File>> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path(socket))?;
    // SAFETY: the descriptor is open for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(err)
    }
}

/// Bind the daemon socket as the single instance.
///
/// The exclusive `flock` on `teiryo.lock` comes first and is held for the
/// process lifetime: losing it means another daemon is starting or running,
/// so this one is `AlreadyRunning`. Only the lock holder ever unlinks a stale
/// socket, which is what stops two daemons started together from each
/// unlinking the other's fresh one. A socket that still answers despite the
/// lock is a daemon from a build without it, and is left alone. The socket
/// file is chmod'd to 0600.
///
/// Deliberately reactor-free: nothing here needs the runtime.
pub fn bind_socket(path: &Path) -> io::Result<BindOutcome> {
    let Some(lock) = acquire_lock(path)? else {
        return Ok(BindOutcome::AlreadyRunning);
    };
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
    let meta = std::fs::symlink_metadata(path)?;
    // Created before the remaining fallible steps, so a failure there still
    // unlinks the socket this call bound.
    let guard = SocketGuard {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        _lock: lock,
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(BindOutcome::Bound(listener, guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique socket path per test, kept out of the real runtime dir.
    fn temp_socket() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("teiryod-paths-{}-{n}.sock", std::process::id()))
    }

    /// Remove what a test left behind: the socket, if any, and the lock file,
    /// which nothing in the daemon ever removes.
    fn cleanup(path: &Path) {
        std::fs::remove_file(path).ok();
        std::fs::remove_file(lock_path(path)).ok();
    }

    /// Deliberately a plain `#[test]`: nothing in `bind_socket` may need the
    /// runtime, or it would panic on every start, before `tracing` can record
    /// why.
    #[test]
    fn binds_without_a_runtime() {
        let path = temp_socket();
        let Ok(BindOutcome::Bound(_listener, _guard)) = bind_socket(&path) else {
            panic!("a fresh path should bind");
        };
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "socket must be owner-only");
        let lock_mode = std::fs::metadata(lock_path(&path))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(lock_mode & 0o777, 0o600, "lock must be owner-only");
        cleanup(&path);
    }

    #[test]
    fn second_binder_reports_already_running() {
        let path = temp_socket();
        let Ok(BindOutcome::Bound(_listener, _guard)) = bind_socket(&path) else {
            panic!("first bind should own the socket");
        };
        assert!(matches!(
            bind_socket(&path),
            Ok(BindOutcome::AlreadyRunning)
        ));
        assert!(
            path.exists(),
            "the loser must not touch the winner's socket"
        );
        cleanup(&path);
    }

    /// The race the lock closes: a daemon that holds the lock but has not yet
    /// bound. The connect probe alone would find no socket and bind a second
    /// daemon; the lock turns the loser away before it looks.
    #[test]
    fn held_lock_wins_before_any_socket_exists() {
        let path = temp_socket();
        let Some(_lock) = acquire_lock(&path).unwrap() else {
            panic!("a fresh lock should be free");
        };
        assert!(matches!(
            bind_socket(&path),
            Ok(BindOutcome::AlreadyRunning)
        ));
        assert!(!path.exists(), "the loser must not bind");
        cleanup(&path);
    }

    /// A daemon from a build that predates the lock answers on the socket
    /// without holding it, and is left serving.
    #[test]
    fn live_socket_without_the_lock_reports_already_running() {
        let path = temp_socket();
        let _old = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(matches!(
            bind_socket(&path),
            Ok(BindOutcome::AlreadyRunning)
        ));
        assert!(path.exists());
        cleanup(&path);
    }

    #[test]
    fn stale_socket_is_unlinked_and_rebound() {
        let path = temp_socket();
        // A `kill -9`'d daemon leaves the file on disk with nothing listening,
        // and the kernel releases its lock.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "dropping a listener keeps the socket file");
        assert!(matches!(bind_socket(&path), Ok(BindOutcome::Bound(..))));
        cleanup(&path);
    }

    #[test]
    fn dropping_the_guard_unlinks_and_releases() {
        let path = temp_socket();
        let Ok(BindOutcome::Bound(listener, guard)) = bind_socket(&path) else {
            panic!("first bind should own the socket");
        };
        drop(listener);
        drop(guard);
        assert!(!path.exists(), "the owner removes its own socket");
        assert!(
            matches!(bind_socket(&path), Ok(BindOutcome::Bound(..))),
            "the lock is released with the guard"
        );
        cleanup(&path);
    }

    /// The falsifier for the test above: a socket replaced since the bind is
    /// someone else's, and survives the guard.
    #[test]
    fn guard_leaves_a_replaced_socket_in_place() {
        let path = temp_socket();
        let Ok(BindOutcome::Bound(_listener, guard)) = bind_socket(&path) else {
            panic!("first bind should own the socket");
        };
        // Bound elsewhere and renamed over the original, so both inodes exist
        // at once and the replacement cannot reuse the original's number.
        let staged = temp_socket();
        let _other = std::os::unix::net::UnixListener::bind(&staged).unwrap();
        std::fs::rename(&staged, &path).unwrap();
        drop(guard);
        assert!(path.exists(), "a replaced socket must not be unlinked");
        cleanup(&path);
    }
}
