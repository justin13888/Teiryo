#!/bin/sh
# Install teiryo and teiryod, then restart the daemon.
#
#   curl -fsSL https://raw.githubusercontent.com/justin13888/Teiryo/master/scripts/install.sh | sh
#   curl -fsSL … | sh -s -- --version 2026.10.29-7e4fed8e9aed --dir ~/bin
#   scripts/install.sh --from-source          # what `mise run install` runs
#
# By default it installs a GitHub release: the archive for this machine's
# target, checked against the release's SHA256SUMS (and its build-provenance
# attestation when an authenticated `gh` is available), into
# ${TEIRYO_INSTALL_DIR:-$HOME/.local/bin}. With --from-source it builds this
# checkout with `cargo install` into cargo's bin directory instead. Everything
# after the binaries land is the same for both.
#
# The directory matters because of `teiryo --ssh`: it runs `teiryod bridge`
# through a non-interactive ssh shell, which sees only the PATH that shell's
# startup files set. A directory added by `mise activate` (where mise's github
# backend puts both binaries) is not on it, so a host that installed Teiryo
# through mise answers `teiryod: not found`. This script installs into a plain
# directory and warns when a non-interactive shell would not find it.
#
# Both binaries, always, into one directory: the TUI spawns `teiryod` from its
# own directory before falling back to $PATH, so installing the TUI alone
# leaves it unable to start its daemon. teiryod goes first, so the TUI is never
# present without it.
#
# An open TUI respawns the daemon within a second of losing it, from whichever
# build that TUI came from, so it races the restart and no outcome of that race
# is right. The script refuses instead: once before installing, so nothing is
# replaced, and again before the restart, for a TUI opened in between.
#
# Debug builds are not counted, here or in `mise run uninstall`. A debug TUI
# (`mise run tui`, `cargo run`) respawns only a debug daemon, which binds
# teiryo-dev.sock and takes teiryo-dev.lock, so neither can race the installed
# daemon's restart or hold its socket. A process is a debug build when its
# executable sits in a cargo `debug` profile directory; one whose executable
# the OS will not name is counted, so nothing that might hold the installed
# socket is passed over.
set -eu

# Everything below is one braced group closed on the last line, so `curl | sh`
# parses all of it before running any: a download cut short is a syntax error,
# not a partial install that stopped the old daemon and started no new one.
{

repo=justin13888/Teiryo

usage() {
  echo "usage: install.sh [--version VERSION] [--dir DIR] | --from-source" >&2
  exit 2
}

from_source=false
version=""
dest="${TEIRYO_INSTALL_DIR:-$HOME/.local/bin}"
dest_given=false
while [ $# -gt 0 ]; do
  case "$1" in
    --from-source) from_source=true ;;
    --version)
      [ $# -ge 2 ] || usage
      version=$2
      shift
      ;;
    --dir)
      [ $# -ge 2 ] || usage
      dest=$2
      dest_given=true
      shift
      ;;
    -h | --help) usage ;;
    *) usage ;;
  esac
  shift
done
if $from_source; then
  # cargo decides where it installs; a --version or --dir would be ignored.
  [ -z "$version" ] && [ "$dest_given" = false ] || usage
  dest="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
fi

# Explicit, because the fallback is worse than failing: without pgrep and ps we
# cannot tell a running daemon from none, would skip the restart below, and
# would then accept the *old* daemon as proof that the new one works. perl is
# how the script takes the daemon's instance lock (macOS has no flock(1)) and,
# where setsid(1) is missing, how it detaches the new daemon. Checked before
# anything is installed, so a missing tool replaces nothing.
for tool in pgrep ps perl; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "install: $tool is required to restart the daemon" >&2
    exit 1
  fi
done

realdir() { CDPATH='' cd -- "$1" && pwd -P; }

uid=$(id -u)

