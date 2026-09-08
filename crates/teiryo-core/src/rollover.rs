//! Detecting when a quota window rolled over, and whether that was a surprise.
//!
//! A rollover is read from `reset_at` **moving** where the provider moved it,
//! and otherwise from `used` collapsing. The second signal is the weaker one
//! and is treated as such: a provider correction that shaves a couple of
//! points off `used` mid-window is not a new window, and treating it as one
//! would split a chart series that never actually broke. What separates the
//! two is [`is_collapse`] — a fall to at most half of what was there, and by
//! at least a few points, which a correction is not and a reset always is.
//!
//! The interesting cases are the ones the provider did not advertise. A window
//! whose `reset_at` advances while the *old* reset was still in the future
//! rolled early; one whose `reset_at` moves backwards had its reset pulled in.
//! Both are recorded so the dashboard can say so rather than showing an
//! unexplained cliff, and both survive a daemon restart because they are
//! written next to the poll that produced them.
//!
//! Note what is *not* a surprise: `reset_at` jumping further ahead than one
//! span. Rolling windows are anchored to first use, so after an idle stretch
//! the next window legitimately starts later than the last one ended.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{AccountId, PollId, QuotaWindow, WindowId};

/// Tolerance absorbing clock skew between our poll timestamp and the
/// provider's published reset instant, plus provider-side rounding. Below this
/// a difference is noise, not a decision.
///
/// Applied to `reset_at` moves in **both** directions. Providers recompute the
/// instant per request, so a window that has not rolled at all still reports a
/// `reset_at` that drifts by a fraction of a second between polls; without the
/// tolerance on the forward comparison, every poll of such a provider looks
/// like a rollover.
pub const RESET_TOLERANCE: Duration = Duration::seconds(120);

/// What must be left for a fall in usage to be a correction rather than a
/// reset. A window holding at most half of what it held a poll ago was not
/// revised — it restarted.
///
/// Deliberately a ratio rather than an absolute drop. An absolute threshold
/// asks "did a lot of quota vanish", which is the wrong question: a weekly
/// window that resets from 20% to 0 gave back a fifth of the week and is
/// indistinguishable, on that measure, from a rounding fix — so it went
/// unrecorded, and every number derived from the window's start stayed
/// anchored to a window that had already ended.
pub const RESET_COLLAPSE_RATIO: f64 = 0.5;

/// How much of the window must actually vanish before a fall is considered at
/// all, whatever the ratio says.
///
/// The ratio alone is too eager at the bottom of the scale, where halving a
/// small number is easy: a correction from 5% to 2% passes it, and a false
/// reset is far more costly than a missed one. It re-anchors the window to an
/// instant nothing actually restarted at, so every derived number — the pace,
/// the runway, the countdown to the cap — is measured against a span that
/// never ran, and the ones the client suppresses while a window is young
/// simply disappear from the row. A missed reset merely leaves the pace
/// reading as it did before any of this existed.
///
/// Five points is comfortably above the corrections providers actually make
/// (a point or two) and comfortably below the reported symptom, a weekly
/// window restarting from 20%.
pub const MIN_RESET_DROP: f64 = 0.05;

/// Why a window's accounting restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RolloverKind {
    /// `reset_at` advanced at or after the old reset was due. Business as
    /// usual — the window simply expired.
    Scheduled,
    /// `reset_at` advanced while the old reset was still in the future: the
    /// window rolled over earlier than the provider said it would.
    Early,
    /// `reset_at` moved backwards — the reset was pulled in.
    Retracted,
    /// Usage collapsed with `reset_at` unchanged. The provider reset the
    /// window without saying so.
    Unannounced,
}

impl RolloverKind {
    /// Whether this is something the provider did not advertise.
    pub fn is_surprise(self) -> bool {
        !matches!(self, RolloverKind::Scheduled)
    }

    /// Whether this marks the boundary between two windows.
    ///
    /// `Unannounced` deliberately does not: it is inferred from `used` alone,
    /// which is exactly the signal that is not trustworthy enough to break a
    /// series on. It is drawn as a marker instead.
    pub fn is_boundary(self) -> bool {
        !matches!(self, RolloverKind::Unannounced)
    }

    /// Stable string for the storage column.
    pub fn as_str(self) -> &'static str {
        match self {
            RolloverKind::Scheduled => "scheduled",
            RolloverKind::Early => "early",
            RolloverKind::Retracted => "retracted",
            RolloverKind::Unannounced => "unannounced",
        }
    }

    /// Inverse of [`RolloverKind::as_str`]. Deliberately not `FromStr`: this
    /// is a storage encoding, not a user-facing parse.
    pub fn from_column(s: &str) -> Option<Self> {
        match s {
            "scheduled" => Some(RolloverKind::Scheduled),
            "early" => Some(RolloverKind::Early),
            "retracted" => Some(RolloverKind::Retracted),
            "unannounced" => Some(RolloverKind::Unannounced),
            _ => None,
        }
    }
}

