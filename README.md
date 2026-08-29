# Teiryo

Teiryō (定量) probes and records LLM subscription quotas for you.

A headless daemon (`teiryod`) polls subscription usage across multiple LLM providers and accounts, persists history in SQLite, and serves it over a Unix domain socket. A terminal UI (`teiryo`) renders live quota dashboards on top. See [docs/](docs/) for the architecture.

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

If a `teiryo` TUI is open while you install, quit and relaunch it — it respawns its own daemon about once a second, from whichever build it was launched from.

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

## Tech Stack

- **Language:** Rust (workspace of 4 crates: `teiryo-core`, `teiryo-providers`, `teiryod`, `teiryo`)
- **Formatter / Linter:** rustfmt + Clippy
- **Tasks / Tools:** mise
- **Git Hooks:** hk

## Git Hooks

This project uses [hk](https://hk.jdx.dev). Pre-commit auto-fixes formatting and lints; commit-msg and pre-push enforce [Conventional Commits](https://www.conventionalcommits.org/) via `convco`; pre-push also runs format check, clippy, and tests. Activate with `hk install` after cloning.

## CI/CD

GitHub Actions runs format checks, clippy, and tests on pushes to `master` and pull requests, plus conventional-commit checks on pull requests.

## License

MIT — see [LICENSE](LICENSE) for details.
