//! One poll task per (provider, account): interval ticks and injected manual
//! triggers share a single loop, so `PollNow` needs no separate code path.

use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use teiryo_core::{
    Account, PollEvent, PollId, PollOutcome, PollTrigger, ProbeError, ProviderAdapter,
};
use tokio::sync::{mpsc, watch};

use crate::state::Daemon;

/// What the config currently says about one account's polling. Delivered over
/// a `watch` rather than captured by value, so a `config.toml` edit reaches a
/// running task instead of waiting for a daemon restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Whether this account's provider is polled at all.
    pub enabled: bool,
    /// Base cadence; actual polls jitter ±10% around it.
    pub interval: Duration,
}

/// How much the wait grows with each consecutive rate-limited poll.
const BACKOFF_FACTOR: u32 = 2;

/// Ceiling on a backed-off interval. Past an hour the throttle has stopped
/// being a courtesy to the provider and started being an outage: the user
/// opened teiryo to watch a quota, and a window can roll over entirely inside
/// a longer gap. A `Retry-After` the provider sent itself is still honored
/// beyond this — that is the provider's own answer, not our guess.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// How far a poll got before it produced its outcome.
///
/// Not on the wire, and deliberately not part of [`PollOutcome`]: a client
/// needs to know *what* happened, while the scheduler needs to know *where it
/// stopped*. The two questions have different answers for the same outcome —
/// an `AuthError` raised resolving the local credential and one raised by the
/// provider rejecting it are the same message and entirely different events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The local credential could not be resolved, so nothing was sent.
    Credential,
    /// A request reached the provider, or failed trying to.
    Provider,
}

/// One completed poll: its outcome, and how far it got.
#[derive(Debug, Clone, PartialEq)]
struct Polled {
    /// What the client is told.
    outcome: PollOutcome,
    /// What the scheduler reasons about.
    stage: Stage,
}

/// Consecutive rate-limited polls, and the wait they buy.
///
/// A 429 is the provider saying we ask too often. Continuing at the configured
/// cadence would keep the account limited for longer and lose the very
/// readings the cadence exists to collect, so each consecutive rate limit
/// doubles the wait. Any other outcome clears it: the throttle answers the
/// provider's current mood, and must not outlive it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Throttle {
    /// Consecutive rate-limited polls; `0` when the last poll was not limited.
    strikes: u32,
    /// The provider's own `Retry-After`, when it sent one.
    retry_after: Option<Duration>,
}

impl Throttle {
    /// Fold in the poll that just finished.
    fn record(&mut self, polled: &Polled) {
        // A poll that stopped at the local credential never reached the
        // provider, so it learned nothing about the provider's mood and must
        // not speak for it — neither adding a strike nor clearing one. Letting
        // it clear one silently discarded a backoff the provider had asked
        // for: 429s built a long wait, the access token then expired, and the
        // next poll failed without sending a byte and reset the strikes. The
        // user re-logged in hours later and the daemon resumed at full cadence
        // straight back into the rate limit it had already been told about.
        if polled.stage == Stage::Credential {
            return;
        }
        match &polled.outcome {
            PollOutcome::RateLimited { retry_after } => {
                self.strikes = self.strikes.saturating_add(1);
                self.retry_after = *retry_after;
            }
            _ => *self = Self::default(),
        }
    }

    /// Whether polls are currently being held back.
    fn is_throttled(self) -> bool {
        self.strikes > 0
    }

    /// The wait before the next scheduled poll, given the configured cadence.
    ///
    /// Never shorter than that cadence — a backoff that polled *more* often
    /// would be backwards — and never shorter than a `Retry-After` the
    /// provider sent, which is its own answer to the same question.
    fn delay(self, base: Duration) -> Duration {
        if self.strikes == 0 {
            return base;
        }
        // 2^31 is the last power that fits the `u32` multiplier, and it is
        // already far past the cap below — so clamping the exponent loses
        // nothing but the overflow.
        let doublings = (self.strikes - 1).min(31);
        // `.max(base)` last, not a `clamp`: a configured cadence already
        // longer than the ceiling is the user's choice, and capping it would
        // make a rate limit speed polling *up*.
        let scaled = base
            .checked_mul(BACKOFF_FACTOR.saturating_pow(doublings))
            .unwrap_or(MAX_BACKOFF)
            .min(MAX_BACKOFF)
            .max(base);
        scaled.max(self.retry_after.unwrap_or_default())
    }
}

