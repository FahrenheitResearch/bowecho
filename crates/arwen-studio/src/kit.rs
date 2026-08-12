// SPDX-License-Identifier: Apache-2.0
//
// Pattern copied with attribution from BowEcho crates/app_ui/src/
// panel_kit.rs @ 6dfcb9f (the density/typography contract: clamped label
// columns, even chip grids, monospace value slots, uppercase section
// titles). Trimmed to the primitives Studio uses.

//! Inspector layout primitives — every card builds rows/sections/chips
//! from these so the panel reads as one instrument.

use eframe::egui;

use crate::theme::{ROW_H, SECTION_SPACING, theme};

const LABEL_COL_FRACTION: f32 = 0.40;
const LABEL_COL_MIN_W: f32 = 100.0;
const LABEL_COL_MAX_W: f32 = 160.0;
const CHIP_MIN_W: f32 = 52.0;

/// Label column width: a clamped fraction of the row.
pub fn label_column_width(available: f32) -> f32 {
    (available * LABEL_COL_FRACTION).clamp(LABEL_COL_MIN_W, LABEL_COL_MAX_W)
}

pub fn chip_grid_columns(available: f32, spacing: f32) -> usize {
    (((available + spacing) / (CHIP_MIN_W + spacing)).floor() as usize).max(1)
}

pub fn chip_width(available: f32, spacing: f32, columns: usize) -> f32 {
    let columns = columns.max(1);
    ((available - spacing * (columns as f32 - 1.0)) / columns as f32)
        .floor()
        .max(1.0)
}

/// Uppercase section header over a hairline rule.
pub fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(SECTION_SPACING);
    let width = ui.available_width().max(1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 2.0), egui::Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        egui::Stroke::new(1.0_f32, theme().section_rule),
    );
    ui.label(
        egui::RichText::new(title.to_uppercase())
            .size(12.0)
            .strong()
            .color(theme().subhead),
    );
    ui.add_space(2.0);
}

/// Label + right-aligned control row. The control closure runs
/// right-to-left; add widgets in visual right-to-left order.
pub fn row<R>(ui: &mut egui::Ui, label: &str, control: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        let width = label_column_width(ui.available_width());
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
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control)
            .inner
    })
    .inner
}

pub struct Chip<'a> {
    pub label: &'a str,
    pub selected: bool,
    pub hover: Option<String>,
    /// Small colored dot before the label (maturity, live state); `None`
    /// for plain chips.
    pub dot: Option<egui::Color32>,
}

/// Evenly-filled wrapping grid of selectable chips. Returns the clicked
/// index.
pub fn chip_grid(ui: &mut egui::Ui, chips: &[Chip<'_>]) -> Option<usize> {
    if chips.is_empty() {
        return None;
    }
    let spacing = ui.spacing().item_spacing.x;
    let available = ui.available_width();
    let columns = chip_grid_columns(available, spacing);
    let width = chip_width(available, spacing, columns);
    let mut clicked = None;
    for (chunk_index, chunk) in chips.chunks(columns).enumerate() {
        ui.horizontal(|ui| {
            for (offset, chip) in chunk.iter().enumerate() {
                let text = egui::RichText::new(chip.label);
                let mut response = ui.add_sized(
                    egui::vec2(width, ROW_H),
                    egui::Button::selectable(chip.selected, text)
                        .wrap_mode(egui::TextWrapMode::Truncate),
                );
                if let Some(dot) = chip.dot {
                    let center = egui::pos2(response.rect.left() + 7.0, response.rect.center().y);
                    ui.painter().circle_filled(center, 2.5, dot);
                }
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

/// Small monospace weak status line, truncating with full-text hover.
pub fn status_line(ui: &mut egui::Ui, text: &str) {
    if text.is_empty() {
        return;
    }
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .monospace()
                .size(crate::theme::READOUT_FONT_SIZE)
                .weak(),
        )
        .truncate(),
    )
    .on_hover_text(text);
}

/// Monospace value text (tabular numerals for time/dims/memory).
pub fn value_text(text: &str) -> egui::RichText {
    egui::RichText::new(text)
        .monospace()
        .size(crate::theme::READOUT_FONT_SIZE)
}

// ---------------------------------------------------------------------------
// Number formatting: exact values, human units. Presets speak user
// language with exact values visible underneath — these helpers are the
// "exact values" half.
// ---------------------------------------------------------------------------

pub fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.0} MiB", bytes / MIB)
    } else {
        format!("{bytes:.0} B")
    }
}

pub fn format_duration_s(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "—".into();
    }
    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// "+2:00" style forecast-hour offset label.
#[allow(dead_code)] // used by the valid-time hover once frames carry leads
pub fn format_lead_hours(seconds: f64) -> String {
    let minutes = (seconds / 60.0).round() as i64;
    format!("+{}:{:02}", minutes / 60, (minutes % 60).abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_column_clamps_across_panel_widths() {
        assert_eq!(label_column_width(200.0), LABEL_COL_MIN_W);
        assert!((label_column_width(340.0) - 136.0).abs() < 0.01);
        assert_eq!(label_column_width(900.0), LABEL_COL_MAX_W);
    }

    #[test]
    fn chip_grid_never_overflows_its_row() {
        for available in [300.0_f32, 320.0, 360.0, 460.0] {
            let spacing = 4.0;
            let columns = chip_grid_columns(available, spacing);
            let width = chip_width(available, spacing, columns);
            assert!(width >= CHIP_MIN_W);
            let row = width * columns as f32 + spacing * (columns as f32 - 1.0);
            assert!(row <= available, "{available}: {row}");
        }
    }

    #[test]
    fn byte_and_duration_formats() {
        assert_eq!(format_bytes(18_253_611_008), "17.0 GiB");
        assert_eq!(format_bytes(1_288_490_188), "1.2 GiB");
        assert_eq!(format_bytes(52_428_800), "50 MiB");
        assert_eq!(format_duration_s(1_680.0), "28:00");
        assert_eq!(format_duration_s(4_212.0), "1:10:12");
        assert_eq!(format_lead_hours(21_600.0), "+6:00");
        assert_eq!(format_lead_hours(2_700.0), "+0:45");
    }
}
