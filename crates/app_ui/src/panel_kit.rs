//! panel_kit — the sidebar's shared layout primitives (docs/ui-refresh-plan.md).
//!
//! Every sidebar tab builds label+control rows, sections, chips, and status
//! lines from these primitives so the whole panel reads as one instrument:
//! dense, ruthlessly aligned, GR2Analyst/AWIPS-feel. The plan's hard rule is
//! encoded here structurally: every primitive renders correctly at 320 pt
//! panel width — labels truncate with full-text hover, chips wrap into an
//! evenly-filled grid, slider tracks never drop below their floor, and prose
//! folds into disclosures. The pure width-budget math lives in free functions
//! so the headless build nodes can test it without a GPU.

use eframe::egui;

use crate::ui_theme::{
    ROW_H, SECTION_SPACING, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH, section_rule_color, subhead_color,
};

/// Label column share of the row (plan: 44%, clamped below).
pub(crate) const LABEL_COL_FRACTION: f32 = 0.44;
/// Label column clamp — keeps labels readable at 320 pt without starving
/// the control column at 900 pt.
pub(crate) const LABEL_COL_MIN_W: f32 = 110.0;
pub(crate) const LABEL_COL_MAX_W: f32 = 170.0;
/// Fixed monospace value slot at the right edge of a slider row.
pub(crate) const VALUE_SLOT_W: f32 = 48.0;
/// Characters that comfortably fit the value slot at the kit's monospace size.
pub(crate) const VALUE_SLOT_MAX_CHARS: usize = 7;
/// A slider track never renders narrower than this (holds at the panel's
/// 300 pt minimum: 300 − label col 110 − value slot 48 − spacing ≥ 70).
pub(crate) const SLIDER_TRACK_MIN_W: f32 = 70.0;
/// Minimum selectable-chip width; chips grow from here to fill the row.
pub(crate) const CHIP_MIN_W: f32 = 52.0;
/// Popovers opened by `gear_popover` lay their kit rows out at least this
/// wide (kit row math yields a usable label column at this width).
pub(crate) const POPOVER_MIN_W: f32 = 260.0;

// Layer-rail row columns (wave 2): the rail aligns by construction — the
// state-dot slot and the right cluster (opacity slider + gear + remove) are
// fixed widths and ALWAYS allocated, so every row's slider starts at the
// same x and every gear sits at the same x; the name column flexes with the
// panel and the middle zone (count text + the row's earned inline extras)
// absorbs the rest, clipped at its budget.
/// State-dot slot width (allocated even when the row has no lifecycle).
pub(crate) const RAIL_DOT_SLOT_W: f32 = 12.0;
/// Gear and remove slot width each (allocated even when absent).
pub(crate) const RAIL_ICON_SLOT_W: f32 = 18.0;
/// Name column share of the post-checkbox row width, and its clamp band.
const RAIL_NAME_FRACTION: f32 = 0.34;
pub(crate) const RAIL_NAME_MIN_W: f32 = 96.0;
pub(crate) const RAIL_NAME_MAX_W: f32 = 150.0;

// ---------------------------------------------------------------------------
// Pure layout math (unit-tested on the headless nodes).
// ---------------------------------------------------------------------------

/// Width of the label column of a kit row: 44% of the available row width,
/// clamped to [`LABEL_COL_MIN_W`, `LABEL_COL_MAX_W`].
pub(crate) fn label_column_width(available: f32) -> f32 {
    (available * LABEL_COL_FRACTION).clamp(LABEL_COL_MIN_W, LABEL_COL_MAX_W)
}

/// How many chips fit per row: as many `CHIP_MIN_W` chips as the width
/// allows, at least one.
pub(crate) fn chip_grid_columns(available: f32, spacing: f32) -> usize {
    (((available + spacing) / (CHIP_MIN_W + spacing)).floor() as usize).max(1)
}

/// Chip width when `columns` chips split `available` evenly (floored so the
/// row's width budget is never exceeded by rounding).
pub(crate) fn chip_width(available: f32, spacing: f32, columns: usize) -> f32 {
    let columns = columns.max(1);
    ((available - spacing * (columns as f32 - 1.0)) / columns as f32)
        .floor()
        .max(1.0)
}

/// Slider track width inside a kit slider row: whatever the control column
/// leaves after the fixed value slot, floored at `SLIDER_TRACK_MIN_W`.
pub(crate) fn slider_track_width(control_available: f32, spacing: f32) -> f32 {
    (control_available - VALUE_SLOT_W - spacing).max(SLIDER_TRACK_MIN_W)
}