/// Spawn the poll task for one (provider, account). Returns the manual
/// trigger sender; the task itself runs until shutdown. Must be called from
/// within a `tokio::task::LocalSet`.
pub fn spawn_poller(
    daemon: &Daemon,
    adapter: Rc<dyn ProviderAdapter>,
    account: Account,
    mut schedule_rx: watch::Receiver<Schedule>,
) -> mpsc::UnboundedSender<PollTrigger> {
    let (tx, mut rx) = mpsc::unbounded_channel::<PollTrigger>();
    let daemon = daemon.clone();
    let mut shutdown_rx = daemon.shutdown_tx.subscribe();
    tokio::task::spawn_local(async move {
        // Tracked rather than assumed: a provider disabled in config.toml must
        // not poll at startup, and re-enabling one should show a reading
        // immediately rather than after a full interval of nothing.
        let mut was_enabled = false;
        // Rate limiting is a property of the credential, not of the config, so
        // it lives with the task rather than travelling over `schedule_rx`.
        let mut throttle = Throttle::default();
        loop {
            let schedule = *schedule_rx.borrow_and_update();
            if !schedule.enabled {
                was_enabled = false;
                // A parked provider is not being rate limited by anyone; the
                // strikes would otherwise apply to a poll hours later.
                throttle = Throttle::default();
                tokio::select! {
                    _ = schedule_rx.changed() => continue,
                    _ = shutdown_rx.changed() => break,
                }
            }
            if !was_enabled {
                was_enabled = true;
                let polled =
                    poll_once(&daemon, adapter.as_ref(), &account, PollTrigger::Startup).await;
                throttle.record(&polled);
                continue;
            }
            let interval = throttle.delay(schedule.interval);
            if throttle.is_throttled() {
                tracing::warn!(
                    provider = %account.provider,
                    account = %account.id,
                    strikes = throttle.strikes,
                    backoff_secs = interval.as_secs(),
                    "rate limited — throttling polls"
                );
            }
            // Clients count down to the next poll from this; publishing the
            // configured cadence while throttled would show a countdown that
            // hits zero and then sits there.
            daemon.set_reported_interval(&account.id, interval);
            let sleep = tokio::time::sleep(jittered(interval));
            tokio::select! {
                _ = sleep => {
                    let polled =
                        poll_once(&daemon, adapter.as_ref(), &account, PollTrigger::Scheduled).await;
                    throttle.record(&polled);
                }
                // A manual trigger is not held back by the throttle: the user
                // asked for this one, and if it succeeds the backoff was stale
                // and clears.
                Some(trigger) = rx.recv() => {
                    let polled = poll_once(&daemon, adapter.as_ref(), &account, trigger).await;
                    throttle.record(&polled);
                }
                // Re-arms the sleep against the new cadence. A shortened
                // interval therefore takes effect now, not after the old
                // (possibly hour-long) one finally elapses.
                _ = schedule_rx.changed() => {}
                _ = shutdown_rx.changed() => break,
            }
        }
    });
    tx
}

/// Run one poll: resolve credential, probe, parse; persist and publish the
/// resulting event whatever the outcome. The result is handed back so the
/// caller can adjust its backoff.
async fn poll_once(
    daemon: &Daemon,
    adapter: &dyn ProviderAdapter,
    account: &Account,
    trigger: PollTrigger,
) -> Polled {
    let started = Instant::now();
    let polled = poll_outcome(adapter, account).await;
    let event = PollEvent {
        id: PollId::generate(),
        ts: Utc::now(),
        provider: adapter.id(),
        account: account.id.clone(),
        trigger,
        outcome: polled.outcome,
        latency_ms: started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
    };
    if let Some(err) = event.outcome.error_message() {
        tracing::warn!(provider = %event.provider, account = %event.account, error = err, "poll failed");
    } else {
        tracing::debug!(provider = %event.provider, account = %event.account, latency_ms = event.latency_ms, "poll ok");
    }
    daemon.record_event(&event);
    Polled {
        outcome: event.outcome,
        stage: polled.stage,
    }
}

async fn poll_outcome(adapter: &dyn ProviderAdapter, account: &Account) -> Polled {
    // The one place a poll stops without reaching the provider. Everything
    // below it has sent a request, whatever it came back with.
    let cred = match adapter.credential_for(account).await {
        Ok(c) => c,
        Err(e) => {
            return Polled {
                outcome: PollOutcome::AuthError(e.to_string()),
                stage: Stage::Credential,
            }
        }
    };
    let sent = |outcome| Polled {
        outcome,
        stage: Stage::Provider,
    };
    let raw = match adapter.probe(account, &cred).await {
        Ok(r) => r,
        Err(ProbeError::Auth(m)) => return sent(PollOutcome::AuthError(m)),
        Err(ProbeError::RateLimited { retry_after }) => {
            return sent(PollOutcome::RateLimited { retry_after })
        }
        Err(e @ (ProbeError::Network(_) | ProbeError::Provider(_))) => {
            return sent(PollOutcome::NetworkError(e.to_string()))
        }
    };
    match adapter.parse(&raw) {
        Ok(windows) => sent(PollOutcome::Success { windows }),
        Err(e) => sent(PollOutcome::SchemaDrift(e.to_string())),
    }
}

