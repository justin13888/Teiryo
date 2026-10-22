//! Daemon internals, exposed as a library so integration tests can drive the
//! server without exec'ing the binary. `main.rs` is a thin wrapper.

pub mod bridge;
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
                daemon
                    .state
                    .borrow_mut()
                    .discovery_errors
                    .insert(provider.clone(), e.to_string());
                // A login that appears later must still be picked up.
                scheduler::spawn_rediscovery(&daemon, adapter);
                continue;
            }
        };
        if accounts.is_empty() {
            tracing::info!(provider, "no accounts discovered");
            scheduler::spawn_rediscovery(&daemon, adapter);
            continue;
        }
        if !daemon.state.borrow().config.provider_enabled(&provider) {
            tracing::info!(provider, "provider disabled in config; not polling");
        }
        for account in accounts {
            scheduler::start_account(&daemon, &adapter, account);
        }
    }

    tokio::task::spawn_local(watch::watch_config(daemon.clone(), config_path, applied));
    server::serve(listener, daemon.clone()).await;
    daemon
}
