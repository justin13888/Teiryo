//! Daemon configuration: `$XDG_CONFIG_HOME/teiryo/config.toml`.
//!
//! Absent file or absent keys mean defaults: every compiled-in provider
//! enabled, 3-minute poll interval (jittered per cycle, and stretched further
//! while a provider is rate limiting us — see [`crate::scheduler`]).
//!
//! The file is read every time it changes on disk, so parsing has to be
//! forgiving in one direction and strict in the other:
//!
//! - **Unknown keys are warnings.** A key this build does not know is dropped
//!   and reported; the rest of the file still applies. A config written for a
//!   newer teiryod must not stop an older one from polling.
//! - **Wrong-shaped values reject the whole file.** A negative interval or a
//!   non-boolean `enabled` means the file cannot be trusted piecewise, so
//!   nothing from it is applied and the previously applied config keeps
//!   running. Partial application would leave the user unable to reason about
//!   which half took effect.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use teiryo_core::{ConfigEdit, ConfigView, ProviderId, ProviderSettings};
use toml_edit::{DocumentMut, Item, Table};

/// Default poll interval when no override is configured.
///
/// Quota figures move slowly enough that a minute of extra resolution buys
/// nothing a user can act on, while costing three times the requests against
/// an endpoint that rate limits — and a rate limit costs the readings
/// themselves. Three minutes still puts several points inside the narrowest
/// chart range.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(180);

/// Smallest accepted poll interval. Below this a typo would hammer a
/// provider's usage endpoint hard enough to get the account rate limited,
/// which costs the user the very data they configured teiryo to collect.
pub const MIN_POLL_INTERVAL_SECS: u64 = 10;

/// Largest accepted poll interval — the ceiling of the wire's `u32` seconds
/// field. This is a representability bound, not a policy one.
pub const MAX_POLL_INTERVAL_SECS: u64 = u32::MAX as u64;

/// Why a config file or a proposed edit was rejected. Rejection is always
/// whole-file: see the module docs.
#[derive(Debug, Clone)]
pub struct ConfigError(String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Top-level daemon config, after validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// Global poll interval override, seconds.
    pub poll_interval_secs: Option<u64>,
    /// Per-provider settings, keyed by provider id.
    pub providers: HashMap<String, ProviderConfig>,
}

/// Per-provider settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    /// Disable polling for this provider entirely.
    pub enabled: bool,
    /// Poll interval override for this provider, seconds.
    pub poll_interval_secs: Option<u64>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_secs: None,
        }
    }
}

/// A validated config plus everything the file said that we ignored.
#[derive(Debug, Clone, Default)]
pub struct LoadedConfig {
    /// The settings to apply.
    pub config: Config,
    /// Unknown keys, already dropped, in file order.
    pub warnings: Vec<String>,
}

impl Config {
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

    /// The wire-facing view of these settings.
    ///
    /// `known` is the compiled-in provider registry: a provider with no config
    /// entry still needs a row, or the client could not enable one. Providers
    /// named only in the file are included too, so a stale or misspelled entry
    /// is visible rather than silently inert.
    pub fn view(&self, known: &[ProviderId]) -> ConfigView {
        let mut ids: Vec<&str> = known
            .iter()
            .map(String::as_str)
            .chain(self.providers.keys().map(String::as_str))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ConfigView {
            poll_interval_secs: self.poll_interval_secs.map(secs_to_u32),
            default_poll_interval_secs: secs_to_u32(DEFAULT_POLL_INTERVAL.as_secs()),
            min_poll_interval_secs: secs_to_u32(MIN_POLL_INTERVAL_SECS),
            providers: ids
                .into_iter()
                .map(|id| ProviderSettings {
                    provider: id.to_owned(),
                    enabled: self.provider_enabled(id),
                    poll_interval_secs: self
                        .providers
                        .get(id)
                        .and_then(|p| p.poll_interval_secs)
                        .map(secs_to_u32),
                    effective_poll_interval_secs: secs_to_u32(self.poll_interval(id).as_secs()),
                })
                .collect(),
        }
    }
}