# "Some teiryod is alive" is not the question — the stale one and the installed
# one are both that. Ask which binary it is, wherever the OS will say: Linux
# through /proc, and macOS, which has no /proc, through `ps -o comm=`, which
# there prints the path the process was executed from.
binary_of() {
  if [ -r "/proc/$1/exe" ]; then
    readlink -f "/proc/$1/exe" 2>/dev/null || true
    return
  fi
  comm=$(ps -o comm= -p "$1" 2>/dev/null || true)
  case "$comm" in
    /*) readlink -f "$comm" 2>/dev/null || true ;;
  esac
}

# The pids of this user's processes named $1, less debug builds, which bind
# teiryo-dev.sock and so never touch the installed daemon's socket (see the
# header). A pid whose binary is unknown is kept.
release_pids() {
  for pid in $(pgrep -x -u "$uid" "$1" 2>/dev/null || true); do
    case "$(binary_of "$pid")" in
      */debug/"$1") ;;
      *) echo "$pid" ;;
    esac
  done
}

if [ -n "$(release_pids teiryo)" ]; then
  echo "install: a teiryo TUI is running; quit it and re-run" >&2
  exit 1
fi

if $from_source; then
  # A shared --target-dir lets the second build reuse the first's artifacts
  # instead of recompiling the dependency graph (bundled SQLite included) from
  # scratch; --force is required because every crate is `0.0.0` (a build is
  # versioned by its commit, not by Cargo), so to Cargo every install is a
  # reinstall of the same version.
  cd "$(dirname "$0")/.."
  if [ ! -f crates/teiryod/Cargo.toml ]; then
    echo "install: --from-source runs from a checkout: scripts/install.sh --from-source" >&2
    exit 2
  fi
  cargo install --path crates/teiryod --locked --force --target-dir target
  cargo install --path crates/teiryo --locked --force --target-dir target
