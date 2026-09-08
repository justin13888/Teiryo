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
    prev_observed_at INTEGER,        -- the poll this was compared against; NULL on pre-existing rows
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
    prev_observed_at: Option<DateTime<Utc>>,              // when the compared-against poll completed
}

/// Where a window is known to have begun, and how precisely.
struct ObservedStart {
    not_before: DateTime<Utc>,   // latest instant the old window was provably running
    not_after: DateTime<Utc>,    // earliest instant the new one provably was
}
```

A reset is never seen happening, only inferred from two polls that straddle it, so `[prev_observed_at, observed_at]` is the honest answer and `ObservedStart` is that bracket. `estimate()` is its midpoint — `not_before` under-reports the burn rate after an outage, and `not_after` can sit at `now` and send a pace to infinity — and `uncertainty()` is its width, one poll interval in normal running and as wide as a daemon outage otherwise.

`teiryo_core::rollover::detect` compares each successful poll against the **previous successful one** — not the previous poll, or a run of failures would read as every window vanishing. The daemon calls it in `record_event` and writes the result inside `record_poll`'s transaction, so a boundary can never outlive the reading that justifies it. `hydrate_account` restores the comparison baseline, so detection survives a restart.

Rules, per window present in both polls:

- `reset_at` **moving** is the primary signal. `used` falling is the fallback, and a weaker one: a provider correction that lowers `used` mid-window is not a new window, and splitting a series on it would draw a break that never happened.
- A jump *further* than one span is **not** a surprise. Rolling windows are anchored to first use, so after an idle stretch the next window legitimately starts later than the last one ended.
- `Early` and `Retracted` are what the provider did not advertise; both are logged at `info` by the daemon.
- `Unannounced` is inferred from usage **collapsing**: `rollover::is_collapse`, a fall of at least `MIN_RESET_DROP` (0.05) that leaves at most `RESET_COLLAPSE_RATIO` (half) of what was there. It needs a percent unit or a published limit to have a scale at all.

  The ratio is there because "did a lot of quota vanish" is the wrong question on its own. A weekly window resetting from 20% to 0 handed back a fifth of the week and is indistinguishable, on an absolute threshold alone, from a rounding fix — so it went unrecorded, and every number derived from the window's start stayed anchored to a window that had already ended. Conversely 90% → 50% is a big drop that still leaves most of the window standing, and is a correction.

  The floor is there because the ratio alone is too eager at the bottom of the scale, where halving a small number is easy: 5% → 2% passes it, and that is an ordinary revision. The asymmetry is deliberate — a **false** reset is much more costly than a missed one. It re-anchors the window, and a window believed to have opened a minute ago divides real usage by a nearly-zero elapsed fraction, so the row would shout an enormous pace at a quota that is 2% used. A missed one leaves the pace reading as it did before any of this existed.

  `Unannounced` is recorded and marked on the chart but is **not** a window boundary — see `RolloverKind::is_boundary`. It *does* anchor the pace: whether to break a drawn series on a `used`-only signal and where the window a rate is measured against began are different questions, and the evidence is good enough for the second. See [dashboard.md](dashboard.md#the-effective-window).
- `RESET_TOLERANCE` (120 s) absorbs clock skew between our poll timestamp and the provider's published reset instant.

The daemon keeps the latest **`Unannounced`** rollover per window as that window's `ObservedStart` and publishes it on `Status`. Every other kind moved `reset_at`, which makes `reset_at - span` the provider's own statement of where the new window began — exact, and better than anything inferred here. Anchoring on those would actively make things worse: a rollover seen across a weekend outage carries a bracket days wide, and its midpoint would override a start the provider had given precisely. So an announced rollover instead **retires** any anchor the window was carrying.

`hydrate_account` reseeds the cache from the last 21 days of stored rollovers, longer than any window an adapter publishes, replaying them oldest-first so an announced rollover retires an earlier unannounced one exactly as it did when live. Each stored row is re-judged against `is_collapse` before it is trusted, scaled through the live window's unit and limit: rows outlive the rule that classified them, and one written under an older rule — a bare drop with no ratio guard — would otherwise install an anchor for an instant nothing restarted at. A row that fails the re-check is skipped rather than retired, because the current detector would not have written it at all.

An anchor is also dropped once it is older than the window it anchors: a rolling window of span `S` that began at `T` has ended by `T + S`. Without that, an anchor is retired only by a later announced rollover — a signal the provider this rule exists for never sends. A window missing from a payload is *not* judged, since a payload says nothing about a window it does not carry.

**Known limit:** for a provider that never moves `reset_at`, a restart missed entirely — the daemon down across it, with usage higher on the far side than the near one, so neither signal fires — leaves the previous anchor in place for the rest of the window. The failure is the same under-reporting this whole rule exists to fix, bounded by the window's own length — a bound the anchor's expiry now enforces rather than merely asserts; correcting the miss itself would need evidence the series does not contain.

`prev_observed_at` was added after the table shipped. There is no migration framework — the schema is `CREATE TABLE IF NOT EXISTS` and the database is a local cache, not a system of record — so `Storage::init` runs an idempotent `ALTER TABLE ... ADD COLUMN` and tolerates the "duplicate column name" error. Rows written before it read as `NULL`, which `ObservedStart::from_rollover` treats as a zero-width bracket at `observed_at`: exactly the behaviour those rows had when they were written.

Rollovers are **exempt from the downsampling** below. They are sparse by construction, and bucketing them would move the very instants they exist to record.

## History retention and downsampling

**There is no retention policy** — no pruning, vacuum, or rollup. At the default 180 s cadence each window accumulates ~480 snapshots a day. That is deliberate (history is the point), but it means a `History` query over a long `since` can return an arbitrarily large row set.

`Storage::history` therefore takes `until` and `max_points` and enforces `MAX_HISTORY_POINTS = 2_000` **per window** regardless of what the caller asks for, so a response can never approach the 1 MiB frame cap.

Downsampling rules, applied after the query:

- Each `window_id`'s series is reduced **independently**, so a multi-window query keeps every series intact rather than sharing one budget.
- `since..=until` is cut into `max_points` equal buckets; the row with the **highest `used`** in each bucket survives. Peaks, not averages: a quota chart exists to show how close to the cap you came, and averaging would smooth away exactly the spike worth seeing.
- The final bucket yields its **newest** row instead of its peak, so a series always ends on the true current reading.
- A series already within budget passes through untouched.