fn secs_to_u32(secs: u64) -> u32 {
    secs.min(MAX_POLL_INTERVAL_SECS) as u32
}

/// Parse and validate config text.
pub fn parse(text: &str) -> Result<LoadedConfig, ConfigError> {
    let table: toml::Table = text
        .parse()
        .map_err(|e| ConfigError::new(format!("not valid TOML: {e}")))?;

    let mut loaded = LoadedConfig::default();
    for (key, value) in &table {
        match key.as_str() {
            "poll_interval_secs" => {
                loaded.config.poll_interval_secs = Some(interval(value, key)?);
            }
            "providers" => {
                let providers = value.as_table().ok_or_else(|| {
                    ConfigError::new(format!("`providers` must be a table, got {}", kind(value)))
                })?;
                for (id, entry) in providers {
                    loaded
                        .config
                        .providers
                        .insert(id.clone(), provider(entry, id, &mut loaded.warnings)?);
                }
            }
            other => loaded.warnings.push(unknown(other)),
        }
    }
    Ok(loaded)
}

/// [`parse`] over a file; a missing file is the default config.
pub fn load(path: &Path) -> Result<LoadedConfig, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LoadedConfig::default()),
        Err(e) => Err(ConfigError::new(format!("cannot read the file: {e}"))),
    }
}

fn provider(
    value: &toml::Value,
    id: &str,
    warnings: &mut Vec<String>,
) -> Result<ProviderConfig, ConfigError> {
    let table = value.as_table().ok_or_else(|| {
        ConfigError::new(format!(
            "`providers.{id}` must be a table, got {}",
            kind(value)
        ))
    })?;
    let mut config = ProviderConfig::default();
    for (key, value) in table {
        match key.as_str() {
            "enabled" => {
                config.enabled = value.as_bool().ok_or_else(|| {
                    ConfigError::new(format!(
                        "`providers.{id}.enabled` must be true or false, got {}",
                        kind(value)
                    ))
                })?;
            }
            "poll_interval_secs" => {
                config.poll_interval_secs =
                    Some(interval(value, &format!("providers.{id}.{key}"))?);
            }
            other => warnings.push(unknown(&format!("providers.{id}.{other}"))),
        }
    }
    Ok(config)
}

/// A poll interval in whole seconds, within the accepted range.
fn interval(value: &toml::Value, path: &str) -> Result<u64, ConfigError> {
    let raw = value.as_integer().ok_or_else(|| {
        ConfigError::new(format!(
            "`{path}` must be a whole number of seconds, got {}",
            kind(value)
        ))
    })?;
    let secs = u64::try_from(raw)
        .map_err(|_| ConfigError::new(format!("`{path}` must not be negative, got {raw}")))?;
    if secs < MIN_POLL_INTERVAL_SECS {
        return Err(ConfigError::new(format!(
            "`{path}` must be at least {MIN_POLL_INTERVAL_SECS} seconds, got {secs}"
        )));
    }
    if secs > MAX_POLL_INTERVAL_SECS {
        return Err(ConfigError::new(format!(
            "`{path}` must be at most {MAX_POLL_INTERVAL_SECS} seconds, got {secs}"
        )));
    }
    Ok(secs)
}

fn unknown(path: &str) -> String {
    format!("unknown key `{path}` — ignored")
}

fn kind(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

/// Reject an edit the daemon would not accept back, *before* it reaches the
/// file — a rejected write must leave `config.toml` exactly as it was.
pub fn validate_edit(edit: &ConfigEdit) -> Result<(), ConfigError> {
    let (secs, path) = match edit {
        ConfigEdit::GlobalPollInterval(secs) => (*secs, "poll_interval_secs".to_owned()),
        ConfigEdit::ProviderPollInterval { provider, secs } => {
            (*secs, format!("providers.{provider}.poll_interval_secs"))
        }
        ConfigEdit::ProviderEnabled { .. } => return Ok(()),
    };
    match secs {
        Some(secs) => interval(&toml::Value::Integer(i64::from(secs)), &path).map(drop),
        None => Ok(()),
    }
}

/// Apply one edit to `config.toml` and write it back atomically. Returns the
/// new file text so the caller can hand it straight to [`parse`] and to the
/// watcher's already-applied comparison.
///
/// Uses `toml_edit` rather than re-serializing: the file is hand-written and
/// hand-commented, and a settings tweak that silently ate the user's comments
/// would be a worse bug than the one it fixed.
pub fn write_edit(path: &Path, edit: &ConfigEdit) -> Result<String, ConfigError> {
    validate_edit(edit)?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(ConfigError::new(format!("cannot read the file: {e}"))),
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| ConfigError::new(format!("not valid TOML: {e}")))?;
    apply_edit(&mut doc, edit)?;
    let out = doc.to_string();
    atomic_write(path, &out)
        .map_err(|e| ConfigError::new(format!("cannot write the file: {e}")))?;
    Ok(out)
}

