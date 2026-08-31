# Architecture

## Process model

- Two processes: `teiryod` (daemon) and `teiryo` (TUI client). Providers are compiled in — no plugin or dylib loading.
- **Startup**: `teiryo` spawns `teiryod` on first connect if the socket is unreachable (its own process group, stdout/stderr redirected to the log file, reaped in the background so it never lingers as a zombie) — tmux's client/server pattern. No manual double-fork.
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
- Discovery runs for **every** compiled-in provider, including ones disabled in config: it is a local credential read, and skipping it would leave a disabled provider with no accounts and no task, so enabling it could not take effect without a daemon restart. `enabled` gates polling, in the task, not discovery.
- Each task owns an `mpsc::UnboundedSender<PollTrigger>`; its loop is `select! { _ = interval.tick() => Scheduled, Some(t) = rx.recv() => t }`. A manual `PollNow` is a message injected into the same loop, tagged with the requesting client — no separate code path around the timer.
- Each task also owns a `watch::Receiver<Schedule>` (`{ enabled, interval }`), re-read every cycle. Cadence is therefore **not** captured by value at spawn: a config reload publishes to the channel and the running task picks it up, and a `changed()` arm re-arms the sleep so a shortened interval takes effect at once rather than after the old one finally elapses. A disabled provider's tasks **park** on that channel instead of ticking and discarding, and `PollNow` against a disabled provider is refused rather than queued into a parked task.
- **Live updates**: one global `tokio::sync::watch::Sender<Option<PollEvent>>`, published after every completed probe, and a second `watch::Sender<u64>` carrying the config generation. `AwaitUpdate { since, config_gen, timeout_ms }` waits on both and returns whichever is already newer, or whichever lands first. Two built-in channels, no pub/sub crate, no per-subscriber bookkeeping. A single global poll channel (not per-account) is deliberate: clients wake for accounts they aren't viewing, which is free to ignore.
- Poll jitter is recomputed each cycle, not fixed per task. The base cadence defaults to **180 s**: quota figures move slowly enough that a minute of extra resolution buys nothing actionable, while costing three times the requests against an endpoint that rate limits.
- **Rate-limit backoff** lives in the poll task, next to the jitter, because it is a property of the credential rather than of the config — it must not travel over the `Schedule` channel or a config reload would clear it. Each consecutive `RateLimited` outcome doubles the wait (capped at 1 h; a provider's own `Retry-After` wins over both the doubling and the cap), and any other outcome clears the strikes outright. The throttled interval is never shorter than the configured cadence. A manual `PollNow` is *not* held back — the user asked for that one, and a success through it clears a stale backoff. Tasks republish the throttled interval into `poll_intervals`, so a client's "next poll in" countdown reflects the backoff instead of hitting zero and sitting there.

## Async runtime & HTTP

- `tokio` with the `current_thread` flavor. Tasks are numerous but I/O-bound and idle almost always.
- `reqwest` with `rustls-tls` (no system OpenSSL).
- One persistent `reqwest::Client` per **account** — separate accounts must not share a connection pool or cookie jar. Clients are reused across probes for connection fidelity (matching the real client's connection-reuse behavior).
- Header order/UA and connection-reuse fidelity to each provider's real client is a v1 goal. Exact TLS-stack fingerprint (JA3) match is an accepted gap — not solved via spoofing.

## Config & paths

- XDG resolution via the `directories` crate.
- `$XDG_CONFIG_HOME/teiryo/config.toml` — per-provider enable/disable, poll interval overrides, and (deferred) `[[account]]` entries for providers that cannot be auto-discovered.
- `$XDG_DATA_HOME/teiryo/teiryo.db` and `$XDG_DATA_HOME/teiryo/teiryo.log`.
- `$XDG_RUNTIME_DIR/teiryo.sock`, fallback `/tmp/teiryo-$UID.sock`. Socket file mode 0600.

### Hot reload

The daemon watches `config.toml` (`notify`, on the containing **directory** — editors replace config files by rename, which leaves a file watch pointing at a dead inode; a ~200 ms debounce coalesces the write burst one save produces) and re-reads it on every change.

**One apply path.** Startup, `SetConfig`, and the watcher all funnel through the same `Daemon::apply_config`, which republishes every account's `Schedule` and bumps the generation. There is one place where a setting becomes real, so the three cannot drift.

**The daemon is the only writer clients go through, but not the only writer.** `SetConfig` writes the file with `toml_edit` (format-preserving — the file is hand-written and hand-commented, and a settings tweak that ate the comments would be a worse bug than the one it fixed) via a temp file and a rename. The watcher then compares the text it reads against the text already applied and skips when identical, which makes the daemon's own writes and an editor's no-op save free in one rule, with no bookkeeping about who wrote last.

**Validation is asymmetric, deliberately:**

- **Unknown keys are warnings.** The key is dropped, reported in `ConfigState.warnings`, and the rest of the file still applies. A config written for a newer teiryod must not stop an older one from polling.
- **Wrong-shaped values reject the whole file.** A negative interval, a non-boolean `enabled`, a TOML syntax error, or an interval below `MIN_POLL_INTERVAL_SECS` (10 s — below that a typo hammers a provider's usage endpoint hard enough to get the account rate limited, costing the user the very data teiryo exists to collect) means nothing from the file is applied. The previously applied config keeps running and `ConfigState.error` says why. Partial application would leave the user unable to reason about which half took effect.

**A bad config is never fatal.** Config is loaded inside `run()`, after the socket bind — a daemon that refuses to start over a typo leaves the user with no client to see the typo in. Startup on a rejected file uses defaults and reports the error.

## Logging & errors

- `tracing` → log file (the daemon has no useful stdout once detached).
- `thiserror` for errors crossing the IPC boundary (`Response::Err(ErrorKind, String)`); `anyhow` for internal daemon glue.
- Credentials use `secrecy::SecretString`: no `Debug`/`Display` leakage, zeroized on drop. Never derive or hand-write `Debug`/`Display` on anything holding a credential.
