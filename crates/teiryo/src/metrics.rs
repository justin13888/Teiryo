//! Derived quota metrics, computed against the window that actually ran.
//!
//! A `Rolling` window's start is normally `reset_at` minus the roll duration,
//! and that is what turns a bare "62% used" into "62% used but only 48% of the
//! way through the window" — the burn-rate framing the dashboard is built
//! around.
//!
//! It is only true while the provider moves `reset_at` with the reset. Where
//! it does not — a weekly quota that restarts early and keeps publishing the
//! old instant — that subtraction names a window which is over, and every
//! number built on it reads low: it divides real usage by a stretch of clock
//! most of which belonged to a window that has already been paid for. Low is
//! the dangerous direction. So the daemon publishes any restart it actually
//! observed, and [`effective_window`] is where the two are reconciled; nothing
//! downstream of it needs to know which of the two answers it got.
//!
//! Nothing here needs the daemon, storage, or provider internals.

use chrono::{DateTime, Duration, Utc};

use teiryo_core::domain::{QuotaSnapshot, QuotaWindow, WindowId};
use teiryo_core::rollover::{WindowRollover, RESET_TOLERANCE};
use teiryo_core::WindowView;

/// Utilization ratio in `0.0..=1.0`, when computable from the window's data.
///
/// `Percent` windows are self-describing; anything else needs a published
/// limit, which not every provider gives (see `docs/providers.md`).
pub fn utilization(window: &QuotaWindow) -> Option<f64> {
    window.utilization()
}

/// The window a rate should be measured against: where it actually began,
/// where it ends, and how sure we are of the first of those.
///
/// Distinct from [`QuotaWindow`] because a window that restarted early is
/// genuinely shorter than its nominal span *and* carries a full budget. Both
/// halves matter: the same usage is a faster burn, and there is more left to
/// afford than the clock alone would suggest.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveWindow {
    /// Which window this describes, so a history series can be filtered to it.
    pub window: WindowId,
    /// When it began.
    pub start: DateTime<Utc>,
    /// When it ends.
    pub reset_at: DateTime<Utc>,
    /// How wide the bracket around `start` was. Zero when the start is the
    /// provider's own arithmetic, and as wide as a daemon outage when the
    /// restart was seen across one.
    pub start_uncertainty: Duration,
    /// Utilization at the latest reading, when computable.
    pub used: Option<f64>,
}

impl EffectiveWindow {
    /// How long the window that actually ran is — what a pace is a multiple of.
    pub fn span(&self) -> Duration {
        self.reset_at - self.start
    }
}

/// Reconcile what the provider publishes with what the daemon observed.
///
/// The observed restart wins only where it says the window began *later* than
/// the provider's arithmetic does, which is the one direction a missed reset
/// can push it: an earlier restart belongs to a window that has since ended,
/// and there is no such thing as a window that began before its own
/// `reset_at - span` and is still running.
///
/// `None` when the provider publishes no `reset_at`, since then neither party
/// knows where the window sits.
pub fn effective_window(view: &WindowView, now: DateTime<Utc>) -> Option<EffectiveWindow> {
    let window = &view.window;
    let reset_at = window.reset_at?;
    let nominal = reset_at - window.span()?;
    let observed = view
        .observed_start
        .filter(|o| o.estimate() > nominal && o.estimate() < reset_at && o.not_after <= now);
    Some(EffectiveWindow {
        window: window.id.clone(),
        start: observed.map_or(nominal, |o| o.estimate()),
        reset_at,
        start_uncertainty: observed.map_or_else(Duration::zero, |o| o.uncertainty()),
        used: window.utilization(),
    })
}

/// How far through the window we are, in `0.0..=1.0`.
pub fn elapsed_fraction(window: &EffectiveWindow, now: DateTime<Utc>) -> Option<f64> {
    let span = window.span().num_seconds();
    if span <= 0 {
        return None;
    }
    let elapsed = (now - window.start).num_seconds();
    Some((elapsed as f64 / span as f64).clamp(0.0, 1.0))
}

/// How much of a window's length its start bracket may span before the
/// numbers measured from that start are marked as estimates.
///
/// A fraction rather than a duration, for the reason [`MIN_ELAPSED_FRACTION`]
/// is one: half an hour is most of a 5-hour window's precision budget and
/// nothing at all against a weekly one. At a twentieth, an ordinary poll
/// cadence never trips it and a bracket left by an outage always does.
const MAX_TRUSTED_BRACKET_FRACTION: f64 = 0.05;

/// Whether the window's start is known loosely enough that the numbers
/// measured from it should say so.
///
/// `start_uncertainty` is the width of the bracket a silent restart was
/// observed in — one poll interval in normal running, and days across a
/// daemon outage. A pace resting on each is the same figure with very
/// different standing, and a caller that prints them identically is reporting
/// a guess as a measurement.
///
/// Only the fields measured *from the start* are affected. `afford` divides
/// the remaining budget by the time left to `reset_at` and never touches it,
/// which is why it stays plain while `pace` beside it is marked.
pub fn start_is_uncertain(window: &EffectiveWindow) -> bool {
    let span = window.span().num_seconds();
    span > 0
        && window.start_uncertainty.num_seconds() as f64 / span as f64
            > MAX_TRUSTED_BRACKET_FRACTION
}

/// Consumption relative to the clock: `1.0` is exactly on track, `2.0` means
/// burning twice as fast as the window can afford.
///
/// Under a linear model this is also the projected utilization at reset — a
/// pace of `1.3` says you would finish the window at 130%, i.e. hit the cap
/// early. The two are deliberately not separate functions.
///
/// [`MIN_ELAPSED_FRACTION`] is the only floor, deliberately. An absolute one
/// beside it would blank the number past the point `docs/dashboard.md`
/// promises it returns, and it would bite exactly where this module's own
/// feature makes it reachable: an effective span is `reset_at` less an
/// *observed* start, so a restart seen an hour before the reset leaves a
/// window whose twentieth is three minutes. Flooring the fraction is what
/// bounds the result, at `1 / MIN_ELAPSED_FRACTION`.
pub fn pace(window: &EffectiveWindow, now: DateTime<Utc>) -> Option<f64> {
    let elapsed = elapsed_fraction(window, now)?;
    if elapsed < MIN_ELAPSED_FRACTION {
        return None;
    }
    Some(window.used? / elapsed)
}

