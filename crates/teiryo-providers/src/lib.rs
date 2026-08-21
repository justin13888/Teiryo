//! Provider adapters for Teiryo. Each provider implements the `teiryo-core`
//! traits ([`teiryo_core::Authenticator`], [`teiryo_core::Prober`],
//! [`teiryo_core::QuotaParser`], [`teiryo_core::WindowPresenter`]) and is
//! compiled in — no plugin loading.

pub mod claude;

use teiryo_core::ProviderAdapter;

/// All compiled-in provider adapters. The daemon builds its registry from
/// this at startup.
pub fn registry() -> Vec<Box<dyn ProviderAdapter>> {
    vec![Box::new(claude::ClaudeAdapter::new())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_claude() {
        let providers = registry();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id(), "claude");
    }
}
