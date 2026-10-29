# Teiryo

Teiryō (定量) probes and records LLM subscription quotas for you.

A headless daemon (`teiryod`) polls subscription usage across multiple LLM providers and accounts, persists history in SQLite, and serves it over a Unix domain socket. A terminal UI (`teiryo`) renders live quota dashboards on top. See [docs/](docs/) for the architecture.

> Status (Sept 8, 2026): This is currently just for myself, so releases are occasional dated snapshots of `master`. Feature requests are accepted as long as it is actionable.

## Prerequisites

- [Rust (rustup)](https://rustup.rs) — toolchain, pinned via `rust-toolchain.toml`
- [mise](https://mise.jdx.dev) — task runner and tool manager; installs everything else (`hk`, `convco`, `pkl`)
- A C compiler — `teiryod` builds SQLite from source (`rusqlite`'s bundled feature). No system SQLite or OpenSSL is needed at runtime.

## Quick Start

```bash
mise install        # install pinned tools
mise run tui        # launch the TUI (spawns the daemon on demand)
```

## Install

```bash
mise run install
```

Builds both crates in release mode and installs them into cargo's bin directory (`$CARGO_HOME/bin`, usually `~/.cargo/bin`) — which must be on your `PATH`. After that, `teiryo` works from any shell.

Both binaries are always installed together, into the same directory. `teiryo` looks for `teiryod` next to its own executable before falling back to `$PATH`, so a TUI installed on its own cannot start its daemon.

Installing is not just copying binaries, because `teiryod` outlives the TUI and the two ship with no protocol compatibility: a daemon left running across an upgrade keeps the socket, and the newly installed TUI exits at the handshake. The task stops the running daemon, starts the newly installed one, and fails loudly if that binary is not the one serving the socket.

Quit every `teiryo` TUI before installing or uninstalling: both tasks refuse while one runs. While its daemon is down a TUI keeps reconnecting — after 1 s, then doubling to every 30 s — and each attempt may spawn whichever `teiryod` sits next to it, racing the task. If the daemon it finds speaks another protocol version it stops reconnecting and says so in its header.

To see which build is installed, ask either binary — neither starts a daemon or touches the terminal to answer:

```console
$ teiryo -V
teiryo 2026.10.17-f78c2ce0f940
commit:   f78c2ce0f940 (2026-10-17T17:52:31+02:00)
profile:  release
rustc:    rustc 1.99.1 (58cf8f9c3 2026-10-13)
built:    2026-10-18T19:55:42Z
protocol: v9
```

The version on the first line is the commit's date in UTC and its hash, `YYYY.MM.DD-<sha12>`: Teiryo has no semver, and every build of one commit reports the same version. `commit` carries a `-dirty` suffix when the tree had uncommitted changes, and so does the version, and any fact the build could not establish reads `unknown`. `protocol` is the wire version: a TUI and daemon whose `protocol` lines differ cannot talk to each other, so comparing `teiryo -V` with `teiryod -V` is the quickest check after an upgrade.

### From a release

Each [GitHub release](https://github.com/justin13888/Teiryo/releases) is a signed tag named by its version, with prebuilt archives for x86_64 and aarch64 Linux (glibc 2.35 or newer) and Apple-silicon macOS. Quit every `teiryo` TUI and stop any running `teiryod` (see above for why). Then extract both binaries into the same directory on your `PATH`, and check that the archive was built by this repository's release workflow:

```bash
gh attestation verify teiryo-<version>-<target>.tar.gz --repo justin13888/Teiryo
```

The macOS binaries are not notarized, so a browser download is quarantined. Clear the quarantine with `xattr -d com.apple.quarantine teiryo teiryod`, or download with `gh release download`, which doesn't set it.

### Remote daemon over SSH

To watch the quotas of a daemon on another machine, install Teiryo there too, then:

```bash
teiryo --ssh user@host
```

Each connection runs `ssh -T -o BatchMode=yes user@host teiryod bridge`, which relays the protocol to that host's daemon and starts it if it is not running; it keeps running after you disconnect. Port, user and keys come from your ssh config, and since no prompt can be answered behind the TUI, the host must accept a key or agent login. The remote daemon still requires the exact same `protocol` version as this client, so install the same Teiryo version on both ends.

| Option | Default | Use |
| --- | --- | --- |
| `--ssh-command CMD` | `$TEIRYO_SSH`, else `ssh` | Another ssh client (one program, not split into words) |
| `--remote-daemon PATH` | `teiryod` on the remote `$PATH` | Where `teiryod` is when a non-interactive ssh shell's `$PATH` lacks it, e.g. `~/.cargo/bin/teiryod` |

The header shows the host after the name. If ssh fails, its last error line is what `teiryo` prints.

To stop the daemon and remove both binaries:

```bash
mise run uninstall
```

## Development

| Command | Description |
| --- | --- |
| `mise run build` | Build the workspace |
| `mise run test` | Run all tests |
| `mise run fmt` | Format code |
| `mise run lint` | Clippy with warnings as errors |
| `mise run check` | Full gate: format check + lint + tests |
| `mise run daemon` | Run `teiryod` in the foreground |
| `mise run tui` | Run the `teiryo` TUI |
| `mise run install` | Install both binaries and restart the daemon |
| `mise run uninstall` | Stop the daemon and remove both binaries |
| `mise run release` | Tag `origin/master`'s head as a signed release and push it |

## Tech Stack

- **Language:** Rust (workspace of 4 crates: `teiryo-core`, `teiryo-providers`, `teiryod`, `teiryo`)
- **Formatter / Linter:** rustfmt + Clippy
- **Tasks / Tools:** mise
- **Git Hooks:** hk

## Git Hooks

This project uses [hk](https://hk.jdx.dev). Pre-commit auto-fixes formatting and lints; commit-msg and pre-push enforce [Conventional Commits](https://www.conventionalcommits.org/) via `convco`; pre-push also runs format check, clippy, and tests. Activate with `hk install` after cloning.

## CI/CD

GitHub Actions runs format checks, clippy, and tests on pushes to `master` and pull requests, plus conventional-commit checks on pull requests.

Releases are cut with `mise run release` on a clean `master` level with `origin`. It creates a signed annotated tag named by the head commit's version and pushes it. The release workflow then publishes nothing until GitHub verifies the tag's signature, the tagged commit is on `master`, the tag's name matches that commit's version, and both built binaries report that version for `-V`. After that it attaches the archives, `SHA256SUMS`, and build-provenance attestations to the release. The signing key must be registered on GitHub as a signing key. The crates are never published to crates.io: none has a version, and all are `publish = false`.

## License

MIT — see [LICENSE](LICENSE) for details.