/// One observed rollover of one window, tied to the poll that revealed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowRollover {
    /// Account the window belongs to.
    pub account: AccountId,
    /// Window that rolled over.
    pub window: WindowId,
    /// Poll that first reported the change. With `window`, the primary key.
    pub poll: PollId,
    /// When that poll completed — the earliest instant we can prove the new
    /// window was already running.
    pub observed_at: DateTime<Utc>,
    /// What kind of rollover this was.
    pub kind: RolloverKind,
    /// `reset_at` before the change.
    pub prev_reset_at: Option<DateTime<Utc>>,
    /// `reset_at` after it.
    pub new_reset_at: Option<DateTime<Utc>>,
    /// `used` before the change, in the window's unit.
    pub prev_used: f64,
    /// `used` after it.
    pub new_used: f64,
    /// When the poll this was compared against completed — the latest instant
    /// the *old* window was still provably running.
    ///
    /// With `observed_at` it brackets the reset. In normal running that
    /// bracket is one poll interval wide and can be ignored; across a daemon
    /// outage it is days wide, and that width is the uncertainty every rate
    /// anchored to this reset inherits. Hiding it is what makes a pace
    /// computed after an outage diverge without saying so.
    ///
    /// `None` on rows written before the column existed; those read as a
    /// zero-width bracket at `observed_at`, which is the behaviour they had.
    pub prev_observed_at: Option<DateTime<Utc>>,
}

/// Where a window is known to have begun, and how precisely.
///
/// A reset is never seen happening, only inferred from two readings that
/// straddle it, so the honest answer is an interval rather than an instant.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ObservedStart {
    /// Latest instant the previous window was still provably running.
    pub not_before: DateTime<Utc>,
    /// Earliest instant the new window was provably running.
    pub not_after: DateTime<Utc>,
}

impl ObservedStart {
    /// The bracket implied by one rollover.
    pub fn from_rollover(rollover: &WindowRollover) -> Self {
        Self {
            not_before: rollover.prev_observed_at.unwrap_or(rollover.observed_at),
            not_after: rollover.observed_at,
        }
    }

    /// The single instant to compute against: the bracket's midpoint.
    ///
    /// Neither end is usable on its own. `not_before` under-reports the pace
    /// after an outage — the same fault this whole rule exists to fix, only
    /// smaller — while `not_after` can sit arbitrarily close to `now`, and a
    /// window that opened a moment ago divides by an elapsed fraction of
    /// nearly zero. The midpoint is wrong by at most half the bracket in
    /// either direction, which is the best a bracket allows.
    pub fn estimate(self) -> DateTime<Utc> {
        self.not_before + (self.not_after - self.not_before) / 2
    }

    /// How wide the bracket is — how much the estimate could be out by,
    /// doubled.
    pub fn uncertainty(self) -> Duration {
        self.not_after - self.not_before
    }
}

/// Compare two consecutive successful polls and report every window that
/// rolled over between them.
///
/// Windows present in only one of the two are skipped: a window appearing for
/// the first time has nothing to have rolled over from, and one that vanished
/// tells us about the provider's payload, not about a reset.
///
/// `prev_observed_at` is when the earlier of the two polls completed; it is
/// recorded on every rollover so the reset's instant is stored as the bracket
/// it actually is rather than as the moment we happened to notice.
pub fn detect(
    account: &AccountId,
    prev: &[QuotaWindow],
    next: &[QuotaWindow],
    poll: PollId,
    prev_observed_at: Option<DateTime<Utc>>,
    observed_at: DateTime<Utc>,
) -> Vec<WindowRollover> {
    let before: HashMap<&WindowId, &QuotaWindow> = prev.iter().map(|w| (&w.id, w)).collect();
    next.iter()
        .filter_map(|after| {
            let before = before.get(&after.id)?;
            let kind = classify(before, after, observed_at)?;
            Some(WindowRollover {
                account: account.clone(),
                window: after.id.clone(),
                poll,
                observed_at,
                kind,
                prev_reset_at: before.reset_at,
                new_reset_at: after.reset_at,
                prev_used: before.used,
                new_used: after.used,
                prev_observed_at,
            })
        })
        .collect()
}