fn apply_edit(doc: &mut DocumentMut, edit: &ConfigEdit) -> Result<(), ConfigError> {
    match edit {
        ConfigEdit::GlobalPollInterval(Some(secs)) => {
            doc["poll_interval_secs"] = toml_edit::value(i64::from(*secs));
        }
        ConfigEdit::GlobalPollInterval(None) => {
            doc.remove("poll_interval_secs");
        }
        ConfigEdit::ProviderPollInterval { provider, secs } => {
            let table = provider_table(doc, provider)?;
            match secs {
                // Clearing removes the key rather than writing a placeholder,
                // so "inherit the global value" reads the same in the file as
                // it does in a config that never set it.
                Some(secs) => table["poll_interval_secs"] = toml_edit::value(i64::from(*secs)),
                None => {
                    table.remove("poll_interval_secs");
                }
            }
        }
        ConfigEdit::ProviderEnabled { provider, enabled } => {
            provider_table(doc, provider)?["enabled"] = toml_edit::value(*enabled);
        }
    }
    Ok(())
}

/// The `[providers.<id>]` table, created if absent.
fn provider_table<'a>(
    doc: &'a mut DocumentMut,
    provider: &str,
) -> Result<&'a mut Table, ConfigError> {
    // Only a table we create ourselves becomes implicit: marking an existing
    // one implicit would delete the user's own `[providers]` header.
    let fresh = !doc.contains_key("providers");
    let providers = doc
        .entry("providers")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::new("`providers` is not a table"))?;
    if fresh {
        providers.set_implicit(true);
    }
    providers
        .entry(provider)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::new(format!("`providers.{provider}` is not a table")))
}

