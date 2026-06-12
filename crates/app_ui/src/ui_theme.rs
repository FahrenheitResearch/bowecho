//! The density + typography contract (ui-overhaul spec §4): every magic
//! number the chrome is allowed to use, in one place. `configure_style`
//! (main.rs) and every panel read from here.
//!
//! Rules the constants encode:
//! - every button/row/slider is `ROW_H` tall — no exceptions;
//! - layer rows and the tab bar pack at `ROW_SPACING_X`;
//! - glyph-only buttons are allowed exactly for the universal set already
//!   in use (↑ ↓ ✕ ⚙ ↻ ◀ ▶ ⏸ ◉) and each MUST carry `on_hover_text`;
//! - `LIVE_COLOR` marks live chips/dots only — never decoration;
//! - row name widths come in exactly three tiers; never invent a fourth.

use eframe::egui::Color32;

/// Every button / row / slider height (the old `PANEL_BUTTON_HEIGHT`).
pub const ROW_H: f32 = 24.0;
/// Horizontal packing inside layer rows + the sidebar tab bar.
pub const ROW_SPACING_X: f32 = 3.0;
/// Vertical air above a section header (paired with a separator).
pub const SECTION_SPACING: f32 = 8.0;

/// Section + rail-group header text (matches guide.rs's subheads).
pub const SUBHEAD_COLOR: Color32 = Color32::from_rgb(148, 160, 172);
/// Editing-pane notice, keycap hints.
pub const ACCENT_COLOR: Color32 = Color32::from_rgb(120, 168, 220);
/// Live chips/dots ONLY — never decorative.
pub const LIVE_COLOR: Color32 = Color32::from_rgb(110, 245, 130);

/// No combo box renders wider than this (the site combo fills, capped by
/// its row's buttons).
pub const COMBO_MAX_W: f32 = 220.0;

/// Layer-row name width tiers: site IDs / standard / placefile titles.
pub const NAME_W_SITE: f32 = 42.0;
pub const NAME_W_STD: f32 = 96.0;
pub const NAME_W_WIDE: f32 = 150.0;

/// Shared opacity-slider width for the unified layer rows.
pub const LAYER_ROW_SLIDER_WIDTH: f32 = 56.0;

pub const SIDEBAR_DEFAULT_WIDTH: f32 = 380.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 300.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 560.0;
