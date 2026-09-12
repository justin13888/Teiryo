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
    fn id_is_server_derived(&self, id: &WindowId) -> bool; // default false
}

trait WindowPresenter: Send + Sync {
    fn render_hint(&self, window: &QuotaWindow) -> RenderHint; // TUI stays provider-agnostic
    // No display-order method: windows are drawn in the order the parser
    // emits them, which is the only ordering anything reads.
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

`id_is_server_derived` answers one question the daemon must not guess: a `WindowId` is a durable storage key, so an id the adapter builds out of something the server names (a model id, a label) stops being reported the day the server renames it, while a compiled-in id cannot — a poll without it is that window being absent. Only the parser that mints the ids knows which is which, and judging it from the id's shape got `weekly_opus` (compiled in, model-scoped, `weekly_`-prefixed like every derived id) wrong. It defaults to `false`, the answer for an adapter whose ids are all constants. The daemon uses it for one thing: warning when a derived id stops being reported, since history stored under it is then reachable under no id the parser will emit. It says only that — a one-poll absence looks identical from there, so nothing is claimed about the loss being permanent — and it says nothing at all for a provider with no adapter registered.

`RenderHint` **crosses the wire**: `Status` pairs every window with its hint inside `WindowView` (see [protocol.md](protocol.md)). This is what keeps the TUI provider-agnostic in practice rather than only in principle — gauge colors come from the adapter's own `warn_threshold`/`critical_threshold`, and `note` is displayed verbatim. "80% is fine" and "what happens at 100%" are provider-specific claims; the client must not invent either.

## Credentials

`Credential` is a core-defined enum (`OAuthToken`, `ApiKey`, `CookieJar`), each variant wrapping `secrecy::SecretString` — no `Debug`/`Display` leakage, zeroized on drop.

## Provider quirks (why the model looks like this)

- **Claude (subscription)**: usage is exposed as headroom, not raw counts — no published token/message figures. Two anchored windows: a rolling 5-hour session and a rolling weekly cap. The one captured response — a Max 20x account — carries session, weekly and a `weekly_scoped` Fable row, with `seven_day_opus`/`seven_day_sonnet` both `null`: a model whose weekly allowance is included with the plan is reported as its own weekly cap rather than a top-level bucket. That Sonnet and Opus draw from **separate** buckets on some Max plans, and share one pool on Pro with Opus unavailable, is from the provider's published description and *not* from any response this project has captured — no Pro response has been seen at all, and none has been seen with those buckets populated. Hitting the cap **hard-blocks** new prompts. → `QuotaUnit::Percent` is the common case; `WindowScope::Model(..)` applies only on Max; `RenderHint.note` says "blocks entirely at cap".
- **ChatGPT (Plus/Pro)**: published as message *counts* on rolling anchored windows (N messages per 3 h; separate weekly count for the reasoning tier). At cap it **degrades to a mini/fallback model** rather than blocking. → `QuotaUnit::Messages`, `limit: Some(n)`, `RenderHint.note` says "auto-downgrades, doesn't block".

These two alone pin: (a) windows carry their own `unit` and `limit: Option<f64>`; (b) the presenter's `note` field exists because "what happens at 100%" differs per provider and changes what the user should do.

## Claude adapter: implemented behavior

- **Credentials**: `~/.claude/.credentials.json`, key `claudeAiOauth.{accessToken, refreshToken, expiresAt, scopes, subscriptionType, rateLimitTier}`. `expiresAt` is epoch millis; expired tokens fail `credential_for` with `AuthError::Expired`, which pauses probing and starts the re-check described in [architecture.md](architecture.md) rather than failing a poll every cadence. The file is read and never written: `refreshToken` is present and unused, because refreshing it would rotate the token Claude Code itself depends on and write the result back into Claude Code's own file — a bug or a write race there logs the user out of the tool this adapter only observes. Access tokens last ~12 h, so the pause is what an unattended overnight expiry costs: no requests, and a resumption roughly half a minute after the user next runs `claude` — the re-check wait is jittered like a poll, so 30 s is the cadence, not a bound. macOS Keychain is not yet supported, and a Keychain-backed implementation must keep `credential_for` cheap and prompt-free for the same reason (see `Authenticator::credential_for`). Overrides: `TEIRYO_CLAUDE_CREDENTIALS`, `TEIRYO_CLAUDE_BASE_URL` (or `ClaudeAdapter::with_config`). Single account `claude:default` until multi-login lands.
- **Probe**: `GET {base}/api/oauth/usage` with `Authorization: Bearer <access token>` and `anthropic-beta: oauth-2025-04-20`, Claude-Code-like User-Agent, 30 s timeout. One persistent `reqwest::Client` per account.
- **Assumed response schema** — *structure* confirmed against **one** live Max 20x response, kept as the parser's `MAX_WITH_FABLE` fixture; everything below beyond what that one response contains is assumed rather than observed. Nothing establishes what a Pro account emits in `limits[]`, what a Max account emits with `seven_day_opus`/`seven_day_sonnet` non-null **alongside** scoped rows, or whether `kind` has a fourth value; the parser's behaviour on each of those shapes is pinned by hand-built fixtures, which proves the parser and not the server. Only a second capture can change that: a recorded response from a Pro account, and one from a Max account with `seven_day_opus`/`seven_day_sonnet` populated alongside scoped rows, would turn the assumed structure into a checked one. Until then, treat every claim here that ranges over plans rather than over the one capture as the provider's description rather than this project's evidence. Fields beyond those named are ignored. Top-level buckets `five_hour`, `seven_day`, and Max-only `seven_day_opus` / `seven_day_sonnet`, each `{ "utilization": <percent used 0–100>, "resets_at": <ISO 8601 | null> }`; plus a `limits` array whose rows carry `kind`, `percent`, `resets_at` and `scope`. A `weekly_scoped` row with `scope.model` is a per-model weekly cap that has no top-level bucket — in the captured response, the only place the Fable weekly limit appears. Only `weekly_scoped` rows are read; every other kind is skipped, whether or not it is one of the two (`session`, `weekly_all`) seen so far. So is a row naming a model a **fixed bucket** already covers — on the bucket being present in the payload, whether or not this poll's reading of it parsed, so one unreadable `utilization` cannot move a model's cap to a second id. That match is on a canonical form — a vendor prefix and a trailing version are ignored, so "Claude Opus 4.5" is recognised as `seven_day_opus`'s cap — and on nothing looser: "Opus Mini" is a different model and keeps its own window. A scoped row never suppresses another scoped row, but two rows whose slugs collide are one id: the first row carrying a percent wins and the second is dropped (logged — "Opus 4.1" and "Opus-4-1" are one window, and the reading of whichever came second is not reported).
- **What is dropped, and what fails the poll.** Anything unreadable costs at most what it describes, and says so in the log: a row in an unexpected shape, a row with no `percent`, a row naming no model, a model name that yields an empty slug, a row whose window id is already reported, and — symmetrically — a fixed bucket with no `utilization`. Only a payload leaving *no* recognized window at all is `SchemaDrift`. A `limits` field that is absent or `null` is simply no per-model caps and is not logged; one that is present but not an array is logged. A reading outside `0–100` is kept (it may be a genuine overage, and `utilization()` clamps for display) and logged. An unreadable `resets_at` costs the instant, not the window — for a fixed bucket as well as a row, since both are held as raw JSON and converted one field at a time. A `utilization` or `percent` outside `f64` likewise costs that bucket or row alone.
- **Window ids.** `session_5h` (Rolling 5 h), `weekly` (Rolling 7 d), `weekly_opus`/`weekly_sonnet` (`WindowScope::Model(..)`), and `weekly_<model>` from each `weekly_scoped` row. The `<model>` part is `scope.model.id` slugified where the server sends one, and the display name's slug otherwise — lower-cased, non-alphanumeric ASCII runs collapsed to `_`. The id keys stored history, so preferring the server's id is what stops a model rename orphaning a window's whole series; the one captured response sends `id: null`, so if the server begins populating it, each live scoped window is renamed once, on the first poll that carries it, and that series starts over. Nothing prunes the orphaned rows; the daemon warns when a derived id it saw before is no longer reported (`id_is_server_derived` above), which is all one poll establishes. Always `QuotaUnit::Percent`, `limit: Some(100.0)`. Fixed buckets come first, then `limits[]` windows in server order.
