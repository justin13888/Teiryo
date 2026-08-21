//! Daemon internals, exposed as a library so integration tests can drive the
//! server without exec'ing the binary. `main.rs` is a thin wrapper.

pub mod config;
pub mod paths;
pub mod scheduler;
pub mod server;
pub mod state;

use std::rc::Rc;

use teiryo_core::{ProviderAdapter, Storage};
use tokio::net::UnixListener;

pub use config::Config;
pub use state::Daemon;

/// Run the daemon: discover accounts, spawn pollers, serve the socket until
/// a `Shutdown` request or an external `daemon.shutdown_tx.send(true)`.
///
/// Must be awaited inside a `tokio::task::LocalSet` on a `current_thread`
/// runtime — poll tasks and connection handlers use `spawn_local`.
pub async fn run(
    listener: UnixListener,
    storage: Storage,
    adapters: Vec<Rc<dyn ProviderAdapter>>,
    config: Config,
) -> Daemon {
    let daemon = Daemon::new(storage);
    for adapter in adapters {
        let provider = adapter.id();
        if !config.provider_enabled(&provider) {
            tracing::info!(provider, "provider disabled in config");
            continue;
        }
        let accounts = match adapter.discover_accounts().await {
            Ok(accounts) => accounts,
            Err(e) => {
                tracing::warn!(provider, error = %e, "account discovery failed");
                continue;
            }
        };
        if accounts.is_empty() {
            tracing::info!(provider, "no accounts discovered");
        }
        let interval = config.poll_interval(&provider);
        for account in accounts {
            {
                let mut st = daemon.state.borrow_mut();
                if let Err(e) = st.storage.upsert_account(&account) {
                    tracing::error!(account = %account.id, error = %e, "failed to persist account");
                }
                st.accounts.push(account.clone());
                st.health
                    .entry((provider.clone(), account.id.clone()))
                    .or_default();
            }
            let tx = scheduler::spawn_poller(&daemon, adapter.clone(), account.clone(), interval);
            daemon
                .state
                .borrow_mut()
                .pollers
                .insert((provider.clone(), account.id.clone()), tx);
        }
    }
    server::serve(listener, daemon.clone()).await;
    daemon
}
