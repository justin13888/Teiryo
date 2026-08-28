# Dashboard & Credential Identity

Finalized design for the TUI's quota presentation and for the account identity
that history hangs off. Supersedes the single-window trend chart and the
synthetic `claude:default` account.

## Why this changes

Two defects drove it:

- The trend chart showed **one** window at a time, so the 5-hour session and
  the weekly buckets were never on screen together — the only view where
  "am I about to be blocked, and by which limit?" is answerable at a glance.
  Worse, the raw `used` series falls diagonally through each reset, drawing a
  descent that never happened.
- `claude:default` is a constant. The credentials file behind it can be
  replaced by a different login — different account, different org, different
  rate-limit tier — with no signal, silently splicing two accounts' quota into
  one series.

## Screen 1 — the dashboard: one row per window

Every window is always visible. No selection is needed to read the numbers;
selection only drives which series the chart emphasizes.

```
Teiryō  claude/max-20x · a1b2  4 windows         ● live

 Session — 5 hour   ██████░░░░  62%  ▁▂▄▆█
   1.4× pace · cap in ~48m · resets 1h47m
 Weekly — all       ███░░░░░░░  38%  ▁▁▂▃▄
   0.9× pace · resets 3d 04h
 Weekly — Opus      ███████░░░  71%  ▂▄▆▇█
   1.3× pace · cap in ~2d · resets 3d 04h
 Weekly — Sonnet    ██░░░░░░░░  22%  ▁▁▁▂▂
   0.5× pace · resets 3d 04h
```

Per row: bar, utilization, inline sparkline, then a continuation line with
`pace`, `eta_to_cap`, and the reset countdown — all three already exist in
`crates/teiryo/src/metrics.rs` and are simply promoted from the detail pane.
The continuation line is dropped when the row is a superseded account's, or
when height is tight (bars survive, detail lines go first).

The sparkline is the same series the chart draws, downsampled to the row
width. It needs recent history in `AccountStatus`, so the daemon pushes a short
per-window tail (the last ~64 points) rather than the TUI issuing a history
request per row.

## Screen 2 — the trend: one overlaid chart

All windows on one axis. Both are percent-valued, so a shared `0..100` y-axis
is honest and no dual-scale is needed.

```
100│                    ╭─ cap
   │          ╭────╯  ← focus: Session — 5 hour
   │     ╭───╯ ╌╌╌╌╌╯  ideal pace
 50│────╯╌╌╌╌╯
   │······················· Weekly — all      (dim)
  0│······················· Weekly — Sonnet   (dim)
    -24h              now
```

Three rules make it readable:

- **Rollover-split.** A reset ends the current segment and starts a new one at
  the new window's start. No diagonal drop. Detected from `reset_at` moving
  forward, not from `used` decreasing — a provider correction that lowers
  `used` mid-window is not a rollover and must not break the line.
- **Ideal-pace guide.** A dashed line from the focused window's start (0%) to
  its `reset_at` (100%), i.e. linear burn. The gap between the series and the
  guide *is* `metrics::pace`, drawn: above the guide means on track to hit the
  cap early. Only the focused window gets one — four guides plus four series in
  a 20-row pane is noise.
- **Focus emphasis.** `j/k` moves focus; the focused series draws in accent
  with its guide and rollover breaks, the rest draw dim. Context without
  competition.

The existing critical-threshold line stays. Ranges stay `1h/6h/24h/7d`; over
`1h` the weekly series are near-flat by nature, which is information, not a
bug.

### Window boundary rules — implemented

Vertical rules mark where each window began and ends, so a series restarting
from zero has a visible cause. Implemented in `render_trend`; the geometry is
`metrics::boundaries`, the events come from `HistoryPage.rollovers`.

```
100│      ┊              ┊          ┊       ┊
   │      ┊              ┊     ╭────╯       ┊
 50│  ╭───╯     ╭────────╯   ╭─╯            ┊
   │╭─╯         ┊          ╭─╯              ┊
  0│╯           ┊        ╭─╯                ┊
    -1d 0h      ┊    -11h 00m        now  +1h 59m
             rollover   early     window     next
              (dim)    (yellow)   start      reset
                                    (cyan, both)
```

- **Past boundaries are observed, never predicted.** They come only from
  recorded rollovers. A rolling window is anchored to first use, so after an
  idle stretch the next one starts later than the last ended — a fixed lattice
  of `reset_at - k·span` would draw rules where nothing happened.