/// Base interval ±10%, recomputed every cycle (§14) so probes don't form a
/// fixed cadence. Randomness source is the subsecond clock — good enough.
fn jittered(base: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let spread = (base.as_millis() as u64 / 5).max(1); // 20% band
    let offset = nanos % spread;
    let low = base.as_millis() as u64 - spread / 2;
    Duration::from_millis(low + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rate limit, which by definition reached the provider.
    fn limited(retry_after: Option<u64>) -> Polled {
        reached(PollOutcome::RateLimited {
            retry_after: retry_after.map(Duration::from_secs),
        })
    }

    /// A poll that got a request out to the provider.
    fn reached(outcome: PollOutcome) -> Polled {
        Polled {
            outcome,
            stage: Stage::Provider,
        }
    }

    /// A poll that stopped at the local credential, having sent nothing.
    fn unusable_credential() -> Polled {
        Polled {
            outcome: PollOutcome::AuthError("token expired".into()),
            stage: Stage::Credential,
        }
    }

    const BASE: Duration = Duration::from_secs(180);

    #[test]
    fn an_unlimited_poll_waits_exactly_the_configured_cadence() {
        let mut t = Throttle::default();
        assert_eq!(t.delay(BASE), BASE);
        t.record(&reached(PollOutcome::Success { windows: vec![] }));
        assert!(!t.is_throttled());
        assert_eq!(t.delay(BASE), BASE);
    }

    #[test]
    fn each_consecutive_rate_limit_doubles_the_wait() {
        let mut t = Throttle::default();
        t.record(&limited(None));
        assert_eq!(t.delay(BASE), BASE);
        t.record(&limited(None));
        assert_eq!(t.delay(BASE), BASE * 2);
        t.record(&limited(None));
        assert_eq!(t.delay(BASE), BASE * 4);
    }

    #[test]
    fn the_backoff_stops_growing_at_the_ceiling() {
        let mut t = Throttle::default();
        for _ in 0..64 {
            t.record(&limited(None));
        }
        assert_eq!(t.delay(BASE), MAX_BACKOFF);
        // A cadence already past the ceiling stays where the user put it,
        // and the multiply cannot overflow on the way there.
        let huge = Duration::from_secs(u64::MAX / 2);
        assert_eq!(t.delay(huge), huge);
    }

    /// The throttle exists to poll *less*; a shorter wait would be backwards.
    #[test]
    fn a_backoff_is_never_shorter_than_the_configured_cadence() {
        let mut t = Throttle::default();
        t.record(&limited(Some(5)));
        assert_eq!(t.delay(BASE), BASE);
    }

    /// The provider's own answer wins over ours, in both directions.
    #[test]
    fn a_retry_after_is_honored_even_past_the_ceiling() {
        let mut t = Throttle::default();
        t.record(&limited(Some(7200)));
        assert_eq!(t.delay(BASE), Duration::from_secs(7200));
        assert!(Duration::from_secs(7200) > MAX_BACKOFF);
    }

    /// Otherwise a single 429 would demote an account for the rest of the
    /// daemon's life.
    #[test]
    fn any_other_outcome_clears_the_backoff() {
        let mut t = Throttle::default();
        t.record(&limited(Some(600)));
        t.record(&limited(Some(600)));
        assert!(t.is_throttled());
        t.record(&reached(PollOutcome::NetworkError("down".into())));
        assert!(!t.is_throttled());
        assert_eq!(t.delay(BASE), BASE);
    }

    /// The backoff answers the provider's mood, and only the provider can
    /// change it. A poll that died resolving the local credential sent no
    /// request, so it has no standing to say the rate limit is over — and
    /// clearing it here is how a re-login used to walk straight back into the
    /// 429s the daemon had already been warned about.
    #[test]
    fn a_credential_failure_does_not_clear_a_rate_limit_backoff() {
        let mut t = Throttle::default();
        t.record(&limited(Some(600)));
        t.record(&limited(Some(600)));
        let backed_off = t.delay(BASE);
        assert!(t.is_throttled());

        // Hours of an expired token, none of it addressed to the provider.
        for _ in 0..64 {
            t.record(&unusable_credential());
        }

        assert!(t.is_throttled(), "a local failure spoke for the provider");
        assert_eq!(t.delay(BASE), backed_off, "the backoff moved");
    }

    /// The other half of the same rule: a rejection *from* the provider is the
    /// provider talking, so it clears the strikes like any other reply.
    #[test]
    fn a_provider_rejection_still_clears_a_rate_limit_backoff() {
        let mut t = Throttle::default();
        t.record(&limited(Some(600)));
        assert!(t.is_throttled());
        t.record(&reached(PollOutcome::AuthError("401".into())));
        assert!(!t.is_throttled());
        assert_eq!(t.delay(BASE), BASE);
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        let base = Duration::from_secs(60);
        for _ in 0..100 {
            let j = jittered(base);
            assert!(j >= Duration::from_secs(54), "{j:?}");
            assert!(j <= Duration::from_secs(66), "{j:?}");
        }
    }
}