/// How much of a window must have run before an average over it is worth
/// printing.
///
/// Not a division guard — [`elapsed_fraction`] is already clamped and the
/// nearly-zero case is the whole problem. An average over the first moments
/// of a window divides whatever has been used by a nearly-zero fraction, so
/// the row reports a number in the tens and the fields derived from it — the
/// runway, the `cap in` countdown and its colour — extrapolate a week from
/// five minutes. A floor in *absolute* time cannot bound that, because the
/// same five minutes is a smaller fraction of a longer window: what makes the
/// number large is the ratio, so the ratio is what has to be floored.
///
/// At a twentieth of the window this bounds pace at `20×`, and costs the
/// field for the first 15 minutes of a 5-hour window or the first 8 hours of a
/// weekly one. That is the stretch where the average says least and
/// [`recent_pace`] says most, and the two are separate fields, so the row
/// still reports a burst that starts right after a reset.
const MIN_ELAPSED_FRACTION: f64 = 0.05;

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
pub fn runway_at(window: &EffectiveWindow, pace: f64) -> Option<Duration> {
    let used = window.used?;
    if used >= 1.0 {
        return Some(Duration::zero());
    }
    if pace <= 0.0 {
        return None;
    }
    let span_secs = window.span().num_seconds();
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
pub fn runway(window: &EffectiveWindow, now: DateTime<Utc>) -> Option<Duration> {
    runway_at(window, pace(window, now)?)
}

/// The pace that spends exactly the remaining headroom over exactly the time
/// left: `1.0` when usage and the clock are level, above `1.0` when there is
/// slack to burn, `0.0` at the cap.
///
/// The forward-looking counterpart to [`pace`], and what answers "how fast may
/// I go from here without running out early".
pub fn affordable_pace(window: &EffectiveWindow, now: DateTime<Utc>) -> Option<f64> {
    let used = window.used?;
    let remaining = 1.0 - elapsed_fraction(window, now)?;
    if remaining <= f64::EPSILON {
        return None;
    }
    Some((1.0 - used) / remaining)
}

/// Shortest stretch of series a rate is worth deriving from. Below this, one
/// poll's rounding is most of the signal.
const MIN_SAMPLE_SECS: i64 = 300;

/// How far usage may slip before the fall counts as a break in the series.
///
/// Half of the last digit the row prints. A revision this small cannot change
/// what the user sees, but cutting the stretch at it can: the field vanishes,
/// or survives at a rate re-scaled over the few readings left after the slip.
/// Far below [`teiryo_core::rollover::MIN_RESET_DROP`], so a restart too small
/// for the detector to record still ends the stretch, which is the case this
/// cut exists for.
const FALL_EPSILON: f64 = 0.005;

/// How long a hole in the series may be before the two sides of it stop being
/// one measurement: four missed polls, and never under ten minutes so a fast
/// cadence does not make the field flicker on one dropped request.
///
/// Derived from the account's own cadence rather than fixed, so a user polling
/// every ten minutes is not permanently "stale" while one polling every thirty
/// seconds gets a rate averaged over an outage.
pub fn gap_tolerance(poll_interval_secs: u32) -> Duration {
    (Duration::seconds(i64::from(poll_interval_secs)) * 4).max(Duration::minutes(10))
}

/// How far back a "recent" rate looks: a tenth of the window, but never so
/// short that two polls dominate it, nor so long that recent stops meaning
/// anything. A 5-hour window looks back 30 minutes, a weekly one 12 hours.
fn recent_lookback(span: Duration) -> Duration {
    Duration::seconds((span.num_seconds() / 10).clamp(15 * 60, 12 * 3600))
}

/// Burn rate over the newest unbroken stretch of the series, on the same scale
/// as [`pace`]: `1.0` is the rate the window can afford, `2.0` is twice that.
///
/// [`pace`] averages everything since the window opened, so a sprint after an
/// idle stretch barely moves it. This measures only the recent end of the
/// series, which is what says whether the sprint is happening now.
///
/// What counts as "the recent end" is the question the whole function is
/// about, because a series is not a continuous record. The daemon stops, and
/// resumes hours later; the provider restarts the window without saying so, or
/// revises a reading downwards. None of those is a measurement, and averaging
/// across one invents a rate that was never burnt — so the stretch is walked
/// backwards from the newest reading and cut at the first step that crosses
/// one.
///
/// The cut on a fall in `used` is deliberately almost *any* fall rather than
/// one large enough to be a reset. Inside a single window instance usage only
/// climbs, so a fall is always either a restart or a revision, and neither
/// leaves the readings on its two sides comparable. Judging how big it was
/// would only reintroduce the question at a smaller scale: a restart from
/// under [`teiryo_core::rollover::MIN_RESET_DROP`] is invisible to the
/// detector, and subtracting straight across one would report `0.00× now`
/// over exactly the stretch a fresh window was being burnt through. The one
/// exception is [`FALL_EPSILON`], which exists so the field does not vanish
/// over a slip too small to print.
///
/// `None` when the newest reading is itself older than `max_gap`: a series
/// that stopped has no current rate, and printing the last one it had under a
/// `now` label is worse than printing nothing.
pub fn recent_pace(
    window: &EffectiveWindow,
    points: &[QuotaSnapshot],
    max_gap: Duration,
    now: DateTime<Utc>,
) -> Option<f64> {
    let span = window.span();
    if span.num_seconds() <= 0 {
        return None;
    }
    let floor = (now - recent_lookback(span)).max(window.start);

    // Sorted and deduplicated, because nothing in the protocol promises an
    // ordering and a repeated poll is not a second measurement.
    let mut run: Vec<(DateTime<Utc>, f64)> = points
        .iter()
        .filter(|p| p.window == window.window)
        .filter(|p| p.ts >= floor && p.ts <= now)
        .filter(|p| in_same_window(p, window))
        .filter_map(|p| Some((p.ts, p.utilization()?)))
        .collect();
    run.sort_by_key(|(ts, _)| *ts);
    run.dedup_by_key(|(ts, _)| *ts);

    let (newest_ts, newest_used) = *run.last()?;
    if now - newest_ts > max_gap {
        return None;
    }
    // Walk back from the newest while each step is one the series can be read
    // across: no hole longer than the tolerance, and no fall in usage.
    let mut first = run.len() - 1;
    while first > 0 {
        let (ts, used) = run[first];
        let (prev_ts, prev_used) = run[first - 1];
        if ts - prev_ts > max_gap || used < prev_used - FALL_EPSILON {
            break;
        }
        first -= 1;
    }

    let (first_ts, first_used) = run[first];
    let elapsed = (newest_ts - first_ts).num_seconds();
    if elapsed < MIN_SAMPLE_SECS {
        return None;
    }
    // The walk stopped at the last real fall, so the only way the stretch ends
    // below where it started is a slip inside the epsilon — a stretch that was
    // flat, which `0.00×` is the honest reading of.
    let burnt = (newest_used - first_used).max(0.0);
    Some(burnt / elapsed as f64 * span.num_seconds() as f64)
}

/// Whether a reading was taken inside the window as it stands now, judged by
/// the reset instant it carried — the only signal a lone snapshot has.
///
/// Note what this cannot see: an unannounced restart leaves `reset_at` exactly
/// where it was, so both sides of one pass this test. Catching those is what
/// [`recent_pace`]'s cut on a falling reading is for; the two are
/// complementary, not redundant.
fn in_same_window(point: &QuotaSnapshot, window: &EffectiveWindow) -> bool {
    point
        .reset_at
        .is_some_and(|theirs| (theirs - window.reset_at).abs() <= RESET_TOLERANCE)
}

/// When the window is projected to hit its cap, extrapolating current usage
/// linearly.
///
/// The reset-aware reading of [`runway`]: `None` when the projected cap falls
/// *after* the reset, because the window rolls over first and there is no
/// exhaustion to warn about. That makes it the test for "is the runway the
/// binding limit", which is how the row decides whether to colour it.
pub fn eta_to_cap(window: &EffectiveWindow, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let eta = now + runway(window, now)?;
    (eta < window.reset_at).then_some(eta)
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
/// `effective` of `None` — a provider that publishes no `reset_at` — leaves
/// only whatever rollovers were recorded, which is what makes this a no-op for
/// a provider or credential that enforces no such cap.
pub fn boundaries(
    window: &WindowId,
    effective: Option<&EffectiveWindow>,
    rollovers: &[WindowRollover],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Boundary> {
    let start = effective.map(|e| e.start);
    let mut out: Vec<Boundary> = rollovers
        .iter()
        .filter(|r| &r.window == window)
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
    if let Some(effective) = effective {
        out.push(Boundary {
            at: effective.reset_at,
            kind: BoundaryKind::UpcomingReset,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use teiryo_core::domain::{AccountId, PollId, QuotaUnit, ResetKind, WindowScope};
    use teiryo_core::rollover::{ObservedStart, RolloverKind};
    use teiryo_core::{BarStyle, RenderHint};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap()
    }

    /// The 10-hour test window as it reaches the TUI, with whatever the daemon
    /// has observed about where it began.
    fn view(window: QuotaWindow, observed_start: Option<ObservedStart>) -> WindowView {
        WindowView {
            window,
            hint: RenderHint {
                style: BarStyle::Percent,
                warn_threshold: 0.8,
                critical_threshold: 0.95,
                note: None,
            },
            observed_start,
        }
    }

    /// The effective window for a provider that moved `reset_at` with the
    /// reset — the uncomplicated case, where it is `reset_at` less the span.
    fn effective(used: f64, remaining_hours: i64) -> EffectiveWindow {
        effective_window(&view(window(used, remaining_hours), None), now())
            .expect("a published reset instant")
    }

    /// The gap tolerance for an account polled every ten minutes, which is the
    /// cadence the readings below are spaced at.
    fn max_gap() -> Duration {
        gap_tolerance(600)
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
        assert_eq!(elapsed_fraction(&effective(0.0, 4), now()), Some(0.6));
    }

    #[test]
    fn an_observed_restart_shortens_the_window_it_is_measured_against() {
        // The provider still publishes a reset 4 hours out and a 10-hour span,
        // so its arithmetic puts the start 6 hours back. The daemon saw the
        // window actually restart 2 hours ago, inside a half-hour bracket.
        let observed = ObservedStart {
            not_before: now() - Duration::hours(2) - Duration::minutes(15),
            not_after: now() - Duration::hours(2) + Duration::minutes(15),
        };
        let w = effective_window(&view(window(30.0, 4), Some(observed)), now()).unwrap();
        assert_eq!(w.start, now() - Duration::hours(2));
        assert_eq!(w.start_uncertainty, Duration::minutes(30));
        // Six hours of window remain, not ten, and 30% of the budget is gone
        // two of those hours in: a third of the window for a third of the
        // clock is dead level, where the published span called it 0.5×.
        assert_eq!(w.span(), Duration::hours(6));
        assert_eq!(pace(&w, now()), Some(0.9));
        assert_eq!(pace(&effective(30.0, 4), now()), Some(0.5));
    }

    #[test]
    fn a_restart_older_than_the_window_itself_is_ignored() {
        // A reset from two windows ago says nothing about this one: a window
        // cannot have begun before its own `reset_at` less its span.
        let stale = ObservedStart {
            not_before: now() - Duration::hours(30),
            not_after: now() - Duration::hours(29),
        };
        let w = effective_window(&view(window(30.0, 4), Some(stale)), now()).unwrap();
        assert_eq!(w.start, now() - Duration::hours(6));
        assert_eq!(w.start_uncertainty, Duration::zero());
    }

    #[test]
    fn a_window_without_a_reset_instant_has_no_effective_window() {
        let mut no_reset = window(0.0, 4);
        no_reset.reset_at = None;
        assert_eq!(effective_window(&view(no_reset, None), now()), None);
    }

    #[test]
    fn pace_compares_usage_against_elapsed_time() {
        // 60% used with 60% elapsed is exactly on track.
        assert_eq!(pace(&effective(60.0, 4), now()), Some(1.0));
        // 90% used with 60% elapsed is burning fast.
        assert_eq!(pace(&effective(90.0, 4), now()), Some(1.5));
        // 30% used with 60% elapsed leaves headroom.
        assert_eq!(pace(&effective(30.0, 4), now()), Some(0.5));
        // A window that just started cannot be extrapolated from.
        assert_eq!(pace(&effective(0.0, 10), now()), None);
    }

    #[test]
    fn a_short_effective_span_pays_the_fraction_and_no_absolute_floor() {
        // The case an observed start makes reachable, and the one an absolute
        // seconds floor would have blanked: a 5-hour window whose restart was
        // seen an hour before its reset runs for an effective 3600 s, so the
        // documented twentieth is 180 s. At 225 s elapsed the window is past
        // it and the number is owed, even though 225 s is a short stretch in
        // absolute terms — which is the whole argument `docs/dashboard.md`
        // makes for a fraction over a duration.
        let mut short = window(25.0, 0);
        short.reset_kind = ResetKind::Rolling(std::time::Duration::from_secs(5 * 3600));
        short.reset_at = Some(now() + Duration::seconds(3375));
        let observed = ObservedStart {
            not_before: now() - Duration::seconds(255),
            not_after: now() - Duration::seconds(195),
        };
        let w = effective_window(&view(short, Some(observed)), now()).unwrap();
        assert_eq!(w.start, now() - Duration::seconds(225));
        assert_eq!(w.span(), Duration::seconds(3600));
        // 225/3600 is 0.0625, comfortably over the 0.05 floor, so a quarter of
        // the budget spent a sixteenth of the way in is 4× the affordable rate.
        assert_eq!(elapsed_fraction(&w, now()), Some(0.0625));
        assert_eq!(pace(&w, now()), Some(4.0));
        // And the fraction floor still bounds it: one second in, nothing.
        assert_eq!(pace(&w, w.start + Duration::seconds(1)), None);
    }

    #[test]
    fn runway_spends_the_remaining_headroom_at_the_pace_given() {
        // Half a 10-hour window left to spend, at exactly the rate the window
        // affords: five hours of headroom.
        assert_eq!(
            runway_at(&effective(50.0, 5), 1.0),
            Some(Duration::hours(5)),
        );
        // Twice that rate empties it in half the time.
        assert_eq!(
            runway_at(&effective(50.0, 5), 2.0),
            Some(Duration::hours(2) + Duration::minutes(30)),
        );
        // Already capped: no headroom left to project.
        assert_eq!(runway_at(&effective(100.0, 4), 2.0), Some(Duration::zero()));
        // A pace of zero never arrives at the cap.
        assert_eq!(runway_at(&effective(50.0, 5), 0.0), None);
    }

    #[test]
    fn runway_is_reported_even_when_the_window_resets_first() {
        // 30% used with 6 of 10 hours gone burns the rest in another 14h —
        // long after the 4h reset, which is exactly the case `eta_to_cap`
        // suppresses. The rate is still a real answer.
        assert_eq!(
            runway(&effective(30.0, 4), now()),
            Some(Duration::hours(14))
        );
        assert_eq!(eta_to_cap(&effective(30.0, 4), now()), None);

        // Nothing used yet: no rate to project from, either way.
        assert_eq!(runway(&effective(0.0, 4), now()), None);
    }

    #[test]
    fn affordable_pace_spreads_what_is_left_over_the_time_left() {
        // 50% used with half the window to go: usage and clock are level.
        assert_eq!(affordable_pace(&effective(50.0, 5), now()), Some(1.0));
        // 30% used with half to go — the shape of a weekly window 84h from
        // reset — leaves 70% for 50% of the span: 1.4× the nominal rate.
        assert_eq!(affordable_pace(&effective(30.0, 5), now()), Some(1.4));
        // At the cap there is nothing left to afford.
        assert_eq!(affordable_pace(&effective(100.0, 5), now()), Some(0.0));
        // A window at its reset has no time left to spread anything over.
        assert_eq!(affordable_pace(&effective(50.0, 0), now()), None);
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
        let window = effective(60.0, 4);
        assert_eq!(pace(&window, now()), Some(1.0));
        // ...but 10 points of it went in the last half hour, which is twice
        // the rate the window can afford.
        let series = [point(30, 50.0), point(0, 60.0)];
        assert_eq!(
            as_shown(recent_pace(&window, &series, max_gap(), now())),
            Some(2.0)
        );
    }

    #[test]
    fn recent_pace_never_measures_across_a_rollover() {
        let window = effective(60.0, 4);
        // A reading from the window that came before this one: same series,
        // different reset instant, and 90% used where this window has 60%.
        let mut previous = point(45, 90.0);
        previous.reset_at = Some(now() - Duration::hours(1));

        let series = [previous, point(30, 50.0), point(0, 60.0)];
        // Measured from the far side of the boundary only — reading across it
        // would report usage falling by 30 points.
        assert_eq!(
            as_shown(recent_pace(&window, &series, max_gap(), now())),
            Some(2.0)
        );
    }

    #[test]
    fn recent_pace_needs_two_readings_far_enough_apart() {
        let window = effective(60.0, 4);
        assert_eq!(recent_pace(&window, &[], max_gap(), now()), None);
        assert_eq!(
            recent_pace(&window, &[point(0, 60.0)], max_gap(), now()),
            None
        );
        // Two polls two minutes apart are mostly rounding.
        let series = [point(2, 59.0), point(0, 60.0)];
        assert_eq!(recent_pace(&window, &series, max_gap(), now()), None);
    }

    #[test]
    fn a_provider_correction_ends_the_stretch_rather_than_reading_across_it() {
        let window = effective(70.0, 4);
        // 62% revised down to 60% half an hour ago, then 10 points burnt since.
        let series = [
            point(60, 40.0),
            point(45, 62.0),
            point(30, 60.0),
            point(0, 70.0),
        ];
        // Measured from the far side of the revision only: 10 points in 30
        // minutes of a 10-hour window is twice what it can afford. Reading
        // across it would subtract two readings that never described the same
        // accounting.
        assert_eq!(
            as_shown(recent_pace(&window, &series, max_gap(), now())),
            Some(2.0)
        );
    }

    #[test]
    fn a_stretch_whose_only_content_is_a_correction_has_no_rate() {
        // Two readings with a revision between them leave nothing to measure:
        // no interval of this series describes a burn. Better to shed the
        // field than to report the `0.00×` that clamping a negative would.
        let window = effective(60.0, 4);
        let series = [point(30, 62.0), point(0, 60.0)];
        assert_eq!(recent_pace(&window, &series, max_gap(), now()), None);
    }

    #[test]
    fn a_reset_too_small_for_the_detector_still_ends_the_stretch() {
        // A weekly window restarting from 1.8% is under `MIN_RESET_DROP`, so
        // nothing recorded it and no anchor exists — but the fall is still a
        // fall, and the burn since it is the only rate there is. Reading
        // across it would subtract 1.8 from 1.2 and print `0.00× now` over
        // exactly the stretch a fresh window was being spent.
        //
        // 1.2 points in the 40 minutes since the restart, against a 10-hour
        // window: 0.012 / 2400s × 36000s.
        let window = effective(1.2, 4);
        let series = [
            point(50, 1.8),
            point(40, 0.0),
            point(20, 0.6),
            point(0, 1.2),
        ];
        assert_eq!(
            as_shown(recent_pace(&window, &series, max_gap(), now())),
            Some(0.18)
        );
    }

    #[test]
    fn a_young_window_has_no_average_pace_however_long_it_has_run() {
        // Five minutes into a five-day window with 5% spent. The division is
        // sound and worthless: 300 seconds is a fourteen-hundredth of the
        // window, so it reports 72×, and the runway derived from it warns of a
        // cap in an hour and a half. A floor in absolute time cannot catch
        // this — the same 300 seconds is a twentieth of a shorter window — so
        // the fraction is what is floored.
        let long = EffectiveWindow {
            window: WindowId::from("w"),
            start: now() - Duration::minutes(5),
            reset_at: now() - Duration::minutes(5) + Duration::days(5),
            start_uncertainty: Duration::zero(),
            used: Some(0.05),
        };
        assert_eq!(pace(&long, now()), None);
        assert_eq!(runway(&long, now()), None);
        assert_eq!(eta_to_cap(&long, now()), None);

        // The field arrives once a twentieth of the window has run, and what
        // it can print on arrival is bounded by that same twentieth.
        let grown = now() + Duration::hours(7);
        let got = pace(&long, grown).expect("a window past its opening");
        assert!(got <= 1.0 / MIN_ELAPSED_FRACTION, "unbounded pace {got}");
    }

    #[test]
    fn a_slip_too_small_to_print_does_not_end_the_stretch() {
        let window = effective(60.0, 4);
        // A tenth of a point given back ten minutes ago: below the rounding
        // the row prints, so cutting there would drop the field over a change
        // nobody can see. 2 points in 40 minutes of a 10-hour window.
        let series = [
            point(40, 58.0),
            point(30, 58.5),
            point(20, 59.0),
            point(10, 58.9),
            point(0, 60.0),
        ];
        assert_eq!(
            as_shown(recent_pace(&window, &series, max_gap(), now())),
            Some(0.3)
        );

        // Six tenths of a point is a change the row would show, and the cut
        // stands: measured from the far side of it only.
        let series = [
            point(40, 58.0),
            point(30, 58.5),
            point(20, 59.0),
            point(10, 58.4),
            point(0, 60.0),
        ];
        assert_eq!(
            as_shown(recent_pace(&window, &series, max_gap(), now())),
            Some(0.96)
        );
    }

    #[test]
    fn a_window_that_just_opened_has_no_recent_stretch_to_measure() {
        // Nothing before the window's own start counts, and it started now.
        let window = effective(0.0, 10);
        let series = [point(30, 50.0), point(0, 60.0)];
        assert_eq!(recent_pace(&window, &series, max_gap(), now()), None);
    }

    #[test]
    fn eta_to_cap_only_fires_when_the_cap_comes_first() {
        // 90% used, 6h elapsed → ~0.67h to burn the last 10%, well before the
        // 4h reset.
        let eta = eta_to_cap(&effective(90.0, 4), now()).expect("cap projected");
        assert!(eta > now() && eta < now() + Duration::hours(4));

        // 30% used at the same point projects past the reset: no warning.
        assert_eq!(eta_to_cap(&effective(30.0, 4), now()), None);
        // Nothing used yet: no rate to extrapolate.
        assert_eq!(eta_to_cap(&effective(0.0, 4), now()), None);
        // Already capped.
        assert_eq!(eta_to_cap(&effective(100.0, 4), now()), Some(now()));
    }

    #[test]
    fn metrics_are_none_without_a_measurable_window() {
        // A window whose unit gives no ratio: the clock is known, the usage is
        // not, and every number here is usage over the clock.
        let mut unmeasurable = window(50.0, 4);
        unmeasurable.unit = QuotaUnit::Messages;
        unmeasurable.limit = None;
        let w = effective_window(&view(unmeasurable, None), now()).unwrap();
        assert_eq!(pace(&w, now()), None);
        assert_eq!(eta_to_cap(&w, now()), None);
        assert_eq!(runway(&w, now()), None);
        assert_eq!(affordable_pace(&w, now()), None);
        // The clock half still works, which is what lets the chart draw rules
        // for a window whose usage it cannot scale.
        assert_eq!(elapsed_fraction(&w, now()), Some(0.6));
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
            prev_observed_at: Some(now() - Duration::hours(hours_ago) - Duration::minutes(3)),
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
            &WindowId::from("w"),
            Some(&effective(60.0, 4)),
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
            &WindowId::from("w"),
            Some(&effective(60.0, 4)),
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
        let found = boundaries(
            &WindowId::from("w"),
            Some(&effective(60.0, 4)),
            &[seen],
            from,
            to,
        );
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
            &WindowId::from("w"),
            Some(&effective(60.0, 4)),
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
        let found = boundaries(
            &WindowId::from("w"),
            Some(&effective(10.0, 20)),
            &[],
            from,
            to,
        );
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
        assert!(boundaries(&WindowId::from("w"), None, &[], from, to).is_empty());
    }
}

/// Property tests for the derived numbers, against a generated ground truth.
///
/// Two families, one per defect. The first generates a window whose real start
/// is later than `reset_at - span` — a reset the provider did not announce —
/// and checks the derived numbers against the window that actually ran. The
/// second generates a series with an outage, a reset, or a stale tail in it,
/// and checks that the "recent" rate describes the recent end of it.
#[cfg(test)]
mod properties {
    use super::*;
    use chrono::TimeZone;
    use proptest::prelude::*;
    use teiryo_core::domain::{PollId, QuotaUnit, ResetKind, WindowScope};
    use teiryo_core::rollover::ObservedStart;
    use teiryo_core::{BarStyle, RenderHint};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap()
    }

    fn hint() -> RenderHint {
        RenderHint {
            style: BarStyle::Percent,
            warn_threshold: 0.8,
            critical_threshold: 0.95,
            note: None,
        }
    }

    /// A window that reset earlier than the provider's arithmetic implies.
    ///
    /// `reset_at` is where the provider says the window ends; `span` is the
    /// nominal length, so `reset_at - span` is what the code took for the start
    /// before this change. `start` is where the window *actually* opened —
    /// after that instant, because a missed reset can only ever have moved the
    /// start forwards.
    ///
    /// The daemon never sees the restart happen, only two polls that straddle
    /// it, so what it publishes is the bracket `gap_before`/`gap_after` mark
    /// out around `start` rather than `start` itself.
    #[derive(Debug, Clone)]
    struct EarlyReset {
        span: Duration,
        reset_at: DateTime<Utc>,
        start: DateTime<Utc>,
        gap_before: Duration,
        gap_after: Duration,
        used: f64,
    }

    impl EarlyReset {
        /// The window as it reaches the TUI: the provider's reading, plus the
        /// bracket the daemon observed the restart inside.
        fn view(&self) -> WindowView {
            WindowView {
                window: QuotaWindow {
                    id: WindowId::from("w"),
                    label: "w".to_owned(),
                    scope: WindowScope::AccountWide,
                    reset_kind: ResetKind::Rolling(self.span.to_std().expect("positive span")),
                    unit: QuotaUnit::Percent,
                    used: self.used * 100.0,
                    limit: Some(100.0),
                    reset_at: Some(self.reset_at),
                },
                hint: hint(),
                observed_start: Some(self.observed()),
            }
        }

        fn observed(&self) -> ObservedStart {
            ObservedStart {
                not_before: self.start - self.gap_before,
                not_after: self.start + self.gap_after,
            }
        }

        /// The pace of a window that began at `start`: usage over the fraction
        /// of `start..reset_at` that has elapsed. A window that reset early is
        /// shorter and still carries a full budget, so the same usage is a
        /// faster burn — and this is increasing in `start`, which is what lets
        /// the bracket's two ends bound the answer.
        fn pace_from(&self, start: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
            let length = (self.reset_at - start).num_seconds() as f64;
            let elapsed = (now - start).num_seconds() as f64;
            self.used / (elapsed / length)
        }
    }

    /// The least elapsed time, in seconds, a draw may leave the window with.
    ///
    /// Enough that `elapsed / (elapsed + remaining)` clears
    /// `MIN_ELAPSED_FRACTION`, and never less than the hour of headroom every
    /// draw keeps on each side of `now`.
    fn youngest_measurable(remaining_secs: i64) -> i64 {
        let youngest = (remaining_secs as f64 * MIN_ELAPSED_FRACTION / (1.0 - MIN_ELAPSED_FRACTION))
            .ceil() as i64;
        youngest.max(3600)
    }

    /// The shared tail of both generators: turn a drawn tuple into the window
    /// it describes.
    fn assemble(
        (span_secs, used, remaining_secs, early_by, before, after): DrawnReset,
    ) -> EarlyReset {
        let reset_at = now() + Duration::seconds(remaining_secs);
        EarlyReset {
            span: Duration::seconds(span_secs),
            reset_at,
            start: reset_at - Duration::seconds(span_secs) + Duration::seconds(early_by),
            gap_before: Duration::seconds(before),
            gap_after: Duration::seconds(after),
            used,
        }
    }

    /// `(span, used, remaining, early_by, gap_before, gap_after)`, all seconds
    /// but `used`.
    type DrawnReset = (i64, f64, i64, i64, i64, i64);

    /// A window that restarted early, in every shape `effective_window` will
    /// accept one.
    ///
    /// Deliberately unrestricted, and not the generator to reach for when a
    /// property needs a pace to talk about. The `MIN_ELAPSED_FRACTION` floor
    /// below is measured at `start`, but `effective_window` anchors at the
    /// bracket's midpoint, which sits later — so this draws windows whose
    /// anchor falls back under the floor, where `pace`, `runway` and
    /// `eta_to_cap` are all `None` while `affordable_pace` still has a number.
    /// That band is the first shape a user meets after an outage, and
    /// `derived_numbers_stay_in_range` — which asserts over *whatever* the
    /// window's shape, skipping each absent number rather than requiring it —
    /// is the property that relies on reaching it.
    ///
    /// Use [`early_reset_with_a_measurable_pace`] instead when the property
    /// must have a pace.
    fn early_reset() -> impl Strategy<Value = EarlyReset> {
        // 5 hours to 14 days, the two shapes Claude publishes and beyond.
        (5i64 * 3600..14 * 24 * 3600i64, 0.01f64..0.95)
            .prop_flat_map(|(span_secs, used)| {
                // Leave at least an hour of window on each side of `now`, so
                // neither the elapsed fraction nor the remaining one degenerates.
                let remaining = 3600i64..(span_secs / 2);
                (Just(span_secs), Just(used), remaining)
            })
            .prop_flat_map(|(span_secs, used, remaining_secs)| {
                // How much later than `reset_at - span` the window really began.
                // An hour of headroom on each end keeps the bracket below both
                // that instant and `now`, and the window must additionally have
                // run for `MIN_ELAPSED_FRACTION` of its effective length *as
                // measured at `start`*, which is where this stops and the
                // restricted generator carries on. `elapsed = span - remaining
                // - early_by`, and the effective length is `elapsed +
                // remaining`.
                let hi = span_secs - remaining_secs - youngest_measurable(remaining_secs);
                // An empty `Range` handed to proptest *panics* at generation
                // time rather than failing a case, and it would take out every
                // property drawing from here. Four numeric ranges across three
                // stages are coupled with nothing linking them, so a future
                // widening of any one of them fails loudly here instead of at
                // random.
                debug_assert!(
                    hi > 3600,
                    "early_by is empty: 3600..{hi} (span {span_secs}, remaining {remaining_secs})"
                );
                let early_by = 3600i64..hi;
                (
                    Just(span_secs),
                    Just(used),
                    Just(remaining_secs),
                    early_by,
                    60i64..3600,
                    60i64..3600,
                )
            })
            .prop_map(assemble)
    }

    /// [`early_reset`], narrowed to the draws whose *anchor* — not merely
    /// whose `start` — is old enough for `pace` to answer.
    ///
    /// For properties that assert something about the pace itself, and so
    /// cannot be handed a window that has none.
    fn early_reset_with_a_measurable_pace() -> impl Strategy<Value = EarlyReset> {
        // 5 hours to 14 days, the two shapes Claude publishes and beyond.
        (5i64 * 3600..14 * 24 * 3600i64, 0.01f64..0.95)
            .prop_flat_map(|(span_secs, used)| {
                // Leave at least an hour of window on each side of `now`, so
                // neither the elapsed fraction nor the remaining one degenerates.
                // The bracket's two gaps are drawn here rather than last,
                // because the headroom `early_by` needs depends on them.
                let remaining = 3600i64..(span_secs / 2);
                (
                    Just(span_secs),
                    Just(used),
                    remaining,
                    60i64..3600,
                    60i64..3600,
                )
            })
            .prop_flat_map(|(span_secs, used, remaining_secs, before, after)| {
                // How much later than `reset_at - span` the window really began.
                // An hour of headroom on each end keeps the bracket below both
                // that instant and `now`, and the window must additionally have
                // run for `MIN_ELAPSED_FRACTION` of its effective length, or it
                // is too young to have a pace at all and there is nothing to
                // anchor.
                //
                // That headroom is measured at the *anchor*, not at `start`,
                // because nothing downstream computes against `start`:
                // `effective_window` anchors at `ObservedStart::estimate()`,
                // the bracket's midpoint, which sits `(after - before) / 2`
                // later than `start` — up to half an hour of it. Every second
                // of that shift comes off the elapsed side while leaving the
                // remaining side untouched, so a window sized to clear
                // `MIN_ELAPSED_FRACTION` at `start` can still fall under it at
                // the midpoint, and `pace` then hands back `None` where the
                // property expects a number. Subtracting the exact shift the
                // gaps imply — rounded up, and floored at zero because a
                // midpoint *earlier* than `start` only adds headroom — makes
                // the guarantee hold at the instant that is actually used.
                //
                // The true displacement is `(after - before) / 2`, and `shift`
                // is its ceiling clamped at zero — so `elapsed` is
                // `span - remaining - early_by` less that displacement, and
                // subtracting `shift` instead concedes at most a second. The
                // effective length is `elapsed + remaining`. The range stays
                // non-empty for every input the earlier stages admit:
                // `span - remaining > span / 2 >= 9000`,
                // `youngest_measurable` is at most `3600` whenever it binds
                // against that floor, and `shift` is at most `1770`, leaving
                // at least 31 seconds of width in the worst case.
                let shift = ((after - before + 1) / 2).max(0);
                let hi = span_secs - remaining_secs - youngest_measurable(remaining_secs) - shift;
                // As above, and tighter here by the width of `shift`: the
                // narrowest admitted case leaves 31 seconds of range.
                debug_assert!(
                    hi > 3600,
                    "early_by is empty: 3600..{hi} (span {span_secs}, \
                     remaining {remaining_secs}, shift {shift})"
                );
                let early_by = 3600i64..hi;
                (
                    Just(span_secs),
                    Just(used),
                    Just(remaining_secs),
                    early_by,
                    Just(before),
                    Just(after),
                )
            })
            .prop_map(assemble)
    }

    /// A window whose *anchor* leaves it under [`MIN_ELAPSED_FRACTION`] — the
    /// band where `pace` and everything measured from the start are `None`.
    ///
    /// [`early_reset`] reaches this band too, in roughly six draws in ten
    /// thousand, and the split exists to keep it able to. But
    /// `derived_numbers_stay_in_range` cannot *see* it when it gets there:
    /// every arm of that property is an `if let Some(..)`, and a shape defined
    /// by its `None`s gives those arms nothing to hold. Deleting both of
    /// `pace`'s blanking gates leaves it green at 200,000 cases. So the band
    /// is generated directly here, and the absences are asserted as absences.
    ///
    /// The ranges are picked to land in the band reliably rather than to
    /// mirror `early_reset`'s. `not_after <= now` requires the bracket's far
    /// end to have passed already, which puts a floor under the elapsed
    /// stretch; the band is only comfortably reachable where a twentieth of
    /// the window clears that floor, so the gaps are narrower and `remaining`
    /// is longer than the unrestricted generator draws.
    fn early_reset_too_young_to_pace() -> impl Strategy<Value = EarlyReset> {
        (
            26_000i64..14 * 24 * 3600,
            0.01f64..0.95,
            60i64..600,
            60i64..600,
        )
            .prop_flat_map(|(span_secs, used, before, after)| {
                let remaining = 12_000i64..(span_secs / 2);
                (
                    Just(span_secs),
                    Just(used),
                    remaining,
                    Just(before),
                    Just(after),
                )
            })
            .prop_flat_map(|(span_secs, used, remaining_secs, before, after)| {
                // Where the anchor sits relative to `start`: the same midpoint
                // arithmetic `ObservedStart::estimate` does.
                let displacement = (after - before) / 2;
                // Under this many seconds elapsed there is no pace.
                // `youngest_measurable` floors its answer at an hour, which is
                // the wrong end of the question here, so the fraction is
                // applied directly.
                let floor = (remaining_secs as f64 * MIN_ELAPSED_FRACTION
                    / (1.0 - MIN_ELAPSED_FRACTION))
                    .ceil() as i64;
                // And at least this many, or the bracket's far end has not
                // passed and `effective_window` drops the anchor entirely.
                let least = (after - displacement).max(1);
                debug_assert!(
                    floor > least,
                    "the sub-floor band is empty: {least}..{floor} (remaining {remaining_secs})"
                );
                (
                    Just(span_secs),
                    Just(used),
                    Just(remaining_secs),
                    least..floor,
                    Just(before),
                    Just(after),
                )
            })
            .prop_map(
                |(span_secs, used, remaining_secs, elapsed, before, after)| {
                    let displacement = (after - before) / 2;
                    let early_by = span_secs - remaining_secs - elapsed - displacement;
                    debug_assert!(
                        early_by >= 3600,
                        "no headroom below the nominal start: {early_by}"
                    );
                    assemble((span_secs, used, remaining_secs, early_by, before, after))
                },
            )
    }

    /// A generated series of readings of one window: an older stretch, an
    /// optional outage, then a newer stretch ending `stale` before `now`.
    #[derive(Debug, Clone)]
    struct Series {
        cadence: Duration,
        /// The stretch between the two, in multiples of `cadence`. At most
        /// four it is ordinary cadence and the two are one continuous run;
        /// beyond that it is longer than `max_gap` and they are not.
        outage: i64,
        /// Utilization at each reading of the older stretch, oldest first.
        /// The lookback catches this stretch mid-window, so it starts wherever
        /// the previous window had already climbed to — which is what makes a
        /// reset after it a fall rather than a flat line.
        older: Vec<f64>,
        /// Where the older stretch picks up from.
        older_base: f64,
        /// The same for the newer stretch, which is what a "now" rate is about.
        newer: Vec<f64>,
        /// How long before `now` the newest reading was taken.
        stale: Duration,
        /// Whether the window reset during the outage. When it did, the
        /// provider does not say so: `reset_at` stays where it was.
        reset_between: bool,
        /// Sort keys, so a property can rearrange the series and check the
        /// answer does not move.
        order: Vec<u64>,
    }

    /// The window these series are readings of: a weekly one, three days from
    /// its reset, so every generated reading falls inside both the window and
    /// the 12-hour lookback and neither edge is what the property is measuring.
    const SERIES_SPAN: Duration = Duration::seconds(7 * 24 * 3600);

    fn series_window(used: f64) -> EffectiveWindow {
        let view = WindowView {
            window: QuotaWindow {
                id: WindowId::from("w"),
                label: "w".to_owned(),
                scope: WindowScope::AccountWide,
                reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(7 * 24 * 3600)),
                unit: QuotaUnit::Percent,
                used: used * 100.0,
                limit: Some(100.0),
                reset_at: Some(now() + Duration::days(3)),
            },
            hint: hint(),
            // Deliberately nothing observed: these properties are about
            // reading the series itself, so the reset inside it has to be
            // found there rather than handed over by the daemon.
            observed_start: None,
        };
        effective_window(&view, now()).expect("a reset instant")
    }

    impl Series {
        /// Utilization at each reading, oldest first. The newer stretch
        /// continues the older one, or restarts from zero when the window reset
        /// between them.
        fn values(&self) -> (Vec<f64>, Vec<f64>) {
            let older: Vec<f64> = self
                .older
                .iter()
                .map(|v| (self.older_base + v).min(1.0))
                .collect();
            let base = if self.reset_between {
                0.0
            } else {
                older.last().copied().unwrap_or(0.0)
            };
            let newer = self.newer.iter().map(|v| (base + v).min(1.0)).collect();
            (older, newer)
        }

        /// The readings, shifted `extra` further into the past — which is how a
        /// property makes an otherwise fresh series stale.
        fn points_aged(&self, extra: Duration) -> Vec<QuotaSnapshot> {
            let (older, newer) = self.values();
            let newest = now() - self.stale - extra;
            let mut out = Vec::new();
            // Newest first while placing them, then reversed: every reading's
            // instant is defined relative to the newest, not to the start.
            for (k, used) in newer.iter().rev().enumerate() {
                out.push(self.point(newest - self.cadence * (k as i32), *used));
            }
            let older_end = newest
                - self.cadence * ((newer.len() as i32 - 1).max(0))
                - self.cadence * (self.outage as i32);
            for (k, used) in older.iter().rev().enumerate() {
                out.push(self.point(older_end - self.cadence * (k as i32), *used));
            }
            out.reverse();
            out
        }

        fn points(&self) -> Vec<QuotaSnapshot> {
            self.points_aged(Duration::zero())
        }

        fn point(&self, ts: DateTime<Utc>, used: f64) -> QuotaSnapshot {
            QuotaSnapshot {
                poll_id: PollId::generate(),
                ts,
                window: WindowId::from("w"),
                label: "w".to_owned(),
                unit: QuotaUnit::Percent,
                used: used * 100.0,
                limit: Some(100.0),
                // Silent throughout: a reset the provider never announced
                // leaves this instant exactly where it was.
                reset_at: Some(now() + Duration::days(3)),
            }
        }

        /// Utilization at the newest reading, which is what the live window
        /// would be reporting.
        fn current(&self) -> f64 {
            self.values().1.last().copied().unwrap_or(0.0)
        }

        /// The rate the newer stretch alone was burnt at, on [`pace`]'s scale.
        /// This is the answer every one of these properties is asking for.
        fn newer_pace(&self) -> f64 {
            let newer = self.values().1;
            let burned = newer.last().copied().unwrap_or(0.0) - newer[0];
            let elapsed = (self.cadence * (newer.len() as i32 - 1)).num_seconds() as f64;
            burned / elapsed * SERIES_SPAN.num_seconds() as f64
        }
    }

    /// A non-decreasing ramp of `len` readings from 0 up to exactly `peak`.
    fn ramp(len: usize, peak: f64) -> Vec<f64> {
        let step = peak / (len - 1) as f64;
        (0..len).map(|k| step * k as f64).collect()
    }

    /// Series of a given shape: how long the stretch between the two runs is
    /// in multiples of the cadence, and whether the window restarted across it.
    fn series_shaped(
        outage: std::ops::Range<i64>,
        reset_between: impl Strategy<Value = bool>,
    ) -> impl Strategy<Value = Series> {
        (
            // Cadence bounded below so `max_gap` is always 4 × cadence, and
            // above so the whole series stays inside the 12-hour lookback.
            150i64..400i64,
            outage,
            (6usize..20, 0.05f64..0.5, 0.0f64..0.5),
            (6usize..20, 0.05f64..0.5),
            0i64..600,
            reset_between,
            prop::collection::vec(any::<u64>(), 40),
        )
            .prop_map(
                |(cadence, outage, older, newer, stale, reset_between, order)| Series {
                    cadence: Duration::seconds(cadence),
                    outage,
                    older: ramp(older.0, older.1),
                    older_base: older.2,
                    newer: ramp(newer.0, newer.1),
                    stale: Duration::seconds(stale),
                    reset_between,
                    order,
                },
            )
    }

    /// Any shape at all: an outage or ordinary cadence, a restart or not.
    fn series() -> impl Strategy<Value = Series> {
        series_shaped(1..12, any::<bool>())
    }

    /// A hole longer than the tolerance — the thing that splits one series
    /// into two measurements.
    fn series_after_an_outage() -> impl Strategy<Value = Series> {
        series_shaped(5..12, any::<bool>())
    }

    /// A silent restart with the daemon polling right through it, so the fall
    /// in `used` is the only thing that can cut the stretch.
    fn series_across_a_silent_reset() -> impl Strategy<Value = Series> {
        series_shaped(1..5, Just(true))
    }

    /// Two rates agree to the two decimals the dashboard prints.
    fn same_rate(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.005
    }

    /// The shrunk cases from the red runs that produced this suite, pinned as
    /// literals.
    ///
    /// They were checked in as `proptest-regressions` seeds, which is not the
    /// same thing: proptest persists an RNG *seed*, not a value, so a seed
    /// reproduces its recorded input only while the strategy is unchanged.
    /// Every seed in that file had already drifted — one described an
    /// `EarlyReset` from before the bracket fields existed, and four described
    /// a `Series` from before `older_base` did — so the file was carrying a
    /// promise it could not keep, and the generators here have moved again
    /// since. Written out, these cases stay the cases.
    #[test]
    fn the_shrunk_early_resets_still_hold() {
        // Both are as recorded; the first predates the bracket, so it is given
        // the narrowest one the generator can draw.
        for w in [
            EarlyReset {
                span: Duration::seconds(209_404),
                reset_at: Utc.with_ymd_and_hms(2026, 8, 21, 13, 0, 23).unwrap(),
                start: Utc.with_ymd_and_hms(2026, 8, 19, 21, 23, 11).unwrap(),
                gap_before: Duration::seconds(60),
                gap_after: Duration::seconds(60),
                used: 0.591_235_081_022_373_7,
            },
            EarlyReset {
                span: Duration::seconds(1_127_057),
                reset_at: Utc.with_ymd_and_hms(2026, 8, 24, 4, 59, 17).unwrap(),
                start: Utc.with_ymd_and_hms(2026, 8, 21, 8, 41, 54).unwrap(),
                gap_before: Duration::seconds(60),
                gap_after: Duration::seconds(60),
                used: 0.770_789_297_552_086,
            },
        ] {
            let e = effective_window(&w.view(), now()).expect("a reset instant");
            assert!(e.start < e.reset_at, "window runs backwards: {w:?}");
            let f = elapsed_fraction(&e, now()).expect("a positive span");
            assert!((0.0..=1.0).contains(&f), "elapsed fraction {f} for {w:?}");
            if let Some(p) = pace(&e, now()) {
                assert!(p.is_finite() && p >= 0.0, "pace {p} for {w:?}");
            }
            if let Some(a) = affordable_pace(&e, now()) {
                assert!(a.is_finite() && a >= 0.0, "affordable {a} for {w:?}");
            }
            if let Some(r) = runway(&e, now()) {
                assert!(r >= Duration::zero(), "runway {r} for {w:?}");
            }
            if let Some(eta) = eta_to_cap(&e, now()) {
                assert!(eta >= now() && eta < w.reset_at, "eta {eta} for {w:?}");
            }
        }
    }

    /// The shrunk `Series`: two identical flat stretches either side of an
    /// outage, which is the degenerate shape the recent-rate properties kept
    /// collapsing to.
    #[test]
    fn the_shrunk_series_still_reports_only_its_newer_stretch() {
        let climb: Vec<f64> = vec![0.0, 0.01, 0.02, 0.03, 0.04, 0.05];
        for reset_between in [false, true] {
            let s = Series {
                cadence: Duration::seconds(150),
                // Beyond four cadences, so the two stretches are not one run.
                outage: 5,
                older: climb.clone(),
                older_base: 0.0,
                newer: climb.clone(),
                stale: Duration::zero(),
                reset_between,
                order: Vec::new(),
            };
            let window = series_window(s.current());
            let gap = gap_tolerance(s.cadence.num_seconds() as u32);
            let got = recent_pace(&window, &s.points(), gap, now()).expect("a rate");
            let want = s.newer_pace();
            assert!(
                (got - want).abs() < 1e-9,
                "reset_between={reset_between}: got {got}, want {want}"
            );
        }
    }

    proptest! {
        /// **P4 — pace is anchored inside the bracket the restart was seen
        /// in.** A window that restarted early is shorter than its nominal
        /// span and still carries a full budget, so the same usage is a faster
        /// burn. The exact instant is unknowable — the daemon has two polls
        /// that straddle it, not the event — so what is checkable is that the
        /// answer lies between the two paces the bracket's ends imply, and
        /// nowhere near the one the provider's arithmetic gives.
        ///
        /// Stated as a bound rather than a value on purpose: after a long
        /// outage the bracket is wide, and a property that pinned a point
        /// value would be asserting a precision the data does not have.
        ///
        /// Red before the change that introduced it: `elapsed_fraction`
        /// computed `reset_at - span` unconditionally, which lies outside the
        /// bracket on the low side —
        /// under-reporting, the direction that says "you are barely using it"
        /// while the quota drains.
        #[test]
        fn pace_is_anchored_inside_the_observed_bracket(
            w in early_reset_with_a_measurable_pace(),
        ) {
            let effective = effective_window(&w.view(), now()).expect("a reset instant");
            let observed = w.observed();
            prop_assert!(
                effective.start >= observed.not_before && effective.start <= observed.not_after,
                "anchored at {} outside the bracket [{}, {}]",
                effective.start, observed.not_before, observed.not_after,
            );

            let got = pace(&effective, now()).expect("a mid-window pace");
            let floor = w.pace_from(observed.not_before, now());
            let ceiling = w.pace_from(observed.not_after, now());
            prop_assert!(
                got >= floor - 0.005 && got <= ceiling + 0.005,
                "pace {got:.4} outside [{floor:.4}, {ceiling:.4}]; the published \
                 span would have read {:.4}",
                w.pace_from(w.reset_at - w.span, now()),
            );
        }

        /// **P10 — every derived number is well-formed.** Whatever the window's
        /// shape, a `Some` is finite and in range: a pace is non-negative, an
        /// elapsed fraction is a fraction, a runway does not run backwards, and
        /// a projected cap lies between now and the reset — which is the whole
        /// content of `eta_to_cap` returning `Some`.
        #[test]
        /// **P11 — a window too young to pace withholds every number measured
        /// from its start, and only those.**
        ///
        /// The shape a user meets right after an outage: the daemon knows the
        /// window restarted and the restart is recent enough that dividing by
        /// the elapsed fraction would report a figure in the tens. `pace`,
        /// `cap in` and the projection are all withheld; `afford`, which
        /// divides the remaining budget by the time left to `reset_at` and
        /// never touches the start, still answers.
        ///
        /// Stated as absences deliberately. This is the one shape
        /// `derived_numbers_stay_in_range` structurally cannot check — its
        /// arms are all `if let Some(..)` — so without this property, deleting
        /// both of `pace`'s blanking gates passes the whole suite.
        #[test]
        fn a_window_too_young_to_pace_withholds_what_rests_on_its_start(
            w in early_reset_too_young_to_pace(),
        ) {
            let effective = effective_window(&w.view(), now()).expect("a reset instant");
            // The anchor was honoured, not dropped back to `reset_at - span`.
            prop_assert_eq!(effective.start, w.observed().estimate(), "anchor dropped");
            let f = elapsed_fraction(&effective, now()).expect("a positive span");
            prop_assert!(f < MIN_ELAPSED_FRACTION, "elapsed fraction {f} clears the floor");

            prop_assert_eq!(pace(&effective, now()), None, "pace at {} elapsed", f);
            prop_assert_eq!(runway(&effective, now()), None, "runway at {} elapsed", f);
            prop_assert_eq!(eta_to_cap(&effective, now()), None, "eta at {} elapsed", f);
            prop_assert!(
                affordable_pace(&effective, now()).is_some(),
                "afford rests on reset_at, not on the start, and must still answer"
            );
        }

        fn derived_numbers_stay_in_range(w in early_reset()) {
            let effective = effective_window(&w.view(), now()).expect("a reset instant");
            prop_assert!(effective.start < effective.reset_at, "window runs backwards");
            let f = elapsed_fraction(&effective, now()).expect("a positive span");
            prop_assert!((0.0..=1.0).contains(&f), "elapsed fraction {f}");
            if let Some(p) = pace(&effective, now()) {
                prop_assert!(p.is_finite() && p >= 0.0, "pace {p}");
            }
            if let Some(a) = affordable_pace(&effective, now()) {
                prop_assert!(a.is_finite() && a >= 0.0, "affordable {a}");
            }
            if let Some(r) = runway(&effective, now()) {
                prop_assert!(r >= Duration::zero(), "runway {r}");
            }
            if let Some(eta) = eta_to_cap(&effective, now()) {
                prop_assert!(eta >= now() && eta < w.reset_at, "eta {eta}");
            }
        }

        /// **P6 — only the newest contiguous stretch counts.** Everything
        /// before an outage longer than `max_gap` is a different measurement
        /// session; rewriting or deleting it must not move the rate labelled
        /// "now".
        ///
        /// Red before the change that introduced it: the first and last
        /// reading in the whole lookback were taken as the ends of the
        /// stretch, so a daemon down for eleven of the last twelve hours
        /// reported a twelve-hour average as the current rate.
        #[test]
        fn a_rate_labelled_now_ignores_everything_before_the_outage(
            s in series_after_an_outage(),
        ) {
            let window = series_window(s.current());
            let gap = gap_tolerance(s.cadence.num_seconds() as u32);
            let whole = recent_pace(&window, &s.points(), gap, now());

            let newest_only: Vec<_> = s
                .points()
                .into_iter()
                .filter(|p| now() - p.ts <= s.cadence * (s.newer.len() as i32) + s.stale)
                .collect();
            let trimmed = recent_pace(&window, &newest_only, gap, now());

            prop_assert!(
                match (whole, trimmed) {
                    (Some(a), Some(b)) => same_rate(a, b),
                    (a, b) => a == b,
                },
                "dropping the stretch before the outage changed the rate: \
                 {whole:?} became {trimmed:?} (expected {:.4})",
                s.newer_pace(),
            );
        }

        /// **P7 — a stale series has no current rate.** Once the newest reading
        /// is older than the gap tolerance there is nothing to say about now,
        /// and the row should shed the field rather than print a measurement
        /// taken an hour ago and label it `× now`.
        ///
        /// Red before the change that introduced it: nothing looked at how
        /// old the newest reading was.
        #[test]
        fn a_stale_series_reports_no_current_rate(s in series()) {
            let window = series_window(s.current());
            let gap = gap_tolerance(s.cadence.num_seconds() as u32);
            let aged = s.points_aged(gap + Duration::minutes(1));
            prop_assert_eq!(
                recent_pace(&window, &aged, gap, now()),
                None,
                "reported a rate from readings {} old, past a {} tolerance",
                (now() - aged.last().expect("readings").ts),
                gap,
            );
        }

        /// **P8 — a reset inside the lookback is measured from its far side.**
        /// When usage restarts mid-lookback the burn since the restart is the
        /// only rate that exists; the drop itself is not a measurement.
        ///
        /// Red before the change that introduced it: `reset_at` did not move,
        /// so the reset-instant check kept both sides of it inside one
        /// stretch, and the rate was
        /// measured from before it — diluted towards zero, and exactly
        /// `0.00× now` whenever the old window had climbed past where the new
        /// one currently sat. Either way the row read idle over exactly the
        /// stretch the user was burning through a fresh window.
        #[test]
        fn a_reset_inside_the_lookback_does_not_read_as_idle(
            s in series_across_a_silent_reset(),
        ) {
            let window = series_window(s.current());
            let gap = gap_tolerance(s.cadence.num_seconds() as u32);
            let got =
                recent_pace(&window, &s.points(), gap, now()).expect("a rate after the reset");
            prop_assert!(
                same_rate(got, s.newer_pace()),
                "read {got:.4} across the reset; the stretch since it burnt at {:.4}",
                s.newer_pace(),
            );
        }

        /// **P9 — the answer does not depend on the order it arrives in.**
        /// `points` comes off the wire, and nothing in the protocol promises an
        /// ordering; a rate that changes when the same readings are shuffled or
        /// a duplicate poll is included is not measuring the series.
        ///
        /// Red before the change that introduced it: the first and last
        /// matching element of the slice were taken as the ends of the
        /// stretch, in iteration order.
        #[test]
        fn the_rate_survives_reordering_and_duplicates(s in series()) {
            let window = series_window(s.current());
            let ordered = s.points();
            let gap = gap_tolerance(s.cadence.num_seconds() as u32);
            let expected = recent_pace(&window, &ordered, gap, now());

            let mut shuffled = ordered.clone();
            shuffled.sort_by_key(|p| s.order[(p.ts.timestamp() as usize) % s.order.len()]);
            prop_assert_eq!(
                recent_pace(&window, &shuffled, gap, now()),
                expected,
                "reordering changed the rate",
            );

            let mut doubled = ordered.clone();
            doubled.extend(ordered.iter().cloned());
            prop_assert_eq!(
                recent_pace(&window, &doubled, gap, now()),
                expected,
                "a duplicated poll changed the rate",
            );
        }
    }

    /// **P5 — the reported symptom, as a worked example.** A weekly window
    /// publishing a Thursday reset actually restarted on the Tuesday; by
    /// Tuesday evening 5% of it is gone.
    ///
    /// Against the published seven-day span that reads as 0.07× — "you are
    /// barely touching it". Against the two-day window that actually ran, with
    /// its full budget, it is 0.60× and climbing.
    #[test]
    fn a_silent_early_weekly_reset_reads_at_its_true_pace() {
        let reset_at = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        let observed_start = Utc.with_ymd_and_hms(2026, 8, 25, 0, 0, 0).unwrap();
        let evening = observed_start + Duration::hours(4);
        let weekly = EarlyReset {
            span: Duration::days(7),
            reset_at,
            start: observed_start,
            // Polled every three minutes, so the restart is bracketed tightly
            // and the midpoint is the restart to within a poll.
            gap_before: Duration::minutes(3),
            gap_after: Duration::minutes(3),
            used: 0.05,
        };

        // What the window that actually ran says: 4 hours into 2 days.
        let expected: f64 = 0.05 / (4.0 / 48.0);
        assert!((expected - 0.6).abs() < 0.005, "worked example: {expected}");

        let effective = effective_window(&weekly.view(), evening).expect("a reset instant");
        let got = pace(&effective, evening).expect("a pace");
        assert!(
            (got - expected).abs() < 0.005,
            "read {got:.2}× against a true {expected:.2}×",
        );

        // And what it read before: the same 5% spread over five of seven days.
        let published = WindowView {
            observed_start: None,
            ..weekly.view()
        };
        let stale = effective_window(&published, evening).expect("a reset instant");
        let was = pace(&stale, evening).expect("a pace");
        assert!((was - 0.07).abs() < 0.005, "the old reading was {was:.2}×");
    }
}
