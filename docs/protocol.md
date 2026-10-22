# Wire Protocol

## Transport & framing

- `tokio::net::UnixListener` / `UnixStream` on the socket path given in [architecture.md](architecture.md); socket file mode 0600 in a per-user directory that is itself 0700 (`$XDG_RUNTIME_DIR` on Linux, the per-user temp dir on macOS). Cross-user access is therefore not a threat model, except on the `/tmp/teiryo-$UID.sock` fallback used when no such directory exists: `/tmp` is world-writable, so another local user who claims that name (or its `.lock`) first can keep the daemon from starting, or answer the TUI in its place. The 0600 mode still keeps them from talking to a daemon that did bind; the handshake below guards against *stale/mismatched builds*, not attackers.
- Framing: `tokio_util::codec::LengthDelimitedCodec`, u32 **little-endian** length prefix, `max_frame_length` = 1 MiB. Oversized/garbage frames from a misbehaving peer are rejected instead of buffered unbounded. Daemon and TUI must construct the codec identically (shared constructor in `teiryo-core`).
- The handshake and the frames run over any byte stream (`teiryo_core::protocol::transport::Transport`, any `AsyncRead + AsyncWrite`); the client holds its connection as a boxed one. Locally that stream is the Unix socket, found or spawned by `transport::connect_or_spawn`, which both binaries share.

### Remote daemon over SSH

`teiryo --ssh [user@]host` uses the daemon on another host. For each connection — one for commands, one for the long-poll — it runs:

```text
ssh -T -o BatchMode=yes <host> teiryod bridge
```

and speaks the protocol over the child's stdin and stdout (`teiryo_core::protocol::ssh`).

- `teiryod bridge` resolves the socket path as the daemon does, spawns the daemon if nothing is listening (the same `connect_or_spawn`, with its own executable as the daemon), then copies bytes both ways between its stdio and the socket until either side closes. It never reads the stream: the client's Hello reaches the remote daemon unchanged, so **the remote daemon enforces exact `PROTOCOL_VERSION` equality** and its reject comes back as the usual two-version message. When the daemon closes first the bridge exits at once; when the client closes first the daemon's read side is closed and whatever it still sends is delivered. A reset after the daemon's last bytes (it closes with the client's commit unread after a reject) is treated as its end of stream. The bridge's own errors go to stderr only; its stdout carries nothing but relayed bytes.
- `-T`: the stream is binary, so no pseudo-terminal. `BatchMode=yes`: nobody can answer a password or host-key prompt behind the TUI, so ssh fails instead of asking. Port, user, keys, jump hosts and the rest come from the user's ssh config.
- `--ssh-command CMD` replaces `ssh` (default: `$TEIRYO_SSH`, else `ssh`). It names one program, run with the arguments above; it is not split into words. `--remote-daemon PATH` replaces `teiryod` (default: `teiryod` on the remote `$PATH`). The remote shell reads it, so `~/.cargo/bin/teiryod` works where a non-interactive shell's `$PATH` lacks cargo's bin directory. A host starting with `-` is refused, so it can never reach ssh as an option.
- When ssh ends the stream before the handshake completes, the client reports ssh's last stderr line (an unknown host, a refused key, a remote `teiryod: not found`, or the bridge's own error) instead of a bare end of stream, naming the host. A usage error's closing `For more information, try '--help'.` is skipped for the line before it. A remote `teiryod` that refuses `bridge` as an unexpected argument predates it: the client says so, tells the user to install the matching `teiryod` there, and stops reconnecting, as on a version mismatch. When ssh is still running at the handshake limit, the client says so rather than claiming the remote daemon was reached. A reject from the remote daemon is never masked this way.
- Reconnects rerun ssh on the usual backoff; a version mismatch still stops them. In remote mode the header names the host, and the build-commit marker compares this client against the remote daemon.
- A daemon the bridge spawns is detached like a locally spawned one, so it keeps running after the ssh session ends.

## Serialization

`bincode` (serde mode, standard config) over `Request`/`Response` enums shared via `teiryo-core`.

Rejected alternatives, each in one line: JSON — text overhead with no local upside; Protobuf — schema/codegen ceremony for two binaries built from one workspace; rkyv — zero-copy is wasted on ~1 Hz small messages.

