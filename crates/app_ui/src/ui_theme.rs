//! The density + typography contract (ui-overhaul spec §4): every magic
//! number the chrome is allowed to use, in one place. `configure_style`
//! (main.rs) and every panel read from here.
//!
//! Rules the constants encode:
//! - every button/row/slider is `ROW_H` tall — no exceptions;
//! - layer rows and the tab bar pack at `ROW_SPACING_X`;
//! - glyph-only buttons are allowed exactly for the universal set already
//!   in use (⏶ ⏷ × ⚙ ↻ ◀ ▶ ⏴ ⏵ ⏸ ⏺ ○) and each MUST carry `on_hover_text`.
//!   Every glyph in the set is COVERED by the default font chain — gated by
//!   `fonts::tests::glyph_button_vocabulary_is_covered`. The pre-theme set
//!   used ✕ ◉ ↑ ↓ ▾ ▼ ▸, all of which rendered as replacement boxes;
//! - `LIVE_COLOR` marks live chips/dots only — never decoration;
//! - layer-row name width is ONE flexible column shared by every row
//!   (`panel_kit::rail_name_width`); never invent per-row widths.

use eframe::egui::Color32;

/// Every button / row / slider height (the old `PANEL_BUTTON_HEIGHT`).
pub const ROW_H: f32 = 24.0;
/// Horizontal packing inside layer rows + the sidebar tab bar.
pub const ROW_SPACING_X: f32 = 3.0;
/// Vertical air above a section header (paired with a separator).
pub const SECTION_SPACING: f32 = 8.0;

/// Section + rail-group header text (candidate A value; prefer
/// [`subhead_color`] so candidate B applies — see the theme section below).
pub const SUBHEAD_COLOR: Color32 = Color32::from_rgb(194, 204, 217);
/// Section rule (candidate A value; prefer [`section_rule_color`]). The
/// theme pass demoted this from a bright band to a structural hairline —
/// the uppercase titles carry the hierarchy now.
pub const SECTION_SEPARATOR_COLOR: Color32 = Color32::from_rgb(57, 67, 79);
/// Editing-pane notice, keycap hints (candidate A value; prefer
/// [`accent_color`]).
pub const ACCENT_COLOR: Color32 = Color32::from_rgb(90, 180, 228);
/// Live chips/dots ONLY — never decorative (candidate A value; prefer
/// [`live_color`]).
pub const LIVE_COLOR: Color32 = Color32::from_rgb(95, 224, 128);

/// No combo box renders wider than this (the site combo fills, capped by
/// its row's buttons).
pub const COMBO_MAX_W: f32 = 220.0;

/// Shared opacity-slider width for the unified layer rows.
pub const LAYER_ROW_SLIDER_WIDTH: f32 = 56.0;

pub const SIDEBAR_DEFAULT_WIDTH: f32 = 380.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 300.0;
/// Generous cap (field request: free sidebar resizing) — the panel edge
/// drags; the map pane absorbs whatever is left.
pub const SIDEBAR_MAX_WIDTH: f32 = 900.0;

// ---------------------------------------------------------------------------
// THEME PASS (ui-refresh-plan §4): the chrome ramp + accent + status colors,
// as TWO candidates for an owner A/B.
//
//   Candidate A (default): "slate" — near-black blue-grey ramp, the current
//     cyan-family accent disciplined to selection + live indicators.
//   Candidate B (`BOWECHO_THEME_B=1`): "graphite + amber" — a pure-neutral
//     ramp with an amber accent; genuinely different, still restrained.
//
// A/B SCAFFOLDING: `theme_b_requested` + `THEME_B` + the env switch are
// TEMPORARY capture plumbing for the integrator and will be deleted once the
// owner picks. Deliberately no UI surface for the switch.
//
// Rules the ramp encodes (spec section 4):
// - three deliberate neutral levels: `inset` (troughs/text edits) < `bg`
//   (panels) < `raised` (buttons/chips), plus `hover` above `raised`;
// - ONE accent, used ONLY for selection + live/active emphasis;
// - status colors carry MEANING only: `live` green, `warn` amber, `alert`
//   red — never decoration;
// - hairline separators (`hairline`, `section_rule`) replace heavy rules;
// - every text tier holds WCAG-ish contrast on its worst background —
//   enforced by the `contrast_ratio` tests below (weak text >= 4:1).
// ---------------------------------------------------------------------------

