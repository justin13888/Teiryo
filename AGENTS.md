# Teiryo

Teiryō (定量) probes and records LLM subscription quotas: `teiryod` (headless daemon) polls providers, stores history in SQLite, and serves clients over a Unix domain socket; `teiryo` (ratatui TUI) is a thin client of that socket. Normative architecture lives in `docs/` — read it before changing protocol, storage, or scheduler code.

## Layout

- `crates/teiryo-core` — domain types, wire protocol, adapter traits, storage. Light, stable dependency; the TUI depends only on this.
- `crates/teiryo-providers` — provider adapters (Claude, …) implementing the core traits; own their credential-store deps.
- `crates/teiryod` — daemon binary: scheduler, IPC server, wiring.
- `crates/teiryo` — TUI binary: ratatui + crossterm; no HTTP, no DB, no scheduling.

## Quality

Validate changes:

```bash
mise run check     # format check + clippy (-D warnings) + tests
```

Rules:

- Clippy warnings are errors; keep the workspace warning-free.
- Never derive/implement `Debug` or `Display` for anything holding credentials — secrets use `secrecy::SecretString`.
- Wire-protocol changes require bumping `PROTOCOL_VERSION` in `teiryo-core` and updating `docs/`.
- The TUI must never grow direct HTTP, DB, or scheduling logic; it speaks only the socket protocol.

## Commits

Commits MUST follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `chore:`, …) — enforced by `convco` via hk hooks (commit-msg, pre-push) and CI on pull requests. Merge commits are exempt.

`prd.md` is intentionally untracked — never commit it.