/// Which rollover, if any, one window underwent between two polls.
fn classify(
    before: &QuotaWindow,
    after: &QuotaWindow,
    observed_at: DateTime<Utc>,
) -> Option<RolloverKind> {
    match (before.reset_at, after.reset_at) {
        (Some(prev), Some(new)) if new > prev + RESET_TOLERANCE => {
            // The old window was still supposed to be running when the new one
            // appeared, so the provider rolled it early.
            if prev > observed_at + RESET_TOLERANCE {
                Some(RolloverKind::Early)
            } else {
                Some(RolloverKind::Scheduled)
            }
        }
        (Some(prev), Some(new)) if new < prev - RESET_TOLERANCE => Some(RolloverKind::Retracted),
        // `reset_at` held still (or was never published), so the only evidence
        // left is the usage itself.
        _ => collapsed(before, after).then_some(RolloverKind::Unannounced),
    }
}

/// Whether usage fell far enough to be a reset the provider did not announce.
///
/// Requires both readings to be expressible as a ratio; without a limit or a
/// percentage unit there is no scale to judge "far enough" against, and a bare
/// token count falling is not evidence of anything.
///
/// Both readings are utilizations in `0.0..=1.0`, not raw `used` values: the
/// thresholds are a fraction of the whole window, so a caller holding a
/// percentage or a message count has to scale it first.
///
/// Exported so that a decision *recorded* under this rule can be re-judged
/// under it later. A stored rollover row outlives the binary that wrote it,
/// and `teiryod` re-checks each one on startup before replaying it as a pace
/// anchor — rows written before the ratio guard existed were classified by a
/// bare drop, and replaying those would anchor a window to an instant nothing
/// restarted at.
///
/// It is deliberately *not* a rule every consumer shares. `teiryo`'s
/// `recent_pace` asks a narrower question — whether to cut a series at a
/// falling reading — and answers it with its own, much smaller epsilon,
/// because a restart too small for this rule to see is one it must still not
/// average across. The two are different questions about the same event, and
/// this doc used to claim otherwise.
pub fn is_collapse(prev: f64, new: f64) -> bool {
    prev - new >= MIN_RESET_DROP && new <= prev * RESET_COLLAPSE_RATIO
}

