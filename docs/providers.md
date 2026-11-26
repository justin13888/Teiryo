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

- **Credentials**: `~/.claude/.credentials.json`, key `claudeAiOauth.{accessToken, refreshToken, expiresAt, scopes, subscriptionType, rateLimitTier}`. `expiresAt` is epoch millis; an expired token fails `credential_for` with `AuthError::Expired`, and the daemon then calls `refresh_credential` (see [architecture.md](architecture.md)).
- **Refresh**: `POST https://platform.claude.com/v1/oauth/token` with JSON `{grant_type: "refresh_token", refresh_token, client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e"}` — the endpoint and public client id Claude Code itself uses (read from Claude Code 2.1.332; like the usage endpoint, undocumented). The reply's `access_token`, optional rotated `refresh_token`, and `expires_in` are written back into `~/.claude/.credentials.json`, because the server rotates the refresh token on use and the one spent no longer works for Claude Code either. The write changes only `claudeAiOauth.{accessToken, refreshToken, expiresAt}` and keeps every other field; it is atomic (a `0600` temporary file in the same directory, `fsync`ed, renamed over the original, directory synced). Refreshes are serialized inside the daemon by a mutex and across daemons by an `flock` on `.credentials.json.teiryo.lock` beside the file, and the file is re-read under both: a token that became usable meanwhile (Claude Code ran) is used as is unless it is the one the provider just refused, and a file whose `refreshToken` changed while the request was in flight is left alone — Claude Code renewed concurrently and its pair wins; teiryo still polls with the token it was issued. Only an expired token, or one the provider refused, is renewed: a running Claude Code renews ahead of expiry itself, so teiryo steps in only when nothing else has, which keeps the two from racing. A refresh token the server refuses (400/401/403, e.g. `invalid_grant`) is remembered and not sent again until the file holds a different one, so a dead login costs one request rather than one per re-check; the account stays paused with a "run `claude` to log in again" message. Network errors, 5xx and 429 are transient and retried on the daemon's refresh backoff. Keychain-backed logins (below) are **not** renewed — the Keychain is read, never written — so on macOS without a credentials file an expired token still pauses until the user runs `claude`. On macOS, where Claude Code keeps the same JSON in the login Keychain (generic password `Claude Code-credentials`) rather than in the file, the adapter reads it from there whenever the file does not exist — decided per call, so a file that appears later wins. The read shells out to `/usr/bin/security find-generic-password -s "Claude Code-credentials" -w` rather than linking the Security framework: Claude Code writes the item with that tool, which leaves `security` on the item's access list, so the read is prompt-free as `Authenticator::credential_for` requires; a read from `teiryod` itself would raise an access prompt on every poll. The read is bounded at 5 s and the child killed past it — a read still running is almost always `security` behind an access prompt nobody is there to answer — and reported as a store error naming that cause; a missing item (`security` status 44) is `NotLoggedIn`. Overrides: `TEIRYO_CLAUDE_CREDENTIALS` (an explicit path is read as a file only, never the Keychain), `TEIRYO_CLAUDE_TOKEN_URL` (the refresh endpoint), `TEIRYO_CLAUDE_BASE_URL` (or `ClaudeAdapter::with_config`, which likewise never reads the Keychain). Single account `claude:default` until multi-login lands.
- **Probe**: `GET {base}/api/oauth/usage` with `Authorization: Bearer <access token>` and `anthropic-beta: oauth-2025-04-20`, Claude-Code-like User-Agent, 30 s timeout. One persistent `reqwest::Client` per account.
- **Assumed response schema** — *structure* confirmed against **one** live Max 20x response, kept as the parser's `MAX_WITH_FABLE` fixture; everything below beyond what that one response contains is assumed rather than observed. Nothing establishes what a Pro account emits in `limits[]`, what a Max account emits with `seven_day_opus`/`seven_day_sonnet` non-null **alongside** scoped rows, or whether `kind` has a fourth value; the parser's behaviour on each of those shapes is pinned by hand-built fixtures, which proves the parser and not the server. Only a second capture can change that: a recorded response from a Pro account, and one from a Max account with `seven_day_opus`/`seven_day_sonnet` populated alongside scoped rows, would turn the assumed structure into a checked one. Until then, treat every claim here that ranges over plans rather than over the one capture as the provider's description rather than this project's evidence. Fields beyond those named are ignored. Top-level buckets `five_hour`, `seven_day`, and Max-only `seven_day_opus` / `seven_day_sonnet`, each `{ "utilization": <percent used 0–100>, "resets_at": <ISO 8601 | null> }`; plus a `limits` array whose rows carry `kind`, `percent`, `resets_at` and `scope`. A `weekly_scoped` row with `scope.model` is a per-model weekly cap that has no top-level bucket — in the captured response, the only place the Fable weekly limit appears. Only `weekly_scoped` rows are read; every other kind is skipped, whether or not it is one of the two (`session`, `weekly_all`) seen so far. So is a row naming a model a **fixed bucket** already covers — on the bucket being present in the payload, whether or not this poll's reading of it parsed, so one unreadable `utilization` cannot move a model's cap to a second id. That match is on a canonical form — a vendor prefix and a trailing version are ignored, so "Claude Opus 4.5" is recognised as `seven_day_opus`'s cap — and on nothing looser: "Opus Mini" is a different model and keeps its own window. A scoped row never suppresses another scoped row, but two rows whose slugs collide are one id: the first row carrying a percent wins and the second is dropped (logged — "Opus 4.1" and "Opus-4-1" are one window, and the reading of whichever came second is not reported).
- **What is dropped, and what fails the poll.** Anything unreadable costs at most what it describes, and says so in the log: a row in an unexpected shape, a row with no `percent`, a row naming no model, a model name that yields an empty slug, a row whose window id is already reported, and — symmetrically — a fixed bucket with no `utilization`. Only a payload leaving *no* recognized window at all is `SchemaDrift`. A `limits` field that is absent or `null` is simply no per-model caps and is not logged; one that is present but not an array is logged. A reading outside `0–100` is kept (it may be a genuine overage, and `utilization()` clamps for display) and logged. An unreadable `resets_at` costs the instant, not the window — for a fixed bucket as well as a row, since both are held as raw JSON and converted one field at a time. A `utilization` or `percent` outside `f64` likewise costs that bucket or row alone.
- **Window ids.** `session_5h` (Rolling 5 h), `weekly` (Rolling 7 d), `weekly_opus`/`weekly_sonnet` (`WindowScope::Model(..)`), and `weekly_<model>` from each `weekly_scoped` row. The `<model>` part is `scope.model.id` slugified where the server sends one, and the display name's slug otherwise — lower-cased, non-alphanumeric ASCII runs collapsed to `_`. The id keys stored history, so preferring the server's id is what stops a model rename orphaning a window's whole series; the one captured response sends `id: null`, so if the server begins populating it, each live scoped window is renamed once, on the first poll that carries it, and that series starts over. Nothing prunes the orphaned rows; the daemon warns when a derived id it saw before is no longer reported (`id_is_server_derived` above), which is all one poll establishes. Always `QuotaUnit::Percent`, `limit: Some(100.0)`. Fixed buckets come first, then `limits[]` windows in server order.
