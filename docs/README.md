# Teiryo Documentation

Normative reference for Teiryo's architecture. These documents pin decisions and invariants; code that contradicts them is wrong until the documents are amended.

Teiryo is two binaries: `teiryod`, a headless Rust daemon that polls LLM subscription usage across multiple providers and multiple accounts per provider, persists it, and serves it over a Unix domain socket; and `teiryo`, a ratatui TUI that is a thin client of that socket — no HTTP, no DB, no scheduling.

**Global constraints:** Rust only. Multi-account and multi-quota-window from day one. The wire protocol must be low-overhead, version-safe, and survive a daemon/TUI version mismatch without corrupting data. Minimal complexity wins every remaining tie.

| Document | Contents |
| --- | --- |
| [architecture.md](architecture.md) | Process model, crate layout, scheduler, runtime, HTTP, config, logging |
| [protocol.md](protocol.md) | Handshake, framing, serialization, wire protocol, versioning, TUI interaction mapping |
| [domain.md](domain.md) | Domain types and SQLite storage schema |
| [providers.md](providers.md) | Adapter trait split, credential handling, provider quirks |
| [roadmap.md](roadmap.md) | MVP cut, deferred work, open research items |
