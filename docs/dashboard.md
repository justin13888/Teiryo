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
   ▲ 1.40× pace · ▲ 2.10× now (1.6–2.6) · cap in 48m (37m–1h05m) · afford 0.62× · →140% (120–165%) at reset
 Weekly — all       ███░░░░░░░  38%  ▁▁▂▃▄  ⟳ 3d 04h
   ▼ 0.79× pace · = 0.90× now (0.7–1.1) · cap in 6d 2h (…) · afford 1.27× · →79% (71–88%) at reset
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
  running, as wide as a daemon outage otherwise. A field measured from a start
  known only that loosely is marked `~` rather than presented as measured:
  `~1.86× pace`, and `avg cap in ~2h` / `avg →~186% at reset` where those fall
  back to the average. The threshold is a twentieth of the effective window, a
  fraction for the same reason the pace floor is one. `afford` carries no mark,
  because it divides the remaining budget by the time left to `reset_at` and
  never touches the start; nor does `now`, which is measured over the recent
  series, nor anything projected from it — a pace is a slope times the span,
  and projecting divides the span back out, so the start cancels. The help
  overlay lists the mark.

Only the daemon can supply this. The TUI fetches 48 hours of series, and the
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
  the number, at `1 / MIN_ELAPSED_FRACTION`. There is deliberately no absolute
  floor beside it: an effective span is `reset_at` less an *observed* start, so
  a restart seen an hour before the reset leaves a window whose twentieth is
  three minutes, and a seconds threshold would blank the row past the point
  this rule promises it returns. `now` is unaffected and carries the signal
  over that stretch, which is where a burst right after a reset shows.
- **`now`** — `estimate_rate`, the same scale over the recent end of the
  series, with its 80% interval: `2.10× now (1.6–2.6)`. This is the one that
  moves when the user does, and the one everything below projects from. It is
  the only field needing history, and it stays blank until enough of it
  exists. See below for what it is measured over and how sure it is.
- **`cap in`** — `project(…).runway`, how long the remaining headroom lasts at
  the recent rate, with the runway at each end of its interval:
  `cap in 48m (37m–1h05m)`, `∞` where the slow end is zero — and
  `cap in ∞ (6h–∞)` when the recent stretch was flat, because the fast end of
  the interval still has a cap. Reported whether or not the window resets
  first: a runway past the reset is a real answer to "how long could I keep
  this up". It is drawn in warn when the runway at the *fast* end of the
  interval lands before the reset — a cap the interval calls plausible — and
  dim otherwise, because then the rollover, not the cap, is what arrives. It used to project from
  `pace`, the average since the window opened, which let an idle morning hide
  a hot afternoon; with no recent rate it still does, and says `avg cap in`.
- **`afford`** — `affordable_pace`, what is left over the time left. The
  forward-looking mirror of `pace`, and the field that answers "may I spend
  the rest of this window in the next day and a half?" without arithmetic.
- **`→…% at reset`** — projected utilization at the recent rate,
  `u + rate × time left`, with its interval: `→140% (120–165%) at reset`.
  Projected from the average instead — the `avg →…%` fallback — it is
  numerically `pace` itself (`u + (u/E)(1-E) = u/E`), which is why it is the
  first field shed when the row narrows.

Fields are appended only while they fit, so a narrow terminal sheds them from
the right, `pace` last; a field with an interval first drops the interval and
keeps its value. The whole continuation line is dropped when the row is
a superseded account's, or when height is tight (bars survive, detail lines go
first) — in which case `pace` returns to a column on the gauge line.

### What `now` is measured over, and how sure it is

A series is not a continuous record. The daemon stops and resumes hours later;
the provider restarts the window without saying so. Neither hole is a
measurement, and averaging across one invents a rate nobody burnt. So
`estimate_rate` sorts and deduplicates the readings — nothing on the wire
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
- at or after the effective start.

