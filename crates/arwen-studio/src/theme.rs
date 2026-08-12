// SPDX-License-Identifier: Apache-2.0
//
// Pattern (ramp discipline, contrast floors, the "data owns the saturated
// colors" rule and its tests) copied with attribution from BowEcho
// crates/app_ui/src/ui_theme.rs @ 6dfcb9f. The palette itself is Studio's
// own: neutral slightly-warm charcoal, ONE cool accent, green/amber/red
// reserved for real states.

//! The chrome palette + density constants. Every color the chrome paints
//! comes from here; the map data owns all other saturation.

use eframe::egui::{self, Color32};

/// Every button / row / control height.
pub const ROW_H: f32 = 24.0;
/// Vertical air above a section header.
pub const SECTION_SPACING: f32 = 8.0;
/// Right inspector width band (design: 320-380).
pub const INSPECTOR_DEFAULT_WIDTH: f32 = 360.0;
pub const INSPECTOR_MIN_WIDTH: f32 = 320.0;
pub const INSPECTOR_MAX_WIDTH: f32 = 460.0;
/// Left rail width (wide enough that "Runs" never wraps).
pub const RAIL_W: f32 = 64.0;
/// Monospace readout size (hover readout, resource strip numbers).
pub const READOUT_FONT_SIZE: f32 = 11.0;

pub struct Theme {
    pub inset: Color32,
    pub bg: Color32,
    pub raised: Color32,
    pub hover: Color32,
    pub active: Color32,
    pub faint: Color32,
    pub hairline: Color32,
    pub outline: Color32,
    pub section_rule: Color32,
    pub text: Color32,
    pub text_strong: Color32,
    pub text_weak: Color32,
    pub subhead: Color32,
    /// THE accent: selection + focus only.
    pub accent: Color32,
    pub accent_text: Color32,
    pub selection_bg: Color32,
    /// Real states ONLY — never decoration.
    pub live: Color32,
    pub warn: Color32,
    pub alert: Color32,
    /// Map canvas background (slightly deeper than chrome so the world
    /// reads as the subject).
    pub map_bg: Color32,
}

/// "Warm charcoal" — the one Studio theme.
pub const THEME: Theme = Theme {
    inset: Color32::from_rgb(15, 14, 13),
    bg: Color32::from_rgb(23, 21, 19),
    raised: Color32::from_rgb(34, 31, 29),
    hover: Color32::from_rgb(45, 42, 38),
    active: Color32::from_rgb(42, 74, 99),
    faint: Color32::from_rgb(28, 26, 24),
    hairline: Color32::from_rgb(46, 43, 39),
    outline: Color32::from_rgb(60, 56, 51),
    section_rule: Color32::from_rgb(58, 55, 50),
    text: Color32::from_rgb(218, 214, 208),
    text_strong: Color32::from_rgb(236, 233, 228),
    text_weak: Color32::from_rgb(162, 156, 148),
    subhead: Color32::from_rgb(201, 196, 188),
    accent: Color32::from_rgb(106, 174, 214),
    accent_text: Color32::from_rgb(163, 205, 232),
    selection_bg: Color32::from_rgb(39, 74, 99),
    live: Color32::from_rgb(88, 214, 115),
    warn: Color32::from_rgb(224, 166, 63),
    alert: Color32::from_rgb(239, 84, 88),
    map_bg: Color32::from_rgb(18, 17, 16),
};

pub fn theme() -> &'static Theme {
    &THEME
}

/// Maturity chip colors: scientific confidence, distinct from operational
/// health (which uses live/warn/alert).
pub fn maturity_color(maturity: &str) -> Color32 {
    match maturity {
        "certified" => THEME.accent,
        "supported" => THEME.subhead,
        _ => THEME.warn,
    }
}

