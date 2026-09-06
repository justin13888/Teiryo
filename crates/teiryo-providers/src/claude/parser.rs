//! Parser for the Claude OAuth usage endpoint.
//!
//! The exact response schema is an acknowledged open item, so parsing is
//! defensive: unknown fields are ignored, absent buckets are skipped, and a
//! payload with *no* recognized bucket (or a bucket missing its utilization)
//! is reported as [`ParseError::SchemaDrift`] rather than panicking.
//!
//! Two shapes carry windows. The fixed top-level buckets (`five_hour`,
//! `seven_day`, `seven_day_opus`, `seven_day_sonnet`) are the long-standing
//! ones. The `limits[]` array is newer and is the only place some per-model
//! weekly caps appear: on plans where a model such as Fable has its own weekly
//! allowance, the server emits a `weekly_scoped` row naming the model instead
//! of a dedicated top-level bucket. Rows of other kinds (`session`,
//! `weekly_all`) restate the fixed buckets and are skipped.

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
///   "seven_day_sonnet": { "utilization":  7.0, "resets_at": "2026-08-25T00:00:00Z" },
///   "limits": [
///     { "kind": "session",       "group": "session", "percent": 34, "resets_at": "...", "scope": null },
///     { "kind": "weekly_all",    "group": "weekly",  "percent": 61, "resets_at": "...", "scope": null },
///     { "kind": "weekly_scoped", "group": "weekly",  "percent": 48, "resets_at": "...",
///       "scope": { "model": { "id": null, "display_name": "Fable" } } }
///   ]
/// }
/// ```
///
/// `seven_day_opus`/`seven_day_sonnet` appear only on Max plans (separate
/// per-model buckets); Pro exposes the shared `five_hour`/`seven_day` pool.
/// A model whose weekly allowance is included with the plan but has no
/// top-level bucket (Fable, on Max) shows up only as a `weekly_scoped` row in
/// `limits[]`, labelled by the server.
pub const ASSUMED_SCHEMA: &str = "five_hour/seven_day[/seven_day_opus/seven_day_sonnet] \
     objects with utilization (percent used) and optional resets_at, plus limits[] rows \
     of kind weekly_scoped with percent, resets_at and scope.model.display_name";

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
    /// Kept as raw JSON: the field may be absent, `null`, or not an array,
    /// and one row in an unexpected shape must not take the recognized
    /// buckets down with it, so rows are decoded one at a time.
    #[serde(default)]
    limits: serde_json::Value,
}

/// One row of `limits[]`. Only `weekly_scoped` rows are mapped; the rest are
/// restatements of the fixed buckets.
#[derive(Deserialize)]
struct LimitEntry {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    scope: Option<LimitScope>,
}

#[derive(Deserialize)]
struct LimitScope {
    #[serde(default)]
    model: Option<LimitModel>,
}

#[derive(Deserialize)]
struct LimitModel {
    #[serde(default)]
    display_name: Option<String>,
}

/// The `limits[].kind` of a per-model weekly window.
const WEEKLY_SCOPED_KIND: &str = "weekly_scoped";

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
/// Only the fixed buckets have a fixed place; [`parse`] emits the windows it
/// derives from `limits[]` after them, in server order.
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
    for value in usage.limits.as_array().into_iter().flatten() {
        let entry = match LimitEntry::deserialize(value) {
            Ok(entry) => entry,
            Err(e) => {
                // Dropping the row loses at most that one window for this
                // poll; failing the poll would lose the fixed buckets too.
                tracing::warn!(row = %value, error = %e, "skipping unreadable limits[] row");
                continue;
            }
        };
        if entry.kind.as_deref() != Some(WEEKLY_SCOPED_KIND) {
            continue;
        }
        let Some(name) = entry
            .scope
            .and_then(|s| s.model)
            .and_then(|m| m.display_name)
        else {
            continue;
        };
        let model = model_slug(&name);
        if model.is_empty() {
            continue;
        }
        let id = WindowId(format!("weekly_{model}"));
        // A fixed bucket for the same model (e.g. `seven_day_opus`) wins: the
        // id keys stored history, so one cap must not appear twice.
        if windows.iter().any(|w| w.id == id) {
            continue;
        }
        let used = entry.percent.ok_or_else(|| {
            ParseError::SchemaDrift(format!(
                "limits[{WEEKLY_SCOPED_KIND}:{name}].percent missing"
            ))
        })?;
        windows.push(QuotaWindow {
            id,
            label: format!("Weekly — {name}"),
            scope: WindowScope::Model(model),
            reset_kind: ResetKind::Rolling(SEVEN_DAYS),
            unit: QuotaUnit::Percent,
            used,
            limit: Some(100.0),
            reset_at: entry.resets_at,
        });
    }
    if windows.is_empty() {
        return Err(ParseError::SchemaDrift(format!(
            "no recognized usage windows in payload; assumed schema: {ASSUMED_SCHEMA}"
        )));
    }
    Ok(windows)
}

