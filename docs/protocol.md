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
struct Hello { magic: [u8; 4] /* b"TEIR" */, protocol_version: u16 /* little-endian; currently 4 */ }
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
    History {                                                            // bounded, see below
        account: AccountId,
        window: Option<WindowId>,
        since: DateTime<Utc>,
        until: Option<DateTime<Utc>>,   // None = now
        max_points: Option<u32>,        // per window; daemon downsamples
    },
    RecentPolls { limit: u32 },
    Providers,
    Shutdown,
}

enum Response {
    Status(Vec<AccountStatus>),
    PollAccepted { poll_id: PollId },
    Update(PollEvent),       // AwaitUpdate resolved with new data
    NoUpdate,                // AwaitUpdate timed out, nothing new
    History(HistoryPage),      // snapshots + the rollovers over the same interval
    RecentPolls(Vec<PollEvent>),
    Providers(Vec<ProviderHealth>),
    Ack,
    Err(ErrorKind, String),
}

struct HistoryPage {
    snapshots: Vec<QuotaSnapshot>,      // the requested interval, oldest first, downsampled
    earliest: Option<DateTime<Utc>>,    // start of the *stored* series, whatever was asked for
    rollovers: Vec<WindowRollover>,     // boundaries over the same interval, never downsampled
}

struct WindowView { window: QuotaWindow, hint: RenderHint }

struct AccountStatus {
    account: Account,
    windows: Vec<WindowView>,
    last_poll: Option<PollEvent>,       // any outcome, may be a failure
    last_success: Option<DateTime<Utc>>, // when the poll backing `windows` completed
    poll_interval_secs: u32,             // cadence in force now; jitters ±10%, stretches under backoff
}
struct AccountHealth {
    account: AccountId,
    consecutive_failures: u32,
    last_error: Option<String>,
    last_poll_ts: Option<DateTime<Utc>>,
    poll_interval_secs: u32,
}
struct ProviderHealth {
    provider: ProviderId,
    accounts: Vec<AccountHealth>,        // per-account, not just ids
    consecutive_failures: u32,           // worst across the provider's accounts
    last_error: Option<String>,
}
```

**Windows carry their render hint.** `WindowView` pairs each `QuotaWindow` with the `RenderHint` its adapter produced, so warn/critical thresholds and the provider caveat (`"blocks entirely at cap"` vs. `"auto-downgrades, doesn't block"`) reach the client instead of being hardcoded there. Pairing them in one struct rather than parallel `Vec`s makes it impossible for the two to drift apart. See [providers.md](providers.md).

**`last_poll` vs. `last_success`.** `windows` is served from the latest *successful* poll while `last_poll` is the latest poll of any outcome. After a failure the two diverge, and only `last_success` says how stale the displayed windows are — a client that reports staleness from `last_poll` would claim fresh data it does not have.

**Bounded `History`.** Nothing prunes `quota_snapshot`, so an unbounded `since` could exceed the 1 MiB frame cap. `until` bounds the far end; `max_points` downsamples each window's series independently. The daemon applies `MAX_HISTORY_POINTS` (2 000 per window) even when `max_points` is `None` — see [domain.md](domain.md).

**`HistoryPage.rollovers` rides with the series rather than behind its own request.** The chart's boundary rules and the points they annotate describe one interval; two requests could straddle a poll and disagree about it. Rollovers are also the one thing in a page that must *not* be downsampled — `Storage::history` keeps the peak per bucket, which at the `7d` range is a ~21-minute smear, and a boundary read back off that could be placed anywhere inside it. Recording them at poll time and shipping them whole is what makes the instants exact. See [domain.md](domain.md#window-rollovers).

**`HistoryPage.earliest` bounds a scroll, not a page.** A page that begins at its own `since` looks identical whether that is the start of the history or merely the start of the query, so a client scrolling a chart backwards through time cannot tell when to stop — it would have to probe blindly, one empty page at a time, and a gap in the history would look like the end of it. `earliest` is a separate `MIN(ts)` over the stored series for that account and window, unaffected by `since`, `until`, or `max_points`. With it a client clips the scroll to the data: the oldest point lands on the left edge and no further, and a history narrower than the visible range pins the view to the right edge, where it keeps following the clock.

`AwaitUpdate` semantics: if the daemon's latest published `PollId` is already newer than `since`, respond `Update` immediately; otherwise wait for the next publish or reply `NoUpdate` at `timeout_ms`.

## TUI ↔ protocol mapping

| Interaction | Trigger | Protocol |
| --- | --- | --- |
| Live dashboard, all accounts | on connect, then pushed | `Status` once, then loop on `AwaitUpdate { since }` |
| Manual poll (selected account or all) | keypress | `PollNow` → `PollAccepted`; result arrives via next `AwaitUpdate` resolution |
| History/sparkline for one window | keypress on a window | `History { account, window, since, until, max_points }` → `HistoryPage` |
| Recent activity / poll log | keypress | `RecentPolls { limit }` |
| Provider/account health | on connect + on update | `Providers` |
| Quit | keypress | close connection; daemon persists |
| Stop the daemon | explicit only | `Shutdown` — never bound to a casual key; gated behind a confirm prompt |

The TUI render loop is: connect → `Status` → render → loop `AwaitUpdate` → re-render on each `Update`/`NoUpdate`. **No polling timer on the TUI side**; the daemon's watch channel is the only clock.