**The risk bincode does not handle**: it is positional, not self-describing. If daemon and TUI disagree on an enum's variant order, one side can silently decode bytes as the *wrong variant* — a `Shutdown` misread as `Status` is the failure mode the handshake exists to design out.

## Handshake

A hand-decoded, **never-changing** preamble runs before any bincode bytes:

```rust
// First 6 bytes on every connection, raw — not bincode, so it can never itself go stale
struct Hello { magic: [u8; 4] /* b"TEIR" */, protocol_version: u16 /* little-endian; currently 9 */ }
```

- Client sends the 6-byte Hello, followed (since v9) by its build commit. Daemon replies with **one raw status byte**: `0x00` accepted, `0x01` version mismatch. After `0x00` the daemon (since v9) sends its own build commit, then bincode frames may follow. After `0x01` the daemon (since v8) sends its own `PROTOCOL_VERSION` as a little-endian `u16`, then closes the connection without ever attempting to decode a `Request`.
- A build commit is the sender's `TEIRYO_COMMIT_HASH` (what `-V` prints on its `commit:` line: a short hash, `-dirty` when the tree had uncommitted changes, or `unknown`), encoded as a little-endian `u16` byte length followed by that many bytes of UTF-8. A sender cuts it to 256 bytes at a character boundary; a receiver refuses a longer length (`HandshakeError::CommitTooLong`) and replaces bytes that are not UTF-8 rather than failing.
- The daemon reads the client's commit only after the version matched, and the reject reply keeps its v8 layout: a v8 client, which sends no commit, still gets `0x01` and the daemon's version, and still names both versions.

```text
client → daemon   T E I R | 09 00 | len:u16le | commit (UTF-8)
daemon → client   00 | len:u16le | commit (UTF-8)            accepted
daemon → client   01 | version:u16le, then close             rejected (unchanged since v8)
```

