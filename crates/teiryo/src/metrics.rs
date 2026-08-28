//! Derived quota metrics, computed purely from a window's own fields.
//!
//! A `Rolling` window carries enough to reconstruct its start: `reset_at`
//! minus the roll duration. That is what turns a bare "62% used" into "62%
//! used but only 48% of the way through the window" — the burn-rate framing
//! the dashboard is built around. Nothing here needs the daemon, storage, or
//! provider internals.

use chrono::{DateTime, Duration, Utc};

use teiryo_core::domain::QuotaWindow;
use teiryo_core::rollover::{WindowRollover, RESET_TOLERANCE};

/// Utilization ratio in `0.0..=1.0`, when computable from the window's data.
///
/// `Percent` windows are self-describing; anything else needs a published
/// limit, which not every provider gives (see `docs/providers.md`).
pub fn utilization(window: &QuotaWindow) -> Option<f64> {
    window.utilization()
}

/// How long the window rolls over.
pub fn window_span(window: &QuotaWindow) -> Option<Duration> {
    window.span()
}

/// How far through the current window we are, in `0.0..=1.0`.
///
/// `None` when the provider did not publish `reset_at`, since without it the
/// window's start is unknowable.
pub fn elapsed_fraction(window: &QuotaWindow, now: DateTime<Utc>) -> Option<f64> {
    let reset_at = window.reset_at?;
    let span = window_span(window)?;
    let span_secs = span.num_seconds();
    if span_secs <= 0 {
        return None;
    }
    let elapsed = (now - (reset_at - span)).num_seconds();
    Some((elapsed as f64 / span_secs as f64).clamp(0.0, 1.0))
}

/// Consumption relative to the clock: `1.0` is exactly on track, `2.0` means
/// burning twice as fast as the window can afford.
///
/// Under a linear model this is also the projected utilization at reset — a
/// pace of `1.3` says you would finish the window at 130%, i.e. hit the cap
/// early. The two are deliberately not separate functions.
pub fn pace(window: &QuotaWindow, now: DateTime<Utc>) -> Option<f64> {
    let elapsed = elapsed_fraction(window, now)?;
    if elapsed <= f64::EPSILON {
        return None;
    }
    Some(utilization(window)? / elapsed)
}

/// When the window is projected to hit its cap, extrapolating current usage
/// linearly.
///
/// `None` when nothing has been used yet, when the window is not far enough
/// along to extrapolate from, or when the projected cap falls *after* the
/// reset — in that last case the window rolls over first and there is no
/// exhaustion to warn about.
pub fn eta_to_cap(window: &QuotaWindow, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let reset_at = window.reset_at?;
    let span = window_span(window)?;
    let used = utilization(window)?;
    if used >= 1.0 {
        return Some(now);
    }
    if used <= 0.0 {
        return None;
    }
    let elapsed = (now - (reset_at - span)).num_seconds();
    if elapsed <= 0 {
        return None;
    }
    let per_second = used / elapsed as f64;
    let secs = ((1.0 - used) / per_second).round();
    if !secs.is_finite() || secs < 0.0 || secs > i64::MAX as f64 {
        return None;
    }
    let eta = now + Duration::seconds(secs as i64);
    (eta < reset_at).then_some(eta)
}

/// What a vertical rule on the trend chart marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    /// A window that expired on schedule and was replaced.
    Rollover,
    /// A window the provider reset without advertising it — early, or with
    /// `reset_at` pulled backwards.
    Surprise,
    /// Where the window currently in progress began.
    CurrentStart,
    /// When the window currently in progress is due to reset. Unlike the
    /// others this lies in the future, so a caller has to make room for it.
    UpcomingReset,
}

/// One vertical rule: an instant, and why it matters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Boundary {
    /// When the boundary falls.
    pub at: DateTime<Utc>,
    /// What it marks.
    pub kind: BoundaryKind,
}