fn collapsed(before: &QuotaWindow, after: &QuotaWindow) -> bool {
    match (before.utilization(), after.utilization()) {
        (Some(prev), Some(new)) => is_collapse(prev, new),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    use crate::domain::{QuotaUnit, ResetKind, WindowScope};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap()
    }

    fn account() -> AccountId {
        AccountId::from("claude:test")
    }

    /// A 5-hour percent window `used`% consumed, resetting at `reset_at`.
    fn window(used: f64, reset_at: Option<DateTime<Utc>>) -> QuotaWindow {
        QuotaWindow {
            id: WindowId::from("session_5h"),
            label: "Session — 5 hour".to_owned(),
            scope: WindowScope::AccountWide,
            reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(5 * 3600)),
            unit: QuotaUnit::Percent,
            used,
            limit: Some(100.0),
            reset_at,
        }
    }

    fn detected(before: QuotaWindow, after: QuotaWindow) -> Option<RolloverKind> {
        let found = detect(
            &account(),
            &[before],
            &[after],
            PollId::generate(),
            Some(now() - Duration::minutes(3)),
            now(),
        );
        found.first().map(|r| r.kind)
    }

    #[test]
    fn a_window_past_its_reset_rolls_on_schedule() {
        // The old reset was a minute ago, so the new window is simply the next
        // one along.
        let before = window(88.0, Some(now() - Duration::minutes(1)));
        let after = window(3.0, Some(now() + Duration::hours(5)));
        assert_eq!(detected(before, after), Some(RolloverKind::Scheduled));
    }

    #[test]
    fn a_reset_that_was_still_an_hour_out_is_early() {
        let before = window(88.0, Some(now() + Duration::hours(1)));
        let after = window(3.0, Some(now() + Duration::hours(6)));
        assert_eq!(detected(before, after), Some(RolloverKind::Early));
    }

    #[test]
    fn a_reset_inside_the_tolerance_is_not_early() {
        // One minute out is clock skew, not a provider decision.
        let before = window(88.0, Some(now() + Duration::minutes(1)));
        let after = window(3.0, Some(now() + Duration::hours(5)));
        assert_eq!(detected(before, after), Some(RolloverKind::Scheduled));
    }

    /// Providers recompute `reset_at` per request, so an unmoved window still
    /// reports an instant that drifts sub-second between polls. Without a
    /// tolerance on the forward comparison every single poll is a rollover.
    #[test]
    fn sub_tolerance_forward_drift_is_not_a_rollover() {
        let reset = now() + Duration::hours(16);
        let before = window(77.0, Some(reset));
        let after = window(77.0, Some(reset + Duration::milliseconds(686)));
        assert_eq!(detected(before, after), None);
    }

    /// The tolerance is symmetric: the same drift backwards is also noise.
    #[test]
    fn sub_tolerance_backward_drift_is_not_a_rollover() {
        let reset = now() + Duration::hours(16);
        let before = window(77.0, Some(reset));
        let after = window(77.0, Some(reset - Duration::milliseconds(686)));
        assert_eq!(detected(before, after), None);
    }

    /// Drift must not mask a real reset the provider failed to announce.
    #[test]
    fn a_collapse_under_drift_is_still_unannounced() {
        let reset = now() + Duration::hours(16);
        let before = window(77.0, Some(reset));
        let after = window(2.0, Some(reset + Duration::milliseconds(686)));
        assert_eq!(detected(before, after), Some(RolloverKind::Unannounced));
    }

    #[test]
    fn a_reset_moving_backwards_is_retracted() {
        let before = window(40.0, Some(now() + Duration::hours(4)));
        let after = window(40.0, Some(now() + Duration::hours(1)));
        assert_eq!(detected(before, after), Some(RolloverKind::Retracted));
    }

    #[test]
    fn usage_collapsing_without_a_reset_move_is_unannounced() {
        let reset = Some(now() + Duration::hours(2));
        assert_eq!(
            detected(window(60.0, reset), window(2.0, reset)),
            Some(RolloverKind::Unannounced)
        );
    }

    #[test]
    fn a_small_correction_is_not_a_rollover() {
        // Exactly the kind of downward revision that must not break a series.
        let reset = Some(now() + Duration::hours(2));
        assert_eq!(detected(window(60.0, reset), window(58.0, reset)), None);
    }

    #[test]
    fn a_reset_is_judged_by_what_is_left_not_by_how_much_went() {
        let reset = Some(now() + Duration::hours(2));
        // A fifth of a weekly window handed back is a reset, however modest
        // the absolute drop. This is the case the old 25-point threshold could
        // not see, and the reason the pace row stayed anchored to a window
        // that had already ended.
        assert_eq!(
            detected(window(20.0, reset), window(0.0, reset)),
            Some(RolloverKind::Unannounced)
        );
        // Halving a number this small is a revision, not a restart: the ratio
        // alone is trivially satisfied down here, so the floor is what decides.
        assert_eq!(detected(window(0.4, reset), window(0.1, reset)), None);
        assert_eq!(detected(window(5.0, reset), window(2.0, reset)), None);
        // A big drop that still leaves most of the window standing is a
        // correction, not a restart — the ratio, not the drop, decides.
        assert_eq!(detected(window(90.0, reset), window(50.0, reset)), None);
    }

    /// Both constants are a strict/non-strict choice, and every test above
    /// straddles them without landing on either. A rule written with `>` and
    /// `<` instead of `>=` and `<=` passes all of them and fails all of these.
    #[test]
    fn a_fall_exactly_on_either_threshold_is_a_reset() {
        let reset = Some(now() + Duration::hours(2));
        // 10% → 5% sits on *both* at once: the drop is exactly
        // `MIN_RESET_DROP` and what is left is exactly
        // `RESET_COLLAPSE_RATIO` of what was there.
        assert_eq!(
            detected(window(10.0, reset), window(5.0, reset)),
            Some(RolloverKind::Unannounced)
        );
        // Exactly on the ratio, well clear of the floor, so only the ratio's
        // boundary is under test here.
        assert_eq!(
            detected(window(90.0, reset), window(45.0, reset)),
            Some(RolloverKind::Unannounced)
        );
        // The shrunk case the checked-in regression seed stood for: a window
        // that climbed to exactly five points and restarted. The seed itself
        // no longer reproduces it — the generator's peak floor moved to 0.08,
        // which cannot produce a 0.05 peak at all — so the case is written out
        // rather than left to an RNG seed that has drifted off it.
        assert_eq!(
            detected(window(5.0, reset), window(0.0, reset)),
            Some(RolloverKind::Unannounced)
        );
    }

    /// The other side of each threshold, isolated: one case that only the
    /// floor rejects and one that only the ratio rejects.
    ///
    /// Without the isolation a single guard could be deleted and the suite
    /// would still fail on the *other* one, which says nothing about which.
    #[test]
    fn a_fall_one_step_past_either_threshold_is_not() {
        let reset = Some(now() + Duration::hours(2));
        // Exactly on the ratio and a hair under the floor: 8% → 4% gives back
        // half of what was there, and half of a number this small is under
        // five points. The floor alone rejects it.
        assert_eq!(detected(window(8.0, reset), window(4.0, reset)), None);
        // Forty-four points is far over the floor, and 46% left is a hair over
        // half of 90%. The ratio alone rejects it.
        assert_eq!(detected(window(90.0, reset), window(46.0, reset)), None);
    }

    #[test]
    fn an_observed_start_is_the_middle_of_the_bracket_it_was_seen_in() {
        let observed_at = now();
        let found = detect(
            &account(),
            &[window(60.0, Some(observed_at + Duration::hours(2)))],
            &[window(0.0, Some(observed_at + Duration::hours(2)))],
            PollId::generate(),
            Some(observed_at - Duration::hours(4)),
            observed_at,
        );
        let start = ObservedStart::from_rollover(&found[0]);
        assert_eq!(start.uncertainty(), Duration::hours(4));
        assert_eq!(start.estimate(), observed_at - Duration::hours(2));
    }

    #[test]
    fn a_rollover_with_no_recorded_predecessor_brackets_nothing() {
        // How a row written before the column existed reads: the instant we
        // noticed, with no claim about how long before that it happened.
        let found = detect(
            &account(),
            &[window(60.0, Some(now() + Duration::hours(2)))],
            &[window(0.0, Some(now() + Duration::hours(2)))],
            PollId::generate(),
            None,
            now(),
        );
        let start = ObservedStart::from_rollover(&found[0]);
        assert_eq!(start.uncertainty(), Duration::zero());
        assert_eq!(start.estimate(), now());
    }

    #[test]
    fn usage_rising_is_never_a_rollover() {
        let reset = Some(now() + Duration::hours(2));
        assert_eq!(detected(window(20.0, reset), window(55.0, reset)), None);
    }

    #[test]
    fn a_window_without_a_reset_instant_still_reports_a_collapse() {
        // No `reset_at` to compare, but the usage evidence stands on its own.
        assert_eq!(
            detected(window(70.0, None), window(1.0, None)),
            Some(RolloverKind::Unannounced)
        );
    }

    #[test]
    fn an_unmeasurable_window_reports_nothing() {
        // Tokens with no published limit: the drop has no scale to be judged
        // against, so it is not evidence of a reset.
        let mut before = window(9_000.0, None);
        before.unit = QuotaUnit::Tokens;
        before.limit = None;
        let mut after = before.clone();
        after.used = 5.0;
        assert_eq!(detected(before, after), None);
    }

    #[test]
    fn windows_seen_only_once_are_skipped() {
        let existing = window(10.0, Some(now() + Duration::hours(1)));
        let mut fresh = window(0.0, Some(now() + Duration::hours(5)));
        fresh.id = WindowId::from("weekly");
        // `fresh` has no predecessor and `existing` has no successor.
        assert!(detect(
            &account(),
            &[existing],
            &[fresh],
            PollId::generate(),
            Some(now() - Duration::minutes(3)),
            now()
        )
        .is_empty());
    }

    #[test]
    fn every_rolled_window_in_one_poll_is_reported() {
        let poll = PollId::generate();
        let mut weekly_before = window(50.0, Some(now() - Duration::minutes(1)));
        weekly_before.id = WindowId::from("weekly");
        let mut weekly_after = window(0.0, Some(now() + Duration::days(7)));
        weekly_after.id = WindowId::from("weekly");

        let found = detect(
            &account(),
            &[
                window(88.0, Some(now() + Duration::hours(2))),
                weekly_before,
            ],
            &[window(1.0, Some(now() + Duration::hours(5))), weekly_after],
            poll,
            Some(now() - Duration::minutes(3)),
            now(),
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].kind, RolloverKind::Early);
        assert_eq!(found[0].prev_used, 88.0);
        assert_eq!(found[1].kind, RolloverKind::Scheduled);
        assert!(found
            .iter()
            .all(|r| r.poll == poll && r.account == account()));
    }

    #[test]
    fn only_scheduled_rollovers_are_unsurprising() {
        assert!(!RolloverKind::Scheduled.is_surprise());
        for kind in [
            RolloverKind::Early,
            RolloverKind::Retracted,
            RolloverKind::Unannounced,
        ] {
            assert!(kind.is_surprise());
        }
    }

    #[test]
    fn an_unannounced_drop_is_not_a_window_boundary() {
        assert!(!RolloverKind::Unannounced.is_boundary());
        assert!(RolloverKind::Scheduled.is_boundary());
        assert!(RolloverKind::Early.is_boundary());
    }

    #[test]
    fn kind_strings_round_trip() {
        for kind in [
            RolloverKind::Scheduled,
            RolloverKind::Early,
            RolloverKind::Retracted,
            RolloverKind::Unannounced,
        ] {
            assert_eq!(RolloverKind::from_column(kind.as_str()), Some(kind));
        }
        assert_eq!(RolloverKind::from_column("nonsense"), None);
    }
}

