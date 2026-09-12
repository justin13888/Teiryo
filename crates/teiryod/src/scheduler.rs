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

/// Ceiling on the wait between re-resolving a credential that last failed to
/// resolve.
///
/// This is not a probe and costs the provider nothing — for the Claude adapter
/// it is a local file read — so it is fast enough to make a re-login feel
/// immediate without being a poll. The wait is the configured cadence when that
/// is shorter, so a user who asked to be polled every 10 s is never told about
/// their new login more slowly than that.
const CREDENTIAL_RECHECK_MAX: Duration = Duration::from_secs(30);

/// What the poll task does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The provider is disabled: wait for config to say otherwise.
    Park,
    /// Poll now, without waiting.
    PollNow,
    /// Re-resolve the credential after this long. Sends nothing.
    Recheck {
        /// How long to wait first.
        after: Duration,
    },
    /// Wait this long, then poll.
    Wait {
        /// How long to wait first.
        after: Duration,
    },
}

/// Why the task is or is not polling right now.
///
/// Two facts rather than one, because they answer to different people and
/// neither may overwrite the other: the throttle is the provider's verdict on
/// how often we may ask, and `credential_stale` is the local credential store's
/// verdict on whether we can ask at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Gate {
    /// The provider's rate limit, and the wait it bought.
    throttle: Throttle,
    /// The local credential last failed to resolve, so probing is pointless
    /// until it resolves again.
    credential_stale: bool,
}

/// A change in why the task is polling or not — worth exactly one log line.
///
/// Reported on the edge rather than on the level, because the level is what
/// used to be logged: a stall that lasted three hours wrote a warning for every
/// cycle inside it, and the 180th copy told nobody anything the first had not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    /// The credential stopped resolving; probes are paused until it does.
    Paused,
    /// The credential resolved again; polling resumes.
    Resumed,
    /// The provider began rate limiting; polls are being spaced out.
    Throttled,
}

impl Gate {
    /// Fold in the poll that just finished, reporting the edges it crossed.
    ///
    /// Two, because one poll can cross both: the poll that ends a pause is
    /// often the very one the provider then rate limits — a re-check resumes
    /// as soon as the credential resolves, without waiting out a backoff the
    /// pause did not clear. Reporting only the resumption would leave the long
    /// wait that follows it unexplained.
    fn record(&mut self, polled: &Polled) -> [Option<Transition>; 2] {
        let was_stale = self.credential_stale;
        let was_throttled = self.throttle.is_throttled();
        self.throttle.record(polled);
        self.credential_stale = polled.stage == Stage::Credential;

        let credential = match (was_stale, self.credential_stale) {
            (false, true) => Some(Transition::Paused),
            (true, false) => Some(Transition::Resumed),
            _ => None,
        };
        // Always `None` for a poll that stopped at the credential: that poll
        // leaves the throttle exactly as it was, so there is no edge to cross.
        let throttled =
            (self.throttle.is_throttled() && !was_throttled).then_some(Transition::Throttled);
        [credential, throttled]
    }

    /// A re-check resolved the credential, sending nothing.
    fn credential_ok(&mut self) -> Option<Transition> {
        let recovered = self.credential_stale;
        self.credential_stale = false;
        recovered.then_some(Transition::Resumed)
    }

    /// A parked provider owes nobody anything: it is not being rate limited,
    /// and whatever its credential did last is about to be re-established by
    /// the poll that re-enabling forces.
    fn park(&mut self) {
        *self = Self::default();
    }

    /// The next thing to do. Precedence is load-bearing: a disabled provider
    /// is excluded before the credential is ever considered, so parking is
    /// what stops a disabled account re-checking, rather than a guard
    /// somebody has to remember to write.
    fn next_step(self, schedule: Schedule, poll_immediately: bool) -> Step {
        if !schedule.enabled {
            return Step::Park;
        }
        if poll_immediately {
            return Step::PollNow;
        }
        if self.credential_stale {
            return Step::Recheck {
                after: schedule.interval.min(CREDENTIAL_RECHECK_MAX),
            };
        }
        Step::Wait {
            after: self.throttle.delay(schedule.interval),
        }
    }
}

