//! Daemon configuration: `$XDG_CONFIG_HOME/teiryo/config.toml`.
//!
//! Absent file or absent keys mean defaults: every compiled-in provider
//! enabled, 60-second poll interval (jittered per cycle).

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// Default poll interval when no override is configured.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Top-level daemon config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Global poll interval override, seconds.
    pub poll_interval_secs: Option<u64>,
    /// Per-provider settings, keyed by provider id.
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

/// Per-provider settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Disable polling for this provider entirely.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Poll interval override for this provider, seconds.
    pub poll_interval_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

impl Config {
    /// Load from `path`; a missing file yields the default config.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether a provider should be polled at all.
    pub fn provider_enabled(&self, provider: &str) -> bool {
        self.providers.get(provider).is_none_or(|p| p.enabled)
    }

    /// Effective poll interval for a provider.
    pub fn poll_interval(&self, provider: &str) -> Duration {
        let secs = self
            .providers
            .get(provider)
            .and_then(|p| p.poll_interval_secs)
            .or(self.poll_interval_secs);
        secs.map_or(DEFAULT_POLL_INTERVAL, Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_missing() {
        let cfg = Config::default();
        assert!(cfg.provider_enabled("claude"));
        assert_eq!(cfg.poll_interval("claude"), DEFAULT_POLL_INTERVAL);
    }

    #[test]
    fn parses_overrides() {
        let cfg: Config = toml::from_str(
            r#"
            poll_interval_secs = 120
            [providers.claude]
            enabled = false
            [providers.openai]
            poll_interval_secs = 30
            "#,
        )
        .unwrap();
        assert!(!cfg.provider_enabled("claude"));
        assert!(cfg.provider_enabled("openai"));
        assert_eq!(cfg.poll_interval("openai"), Duration::from_secs(30));
        assert_eq!(cfg.poll_interval("other"), Duration::from_secs(120));
    }
}