/// Value-slot text: pass-through when it fits, hard-truncated with an
/// ellipsis when it does not (full text goes on the hover).
pub(crate) fn value_slot_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Section titles render uppercase — callers pass "Loop"/"Products"/…
pub(crate) fn section_title(title: &str) -> String {
    title.to_uppercase()
}

/// Layer-rail name column: a fixed share of the post-checkbox row width,
/// clamped — every row uses the SAME width, so the columns after it align.
pub(crate) fn rail_name_width(available: f32) -> f32 {
    (available * RAIL_NAME_FRACTION).clamp(RAIL_NAME_MIN_W, RAIL_NAME_MAX_W)
}

/// Width of a rail row's fixed right cluster: opacity slider + gear +
/// remove slots plus their inter-slot spacing.
pub(crate) fn rail_cluster_width(slider_width: f32, spacing: f32) -> f32 {
    slider_width + 2.0 * RAIL_ICON_SLOT_W + 2.0 * spacing
}

/// Persisted sidebar width (whole logical points in settings — `AppSettings`
/// derives `Eq`, so no `f32` there) → the width the panel is built with.
pub(crate) fn sidebar_width_from_settings(stored: Option<u16>) -> Option<f32> {
    stored.map(|width| f32::from(width).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH))
}

/// Rendered panel width → the whole-point value persisted in settings.
/// Non-finite widths (never observed, but a panel mid-animation is not worth
/// trusting) persist nothing.
pub(crate) fn sidebar_width_to_settings(rendered: f32) -> Option<u16> {
    rendered
        .is_finite()
        .then(|| rendered.round().clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH) as u16)
}

// ---------------------------------------------------------------------------
// Primitives.
// ---------------------------------------------------------------------------

pub(crate) struct SectionResponse<R> {
    /// The header was clicked this frame — the caller owns the persisted
    /// open-state map and flips it.
    pub toggled: bool,
    /// What the body closure returned (None while collapsed).
    pub body: Option<R>,
}

/// Kit 1 — Collapsible section: thin rule + uppercase strong title + chevron.
/// Pure renderer — `open` comes from and `toggled` returns to the caller's
/// persisted `sidebar_section_open` map (see `ViewerApp::remembered_section`,
/// the persistence wrapper every tab goes through).
pub(crate) fn section<R>(
    ui: &mut egui::Ui,
    key: &str,
    title: &str,
    open: bool,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> SectionResponse<R> {
    ui.add_space(SECTION_SPACING);
    section_rule(ui);
    let heading = egui::RichText::new(section_title(title))
        .size(12.5)
        .strong()
        .color(subhead_color());
    let response = egui::CollapsingHeader::new(heading)
        .id_salt(key)
        .open(Some(open))
        .show(ui, body);
    SectionResponse {
        toggled: response.header_response.clicked(),
        body: response.body_returned,
    }
}

/// Kit 2 — Label+control row: label in a clamped left column (truncating, hover
/// carries the full text), control right-aligned in the rest. The control
/// closure runs in a right-to-left layout — add widgets in visual
/// right-to-left order; a single control simply renders right-aligned.
pub(crate) fn row<R>(
    ui: &mut egui::Ui,
    label: &str,
    control: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        let label_width = label_column_width(ui.available_width());
        label_cell(ui, label_width, label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control)
            .inner
    })
    .inner
}

/// Kit 3 — Slider row: label column + track filling the middle + value
/// right-aligned in a fixed monospace slot. The slider itself renders no
/// value (`show_value(false)`); the kit draws it so every slider row's
/// numbers land in the same column. `step` 0.0 = egui's default stepping
/// (integer types still snap). Width budget: label column (clamped) +
/// track (≥ `SLIDER_TRACK_MIN_W`) + `VALUE_SLOT_W`; holds at the 300 pt
/// panel minimum by construction.
pub(crate) fn slider_row<Num: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Num,
    range: std::ops::RangeInclusive<Num>,
    step: f64,
    display: impl Fn(Num) -> String,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        let label_width = label_column_width(ui.available_width());
        label_cell(ui, label_width, label);
        let spacing = ui.spacing().item_spacing.x;
        let track_width = slider_track_width(ui.available_width(), spacing);
        ui.spacing_mut().slider_width = track_width;
        let response = ui.add(
            egui::Slider::new(value, range)
                .step_by(step)
                .show_value(false),
        );
        let full = display(*value);
        let shown = value_slot_text(&full, VALUE_SLOT_MAX_CHARS);
        ui.allocate_ui_with_layout(
            egui::vec2(VALUE_SLOT_W, ROW_H),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.set_min_width(VALUE_SLOT_W);
                ui.add(egui::Label::new(
                    egui::RichText::new(shown).monospace().size(11.0),
                ))
                .on_hover_text(full);
            },
        );
        response
    })
    .inner
}