/// The cadence clients should count down to for `step`, given the wait the
/// task will settle back to once it is polling normally again. `None` leaves
/// the published value alone because something else owns it.
///
/// A paused account reports `0`, the value `AccountStatus.poll_interval_secs`
/// already uses for "no next poll to count down to". Reporting the re-check
/// cadence instead would promise a poll that is not scheduled — nothing is
/// polled while paused, and the re-check may find nothing for hours.
///
/// `PollNow` reports `resting` rather than nothing, because the poll it is
/// about to run is what *wakes* the clients: `record_event` returns every open
/// `AwaitUpdate`, and the TUI's only refresh is that wakeup — it runs no timer
/// of its own. Leaving the published value alone here would hand the client
/// whichever cadence the previous step wrote, and the previous step on the
/// path this whole change exists to create is a pause, which writes `0`. A
/// freshly recovered account would have reported "no next poll" for a full
/// cadence, having just started polling again.
fn reported_interval(step: Step, resting: Duration) -> Option<Duration> {
    match step {
        Step::Recheck { .. } => Some(Duration::ZERO),
        Step::Wait { after } => Some(after),
        Step::PollNow => Some(resting),
        // The config's business: `apply_config` publishes `0` for a disabled
        // provider as it parks it.
        Step::Park => None,
    }
}