/// Property tests for reset detection against a generated ground truth.
///
/// The invariant all of these rest on: inside one window instance `used` only
/// ever climbs. A reading lower than the one before it therefore was not
/// revised — the window restarted. These generate a series whose reset
/// instants are *known*, and hold the detector to them.
#[cfg(test)]
mod properties {
    use super::*;
    use chrono::TimeZone;
    use proptest::prelude::*;

    use crate::domain::{QuotaUnit, ResetKind, WindowScope};

    /// One generated window instance: when it opened, and how far usage got
    /// before the window ended.
    #[derive(Debug, Clone)]
    struct Run {
        opened_at: DateTime<Utc>,
        /// Utilization at each reading, oldest first, non-decreasing.
        used: Vec<f64>,
    }

    /// A generated history of one window: consecutive runs, the readings taken
    /// of them, and whether the provider moved `reset_at` at each boundary.
    #[derive(Debug, Clone)]
    struct Scenario {
        span: Duration,
        cadence: Duration,
        runs: Vec<Run>,
        /// Whether `reset_at` advances with each reset. `false` is the case
        /// this change is about: the provider restarts the quota and leaves
        /// the published instant where it was.
        announced: bool,
        /// When announced, whether each run publishes its own real end rather
        /// than a distant one.
        ///
        /// This is what separates `Scheduled` from `Early`. With a `reset_at`
        /// a whole span out, the old instant is always still in the future
        /// when the next window appears, so `classify` can only ever answer
        /// `Early` — which is why every announced rollover the generator used
        /// to produce was one, and `Scheduled` was unreachable from here.
        punctual: bool,
        /// Sub-tolerance drift in the published instant, in seconds, applied
        /// with alternating sign.
        ///
        /// Providers recompute `reset_at` per request, so it moves a little
        /// between polls with nothing having happened — the case
        /// `RESET_TOLERANCE` exists for. The generator used to emit instants
        /// that were byte-identical across every reading, so a detector with
        /// no tolerance at all passed: consecutive readings now differ by
        /// twice this, which the bound keeps under the tolerance.
        jitter: i64,
    }