A stretch whose only content is a correction has **no** rate rather than
`0.00×`, and one with usable readings on the far side of it is measured from
there. A slip under `FALL_EPSILON` — half of the last digit the row prints — is
not a cut, because losing the field over a change nobody can see is worse than
reading across it.

**The rate.** Usage is a cumulative counter, so the burn over a stretch divided
by its length *is* the average rate over it — the ratio estimator `Σd / Στ`
over the steps between readings — exact up to rounding at the two ends. A
least-squares slope is not, and was tried: it weights the middle of the
stretch, so one rounding step landing mid-stretch threw a slow weekly burn to
`9.6×` against a true `0.71×`.

**The interval** is 80%, and its half-width is the larger of:

- the scatter of the steps around that rate, as a standard error widened by
  Student's t for how few steps there are — what bursty use leaves undecided
  about the rate going forward;
- one rounding step over the stretch, `q / T`. Providers round — Claude reports
  whole percent — and the true burn between two readings lies within one step
  of the reported one, so this end is exact: a steady rate is always inside the
  interval however few readings there are. Successive readings of a slow climb
  share their rounding error, so no regression can claim better. `q` is
  inferred from the readings themselves — whole numbers, halves or tenths.

The lower end never goes below zero. An estimate from two readings is never
called precise, however narrow: its interval is the rounding bound alone, with
no scatter to say how bursty the use was.

**How far back.** Recency argues for a short stretch and precision for a long
one, and which wins depends on the series: a 5-hour window in whole percent
resolves a half-hour slope to a tenth of the scale, a weekly window over the
same half hour to nearly three. So the lookback starts at a tenth of the window
(15 minutes to 12 hours) and doubles while the interval is wider than `0.25×`
or a quarter of the rate, until it is narrow enough or the stretch runs out.
Where it ends up is recorded, and a wide interval on a short stretch is the
honest output when that is all the data allows — the alternative was a
two-decimal figure from two readings, printed as if it were measured.

`max_gap` is `metrics::gap_tolerance`: four missed polls at the account's own
cadence, never under ten minutes. Derived rather than fixed, so a user polling
every ten minutes is not permanently stale while one polling every thirty
seconds gets a rate averaged over an outage.

The field is **dropped entirely** when the newest reading is itself older than
`max_gap`. A series that stopped has no current rate, and printing the last one
it had under a `now` label is worse than printing nothing.

The rows read **48 hours** of history per account: two doublings past the
12-hour lookback a weekly window starts from. Twelve alone left a weekly
window nothing to widen into — one rounding step over twelve hours of a week is
already wider than the precision target. The charts slide the same estimator
over their own range, downsampled, at a gap tolerance widened to the bucket, so
over a long range the chart's rate is the coarser of the two.

The recorded rollover list is deliberately not what guards any of this:
`boundaries` filters unannounced rollovers out, and an unannounced rollover is
exactly the drop in `used` that would otherwise read as burn. The TUI fetches
the series with one `History` request per account (`window: None`), alongside
the `Status` it already refreshes.

The sparkline is the same series the chart draws, downsampled to the row width.
It is the one part of this row still unbuilt. It needs no new plumbing: the
per-account `History` fetch that feeds `estimate_rate` already carries every
window's recent series, which supersedes the earlier plan of pushing a
per-window tail in `AccountStatus` and the protocol change that would have
taken.

## Screen 2 — the trend: chart views

The Trend tab cycles four views — `v`/`V`, or a click on the strip along its
top. The detail pane grows to half the terminal, up to 32 rows, so two charts
can stack in it.

- **Usage·Pace** — the selected window's usage over its burn rate, on one time
  axis and one label column, so the two plots line up cell for cell:
  - *Usage* keeps everything below: the readings drawn as a line and as dots,
    the critical threshold, the boundary rules, the unannounced-drop markers.
    On a live chart it adds the **projection cone**: from the newest reading
    to the reset, or to the cap where that comes first, at the recent rate and
    at both ends of its interval — measured from the same series as the row,
    so the cone and `cap in` cannot disagree.
  - *Pace* draws the recent rate as it stood at every reading, with its 80%
    band, the average since the window opened, and the 1.0 line. Each reading
    is measured against its own window instance, so the line breaks at a
    rollover instead of bridging it.
  - A one-row **coverage strip** under both: `•` a column holding a reading,
    `─` interpolation between readings close enough to be one stretch, `░` a
    hole no rate can be measured across. It is what explains a missing or
    wide interval.