/// Say once what just changed. `paused_since` is the clock the resume line
/// reports against, started here and read here so the two lines cannot drift.
fn log_transition(
    account: &Account,
    transition: Transition,
    schedule: Schedule,
    gate: Gate,
    paused_since: &mut Option<Instant>,
) {
    match transition {
        Transition::Paused => {
            *paused_since = Some(Instant::now());
            tracing::warn!(
                provider = %account.provider,
                account = %account.id,
                recheck_secs = schedule.interval.min(CREDENTIAL_RECHECK_MAX).as_secs(),
                "credential unusable — pausing probes until it resolves"
            );
        }
        Transition::Resumed => {
            tracing::info!(
                provider = %account.provider,
                account = %account.id,
                paused_secs = paused_since.take().map_or(0, |t| t.elapsed().as_secs()),
                "credential resolved — resuming polls"
            );
        }
        Transition::Throttled => {
            tracing::warn!(
                provider = %account.provider,
                account = %account.id,
                strikes = gate.throttle.strikes,
                backoff_secs = gate.throttle.delay(schedule.interval).as_secs(),
                "rate limited — throttling polls"
            );
        }
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
        // immediately rather than after a full interval of nothing. A
        // credential that has just come back sets it for the same reason.
        let mut poll_immediately = true;
        // Rate limiting and credential staleness are properties of the
        // credential, not of the config, so they live with the task rather
        // than travelling over `schedule_rx`.
        let mut gate = Gate::default();
        // When the current pause began, for the one line that reports it.
        let mut paused_since: Option<Instant> = None;
        loop {
            let schedule = *schedule_rx.borrow_and_update();
            let step = gate.next_step(schedule, poll_immediately);
            // Published before this iteration's first `.await`: `poll_once`
            // wakes every `AwaitUpdate` client as it records, and a client
            // that got in between would latch the old cadence and not learn
            // better until the next poll — which, while paused, may be hours.
            if let Some(interval) = reported_interval(step, gate.throttle.delay(schedule.interval))
            {
                daemon.set_reported_interval(&account.id, interval);
            }
            match step {
                Step::Park => {
                    poll_immediately = true;
                    // Parking ends a pause too, and the edge is still owed a
                    // closing line — seeing the warning and disabling the
                    // provider is a plausible thing to do about it, and that
                    // path would otherwise leave the stall open in the log
                    // forever.
                    if gate.credential_stale {
                        tracing::info!(
                            provider = %account.provider,
                            account = %account.id,
                            paused_secs = paused_since.map_or(0, |t| t.elapsed().as_secs()),
                            "provider disabled while its credential was unusable — pause ended"
                        );
                    }
                    paused_since = None;
                    gate.park();
                    tokio::select! {
                        _ = schedule_rx.changed() => continue,
                        _ = shutdown_rx.changed() => break,
                    }
                }
                Step::PollNow => {
                    poll_immediately = false;
                    let polled =
                        poll_once(&daemon, adapter.as_ref(), &account, PollTrigger::Startup).await;
                    for t in gate.record(&polled).into_iter().flatten() {
                        log_transition(&account, t, schedule, gate, &mut paused_since);
                    }
                }
                // Nothing is sent while paused: the credential we hold cannot
                // work, and asking anyway is what used to fill the log with
                // thousands of identical failures over a single expired token.
                Step::Recheck { after } => {
                    tokio::select! {
                        // Jittered like a poll, though no request goes out: a
                        // future multi-account Claude reads one file from
                        // several tasks, and recovering them all on the same
                        // tick would fire simultaneous probes.
                        _ = tokio::time::sleep(jittered(after)) => {
                            if adapter.credential_for(&account).await.is_ok() {
                                if let Some(t) = gate.credential_ok() {
                                    log_transition(&account, t, schedule, gate, &mut paused_since);
                                }
                                poll_immediately = true;
                            }
                        }
                        // A manual trigger is not held back by the pause: the
                        // user asked for this one, and `poll_once` resolves the
                        // credential itself, so a recovered login clears the
                        // pause through the same path a scheduled poll uses.
                        Some(trigger) = rx.recv() => {
                            let polled = poll_once(&daemon, adapter.as_ref(), &account, trigger).await;
                            for t in gate.record(&polled).into_iter().flatten() {
                                log_transition(&account, t, schedule, gate, &mut paused_since);
                            }
                        }
                        _ = schedule_rx.changed() => {}
                        _ = shutdown_rx.changed() => break,
                    }
                }
                Step::Wait { after } => {
                    tokio::select! {
                        _ = tokio::time::sleep(jittered(after)) => {
                            let polled =
                                poll_once(&daemon, adapter.as_ref(), &account, PollTrigger::Scheduled).await;
                            for t in gate.record(&polled).into_iter().flatten() {
                                log_transition(&account, t, schedule, gate, &mut paused_since);
                            }
                        }
                        // A manual trigger is not held back by the throttle:
                        // the user asked for this one, and if it succeeds the
                        // backoff was stale and clears.
                        Some(trigger) = rx.recv() => {
                            let polled = poll_once(&daemon, adapter.as_ref(), &account, trigger).await;
                            for t in gate.record(&polled).into_iter().flatten() {
                                log_transition(&account, t, schedule, gate, &mut paused_since);
                            }
                        }
                        // Re-arms the sleep against the new cadence. A
                        // shortened interval therefore takes effect now, not
                        // after the old (possibly hour-long) one finally
                        // elapses.
                        _ = schedule_rx.changed() => {}
                        _ = shutdown_rx.changed() => break,
                    }
                }
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

    /// The edges one poll crossed, in the order they are logged.
    fn edges(crossed: [Option<Transition>; 2]) -> Vec<Transition> {
        crossed.into_iter().flatten().collect()
    }

    fn enabled(interval_secs: u64) -> Schedule {
        Schedule {
            enabled: true,
            interval: Duration::from_secs(interval_secs),
        }
    }

    /// The whole point: a credential that cannot work stops costing the
    /// provider requests. Before this, an expired overnight token bought 180
    /// consecutive probes that could not have succeeded.
    #[test]
    fn an_unusable_credential_rechecks_locally_instead_of_probing() {
        let mut g = Gate::default();
        assert_eq!(
            edges(g.record(&unusable_credential())),
            [Transition::Paused]
        );
        assert_eq!(
            g.next_step(enabled(180), false),
            Step::Recheck {
                after: CREDENTIAL_RECHECK_MAX
            }
        );
    }

    /// A long pause is only tolerable because a re-login ends it, so the
    /// re-check must never be slower than the polling the user asked for.
    #[test]
    fn the_recheck_never_waits_longer_than_the_configured_cadence() {
        let mut g = Gate::default();
        g.record(&unusable_credential());
        assert_eq!(
            g.next_step(enabled(10), false),
            Step::Recheck {
                after: Duration::from_secs(10)
            },
            "a 10 s cadence was slowed to the re-check ceiling"
        );
        assert_eq!(
            g.next_step(enabled(3600), false),
            Step::Recheck {
                after: CREDENTIAL_RECHECK_MAX
            }
        );
    }

    /// Excluded by precedence rather than by a guard inside the re-check.
    #[test]
    fn a_disabled_provider_parks_instead_of_rechecking() {
        let mut g = Gate::default();
        g.record(&unusable_credential());
        let parked = Schedule {
            enabled: false,
            interval: Duration::from_secs(180),
        };
        assert_eq!(g.next_step(parked, false), Step::Park);
        g.park();
        assert_eq!(g, Gate::default(), "parking left state behind");
    }

    /// Recovery is the `Err` → `Ok` edge, and it polls at once rather than
    /// waiting out the cadence the user has already been kept waiting by.
    #[test]
    fn a_resolved_credential_polls_immediately() {
        let mut g = Gate::default();
        g.record(&unusable_credential());
        assert_eq!(g.credential_ok(), Some(Transition::Resumed));
        assert_eq!(g.next_step(enabled(180), true), Step::PollNow);
    }

    /// The edge, not the level. A token the provider rejects still resolves
    /// locally, so re-checking it reports `Ok` forever; treating that as
    /// recovery would poll, get rejected, and poll again on the next re-check.
    #[test]
    fn a_credential_that_never_failed_locally_does_not_trigger_recovery() {
        let mut g = Gate::default();
        assert_eq!(
            edges(g.record(&reached(PollOutcome::AuthError("401".into())))),
            [],
            "a provider rejection was mistaken for a local credential failure"
        );
        assert!(!g.credential_stale);
        for _ in 0..10 {
            assert_eq!(g.credential_ok(), None, "resolving an unpaused credential");
        }
        assert_eq!(
            g.next_step(enabled(180), false),
            Step::Wait {
                after: Duration::from_secs(180)
            },
            "a rejected token should keep polling at cadence, not re-check"
        );
    }

    /// No poll happens while paused, so there is no next poll to count down to
    /// — and `0` is the value clients already read that way.
    #[test]
    fn a_stale_credential_reports_no_next_poll() {
        let mut g = Gate::default();
        g.record(&unusable_credential());
        let step = g.next_step(enabled(180), false);
        assert_eq!(reported_interval(step, BASE), Some(Duration::ZERO));
    }

    /// The poll that ends a pause is the one that wakes every client, and the
    /// value standing at that moment is `0` — the pause published it. Leaving
    /// it alone would hand a freshly recovered account "no next poll" for a
    /// whole cadence, because the TUI refreshes on poll events and runs no
    /// timer of its own.
    #[test]
    fn the_poll_that_ends_a_pause_publishes_the_cadence_it_returns_to() {
        let mut g = Gate::default();
        g.record(&unusable_credential());
        assert_eq!(
            reported_interval(g.next_step(enabled(180), false), BASE),
            Some(Duration::ZERO),
            "a paused account should offer no countdown"
        );

        g.credential_ok();
        let step = g.next_step(enabled(180), true);
        assert_eq!(step, Step::PollNow);
        assert_eq!(
            reported_interval(step, g.throttle.delay(enabled(180).interval)),
            Some(BASE),
            "recovery left the client counting down to nothing"
        );
    }

    /// The half that already worked, kept working: a countdown that reflects
    /// the backoff rather than hitting zero and sitting there.
    #[test]
    fn a_throttled_account_still_reports_its_backed_off_cadence() {
        let mut g = Gate::default();
        g.record(&limited(None));
        g.record(&limited(None));
        let step = g.next_step(enabled(180), false);
        assert_eq!(step, Step::Wait { after: BASE * 2 });
        assert_eq!(reported_interval(step, BASE * 2), Some(BASE * 2));
    }

    /// One line in, one line out — however long the stall lasts between them.
    #[test]
    fn a_pause_is_reported_once_in_and_once_out() {
        let mut g = Gate::default();
        assert_eq!(
            edges(g.record(&unusable_credential())),
            [Transition::Paused]
        );
        for _ in 0..64 {
            assert_eq!(
                edges(g.record(&unusable_credential())),
                [],
                "repeated pause"
            );
        }
        assert_eq!(
            edges(g.record(&reached(PollOutcome::Success { windows: vec![] }))),
            [Transition::Resumed]
        );
        assert_eq!(
            edges(g.record(&reached(PollOutcome::Success { windows: vec![] }))),
            [],
            "repeated recovery"
        );
    }

    /// The poll that ends a pause is a likely candidate to be rate limited: a
    /// re-check resumes the moment the credential resolves, without waiting out
    /// a backoff the pause deliberately did not clear. Both edges are crossed
    /// at once, and reporting only the resumption would leave the long wait
    /// that immediately follows it unexplained.
    #[test]
    fn a_resumption_the_provider_then_limits_reports_both() {
        let mut g = Gate::default();
        g.record(&unusable_credential());
        assert_eq!(
            edges(g.record(&limited(None))),
            [Transition::Resumed, Transition::Throttled],
            "one of the two edges was swallowed"
        );
    }

    /// Likewise for the rate limit, which used to warn on every cycle.
    #[test]
    fn a_rate_limit_is_reported_once_however_long_it_lasts() {
        let mut g = Gate::default();
        assert_eq!(edges(g.record(&limited(None))), [Transition::Throttled]);
        for _ in 0..64 {
            assert_eq!(edges(g.record(&limited(None))), [], "repeated throttle");
        }
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
