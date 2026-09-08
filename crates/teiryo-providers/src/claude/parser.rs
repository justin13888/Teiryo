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

use serde::Deserialize;
use serde_json::value::RawValue;
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
    limits: Option<Box<RawValue>>,
}

/// One row of `limits[]`. Only `weekly_scoped` rows are mapped; the rest are
/// restatements of the fixed buckets.
#[derive(Deserialize)]
struct LimitEntry {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    percent: Option<Box<RawValue>>,
    /// Held raw for the reason `limits` is. Unreadable rather than absent
    /// must cost the instant and not the window: a window with no reset
    /// instant is a shape the rest of the codebase already handles, so a
    /// `resets_at` this parser cannot read is no reason to discard a cap the
    /// user is charged against.
    ///
    /// A typed field cannot give that guarantee however leniently it is
    /// deserialized. Deserializing it at all converts the value, and a number
    /// outside `f64` fails the whole row before any leniency can run.
    #[serde(default)]
    resets_at: Option<Box<RawValue>>,
    #[serde(default)]
    scope: Option<LimitScope>,
}

/// Read one raw JSON value, treating anything unreadable as absent.
///
/// The point of holding these fields raw: conversion happens here, one field
/// at a time, so a value this parser cannot represent costs that field alone.
fn readable<T: serde::de::DeserializeOwned>(raw: Option<&RawValue>) -> Option<T> {
    serde_json::from_str(raw?.get()).ok()
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

/// One fixed top-level bucket.
///
/// Both fields raw, for the reason `limits` is: they are read during the outer
/// `from_slice`, so a strictly-typed one hands the whole poll to whichever
/// value the server got wrong. An unparseable `resets_at`, or a `utilization`
/// outside `f64`, blacked out every window on every account — the same blast
/// radius this parser exists to avoid, reached through a field nobody was
/// watching.
#[derive(Deserialize)]
struct UsageBucket {
    #[serde(default)]
    utilization: Option<Box<RawValue>>,
    #[serde(default)]
    resets_at: Option<Box<RawValue>>,
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

/// Whether `id` is one [`parse`] derived from a name the server chose.
///
/// Every window this parser emits is either a fixed bucket, whose id is one of
/// the compiled-in constants in [`specs`], or a `weekly_<slug>` built from a
/// `limits[]` row — where the slug comes from `scope.model.id` or the display
/// name, either of which the server can rewrite under a stored series. So the
/// question is answered by membership in the fixed set, not by the id's shape:
/// `weekly_opus` and a derived `weekly_fable` look alike and are not alike.
pub(crate) fn is_server_derived(id: &WindowId) -> bool {
    !specs().into_iter().any(|(spec, _)| spec.id == id.0)
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
    // The models the fixed buckets already account for — every bucket the
    // payload carries, readable or not. Only these suppress a `limits[]` row:
    // a row is never suppressed by another row, because two scoped rows are
    // two caps until their ids actually collide, and the exact check below
    // catches that.
    let mut fixed_models: Vec<String> = Vec::new();
    for (spec, get) in specs() {
        let Some(bucket) = get(&usage) else { continue };
        // The bucket being *present* is what suppresses a `limits[]` row for
        // the same model, not whether this poll's reading parsed. Otherwise a
        // reading the server got wrong once moved the model's cap to a second
        // id — `weekly_opus` on one poll, `weekly_claude_opus_4_5` on the
        // next, from payloads differing only in a field neither id is built
        // from. `rollover::detect` records nothing across an id change, so the
        // chart and the burn rate start over each time it flips.
        if let WindowScope::Model(model) = &spec.scope {
            fixed_models.push(model.clone());
        }
        let Some(used) = readable::<f64>(bucket.utilization.as_deref()) else {
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
            reset_at: readable(bucket.resets_at.as_deref()),
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
        // An id already used is one window's worth of history, so the second
        // row cannot have it. Which row that is comes down to array order —
        // "Opus 4.1" and "Opus-4-1" slug alike, and the second one's reading is
        // simply not reported. The drop says so: silence here meant a cap the
        // server sent went missing from the dashboard with nothing anywhere to
        // explain it.
        if windows.iter().any(|w| w.id == id) {
            tracing::warn!(
                row = %value,
                model = name,
                window = %id.0,
                "skipping {WEEKLY_SCOPED_KIND} row for a window id already reported; \
                 the first row carrying a percent wins"
            );
            continue;
        }
        // A fixed bucket for the same model (e.g. `seven_day_opus`) wins: the
        // id keys stored history, so one cap must not appear twice. Matched by
        // alias rather than by exact slug — the server names the same cap
        // "Opus", "Claude Opus 4.5" and whatever it renames it to next, and an
        // exact comparison catches only the first, listing one cap twice under
        // two confusable labels.
        if let Some(covered) = fixed_models.iter().find(|m| same_model(&slug, m)) {
            tracing::warn!(
                row = %value,
                model = name,
                covered_by = covered,
                "skipping {WEEKLY_SCOPED_KIND} row for a model a fixed bucket already reports"
            );
            continue;
        }
        let Some(used) = readable::<f64>(entry.percent.as_deref()) else {
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
        // The window id, as the fixed buckets pass: `window` means one thing,
        // and a search for the id that turned up in a `usage reading outside`
        // line found the scoped windows too, which passing the bare slug —
        // `fable`, never `weekly_fable` — quietly ruled out.
        let used = checked_percent(&id.0, used);
        windows.push(QuotaWindow {
            id,
            label,
            scope: WindowScope::Model(slug),
            reset_kind: ResetKind::Rolling(SEVEN_DAYS),
            unit: QuotaUnit::Percent,
            used,
            limit: Some(100.0),
            reset_at: readable(entry.resets_at.as_deref()),
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
fn limit_rows(limits: Option<&RawValue>) -> Vec<&RawValue> {
    let Some(raw) = limits else { return Vec::new() };
    if raw.get().trim() == "null" {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<&RawValue>>(raw.get()) {
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

/// Whether a scoped row's slug names the same model as a fixed bucket.
///
/// The fixed buckets are `opus` and `sonnet`; the server names those same caps
/// in `limits[]` with whatever label it currently uses — "Opus", "Claude Opus
/// 4.5". Comparing slugs exactly recognises only the bare form, so any other
/// label produced a second window for a cap already listed.
///
/// The comparison is on a canonical form, not on containment, and the
/// difference is not cosmetic. Containment suppresses every *neighbouring*
/// model: "Opus Mini" and "Mini Opus" both contain "opus", and dropping either
/// loses a cap the user is charged against — silently, and in the one
/// direction this parser must never fail. Over-reporting a duplicate is
/// cosmetic; under-reporting a cap is the defect being fixed. So only a vendor
/// prefix and a trailing version are discarded, and anything else that
/// distinguishes the name keeps the row.
fn same_model(slug: &str, fixed: &str) -> bool {
    canonical_model(slug) == fixed
}

/// A slug reduced to the model it names: no vendor prefix, no version tail.
///
/// A version tail is only a tail if a name precedes it, so a slug that is
/// nothing but digits is left alone rather than reduced to its first one.
fn canonical_model(slug: &str) -> String {
    let numeric = |p: &&str| p.chars().all(|c| c.is_ascii_digit());
    let mut parts: Vec<&str> = slug.split('_').filter(|p| !p.is_empty()).collect();
    if parts.first() == Some(&"claude") && parts.len() > 1 {
        parts.remove(0);
    }
    if parts.first().is_some_and(|p| !numeric(p)) {
        while parts.len() > 1 && parts.last().is_some_and(numeric) {
            parts.pop();
        }
    }
    parts.join("_")
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
            fetched_at: chrono::Utc::now(),
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
            // Field names and their `=` are separated by escape codes
            // otherwise, so an assertion on `name=` never matches.
            .with_ansi(false)
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
        // The row itself is readable now — only `percent` is not — so the
        // loss is reported against that field rather than against the row.
        // Holding each field raw is what makes the distinction possible.
        assert!(logs.contains("with no percent"), "{logs}");
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
        // `window` names a window id in both lines, so one grep finds both.
        assert!(logs.contains(r#"window="session_5h""#), "{logs}");
        assert!(logs.contains(r#"window="weekly_fable""#), "{logs}");
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

    /// A row the server named with an id but no caption still gets a caption:
    /// the slug, rather than a "Weekly — " with nothing after it.
    ///
    /// The three shapes are the same case to the parser — `display_name`
    /// absent, `null`, and the empty string all read as no name — and each
    /// needs a sluggable `scope.model.id` beside it, which is what carries the
    /// row past the empty-slug guard and into the fallback.
    #[test]
    fn a_model_with_an_id_but_no_name_is_captioned_by_its_slug() {
        for model in [
            r#"{"id":"claude-fable-5-1"}"#,
            r#"{"id":"claude-fable-5-1","display_name":null}"#,
            r#"{"id":"claude-fable-5-1","display_name":""}"#,
        ] {
            let windows = parse(&raw(
                200,
                &format!(
                    r#"{{"five_hour":{{"utilization":5}},
                         "limits":[{{"kind":"weekly_scoped","percent":48,
                                     "scope":{{"model":{model}}}}}]}}"#
                ),
            ))
            .unwrap();
            assert_eq!(windows[1].id.0, "weekly_claude_fable_5_1", "{model}");
            assert_eq!(windows[1].label, "Weekly — claude_fable_5_1", "{model}");
        }
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

    /// `null` is the ordinary shape for a plan with no per-model caps, so it
    /// must not be reported as a malformed field. Without this the fast path
    /// could be deleted and only the window count would notice — which it
    /// would not, since both routes yield no rows.
    #[test]
    fn a_null_limits_field_is_silent_rather_than_reported() {
        for body in [
            r#"{"five_hour":{"utilization":5},"limits":null}"#,
            r#"{"five_hour":{"utilization":5}}"#,
        ] {
            let mut windows = Vec::new();
            let logs = captured_logs(|| windows = parse(&raw(200, body)).unwrap());
            assert_eq!(windows.len(), 1, "body: {body}");
            assert!(logs.is_empty(), "body: {body}, logs: {logs}");
        }
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

    /// The dedupe must not eat a neighbouring model.
    ///
    /// An earlier attempt matched the fixed bucket's model name as a substring
    /// of the candidate slug, which suppressed "Opus Mini" and "Mini Opus"
    /// along with "Claude Opus 4.5" — losing caps the user is charged against,
    /// silently, in the one direction this parser must never fail. Over-
    /// reporting a duplicate is cosmetic; under-reporting a cap is the defect.
    #[test]
    fn a_neighbouring_model_is_not_mistaken_for_the_fixed_bucket() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":10},
                "seven_day_opus":{"utilization":30},
                "limits":[
                    {"kind":"weekly_scoped","percent":77,
                     "scope":{"model":{"display_name":"Opus Mini"}}},
                    {"kind":"weekly_scoped","percent":66,
                     "scope":{"model":{"display_name":"Mini Opus"}}},
                    {"kind":"weekly_scoped","percent":55,
                     "scope":{"model":{"display_name":"Opusx"}}}
                ]}"#,
        ))
        .unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(
            ids,
            [
                "session_5h",
                "weekly_opus",
                "weekly_opus_mini",
                "weekly_mini_opus",
                "weekly_opusx"
            ]
        );
    }

    /// And a scoped row never suppresses another scoped row. Only the fixed
    /// buckets suppress, because only they are known to restate a cap; two
    /// scoped rows are two caps until their ids actually collide.
    #[test]
    fn one_scoped_row_never_suppresses_another() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":10},
                "limits":[
                    {"kind":"weekly_scoped","percent":10,
                     "scope":{"model":{"display_name":"Fable"}}},
                    {"kind":"weekly_scoped","percent":20,
                     "scope":{"model":{"display_name":"Fable Mini"}}},
                    {"kind":"weekly_scoped","percent":30,
                     "scope":{"model":{"display_name":"Turbo Fable 2"}}}
                ]}"#,
        ))
        .unwrap();
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(
            ids,
            [
                "session_5h",
                "weekly_fable",
                "weekly_fable_mini",
                "weekly_turbo_fable_2"
            ]
        );
    }

    #[test]
    fn canonical_model_drops_only_a_vendor_prefix_and_a_version_tail() {
        assert_eq!(canonical_model("opus"), "opus");
        assert_eq!(canonical_model("claude_opus_4_5"), "opus");
        assert_eq!(canonical_model("opus_4_5"), "opus");
        // Anything that still distinguishes the name is kept.
        assert_eq!(canonical_model("opus_mini"), "opus_mini");
        assert_eq!(canonical_model("mini_opus"), "mini_opus");
        assert_eq!(canonical_model("opusx"), "opusx");
        // Never reduced away entirely.
        assert_eq!(canonical_model("claude"), "claude");
        assert_eq!(canonical_model("4_5"), "4_5");
        assert!(same_model("claude_opus_4_5", "opus"));
        assert!(!same_model("opus_mini", "opus"));
    }

    /// The suppression is a drop like any other, so it says so. Without this
    /// the one path that discards a row on purpose was also the one path that
    /// discarded it in silence.
    #[test]
    fn suppressing_a_restated_cap_says_so() {
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
                200,
                r#"{"five_hour":{"utilization":10},
                    "seven_day_opus":{"utilization":30},
                    "limits":[{"kind":"weekly_scoped","percent":30,
                               "scope":{"model":{"display_name":"Claude Opus 4.5"}}}]}"#,
            ))
            .unwrap();
        });
        assert_eq!(windows.len(), 2);
        assert!(logs.contains("a fixed bucket already reports"), "{logs}");
        assert!(logs.contains("covered_by"), "{logs}");
    }

    /// A fixed bucket is read at the *outer* parse, so a strictly-typed field
    /// there hands the whole poll to whichever value the server got wrong —
    /// the same blast radius `limits[]` was re-typed to avoid, reached through
    /// a field nobody was watching.
    #[test]
    fn an_unreadable_fixed_bucket_field_costs_that_field_not_the_poll() {
        // An instant the parser cannot read: the bucket survives without one.
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5,"resets_at":"soon"},
                "seven_day":{"utilization":41}}"#,
        ))
        .expect("an unreadable instant must not fail the poll");
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly"]);
        assert_eq!(windows[0].reset_at, None);
        assert_eq!(windows[0].used, 5.0);

        // A reading outside f64: that bucket is skipped, the others stand.
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":1e400},"seven_day":{"utilization":41}}"#,
        ))
        .expect("an unrepresentable reading must not fail the poll");
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["weekly"]);
    }

    /// The same guarantee for a scoped row, which a leniently-deserialized but
    /// still typed field could not give: deserializing at all converts the
    /// value, and a number outside `f64` failed the whole row first.
    #[test]
    fn an_unreadable_instant_in_a_scoped_row_costs_only_the_instant() {
        for body in [
            r#"{"five_hour":{"utilization":5},
                "limits":[{"kind":"weekly_scoped","percent":48,"resets_at":"soon",
                           "scope":{"model":{"display_name":"Fable"}}}]}"#,
            r#"{"five_hour":{"utilization":5},
                "limits":[{"kind":"weekly_scoped","percent":48,"resets_at":1e400,
                           "scope":{"model":{"display_name":"Fable"}}}]}"#,
        ] {
            let windows = parse(&raw(200, body)).unwrap();
            let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
            assert_eq!(ids, ["session_5h", "weekly_fable"], "body: {body}");
            assert_eq!(windows[1].used, 48.0);
            assert_eq!(windows[1].reset_at, None);
        }
    }

    #[test]
    fn a_skipped_fixed_bucket_says_so() {
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
                200,
                r#"{"five_hour":{"resets_at":null},"seven_day":{"utilization":41}}"#,
            ))
            .unwrap();
        });
        assert_eq!(windows.len(), 1);
        assert!(logs.contains("skipping fixed bucket"), "{logs}");
    }

    #[test]
    fn a_model_id_that_slugs_to_nothing_falls_back_to_the_label() {
        let windows = parse(&raw(
            200,
            r#"{"five_hour":{"utilization":5},
                "limits":[{"kind":"weekly_scoped","percent":48,
                           "scope":{"model":{"id":"日本","display_name":"Fable"}}}]}"#,
        ))
        .unwrap();
        assert_eq!(windows[1].id.0, "weekly_fable");
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
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
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
        });
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly_fable_5_1"]);
        assert_eq!(windows[1].scope, WindowScope::Model("fable_5_1".to_owned()));
        assert_eq!(windows[1].label, "Weekly — Fable 5.1");
        // The two rows this parser cannot decode at all — the bare string and
        // the one whose `scope` is a string — are the only silent losses left
        // if the warning goes: nothing about them reaches the ids above.
        assert_eq!(
            logs.matches("skipping unreadable limits[] row").count(),
            2,
            "{logs}"
        );
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

    /// The distinction an id's shape cannot make, and the daemon's warning
    /// about stranded history turns on: `weekly_opus` is model-scoped and
    /// `weekly_`-prefixed exactly like a derived id, and is a compiled-in
    /// constant that cannot rename underneath its stored series.
    #[test]
    fn only_ids_built_from_server_text_count_as_derived() {
        for id in ["session_5h", "weekly", "weekly_opus", "weekly_sonnet"] {
            assert!(
                !is_server_derived(&WindowId::from(id)),
                "{id} is compiled in"
            );
        }
        // What `parse` builds from `scope.model.id` or a display name.
        for id in [
            "weekly_fable",
            "weekly_claude_fable_5_1",
            "weekly_opus_mini",
        ] {
            assert!(is_server_derived(&WindowId::from(id)), "{id} is derived");
        }
    }

    /// Issue #10, exactly as reported: two labels for one model slug to one
    /// id, and the second row's reading — the larger one — is discarded by
    /// array order. Which row wins is unchanged; that it is now said out loud
    /// is the fix, since the number the server sent is otherwise absent from
    /// the dashboard with nothing to explain it.
    #[test]
    fn a_second_row_for_one_id_is_dropped_with_a_reason() {
        let mut windows = Vec::new();
        let logs = captured_logs(|| {
            windows = parse(&raw(
                200,
                r#"{"five_hour":{"utilization":5},
                    "limits":[
                        {"kind":"weekly_scoped","percent":10,
                         "scope":{"model":{"display_name":"Opus 4.1"}}},
                        {"kind":"weekly_scoped","percent":90,
                         "scope":{"model":{"display_name":"Opus-4-1"}}}
                    ]}"#,
            ))
            .unwrap();
        });
        let ids: Vec<_> = windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["session_5h", "weekly_opus_4_1"]);
        // Precedence is unchanged: the first row carrying a percent wins.
        assert_eq!(windows[1].used, 10.0);
        assert!(logs.contains("already reported"), "{logs}");
        // Named well enough to find the row: the discarded 90 and its label.
        assert!(logs.contains("Opus-4-1"), "{logs}");
        assert!(logs.contains("weekly_opus_4_1"), "{logs}");
    }

    /// Two consecutive polls differing only in whether `seven_day_opus`
    /// carries a reading. The Opus cap must never appear under a second id
    /// because of that: the id keys stored history, `rollover::detect` records
    /// nothing across an id change, and a set that flips each poll restarts the
    /// chart and the burn rate every time.
    ///
    /// The second poll reporting one window fewer is the intended trade — a
    /// reading the server did not send is a window absent for that poll, which
    /// the rest of the codebase already handles. A *different* id for the same
    /// cap is not.
    #[test]
    fn an_unreadable_fixed_bucket_still_suppresses_its_scoped_row() {
        let poll = |opus: &str| {
            format!(
                r#"{{"five_hour":{{"utilization":27}},
                     "seven_day_opus":{opus},
                     "limits":[{{"kind":"weekly_scoped","percent":30,
                                 "scope":{{"model":{{"display_name":"Claude Opus 4.5"}}}}}}]}}"#
            )
        };
        let ids = |body: &str| {
            parse(&raw(200, body))
                .unwrap()
                .iter()
                .map(|w| w.id.0.clone())
                .collect::<Vec<String>>()
        };

        let read = ids(&poll(r#"{"utilization":30}"#));
        assert_eq!(read, ["session_5h", "weekly_opus"]);

        // Identical payload but for a reading the parser cannot read.
        let unread = ids(&poll(r#"{"resets_at":null}"#));
        assert_eq!(unread, ["session_5h"]);
        assert!(
            !unread.iter().any(|id| id == "weekly_claude_opus_4_5"),
            "the Opus cap must not move to a second id: {unread:?}"
        );
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