- **The live window's own edges are derived.** `reset_at - span` and `reset_at`,
  from the current `QuotaWindow`. The start is drawn even when history is too
  short to have observed that rollover. When the start coincides with a recorded
  rollover — the same event from the other side — only one rule is drawn.
- **The axis grows to reach the next reset.** It lies in the future, so x
  extends past `now` by up to `FUTURE_LEAD_MAX` (20%) of the visible range and
  the right label runs forwards, `+1h 59m`. Past that ceiling the rule is
  dropped rather than squashing the history — a weekly reset three days out has
  no business compressing a `1h` chart, and the row's own countdown already
  says when it lands. A **panned** chart never grows: it is not showing the
  present, so the current window's reset does not belong off its right edge.
- **Colors carry the severity**, all from the existing named-ANSI palette:
  `BOUNDARY` (dim) for a scheduled rollover — several may share a chart, so
  they read as a background grid; `BOUNDARY_LIVE` (accent) for the live
  window's two edges, matching its own series; `BOUNDARY_SURPRISE` (warn) for
  anything the provider did not advertise.
- **An unannounced drop gets a marker, not a rule.** Inferred from `used`
  alone, it is not trustworthy enough to break the chart — the same reasoning
  as Rollover-split above.
- **Nothing is drawn without a cap.** A window with no `reset_at` and no
  recorded rollover charts exactly as it did before this existed, which is the
  case for a provider or credential that enforces no such limit. The Claude
  parser omits absent buckets entirely, so such a window usually has no row at
  all.

The footer states the counts in words — `· 3 reset(s) · 2 unexpected` — because
a rule is easy to miss on a busy chart and a surprise reset should not be left
to a color.

## Credential identity

A credential change produces a **new account**, so history can never silently
mix two logins. The account id must therefore be stable across ordinary token
refresh — which rules out hashing the access token (rotates every few hours)
and makes the refresh token a poor anchor too.

Identity comes from `GET {base}/api/oauth/profile`, verified to return:

```jsonc
{
  "account":      { "uuid": "…", "email": "…", "display_name": "…" },
  "organization": { "uuid": "…", "name": "…", "rate_limit_tier": "default_claude_max_20x",
                    "organization_type": "claude_max", "subscription_status": "active" },
  "application":  { "slug": "claude-code" }
}
```

- **Account id**: `claude:{first 8 of account.uuid}`, e.g. `claude:b5a098c4`.
  Stable across refresh, re-login as the same user, and machine moves.
- **Label**: derived for display — `max-20x · b5a098c4`, from
  `organization.rate_limit_tier` — but *not* part of the id. A plan upgrade
  must not fork the history.
- **Caching**: profile is fetched only when the credentials file's mtime or
  token changes, never per poll. `/api/oauth/usage` already returns **429**
  under the current cadence; a second per-poll request would make that worse.
- **Fallback**: if profile is unreachable (404/network), fall back to
  `sha256(refresh_token)[..8]` and mark the account `identity: derived` so the
  UI can explain a split that may be spurious.

`email` and `display_name` are personal data and are neither stored nor
rendered; the uuid and tier are sufficient to tell accounts apart.

### Storage

```sql
ALTER TABLE account ADD COLUMN org_uuid TEXT;          -- organization.uuid
ALTER TABLE account ADD COLUMN rate_limit_tier TEXT;   -- display + tier-change detection
ALTER TABLE account ADD COLUMN identity TEXT NOT NULL DEFAULT 'profile'; -- 'profile' | 'derived'
ALTER TABLE account ADD COLUMN first_seen INTEGER;
ALTER TABLE account ADD COLUMN last_seen INTEGER;      -- drives the dimmed-row state
```

No separate `credential` table: with the change modelled as a new account, the
account row *is* the credential record. `poll_event.account_id` already carries
the association, so every historical point is attributable without a new FK.

`last_seen` updates on each successful poll. Existing `claude:default` rows are
left alone — they are pre-identity history, shown as a superseded account.

### Superseded accounts

Kept visible, dimmed, below the live one, with history still chartable:

```
 claude/max-20x · a1b2                       ● live
   Session — 5 hour  ██████░░░░ 62%
   …
 claude/max-5x · 9f3c                  last seen 2h ago
   Session — 5 hour  ░░░░░░░░░░ —
   (history preserved, press ↵ to chart)
```

Nothing is auto-hidden and nothing vanishes; a login switch is visible as a
state of the dashboard rather than as an unexplained discontinuity.
