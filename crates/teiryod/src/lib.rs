//! Daemon internals, exposed as a library so integration tests can drive the
//! server without exec'ing the binary. `main.rs` is a thin wrapper.

pub mod config;
pub mod paths;
pub mod scheduler;
pub mod server;
pub mod state;
pub mod watch;

use std::path::PathBuf;
use std::rc::Rc;

use teiryo_core::{ProviderAdapter, Storage};
use tokio::net::UnixListener;

pub use config::Config;
pub use state::Daemon;

/// Run the daemon: load config, discover accounts, spawn pollers, watch
/// `config.toml`, and serve the socket until a `Shutdown` request or an
/// external `daemon.shutdown_tx.send(true)`.
///
/// Must be awaited inside a `tokio::task::LocalSet` on a `current_thread`
/// runtime — poll tasks and connection handlers use `spawn_local`.
pub async fn run(
    listener: UnixListener,
    storage: Storage,
    adapters: Vec<Rc<dyn ProviderAdapter>>,
    config_path: PathBuf,
) -> Daemon {
    let known: Vec<_> = adapters.iter().map(|a| a.id()).collect();
    let daemon = Daemon::new(storage, config_path.clone(), known);
    let applied = watch::load_initial(&daemon, &config_path);

    for adapter in adapters {
        let provider = adapter.id();
        // Discovery runs even for a provider disabled in config: it is a local
        // credential read, and skipping it would leave the provider with no
        // accounts and no poll task, so enabling it later could not take
        // effect without a daemon restart. Polling — not discovery — is what
        // `enabled` gates, in the poll task itself.
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
        if !daemon.state.borrow().config.provider_enabled(&provider) {
            tracing::info!(provider, "provider disabled in config; not polling");
        }
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
            // Serve what the last run already learned about this account
            // while the first poll of this run is still in flight.
            daemon.hydrate_account(&account);
            let schedule_rx = daemon.register_poller(&account, adapter.clone());
            let tx =
                scheduler::spawn_poller(&daemon, adapter.clone(), account.clone(), schedule_rx);
            daemon
                .state
                .borrow_mut()
                .pollers
                .insert((provider.clone(), account.id.clone()), tx);
        }
    }

    tokio::task::spawn_local(watch::watch_config(daemon.clone(), config_path, applied));
    server::serve(listener, daemon.clone()).await;
    daemon
}
