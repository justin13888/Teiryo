//! Semantic palette.
//!
//! Everything is a named ANSI color rather than an RGB literal, so the
//! dashboard inherits whatever palette the user's terminal is themed with and
//! stays legible on light and dark backgrounds alike. Selection uses
//! `REVERSED` for the same reason: a fixed highlight background is only ever
//! right for one of the two.

use ratatui::style::{Color, Modifier, Style};

use teiryo_core::RenderHint;

/// Comfortably below the warn threshold.
pub const OK: Color = Color::Green;
/// Past the provider's warn threshold.
pub const WARN: Color = Color::Yellow;
/// Past the provider's critical threshold.
pub const CRIT: Color = Color::Red;
/// Chart series, active tab, headings.
pub const ACCENT: Color = Color::Cyan;
/// Secondary text and the unfilled part of a bar.
pub const DIM: Color = Color::DarkGray;
/// Block borders.
pub const BORDER: Color = Color::DarkGray;

/// A window that expired on schedule. Several can share a chart, so they read
/// as a background grid rather than as something to look at.
pub const BOUNDARY: Color = DIM;
/// The edges of the window currently in progress — the same color as its own
/// series, because they bracket it.
pub const BOUNDARY_LIVE: Color = ACCENT;
/// A reset the provider never advertised.
pub const BOUNDARY_SURPRISE: Color = WARN;

/// Color for a utilization ratio, using the provider's own thresholds rather
/// than a value baked into the TUI — "80% is fine" is a provider-specific
/// claim, which is exactly what [`RenderHint`] exists to carry.
pub fn severity(ratio: f64, hint: &RenderHint) -> Color {
    if ratio >= f64::from(hint.critical_threshold) {
        CRIT
    } else if ratio >= f64::from(hint.warn_threshold) {
        WARN
    } else {
        OK
    }
}

/// Border style for a pane, brightened while it holds the cursor. `j`/`k`
/// change meaning with focus, so which pane has it cannot be invisible.
pub fn border(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(BORDER)
    }
}

/// Style for the selected row.
pub fn selected() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Style for secondary text.
pub fn dim() -> Style {
    Style::default().fg(DIM)
}

/// Style for headings and emphasis.
pub fn heading() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Frames of the activity spinner shown while a manual poll is in flight.
pub const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// The spinner frame for a tick counter.
pub fn spinner_frame(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}