    impl Scenario {
        /// The readings, oldest first, as the daemon would have polled them,
        /// each paired with the instant it was taken.
        fn readings(&self) -> Vec<(DateTime<Utc>, QuotaWindow)> {
            let mut out = Vec::new();
            for (r, run) in self.runs.iter().enumerate() {
                // Announced: each run publishes its own end — its real one when
                // punctual, so the boundary arrives with the old instant
                // already reached. Silent: every run keeps reporting the first
                // one's, which is the whole defect.
                let reset_at = match (self.announced, self.punctual) {
                    (true, true) => run.opened_at + self.cadence * (run.used.len() as i32),
                    (true, false) => run.opened_at + self.span,
                    (false, _) => self.runs[0].opened_at + self.span,
                };
                for (k, used) in run.used.iter().enumerate() {
                    // Alternating, so consecutive readings differ by twice the
                    // jitter — under `RESET_TOLERANCE`, and far enough under
                    // that a punctual boundary still clears it.
                    let drift = if (r + k) % 2 == 0 {
                        self.jitter
                    } else {
                        -self.jitter
                    };
                    out.push((
                        run.opened_at + self.cadence * (k as i32),
                        QuotaWindow {
                            id: WindowId::from("w"),
                            label: "w".to_owned(),
                            scope: WindowScope::AccountWide,
                            reset_kind: ResetKind::Rolling(
                                self.span.to_std().expect("positive span"),
                            ),
                            unit: QuotaUnit::Percent,
                            used: used * 100.0,
                            limit: Some(100.0),
                            reset_at: Some(reset_at + Duration::seconds(drift)),
                        },
                    ));
                }
            }
            out.sort_by_key(|(ts, _)| *ts);
            out
        }

        /// Each instant at which a run actually gave way to the next, as the
        /// bracket between the last reading of the old and the first of the new.
        fn true_resets(&self) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
            self.runs
                .windows(2)
                .map(|pair| {
                    let last_old =
                        pair[0].opened_at + self.cadence * (pair[0].used.len() as i32 - 1);
                    (last_old, pair[1].opened_at)
                })
                .collect()
        }