**Build check.** Equal protocol versions are what the connection requires; equal builds are only reported. Two builds match only when their commits are equal and neither ends in `-dirty`; `unknown` never matches either (`teiryo_core::version::builds_match`). On an accepted connection the daemon logs both commits and both protocol versions, at `error!` on a mismatch and at `debug!` on a match, in every build profile. The TUI keeps the mismatch for the command connection it holds and shows it as a dim `· build ≠ daemon (client / daemon)` at the end of the header's connection line. It never refuses the connection over it, and the incompatible and unreachable states, which replace that line, take precedence.
- On mismatch the TUI names both versions: at startup it prints "the running daemon speaks protocol vX; this client is vY" and exits, and on a reconnect its header shows "daemon speaks protocol vX, this client vY — stop it (`pkill teiryod`) and relaunch teiryo" (over `--ssh`, "daemon on HOST speaks …", with reinstalling there or `ssh HOST pkill teiryod` as the remedy). A daemon older than v8 sends `0x01` alone and closes; the client reads the missing version as "older than v8" and still reports the mismatch. The trailing version is the one addition the never-changing preamble allows: a client that predates it reads only the first byte and closes, so it is unaffected. **No negotiation, no backward compat** in v1: daemon and TUI ship together; the handshake fails loudly on the unclean case (stale daemon left running across an upgrade), it does not support long-term protocol drift.
- Any wire-protocol change (variant added/reordered/removed, field change) **must** bump `PROTOCOL_VERSION`; see [Versioning policy](#versioning-policy).
- Two recorded fixtures enforce that pairing rather than trusting it: the six Hello bytes (and, since v9, a whole opening with its commit) are pinned in `handshake.rs`, and one encoded `WindowView` frame is pinned byte for byte in `codec.rs`. A round-trip test cannot see a wire change — both sides move together — so without them a reorder or a forgotten bump passed silently. When either fails legitimately, bump the version, update this file, and re-record the bytes together. The fingerprint guard below covers the rest of the wire.

## Versioning policy

`PROTOCOL_VERSION` is a single integer, and client and daemon must hold **exactly** the same one. There is no negotiation and no compatibility window: a daemon accepts its own version and rejects every other.

**Any change to the bytes on the wire bumps `PROTOCOL_VERSION` by 1.** That covers:

- the handshake: the Hello, the accept and reject replies, and anything added after them;
- framing: the length prefix's width, byte order, and `MAX_FRAME_LEN`;
- `wire.rs` and every type it carries (`teiryo-core`'s domain, adapter, error, and rollover types included): a variant added, removed, or reordered, and a field added, removed, reordered, or changed in type or representation.

Renaming a field or variant changes nothing on the wire (bincode is positional) and needs no bump.

**The bump guard.** `the_wire_format_is_pinned_to_the_protocol_version` in `crates/teiryo-core/src/protocol/mod.rs` encodes one canonical value of every variant of `Request`, `Response`, and every enum nested in them, plus the Hello, the accept and reject replies the real `server_handshake` sends, and one frame's length prefix. It hashes those encodings into a fingerprint and asserts that `(PROTOCOL_VERSION, fingerprint)` equals the pinned `WIRE_FINGERPRINT`. Each enum's canonical values are checked against the variants serde reports the enum declares, so an appended variant — which leaves every existing encoding unchanged but still breaks an older peer that receives it — fails the guard too.

When it fails:

1. Bump `PROTOCOL_VERSION` in `handshake.rs` by 1.
2. Update this file, and re-record the golden Hello bytes in `handshake.rs` (and the `WindowView` frame in `codec.rs` if it moved).
3. Rerun the test, and repin `WIRE_FINGERPRINT` to the pair its failure prints. Repinning the hash without bumping the version is never the fix; bumping the version without repinning fails as well, so the two only move together.

What the guard cannot see: a change that leaves the canonical values' bytes identical. bincode's integers are variable-length, so widening a field's type (a `u32` to a `u64`) encodes the same small value the same way. Such a change still bumps the version under this policy; the guard only makes a forgotten bump unlikely, not impossible.

## Requests & responses

```rust
enum Request {
    Status { provider: Option<ProviderId>, account: Option<AccountId> }, // None = all
    PollNow { provider: ProviderId, account: Option<AccountId> },        // None = all accounts on that provider
    AwaitUpdate {                                                        // long-poll, both clocks
        since: PollId,
        config_gen: u64,                // newest ConfigState.generation seen; u64::MAX = don't care
        timeout_ms: u32,
    },
    History {                                                            // bounded, see below
        account: AccountId,
        window: Option<WindowId>,
        since: DateTime<Utc>,
        until: Option<DateTime<Utc>>,   // None = now
        max_points: Option<u32>,        // per window; daemon downsamples
    },
    RecentPolls { limit: u32 },
    Providers,
    Shutdown,
    GetConfig,
    SetConfig(ConfigEdit),              // daemon validates, rewrites config.toml, applies
}

enum Response {
    Status(Vec<AccountStatus>),
    PollAccepted { poll_id: PollId },
    Update(PollEvent),       // AwaitUpdate resolved with new data
    NoUpdate,                // AwaitUpdate timed out, nothing new
    History(HistoryPage),      // snapshots + the rollovers over the same interval
    RecentPolls(Vec<PollEvent>),
    Providers(Vec<ProviderHealth>),
    Ack,
    Err(ErrorKind, String),
    Config(ConfigState),     // GetConfig, SetConfig, and a config-woken AwaitUpdate
}

struct HistoryPage {
    snapshots: Vec<QuotaSnapshot>,      // the requested interval, oldest first, downsampled
    earliest: Option<DateTime<Utc>>,    // start of the *stored* series, whatever was asked for
    rollovers: Vec<WindowRollover>,     // boundaries over the same interval, never downsampled
}

struct WindowView {
    window: QuotaWindow,
    hint: RenderHint,
    observed_start: Option<ObservedStart>,  // the last restart actually seen for this window
}

struct AccountStatus {
    account: Account,
    windows: Vec<WindowView>,
    last_poll: Option<PollEvent>,       // any outcome, may be a failure
    last_success: Option<DateTime<Utc>>, // when the poll backing `windows` completed
    poll_interval_secs: u32,             // cadence in force now; jitters ±10%, stretches under backoff
}

struct AccountHealth {
    account: AccountId,
    consecutive_failures: u32,
    last_error: Option<String>,
    last_poll_ts: Option<DateTime<Utc>>,
    poll_interval_secs: u32,
}
struct ProviderHealth {
    provider: ProviderId,
    accounts: Vec<AccountHealth>,        // per-account, not just ids
    consecutive_failures: u32,           // worst across the provider's accounts
    last_error: Option<String>,
    discovery_error: Option<String>,     // why discovery found no accounts; listed with empty `accounts`
}
```

## Settings

```rust
struct ConfigState {
    path: String,                        // the file the daemon reads, writes, and watches
    generation: u64,                     // bumped on every load attempt, successful or not
    effective: ConfigView,               // what is running, which may differ from the file
    loaded_at: DateTime<Utc>,
    warnings: Vec<String>,               // unknown keys, dropped; the rest still applied
    error: Option<String>,               // why the last load was rejected
}

struct ConfigView {
    poll_interval_secs: Option<u32>,     // None = compiled-in default
    default_poll_interval_secs: u32,     // so a client can label "(default)"
    min_poll_interval_secs: u32,         // so a client can clamp before sending
    providers: Vec<ProviderSettings>,    // sorted by provider id
}

struct ProviderSettings {
    provider: ProviderId,
    enabled: bool,
    poll_interval_secs: Option<u32>,     // None = inherit the global value
    effective_poll_interval_secs: u32,   // what the scheduler uses
}

enum ConfigEdit {
    GlobalPollInterval(Option<u32>),                                  // None = clear
    ProviderPollInterval { provider: ProviderId, secs: Option<u32> }, // None = inherit
    ProviderEnabled { provider: ProviderId, enabled: bool },
}
```

**The daemon owns the file.** Clients never write `config.toml`; they send `SetConfig` and the daemon validates, writes atomically, and applies — one validation path, shared with the file watcher, and a rejection arrives as a reply rather than as silence. See [architecture.md](architecture.md) for the reload and validation rules.

**One edit, not a whole document.** A client that submitted a full `ConfigView` would clobber any external edit made between its last `GetConfig` and its write.

**`effective` is what is running, not what the file says.** A rejected load leaves the previous settings in force and reports why in `error`; only `ConfigState` carries both, which is what lets a client say "your edit did not take" instead of showing one and implying the other.

**`ProviderSettings` covers the registry, not just the file.** A compiled-in provider the config never mentions still gets a row, or a client would have no way to configure it.

`poll_interval_secs` on `AccountStatus` and `AccountHealth` is `0` when the account's provider is disabled, when no poller is registered, and while the account's local credential cannot be resolved — all three mean "no next poll to count down to". The third is a pause rather than a park: the daemon is re-checking that credential and resumes on its own once it resolves (see [architecture.md](architecture.md)). A client that wants to tell the two apart has what it needs without a new field — `ConfigView` says whether the provider is enabled, and `last_poll` carries the `AuthError` with the adapter's own remedy in it, beside a `last_success` saying how stale the windows are. The TUI does not yet draw that distinction: it branches on `poll_interval_secs > 0` alone, so a paused account currently renders like a disabled one. It reports the cadence the poll task is *actually* running at, so while the provider is rate limiting us it reads longer than `ConfigView.effective_poll_interval_secs`; a client counting down to the next poll wants the former, and one labelling the user's setting wants the latter. See [domain.md](domain.md) and [architecture.md](architecture.md).

**Windows carry their render hint.** `WindowView` pairs each `QuotaWindow` with the `RenderHint` its adapter produced, so warn/critical thresholds and the provider caveat (`"blocks entirely at cap"` vs. `"auto-downgrades, doesn't block"`) reach the client instead of being hardcoded there. Pairing them in one struct rather than parallel `Vec`s makes it impossible for the two to drift apart. See [providers.md](providers.md).

**Windows carry where they actually began.** `reset_at` minus the roll duration is a window's start only while the provider moves `reset_at` with the reset; where it does not, that arithmetic describes a window that is over, and every rate derived from it reads low. `observed_start` is the bracket around the last restart the daemon saw (`ObservedStart { not_before, not_after }` — see [domain.md](domain.md#window-rollovers)), and the client reconciles the two in `metrics::effective_window`.

It has to travel on `Status` rather than be derived client-side: the restart anchoring a weekly window is routinely days older than the 48 hours of series a dashboard fetches, and it is inferred from consecutive polls the client never sees individually. `None` means no restart has been observed, in which case the provider's own arithmetic is all there is. Added in protocol v6.

**A provider whose discovery failed is still listed.** Account discovery is a local credential read the daemon makes at startup, and repeats for a provider with no accounts until it finds some; while it fails the provider has no accounts at all, so `Status` is empty and nothing else on the wire says why. `ProviderHealth.discovery_error` carries the adapter's own message (for Claude on macOS, e.g. that no Claude Code login is in the Keychain), on a `ProviderHealth` with empty `accounts`. A client showing an empty dashboard asks `Providers` and reports it, rather than claiming to wait for a daemon it is already talking to. Added in protocol v7.

**`last_poll` vs. `last_success`.** `windows` is served from the latest *successful* poll while `last_poll` is the latest poll of any outcome. After a failure the two diverge, and only `last_success` says how stale the displayed windows are — a client that reports staleness from `last_poll` would claim fresh data it does not have.

**Bounded `History`.** Nothing prunes `quota_snapshot`, so an unbounded `since` could exceed the 1 MiB frame cap. `until` bounds the far end; `max_points` downsamples each window's series independently. The daemon applies `MAX_HISTORY_POINTS` (2 000 per window) even when `max_points` is `None` — see [domain.md](domain.md).

**`HistoryPage.rollovers` rides with the series rather than behind its own request.** The chart's boundary rules and the points they annotate describe one interval; two requests could straddle a poll and disagree about it. Rollovers are also the one thing in a page that must *not* be downsampled — `Storage::history` keeps the peak per bucket, which at the `7d` range is a ~21-minute smear, and a boundary read back off that could be placed anywhere inside it. Recording them at poll time and shipping them whole is what makes the instants exact. See [domain.md](domain.md#window-rollovers).

**`HistoryPage.earliest` bounds a scroll, not a page.** A page that begins at its own `since` looks identical whether that is the start of the history or merely the start of the query, so a client scrolling a chart backwards through time cannot tell when to stop — it would have to probe blindly, one empty page at a time, and a gap in the history would look like the end of it. `earliest` is a separate `MIN(ts)` over the stored series for that account and window, unaffected by `since`, `until`, or `max_points`. With it a client clips the scroll to the data: the oldest point lands on the left edge and no further, and a history narrower than the visible range pins the view to the right edge, where it keeps following the clock.

**`AwaitUpdate` waits on both of the daemon's clocks.** If the latest published `PollId` is newer than `since`, respond `Update` immediately; if the latest config generation is newer than `config_gen`, respond `Config` immediately; otherwise wait for whichever comes first, or reply `NoUpdate` at `timeout_ms`. One long-poll rather than two keeps the daemon the only clock that matters — a client learns about a `config.toml` edit without a second connection or a timer of its own. A client that does not track settings passes `config_gen: u64::MAX`.

## TUI ↔ protocol mapping

| Interaction | Trigger | Protocol |
| --- | --- | --- |
| Live dashboard, all accounts | on connect, then pushed | `Status` once, then loop on `AwaitUpdate { since, config_gen }` |
| Settings, current values | on connect + on opening the overlay | `GetConfig` → `Config` |
| Change a setting | keypress in the overlay | `SetConfig(edit)` → `Config`, or `Err(BadRequest, why)` |
| Someone edits `config.toml` | pushed | `AwaitUpdate` resolves with `Config` |
| Manual poll (selected account or all) | keypress | `PollNow` → `PollAccepted`; result arrives via next `AwaitUpdate` resolution |
| Trend chart for the selected window | selection/range/pan change **and every update** | `History { account, window, since, until, max_points }` → `HistoryPage` |
| Activity / poll log | tab active **and every update** | `RecentPolls { limit }` |
| Provider/account health | tab active **and every update** | `Providers` |
| Why there are no accounts | `Status` came back empty | `Providers` → `discovery_error` |
| Quit | keypress | close connection; daemon persists |
| Stop the daemon | explicit only | `Shutdown` — never bound to a casual key; gated behind a confirm prompt |

The TUI render loop is: connect → `Status` → render → loop `AwaitUpdate` → re-render on each `Update`/`NoUpdate`. **No polling timer on the TUI side**; the daemon's watch channel is the only clock.

The TUI presents a **single view** — a quota dashboard with a tabbed detail pane (Trend / Activity / Health) and three overlays drawn over it. The pane's data is refetched on every `Update` alongside `Status`, so detail content is live rather than a snapshot frozen when a tab was opened.