/// One chrome palette. Fields are the only colors `configure_style`
/// (main.rs) and the panel kit are allowed to paint chrome with.
pub struct Theme {
    /// Deepest level: slider troughs, text-edit interiors (`extreme_bg_color`).
    pub inset: Color32,
    /// Panel/window background.
    pub bg: Color32,
    /// Raised furniture: buttons, chips, combo boxes.
    pub raised: Color32,
    /// Hover state above `raised`.
    pub hover: Color32,
    /// Pressed/active widget fill (accent-tinted, momentary).
    pub active: Color32,
    /// Faint striping (tables).
    pub faint: Color32,
    /// Generic hairline separators + widget outlines.
    pub hairline: Color32,
    /// Window border stroke (one step brighter than `hairline`).
    pub outline: Color32,
    /// The kit's section rule — structural, no longer a bright band.
    pub section_rule: Color32,
    /// Primary text.
    pub text: Color32,
    /// Strong/hovered text (egui derives `.strong()` from the active
    /// widget's fg stroke).
    pub text_strong: Color32,
    /// Weak text — still >= 4:1 on every background it sits on.
    pub text_weak: Color32,
    /// Uppercase section/subgroup titles.
    pub subhead: Color32,
    /// THE accent: selection + live-emphasis only.
    pub accent: Color32,
    /// Text/glyphs on top of `selection_bg` (a lighter accent tint).
    pub accent_text: Color32,
    /// Selected chip/row fill.
    pub selection_bg: Color32,
    /// Live chips/dots ONLY.
    pub live: Color32,
    /// Warnings ONLY (RAM budget, stale data).
    pub warn: Color32,
    /// Errors/alerts ONLY.
    pub alert: Color32,
}

/// Candidate A — "slate". Sources the legacy consts above so code still on
/// the consts renders as candidate A.
pub const THEME_A: Theme = Theme {
    inset: Color32::from_rgb(11, 13, 16),
    bg: Color32::from_rgb(17, 20, 24),
    raised: Color32::from_rgb(27, 32, 39),
    hover: Color32::from_rgb(35, 43, 52),
    active: Color32::from_rgb(46, 95, 134),
    faint: Color32::from_rgb(22, 26, 31),
    hairline: Color32::from_rgb(39, 46, 55),
    outline: Color32::from_rgb(51, 60, 71),
    section_rule: SECTION_SEPARATOR_COLOR,
    text: Color32::from_rgb(214, 219, 227),
    text_strong: Color32::from_rgb(230, 235, 242),
    text_weak: Color32::from_rgb(154, 165, 180),
    subhead: SUBHEAD_COLOR,
    accent: ACCENT_COLOR,
    accent_text: Color32::from_rgb(159, 210, 238),
    selection_bg: Color32::from_rgb(31, 75, 102),
    live: LIVE_COLOR,
    warn: Color32::from_rgb(230, 176, 69),
    alert: Color32::from_rgb(240, 84, 90),
};

/// Candidate B — "graphite + amber" (A/B scaffolding; see module comment).
pub const THEME_B: Theme = Theme {
    inset: Color32::from_rgb(13, 13, 15),
    bg: Color32::from_rgb(20, 20, 23),
    raised: Color32::from_rgb(30, 30, 34),
    hover: Color32::from_rgb(41, 41, 46),
    active: Color32::from_rgb(92, 71, 24),
    faint: Color32::from_rgb(25, 25, 28),
    hairline: Color32::from_rgb(44, 44, 49),
    outline: Color32::from_rgb(58, 58, 64),
    section_rule: Color32::from_rgb(65, 65, 71),
    text: Color32::from_rgb(219, 217, 212),
    text_strong: Color32::from_rgb(234, 232, 227),
    text_weak: Color32::from_rgb(165, 162, 155),
    subhead: Color32::from_rgb(204, 201, 194),
    accent: Color32::from_rgb(227, 172, 78),
    accent_text: Color32::from_rgb(239, 201, 138),
    selection_bg: Color32::from_rgb(76, 59, 21),
    live: Color32::from_rgb(88, 214, 115),
    warn: Color32::from_rgb(239, 142, 60),
    alert: Color32::from_rgb(240, 84, 90),
};

/// `BOWECHO_THEME_B` parsing, factored pure for the headless tests: set and
/// not literally `"0"`/empty selects candidate B.
fn theme_b_requested(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// The active candidate. Read once at first use — the A/B switch is a
/// restart-scope capture aid, not a live setting.
pub fn theme() -> &'static Theme {
    static CHOICE: std::sync::OnceLock<&'static Theme> = std::sync::OnceLock::new();
    CHOICE.get_or_init(|| {
        let requested = std::env::var("BOWECHO_THEME_B").ok();
        if theme_b_requested(requested.as_deref()) {
            &THEME_B
        } else {
            &THEME_A
        }
    })
}