        /// Every rollover the detector finds, walking the readings pairwise the
        /// way the daemon does.
        fn detected(&self) -> Vec<WindowRollover> {
            self.readings()
                .windows(2)
                .flat_map(|pair| {
                    detect(
                        &AccountId::from("a"),
                        std::slice::from_ref(&pair[0].1),
                        std::slice::from_ref(&pair[1].1),
                        PollId::generate(),
                        Some(pair[0].0),
                        pair[1].0,
                    )
                })
                .collect()
        }
    }

    /// A non-decreasing ramp of `len` readings from `base` up to exactly
    /// `peak`.
    ///
    /// `base` is what the window restarts *from*. It used to be zero always,
    /// which made every generated reset a fall to nothing — and a fall to
    /// nothing satisfies `new <= prev * RESET_COLLAPSE_RATIO` as `0 <=
    /// anything`, so the ratio was never the thing deciding.
    fn ramp(len: usize, base: f64, peak: f64) -> Vec<f64> {
        let step = (peak - base) / (len - 1) as f64;
        (0..len).map(|k| base + step * k as f64).collect()
    }

    /// Scenarios with `runs` consecutive window instances. Each run restarts at
    /// zero and climbs to its own peak; that peak is how much usage vanishes at
    /// the reset, and so is exactly what decides whether an absolute threshold
    /// can see the reset at all.
    fn scenario(runs: usize) -> impl Strategy<Value = Scenario> {
        (
            // 5 hours to 14 days: the two shapes Claude publishes, and beyond.
            5i64 * 3600..14 * 24 * 3600i64,
            60i64..600i64,
            // A peak floor clear of `MIN_RESET_DROP`, so every generated reset
            // is one a correct detector is obliged to report rather than one
            // it is entitled to read as a correction. The third number is how
            // much of the allowed leftover the *next* run restarts from.
            prop::collection::vec((4usize..40, 0.08f64..1.0, 0.5f64..1.0), runs),
            any::<bool>(),
            any::<bool>(),
            // Half of `RESET_TOLERANCE` would put a punctual boundary's own
            // move inside the tolerance too; a third leaves both sides clear.
            0i64..=40,
        )
            .prop_map(
                |(span_secs, cadence_secs, shapes, announced, punctual, jitter)| {
                    let cadence = Duration::seconds(cadence_secs);
                    let mut opened_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
                    let mut out = Vec::new();
                    let mut base = 0.0f64;
                    for (len, peak, leftover) in shapes {
                        // Kept below this run's own peak, or it would not
                        // climb — but only just below, because clamping hard
                        // here would undo the leftover the previous run's
                        // reset was chosen to leave.
                        let from = base.min(peak * 0.9);
                        out.push(Run {
                            opened_at,
                            used: ramp(len, from, peak),
                        });
                        // What the *next* run restarts from: comfortably inside
                        // both conditions a reset has to satisfy, so the ratio
                        // is exercised against a real number instead of being
                        // trivially true against zero — and drawn from the
                        // upper half of what is allowed, so the ratio rather
                        // than the floor is usually the binding condition.
                        // Small peaks leave no
                        // room for a partial reset and fall back to zero, which
                        // is correct — five points cannot vanish out of eight
                        // and still leave half standing.
                        // Literals, deliberately, not the constants under
                        // test. Sizing the generator from `MIN_RESET_DROP` and
                        // `RESET_COLLAPSE_RATIO` would move the ground truth
                        // with the rule: tightening either constant would also
                        // tighten what the generator produces, and the change
                        // would be invisible to every property here.
                        let ceiling = (peak - 0.075).min(peak * 0.475);
                        base = (leftover * ceiling).max(0.0);
                        opened_at += cadence * (len as i32);
                    }
                    Scenario {
                        span: Duration::seconds(span_secs),
                        cadence,
                        runs: out,
                        announced,
                        punctual,
                        jitter,
                    }
                },
            )
    }

    /// What the generator actually reaches, sampled and asserted.
    ///
    /// Both of these were unreachable, and neither fact was visible from a
    /// green suite. Every announced boundary classified as `Early`, because
    /// the published instant was always a whole span out when the next window
    /// appeared. And every reset fell to exactly zero, which satisfies
    /// `new <= prev * RESET_COLLAPSE_RATIO` as `0 <= anything` — so the ratio
    /// never decided anything and `MIN_RESET_DROP` was the only live guard.
    ///
    /// A generator that cannot produce a case is a property that cannot test
    /// it, which is why the reach is pinned here rather than left to a comment.
    #[test]
    fn the_generator_reaches_both_announced_kinds_and_partial_resets() {
        use proptest::strategy::ValueTree;
        use proptest::test_runner::TestRunner;

        let mut runner = TestRunner::deterministic();
        let strategy = scenario(3);
        let mut kinds: Vec<RolloverKind> = Vec::new();
        let mut partial = false;
        for _ in 0..512 {
            let s = strategy
                .new_tree(&mut runner)
                .expect("a scenario")
                .current();
            for r in s.detected() {
                if !kinds.contains(&r.kind) {
                    kinds.push(r.kind);
                }
                // A reset that left something behind is the only kind that
                // puts the ratio guard in charge of the answer.
                partial |= r.new_used > 0.0;
            }
        }
        for wanted in [
            RolloverKind::Scheduled,
            RolloverKind::Early,
            RolloverKind::Unannounced,
        ] {
            assert!(
                kinds.contains(&wanted),
                "the generator never produced {wanted:?} in 512 scenarios; saw {kinds:?}"
            );
        }
        assert!(partial, "every generated reset still falls to exactly zero");
    }

    proptest! {
        /// **P1 — the ground truth is what it claims to be.** Readings are
        /// strictly ordered in time and non-decreasing inside each run, and the
        /// only falls are at the generated reset instants. Without this the
        /// other two properties would be evidence about the generator rather
        /// than about the detector.
        #[test]
        fn the_generated_history_falls_only_at_its_own_resets(scenario in scenario(3)) {
            let readings = scenario.readings();
            let resets: Vec<_> = scenario.true_resets().iter().map(|(_, to)| *to).collect();
            for pair in readings.windows(2) {
                let (prev_ts, prev) = &pair[0];
                let (ts, next) = &pair[1];
                prop_assert!(ts > prev_ts, "readings out of order at {ts}");
                if next.used < prev.used {
                    prop_assert!(resets.contains(ts), "usage fell at {ts}, which is no reset");
                }
            }
        }

        /// **P2 — every reset is observed.** A window that restarts from zero
        /// after climbing to `peak` reset, however small `peak` was, and the
        /// detector must report it between the two readings that straddle it.
        ///
        /// Red before the change that introduced it: the rule was an absolute
        /// drop of a quarter of the window, so a silent weekly reset from 20%
        /// to 0 was never recorded — and a reset nobody recorded is a window
        /// start nobody can correct.
        #[test]
        fn every_reset_is_observed(scenario in scenario(3)) {
            let found = scenario.detected();
            for (from, to) in scenario.true_resets() {
                prop_assert!(
                    found.iter().any(|r| r.observed_at == to),
                    "reset in ({from}, {to}] went unobserved; detected: {:?}",
                    found.iter().map(|r| (r.observed_at, r.kind)).collect::<Vec<_>>(),
                );
            }
        }

        /// **P3 — no phantom resets.** One run climbing monotonically is one
        /// window: nothing inside it may read as a boundary. This is the guard
        /// on relaxing the threshold — a rule sensitive enough for P2 must
        /// still not fire on an ordinary ramp.
        #[test]
        fn a_single_climbing_run_holds_no_resets(scenario in scenario(1)) {
            let found = scenario.detected();
            prop_assert!(
                found.is_empty(),
                "invented {} reset(s) inside one window: {:?}",
                found.len(),
                found.iter().map(|r| (r.observed_at, r.kind)).collect::<Vec<_>>(),
            );
        }
    }

    /// The reported symptom, as a case that does not depend on the generator:
    /// a weekly window sitting at 20% restarts, and the provider leaves
    /// `reset_at` alone. Nothing about that is ambiguous — a fifth of the
    /// week's budget did not get refunded by a rounding fix.
    #[test]
    fn a_silent_weekly_reset_from_twenty_percent_is_observed() {
        let reset_at = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        let weekly = |used: f64| QuotaWindow {
            id: WindowId::from("weekly"),
            label: "Weekly — all models".to_owned(),
            scope: WindowScope::AccountWide,
            reset_kind: ResetKind::Rolling(std::time::Duration::from_secs(7 * 24 * 3600)),
            unit: QuotaUnit::Percent,
            used,
            limit: Some(100.0),
            reset_at: Some(reset_at),
        };
        let observed_at = Utc.with_ymd_and_hms(2026, 8, 25, 9, 0, 0).unwrap();
        let found = detect(
            &AccountId::from("claude:test"),
            &[weekly(20.0)],
            &[weekly(0.0)],
            PollId::generate(),
            Some(observed_at - Duration::minutes(3)),
            observed_at,
        );
        assert_eq!(
            found.first().map(|r| r.kind),
            Some(RolloverKind::Unannounced),
        );
    }
}
