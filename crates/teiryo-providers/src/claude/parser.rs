//! Parser for the Claude OAuth usage endpoint.
//!
//! The exact response schema is an acknowledged open item, so parsing is
//! defensive: unknown fields are ignored, and anything unreadable costs at
//! most what it describes. A bucket or row with no usable reading is dropped
//! with a warning; only a payload with *no* recognized window left at all is
//! reported as [`ParseError::SchemaDrift`].
//!
//! That leniency is uniform on purpose. Failing the poll over one bad reading
//! costs every window that parsed, which freezes an established account on
//! stale numbers and shows a fresh one nothing, with no operator remedy — and
//! a rule that spared `limits[]` rows while a missing `utilization` still took
//! the whole poll down simply moved the same defect one field along. What
//! survives is the floor: nothing readable anywhere is still drift.
//!
//! Two shapes carry windows. The fixed top-level buckets (`five_hour`,
//! `seven_day`, `seven_day_opus`, `seven_day_sonnet`) are the long-standing
//! ones. The `limits[]` array is newer and is the only place some per-model
//! weekly caps appear: on plans where a model such as Fable has its own weekly
//! allowance, the server emits a `weekly_scoped` row naming the model instead
//! of a dedicated top-level bucket. Rows of other kinds (`session`,
//! `weekly_all`) restate the fixed buckets and are skipped.
//!
//! A derived window's id comes from `scope.model.id` where the server sends
//! one, and from the display name's slug otherwise. The id is a durable
//! storage key rather than a caption — `quota_snapshot` and `window_rollover`
//! are both keyed on it — so deriving it from a label the provider may rewrite
//! orphans that window's history the day the model is renamed, silently.

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
    /// Kept as raw, *undecoded* JSON: the field may be absent, `null`, or not
    /// an array, and one row in an unexpected shape must not take the
    /// recognized buckets down with it.
    ///
    /// `RawValue` rather than `Value`, and the difference is the whole point.
    /// A `Value` is built during this outer `from_slice`, which converts every
    /// number in the array before any row has been looked at — so a single
    /// unrepresentable one, in a row this parser skips and a field it never
    /// reads, failed the entire poll and took the fixed buckets with it. A
    /// `RawValue` holds the bytes and converts nothing until each row is
    /// examined on its own, which is what the isolation above claims.
    #[serde(default)]
    limits: Option<Box<serde_json::value::RawValue>>,
}

/// One row of `limits[]`. Only `weekly_scoped` rows are mapped; the rest are
/// restatements of the fixed buckets.
#[derive(Deserialize)]
struct LimitEntry {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    /// Unreadable rather than absent costs the instant, not the window: a
    /// window with no reset instant is a shape the rest of the codebase
    /// already handles, so a `resets_at` this parser cannot read is no reason
    /// to discard a cap the user is being charged against.
    #[serde(default, deserialize_with = "lenient_instant")]
    resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    scope: Option<LimitScope>,
}

/// Deserialize an instant, treating anything unreadable as absent.
fn lenient_instant<'de, D>(de: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(de)?;
    Ok(raw.and_then(|v| serde_json::from_value(v).ok()))
}

#[derive(Deserialize)]
struct LimitScope {
    #[serde(default)]
    model: Option<LimitModel>,
}

