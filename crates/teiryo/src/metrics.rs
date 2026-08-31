//! Derived quota metrics, computed purely from a window's own fields.
//!
//! A `Rolling` window carries enough to reconstruct its start: `reset_at`
//! minus the roll duration. That is what turns a bare "62% used" into "62%
//! used but only 48% of the way through the window" — the burn-rate framing
//! the dashboard is built around. Nothing here needs the daemon, storage, or
//! provider internals.

use chrono::{DateTime, Duration, Utc};

use teiryo_core::domain::{QuotaSnapshot, QuotaWindow};
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

/// How much longer the remaining headroom lasts at `pace`, sustained.
///
/// Deliberately says nothing about the reset: a runway longer than the window
/// has left is a real answer to "how long could I keep this up", and the row
/// prints it alongside the countdown so the two can be compared. `Some(zero)`
/// when the cap is already reached; `None` at a pace of zero, which never
/// arrives.
///
/// Takes a pace rather than a clock because the caller chooses which rate to
/// project: the average since the window opened, or a rate measured over some
/// recent stretch of it.
pub fn runway_at(window: &QuotaWindow, pace: f64) -> Option<Duration> {
    let used = utilization(window)?;
    if used >= 1.0 {
        return Some(Duration::zero());
    }
    if pace <= 0.0 {
        return None;
    }
    let span_secs = window_span(window)?.num_seconds();
    if span_secs <= 0 {
        return None;
    }
    // A pace is a multiple of the rate that exactly spends the window over its
    // own span, so the span is what converts it back into a rate per second.
    let per_second = pace / span_secs as f64;
    let secs = ((1.0 - used) / per_second).round();
    if !secs.is_finite() || secs < 0.0 || secs > i64::MAX as f64 {
        return None;
    }
    Some(Duration::seconds(secs as i64))
}

/// How much longer the headroom lasts at the pace held since the window
/// opened.
pub fn runway(window: &QuotaWindow, now: DateTime<Utc>) -> Option<Duration> {
    runway_at(window, pace(window, now)?)
}

/// The pace that spends exactly the remaining headroom over exactly the time
/// left: `1.0` when usage and the clock are level, above `1.0` when there is
/// slack to burn, `0.0` at the cap.
///
/// The forward-looking counterpart to [`pace`], and what answers "how fast may
/// I go from here without running out early".
pub fn affordable_pace(window: &QuotaWindow, now: DateTime<Utc>) -> Option<f64> {
    let used = utilization(window)?;
    let remaining = 1.0 - elapsed_fraction(window, now)?;
    if remaining <= f64::EPSILON {
        return None;
    }
    Some((1.0 - used) / remaining)
}

/// Shortest stretch of series a rate is worth deriving from. Below this, one
/// poll's rounding is most of the signal.
const MIN_SAMPLE_SECS: i64 = 300;

/// How far back a "recent" rate looks: a tenth of the window, but never so
/// short that two polls dominate it, nor so long that recent stops meaning
/// anything. A 5-hour window looks back 30 minutes, a weekly one 12 hours.
fn recent_lookback(span: Duration) -> Duration {
    Duration::seconds((span.num_seconds() / 10).clamp(15 * 60, 12 * 3600))
}

/// Burn rate over the last stretch of the window, on the same scale as
/// [`pace`]: `1.0` is the rate the window can afford, `2.0` is twice that.
///
/// [`pace`] averages everything since the window opened, so a sprint after an
/// idle stretch barely moves it. This measures only the recent end of the
/// series, which is what says whether the sprint is happening now.
///
/// Never measured across a rollover, where `used` falls back to zero: the
/// series is floored at the window's own start, and cut again at any reading
/// whose `reset_at` is not this window's. The recorded rollover list is
/// deliberately not consulted — it omits the unannounced kind
/// (`rollover::RolloverKind::Unannounced`), which is exactly a large drop in
/// `used`, and reading across one would invent a burn that never happened.
///
/// `None` until two readings at least [`MIN_SAMPLE_SECS`] apart lie inside
/// both the lookback and the current window. A drop in `used` with no
/// rollover behind it — a provider correction — reads as `0.0`, not as a
/// negative rate.
pub fn recent_pace(
    window: &QuotaWindow,
    points: &[QuotaSnapshot],
    now: DateTime<Utc>,
) -> Option<f64> {
    let span = window_span(window)?;
    let floor = (now - recent_lookback(span)).max(window.started_at()?);

    let (mut first, mut last) = (None, None);
    for point in points
        .iter()
        .filter(|p| p.window == window.id)
        .filter(|p| p.ts >= floor && p.ts <= now)
    {
        if !is_same_window(point, window) {
            // Everything up to here belongs to a window that has since rolled
            // over. Start again on the far side of the boundary.
            first = None;
            continue;
        }
        first = first.or(Some(point));
        last = Some(point);
    }

    let (first, last) = (first?, last?);
    let elapsed = (last.ts - first.ts).num_seconds();
    if elapsed < MIN_SAMPLE_SECS {
        return None;
    }
    let burned = (last.utilization()? - first.utilization()?).max(0.0);
    Some(burned / elapsed as f64 * span.num_seconds() as f64)
}

/// Whether a reading was taken inside the window as it stands now, judged by
/// the reset instant it carried — the same signal `rollover::classify` uses,
/// and the only one a lone snapshot carries.
fn is_same_window(point: &QuotaSnapshot, window: &QuotaWindow) -> bool {
    match (point.reset_at, window.reset_at) {
        (Some(theirs), Some(ours)) => (theirs - ours).abs() <= RESET_TOLERANCE,
        _ => false,
    }
}