else
  for tool in curl tar; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "install: $tool is required to download a release" >&2
      exit 1
    fi
  done
  if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
  elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
  else
    echo "install: sha256sum or shasum is required to check the download" >&2
    exit 1
  fi

  # The targets release.yml builds; anything else has no archive to fetch.
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
    *)
      echo "install: no release is built for $(uname -s) $(uname -m)" >&2
      echo "install: build from a checkout instead: mise run install" >&2
      exit 1
      ;;
  esac

  # The latest release, from where github.com redirects /releases/latest
  # rather than from the API, which rate-limits anonymous callers.
  if [ -z "$version" ]; then
    url=$(curl --proto '=https' -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")
    version=${url##*/}
  fi
  # A release is named by its commit's version (scripts/release-version.sh);
  # anything else is not one, and would otherwise reach a URL and a path. grep
  # matches per line, so a value holding a newline is refused before it.
  nl='
'
  case "$version" in
    *"$nl"*) version_ok=false ;;
    *) version_ok=true ;;
  esac
  if ! $version_ok || ! printf '%s\n' "$version" | grep -Eqx '[0-9]{4}\.[0-9]{2}\.[0-9]{2}-[0-9a-f]{12}'; then
    echo "install: '$version' is not a release version (YYYY.MM.DD-<sha12>)" >&2
    exit 1
  fi

  tmp=$(mktemp -d)
  # dash runs no EXIT trap on a signal, so a signal exits first.
  trap 'rm -rf "$tmp"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  name="teiryo-$version-$target"
  base="https://github.com/$repo/releases/download/$version"
  echo "install: downloading $name"
  curl --proto '=https' -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz"
  curl --proto '=https' -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

  want_sum=$(awk -v f="$name.tar.gz" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
  if [ -z "$want_sum" ]; then
    echo "install: SHA256SUMS of $version lists no $name.tar.gz" >&2
    exit 1
  fi
  if [ "$(sha256 "$tmp/$name.tar.gz")" != "$want_sum" ]; then
    echo "install: $name.tar.gz does not match its SHA256SUMS entry" >&2
    exit 1
  fi
  # The checksum only says the archive is the one the release lists; the
  # attestation says this repository's release workflow built it. It needs an
  # authenticated gh, so without one it is skipped, and said to be. Pinned to
  # release.yml, so an attestation some other workflow of the repository made
  # does not pass for a release.
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh attestation verify "$tmp/$name.tar.gz" --repo "$repo" \
      --signer-workflow "$repo/.github/workflows/release.yml" >/dev/null
    echo "install: attestation verified"
  else
    echo "install: no authenticated gh; the build-provenance attestation was not checked"
  fi

  tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
  # The same check release.yml held the build to, so a renamed or mixed-up
  # archive cannot install binaries of another version than it claims.
  for binary in teiryod teiryo; do
    got=$("$tmp/$name/$binary" -V | sed -n 1p)
    if [ "$got" != "$binary $version" ]; then
      echo "install: $name/$binary -V reports '$got', not '$binary $version'" >&2
      exit 1
    fi
  done

  # Copied beside the target and renamed over it, so a running binary is
  # replaced rather than rewritten in place (which Linux refuses with ETXTBSY).
  mkdir -p "$dest"
  for binary in teiryod teiryo; do
    cp "$tmp/$name/$binary" "$dest/.$binary.new"
    chmod 755 "$dest/.$binary.new"
    mv -f "$dest/.$binary.new" "$dest/$binary"
  done
  echo "install: installed $version into $dest"
fi

for binary in teiryo teiryod; do
  if ! command -v "$binary" >/dev/null 2>&1; then
    echo "install: $binary is installed but not on PATH — add $dest to PATH and re-run" >&2
    if [ "$dest" = "$HOME/.local/bin" ]; then
      echo "install: (on many Linux systems a new login shell adds ~/.local/bin once it exists)" >&2
    fi
    echo "install: the old teiryod, if any, was left running" >&2
    exit 1
  fi
done

tui_dir=$(realdir "$(dirname "$(command -v teiryo)")")
daemon_dir=$(realdir "$(dirname "$(command -v teiryod)")")
if [ "$tui_dir" != "$daemon_dir" ]; then
  echo "install: teiryo ($tui_dir) and teiryod ($daemon_dir) are not in the same directory" >&2
  echo "install: the TUI looks for teiryod next to itself; remove whichever is stale" >&2
  exit 1
fi
if [ "$tui_dir" != "$(realdir "$dest")" ]; then
  echo "install: PATH resolves teiryo to $tui_dir, shadowing what was just installed in $dest" >&2
  exit 1
fi

# What `teiryo --ssh` to this host will find: a login shell's own startup
# files, run as sshd runs a command — non-interactive, from a bare environment.
# SSH_CLIENT is what makes bash read ~/.bashrc there, as it does under sshd.
# An approximation of a real ssh session, so it only warns.
ssh_daemon=$(env -i HOME="$HOME" USER="${USER:-}" SSH_CLIENT="127.0.0.1 0 22" \
  "${SHELL:-/bin/sh}" -c 'command -v teiryod' </dev/null 2>/dev/null || true)
case "$ssh_daemon" in
  /*) ssh_dir=$(realdir "$(dirname "$ssh_daemon")" 2>/dev/null || true) ;;
  *) ssh_dir="" ;;
esac
if [ "$ssh_dir" != "$(realdir "$dest")" ]; then
  echo "install: warning: a non-interactive ${SHELL:-/bin/sh} does not find $dest/teiryod" >&2
  echo "install: warning: \`teiryo --ssh\` to this host will not either; add $dest to PATH in" >&2
  echo "install: warning: your shell's non-interactive startup file (~/.zshenv, or ~/.bashrc" >&2
  echo "install: warning: above any line that returns early for a non-interactive shell)," >&2
  echo "install: warning: or connect with --remote-daemon $dest/teiryod" >&2
fi

daemon="$daemon_dir/teiryod"
# Asked of the binary just installed rather than recomputed here, so the script
# can never disagree with the daemon about where it binds or logs. Asking for
# the log path also creates its directory, which the redirect below needs.
sock=$("$daemon" --print-socket-path)
log=$("$daemon" --print-log-path)
# The daemon's instance lock: the socket's name with a `.lock` extension
# (teiryo_core::paths::lock_path).
lock="${sock%.sock}.lock"

# A pid neither /proc nor ps answers for absolutely is not counted as the new
# daemon, so a stale one is never taken for it.
want=$(readlink -f "$daemon")

# Checked again here, not only before installing: a TUI opened since would
# respawn the daemon this script is about to stop. See the header.
if [ -n "$(release_pids teiryo)" ]; then
  echo "install: a teiryo TUI is running; quit it and re-run" >&2
  echo "install: the binaries are installed, but the old teiryod was left running" >&2
  exit 1
fi

# A TUI reaps the daemon it spawns, but a build from before it did would leave
# one it outlived as a zombie until the TUI itself exited. Zombies answer pgrep
# but hold no socket, and counting one would stall every stop below on a process
# that cannot exit.
live_daemon_pids() {
  for pid in $(release_pids teiryod); do
    state=$(ps -o stat= -p "$pid" 2>/dev/null | tr -d " \t\n" || true)
    case "$state" in
      Z* | "") ;;
      *) echo "$pid" ;;
    esac
  done
}

installed_daemon_running() {
  for pid in $(live_daemon_pids); do
    if [ "$(binary_of "$pid")" = "$want" ]; then
      return 0
    fi
  done
  return 1
}

# A daemon outlives the TUI, and the two ship together with no protocol
# compatibility: left running across an upgrade, the old one keeps the socket
# and the new TUI dies at the handshake. Signalled by pid, so only the
# processes counted here are touched.
pids=$(live_daemon_pids)
if [ -n "$pids" ]; then
  echo "install: stopping the running teiryod"
  # shellcheck disable=SC2086 # one word per pid
  kill -TERM $pids 2>/dev/null || true
  waited=0
  while [ -n "$(live_daemon_pids)" ]; do
    waited=$((waited + 1))
    if [ "$waited" -gt 50 ]; then
      # shellcheck disable=SC2046 # one word per pid
      kill -KILL $(live_daemon_pids) 2>/dev/null || true
      break
    fi
    sleep 0.1
  done
fi

# A socket file still sitting there is debris from a crash or from the SIGKILL
# above, unless a daemon holds it: that is what the instance lock says. Taking
# the lock first and removing the socket while holding it means a live
# daemon's socket is never deleted, and with it gone, the file reappearing
# below is proof that the new daemon bound rather than something the old one
# left behind. Exit 1 from perl is the lock being held; anything else failed.
held=0
# shellcheck disable=SC2016 # perl, not shell, expands these
perl -e '
  use Fcntl qw(:flock O_WRONLY O_CREAT O_NOFOLLOW);
  my ($lock, $sock) = @ARGV;
  sub fail { print STDERR "install: $_[0]: $!\n"; exit 2 }
  sysopen(my $fh, $lock, O_WRONLY | O_CREAT | O_NOFOLLOW, 0600)
    or fail("cannot open $lock");
  flock($fh, LOCK_EX | LOCK_NB) or ($!{EWOULDBLOCK} ? exit 1 : fail("cannot lock $lock"));
  unlink($sock) or $!{ENOENT} or fail("cannot remove $sock");
  exit 0;
' "$lock" "$sock" || held=$?
if [ "$held" = 1 ]; then
  echo "install: a teiryod that could not be stopped still holds $lock" >&2
  exit 1
elif [ "$held" != 0 ]; then
  exit 1
fi

# teiryod does not detach itself — the TUI is what normally starts it in a
# session of its own — so the script has to: in a session of its own, so a
# Ctrl-C during the wait below cannot reach it. Its output is appended to the
# log, where a failure before logging starts would otherwise be lost. macOS
# has no setsid(1); perl's POSIX::setsid is the same call. Neither fails here,
# because a background job of a non-interactive shell never leads its group.
if command -v setsid >/dev/null 2>&1; then
  setsid "$daemon" </dev/null >>"$log" 2>&1 &
else
  # shellcheck disable=SC2016 # perl, not shell, expands these
  perl -MPOSIX -e 'POSIX::setsid() or die "setsid: $!\n"; exec { $ARGV[0] } @ARGV or die "exec $ARGV[0]: $!\n"' \
    "$daemon" </dev/null >>"$log" 2>&1 &
fi

# Both conditions, because either alone accepts what it should not: the socket
# file can outlive a crashed daemon, and a live process may be one that found
# the socket already owned and exited.
waited=0
while :; do
  if [ -S "$sock" ] && installed_daemon_running; then
    echo "install: teiryo and teiryod are in $daemon_dir; teiryod is serving $sock"
    exit 0
  fi
  waited=$((waited + 1))
  if [ "$waited" -gt 100 ]; then
    break
  fi
  sleep 0.1
done

echo "install: the binaries are installed, but $daemon is not serving $sock" >&2
if [ -n "$(release_pids teiryo)" ]; then
  echo "install: a teiryo TUI started meanwhile and is respawning its own daemon — quit it and re-run" >&2
else
  echo "install: see $log" >&2
fi
exit 1

}