#[derive(Deserialize)]
struct LimitModel {
    /// The server's own identifier for the model, when it sends one.
    ///
    /// Preferred over `display_name` for the window id. The id keys stored
    /// history — `quota_snapshot` and `window_rollover` are both keyed on
    /// `(poll, window)` — so an id derived from a label the provider may
    /// rewrite orphans a window's whole series the day it renames the model,
    /// silently: `rollover::detect` skips windows present in only one of two
    /// polls, so there is no rollover, no warning, and no repair path.
    ///
    /// Every response seen so far sends `null` here, which is why the slug is
    /// still the fallback rather than the other way round — and why adopting
    /// this costs exactly one rename, on the first poll that populates it.
    #[serde(default)]
    id: Option<String>,
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
        let Some(used) = bucket.utilization else {
            // The same trade a `limits[]` row gets, and for the same reason:
            // one reading the server did not send must not cost the poll every
            // window that did arrive. Losing them freezes an established
            // account on stale numbers and shows a fresh one nothing, with no
            // operator remedy — which is the cost this parser exists to avoid.
            // A payload with nothing readable in it still fails, at the
            // `windows.is_empty()` check below.
            tracing::warn!(
                window = spec.id,
                "skipping fixed bucket with no utilization"
            );
            continue;
        };
        windows.push(QuotaWindow {
            id: WindowId::from(spec.id),
            label: spec.label.to_owned(),
            scope: spec.scope,
            reset_kind: ResetKind::Rolling(spec.length),
            unit: QuotaUnit::Percent,
            used: checked_percent(spec.id, used),
            limit: Some(100.0),
            reset_at: bucket.resets_at,
        });
    }
    for value in limit_rows(usage.limits.as_deref()) {
        let entry = match serde_json::from_str::<LimitEntry>(value.get()) {
            Ok(entry) => entry,
            Err(e) => {
                // Dropping the row loses at most that one window for this
                // poll; failing the poll would lose the fixed buckets too.
                tracing::warn!(row = %value, error = %e, "skipping unreadable limits[] row");
                continue;
            }
        };
        // Rows of other kinds restate the fixed buckets. Not a warning: they
        // are expected, and every payload carries them.
        if entry.kind.as_deref() != Some(WEEKLY_SCOPED_KIND) {
            continue;
        }
        // From here down every `continue` discards a row this parser has
        // already recognised as a per-model cap the user is charged against,
        // so each one says so. Silently dropping them made a cap the server
        // reported simply absent from the dashboard, with nothing anywhere to
        // explain it.
        let Some(model) = entry.scope.and_then(|s| s.model) else {
            tracing::warn!(
                row = %value,
                "skipping {WEEKLY_SCOPED_KIND} row with no scope.model to name it"
            );
            continue;
        };
        let name = model.display_name.as_deref().unwrap_or_default();
        // The server's own id when it sends one, the label's slug otherwise.
        // See `LimitModel::id`: this is a durable storage key, not a caption.
        let slug = match model.id.as_deref().map(model_slug) {
            Some(id) if !id.is_empty() => id,
            _ => model_slug(name),
        };
        if slug.is_empty() {
            tracing::warn!(
                row = %value,
                display_name = name,
                "skipping {WEEKLY_SCOPED_KIND} row whose model name yields no id"
            );
            continue;
        }
        let id = WindowId(format!("weekly_{slug}"));
        // A fixed bucket for the same model (e.g. `seven_day_opus`) wins: the
        // id keys stored history, so one cap must not appear twice. Matched by
        // alias rather than by exact slug — the server names the same cap
        // "Opus", "Claude Opus 4.5" and whatever it renames it to next, and an
        // exact comparison catches only the first, listing one cap twice under
        // two confusable labels.
        if windows
            .iter()
            .any(|w| w.id == id || covers_same_model(w, &slug))
        {
            continue;
        }
        let Some(used) = entry.percent else {
            // Same trade as an unreadable row: one row with no reading must
            // not cost the poll the windows that already parsed.
            tracing::warn!(
                row = %value,
                model = name,
                "skipping limits[] row of kind {WEEKLY_SCOPED_KIND} with no percent"
            );
            continue;
        };
        let label = if name.is_empty() {
            format!("Weekly — {slug}")
        } else {
            format!("Weekly — {name}")
        };
        windows.push(QuotaWindow {
            id,
            label,
            scope: WindowScope::Model(slug.clone()),
            reset_kind: ResetKind::Rolling(SEVEN_DAYS),
            unit: QuotaUnit::Percent,
            used: checked_percent(&slug, used),
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
/// The rows of `limits[]`, still undecoded.
///
/// Absent or `null` is the ordinary case for a plan with no per-model caps and
/// says nothing worth logging. Present but not an array is the provider having
/// changed the field's shape, which is worth a line — `docs/providers.md` said
/// this warned long before anything did: the old `as_array().into_iter()`
/// yielded an empty iterator and emitted nothing at all.
fn limit_rows(limits: Option<&serde_json::value::RawValue>) -> Vec<&serde_json::value::RawValue> {
    let Some(raw) = limits else { return Vec::new() };
    if raw.get().trim() == "null" {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<&serde_json::value::RawValue>>(raw.get()) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "ignoring a limits field that is not an array");
            Vec::new()
        }
    }
}

/// A reading outside `0..=100`, reported and kept.
///
/// Kept because it is the provider's number and may be a genuine overage;
/// reported because it is equally a sign the field stopped meaning percent.
/// `QuotaWindow::utilization` clamps for display either way, so nothing
/// downstream is at risk from the value itself — only from nobody noticing.
fn checked_percent(window: &str, used: f64) -> f64 {
    if !(0.0..=100.0).contains(&used) {
        tracing::warn!(window, used, "usage reading outside 0..=100");
    }
    used
}

/// Whether an already-mapped window is the same model cap as `slug`.
///
/// The fixed buckets are `weekly_opus` and `weekly_sonnet`, and the server
/// names those same caps in `limits[]` with whatever label it currently uses —
/// "Opus", "Claude Opus 4.5". Comparing ids exactly recognises only the bare
/// form, so any other label produced a second window for a cap already listed.
/// Matching on the fixed bucket's model name appearing in the candidate slug
/// catches the family without needing to know the naming scheme.
fn covers_same_model(mapped: &QuotaWindow, slug: &str) -> bool {
    let WindowScope::Model(model) = &mapped.scope else {
        return false;
    };
    mapped.id.0.starts_with("weekly_")
        && (slug == model
            || slug.starts_with(&format!("{model}_"))
            || slug.ends_with(&format!("_{model}"))
            || slug.contains(&format!("_{model}_")))
}

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

    /// Replaces `missing_utilization_is_schema_drift`. A fixed bucket with no
    /// reading used to fail the whole poll, which is the mirror image of the
    /// defect the `limits[]` row policy was written to avoid: the `?` fired in
    /// the first loop, before `limits[]` ran, so a perfectly readable
    /// per-model cap was thrown away over an unrelated bucket.
    #[test]
    fn a_bucket_with_no_utilization_is_skipped_keeping_everything_else() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"resets_at":null},
                "seven_day":{"utilization":41},
                "limits":[
                    {"kind":"weekly_scoped","percent":48,
                     "scope":{"model":{"display_name":"Fable"}}}
                ]}"#,
        ))
        .unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["weekly", "weekly_fable"]);
    }

    /// The floor under that leniency: skipping every bucket is still drift.
    #[test]
    fn a_payload_with_no_readable_window_at_all_is_still_schema_drift() {
        let err = parse(&raw(200, r#"{"five_hour":{"resets_at":null}}"#)).unwrap_err();
        let ParseError::SchemaDrift(msg) = err;
        assert!(msg.contains("no recognized usage windows"), "got: {msg}");
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

    /// Run `f`, returning everything it logged.
    ///
    /// The parser's whole answer to a row it cannot use is a warning: the row
    /// is dropped, the poll survives, and the operator log is the only place
    /// the gap is visible. Deleting any of those warnings left all 22 tests
    /// green, which made the advertised signal a comment.
    fn captured_logs(f: impl FnOnce()) -> String {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let made = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || made.clone())
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = sink.0.lock().unwrap().clone();
        String::from_utf8(bytes).expect("utf-8 log output")
    }

    /// The regression this whole field was re-typed for. An out-of-range
    /// number anywhere in `limits[]` used to fail the entire poll — in a row
    /// the parser skips (`kind: session`) and a field it never reads (`cost`)
    /// — discarding the good `five_hour` bucket with it. `Value` converts
    /// every number during the outer parse; `RawValue` converts none.
    #[test]
    fn an_out_of_range_number_in_limits_does_not_cost_the_poll() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5},
                "limits":[{"kind":"session","percent":27,"cost":1e400}]}"#,
        ))
        .expect("the fixed bucket survives a row this parser never reads");
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h"]);
    }

    /// And the same magnitude inside a row the parser *does* read costs that
    /// row alone, with a line saying so.
    #[test]
    fn an_out_of_range_number_in_a_scoped_row_costs_only_that_row() {
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
                200,
                r#"{"five_hour":{"utilization":5},
                    "limits":[
                        {"kind":"weekly_scoped","percent":1e400,
                         "scope":{"model":{"display_name":"Fable"}}},
                        {"kind":"weekly_scoped","percent":48,
                         "scope":{"model":{"display_name":"Opus"}}}
                    ]}"#,
            ))
            .unwrap();
        });
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly_opus"]);
        assert!(logs.contains("skipping unreadable limits[] row"), "{logs}");
    }

    #[test]
    fn an_unreadable_resets_at_costs_the_instant_not_the_window() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5},
                "limits":[{"kind":"weekly_scoped","percent":48,"resets_at":"soon",
                           "scope":{"model":{"display_name":"Fable"}}}]}"#,
        ))
        .unwrap();
        // A window with no reset instant is a shape the rest of the codebase
        // already handles; losing the whole cap is stricter than it needs to be.
        assert_eq!(windows[1].id.0, "weekly_fable");
        assert_eq!(windows[1].used, 48.0);
        assert_eq!(windows[1].reset_at, None);
    }

    #[test]
    fn a_reading_outside_the_percent_range_is_kept_and_reported() {
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
                200,
                r#"{"five_hour":{"utilization":140},
                    "limits":[{"kind":"weekly_scoped","percent":-3,
                               "scope":{"model":{"display_name":"Fable"}}}]}"#,
            ))
            .unwrap();
        });
        // Kept: it is the provider's number and may be a real overage, and
        // `utilization()` clamps for display anyway. Reported: it is equally a
        // sign the field stopped meaning percent. Both shapes alike — the
        // fixed buckets never clamped either.
        assert_eq!(windows[0].used, 140.0);
        assert_eq!(windows[1].used, -3.0);
        assert_eq!(
            logs.matches("usage reading outside 0..=100").count(),
            2,
            "{logs}"
        );
    }

    #[test]
    fn a_model_name_that_yields_no_id_is_skipped_with_a_reason() {
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
                200,
                r#"{"five_hour":{"utilization":5},
                    "limits":[{"kind":"weekly_scoped","percent":48,
                               "scope":{"model":{"display_name":"オーパス"}}}]}"#,
            ))
            .unwrap();
        });
        // `model_slug` keeps ASCII alphanumerics only, so this name slugs to
        // nothing. The cap still cannot be tracked, but it is no longer
        // invisible: it used to vanish with no log line and no UI indication.
        assert_eq!(windows.len(), 1);
        assert!(logs.contains("yields no id"), "{logs}");
    }

    #[test]
    fn a_scoped_row_with_no_model_is_skipped_with_a_reason() {
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
                200,
                r#"{"five_hour":{"utilization":5},
                    "limits":[
                        {"kind":"weekly_scoped","percent":48,"scope":null},
                        {"kind":"weekly_scoped","percent":49}
                    ]}"#,
            ))
            .unwrap();
        });
        assert_eq!(windows.len(), 1);
        assert_eq!(
            logs.matches("no scope.model to name it").count(),
            2,
            "{logs}"
        );
    }

    /// #9's decision, and the one rename it costs.
    #[test]
    fn the_servers_own_model_id_is_preferred_over_its_label() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5},
                "limits":[{"kind":"weekly_scoped","percent":48,
                           "scope":{"model":{"id":"claude-fable-5-1","display_name":"Fable 5.1"}}}]}"#,
        ))
        .unwrap();
        // The id keys stored history and the label does not, so the label may
        // change freely from here without orphaning the series.
        assert_eq!(windows[1].id.0, "weekly_claude_fable_5_1");
        assert_eq!(
            windows[1].scope,
            WindowScope::Model("claude_fable_5_1".to_owned())
        );
        // The caption still reads as the server writes it.
        assert_eq!(windows[1].label, "Weekly — Fable 5.1");
    }

    #[test]
    fn a_null_model_id_falls_back_to_the_label() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5},
                "limits":[{"kind":"weekly_scoped","percent":48,
                           "scope":{"model":{"id":null,"display_name":"Fable"}}}]}"#,
        ))
        .unwrap();
        assert_eq!(windows[1].id.0, "weekly_fable");
    }

    /// The dedupe the old exact-slug comparison only appeared to do.
    #[test]
    fn a_fixed_bucket_suppresses_the_same_cap_under_any_label() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":10},
                "seven_day":{"utilization":20},
                "seven_day_opus":{"utilization":30},
                "seven_day_sonnet":{"utilization":40},
                "limits":[
                    {"kind":"weekly_scoped","percent":30,
                     "scope":{"model":{"display_name":"Claude Opus 4.5"}}},
                    {"kind":"weekly_scoped","percent":40,
                     "scope":{"model":{"display_name":"Sonnet 4.5"}}}
                ]}"#,
        ))
        .unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        // Not `weekly_claude_opus_4_5` and `weekly_sonnet_4_5` beside them:
        // that listed one cap twice under two confusable labels and inflated
        // the header's window count.
        assert_eq!(
            ids,
            ["session_5h", "weekly", "weekly_opus", "weekly_sonnet"]
        );
    }

    /// Mapping `session` and `weekly_all` rows instead of skipping them left
    /// all 22 tests green: every such row in every fixture carried
    /// `scope: null`, so the `display_name` guard dropped them regardless of
    /// the kind guard, and two units read as covered that were not.
    #[test]
    fn rows_of_other_kinds_are_skipped_even_when_they_name_a_model() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":10},
                "limits":[
                    {"kind":"session","percent":10,
                     "scope":{"model":{"display_name":"Fable"}}},
                    {"kind":"weekly_all","percent":20,
                     "scope":{"model":{"display_name":"Fable"}}}
                ]}"#,
        ))
        .unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h"]);
    }

    /// "Fixed buckets first, then `limits[]` windows in server order" is
    /// stated in the description, the module doc and `docs/providers.md`, and
    /// was asserted nowhere: no fixture yielded two derived windows, so
    /// reversing them left all 22 tests green.
    #[test]
    fn derived_windows_follow_the_fixed_buckets_in_server_order() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":10},
                "limits":[
                    {"kind":"weekly_scoped","percent":1,
                     "scope":{"model":{"display_name":"Zeta"}}},
                    {"kind":"weekly_scoped","percent":2,
                     "scope":{"model":{"display_name":"Alpha"}}}
                ]}"#,
        ))
        .unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly_zeta", "weekly_alpha"]);
    }

    #[test]
    fn an_empty_limits_array_leaves_the_fixed_buckets_alone() {
        let windows = parse(&raw(200, r#"{"five_hour":{"utilization":5},"limits":[]}"#)).unwrap();
        assert_eq!(windows.len(), 1);
    }

    #[test]
    fn a_limits_field_that_is_not_an_array_is_reported() {
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
                200,
                r#"{"five_hour":{"utilization":5},"limits":{"kind":"session"}}"#,
            ))
            .unwrap();
        });
        assert_eq!(windows.len(), 1);
        // The description and `docs/providers.md` both claimed this warned
        // long before anything did.
        assert!(logs.contains("not an array"), "{logs}");
    }

    #[test]
    fn limits_present_but_wholly_unusable_with_no_buckets_is_schema_drift() {
        let err = parse(&raw(
            200,
            r#"{"limits":[{"kind":"weekly_scoped","percent":48,"scope":null}]}"#,
        ))
        .unwrap_err();
        let ParseError::SchemaDrift(msg) = err;
        assert!(msg.contains("no recognized usage windows"), "got: {msg}");
    }

    /// `model_slug` had no direct test: every rule was reached only through
    /// `parse`, and swapping `is_ascii_alphanumeric` for `is_alphanumeric`,
    /// dropping the leading-separator guard, or dropping the trailing trim
    /// each left the suite green.
    #[test]
    fn model_slug_keeps_ascii_alphanumerics_and_collapses_the_rest() {
        assert_eq!(model_slug("Opus"), "opus");
        // Runs of separators collapse to one underscore.
        assert_eq!(model_slug("Claude Opus 4.5"), "claude_opus_4_5");
        assert_eq!(model_slug("Opus---4"), "opus_4");
        // No leading separator: the guard is `!slug.is_empty()`.
        assert_eq!(model_slug("  Opus"), "opus");
        assert_eq!(model_slug("...Opus"), "opus");
        // No trailing one either.
        assert_eq!(model_slug("Opus 4.5!"), "opus_4_5");
        assert_eq!(model_slug("Opus   "), "opus");
        // ASCII only, so a wholly non-ASCII name yields nothing at all.
        assert_eq!(model_slug("オーパス"), "");
        assert_eq!(model_slug("Opus 東京"), "opus");
        assert_eq!(model_slug(""), "");
    }

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
    fn weekly_scoped_row_without_percent_is_skipped_keeping_fixed_buckets() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":27},"seven_day":{"utilization":41},
                "limits":[{"kind":"weekly_scoped","resets_at":null,
                           "scope":{"model":{"display_name":"Fable"}}}]}"#,
        ))
        .unwrap();
        let ids: Vec<&str> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly"]);
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