/// One chip of a [`chip_grid`].
pub(crate) struct Chip<'a> {
    pub label: &'a str,
    /// Optional weak hotkey prefix (e.g. the product hotkey digit).
    pub hotkey: Option<&'a str>,
    pub selected: bool,
    pub hover: Option<String>,
}

/// Kit 4 — Wrapping grid of fixed-width selectable chips: chips start at
/// `CHIP_MIN_W` and grow to split each row evenly; text truncates. Returns
/// the index of the clicked chip, if any.
pub(crate) fn chip_grid(ui: &mut egui::Ui, chips: &[Chip<'_>]) -> Option<usize> {
    if chips.is_empty() {
        return None;
    }
    let spacing = ui.spacing().item_spacing.x;
    let available = ui.available_width();
    let columns = chip_grid_columns(available, spacing);
    let width = chip_width(available, spacing, columns);
    let label_font = egui::TextStyle::Button.resolve(ui.style());
    let hotkey_font = egui::FontId::proportional((label_font.size - 2.0).max(9.0));
    let label_color = ui.visuals().text_color();
    let hotkey_color = ui.visuals().weak_text_color();
    let mut clicked = None;
    for (chunk_index, chunk) in chips.chunks(columns).enumerate() {
        ui.horizontal(|ui| {
            for (offset, chip) in chunk.iter().enumerate() {
                let mut text = egui::text::LayoutJob::default();
                if let Some(hotkey) = chip.hotkey {
                    text.append(
                        hotkey,
                        0.0,
                        egui::TextFormat::simple(hotkey_font.clone(), hotkey_color),
                    );
                }
                text.append(
                    chip.label,
                    if chip.hotkey.is_some() { 3.0 } else { 0.0 },
                    egui::TextFormat::simple(label_font.clone(), label_color),
                );
                let mut response = ui.add_sized(
                    egui::vec2(width, ROW_H),
                    egui::Button::selectable(chip.selected, egui::WidgetText::from(text))
                        .wrap_mode(egui::TextWrapMode::Truncate),
                );
                if let Some(hover) = &chip.hover {
                    response = response.on_hover_text(hover);
                }
                if response.clicked() {
                    clicked = Some(chunk_index * columns + offset);
                }
            }
        });
    }
    clicked
}

/// Kit 5 — Up-to-two-line small monospace weak status text; each line truncates
/// with a full-text hover. For radar/chunk/frame status lines.
pub(crate) fn status_block(ui: &mut egui::Ui, primary: &str, secondary: Option<&str>) {
    for line in std::iter::once(primary).chain(secondary) {
        if line.is_empty() {
            continue;
        }
        ui.add(
            egui::Label::new(egui::RichText::new(line).monospace().size(11.0).weak()).truncate(),
        )
        .on_hover_text(line);
    }
}

/// Kit 6 — Collapsed-by-default disclosure for explainer prose ("About {topic}").
/// Prose never renders inline-expanded by default.
pub(crate) fn about(ui: &mut egui::Ui, key: &str, topic: &str, paragraphs: &[&str]) {
    let heading = egui::RichText::new(format!("About {topic}"))
        .size(11.0)
        .weak();
    egui::CollapsingHeader::new(heading)
        .id_salt(key)
        .default_open(false)
        .show(ui, |ui| {
            for paragraph in paragraphs {
                ui.add(egui::Label::new(egui::RichText::new(*paragraph).weak()).wrap());
            }
        });
}

/// Kit 7 — Small gear button opening a popover for advanced/rare controls.
/// `title` is the popover heading and the gear's hover text (glyph-only
/// buttons must explain themselves).
pub(crate) fn gear_popover<R>(
    ui: &mut egui::Ui,
    title: &str,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let mut result = None;
    ui.menu_button("⚙", |ui| {
        ui.set_min_width(POPOVER_MIN_W);
        ui.label(
            egui::RichText::new(section_title(title))
                .size(11.0)
                .strong()
                .color(subhead_color()),
        );
        ui.separator();
        result = Some(body(ui));
    })
    .response
    .on_hover_text(title.to_owned());
    result
}

/// Kit 8 — Full-width selectable list row: LEFT-aligned truncating monospace
/// content (tilt rows, and wave 2's tabular alert rows) — never centered.
/// Mirrors `SelectableLabel`'s paint so hover/selection visuals match the
/// rest of the app.
pub(crate) fn select_row(
    ui: &mut egui::Ui,
    selected: bool,
    enabled: bool,
    text: &str,
    hover: Option<&str>,
) -> egui::Response {
    let width = ui.available_width().max(1.0);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, ROW_H), sense);
    if ui.is_rect_visible(rect) {
        let visuals = if enabled {
            ui.style().interact_selectable(&response, selected)
        } else {
            ui.style().visuals.widgets.noninteractive
        };
        if selected || (enabled && (response.hovered() || response.has_focus())) {
            ui.painter().rect(
                rect,
                visuals.corner_radius,
                visuals.weak_bg_fill,
                visuals.bg_stroke,
                egui::StrokeKind::Inside,
            );
        }
        let color = if enabled {
            visuals.text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        let galley =
            ui.painter()
                .layout_no_wrap(text.to_owned(), egui::FontId::monospace(12.0), color);
        let text_pos = egui::pos2(rect.left() + 6.0, rect.center().y - galley.size().y * 0.5);
        ui.painter()
            .with_clip_rect(rect)
            .galley(text_pos, galley, color);
    }
    match hover {
        Some(hover) => response.on_hover_text(hover.to_owned()),
        None => response,
    }
}