/// Section/subgroup title color (theme-aware successor of [`SUBHEAD_COLOR`]).
pub fn subhead_color() -> Color32 {
    theme().subhead
}

/// Section rule color (theme-aware successor of [`SECTION_SEPARATOR_COLOR`]).
pub fn section_rule_color() -> Color32 {
    theme().section_rule
}

/// Accent color (theme-aware successor of [`ACCENT_COLOR`]).
pub fn accent_color() -> Color32 {
    theme().accent
}

/// Live-indicator color (theme-aware successor of [`LIVE_COLOR`]).
pub fn live_color() -> Color32 {
    theme().live
}

/// WCAG 2.x contrast ratio between two opaque colors (1.0..=21.0). Pure and
/// test-only today — the headless nodes gate the palette floors with it;
/// promote out of `cfg(test)` when a runtime consumer appears.
#[cfg(test)]
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    fn linear(channel: u8) -> f32 {
        let channel = f32::from(channel) / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    fn luminance(color: Color32) -> f32 {
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }
    let (bright, dark) = {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb), la.min(lb))
    };
    (bright + 0.05) / (dark + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_ratio_matches_wcag_reference_points() {
        let ratio = contrast_ratio(Color32::WHITE, Color32::BLACK);
        assert!((ratio - 21.0).abs() < 0.01, "white/black: {ratio}");
        let ratio = contrast_ratio(Color32::from_gray(128), Color32::from_gray(128));
        assert!((ratio - 1.0).abs() < 1e-6, "same/same: {ratio}");
        // Symmetry.
        let forward = contrast_ratio(THEME_A.text, THEME_A.bg);
        let reverse = contrast_ratio(THEME_A.bg, THEME_A.text);
        assert_eq!(forward, reverse);
    }

    /// Spec floor: weak text >= ~4:1 on every background it can sit on;
    /// primary/subhead text comfortably higher; meaning colors legible.
    fn assert_theme_contrast_floors(theme: &Theme, name: &str) {
        let checks: &[(&str, Color32, Color32, f32)] = &[
            ("text on bg", theme.text, theme.bg, 7.0),
            ("text on raised", theme.text, theme.raised, 7.0),
            ("text on hover", theme.text, theme.hover, 4.5),
            ("text on selection", theme.text, theme.selection_bg, 4.5),
            ("weak on bg", theme.text_weak, theme.bg, 4.0),
            ("weak on raised", theme.text_weak, theme.raised, 4.0),
            ("weak on hover", theme.text_weak, theme.hover, 4.0),
            ("subhead on bg", theme.subhead, theme.bg, 7.0),
            (
                "accent_text on selection",
                theme.accent_text,
                theme.selection_bg,
                4.5,
            ),
            ("accent on bg", theme.accent, theme.bg, 4.5),
            ("live on bg", theme.live, theme.bg, 4.5),
            ("warn on bg", theme.warn, theme.bg, 4.5),
            ("alert on bg", theme.alert, theme.bg, 4.5),
        ];
        for (label, fg, bg, floor) in checks {
            let ratio = contrast_ratio(*fg, *bg);
            assert!(
                ratio >= *floor,
                "{name}: {label} = {ratio:.2}:1, floor {floor}:1"
            );
        }
        // Ramp ordering is what makes the levels read as levels.
        for (label, lower, upper) in [
            ("inset < bg", theme.inset, theme.bg),
            ("bg < raised", theme.bg, theme.raised),
            ("raised < hover", theme.raised, theme.hover),
        ] {
            assert!(
                contrast_ratio(lower, Color32::BLACK) < contrast_ratio(upper, Color32::BLACK),
                "{name}: ramp order violated at {label}"
            );
        }
    }

    #[test]
    fn candidate_a_meets_contrast_floors() {
        assert_theme_contrast_floors(&THEME_A, "A/slate");
    }

    #[test]
    fn candidate_b_meets_contrast_floors() {
        assert_theme_contrast_floors(&THEME_B, "B/graphite+amber");
    }

    #[test]
    fn theme_b_env_switch_parsing() {
        assert!(!theme_b_requested(None));
        assert!(!theme_b_requested(Some("")));
        assert!(!theme_b_requested(Some("0")));
        assert!(theme_b_requested(Some("1")));
        assert!(theme_b_requested(Some("true")));
    }
}