/// When the window is projected to hit its cap, extrapolating current usage
/// linearly.
///
/// The reset-aware reading of [`runway`]: `None` when the projected cap falls
/// *after* the reset, because the window rolls over first and there is no
/// exhaustion to warn about. That makes it the test for "is the runway the
/// binding limit", which is how the row decides whether to colour it.
pub fn eta_to_cap(window: &QuotaWindow, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let reset_at = window.reset_at?;
    let eta = now + runway(window, now)?;
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
    fn runway_spends_the_remaining_headroom_at_the_pace_given() {
        // Half a 10-hour window left to spend, at exactly the rate the window
        // affords: five hours of headroom.
        assert_eq!(runway_at(&window(50.0, 5), 1.0), Some(Duration::hours(5)),);
        // Twice that rate empties it in half the time.
        assert_eq!(
            runway_at(&window(50.0, 5), 2.0),
            Some(Duration::hours(2) + Duration::minutes(30)),
        );
        // Already capped: no headroom left to project.
        assert_eq!(runway_at(&window(100.0, 4), 2.0), Some(Duration::zero()));
        // A pace of zero never arrives at the cap.
        assert_eq!(runway_at(&window(50.0, 5), 0.0), None);
    }

    #[test]
    fn runway_is_reported_even_when_the_window_resets_first() {
        // 30% used with 6 of 10 hours gone burns the rest in another 14h —
        // long after the 4h reset, which is exactly the case `eta_to_cap`
        // suppresses. The rate is still a real answer.
        assert_eq!(runway(&window(30.0, 4), now()), Some(Duration::hours(14)));
        assert_eq!(eta_to_cap(&window(30.0, 4), now()), None);

        // Nothing used yet: no rate to project from, either way.
        assert_eq!(runway(&window(0.0, 4), now()), None);
    }

    #[test]
    fn affordable_pace_spreads_what_is_left_over_the_time_left() {
        // 50% used with half the window to go: usage and clock are level.
        assert_eq!(affordable_pace(&window(50.0, 5), now()), Some(1.0));
        // 30% used with half to go — the shape of a weekly window 84h from
        // reset — leaves 70% for 50% of the span: 1.4× the nominal rate.
        assert_eq!(affordable_pace(&window(30.0, 5), now()), Some(1.4));
        // At the cap there is nothing left to afford.
        assert_eq!(affordable_pace(&window(100.0, 5), now()), Some(0.0));
        // A window at its reset has no time left to spread anything over.
        assert_eq!(affordable_pace(&window(50.0, 0), now()), None);
    }

    /// A burn rate rounded to the two decimals the row prints. Rates are
    /// ratios of ratios, so the last bit of a `f64` is noise the UI never
    /// shows.
    fn as_shown(rate: Option<f64>) -> Option<f64> {
        rate.map(|r| (r * 100.0).round() / 100.0)
    }

    /// A reading of the 10-hour test window taken `minutes_ago`, `used`%
    /// consumed, belonging to the window that resets 4 hours from `now()`.
    fn point(minutes_ago: i64, used: f64) -> QuotaSnapshot {
        QuotaSnapshot {
            poll_id: PollId::generate(),
            ts: now() - Duration::minutes(minutes_ago),
            window: WindowId::from("w"),
            label: "w".to_owned(),
            unit: QuotaUnit::Percent,
            used,
            limit: Some(100.0),
            reset_at: Some(now() + Duration::hours(4)),
        }
    }

    #[test]
    fn recent_pace_measures_the_last_stretch_not_the_whole_window() {
        // 60% of a 10-hour window used with 6 hours gone is dead on track...
        let window = window(60.0, 4);
        assert_eq!(pace(&window, now()), Some(1.0));
        // ...but 10 points of it went in the last half hour, which is twice
        // the rate the window can afford.
        let series = [point(30, 50.0), point(0, 60.0)];
        assert_eq!(as_shown(recent_pace(&window, &series, now())), Some(2.0));
    }

    #[test]
    fn recent_pace_never_measures_across_a_rollover() {
        let window = window(60.0, 4);
        // A reading from the window that came before this one: same series,
        // different reset instant, and 90% used where this window has 60%.
        let mut previous = point(45, 90.0);
        previous.reset_at = Some(now() - Duration::hours(1));

        let series = [previous, point(30, 50.0), point(0, 60.0)];
        // Measured from the far side of the boundary only — reading across it
        // would report usage falling by 30 points.
        assert_eq!(as_shown(recent_pace(&window, &series, now())), Some(2.0));
    }

    #[test]
    fn recent_pace_needs_two_readings_far_enough_apart() {
        let window = window(60.0, 4);
        assert_eq!(recent_pace(&window, &[], now()), None);
        assert_eq!(recent_pace(&window, &[point(0, 60.0)], now()), None);
        // Two polls two minutes apart are mostly rounding.
        let series = [point(2, 59.0), point(0, 60.0)];
        assert_eq!(recent_pace(&window, &series, now()), None);
    }

    #[test]
    fn a_provider_correction_reads_as_idle_rather_than_as_negative_burn() {
        let window = window(60.0, 4);
        let series = [point(30, 62.0), point(0, 60.0)];
        assert_eq!(recent_pace(&window, &series, now()), Some(0.0));
    }

    #[test]
    fn a_window_that_just_opened_has_no_recent_stretch_to_measure() {
        // Nothing before the window's own start counts, and it started now.
        let window = window(0.0, 10);
        let series = [point(30, 50.0), point(0, 60.0)];
        assert_eq!(recent_pace(&window, &series, now()), None);
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
