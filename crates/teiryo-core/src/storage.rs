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
CREATE INDEX IF NOT EXISTS idx_poll_lookup ON poll_event(provider, account_id, ts);
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

    /// Persist one poll event and its snapshot batch atomically. The
    /// snapshots are FK'd to the event so trigger, timestamp, and latency are
    /// never separated from the data they produced.
    pub fn record_poll(
        &mut self,
        event: &PollEvent,
        windows: &[QuotaWindow],
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
        tx.commit()?;
        Ok(())
    }

    /// Snapshots for one account (optionally one window) at or after `since`,
    /// oldest first.
    pub fn history(
        &self,
        account: &AccountId,
        window: Option<&WindowId>,
        since: DateTime<Utc>,
    ) -> Result<Vec<QuotaSnapshot>, StorageError> {
        let mut sql = String::from(
            "SELECT s.poll_id, e.ts, s.window_id, s.label, s.unit, s.used, s.limit_val, s.reset_at
             FROM quota_snapshot s JOIN poll_event e ON e.id = s.poll_id
             WHERE e.account_id = ?1 AND e.ts >= ?2",
        );
        if window.is_some() {
            sql.push_str(" AND s.window_id = ?3");
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
        let raw: Vec<_> = if let Some(w) = window {
            stmt.query_map(params![account.0, ts_to_millis(since), w.0], map_row)?
                .collect::<Result<_, _>>()?
        } else {
            stmt.query_map(params![account.0, ts_to_millis(since)], map_row)?
                .collect::<Result<_, _>>()?
        };
        raw.into_iter()
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
            .collect()
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
    fn record_and_query_roundtrip() {
        let mut storage = Storage::open_in_memory().unwrap();
        storage.upsert_account(&account()).unwrap();
        assert_eq!(storage.accounts().unwrap(), vec![account()]);

        let windows = vec![window("session_5h", 40.0), window("weekly", 12.5)];
        let ok = event(PollOutcome::Success {
            windows: windows.clone(),
        });
        storage.record_poll(&ok, &windows).unwrap();

        // ULID ordering (and thus recent_polls order) is only guaranteed
        // across distinct milliseconds.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let failed = event(PollOutcome::AuthError("expired".into()));
        storage.record_poll(&failed, &[]).unwrap();

        // History: both windows, then filtered to one.
        let since = Utc::now() - chrono::Duration::hours(1);
        let all = storage.history(&account().id, None, since).unwrap();
        assert_eq!(all.len(), 2);
        let one = storage
            .history(&account().id, Some(&WindowId::from("weekly")), since)
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
            storage.record_poll(&e, &[]).unwrap();
        }
        let reopened = Storage::open(&path).unwrap();
        assert_eq!(reopened.accounts().unwrap().len(), 1);
        assert_eq!(reopened.recent_polls(5).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
