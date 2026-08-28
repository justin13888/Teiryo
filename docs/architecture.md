# Architecture

## Process model

- Two processes: `teiryod` (daemon) and `teiryo` (TUI client). Providers are compiled in — no plugin or dylib loading.
- **Startup**: `teiryo` spawns `teiryod` on first connect if the socket is unreachable (`setsid`-detached, stdout/stderr redirected to the log file) — tmux's client/server pattern. No manual double-fork.
- **Single instance**: the UDS bind *is* the lock. `EADDRINUSE` → a daemon is already running → exit 0. No separate pidfile/lockfile.
- **Stale socket**: on bind failure, try connecting as a client first; `ECONNREFUSED` means the socket is stale from a crash → unlink and rebind.
- **Shutdown**: SIGTERM/SIGINT → flush DB, unlink socket. The TUI quitting never kills the daemon; daemon shutdown is only via the explicit `Shutdown` request or a signal.

## Crate layout

```
teiryo/                    # workspace root
  crates/
    teiryo-core/           # domain types, wire protocol, adapter traits, storage
    teiryo-providers/      # claude, openai, ... — implement teiryo-core traits, own their deps
    teiryod/               # daemon bin: scheduler, IPC server, wires providers + storage
    teiryo/                # TUI bin: ratatui + crossterm
```

**Dependency rules (invariant):**

- `teiryo` (TUI) depends only on `teiryo-core`. It must never gain provider internals, HTTP, DB, or scheduling code.
- Provider adapters live in `teiryo-providers`, not `teiryo-core`, so credential-store dependencies and per-provider trait impls stay out of the light, stable crate the TUI links against.
- The adapter registry is a `Vec<Box<dyn ProviderAdapter>>` built by hand in `teiryod`'s `main()` — the provider count is small; no registration framework.

## Scheduler & live updates

- One polling task per **(provider, account)** pair — a probe returns all of that account's windows in one call, so per-provider tasks would be too coarse and per-window tasks too fine. Tasks are spawned/aborted as `Authenticator::discover_accounts()` adds/removes accounts; handles live in `HashMap<(ProviderId, AccountId), TaskHandle>` in daemon state.
- Each task owns an `mpsc::UnboundedSender<PollTrigger>`; its loop is `select! { _ = interval.tick() => Scheduled, Some(t) = rx.recv() => t }`. A manual `PollNow` is a message injected into the same loop, tagged with the requesting client — no separate code path around the timer.
- **Live updates**: one global `tokio::sync::watch::Sender<Option<PollEvent>>`, published after every completed probe. `AwaitUpdate { since, timeout_ms }` compares against the watch's current `PollId`; returns immediately if newer than `since`, otherwise awaits `.changed()` under `tokio::time::timeout`. One built-in channel, no pub/sub crate, no per-subscriber bookkeeping. A single global channel (not per-account) is deliberate: clients wake for accounts they aren't viewing, which is free to ignore.
- Each task also owns a `watch::Receiver<Schedule>` (`{ enabled, interval }`), re-read every cycle. Cadence is therefore **not** captured by value at spawn: publishing to the channel is picked up by the running task, and a `changed()` arm re-arms the sleep so a shortened interval takes effect at once rather than after the old one finally elapses. A disabled provider's tasks **park** on that channel instead of ticking and discarding.
- Poll jitter is recomputed each cycle, not fixed per task.

## Async runtime & HTTP

- `tokio` with the `current_thread` flavor. Tasks are numerous but I/O-bound and idle almost always.
- `reqwest` with `rustls-tls` (no system OpenSSL).
- One persistent `reqwest::Client` per **account** — separate accounts must not share a connection pool or cookie jar. Clients are reused across probes for connection fidelity (matching the real client's connection-reuse behavior).
- Header order/UA and connection-reuse fidelity to each provider's real client is a v1 goal. Exact TLS-stack fingerprint (JA3) match is an accepted gap — not solved via spoofing.

## Config & paths

- XDG resolution via the `directories` crate.
- `$XDG_CONFIG_HOME/teiryo/config.toml` — per-provider enable/disable, poll interval overrides, and `[[account]]` entries for providers that cannot be auto-discovered (credential pasted, encrypted at rest).
- `$XDG_DATA_HOME/teiryo/teiryo.db` and `$XDG_DATA_HOME/teiryo/teiryo.log`.
- `$XDG_RUNTIME_DIR/teiryo.sock`, fallback `/tmp/teiryo-$UID.sock`. Socket file mode 0600.

## Logging & errors

- `tracing` → log file (the daemon has no useful stdout once detached).
- `thiserror` for errors crossing the IPC boundary (`Response::Err(ErrorKind, String)`); `anyhow` for internal daemon glue.
- Credentials use `secrecy::SecretString`: no `Debug`/`Display` leakage, zeroized on drop. Never derive or hand-write `Debug`/`Display` on anything holding a credential.
