//! Where the daemon's socket lives. The daemon binds it and the TUI connects
//! to it, so both binaries resolve it here: two copies of the rule drifted
//! once already, and a disagreement shows up only as a TUI that spawns a
//! daemon it can never reach.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The socket's file name inside a per-user directory.
pub const SOCKET_NAME: &str = "teiryo.sock";

/// The longest socket path, in bytes, that every supported platform can bind:
/// `sun_path` holds 104 bytes on macOS (108 on Linux), terminator included.
pub const MAX_SOCKET_PATH: usize = 103;

/// The daemon's socket path.
///
/// - macOS: the per-user temporary directory (`confstr(_CS_DARWIN_USER_TEMP_DIR)`,
///   falling back to `$TMPDIR`), which the system creates mode 0700.
/// - Elsewhere: `$XDG_RUNTIME_DIR` when it is absolute.
///
/// Both fall back to `/tmp/teiryo-$UID.sock` when nothing better exists or the
/// candidate would be too long to bind.
pub fn socket_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        from_darwin_temp_dir(
            darwin_user_temp_dir().as_deref(),
            std::env::var_os("TMPDIR").as_deref(),
            uid(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        from_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), uid())
    }
}

/// The instance lock beside `socket`: the same name with a `.lock` extension.
/// It is never unlinked, because a lock file removed while another process
/// holds it lets a third lock a fresh inode and run alongside it.
pub fn lock_path(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

/// The macOS rule, with its inputs passed in so every platform can test it.
pub fn from_darwin_temp_dir(confstr: Option<&Path>, tmpdir: Option<&OsStr>, uid: u32) -> PathBuf {
    confstr
        .and_then(candidate)
        .or_else(|| tmpdir.and_then(|dir| candidate(Path::new(dir))))
        .unwrap_or_else(|| fallback(uid))
}

/// The non-macOS rule, with its inputs passed in so every platform can test it.
/// A relative `$XDG_RUNTIME_DIR` is ignored, as the XDG spec requires.
pub fn from_runtime_dir(runtime_dir: Option<&OsStr>, uid: u32) -> PathBuf {
    runtime_dir
        .and_then(|dir| candidate(Path::new(dir)))
        .unwrap_or_else(|| fallback(uid))
}

/// `dir/teiryo.sock`, when `dir` is absolute and the result can be bound.
fn candidate(dir: &Path) -> Option<PathBuf> {
    if !dir.is_absolute() {
        return None;
    }
    let path = dir.join(SOCKET_NAME);
    (path.as_os_str().as_bytes().len() <= MAX_SOCKET_PATH).then_some(path)
}

fn fallback(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/teiryo-{uid}.sock"))
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// The per-user temporary directory macOS assigns this user, independent of
/// the environment the process was started with.
#[cfg(target_os = "macos")]
fn darwin_user_temp_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: `buf` is writable for `buf.len()` bytes, and confstr writes at
    // most that many, NUL included.
    let needed = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    // 0 is an error; more than the buffer means it was truncated.
    if needed == 0 || needed > buf.len() {
        return None;
    }
    buf.truncate(needed - 1);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn darwin_prefers_confstr_then_tmpdir() {
        let confstr = Path::new("/var/folders/ab/cd/T/");
        assert_eq!(
            from_darwin_temp_dir(Some(confstr), Some(OsStr::new("/elsewhere")), 501),
            PathBuf::from("/var/folders/ab/cd/T/teiryo.sock")
        );
        assert_eq!(
            from_darwin_temp_dir(None, Some(OsStr::new("/private/tmp/x")), 501),
            PathBuf::from("/private/tmp/x/teiryo.sock")
        );
        assert_eq!(
            from_darwin_temp_dir(None, None, 501),
            PathBuf::from("/tmp/teiryo-501.sock")
        );
        assert_eq!(
            from_darwin_temp_dir(None, Some(OsStr::new("relative")), 501),
            PathBuf::from("/tmp/teiryo-501.sock"),
            "a relative TMPDIR is not a place to bind"
        );
    }

    #[test]
    fn runtime_dir_must_be_absolute() {
        assert_eq!(
            from_runtime_dir(Some(OsStr::new("/run/user/1000")), 1000),
            PathBuf::from("/run/user/1000/teiryo.sock")
        );
        assert_eq!(
            from_runtime_dir(None, 1000),
            PathBuf::from("/tmp/teiryo-1000.sock")
        );
        assert_eq!(
            from_runtime_dir(Some(OsStr::new("")), 1000),
            PathBuf::from("/tmp/teiryo-1000.sock"),
            "an empty XDG_RUNTIME_DIR is unset"
        );
        assert_eq!(
            from_runtime_dir(Some(OsStr::new("run/user/1000")), 1000),
            PathBuf::from("/tmp/teiryo-1000.sock"),
            "a relative XDG_RUNTIME_DIR is ignored"
        );
    }

    /// A directory whose socket path would not fit in `sun_path` is passed
    /// over, so the bind cannot fail with a path the user never chose.
    #[test]
    fn too_long_a_directory_falls_back() {
        let fits = format!(
            "/{}",
            "a".repeat(MAX_SOCKET_PATH - 1 - SOCKET_NAME.len() - 1)
        );
        let path = from_runtime_dir(Some(OsStr::new(&fits)), 7);
        assert_eq!(path.as_os_str().len(), MAX_SOCKET_PATH);
        assert!(path.ends_with(SOCKET_NAME));

        let long = format!("{fits}a");
        assert_eq!(
            from_runtime_dir(Some(OsStr::new(&long)), 7),
            PathBuf::from("/tmp/teiryo-7.sock")
        );
        assert_eq!(
            from_darwin_temp_dir(Some(Path::new(&long)), Some(OsStr::new(&long)), 7),
            PathBuf::from("/tmp/teiryo-7.sock")
        );
    }

    #[test]
    fn this_platform_resolves_a_bindable_path() {
        let path = socket_path();
        assert!(path.is_absolute(), "{}", path.display());
        assert!(
            path.as_os_str().len() <= MAX_SOCKET_PATH,
            "{} is too long to bind",
            path.display()
        );
        assert_eq!(
            lock_path(&path).parent(),
            path.parent(),
            "the lock sits beside the socket"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_the_per_user_temp_dir() {
        let dir = darwin_user_temp_dir().expect("macOS assigns every user a temp dir");
        assert_eq!(socket_path(), dir.join(SOCKET_NAME));
    }

    #[test]
    fn lock_replaces_the_extension() {
        assert_eq!(
            lock_path(Path::new("/run/user/1000/teiryo.sock")),
            PathBuf::from("/run/user/1000/teiryo.lock")
        );
        assert_eq!(
            lock_path(Path::new("/tmp/teiryo-501.sock")),
            PathBuf::from("/tmp/teiryo-501.lock")
        );
    }
}