/// Kit 9 — Subgroup header (wave 2): the lighter, NON-collapsible cousin of
/// [`section`] for groups INSIDE a section (the layer rail's BASE /
/// ATMOSPHERE / OBS / SEVERE / COMMUNITY). Small uppercase strong title in
/// the section palette plus an optional right-aligned action slot; no rule,
/// no persistence — subgroups are a reading aid, not a fold.
pub(crate) fn subgroup<R>(
    ui: &mut egui::Ui,
    title: &str,
    right: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(section_title(title))
                .size(11.0)
                .strong()
                .color(subhead_color()),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right)
            .inner
    })
    .inner
}

fn label_cell(ui: &mut egui::Ui, width: f32, label: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ROW_H),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            ui.add(egui::Label::new(label).truncate())
                .on_hover_text(label);
        },
    );
}

fn section_rule(ui: &mut egui::Ui) {
    let width = ui.available_width().max(1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 2.0), egui::Sense::hover());
    // THEME PASS: hairline, not a band — the uppercase title carries the
    // hierarchy; the rule only marks the boundary.
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        egui::Stroke::new(1.0, section_rule_color()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_column_clamps_at_narrow_and_wide_panels() {
        // 320 pt rule: the label column stays inside its clamp band at every
        // width the panel can take (min 300 → max 900).
        assert_eq!(label_column_width(200.0), LABEL_COL_MIN_W); // 88 → floor
        assert!((label_column_width(320.0) - 140.8).abs() < 0.01);
        assert!((label_column_width(380.0) - 167.2).abs() < 0.01);
        assert_eq!(label_column_width(650.0), LABEL_COL_MAX_W); // 286 → ceiling
        assert_eq!(label_column_width(900.0), LABEL_COL_MAX_W);
        // Control column keeps the majority at the narrow end.
        let label = label_column_width(300.0);
        assert!(300.0 - label >= label);
    }

    #[test]
    fn chip_grid_splits_the_row_evenly_and_never_overflows() {
        for available in [296.0_f32, 320.0, 356.0, 380.0, 626.0, 650.0, 900.0] {
            let spacing = 4.0;
            let columns = chip_grid_columns(available, spacing);
            let width = chip_width(available, spacing, columns);
            assert!(columns >= 1);
            assert!(
                width >= CHIP_MIN_W,
                "chips shrank below the minimum at {available}pt: {width}"
            );
            let row_width = width * columns as f32 + spacing * (columns as f32 - 1.0);
            assert!(
                row_width <= available,
                "row overflows its budget at {available}pt: {row_width}"
            );
        }
        // Degenerate width still yields a single (clamped) column.
        assert_eq!(chip_grid_columns(10.0, 4.0), 1);
        assert!(chip_width(10.0, 4.0, 1) >= 1.0);
    }

    #[test]
    fn value_slot_text_truncates_with_ellipsis() {
        assert_eq!(value_slot_text("100%", VALUE_SLOT_MAX_CHARS), "100%");
        assert_eq!(value_slot_text("1234567", VALUE_SLOT_MAX_CHARS), "1234567");
        assert_eq!(value_slot_text("12345678", VALUE_SLOT_MAX_CHARS), "123456…");
        assert_eq!(value_slot_text("", VALUE_SLOT_MAX_CHARS), "");
    }

    #[test]
    fn slider_track_never_drops_below_its_floor() {
        // Control column at the 300 pt panel minimum (≈ 300 − label 110 −
        // margins): still at or above the floor.
        assert!(slider_track_width(170.0, 8.0) >= SLIDER_TRACK_MIN_W);
        // Pathologically narrow: floored, never negative or tiny.
        assert_eq!(slider_track_width(0.0, 8.0), SLIDER_TRACK_MIN_W);
        // Wide panels: the track takes everything the value slot leaves.
        assert!((slider_track_width(500.0, 8.0) - (500.0 - VALUE_SLOT_W - 8.0)).abs() < 0.01);
    }

    #[test]
    fn section_titles_render_uppercase() {
        assert_eq!(section_title("Loop"), "LOOP");
        assert_eq!(section_title("PRODUCTS"), "PRODUCTS");
        assert_eq!(section_title("Grid / Composites"), "GRID / COMPOSITES");
    }

    #[test]
    fn rail_name_column_clamps_and_stays_row_independent() {
        // One width per panel width — rows align because every row asks the
        // same question. Clamp band holds across the panel's 300..900 range.
        assert_eq!(rail_name_width(200.0), RAIL_NAME_MIN_W);
        assert_eq!(rail_name_width(282.0), RAIL_NAME_MIN_W); // 95.9 → floor
        assert!((rail_name_width(360.0) - 122.4).abs() < 0.01);
        assert_eq!(rail_name_width(600.0), RAIL_NAME_MAX_W);
        assert_eq!(rail_name_width(900.0), RAIL_NAME_MAX_W);
    }

    #[test]
    fn rail_middle_zone_holds_the_extras_budget_at_the_panel_minimum() {
        // Mirror of layer_row's live arithmetic: middle = post-checkbox
        // width − name column − dot slot − right cluster − 3 gaps. At the
        // 300 pt panel minimum inside a section indent the post-checkbox
        // width is ≈ 250 pt, and the middle zone must still hold a rail
        // row's largest inline-extras set (model rows' ↑/↓ ≈ 35 pt,
        // placefiles' T + ↻ ≈ 39 pt at 320) — everything wider lives
        // behind ⚙.
        let slider = 56.0;
        let spacing = 3.0;
        let middle_at = |available: f32| {
            (available
                - rail_name_width(available)
                - RAIL_DOT_SLOT_W
                - rail_cluster_width(slider, spacing)
                - 3.0 * spacing)
                .max(0.0)
        };
        for available in [250.0_f32, 260.0, 282.0, 340.0, 620.0] {
            let middle = middle_at(available);
            assert!(
                middle >= 35.0,
                "middle zone collapsed at {available}pt: {middle}"
            );
            // The fixed columns + middle never exceed the row's budget.
            let total = rail_name_width(available)
                + RAIL_DOT_SLOT_W
                + rail_cluster_width(slider, spacing)
                + 3.0 * spacing
                + middle;
            assert!(total <= available + 0.01, "row overflows at {available}pt");
        }
        // Degenerate widths clamp to zero instead of going negative.
        assert_eq!(middle_at(100.0), 0.0);
    }

    #[test]
    fn sidebar_width_settings_roundtrip_clamps_to_panel_range() {
        assert_eq!(sidebar_width_from_settings(None), None);
        assert_eq!(sidebar_width_from_settings(Some(380)), Some(380.0));
        assert_eq!(
            sidebar_width_from_settings(Some(100)),
            Some(SIDEBAR_MIN_WIDTH)
        );
        assert_eq!(
            sidebar_width_from_settings(Some(5000)),
            Some(SIDEBAR_MAX_WIDTH)
        );
        assert_eq!(sidebar_width_to_settings(380.4), Some(380));
        assert_eq!(
            sidebar_width_to_settings(12.0),
            Some(SIDEBAR_MIN_WIDTH as u16)
        );
        assert_eq!(
            sidebar_width_to_settings(5000.0),
            Some(SIDEBAR_MAX_WIDTH as u16)
        );
        assert_eq!(sidebar_width_to_settings(f32::NAN), None);
    }
}
