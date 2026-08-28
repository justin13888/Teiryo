//! Detecting when a quota window rolled over, and whether that was a surprise.
//!
//! A rollover is read from `reset_at` **moving**, never from `used` falling: a
//! provider correction that lowers `used` mid-window is not a new window, and
//! treating it as one would split a chart series that never actually broke.
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
pub const RESET_TOLERANCE: Duration = Duration::seconds(120);

/// Utilization drop that counts as an unannounced reset rather than a
/// provider correction. A quarter of the window's capacity vanishing in one
/// poll interval is not a rounding fix.
pub const UNANNOUNCED_DROP: f64 = 0.25;

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
}

/// Compare two consecutive successful polls and report every window that
/// rolled over between them.
///
/// Windows present in only one of the two are skipped: a window appearing for
/// the first time has nothing to have rolled over from, and one that vanished
/// tells us about the provider's payload, not about a reset.
pub fn detect(
    account: &AccountId,
    prev: &[QuotaWindow],
    next: &[QuotaWindow],
    poll: PollId,
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
        (Some(prev), Some(new)) if new > prev => {
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
fn collapsed(before: &QuotaWindow, after: &QuotaWindow) -> bool {
    match (before.utilization(), after.utilization()) {
        (Some(prev), Some(new)) => prev - new > UNANNOUNCED_DROP,
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
        let found = detect(&account(), &[before], &[after], PollId::generate(), now());
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
        assert!(detect(&account(), &[existing], &[fresh], PollId::generate(), now()).is_empty());
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
