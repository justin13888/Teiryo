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

 Session — 5 hour   ██████░░░░  62%  ▁▂▄▆█  ⟳ 1h 47m
   ▲ 1.40× pace · ▲ 2.10× now · cap in 48m · afford 0.62× · →140% at reset
 Weekly — all       ███░░░░░░░  38%  ▁▁▂▃▄  ⟳ 3d 04h
   ▼ 0.79× pace · = 0.90× now · cap in 6d 2h · afford 1.27× · →79% at reset
```

Per row: bar, utilization, inline sparkline and the reset countdown, then a
continuation line of everything derived from them. All of it comes from
`crates/teiryo/src/metrics.rs` and needs no data the row does not already
carry, except the recent rate — see below.

### The effective window

Every field below is usage measured against the window's start, so the start
has to be right. `reset_at` minus the roll duration gives it only while the
provider moves `reset_at` with the reset. Where it does not — a weekly quota
that restarts early and keeps publishing the old instant — that subtraction
names a window which is over, and every number built on it reads *low*: it
divides real usage by a stretch of clock most of which belonged to a window
already paid for. A weekly window that really restarted on Tuesday, 5% gone by
Tuesday evening, read `0.07× pace` — "you are barely touching it" — against a
true `0.60×` and climbing. Low is the dangerous direction.

So the daemon publishes `WindowView.observed_start`, the bracket around any
restart it actually saw (see [domain.md](domain.md#window-rollovers)), and
`metrics::effective_window` reconciles the two:

- Only an **unannounced** restart is published at all. Where `reset_at` moved,
  the provider stated the new window's start precisely and there is nothing to
  correct — see [domain.md](domain.md#window-rollovers).
- The observed restart then wins only where it puts the start **later** than
  the provider's arithmetic. That is the one direction an unannounced reset can
  push it; an earlier restart belongs to a window that has since ended, and no
  window began before its own `reset_at - span` and is still running.
- `EffectiveWindow::span()` is `reset_at - start`, so a window that restarted
  early is genuinely **shorter and still carries a full budget**. Both halves
  matter: the same usage is a faster burn, and there is more left to afford
  than the clock alone suggests. `pace`, `cap in`, `afford` and `→…% at reset`
  all move together with it.
- `start_uncertainty` carries the bracket's width — a poll interval in normal
  running, as wide as a daemon outage otherwise. Nothing renders it yet; it is
  there so a field derived from a days-wide bracket can be dimmed rather than
  presented as measured.

Only the daemon can supply this. The TUI fetches 12 hours of series, and the
reset anchoring a weekly window is routinely days older than that, so there is
nothing client-side to reconstruct it from.

The five derived fields, and why each is not the others:

- **`pace`** — usage over elapsed time, both as fractions of the window. The
  average *since the window opened*, so it answers "how have I been going",
  and an idle stretch keeps it low however hard the last hour ran. Blank for
  the window's first twentieth (`MIN_ELAPSED_FRACTION`): whatever has been
  used divided by a nearly-zero elapsed fraction is a number in the tens that
  says nothing, and `cap in` inherits it as an hour-and-a-half warning
  extrapolated from five minutes. The threshold is a *fraction* rather than a
  duration because the same five minutes is a twentieth of a short window and
  a fourteen-hundredth of a weekly one; flooring the fraction is what bounds
  the number, at `1 / MIN_ELAPSED_FRACTION`. `now` is unaffected and carries
  the signal over that stretch, which is where a burst right after a reset
  shows.
- **`now`** — `recent_pace`, the same scale over a lookback of a tenth of the
  window. This is the one that moves when the user does. It is the only field
  needing history, and it stays blank until enough of it exists.
- **`cap in`** — `runway`, how long the remaining headroom lasts at `pace`.
  Reported whether or not the window resets first: a runway past the reset is
  a real answer to "how long could I keep this up", and it is drawn dim
  instead of warn, because the rollover, not the cap, is what arrives.
- **`afford`** — `affordable_pace`, what is left over the time left. The
  forward-looking mirror of `pace`, and the field that answers "may I spend
  the rest of this window in the next day and a half?" without arithmetic.
- **`→…% at reset`** — projected utilization, which is *numerically identical*
  to `pace` (`u + (u/E)(1-E) = u/E`). Kept only because an outcome reads more
  plainly than a ratio, and so the first field shed when the row narrows.

Fields are appended only while they fit, so a narrow terminal sheds them from
the right, `pace` last. The whole continuation line is dropped when the row is
a superseded account's, or when height is tight (bars survive, detail lines go
first) — in which case `pace` returns to a column on the gauge line.

### What `now` is measured over

A series is not a continuous record. The daemon stops and resumes hours later;
the provider restarts the window without saying so. Neither hole is a
measurement, and averaging across one invents a rate nobody burnt. So
`recent_pace` sorts and deduplicates the readings — nothing on the wire
promises an ordering, and a repeated poll is not a second measurement — then
walks **backwards from the newest**, extending the stretch while each step back
is all of:

- no longer than `max_gap`,
- not a **fall in `used`** — deliberately any fall, not one large enough to be
  a reset. Inside a single window instance usage only climbs, so a fall is
  always either a restart or a revision, and neither leaves the readings on
  its two sides comparable. Judging how big it was would only reintroduce the
  question at a smaller scale: a restart from under `MIN_RESET_DROP` is
  invisible to the detector, and subtracting straight across one would report
  `0.00× now` over exactly the stretch a fresh window was being burnt through,
- carrying this window's `reset_at` within `RESET_TOLERANCE`, which catches an
  announced rollover,
- at or after the effective start and the lookback floor.

The difference is what the field now shows on a revision: a stretch whose only
content is a correction has **no** rate rather than `0.00×`, and one with usable
readings on the far side of it is measured from there. A slip under
`FALL_EPSILON` — half of the last digit the row prints — is not a cut, because
losing the field over a change nobody can see is worse than reading across it.

`max_gap` is `metrics::gap_tolerance`: four missed polls at the account's own
cadence, never under ten minutes. Derived rather than fixed, so a user polling
every ten minutes is not permanently stale while one polling every thirty
seconds gets a rate averaged over an outage.

The field is **dropped entirely** when the newest reading is itself older than
`max_gap`. A series that stopped has no current rate, and printing the last one
it had under a `now` label is worse than printing nothing.

The recorded rollover list is deliberately not what guards any of this:
`boundaries` filters unannounced rollovers out, and an unannounced rollover is
exactly the drop in `used` that would otherwise read as burn. The TUI fetches
the series with one `History` request per account (`window: None`), alongside
the `Status` it already refreshes.

The sparkline is the same series the chart draws, downsampled to the row width.
It is the one part of this row still unbuilt. It needs no new plumbing: the
per-account `History` fetch that feeds `recent_pace` already carries every
window's recent series, which supersedes the earlier plan of pushing a
per-window tail in `AccountStatus` and the protocol change that would have
taken.

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
- **The live window's own edges are derived.** `EffectiveWindow::start` and
  `reset_at` — so the rule sits where the window actually began, and the chart
  and the row's numbers can never describe different windows. The start is
  drawn even when history is too short to have observed that rollover. When it
  coincides with a recorded rollover — the same event from the other side —
  only one rule is drawn.
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
  as Rollover-split above. It *does* anchor the effective window: whether to
  break a drawn series and where to measure a rate from are different
  questions, and the evidence clears the second bar. It is also the **only**
  kind that anchors one, because it is the only kind that says something
  `reset_at` does not already say.
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
