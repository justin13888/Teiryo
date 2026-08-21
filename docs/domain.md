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
CREATE INDEX IF NOT EXISTS idx_poll_lookup ON poll_event(provider, account_id, ts);
```

Every snapshot batch is FK'd to its `poll_event`, so trigger reason, timestamp, and latency are never separated from the data they produced.
