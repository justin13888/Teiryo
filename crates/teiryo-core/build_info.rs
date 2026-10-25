// Build script shared by the `teiryo` and `teiryod` binaries, pulled into each
// crate's `build.rs` with `include!`. It captures the commit a binary was built
// from, the profile, the compiler and the build time, and exposes them as
// compile-time env vars that `teiryo_core::build_info!` reads for `-V`.
//
// It sits beside `src/version.rs`, which renders what it captures, but it is
// not `teiryo-core`'s own build script: one there would rebuild the whole
// workspace on every commit, where the binaries only relink.
//
// Every fact is best-effort. In a source tarball, or a checkout with no `.git`,
// the value falls back to `unknown` rather than failing the build.

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // An explicit hash wins over the local probe, for builds from an exported
    // tree that has no git metadata to ask.
    // Exactly 12 characters, not `--short=12`: that grows where the clone holds
    // an ambiguous prefix, and the hash is part of the version a release tag
    // must equal (`teiryo_core::version::release_version`). The override is
    // used verbatim, so it must be those same 12 characters to match a build
    // from git.
    let commit = env_override("TEIRYO_BUILD_SHA").or_else(|| {
        git(&["rev-parse", "HEAD"]).map(|sha| {
            let sha: String = sha.chars().take(12).collect();
            if is_dirty() {
                format!("{sha}-dirty")
            } else {
                sha
            }
        })
    });
    let commit_date =
        env_override("TEIRYO_BUILD_COMMIT_DATE").or_else(|| git(&["log", "-1", "--format=%cI"]));

    emit("TEIRYO_COMMIT_HASH", commit.as_deref());
    emit("TEIRYO_COMMIT_DATE", commit_date.as_deref());
    emit("TEIRYO_BUILD_PROFILE", env_override("PROFILE").as_deref());
    emit("TEIRYO_RUSTC_VERSION", rustc_version().as_deref());
    emit("TEIRYO_BUILD_EPOCH", build_epoch().as_deref());

    rerun_on_new_commit();
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=../teiryo-core/build_info.rs");
    println!("cargo::rerun-if-env-changed=TEIRYO_BUILD_SHA");
    println!("cargo::rerun-if-env-changed=TEIRYO_BUILD_COMMIT_DATE");
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

/// Ask Cargo to rerun when the commit moves. Watching `HEAD` alone only sees a
/// branch switch: a commit on the current branch rewrites the ref `HEAD` points
/// to, and `git gc` moves refs into `packed-refs`. In a linked worktree `HEAD`
/// is per-worktree while refs live in the common dir, so each is resolved
/// separately. Only existing files are watched: Cargo reruns on every build for
/// a path that does not exist.
fn rerun_on_new_commit() {
    let mut watched = Vec::new();
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        watched.push(Path::new(&git_dir).join("HEAD"));
    }
    if let Some(common_dir) = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watched.push(Path::new(&common_dir).join(head_ref));
        }
        watched.push(Path::new(&common_dir).join("packed-refs"));
    }
    for path in watched.iter().filter(|p| p.exists()) {
        println!("cargo::rerun-if-changed={}", path.display());
    }
}

/// `rustc --version` of the compiler Cargo selected for this build.
fn rustc_version() -> Option<String> {
    let rustc = env_override("RUSTC").unwrap_or_else(|| "rustc".to_owned());
    let out = Command::new(rustc).arg("--version").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// When this build ran, in seconds since the Unix epoch. `SOURCE_DATE_EPOCH`
/// wins, so a reproducible build can pin it. Emitted raw: `teiryo-core`
/// renders it, where the formatting is tested.
fn build_epoch() -> Option<String> {
    env_override("SOURCE_DATE_EPOCH")
        .filter(|v| v.parse::<u64>().is_ok())
        .or_else(|| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
            Some(now.as_secs().to_string())
        })
}

/// A trimmed, non-empty environment variable, or `None`.
fn env_override(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn emit(key: &str, value: Option<&str>) {
    println!("cargo::rustc-env={key}={}", value.unwrap_or("unknown"));
}

/// Runs `git` with `args`, returning trimmed stdout on success.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Whether the working tree has uncommitted changes.
fn is_dirty() -> bool {
    git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty())
}
