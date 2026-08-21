//! One poll task per (provider, account): interval ticks and injected manual
//! triggers share a single loop, so `PollNow` needs no separate code path.

use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use teiryo_core::{
    Account, PollEvent, PollId, PollOutcome, PollTrigger, ProbeError, ProviderAdapter,
};
use tokio::sync::mpsc;

use crate::state::Daemon;

/// Spawn the poll task for one (provider, account). Returns the manual
/// trigger sender; the task itself runs until shutdown. Must be called from
/// within a `tokio::task::LocalSet`.
pub fn spawn_poller(
    daemon: &Daemon,
    adapter: Rc<dyn ProviderAdapter>,
    account: Account,
    base_interval: Duration,
) -> mpsc::UnboundedSender<PollTrigger> {
    let (tx, mut rx) = mpsc::unbounded_channel::<PollTrigger>();
    let daemon = daemon.clone();
    let mut shutdown_rx = daemon.shutdown_tx.subscribe();
    tokio::task::spawn_local(async move {
        poll_once(&daemon, adapter.as_ref(), &account, PollTrigger::Startup).await;
        loop {
            let sleep = tokio::time::sleep(jittered(base_interval));
            tokio::select! {
                _ = sleep => {
                    poll_once(&daemon, adapter.as_ref(), &account, PollTrigger::Scheduled).await;
                }
                Some(trigger) = rx.recv() => {
                    poll_once(&daemon, adapter.as_ref(), &account, trigger).await;
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });
    tx
}

/// Run one poll: resolve credential, probe, parse; persist and publish the
/// resulting event whatever the outcome.
async fn poll_once(
    daemon: &Daemon,
    adapter: &dyn ProviderAdapter,
    account: &Account,
    trigger: PollTrigger,
) {
    let started = Instant::now();
    let outcome = poll_outcome(adapter, account).await;
    let event = PollEvent {
        id: PollId::generate(),
        ts: Utc::now(),
        provider: adapter.id(),
        account: account.id.clone(),
        trigger,
        outcome,
        latency_ms: started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
    };
    if let Some(err) = event.outcome.error_message() {
        tracing::warn!(provider = %event.provider, account = %event.account, error = err, "poll failed");
    } else {
        tracing::debug!(provider = %event.provider, account = %event.account, latency_ms = event.latency_ms, "poll ok");
    }
    daemon.record_event(&event);
}

async fn poll_outcome(adapter: &dyn ProviderAdapter, account: &Account) -> PollOutcome {
    let cred = match adapter.credential_for(account).await {
        Ok(c) => c,
        Err(e) => return PollOutcome::AuthError(e.to_string()),
    };
    let raw = match adapter.probe(account, &cred).await {
        Ok(r) => r,
        Err(ProbeError::Auth(m)) => return PollOutcome::AuthError(m),
        Err(ProbeError::RateLimited { retry_after }) => {
            return PollOutcome::RateLimited { retry_after }
        }
        Err(e @ (ProbeError::Network(_) | ProbeError::Provider(_))) => {
            return PollOutcome::NetworkError(e.to_string())
        }
    };
    match adapter.parse(&raw) {
        Ok(windows) => PollOutcome::Success { windows },
        Err(e) => PollOutcome::SchemaDrift(e.to_string()),
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