/// Install the theme into egui's style document. Called once at startup.
pub fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.global_style()).clone();
    let theme = theme();

    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    style.spacing.interact_size = egui::vec2(ROW_H, ROW_H);
    style.visuals.override_text_color = Some(theme.text);
    style.visuals.panel_fill = theme.bg;
    style.visuals.window_fill = theme.bg;
    style.visuals.window_stroke = egui::Stroke::new(1.0, theme.outline);
    style.visuals.extreme_bg_color = theme.inset;
    style.visuals.faint_bg_color = theme.faint;
    style.visuals.selection.bg_fill = theme.selection_bg;
    style.visuals.selection.stroke = egui::Stroke::new(1.0, theme.accent);
    style.visuals.hyperlink_color = theme.accent;
    // Modest radii — instrument, not toy.
    let radius = egui::CornerRadius::same(3);
    style.visuals.menu_corner_radius = radius;
    style.visuals.window_corner_radius = egui::CornerRadius::same(4);

    let widgets = &mut style.visuals.widgets;
    widgets.noninteractive.bg_fill = theme.bg;
    widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, theme.hairline);
    widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, theme.text);
    widgets.noninteractive.corner_radius = radius;
    widgets.inactive.bg_fill = theme.raised;
    widgets.inactive.weak_bg_fill = theme.raised;
    widgets.inactive.bg_stroke = egui::Stroke::NONE;
    widgets.inactive.fg_stroke = egui::Stroke::new(1.0, theme.text);
    widgets.inactive.corner_radius = radius;
    widgets.hovered.bg_fill = theme.hover;
    widgets.hovered.weak_bg_fill = theme.hover;
    widgets.hovered.bg_stroke = egui::Stroke::new(1.0, theme.outline);
    widgets.hovered.fg_stroke = egui::Stroke::new(1.0, theme.text_strong);
    widgets.hovered.corner_radius = radius;
    widgets.active.bg_fill = theme.active;
    widgets.active.weak_bg_fill = theme.active;
    widgets.active.bg_stroke = egui::Stroke::new(1.0, theme.accent);
    widgets.active.fg_stroke = egui::Stroke::new(1.0, theme.text_strong);
    widgets.active.corner_radius = radius;
    widgets.open.bg_fill = theme.raised;
    widgets.open.weak_bg_fill = theme.raised;
    widgets.open.bg_stroke = egui::Stroke::new(1.0, theme.outline);
    widgets.open.fg_stroke = egui::Stroke::new(1.0, theme.text_strong);
    widgets.open.corner_radius = radius;

    ctx.set_global_style(style);
}

/// WCAG 2.x contrast ratio (test-gated palette floors, BowEcho pattern).
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

    /// Contrast floors (BowEcho's spec): weak text >= 4:1 on every
    /// background it sits on; primary text comfortably higher; the accent
    /// and the three state colors legible on bg.
    #[test]
    fn palette_meets_contrast_floors() {
        let theme = theme();
        let checks: &[(&str, Color32, Color32, f32)] = &[
            ("text on bg", theme.text, theme.bg, 7.0),
            ("text on raised", theme.text, theme.raised, 7.0),
            ("text on hover", theme.text, theme.hover, 4.5),
            ("text on selection", theme.text, theme.selection_bg, 4.5),
            ("weak on bg", theme.text_weak, theme.bg, 4.0),
            ("weak on raised", theme.text_weak, theme.raised, 4.0),
            ("weak on hover", theme.text_weak, theme.hover, 4.0),
            ("subhead on bg", theme.subhead, theme.bg, 7.0),
            ("accent on bg", theme.accent, theme.bg, 4.5),
            (
                "accent_text on selection",
                theme.accent_text,
                theme.selection_bg,
                4.5,
            ),
            ("live on bg", theme.live, theme.bg, 4.5),
            ("warn on bg", theme.warn, theme.bg, 4.5),
            ("alert on bg", theme.alert, theme.bg, 4.5),
        ];
        for (label, fg, bg, floor) in checks {
            let ratio = contrast_ratio(*fg, *bg);
            assert!(ratio >= *floor, "{label}: {ratio:.2}:1, floor {floor}:1");
        }
        // Ramp ordering makes the levels read as levels.
        for (label, lower, upper) in [
            ("inset < bg", theme.inset, theme.bg),
            ("bg < raised", theme.bg, theme.raised),
            ("raised < hover", theme.raised, theme.hover),
        ] {
            assert!(
                contrast_ratio(lower, Color32::BLACK) < contrast_ratio(upper, Color32::BLACK),
                "ramp order violated at {label}"
            );
        }
    }
}
