//! Pure formatting helpers. No widgets, no state — everything here is a
//! value-in, string-out function so it can be tested directly.

use chrono::{DateTime, Utc};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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

/// Width of `text` in terminal cells.
///
/// Not `chars().count()`. A cell is what the terminal draws, and the two part
/// company exactly where provider-supplied text lives: CJK and most emoji
/// occupy two cells, combining marks none. ratatui charges every span its real
/// width against one shared row budget, so a label measured in chars pushes
/// each column after it to the right and clips the last one — with the data
/// still correct, which is what makes it hard to see.
///
/// Measured per *grapheme*, which is what ratatui does. Summing per `char`
/// gets emoji wrong in the widening direction: `U+FE0F`, the variation
/// selector that makes `❤` render as an emoji, is zero cells on its own and
/// makes the pair two — so a char-wise sum reads 1 where the terminal draws 2,
/// and a column of them overruns by its own length again.
pub fn cells(text: &str) -> usize {
    drawn(text).map(|(_, w)| w).sum()
}

/// The graphemes of `text` with the width each will actually be drawn at.
///
/// A grapheme carrying a control character is charged nothing, because ratatui
/// discards it rather than drawing it. Charging it a cell would shed a field
/// that fits.
fn drawn(text: &str) -> impl Iterator<Item = (&str, usize)> {
    text.graphemes(true).map(|g| {
        if g.chars().any(char::is_control) {
            (g, 0)
        } else {
            (g, UnicodeWidthStr::width(g))
        }
    })
}

/// How many `char`s per cell of budget `truncate` will carry.
///
/// A cell budget bounds how wide the result draws but not how long it is: a
/// grapheme cluster is a base character plus any number of combining marks,
/// so a single one-cell cluster can be megabytes. The longest cluster real
/// text produces is a four-person family emoji with per-person skin tones —
/// eleven `char`s across at least two cells — and Indic clusters run to about
/// half that rate. Sixteen `char`s per cell is several times that worst case
/// at every width, so no input a provider could plausibly send is altered;
/// only text that is padding rather than glyphs reaches the cap.
const MAX_CHARS_PER_CELL: usize = 16;

