//! Pure formatting helpers. No widgets, no state — everything here is a
//! value-in, string-out function so it can be tested directly.

use chrono::{DateTime, Utc};

use teiryo_core::domain::{PollOutcome, PollTrigger, QuotaUnit, QuotaWindow};
use teiryo_core::PollEvent;

/// Coarse, human-scaled duration: "3d 4h", "2h 05m", "2m 05s", "41s".
fn humanize(secs: i64) -> String {
    let (d, h, m, s) = (
        secs / 86_400,
        (secs / 3_600) % 24,
        (secs / 60) % 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Humanized countdown to a reset instant: "2h 05m", "3d 4h", "41s", "due".
pub fn format_countdown(reset_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (reset_at - now).num_seconds();
    if secs <= 0 {
        return "due".to_owned();
    }
    humanize(secs)
}

/// Humanized length of a duration: "3d 4h", "2h 05m", "41s", "0s".
///
/// The same ladder [`format_countdown`] uses, for a span that is not anchored
/// to an instant — how long something lasts rather than when it lands. Note it
/// is coarse above a day: 36 hours reads "1d 12h".
pub fn format_span(span: chrono::Duration) -> String {
    humanize(span.num_seconds().max(0))
}

/// Humanized time since an instant: "2h 05m ago", "41s ago", "just now".
///
/// Deliberately a separate function rather than calling [`format_countdown`]
/// with swapped arguments: that idiom rendered anything within the current
/// second as "due ago".
pub fn format_elapsed(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds();
    if secs < 1 {
        return "just now".to_owned();
    }
    format!("{} ago", humanize(secs))
}

/// A poll cadence in the shortest exact form: "30s", "5m", "1h", "1h 30m".
///
/// Exact rather than coarse, unlike [`format_countdown`]: this is a value the
/// user sets, so rounding "90s" to "1m" would show them something they did not
/// choose and could not reproduce.
pub fn format_interval(secs: u32) -> String {
    let (h, m, s) = (secs / 3_600, (secs / 60) % 60, secs % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

/// Fixed-width bar: `ratio` filled with `█` and an eighth-block glyph for the
/// partial cell, the rest `░`.
///
/// Sub-cell resolution matters at dashboard widths, where one cell is several
/// percent of a quota: a whole-cell bar visibly quantizes, and a barely-used
/// window reads as completely empty rather than showing a sliver.
pub fn text_bar_fine(ratio: f64, width: usize) -> String {
    const EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (ratio.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = (eighths / 8).min(width);
    let remainder = eighths % 8;
    let mut bar = String::with_capacity(width * 3);
    for _ in 0..full {
        bar.push('█');
    }
    let mut drawn = full;
    if full < width && remainder > 0 {
        bar.push(EIGHTHS[remainder - 1]);
        drawn += 1;
    }
    for _ in drawn..width {
        bar.push('░');
    }
    bar
}

/// Usage text for a window, e.g. "42% used", "37/80 messages".
pub fn usage_text(window: &QuotaWindow) -> String {
    let unit = match window.unit {
        QuotaUnit::Percent => "%",
        QuotaUnit::Messages => " messages",
        QuotaUnit::Tokens => " tokens",
        QuotaUnit::Hours => " hours",
    };
    match (window.unit, window.limit) {
        (QuotaUnit::Percent, _) => format!("{:.0}% used", window.used),
        (_, Some(limit)) => format!("{:.0}/{:.0}{unit}", window.used, limit),
        (_, None) => format!("{:.0}{unit} used", window.used),
    }
}

/// Compact usage for a window, e.g. "62%" or "37/80".
pub fn usage_short(window: &QuotaWindow) -> String {
    match (window.unit, window.limit) {
        (QuotaUnit::Percent, _) => format!("{:.0}%", window.used),
        (_, Some(limit)) => format!("{:.0}/{:.0}", window.used, limit),
        (_, None) => format!("{:.0}", window.used),
    }
}

/// How a poll was triggered, as a single glyph.
pub fn trigger_glyph(trigger: &PollTrigger) -> &'static str {
    match trigger {
        PollTrigger::Scheduled => "⏱",
        PollTrigger::Manual { .. } => "▶",
        PollTrigger::Startup => "⏻",
    }
}

/// One-word outcome for a poll event, plus whether it was a failure.
pub fn outcome_text(event: &PollEvent) -> (String, bool) {
    match &event.outcome {
        PollOutcome::Success { .. } => ("ok".to_owned(), false),
        other => (other.error_message().unwrap_or("error").to_owned(), true),
    }
}

/// Truncate to `width` display cells, marking elision with an ellipsis.
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use teiryo_core::domain::{ResetKind, WindowId, WindowScope};

    fn window(unit: QuotaUnit, used: f64, limit: Option<f64>) -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from("w"),
            label: "w".into(),
            scope: WindowScope::AccountWide,
            reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(3600)),
            unit,
            used,
            limit,
            reset_at: None,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap()
    }

    #[test]
    fn format_span_measures_a_length_not_an_instant() {
        assert_eq!(format_span(Duration::seconds(41)), "41s");
        assert_eq!(format_span(Duration::minutes(90)), "1h 30m");
        // Coarse above a day: 36 hours is a day and a half, not "36h".
        assert_eq!(format_span(Duration::hours(36)), "1d 12h");
        // A span that has already run out is zero, never negative.
        assert_eq!(format_span(Duration::seconds(-5)), "0s");
    }

    #[test]
    fn countdown_formats() {
        let at = |secs: i64| now() + chrono::Duration::seconds(secs);
        assert_eq!(format_countdown(at(30), now()), "30s");
        assert_eq!(format_countdown(at(125), now()), "2m 05s");
        assert_eq!(format_countdown(at(2 * 3600 + 300), now()), "2h 05m");
        assert_eq!(format_countdown(at(3 * 86_400 + 4 * 3600), now()), "3d 4h");
        assert_eq!(format_countdown(at(-5), now()), "due");
    }

    /// Exact, unlike the countdown: this is a value the user chose, so
    /// rounding it would show them something they cannot reproduce.
    #[test]
    fn interval_formats_exactly() {
        assert_eq!(format_interval(0), "0s");
        assert_eq!(format_interval(30), "30s");
        assert_eq!(format_interval(60), "1m");
        assert_eq!(format_interval(90), "1m 30s");
        assert_eq!(format_interval(3_600), "1h");
        assert_eq!(format_interval(5_445), "1h 30m 45s");
    }

    #[test]
    fn elapsed_formats_without_the_due_ago_bug() {
        let ago = |secs: i64| now() - chrono::Duration::seconds(secs);
        // A poll that landed within the current second is "just now", not the
        // nonsensical "due ago" the swapped-argument idiom produced.
        assert_eq!(format_elapsed(ago(0), now()), "just now");
        assert_eq!(format_elapsed(now(), now()), "just now");
        assert_eq!(format_elapsed(ago(41), now()), "41s ago");
        assert_eq!(format_elapsed(ago(2 * 3600 + 300), now()), "2h 05m ago");
        // A clock skew that puts the event in the future must not panic or
        // render a negative duration.
        assert_eq!(
            format_elapsed(now() + chrono::Duration::seconds(5), now()),
            "just now"
        );
    }

    #[test]
    fn bar_fills_proportionally_and_clamps() {
        assert_eq!(text_bar_fine(0.0, 4), "░░░░");
        assert_eq!(text_bar_fine(0.5, 4), "██░░");
        assert_eq!(text_bar_fine(1.0, 4), "████");
        assert_eq!(text_bar_fine(2.0, 4), "████"); // clamped
    }

    #[test]
    fn fine_bar_keeps_width_and_shows_slivers() {
        for ratio in [0.0, 0.01, 0.33, 0.5, 0.99, 1.0, 2.0] {
            assert_eq!(
                text_bar_fine(ratio, 10).chars().count(),
                10,
                "width drifted at {ratio}"
            );
        }
        assert_eq!(text_bar_fine(0.0, 4), "░░░░");
        assert_eq!(text_bar_fine(0.5, 4), "██░░");
        assert_eq!(text_bar_fine(1.0, 4), "████");
        // A barely-used window renders a partial cell instead of nothing.
        assert_ne!(text_bar_fine(0.03, 4), "░░░░");
    }

    #[test]
    fn usage_text_by_unit() {
        assert_eq!(
            usage_text(&window(QuotaUnit::Percent, 42.4, None)),
            "42% used"
        );
        assert_eq!(
            usage_text(&window(QuotaUnit::Messages, 30.0, Some(60.0))),
            "30/60 messages"
        );
        assert_eq!(
            usage_text(&window(QuotaUnit::Hours, 3.0, None)),
            "3 hours used"
        );
        assert_eq!(usage_short(&window(QuotaUnit::Percent, 62.0, None)), "62%");
        assert_eq!(
            usage_short(&window(QuotaUnit::Messages, 30.0, Some(60.0))),
            "30/60"
        );
    }

    #[test]
    fn truncate_marks_elision() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("truncate me", 5), "trun…");
        assert_eq!(truncate("abc", 1), "…");
    }
}