/// Write via a temp file in the same directory, then rename. A crash mid-write
/// must not leave the daemon's own config truncated.
fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map_or_else(|| "config.toml".to_owned(), |n| n.to_string_lossy().into());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    };
    if let Err(e) = write() {
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        std::fs::remove_file(&tmp).ok();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> LoadedConfig {
        parse(text).expect("should parse")
    }

    fn err(text: &str) -> String {
        parse(text).expect_err("should be rejected").to_string()
    }

    #[test]
    fn defaults_when_missing() {
        let cfg = Config::default();
        assert!(cfg.provider_enabled("claude"));
        assert_eq!(cfg.poll_interval("claude"), DEFAULT_POLL_INTERVAL);
    }

    #[test]
    fn parses_overrides() {
        let cfg = ok(r#"
            poll_interval_secs = 120
            [providers.claude]
            enabled = false
            [providers.openai]
            poll_interval_secs = 30
        "#)
        .config;
        assert!(!cfg.provider_enabled("claude"));
        assert!(cfg.provider_enabled("openai"));
        assert_eq!(cfg.poll_interval("openai"), Duration::from_secs(30));
        assert_eq!(cfg.poll_interval("other"), Duration::from_secs(120));
    }

    /// A config written for a newer build must not stop this one from polling:
    /// the unknown key is dropped and reported, the rest still applies.
    #[test]
    fn unknown_keys_warn_but_still_apply() {
        let loaded = ok(r#"
            poll_interval_secs = 120
            retention_days = 30
            [providers.claude]
            enabled = true
            retrys = 3
        "#);
        assert_eq!(loaded.config.poll_interval_secs, Some(120));
        assert!(loaded.config.provider_enabled("claude"));
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("`retention_days`")),
            "{:?}",
            loaded.warnings
        );
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("`providers.claude.retrys`")),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn wrong_shapes_are_rejected() {
        assert!(err("poll_interval_secs = -5").contains("must not be negative"));
        assert!(err(r#"poll_interval_secs = "fast""#).contains("whole number"));
        assert!(err("[providers.claude]\nenabled = \"yes\"").contains("true or false"));
        assert!(err("providers = 3").contains("must be a table"));
        assert!(err("poll_interval_secs = ").contains("not valid TOML"));
    }

    #[test]
    fn intervals_below_the_floor_are_rejected() {
        let message = err("poll_interval_secs = 5");
        assert!(message.contains("at least 10"), "{message}");
        // Per-provider overrides go through the same check.
        assert!(err("[providers.claude]\npoll_interval_secs = 0").contains("at least 10"));
        // The floor itself is fine.
        assert_eq!(
            ok("poll_interval_secs = 10").config.poll_interval_secs,
            Some(10)
        );
    }

    #[test]
    fn view_covers_registry_providers_with_no_config_entry() {
        let cfg =
            ok("poll_interval_secs = 120\n[providers.claude]\npoll_interval_secs = 30").config;
        let view = cfg.view(&["claude".to_owned(), "openai".to_owned()]);
        assert_eq!(view.poll_interval_secs, Some(120));
        assert_eq!(view.default_poll_interval_secs, 180);
        assert_eq!(view.min_poll_interval_secs, 10);

        let claude = &view.providers[0];
        assert_eq!(claude.provider, "claude");
        assert_eq!(claude.poll_interval_secs, Some(30));
        assert_eq!(claude.effective_poll_interval_secs, 30);

        // No entry in the file, so no override — but still a row, or the
        // client would have no way to configure it.
        let openai = &view.providers[1];
        assert_eq!(openai.provider, "openai");
        assert!(openai.enabled);
        assert_eq!(openai.poll_interval_secs, None);
        assert_eq!(openai.effective_poll_interval_secs, 120);
    }

    fn temp_config(body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("teiryod-config-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// The file is hand-written and hand-commented; an edit that ate the
    /// comments would be a worse bug than the one it fixed.
    #[test]
    fn edits_preserve_comments_and_layout() {
        let path = temp_config(
            "# how often to poll\npoll_interval_secs = 60\n\n[providers.claude]\nenabled = true\n",
        );
        let out = write_edit(&path, &ConfigEdit::GlobalPollInterval(Some(300))).unwrap();
        assert!(out.contains("# how often to poll"), "{out}");
        assert!(out.contains("poll_interval_secs = 300"), "{out}");
        assert!(out.contains("[providers.claude]"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), out);
        assert_eq!(parse(&out).unwrap().config.poll_interval_secs, Some(300));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn clearing_an_override_removes_the_key() {
        let path = temp_config("[providers.claude]\npoll_interval_secs = 30\nenabled = true\n");
        let out = write_edit(
            &path,
            &ConfigEdit::ProviderPollInterval {
                provider: "claude".into(),
                secs: None,
            },
        )
        .unwrap();
        assert!(!out.contains("poll_interval_secs"), "{out}");
        assert!(out.contains("enabled = true"), "{out}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn edits_create_missing_tables_and_files() {
        let dir = std::env::temp_dir().join(format!("teiryod-config-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let out = write_edit(
            &path,
            &ConfigEdit::ProviderEnabled {
                provider: "claude".into(),
                enabled: false,
            },
        )
        .unwrap();
        assert!(out.contains("[providers.claude]"), "{out}");
        assert!(!parse(&out).unwrap().config.provider_enabled("claude"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A rejected edit must leave the file byte-identical — the daemon writes
    /// only what it would accept back.
    #[test]
    fn rejected_edits_never_touch_the_file() {
        let before = "poll_interval_secs = 60\n";
        let path = temp_config(before);
        let error = write_edit(&path, &ConfigEdit::GlobalPollInterval(Some(5)))
            .expect_err("below the floor");
        assert!(error.to_string().contains("at least 10"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
