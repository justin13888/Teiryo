# Roadmap

## MVP (v1)

- `teiryod` + `teiryo` + the Claude adapter — single provider, but the account/window model is multi-provider and multi-account from the first line of code.
- SQLite storage and the **full** `Request`/`Response` set — it is the same complexity whether one or five accounts use it.
- Single auto-discovered account is enough to prove the model.

## Deferred (post-MVP)

- Second provider adapter (ChatGPT).
- Config-file `[[account]]` entries for providers that cannot be auto-discovered.
- `RecentPolls` UI polish.
- `ResetKind` fixed-calendar variant — add only when a provider actually needs it.
- Exact TLS-stack fingerprint (JA3) match — accepted gap in v1.
- Protocol backward-compatibility negotiation — the v1 handshake only fails loudly on mismatch.

## Open research items

Require direct inspection of the installed CLI/client, not an architecture call:

- Claude Code's actual credential storage path/format (Keychain vs. `~/.claude/...`), and whether it supports multiple concurrent local logins or needs separate config dirs per account.
- The exact low-cost endpoint each provider's real client hits for usage/rate-limit data, and its header/UA fingerprint.
- ChatGPT's credential model (session cookie vs. token) and whether it is locally readable at all or needs an interactive `teiryo auth chatgpt` login flow.