- **Forecast** — the projected at-reset figure, with its band, as it stood at
  every reading, over the pace the window could afford against the pace it ran
  at. Where the forecast drifts, and when the slack ran out, are both read off
  it.
- **All windows** — every window of the account on one 0–100% axis, the
  selected one in accent and the rest each in a colour of its own (none of
  them a severity colour), each line broken at its rollovers.
- **History** — the peak each past window instance reached before it rolled
  over, over the last 30 days, coloured by severity or flagged when the reset
  was unannounced; and burn by hour of the local day over the last week.

`f` **splits** Usage·Pace and Forecast into two columns: the session window
(a span of a day or less) on the left and a weekly one on the right, whichever
of the two is selected.

Every series is an ordinary `History` request; the extra ones are made only
for the view that draws them, and History's month of rollovers and week of
readings are reused for ten minutes rather than refetched on every poll. A week drawn in 480 points has a reading every
21 minutes, so the charts measure rates with a gap tolerance widened to twice
the downsampling bucket — otherwise every step of a long range reads as an
outage.

### Reading a chart

Click any chart and a cursor drops on the nearest reading — or on the bar
under the pointer — and the pane's title reads out everything at it: the time,
usage, `now` with its interval, the average, the projection and `afford`, as
they stood then. The cursor runs through every time chart on screen, since they
share an axis. With the pane focused, Enter puts it on the newest reading and
`,`/`.` step it a reading at a time. The same point again, a right click or Esc
puts it away; so does panning, `g`, changing the range, or toggling the split.
A live refetch can drop the very reading the cursor was on — the daemon keeps
the highest reading of each bucket, and the buckets move with the chart's
edge — so the cursor moves to the nearest reading that came back, and goes
away once its instant has scrolled off the series. Enter means this only in a
focused Trend pane, and opens Details in Activity; from the list it still
polls, as `r` does everywhere.

Plots are laid out by hand rather than by ratatui's axes, so the rect recorded
for hit-testing is exactly the rect the data occupies and a column maps back to
an instant without guessing at label widths.

Three rules from the original single chart still hold on Usage:

- **Rollover-split.** Intended: a reset ends the current segment and starts a
  new one at the new window's start, detected from `reset_at` moving forward,
  not from `used` decreasing. The All windows chart and the Pace and Forecast
  lines break this way; the Usage line itself is still drawn unbroken, as it
  was before this change, with the boundary rules marking each reset.
- **Ideal-pace guide.** The gap between the series and the linear burn from the
  window's start to its `reset_at` *is* `metrics::pace`; the Pace chart below
  now draws it directly.
- **Focus emphasis.** `j/k` moves focus between windows; the charts follow it.

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

## Activity details

The Activity tab is a log with a row cursor. `j`/`k`, PgUp/PgDn and `g`/`G`
move it; a click puts it on a row. Enter, or a click on the row already
selected, opens **Details** — beside the log where the pane is at least 90
columns wide, in its place otherwise — and the same again shuts it, as does
Esc. Details lists every field the poll event carries: its id, local and UTC
time, provider, account, trigger (and which client asked), outcome, the full
error text the log column truncates, the suggested retry, latency, and for a
success each window's scope, length, usage and reset.

The cursor follows its poll by id, because the log is refetched on every
update and new polls arrive at the top. The log only scrolls to the cursor
after a key moves it, so the wheel can still browse without dragging it.
Sideways scrolling, which `j`/`k` used to do on this tab, moves to `<`/`>`.

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
