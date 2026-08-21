//! Parser for the Claude OAuth usage endpoint.
//!
//! The exact response schema is an acknowledged open item, so parsing is
//! defensive: unknown fields are ignored, absent buckets are skipped, and a
//! payload with *no* recognized bucket (or a bucket missing its utilization)
//! is reported as [`ParseError::SchemaDrift`] rather than panicking.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use teiryo_core::{
    ParseError, QuotaUnit, QuotaWindow, RawResponse, ResetKind, WindowId, WindowScope,
};

/// The response shape this parser assumes for `GET /api/oauth/usage`
/// (utilization is percent used, 0–100):
///
/// ```json
/// {
///   "five_hour":        { "utilization": 34.0, "resets_at": "2026-08-21T12:00:00Z" },
///   "seven_day":        { "utilization": 61.0, "resets_at": "2026-08-25T00:00:00Z" },
///   "seven_day_opus":   { "utilization": 12.0, "resets_at": "2026-08-25T00:00:00Z" },
///   "seven_day_sonnet": { "utilization":  7.0, "resets_at": "2026-08-25T00:00:00Z" }
/// }
/// ```
///
/// `seven_day_opus`/`seven_day_sonnet` appear only on Max plans (separate
/// per-model buckets); Pro exposes the shared `five_hour`/`seven_day` pool.
pub const ASSUMED_SCHEMA: &str = "five_hour/seven_day[/seven_day_opus/seven_day_sonnet] \
     objects with utilization (percent used) and optional resets_at";

const FIVE_HOURS: Duration = Duration::from_secs(5 * 60 * 60);
const SEVEN_DAYS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Deserialize, Default)]
struct UsageResponse {
    #[serde(default)]
    five_hour: Option<UsageBucket>,
    #[serde(default)]
    seven_day: Option<UsageBucket>,
    #[serde(default)]
    seven_day_opus: Option<UsageBucket>,
    #[serde(default)]
    seven_day_sonnet: Option<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<DateTime<Utc>>,
}

struct BucketSpec {
    id: &'static str,
    label: &'static str,
    scope: WindowScope,
    length: Duration,
}

type BucketGetter = fn(&UsageResponse) -> &Option<UsageBucket>;

fn specs() -> [(BucketSpec, BucketGetter); 4] {
    [
        (
            BucketSpec {
                id: "session_5h",
                label: "Session — 5 hour",
                scope: WindowScope::AccountWide,
                length: FIVE_HOURS,
            },
            |r| &r.five_hour,
        ),
        (
            BucketSpec {
                id: "weekly",
                label: "Weekly — all models",
                scope: WindowScope::AccountWide,
                length: SEVEN_DAYS,
            },
            |r| &r.seven_day,
        ),
        (
            BucketSpec {
                id: "weekly_opus",
                label: "Weekly — Opus",
                scope: WindowScope::Model("opus".to_owned()),
                length: SEVEN_DAYS,
            },
            |r| &r.seven_day_opus,
        ),
        (
            BucketSpec {
                id: "weekly_sonnet",
                label: "Weekly — Sonnet",
                scope: WindowScope::Model("sonnet".to_owned()),
                length: SEVEN_DAYS,
            },
            |r| &r.seven_day_sonnet,
        ),
    ]
}

/// Display order for Claude windows: session first, then weekly buckets.
pub(crate) fn group_order() -> Vec<WindowId> {
    specs()
        .into_iter()
        .map(|(s, _)| WindowId::from(s.id))
        .collect()
}

