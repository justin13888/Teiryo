//! SQLite persistence for accounts, poll events, and quota snapshots.
//!
//! `rusqlite` with the bundled engine, WAL mode, no migration framework —
//! the schema is small and created idempotently at open. Timestamps are
//! stored as unix milliseconds. `trigger` and `outcome` columns hold the
//! serde_json encoding of [`PollTrigger`] / [`PollOutcome`] so events round-
//! trip losslessly; `quota_snapshot` rows are the normalized, queryable
//! history the TUI's sparklines read.

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::domain::{
    Account, AccountId, PollEvent, PollId, PollOutcome, PollTrigger, QuotaSnapshot, QuotaUnit,
    QuotaWindow, WindowId,
};
use crate::rollover::{RolloverKind, WindowRollover};

/// Hard ceiling on the points [`Storage::history`] returns *per window*,
/// applied even when the caller asks for no downsampling. Nothing prunes the
/// tables, so at the default 60 s cadence a series grows by ~1 440 points a
/// day; without this cap a far-back `since` could produce a response too large
/// for the 1 MiB frame limit.
pub const MAX_HISTORY_POINTS: u32 = 2_000;

/// Storage failure.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Underlying SQLite error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON (de)serialization of a stored column failed.
    #[error("stored json: {0}")]
    Json(#[from] serde_json::Error),
    /// A stored value was structurally invalid (e.g. unparseable ULID).
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

/// Handle to the Teiryo database. Synchronous by design — it is used from
/// the daemon's single thread.
pub struct Storage {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS account (
    id TEXT PRIMARY KEY, provider TEXT NOT NULL, label TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS poll_event (
    id TEXT PRIMARY KEY,
    ts INTEGER NOT NULL, provider TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES account(id),
    trigger TEXT NOT NULL,
    outcome TEXT NOT NULL, latency_ms INTEGER, error TEXT
);
CREATE TABLE IF NOT EXISTS quota_snapshot (
    poll_id TEXT NOT NULL REFERENCES poll_event(id),
    window_id TEXT NOT NULL, label TEXT NOT NULL, unit TEXT NOT NULL,
    used REAL NOT NULL, limit_val REAL, reset_at INTEGER,
    PRIMARY KEY (poll_id, window_id)
);
-- Observed window rollovers. Keyed like `quota_snapshot`, and written in the
-- same transaction as the poll that revealed them, so a boundary can never
-- outlive its evidence. Unlike snapshots these are sparse and are never
-- downsampled: a rollover's instant is the whole point of recording it.
CREATE TABLE IF NOT EXISTS window_rollover (
    poll_id TEXT NOT NULL REFERENCES poll_event(id),
    window_id TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES account(id),
    observed_at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    prev_reset_at INTEGER, new_reset_at INTEGER,
    prev_used REAL NOT NULL, new_used REAL NOT NULL,
    PRIMARY KEY (poll_id, window_id)
);
CREATE INDEX IF NOT EXISTS idx_poll_lookup ON poll_event(provider, account_id, ts);
-- Every history query filters on the account alone, which the index above
-- cannot serve: `provider` leads it.
CREATE INDEX IF NOT EXISTS idx_poll_account_ts ON poll_event(account_id, ts);
CREATE INDEX IF NOT EXISTS idx_rollover_lookup
    ON window_rollover(account_id, window_id, observed_at);
";

fn ts_to_millis(ts: DateTime<Utc>) -> i64 {
    ts.timestamp_millis()
}

fn millis_to_ts(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_default()
}

fn unit_to_str(unit: QuotaUnit) -> &'static str {
    match unit {
        QuotaUnit::Percent => "percent",
        QuotaUnit::Messages => "messages",
        QuotaUnit::Tokens => "tokens",
        QuotaUnit::Hours => "hours",
    }
}

fn unit_from_str(s: &str) -> Result<QuotaUnit, StorageError> {
    match s {
        "percent" => Ok(QuotaUnit::Percent),
        "messages" => Ok(QuotaUnit::Messages),
        "tokens" => Ok(QuotaUnit::Tokens),
        "hours" => Ok(QuotaUnit::Hours),
        other => Err(StorageError::Corrupt(format!("unknown unit {other:?}"))),
    }
}

impl Storage {
    /// Open (creating if needed) the database at `path`, enable WAL, and
    /// ensure the schema exists.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory database, for tests.
    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StorageError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Insert or update an account row.
    pub fn upsert_account(&self, account: &Account) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO account (id, provider, label) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET provider = ?2, label = ?3",
            params![account.id.0, account.provider, account.label],
        )?;
        Ok(())
    }

    /// All known accounts.
    pub fn accounts(&self) -> Result<Vec<Account>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, provider, label FROM account ORDER BY provider, id")?;
        let rows = stmt.query_map([], |row| {
            Ok(Account {
                id: AccountId(row.get(0)?),
                provider: row.get(1)?,
                label: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Persist one poll event, its snapshot batch, and any rollovers the poll
    /// revealed, atomically. All three are FK'd to the event so trigger,
    /// timestamp, and latency are never separated from the data they produced
    /// — and so a recorded boundary always has the reading that justifies it.
    pub fn record_poll(
        &mut self,
        event: &PollEvent,
        windows: &[QuotaWindow],
        rollovers: &[WindowRollover],
    ) -> Result<(), StorageError> {
        let trigger_json = serde_json::to_string(&event.trigger)?;
        let outcome_json = serde_json::to_string(&event.outcome)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO poll_event (id, ts, provider, account_id, trigger, outcome, latency_ms, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                event.id.to_string(),
                ts_to_millis(event.ts),
                event.provider,
                event.account.0,
                trigger_json,
                outcome_json,
                event.latency_ms,
                event.outcome.error_message(),
            ],
        )?;
        for window in windows {
            tx.execute(
                "INSERT INTO quota_snapshot (poll_id, window_id, label, unit, used, limit_val, reset_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    event.id.to_string(),
                    window.id.0,
                    window.label,
                    unit_to_str(window.unit),
                    window.used,
                    window.limit,
                    window.reset_at.map(ts_to_millis),
                ],
            )?;
        }
        for rollover in rollovers {
            tx.execute(
                "INSERT INTO window_rollover
                     (poll_id, window_id, account_id, observed_at, kind,
                      prev_reset_at, new_reset_at, prev_used, new_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    rollover.poll.to_string(),
                    rollover.window.0,
                    rollover.account.0,
                    ts_to_millis(rollover.observed_at),
                    rollover.kind.as_str(),
                    rollover.prev_reset_at.map(ts_to_millis),
                    rollover.new_reset_at.map(ts_to_millis),
                    rollover.prev_used,
                    rollover.new_used,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Rollovers observed for one account (optionally one window) within
    /// `since..=until`, oldest first.
    ///
    /// Deliberately not downsampled, unlike [`Storage::history`]: rollovers are
    /// sparse by construction, and their exact instants are the reason they are
    /// stored at all. Bucketing them would move the very boundaries a caller
    /// asked for.
    pub fn rollovers(
        &self,
        account: &AccountId,
        window: Option<&WindowId>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<WindowRollover>, StorageError> {
        let mut sql = String::from(
            "SELECT poll_id, window_id, observed_at, kind,
                    prev_reset_at, new_reset_at, prev_used, new_used
             FROM window_rollover
             WHERE account_id = ?1 AND observed_at >= ?2 AND observed_at <= ?3",
        );
        if window.is_some() {
            sql.push_str(" AND window_id = ?4");
        }
        sql.push_str(" ORDER BY observed_at ASC");
        let mut stmt = self.conn.prepare(&sql)?;
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<RolloverRow> {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        };
        let (from, to) = (ts_to_millis(since), ts_to_millis(until));
        let raw: Vec<RolloverRow> = if let Some(w) = window {
            stmt.query_map(params![account.0, from, to, w.0], map_row)?
                .collect::<Result<_, _>>()?
        } else {
            stmt.query_map(params![account.0, from, to], map_row)?
                .collect::<Result<_, _>>()?
        };
        raw.into_iter()
            .map(
                |(
                    poll,
                    window_id,
                    observed_at,
                    kind,
                    prev_reset,
                    new_reset,
                    prev_used,
                    new_used,
                )| {
                    Ok(WindowRollover {
                        account: account.clone(),
                        window: WindowId(window_id),
                        poll: parse_poll_id(&poll)?,
                        observed_at: millis_to_ts(observed_at),
                        kind: RolloverKind::from_column(&kind).ok_or_else(|| {
                            StorageError::Corrupt(format!("unknown rollover kind {kind:?}"))
                        })?,
                        prev_reset_at: prev_reset.map(millis_to_ts),
                        new_reset_at: new_reset.map(millis_to_ts),
                        prev_used,
                        new_used,
                    })
                },
            )
            .collect()
    }

    /// Snapshots for one account (optionally one window) within
    /// `since..=until`, oldest first. `until` of `None` means "now".
    ///
    /// Each window's series is independently downsampled to at most
    /// `max_points` (capped by [`MAX_HISTORY_POINTS`]) — see
    /// [`downsample`] for why peaks rather than averages survive.
    pub fn history(
        &self,
        account: &AccountId,
        window: Option<&WindowId>,
        since: DateTime<Utc>,
        until: Option<DateTime<Utc>>,
        max_points: Option<u32>,
    ) -> Result<Vec<QuotaSnapshot>, StorageError> {
        let until = until.unwrap_or_else(Utc::now);
        let mut sql = String::from(
            "SELECT s.poll_id, e.ts, s.window_id, s.label, s.unit, s.used, s.limit_val, s.reset_at
             FROM quota_snapshot s JOIN poll_event e ON e.id = s.poll_id
             WHERE e.account_id = ?1 AND e.ts >= ?2 AND e.ts <= ?3",
        );
        if window.is_some() {
            sql.push_str(" AND s.window_id = ?4");
        }
        sql.push_str(" ORDER BY e.ts ASC");
        let mut stmt = self.conn.prepare(&sql)?;
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<SnapshotRow> {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        };
        let (from, to) = (ts_to_millis(since), ts_to_millis(until));
        let raw: Vec<_> = if let Some(w) = window {
            stmt.query_map(params![account.0, from, to, w.0], map_row)?
                .collect::<Result<_, _>>()?
        } else {
            stmt.query_map(params![account.0, from, to], map_row)?
                .collect::<Result<_, _>>()?
        };
        let snapshots: Vec<QuotaSnapshot> = raw
            .into_iter()
            .map(
                |(poll_id, ts, window_id, label, unit, used, limit, reset_at)| {
                    Ok(QuotaSnapshot {
                        poll_id: parse_poll_id(&poll_id)?,
                        ts: millis_to_ts(ts),
                        window: WindowId(window_id),
                        label,
                        unit: unit_from_str(&unit)?,
                        used,
                        limit,
                        reset_at: reset_at.map(millis_to_ts),
                    })
                },
            )
            .collect::<Result<_, StorageError>>()?;
        let budget = max_points
            .unwrap_or(MAX_HISTORY_POINTS)
            .clamp(1, MAX_HISTORY_POINTS);
        Ok(downsample(snapshots, since, until, budget))
    }

    /// When the oldest stored snapshot for `account` was taken, optionally
    /// restricted to one window.
    ///
    /// Answers "how far back does this series reach?" without reading the
    /// series: a client panning through time needs the far end, and finding it
    /// by widening a [`Storage::history`] query would mean fetching everything
    /// just to learn where to stop.
    pub fn earliest_snapshot(
        &self,
        account: &AccountId,
        window: Option<&WindowId>,
    ) -> Result<Option<DateTime<Utc>>, StorageError> {
        let mut sql = String::from(
            "SELECT MIN(e.ts) FROM quota_snapshot s JOIN poll_event e ON e.id = s.poll_id
             WHERE e.account_id = ?1",
        );
        if window.is_some() {
            sql.push_str(" AND s.window_id = ?2");
        }
        let mut stmt = self.conn.prepare(&sql)?;
        // `MIN` over no rows is one NULL row, not zero rows, so this always
        // yields exactly one value to unwrap.
        let millis: Option<i64> = match window {
            Some(w) => stmt.query_row(params![account.0, w.0], |row| row.get(0))?,
            None => stmt.query_row(params![account.0], |row| row.get(0))?,
        };
        Ok(millis.map(millis_to_ts))
    }

    /// The most recent poll events across all accounts, newest first.
    pub fn recent_polls(&self, limit: u32) -> Result<Vec<PollEvent>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, provider, account_id, trigger, outcome, latency_ms
             FROM poll_event ORDER BY id DESC LIMIT ?1",
        )?;
        let raw: Vec<(String, i64, String, String, String, String, u32)> = stmt
            .query_map(params![limit], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        raw.into_iter().map(row_to_event).collect()
    }

    /// The most recent poll event for one account, if any.
    pub fn latest_poll_for(&self, account: &AccountId) -> Result<Option<PollEvent>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, provider, account_id, trigger, outcome, latency_ms
             FROM poll_event WHERE account_id = ?1 ORDER BY id DESC LIMIT 1",
        )?;
        let raw = stmt
            .query_row(params![account.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, u32>(6)?,
                ))
            })
            .optional()?;
        raw.map(row_to_event).transpose()
    }
}

fn parse_poll_id(s: &str) -> Result<PollId, StorageError> {
    s.parse::<ulid::Ulid>()
        .map(PollId)
        .map_err(|e| StorageError::Corrupt(format!("bad poll id {s:?}: {e}")))
}

/// Reduce each window's series in `snapshots` to at most `budget` points.
///
/// Windows are reduced independently, so a multi-window query keeps every
/// series intact rather than interleaving them into one budget. Input and
/// output are both oldest-first.
fn downsample(
    snapshots: Vec<QuotaSnapshot>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    budget: u32,
) -> Vec<QuotaSnapshot> {
    let span = (until - since).num_milliseconds();
    if span <= 0 {
        return snapshots;
    }
    // Group by window, first-seen order, so output is deterministic without
    // needing a hash map — providers expose a handful of windows at most.
    let mut series: Vec<(WindowId, Vec<QuotaSnapshot>)> = Vec::new();
    for snapshot in snapshots {
        match series.iter_mut().find(|(id, _)| *id == snapshot.window) {
            Some((_, rows)) => rows.push(snapshot),
            None => series.push((snapshot.window.clone(), vec![snapshot])),
        }
    }
    let mut out = Vec::new();
    for (_, rows) in series {
        out.extend(reduce_series(rows, since, span, budget));
    }
    out.sort_by_key(|s| s.ts);
    out
}

/// Reduce one window's oldest-first series to at most `budget` points.
///
/// `since..since + span` is cut into `budget` equal buckets and the row with
/// the **highest** `used` in each survives. Peaks rather than averages,
/// because a quota chart exists to show how close to the cap you came —
/// averaging would smooth away exactly the spike worth seeing. The final
/// bucket yields its newest row instead of its peak, so the series always
/// ends on the true current reading.
fn reduce_series(
    rows: Vec<QuotaSnapshot>,
    since: DateTime<Utc>,
    span: i64,
    budget: u32,
) -> Vec<QuotaSnapshot> {
    if rows.len() <= budget as usize {
        return rows;
    }
    let bucket_of = |snapshot: &QuotaSnapshot| -> i128 {
        let offset = (snapshot.ts - since).num_milliseconds().max(0);
        (i128::from(offset) * i128::from(budget) / i128::from(span)).min(i128::from(budget) - 1)
    };
    let mut picks = Vec::with_capacity(budget as usize);
    let mut current: Option<i128> = None;
    let mut peak = 0usize;
    for (i, row) in rows.iter().enumerate() {
        let bucket = bucket_of(row);
        if current != Some(bucket) {
            if current.is_some() {
                picks.push(rows[peak].clone());
            }
            current = Some(bucket);
            peak = i;
        } else if row.used > rows[peak].used {
            peak = i;
        }
    }
    if current.is_some() {
        picks.push(rows[rows.len() - 1].clone());
    }
    picks
}

type EventRow = (String, i64, String, String, String, String, u32);
type SnapshotRow = (
    String,
    i64,
    String,
    String,
    String,
    f64,
    Option<f64>,
    Option<i64>,
);
type RolloverRow = (
    String,
    String,
    i64,
    String,
    Option<i64>,
    Option<i64>,
    f64,
    f64,
);

fn row_to_event(row: EventRow) -> Result<PollEvent, StorageError> {
    let (id, ts, provider, account_id, trigger_json, outcome_json, latency_ms) = row;
    let trigger: PollTrigger = serde_json::from_str(&trigger_json)?;
    let outcome: PollOutcome = serde_json::from_str(&outcome_json)?;
    Ok(PollEvent {
        id: parse_poll_id(&id)?,
        ts: millis_to_ts(ts),
        provider,
        account: AccountId(account_id),
        trigger,
        outcome,
        latency_ms,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;

    use super::*;
    use crate::domain::{ClientKind, QuotaUnit, ResetKind, WindowScope};

    fn account() -> Account {
        Account {
            id: AccountId::from("claude:personal"),
            provider: "claude".into(),
            label: "personal".into(),
        }
    }

    fn window(id: &str, used: f64) -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from(id),
            label: format!("window {id}"),
            scope: WindowScope::AccountWide,
            reset_kind: ResetKind::Rolling(Duration::from_secs(5 * 3600)),
            unit: QuotaUnit::Percent,
            used,
            limit: Some(100.0),
            reset_at: Some(Utc::now()),
        }
    }

    fn event(outcome: PollOutcome) -> PollEvent {
        PollEvent {
            id: PollId::generate(),
            ts: Utc::now(),
            provider: "claude".into(),
            account: AccountId::from("claude:personal"),
            trigger: PollTrigger::Manual {
                client: ClientKind::Tui,
            },
            outcome,
            latency_ms: 42,
        }
    }

    #[test]
    fn rollovers_roundtrip_and_honor_their_interval() {
        let mut storage = Storage::open_in_memory().unwrap();
        storage.upsert_account(&account()).unwrap();
        // Whole seconds: the columns are unix millis, so a `Utc::now()` with
        // microsecond precision would not compare equal after a round trip.
        let now = Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap();
        let windows = vec![window("session_5h", 3.0)];
        let poll = event(PollOutcome::Success {
            windows: windows.clone(),
        });

        let rollover = |window_id: &str, kind, minutes_ago: i64| WindowRollover {
            account: account().id,
            window: WindowId::from(window_id),
            poll: poll.id,
            observed_at: now - chrono::Duration::minutes(minutes_ago),
            kind,
            prev_reset_at: Some(now - chrono::Duration::minutes(minutes_ago)),
            new_reset_at: Some(now + chrono::Duration::hours(5)),
            prev_used: 91.5,
            new_used: 3.0,
        };
        // Two windows on one poll — the composite key must keep both.
        let written = vec![
            rollover("session_5h", RolloverKind::Early, 30),
            rollover("weekly", RolloverKind::Scheduled, 90),
        ];
        storage.record_poll(&poll, &windows, &written).unwrap();

        let id = account().id;
        let all = storage
            .rollovers(&id, None, now - chrono::Duration::hours(2), now)
            .unwrap();
        assert_eq!(all.len(), 2);
        // Oldest first, and every field survives the trip.
        assert_eq!(all[0].window, WindowId::from("weekly"));
        assert_eq!(all[1], written[0]);

        // Filtered to one window.
        let one = storage
            .rollovers(
                &id,
                Some(&WindowId::from("session_5h")),
                now - chrono::Duration::hours(2),
                now,
            )
            .unwrap();
        assert_eq!(one, vec![written[0].clone()]);

        // The interval is inclusive of neither end beyond its bounds: a 1-hour
        // window excludes the 90-minute-old row.
        let recent = storage
            .rollovers(&id, None, now - chrono::Duration::hours(1), now)
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].kind, RolloverKind::Early);

        // Another account's history is never mixed in.
        assert!(storage
            .rollovers(
                &AccountId::from("nope"),
                None,
                now - chrono::Duration::hours(2),
                now
            )
            .unwrap()
            .is_empty());
    }

    /// A rollover is only meaningful next to the reading that justified it, so
    /// it must never outlive a poll insert that failed.
    #[test]
    fn a_failed_poll_insert_takes_its_rollovers_with_it() {
        let mut storage = Storage::open_in_memory().unwrap();
        storage.upsert_account(&account()).unwrap();
        let now = Utc::now();
        let windows = vec![window("session_5h", 3.0)];
        let poll = event(PollOutcome::Success {
            windows: windows.clone(),
        });
        let rollovers = vec![WindowRollover {
            account: account().id,
            window: WindowId::from("session_5h"),
            poll: poll.id,
            observed_at: now,
            kind: RolloverKind::Early,
            prev_reset_at: None,
            new_reset_at: None,
            prev_used: 90.0,
            new_used: 1.0,
        }];
        storage.record_poll(&poll, &windows, &rollovers).unwrap();
        // Re-recording the same poll violates the primary key and rolls back.
        assert!(storage.record_poll(&poll, &windows, &rollovers).is_err());

        let stored = storage
            .rollovers(&account().id, None, now - chrono::Duration::hours(1), now)
            .unwrap();
        assert_eq!(stored.len(), 1, "the retry must not have added a duplicate");
    }

    #[test]
    fn record_and_query_roundtrip() {
        let mut storage = Storage::open_in_memory().unwrap();
        storage.upsert_account(&account()).unwrap();
        assert_eq!(storage.accounts().unwrap(), vec![account()]);

        let windows = vec![window("session_5h", 40.0), window("weekly", 12.5)];
        let ok = event(PollOutcome::Success {
            windows: windows.clone(),
        });
        storage.record_poll(&ok, &windows, &[]).unwrap();

        // ULID ordering (and thus recent_polls order) is only guaranteed
        // across distinct milliseconds.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let failed = event(PollOutcome::AuthError("expired".into()));
        storage.record_poll(&failed, &[], &[]).unwrap();

        // History: both windows, then filtered to one.
        let since = Utc::now() - chrono::Duration::hours(1);
        let all = storage
            .history(&account().id, None, since, None, None)
            .unwrap();
        assert_eq!(all.len(), 2);
        let one = storage
            .history(
                &account().id,
                Some(&WindowId::from("weekly")),
                since,
                None,
                None,
            )
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].used, 12.5);
        assert_eq!(one[0].poll_id, ok.id);
        assert_eq!(one[0].unit, QuotaUnit::Percent);

        // Recent polls come back newest first, losslessly.
        let recent = storage.recent_polls(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, failed.id);
        assert_eq!(recent[0].outcome, failed.outcome);
        assert_eq!(recent[1].outcome, ok.outcome);

        // Latest per account.
        let latest = storage.latest_poll_for(&account().id).unwrap().unwrap();
        assert_eq!(latest.id, failed.id);
        assert!(storage
            .latest_poll_for(&AccountId::from("nope"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn upsert_account_updates_label() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_account(&account()).unwrap();
        let mut renamed = account();
        renamed.label = "work".into();
        storage.upsert_account(&renamed).unwrap();
        let accounts = storage.accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label, "work");
    }

    #[test]
    fn open_creates_file_and_reopens() {
        let dir = std::env::temp_dir().join(format!("teiryo-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("teiryo.db");
        {
            let mut storage = Storage::open(&path).unwrap();
            storage.upsert_account(&account()).unwrap();
            let e = event(PollOutcome::Success { windows: vec![] });
            storage.record_poll(&e, &[], &[]).unwrap();
        }
        let reopened = Storage::open(&path).unwrap();
        assert_eq!(reopened.accounts().unwrap().len(), 1);
        assert_eq!(reopened.recent_polls(5).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Build a synthetic series for `window` at one-minute spacing.
    fn snapshots(window: &str, start: DateTime<Utc>, used: &[f64]) -> Vec<QuotaSnapshot> {
        used.iter()
            .enumerate()
            .map(|(i, &used)| QuotaSnapshot {
                poll_id: PollId::generate(),
                ts: start + chrono::Duration::minutes(i as i64),
                window: WindowId::from(window),
                label: window.to_owned(),
                unit: QuotaUnit::Percent,
                used,
                limit: Some(100.0),
                reset_at: None,
            })
            .collect()
    }

    #[test]
    fn downsample_keeps_peaks_and_the_newest_point() {
        let start = Utc::now() - chrono::Duration::minutes(60);
        let until = start + chrono::Duration::minutes(59);
        let mut used: Vec<f64> = (0..60).map(|i| f64::from(i % 10)).collect();
        used[17] = 99.0; // a spike an averaging reducer would erase
        let series = snapshots("w", start, &used);
        let newest = series.last().unwrap().clone();

        let out = downsample(series, start, until, 6);

        assert!(out.len() <= 6, "budget exceeded: {}", out.len());
        assert!(out.iter().any(|s| s.used == 99.0), "peak was lost");
        let last = out.last().unwrap();
        assert_eq!(last.ts, newest.ts, "series must end on the current reading");
        assert_eq!(last.used, newest.used);
        assert!(
            out.windows(2).all(|w| w[0].ts <= w[1].ts),
            "not oldest-first"
        );
    }

    #[test]
    fn downsample_reduces_each_window_independently() {
        let start = Utc::now() - chrono::Duration::minutes(40);
        let until = start + chrono::Duration::minutes(39);
        let mut series = snapshots("session_5h", start, &vec![10.0; 40]);
        series.extend(snapshots("weekly", start, &vec![20.0; 40]));

        let out = downsample(series, start, until, 4);

        for id in ["session_5h", "weekly"] {
            let kept = out.iter().filter(|s| s.window.0 == id).count();
            assert!(kept > 0 && kept <= 4, "{id} kept {kept}");
        }
    }

    #[test]
    fn short_series_pass_through_untouched() {
        let start = Utc::now() - chrono::Duration::minutes(3);
        let series = snapshots("w", start, &[1.0, 2.0, 3.0]);
        let out = downsample(series.clone(), start, Utc::now(), 100);
        assert_eq!(out.len(), series.len());
    }

    #[test]
    fn history_honors_until_and_max_points() {
        let mut storage = Storage::open_in_memory().unwrap();
        storage.upsert_account(&account()).unwrap();
        for used in [10.0, 20.0, 30.0] {
            let windows = vec![window("session_5h", used)];
            let e = event(PollOutcome::Success {
                windows: windows.clone(),
            });
            storage.record_poll(&e, &windows, &[]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let since = Utc::now() - chrono::Duration::hours(1);

        // `until` in the past excludes everything recorded just now.
        let excluded = storage
            .history(
                &account().id,
                None,
                since,
                Some(Utc::now() - chrono::Duration::minutes(30)),
                None,
            )
            .unwrap();
        assert!(excluded.is_empty());

        // A max_points of 1 still returns the newest reading.
        let capped = storage
            .history(&account().id, None, since, None, Some(1))
            .unwrap();
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].used, 30.0);
    }

    /// The extent is a property of the stored series, not of any query:
    /// a client that panned a chart back would otherwise have to fetch
    /// everything just to learn where the data stops.
    #[test]
    fn earliest_snapshot_reports_the_start_of_each_series() {
        let mut storage = Storage::open_in_memory().unwrap();
        storage.upsert_account(&account()).unwrap();
        assert_eq!(
            storage.earliest_snapshot(&account().id, None).unwrap(),
            None,
            "nothing recorded has no beginning"
        );

        // The 5h window starts first; the weekly one appears a poll later.
        let first = vec![window("session_5h", 10.0)];
        let e = event(PollOutcome::Success {
            windows: first.clone(),
        });
        storage.record_poll(&e, &first, &[]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let both = vec![window("session_5h", 20.0), window("weekly", 5.0)];
        let e = event(PollOutcome::Success {
            windows: both.clone(),
        });
        storage.record_poll(&e, &both, &[]).unwrap();

        let account_start = storage
            .earliest_snapshot(&account().id, None)
            .unwrap()
            .expect("two polls recorded");
        let weekly_start = storage
            .earliest_snapshot(&account().id, Some(&WindowId::from("weekly")))
            .unwrap()
            .expect("the weekly window was recorded once");
        assert!(
            weekly_start > account_start,
            "a window that started later has its own, later start"
        );

        // A window that was never recorded, and an account that does not
        // exist, both simply have no history rather than erroring.
        assert_eq!(
            storage
                .earliest_snapshot(&account().id, Some(&WindowId::from("nope")))
                .unwrap(),
            None
        );
        assert_eq!(
            storage
                .earliest_snapshot(&AccountId::from("claude:other"), None)
                .unwrap(),
            None
        );
    }
}
