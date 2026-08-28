# Domain Model & Storage

## Domain types

```rust
type ProviderId = String;              // "claude", "openai" — small open set, no enum needed
struct AccountId(String);              // provider-derived, stable across restarts
struct WindowId(String);               // provider-defined, e.g. "session_5h_opus"
struct PollId(Ulid);                   // core-generated, sortable

struct Account { id: AccountId, provider: ProviderId, label: String } // "personal", "work"

struct QuotaWindow {
    id: WindowId,
    label: String,                     // "Opus — 5 hour", adapter-supplied
    scope: WindowScope,
    reset_kind: ResetKind,
    unit: QuotaUnit,
    used: f64,
    limit: Option<f64>,                // None where the provider only exposes % remaining
    reset_at: Option<DateTime<Utc>>,
}
enum WindowScope { AccountWide, Model(String) }
enum ResetKind { Rolling(Duration) }   // anchored-window; add fixed-calendar only when a provider needs it
enum QuotaUnit { Percent, Messages, Tokens, Hours }

enum PollTrigger { Scheduled, Manual { client: ClientKind }, Startup }
enum ClientKind { Tui, Other(String) } // future non-TUI callers, e.g. a CLI or HTTP shim

enum PollOutcome {
    Success { windows: Vec<QuotaWindow> },
    AuthError(String),
    NetworkError(String),
    SchemaDrift(String),
    RateLimited { retry_after: Option<Duration> },
}

struct PollEvent {
    id: PollId, ts: DateTime<Utc>,
    provider: ProviderId, account: AccountId,
    trigger: PollTrigger, outcome: PollOutcome, latency_ms: u32,
}
```

Invariants:

- A `QuotaWindow` is per (account, window); a single poll usually returns several — this is the multi-quota requirement. Multi-*account* is just more `Account` rows per provider; nothing else in the model changes.
- `PollId` is a ULID: sortable by creation time, generated in core with no coordination.
- Windows carry their own `unit` and `limit: Option<f64>`; never assume a global percentage model (see [providers.md](providers.md)).

## Storage

`rusqlite` with the `bundled` feature, WAL mode. No ORM, no migration framework — the schema is small; `CREATE TABLE IF NOT EXISTS` at startup.

```sql
CREATE TABLE IF NOT EXISTS account (
    id TEXT PRIMARY KEY, provider TEXT NOT NULL, label TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS poll_event (
    id TEXT PRIMARY KEY,             -- ULID: sortable, no coordination needed
    ts INTEGER NOT NULL, provider TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES account(id),
    trigger TEXT NOT NULL,           -- json: {"kind":"manual","client":"tui"}
    outcome TEXT NOT NULL, latency_ms INTEGER, error TEXT
);
CREATE TABLE IF NOT EXISTS quota_snapshot (
    poll_id TEXT NOT NULL REFERENCES poll_event(id),
    window_id TEXT NOT NULL, label TEXT NOT NULL, unit TEXT NOT NULL,
    used REAL NOT NULL, limit_val REAL, reset_at INTEGER,
    PRIMARY KEY (poll_id, window_id)
);
CREATE TABLE IF NOT EXISTS window_rollover (
    poll_id TEXT NOT NULL REFERENCES poll_event(id),
    window_id TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES account(id),
    observed_at INTEGER NOT NULL,
    kind TEXT NOT NULL,              -- scheduled | early | retracted | unannounced
    prev_reset_at INTEGER, new_reset_at INTEGER,
    prev_used REAL NOT NULL, new_used REAL NOT NULL,
    PRIMARY KEY (poll_id, window_id)
);
CREATE INDEX IF NOT EXISTS idx_poll_lookup ON poll_event(provider, account_id, ts);
CREATE INDEX IF NOT EXISTS idx_rollover_lookup ON window_rollover(account_id, window_id, observed_at);
```

Every snapshot batch is FK'd to its `poll_event`, so trigger reason, timestamp, and latency are never separated from the data they produced.

## Window rollovers

```rust
enum RolloverKind {
    Scheduled,    // reset_at advanced at or after the old reset was due
    Early,        // reset_at advanced while the old reset was still in the future
    Retracted,    // reset_at moved backwards
    Unannounced,  // usage collapsed with reset_at unchanged
}

struct WindowRollover {
    account: AccountId, window: WindowId, poll: PollId,   // (poll, window) is the key
    observed_at: DateTime<Utc>, kind: RolloverKind,
    prev_reset_at: Option<DateTime<Utc>>, new_reset_at: Option<DateTime<Utc>>,
    prev_used: f64, new_used: f64,
}
```

`teiryo_core::rollover::detect` compares each successful poll against the **previous successful one** — not the previous poll, or a run of failures would read as every window vanishing. The daemon calls it in `record_event` and writes the result inside `record_poll`'s transaction, so a boundary can never outlive the reading that justifies it. `hydrate_account` restores the comparison baseline, so detection survives a restart.

Rules, per window present in both polls:

- `reset_at` **moving** is the signal, never `used` falling on its own: a provider correction that lowers `used` mid-window is not a new window, and splitting a series on it would draw a break that never happened.
- A jump *further* than one span is **not** a surprise. Rolling windows are anchored to first use, so after an idle stretch the next window legitimately starts later than the last one ended.
- `Early` and `Retracted` are what the provider did not advertise; both are logged at `info` by the daemon.
- `Unannounced` is inferred from a utilization drop past `UNANNOUNCED_DROP` (0.25) and needs a percent unit or a published limit to have a scale at all. It is recorded and marked on the chart but is **not** treated as a window boundary — see `RolloverKind::is_boundary`.
- `RESET_TOLERANCE` (120 s) absorbs clock skew between our poll timestamp and the provider's published reset instant.

Rollovers are **exempt from the downsampling** below. They are sparse by construction, and bucketing them would move the very instants they exist to record.

## History retention and downsampling

**There is no retention policy** — no pruning, vacuum, or rollup. At the default 180 s cadence each window accumulates ~480 snapshots a day. That is deliberate (history is the point), but it means a `History` query over a long `since` can return an arbitrarily large row set.

`Storage::history` therefore takes `until` and `max_points` and enforces `MAX_HISTORY_POINTS = 2_000` **per window** regardless of what the caller asks for, so a response can never approach the 1 MiB frame cap.

Downsampling rules, applied after the query:

- Each `window_id`'s series is reduced **independently**, so a multi-window query keeps every series intact rather than sharing one budget.
- `since..=until` is cut into `max_points` equal buckets; the row with the **highest `used`** in each bucket survives. Peaks, not averages: a quota chart exists to show how close to the cap you came, and averaging would smooth away exactly the spike worth seeing.
- The final bucket yields its **newest** row instead of its peak, so a series always ends on the true current reading.
- A series already within budget passes through untouched.
