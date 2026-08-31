//! Centered modals drawn *over* the dashboard.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Overlay, SettingsRow};
use crate::ui::{format, theme};

/// Render the active overlay.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    match app.overlay {
        Some(Overlay::Help) => render_help(frame, area),
        Some(Overlay::ConfirmShutdown) => render_confirm(frame, area),
        Some(Overlay::Settings) => render_settings(frame, area, app),
        None => {}
    }
}

/// A centered rectangle of at most `width` × `height`, shrinking to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    cell
}

const KEYS: &[(&str, &str)] = &[
    (
        "h / l · ← / →",
        "cursor to the quota list / the detail pane",
    ),
    ("j / k · ↓ / ↑", "in the list: move selection"),
    ("", "in the detail pane: scroll it sideways"),
    ("", "(Trend pans through time, g returns to now)"),
    (
        "g / G · Home / End",
        "first / last row, or both ends of the pane",
    ),
    ("Tab / Shift-Tab", "cycle detail tab"),
    ("1 / 2 / 3", "Trend / Activity / Health"),
    ("[ / ]", "trend range (1h · 6h · 24h · 7d)"),
    ("PgUp / PgDn", "scroll the detail pane's rows"),
    ("wheel", "scrolls the pane under the pointer, one line"),
    ("", "(on Trend: nudges time by a column, stopping"),
    ("", "at the present and at the oldest point)"),
    ("d", "show or hide the detail pane"),
    ("r · Enter", "poll the selected account now"),
    ("R", "poll every account now"),
    ("s", "daemon settings (poll intervals, providers)"),
    ("?", "this help"),
    ("Esc", "close overlay / collapse pane"),
    ("q · Ctrl-C", "quit (the daemon keeps running)"),
    ("Q", "stop the daemon (confirmation required)"),
];

/// What each derived number on a quota row means. Five numbers per row are
/// worth having only if there is somewhere that says what they are.
///
/// Every description has to fit the overlay's description column unwrapped:
/// the box sizes itself from the line count, so a wrapped line pushes the last
/// entries out through the bottom border.
const NUMBERS: &[(&str, &str)] = &[
    ("⟳ 1h 59m", "when the window rolls over"),
    ("1.03× pace", "burn rate since the window opened"),
    ("3.00× now", "the same rate over the recent stretch"),
    ("cap in 1h 50m", "how long the headroom lasts at that pace"),
    (
        "afford 0.95×",
        "the pace that spends the rest exactly by then",
    ),
    ("→103% at reset", "where the current pace lands"),
    ("▲ ▼ =", "above, below, or level with what it affords"),
];

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let mut lines = vec![Line::from(Span::styled(
        "Every key works from the one dashboard — there are no other screens.",
        theme::dim(),
    ))];
    lines.push(Line::from(""));
    lines.extend(KEYS.iter().map(|(keys, description)| {
        Line::from(vec![
            Span::styled(
                format!("  {keys:<22}"),
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(*description),
        ])
    }));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "What the numbers on a row mean.",
        theme::dim(),
    )));
    lines.push(Line::from(""));
    lines.extend(NUMBERS.iter().map(|(number, description)| {
        Line::from(vec![
            Span::styled(
                format!("  {number:<22}"),
                Style::default().fg(theme::ACCENT),
            ),
            Span::raw(*description),
        ])
    }));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        theme::dim(),
    )));

    let height = lines.len() as u16 + 2;
    let target = centered(area, 72, height);
    frame.render_widget(Clear, target);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::ACCENT))
                .title(Span::styled(" Keys ", theme::heading())),
        ),
        target,
    );
}

/// Overlay width, and the label column inside it, so values line up into one
/// scannable column and the selected row reverses across its whole width.
const SETTINGS_WIDTH: u16 = 78;
const LABEL: usize = 26;
const ROW: usize = SETTINGS_WIDTH as usize - 4;

/// The settings overlay: what the daemon is actually configured to do, where
/// that came from, and an editor for it.
///
/// Every value carries its provenance — `default`, `set here`, `override`,
/// `inherited` — because the same "60s" means something different depending on
/// whether the user set it, a provider override set it, or nothing did. That
/// ambiguity is the thing this overlay exists to remove.
fn render_settings(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(config) = &app.config else {
        let target = centered(area, 60, 5);
        frame.render_widget(Clear, target);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  waiting for the daemon's settings…",
                theme::dim(),
            )))
            .block(settings_block()),
            target,
        );
        return;
    };

    let view = &config.effective;
    let rows = app.settings_rows();
    let cursor = app.settings_cursor.min(rows.len().saturating_sub(1));

    let mut lines = vec![
        Line::from(Span::styled(format!("  {}", config.path), theme::dim())),
        Line::from(""),
    ];
    for (index, row) in rows.iter().enumerate() {
        let selected = index == cursor;
        let (label, value, origin) = match *row {
            SettingsRow::GlobalInterval => (
                "Poll interval".to_owned(),
                format::format_interval(
                    view.poll_interval_secs
                        .unwrap_or(view.default_poll_interval_secs),
                ),
                match view.poll_interval_secs {
                    Some(_) => "set here",
                    None => "default",
                },
            ),
            SettingsRow::ProviderEnabled(i) => {
                let provider = &view.providers[i];
                (
                    format!("{} · polling", provider.provider),
                    if provider.enabled { "on" } else { "off" }.to_owned(),
                    "",
                )
            }
            SettingsRow::ProviderInterval(i) => {
                let provider = &view.providers[i];
                (
                    format!("{} · interval", provider.provider),
                    format::format_interval(provider.effective_poll_interval_secs),
                    match provider.poll_interval_secs {
                        Some(_) => "override",
                        None => "inherited",
                    },
                )
            }
        };
        let text = format!(
            "  {}{label:<LABEL$}{value:<10}{origin}",
            if selected { "▸ " } else { "  " }
        );
        // Padded so the reversed highlight covers the full row rather than
        // stopping at the last glyph.
        let text = format!("{text:<ROW$}");
        lines.push(Line::from(Span::styled(
            text,
            if selected {
                theme::selected()
            } else {
                Style::default()
            },
        )));
    }

    if let Some(error) = &config.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  ✗ not applied: {error}"),
            Style::default().fg(theme::CRIT),
        )));
        lines.push(Line::from(Span::styled(
            "    the settings above are still what is running",
            theme::dim(),
        )));
    }
    for warning in &config.warnings {
        lines.push(Line::from(Span::styled(
            format!("  ⚠ {warning}"),
            Style::default().fg(theme::WARN),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  j/k move · +/- adjust · Enter toggle · Backspace inherit · Esc close",
        theme::dim(),
    )));

    let height = lines.len() as u16 + 2;
    let target = centered(area, SETTINGS_WIDTH, height);
    frame.render_widget(Clear, target);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(settings_block()),
        target,
    );
}

fn settings_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(" Settings ", theme::heading()))
}

fn render_confirm(frame: &mut Frame<'_>, area: Rect) {
    let target = centered(area, 60, 7);
    frame.render_widget(Clear, target);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Stop the daemon?"),
            Line::from(Span::styled(
                "Quota polling halts until the next launch, and history stops accumulating.",
                theme::dim(),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "y",
                    Style::default()
                        .fg(theme::CRIT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" to confirm · any other key to cancel"),
            ]),
        ])
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::CRIT))
                .title(Span::styled(
                    " Stop daemon ",
                    Style::default()
                        .fg(theme::CRIT)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        target,
    );
}
