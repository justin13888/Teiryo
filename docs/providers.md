# Provider Adapters

## Trait split

One provider cannot be one trait once it needs per-account credentials, provider-specific probing, provider-specific parsing, *and* provider-specific rendering rules — collapsing those into one method set forces the TUI to know provider internals. Split by concern:

```rust
#[async_trait]
trait Authenticator: Send + Sync {
    async fn discover_accounts(&self) -> Result<Vec<Account>, AuthError>; // multi-account, day one
    async fn credential_for(&self, account: &Account) -> Result<Credential, AuthError>;
}

#[async_trait]
trait Prober: Send + Sync {
    async fn probe(&self, account: &Account, cred: &Credential) -> Result<RawResponse, ProbeError>;
}

trait QuotaParser: Send + Sync {
    fn parse(&self, raw: &RawResponse) -> Result<Vec<QuotaWindow>, ParseError>;
}

trait WindowPresenter: Send + Sync {
    fn render_hint(&self, window: &QuotaWindow) -> RenderHint; // TUI stays provider-agnostic
    fn group_order(&self) -> &[WindowId];                      // display grouping/order
}

trait ProviderAdapter: Authenticator + Prober + QuotaParser + WindowPresenter {
    fn id(&self) -> ProviderId;
}

struct RenderHint {
    style: BarStyle,           // Percent | FractionOfLimit | CountOnly
    warn_threshold: f32,       // e.g. 0.8
    critical_threshold: f32,
    note: Option<String>,      // provider UX quirk surfaced to the user
}
```

## Credentials

`Credential` is a core-defined enum (`OAuthToken`, `ApiKey`, `CookieJar`), each variant wrapping `secrecy::SecretString` — no `Debug`/`Display` leakage, zeroized on drop.

## Provider quirks (why the model looks like this)

- **Claude (subscription)**: usage is exposed as headroom, not raw counts — no published token/message figures. Two anchored windows: a rolling 5-hour session and a rolling weekly cap. On Max plans Sonnet and Opus draw from **separate** 5h/weekly buckets (4 windows); on Pro they share one pool and Opus isn't available. Hitting the cap **hard-blocks** new prompts. → `QuotaUnit::Percent` is the common case; `WindowScope::Model(..)` applies only on Max; `RenderHint.note` says "blocks entirely at cap".
- **ChatGPT (Plus/Pro)**: published as message *counts* on rolling anchored windows (N messages per 3 h; separate weekly count for the reasoning tier). At cap it **degrades to a mini/fallback model** rather than blocking. → `QuotaUnit::Messages`, `limit: Some(n)`, `RenderHint.note` says "auto-downgrades, doesn't block".

These two alone pin: (a) windows carry their own `unit` and `limit: Option<f64>`; (b) the presenter's `note` field exists because "what happens at 100%" differs per provider and changes what the user should do.

## Claude adapter: implemented behavior

- **Credentials**: `~/.claude/.credentials.json`, key `claudeAiOauth.{accessToken, refreshToken, expiresAt, scopes, subscriptionType, rateLimitTier}`. `expiresAt` is epoch millis; expired tokens fail `credential_for` with `AuthError::Expired`. macOS Keychain is not yet supported. Overrides: `TEIRYO_CLAUDE_CREDENTIALS`, `TEIRYO_CLAUDE_BASE_URL` (or `ClaudeAdapter::with_config`). Single account `claude:default` until multi-login lands.
- **Probe**: `GET {base}/api/oauth/usage` with `Authorization: Bearer <access token>` and `anthropic-beta: oauth-2025-04-20`, Claude-Code-like User-Agent, 30 s timeout. One persistent `reqwest::Client` per account.
- **Assumed response schema** (verified to exist, exact shape still an open item — parser returns `ParseError::SchemaDrift` naming what's missing rather than guessing): top-level buckets `five_hour`, `seven_day`, and Max-only `seven_day_opus` / `seven_day_sonnet`, each `{ "utilization": <percent used 0–100>, "resets_at": <ISO 8601 | null> }`. Unknown fields and buckets are ignored. Mapped windows: `session_5h` (Rolling 5 h), `weekly` (Rolling 7 d), `weekly_opus`/`weekly_sonnet` (`WindowScope::Model(..)`); always `QuotaUnit::Percent`, `limit: Some(100.0)`.
