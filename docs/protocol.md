# Wire Protocol

## Transport & framing

- `tokio::net::UnixListener` / `UnixStream` on the socket path given in [architecture.md](architecture.md); socket file mode 0600 in `$XDG_RUNTIME_DIR` (itself 0700). Cross-user access is therefore not a threat model; the handshake below guards against *stale/mismatched builds*, not attackers.
- Framing: `tokio_util::codec::LengthDelimitedCodec`, u32 **little-endian** length prefix, `max_frame_length` = 1 MiB. Oversized/garbage frames from a misbehaving peer are rejected instead of buffered unbounded. Daemon and TUI must construct the codec identically (shared constructor in `teiryo-core`).

## Serialization

`bincode` (serde mode, standard config) over `Request`/`Response` enums shared via `teiryo-core`.

Rejected alternatives, each in one line: JSON — text overhead with no local upside; Protobuf — schema/codegen ceremony for two binaries built from one workspace; rkyv — zero-copy is wasted on ~1 Hz small messages.

**The risk bincode does not handle**: it is positional, not self-describing. If daemon and TUI disagree on an enum's variant order, one side can silently decode bytes as the *wrong variant* — a `Shutdown` misread as `Status` is the failure mode the handshake exists to design out.

## Handshake

A hand-decoded, **never-changing** preamble runs before any bincode bytes:

```rust
// First 6 bytes on every connection, raw — not bincode, so it can never itself go stale
struct Hello { magic: [u8; 4] /* b"TEIR" */, protocol_version: u16 /* little-endian; currently 1 */ }
```

- Client sends the 6-byte Hello. Daemon replies with **one raw byte**: `0x00` accepted, `0x01` version mismatch — then closes the connection on mismatch without ever attempting to decode a `Request`.
- On mismatch the TUI reports "daemon is vX, client is vY — restart the daemon". **No negotiation, no backward compat** in v1: daemon and TUI ship together; the handshake fails loudly on the unclean case (stale daemon left running across an upgrade), it does not support long-term protocol drift.
- Any wire-protocol change (variant added/reordered/removed, field change) **must** bump `PROTOCOL_VERSION`.

## Requests & responses

```rust
enum Request {
    Status { provider: Option<ProviderId>, account: Option<AccountId> }, // None = all
    PollNow { provider: ProviderId, account: Option<AccountId> },        // None = all accounts on that provider
    AwaitUpdate { since: PollId, timeout_ms: u32 },                      // long-poll
    History { account: AccountId, window: Option<WindowId>, since: DateTime<Utc> },
    RecentPolls { limit: u32 },
    Providers,
    Shutdown,
}

enum Response {
    Status(Vec<AccountStatus>),
    PollAccepted { poll_id: PollId },
    Update(PollEvent),       // AwaitUpdate resolved with new data
    NoUpdate,                // AwaitUpdate timed out, nothing new
    History(Vec<QuotaSnapshot>),
    RecentPolls(Vec<PollEvent>),
    Providers(Vec<ProviderHealth>),
    Ack,
    Err(ErrorKind, String),
}

struct AccountStatus { account: Account, windows: Vec<QuotaWindow>, last_poll: Option<PollEvent> }
struct ProviderHealth { provider: ProviderId, accounts: Vec<AccountId>, consecutive_failures: u32, last_error: Option<String> }
```

`AwaitUpdate` semantics: if the daemon's latest published `PollId` is already newer than `since`, respond `Update` immediately; otherwise wait for the next publish or reply `NoUpdate` at `timeout_ms`.

## TUI ↔ protocol mapping

| Interaction | Trigger | Protocol |
| --- | --- | --- |
| Live dashboard, all accounts | on connect, then pushed | `Status` once, then loop on `AwaitUpdate { since }` |
| Manual poll (selected account or all) | keypress | `PollNow` → `PollAccepted`; result arrives via next `AwaitUpdate` resolution |
| History/sparkline for one window | keypress on a window | `History { account, window, since }` |
| Recent activity / poll log | keypress | `RecentPolls { limit }` |
| Provider/account health | on connect + on update | `Providers` |
| Quit | keypress | close connection; daemon persists |
| Stop the daemon | explicit only | `Shutdown` — never bound to a casual key; gated behind a confirm prompt |

The TUI render loop is: connect → `Status` → render → loop `AwaitUpdate` → re-render on each `Update`/`NoUpdate`. **No polling timer on the TUI side**; the daemon's watch channel is the only clock.