/// Window boundaries to draw over `from..=to`, oldest first, with the upcoming
/// reset (which lies outside that interval) last when there is one.
///
/// Past boundaries come only from `rollovers` — observed, recorded events.
/// They are deliberately not extrapolated backwards from `reset_at`: a rolling
/// window is anchored to first use, so after an idle stretch the next one
/// starts later than the last ended, and a fixed lattice of predicted
/// boundaries would draw lines where nothing happened.
///
/// Empty when the window publishes no `reset_at` and nothing rolled over,
/// which is what makes this a no-op for a provider or credential that enforces
/// no such cap.
pub fn boundaries(
    window: &QuotaWindow,
    rollovers: &[WindowRollover],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Boundary> {
    let start = window.started_at();
    let mut out: Vec<Boundary> = rollovers
        .iter()
        .filter(|r| r.window == window.id)
        .filter(|r| r.kind.is_boundary())
        .filter(|r| r.observed_at >= from && r.observed_at <= to)
        // The rollover that began the current window is the same event as
        // `CurrentStart` below, told from the other side. Drawing both would
        // put two rules a poll interval apart on one boundary.
        .filter(|r| start.is_none_or(|s| (r.observed_at - s).abs() > RESET_TOLERANCE))
        .map(|r| Boundary {
            at: r.observed_at,
            kind: if r.kind.is_surprise() {
                BoundaryKind::Surprise
            } else {
                BoundaryKind::Rollover
            },
        })
        .collect();
    if let Some(start) = start.filter(|s| *s >= from && *s <= to) {
        out.push(Boundary {
            at: start,
            kind: BoundaryKind::CurrentStart,
        });
    }
    out.sort_by_key(|b| b.at);
    if let Some(reset_at) = window.reset_at {
        out.push(Boundary {
            at: reset_at,
            kind: BoundaryKind::UpcomingReset,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use teiryo_core::domain::{AccountId, PollId, QuotaUnit, ResetKind, WindowId, WindowScope};
    use teiryo_core::rollover::RolloverKind;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap()
    }

    /// A 10-hour rolling window `used`% consumed, with `remaining` hours left
    /// before it resets.
    fn window(used: f64, remaining_hours: i64) -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from("w"),
            label: "w".into(),
            scope: WindowScope::AccountWide,
            reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(10 * 3600)),
            unit: QuotaUnit::Percent,
            used,
            limit: Some(100.0),
            reset_at: Some(now() + Duration::hours(remaining_hours)),
        }
    }

    #[test]
    fn utilization_from_percent_and_limits() {
        let mut w = window(42.0, 5);
        assert_eq!(utilization(&w), Some(0.42));

        w.unit = QuotaUnit::Messages;
        w.used = 30.0;
        w.limit = Some(60.0);
        assert_eq!(utilization(&w), Some(0.5));

        w.limit = None;
        assert_eq!(utilization(&w), None);

        // Overuse clamps rather than overflowing a bar.
        w.unit = QuotaUnit::Percent;
        w.used = 150.0;
        assert_eq!(utilization(&w), Some(1.0));
    }

    #[test]
    fn elapsed_fraction_tracks_the_clock() {
        // 4 of 10 hours remain, so 60% of the window has elapsed.
        assert_eq!(elapsed_fraction(&window(0.0, 4), now()), Some(0.6));

        let mut no_reset = window(0.0, 4);
        no_reset.reset_at = None;
        assert_eq!(elapsed_fraction(&no_reset, now()), None);
    }

    #[test]
    fn pace_compares_usage_against_elapsed_time() {
        // 60% used with 60% elapsed is exactly on track.
        assert_eq!(pace(&window(60.0, 4), now()), Some(1.0));
        // 90% used with 60% elapsed is burning fast.
        assert_eq!(pace(&window(90.0, 4), now()), Some(1.5));
        // 30% used with 60% elapsed leaves headroom.
        assert_eq!(pace(&window(30.0, 4), now()), Some(0.5));
        // A window that just started cannot be extrapolated from.
        assert_eq!(pace(&window(0.0, 10), now()), None);
    }

    #[test]
    fn eta_to_cap_only_fires_when_the_cap_comes_first() {
        // 90% used, 6h elapsed → ~0.67h to burn the last 10%, well before the
        // 4h reset.
        let eta = eta_to_cap(&window(90.0, 4), now()).expect("cap projected");
        assert!(eta > now() && eta < now() + Duration::hours(4));

        // 30% used at the same point projects past the reset: no warning.
        assert_eq!(eta_to_cap(&window(30.0, 4), now()), None);
        // Nothing used yet: no rate to extrapolate.
        assert_eq!(eta_to_cap(&window(0.0, 4), now()), None);
        // Already capped.
        assert_eq!(eta_to_cap(&window(100.0, 4), now()), Some(now()));
    }

    #[test]
    fn metrics_are_none_without_a_reset_instant() {
        let mut w = window(50.0, 4);
        w.reset_at = None;
        assert_eq!(pace(&w, now()), None);
        assert_eq!(eta_to_cap(&w, now()), None);
    }

    /// A rollover of `kind` for the 10-hour test window, `hours_ago` back.
    fn rollover(kind: RolloverKind, hours_ago: i64) -> WindowRollover {
        WindowRollover {
            account: AccountId::from("a"),
            window: WindowId::from("w"),
            poll: PollId::generate(),
            observed_at: now() - Duration::hours(hours_ago),
            kind,
            prev_reset_at: None,
            new_reset_at: None,
            prev_used: 90.0,
            new_used: 1.0,
        }
    }

    /// The 24h interval the chart would be showing at `now`.
    fn day() -> (DateTime<Utc>, DateTime<Utc>) {
        (now() - Duration::hours(24), now())
    }

    #[test]
    fn observed_rollovers_become_rules_by_severity() {
        let (from, to) = day();
        // 4 of 10 hours remain, so the current window began 6 hours ago; keep
        // both rollovers well clear of that.
        let found = boundaries(
            &window(60.0, 4),
            &[
                rollover(RolloverKind::Scheduled, 20),
                rollover(RolloverKind::Early, 14),
            ],
            from,
            to,
        );
        let kinds: Vec<_> = found.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BoundaryKind::Rollover,
                BoundaryKind::Surprise,
                BoundaryKind::CurrentStart,
                BoundaryKind::UpcomingReset,
            ]
        );
        // Past boundaries are ordered oldest first.
        assert!(found[0].at < found[1].at && found[1].at < found[2].at);
    }

    #[test]
    fn an_unannounced_drop_marks_no_boundary() {
        let (from, to) = day();
        let found = boundaries(
            &window(60.0, 4),
            &[rollover(RolloverKind::Unannounced, 20)],
            from,
            to,
        );
        // Only the current window's own two edges.
        assert_eq!(
            found.iter().map(|b| b.kind).collect::<Vec<_>>(),
            vec![BoundaryKind::CurrentStart, BoundaryKind::UpcomingReset]
        );
    }

    #[test]
    fn the_rollover_that_began_this_window_is_not_drawn_twice() {
        let (from, to) = day();
        // The current window started 6h ago; a rollover observed one poll
        // later is that same event seen from the other side.
        let mut seen = rollover(RolloverKind::Scheduled, 6);
        seen.observed_at += Duration::seconds(60);
        let found = boundaries(&window(60.0, 4), &[seen], from, to);
        assert_eq!(
            found.iter().map(|b| b.kind).collect::<Vec<_>>(),
            vec![BoundaryKind::CurrentStart, BoundaryKind::UpcomingReset]
        );
    }

    #[test]
    fn boundaries_outside_the_interval_and_other_windows_are_dropped() {
        let (from, to) = day();
        let mut elsewhere = rollover(RolloverKind::Early, 14);
        elsewhere.window = WindowId::from("other");
        let found = boundaries(
            &window(60.0, 4),
            &[rollover(RolloverKind::Early, 30), elsewhere],
            from,
            to,
        );
        assert_eq!(
            found.iter().map(|b| b.kind).collect::<Vec<_>>(),
            vec![BoundaryKind::CurrentStart, BoundaryKind::UpcomingReset]
        );
    }

    #[test]
    fn a_window_start_outside_the_interval_is_not_drawn() {
        let (from, to) = day();
        // A 10-hour window resetting in 20 hours started 10 hours before the
        // left edge — off the chart, but the reset is still known.
        let found = boundaries(&window(10.0, 20), &[], from, to);
        assert_eq!(
            found.iter().map(|b| b.kind).collect::<Vec<_>>(),
            vec![BoundaryKind::UpcomingReset]
        );
    }

    /// The "does nothing without a cap" requirement: a provider that publishes
    /// no reset instant, and no history of ever having rolled over, must
    /// produce no rules at all.
    #[test]
    fn a_window_with_no_reset_instant_has_no_boundaries() {
        let (from, to) = day();
        let mut w = window(50.0, 4);
        w.reset_at = None;
        assert!(boundaries(&w, &[], from, to).is_empty());
    }
}