/// Truncate to `width` display cells, marking elision with an ellipsis.
///
/// The result is never wider than `width`, including when a two-cell glyph
/// straddles the boundary: that glyph is dropped rather than half-drawn, which
/// can leave the result one cell short of the budget. Short is safe; over is
/// the bug. A `width` of zero yields the empty string, since even the ellipsis
/// costs a cell.
///
/// Nor is it longer than [`MAX_CHARS_PER_CELL`] per cell of `width`, plus the
/// ellipsis. `window.label` is server-supplied and the result is cloned into a
/// `Span` on every frame, so a cell budget alone leaves an unbounded string
/// being copied sixty times a second to draw one cell.
pub fn truncate(text: &str, width: usize) -> String {
    // `width.max(1)` so that a zero width still admits the ellipsis path
    // below rather than a cap of zero deciding the answer.
    let char_cap = MAX_CHARS_PER_CELL.saturating_mul(width.max(1));
    // Length first, and via `take`, so that a pathological string is never
    // measured end to end: the loop below stops at the cap too.
    if text.chars().take(char_cap + 1).count() <= char_cap && cells(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    // One cell reserved for the ellipsis, which is itself one cell wide.
    let budget = width - 1;
    let mut out = String::new();
    let mut used = 0;
    let mut chars = 0;
    for (grapheme, w) in drawn(text) {
        let len = grapheme.chars().count();
        if used + w > budget || chars + len > char_cap {
            break;
        }
        out.push_str(grapheme);
        used += w;
        chars += len;
    }
    out.push('…');
    out
}

/// Pad `text` on the right to `width` display cells.
///
/// The counterpart to [`truncate`], and needed for the same reason:
/// `format!("{:<width$}")` pads by `char` count, so a label holding any
/// wide glyph is padded too little and the row's later columns shift.
pub fn pad_to_cells(text: &str, width: usize) -> String {
    let mut out = text.to_owned();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(cells(text))));
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

    /// The budget is cells, which is what `truncate`'s own doc always said and
    /// what ratatui actually charges. Counting `char`s let a label of wide
    /// glyphs pass a width check at up to twice its real size.
    #[test]
    fn truncate_budgets_display_cells_not_chars() {
        // Eight chars, sixteen cells. Under the old rule this passed a width
        // check of 10 and then drew 16 columns.
        let wide = "東京東京東京東京";
        assert_eq!(cells(wide), 16);
        assert!(cells(&truncate(wide, 10)) <= 10);
        // Nothing is dropped when it already fits.
        assert_eq!(truncate(wide, 16), wide);
        // A two-cell glyph straddling the boundary is dropped whole rather
        // than half-drawn, so the result may come up a cell short.
        assert_eq!(truncate(wide, 6), "東京…");
        assert_eq!(cells(&truncate(wide, 6)), 5);
        // ASCII is unaffected, which is what keeps every existing budget sound.
        assert_eq!(cells("truncate me"), 11);
    }

    /// The other class of glyph a per-char sum gets wrong, and in the
    /// dangerous direction.
    ///
    /// `U+FE0F` is the variation selector that renders `❤` as an emoji. On its
    /// own it is zero cells, so a char-wise sum reads the pair as 1 — while
    /// ratatui measures the grapheme and draws 2. A column of them therefore
    /// overran by its own length again, which is #14's symptom reached through
    /// a different input.
    #[test]
    fn truncate_charges_an_emoji_sequence_what_the_terminal_draws() {
        let heart = "\u{2764}\u{FE0F}";
        assert_eq!(cells(heart), 2, "one grapheme, two cells");
        assert_eq!(cells(&heart.repeat(5)), 10);
        assert!(cells(&truncate(&heart.repeat(5), 6)) <= 6);
        // The keycap sequence is the same shape: three chars, two cells.
        assert_eq!(cells("1\u{FE0F}\u{20E3}"), 2);
        // And the label column holds, which a per-char sum did not.
        let column = 24;
        let drawn = pad_to_cells(&truncate(&heart.repeat(13), column - 1), column);
        assert_eq!(cells(&drawn), column);
    }

    /// A control character is discarded by ratatui rather than drawn, so it
    /// costs no cells. Charging it one sheds a field that would have fit.
    #[test]
    fn a_control_character_costs_no_cells() {
        assert_eq!(cells("\u{7}"), 0);
        assert_eq!(cells("ab\u{7}c"), 3);
        // Consistent with the budget check, which used to disagree with the
        // loop: `cells` said 10 while the loop charged 0, so the early return
        // was skipped and every control character was emitted anyway.
        assert_eq!(truncate(&"\u{7}".repeat(10), 6), "\u{7}".repeat(10));
    }

    #[test]
    fn truncate_to_no_width_yields_nothing() {
        // Even the ellipsis costs a cell, so there is nothing that fits.
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("", 0), "");
    }

    /// A cell budget bounds how wide the result draws, not how long it is.
    ///
    /// One grapheme cluster carries any number of combining marks and still
    /// occupies one cell, so `cells(text) <= width` used to hand back the
    /// input whole however large it was — and `window.label` is server-
    /// supplied, and the result is cloned into a `Span` every frame.
    #[test]
    fn truncate_bounds_the_length_of_what_it_returns() {
        const WIDTH: usize = 23;
        let cap = WIDTH * MAX_CHARS_PER_CELL;

        // One grapheme, one cell, a hundred thousand chars.
        let heavy = "a".to_owned() + &"\u{301}".repeat(100_000);
        assert_eq!(heavy.graphemes(true).count(), 1);
        assert_eq!(cells(&heavy), 1);
        let out = truncate(&heavy, WIDTH);
        assert!(cells(&out) <= WIDTH, "{} cells", cells(&out));
        assert!(
            out.chars().count() <= cap + 1,
            "returned {} chars for a {WIDTH}-cell column",
            out.chars().count()
        );

        // The other way past the cell budget: graphemes that cost nothing, so
        // the budget never fires however many of them arrive.
        let controls = "\u{7}".repeat(100_000);
        assert_eq!(cells(&controls), 0);
        let out = truncate(&controls, WIDTH);
        assert!(
            out.chars().count() <= cap + 1,
            "returned {} chars for a {WIDTH}-cell column",
            out.chars().count()
        );

        // Anything already inside both bounds is returned exactly as before.
        let ordinary = "Weekly — Opus 東京";
        assert_eq!(truncate(ordinary, WIDTH), ordinary);
        assert_eq!(truncate(&"x".repeat(cap), cap), "x".repeat(cap));
    }

    /// #14's reproduction, at the width the dashboard actually uses: a
    /// server-supplied model name reaching `QuotaWindow.label` for the first
    /// time. `truncate(_, 23)` passed the 22-char string through untouched and
    /// `{:<24}` padded it to 24 chars — 32 cells against a 24-cell column, so
    /// the bar, usage and reset spans all shifted right and the last was
    /// clipped.
    #[test]
    fn a_label_of_wide_glyphs_stays_inside_its_column() {
        let label = "Weekly — Opus 東京東京東京東京";
        let column = 24;
        let drawn = pad_to_cells(&truncate(label, column - 1), column);
        assert_eq!(cells(&drawn), column, "the column is exactly its budget");
    }

    #[test]
    fn padding_counts_cells_and_never_shrinks_what_it_is_given() {
        assert_eq!(pad_to_cells("ab", 5), "ab   ");
        assert_eq!(cells(&pad_to_cells("東京", 6)), 6);
        // Already at or over budget: left alone rather than truncated, which
        // is `truncate`'s job and not this one's.
        assert_eq!(pad_to_cells("abcdef", 3), "abcdef");
    }
}