/// Parse one usage response into quota windows.
pub(crate) fn parse(raw: &RawResponse) -> Result<Vec<QuotaWindow>, ParseError> {
    if raw.status != 200 {
        return Err(ParseError::SchemaDrift(format!(
            "expected HTTP 200 from usage endpoint, got {}",
            raw.status
        )));
    }
    let usage: UsageResponse = serde_json::from_slice(&raw.body).map_err(|e| {
        ParseError::SchemaDrift(format!("usage payload is not the expected JSON: {e}"))
    })?;

    let mut windows = Vec::new();
    for (spec, get) in specs() {
        let Some(bucket) = get(&usage) else { continue };
        let used = bucket
            .utilization
            .ok_or_else(|| ParseError::SchemaDrift(format!("{}.utilization missing", spec.id)))?;
        windows.push(QuotaWindow {
            id: WindowId::from(spec.id),
            label: spec.label.to_owned(),
            scope: spec.scope,
            reset_kind: ResetKind::Rolling(spec.length),
            unit: QuotaUnit::Percent,
            used,
            limit: Some(100.0),
            reset_at: bucket.resets_at,
        });
    }
    if windows.is_empty() {
        return Err(ParseError::SchemaDrift(format!(
            "no recognized usage windows in payload; assumed schema: {ASSUMED_SCHEMA}"
        )));
    }
    Ok(windows)
}

/// A representative window for tests.
#[cfg(test)]
pub(crate) fn test_window() -> QuotaWindow {
    QuotaWindow {
        id: WindowId::from("session_5h"),
        label: "Session — 5 hour".to_owned(),
        scope: WindowScope::AccountWide,
        reset_kind: ResetKind::Rolling(FIVE_HOURS),
        unit: QuotaUnit::Percent,
        used: 34.0,
        limit: Some(100.0),
        reset_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: u16, body: &str) -> RawResponse {
        RawResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
            fetched_at: Utc::now(),
        }
    }

    #[test]
    fn parses_pro_plan_shared_pool() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":34.5,"resets_at":"2026-08-21T12:00:00Z"},
                "seven_day":{"utilization":61.0,"resets_at":"2026-08-25T00:00:00Z"}}"#,
        ))
        .unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, WindowId::from("session_5h"));
        assert_eq!(windows[0].used, 34.5);
        assert_eq!(windows[0].limit, Some(100.0));
        assert_eq!(windows[0].unit, QuotaUnit::Percent);
        assert_eq!(windows[0].scope, WindowScope::AccountWide);
        assert!(windows[0].reset_at.is_some());
        assert_eq!(windows[1].id, WindowId::from("weekly"));
    }

    #[test]
    fn parses_max_plan_per_model_buckets() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":10},
                "seven_day":{"utilization":20},
                "seven_day_opus":{"utilization":30},
                "seven_day_sonnet":{"utilization":40}}"#,
        ))
        .unwrap();
        assert_eq!(windows.len(), 4);
        assert_eq!(windows[2].scope, WindowScope::Model("opus".to_owned()));
        assert_eq!(windows[3].scope, WindowScope::Model("sonnet".to_owned()));
    }

    #[test]
    fn ignores_unknown_fields() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5,"extra":true},"future_bucket":{"utilization":1},"plan":"max"}"#,
        ))
        .unwrap();
        assert_eq!(windows.len(), 1);
    }

    #[test]
    fn missing_utilization_is_schema_drift() {
        let err = parse(&raw(200, r#"{"five_hour":{"resets_at":null}}"#)).unwrap_err();
        let ParseError::SchemaDrift(msg) = err;
        assert!(msg.contains("session_5h.utilization"), "got: {msg}");
    }

    #[test]
    fn no_recognized_windows_is_schema_drift() {
        let err = parse(&raw(200, r#"{"totally":"different"}"#)).unwrap_err();
        let ParseError::SchemaDrift(msg) = err;
        assert!(msg.contains("no recognized usage windows"), "got: {msg}");
    }

    #[test]
    fn non_json_body_is_schema_drift() {
        let err = parse(&raw(200, "<html>maintenance</html>")).unwrap_err();
        let ParseError::SchemaDrift(msg) = err;
        assert!(msg.contains("not the expected JSON"), "got: {msg}");
    }

    #[test]
    fn non_200_status_is_schema_drift() {
        let err = parse(&raw(500, "{}")).unwrap_err();
        let ParseError::SchemaDrift(msg) = err;
        assert!(msg.contains("500"), "got: {msg}");
    }
}