/// Lower-case `[a-z0-9]` runs joined by `_`: the form the fixed buckets use
/// for their model (`"opus"`, `"sonnet"`), so `"Fable"` lands as `"fable"`.
fn model_slug(display_name: &str) -> String {
    let mut slug = String::with_capacity(display_name.len());
    for c in display_name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('_') {
            slug.push('_');
        }
    }
    slug.trim_end_matches('_').to_owned()
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

    /// The shape a Max account returns when Fable's weekly allowance is
    /// included with the plan: no `seven_day_opus`/`seven_day_sonnet`, and
    /// the Fable cap only as a `weekly_scoped` row.
    const MAX_WITH_FABLE: &str = r#"{
        "five_hour": {"utilization": 27.0, "resets_at": "2026-09-06T18:50:00.313970+00:00",
                      "limit_dollars": null, "locked_reason": null},
        "seven_day": {"utilization": 41.0, "resets_at": "2026-09-12T13:00:00.313996+00:00"},
        "seven_day_oauth_apps": null,
        "seven_day_opus": null,
        "seven_day_sonnet": null,
        "extra_usage": {"is_enabled": false, "utilization": 0.0},
        "limits": [
            {"kind": "session", "group": "session", "percent": 27, "severity": "normal",
             "resets_at": "2026-09-06T18:50:00.313970+00:00", "scope": null, "is_active": false},
            {"kind": "weekly_all", "group": "weekly", "percent": 41, "severity": "normal",
             "resets_at": "2026-09-12T13:00:00.313996+00:00", "scope": null, "is_active": false},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 48, "severity": "normal",
             "resets_at": "2026-09-12T13:00:00.314237+00:00",
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
             "is_active": true}
        ]
    }"#;

    #[test]
    fn parses_model_scoped_weekly_window_from_limits() {
        let windows = parse(&raw(200, MAX_WITH_FABLE)).unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly", "weekly_fable"]);
        let fable = &windows[2];
        assert_eq!(fable.label, "Weekly — Fable");
        assert_eq!(fable.scope, WindowScope::Model("fable".to_owned()));
        assert_eq!(fable.reset_kind, ResetKind::Rolling(SEVEN_DAYS));
        assert_eq!(fable.unit, QuotaUnit::Percent);
        assert_eq!(fable.used, 48.0);
        assert_eq!(fable.limit, Some(100.0));
        assert_eq!(
            fable.reset_at.map(|t| t.to_rfc3339()),
            Some("2026-09-12T13:00:00.314237+00:00".to_owned())
        );
    }

    #[test]
    fn limits_rows_that_restate_fixed_buckets_are_skipped() {
        // `seven_day_opus` and a `weekly_scoped` "Opus" row describe one cap;
        // the fixed bucket keeps the `weekly_opus` id and the row adds nothing.
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":10},
                "seven_day":{"utilization":20},
                "seven_day_opus":{"utilization":30},
                "limits":[
                    {"kind":"session","percent":10,"scope":null},
                    {"kind":"weekly_all","percent":20,"scope":null},
                    {"kind":"weekly_scoped","percent":30,
                     "scope":{"model":{"display_name":"Opus"}}}
                ]}"#,
        ))
        .unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly", "weekly_opus"]);
        assert_eq!(windows[2].used, 30.0);
    }

    #[test]
    fn malformed_limits_rows_are_ignored() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5},
                "limits":[
                    "not an object",
                    {"kind":"weekly_scoped","percent":"48","scope":{"model":{"display_name":"Fable"}}},
                    {"kind":"weekly_scoped","percent":1,"scope":"Fable"},
                    {"kind":"weekly_scoped","percent":2,"scope":{"model":null}},
                    {"kind":"weekly_scoped","percent":3,"scope":{"model":{"display_name":"  "}}},
                    {"kind":"future_kind","percent":4,"scope":{"model":{"display_name":"Fable"}}},
                    {"kind":"weekly_scoped","percent":6,
                     "scope":{"model":{"display_name":"Fable 5.1"}}}
                ]}"#,
        ))
        .unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly_fable_5_1"]);
        assert_eq!(windows[1].scope, WindowScope::Model("fable_5_1".to_owned()));
        assert_eq!(windows[1].label, "Weekly — Fable 5.1");
    }

    #[test]
    fn weekly_scoped_row_without_percent_is_schema_drift() {
        let err = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5},
                "limits":[{"kind":"weekly_scoped","resets_at":null,
                           "scope":{"model":{"display_name":"Fable"}}}]}"#,
        ))
        .unwrap_err();
        let ParseError::SchemaDrift(msg) = err;
        assert!(
            msg.contains("limits[weekly_scoped:Fable].percent"),
            "got: {msg}"
        );
    }

    #[test]
    fn null_or_non_array_limits_leave_fixed_buckets_alone() {
        for limits in ["null", "{}", "\"weekly_scoped\"", "7"] {
            let body = format!(r#"{{"five_hour":{{"utilization":5}},"limits":{limits}}}"#);
            let windows = parse(&raw(200, &body)).unwrap_or_else(|e| panic!("{limits}: {e:?}"));
            assert_eq!(windows.len(), 1, "limits = {limits}");
        }
    }

    #[test]
    fn repeated_scoped_rows_for_one_model_keep_the_first() {
        let windows = parse(&raw(
            200,
            r#"{"limits":[
                {"kind":"weekly_scoped","percent":48,"scope":{"model":{"display_name":"Fable"}}},
                {"kind":"weekly_scoped","percent":99,"scope":{"model":{"display_name":"fable"}}}
            ]}"#,
        ))
        .unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used, 48.0);
    }

    #[test]
    fn limits_alone_still_yield_windows() {
        let windows = parse(&raw(
            200,
            r#"{"limits":[{"kind":"weekly_scoped","percent":48,
                           "scope":{"model":{"display_name":"Fable"}}}]}"#,
        ))
        .unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, WindowId::from("weekly_fable"));
        assert!(windows[0].reset_at.is_none());
    }
}
