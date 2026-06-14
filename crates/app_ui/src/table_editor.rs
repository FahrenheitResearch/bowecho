//! Color table editor window (docs/customization-spec.md §2.2) and the
//! shared picker widgets: checkerboard-backed swatch strips and badged
//! catalog rows (§2.1). New UI surfaces live in modules, not main.rs.
//!
//! The editor authors a [`TableDraft`] in DECLARED units (a velocity
//! table authored in kt behaves exactly like a community .pal: the
//! draft pre-scales stops the same way the parser scales `Units: kt`
//! files). Live preview pushes the built table through the existing
//! palette-switch path (`set_family` + `clear_texture`), and Save
//! writes a GR2Analyst-compatible `.pal` via `color_tables::to_gr_pal`
//! into the "My tables" folder.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use color_tables::{
    Badge, ColorStop, ColorTable, ColorTableFamily, Rgba8, SampleMode, builtin_tables_for_family,
    product_code_for_family, to_gr_pal, unit_scale_to_internal,
};
use eframe::egui;

use crate::ViewerApp;

// ------------------------------------------------------------------------
// Shared picker widgets (§2.1)
// ------------------------------------------------------------------------

/// Checkerboard backing so alpha reads in swatches and the preview bar.
fn paint_checkerboard(painter: &egui::Painter, rect: egui::Rect, cell: f32) {
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(70));
    let light = egui::Color32::from_gray(100);
    let rows = (rect.height() / cell).ceil() as usize;
    let columns = (rect.width() / cell).ceil() as usize;
    for row in 0..rows {
        for column in 0..columns {
            if (row + column) % 2 == 0 {
                continue;
            }
            let min = egui::pos2(
                rect.left() + column as f32 * cell,
                rect.top() + row as f32 * cell,
            );
            let max = egui::pos2(
                (min.x + cell).min(rect.right()),
                (min.y + cell).min(rect.bottom()),
            );
            painter.rect_filled(egui::Rect::from_min_max(min, max), 0.0, light);
        }
    }
}

/// Paint a horizontal strip sampling `table` across its stop range onto a
/// checkerboard (alpha shows). Pure painting — callers own interaction.
pub(crate) fn paint_swatch_strip(painter: &egui::Painter, rect: egui::Rect, table: &ColorTable) {
    paint_checkerboard(painter, rect, rect.height() / 2.0);
    let Some((first, last)) = table.stops().first().zip(table.stops().last()) else {
        return;
    };
    let span = (last.value - first.value).max(f32::EPSILON);
    let columns = (rect.width() / 2.0).ceil().max(1.0) as usize;
    let sampler = table.sampler();
    let column_width = rect.width() / columns as f32;
    for column in 0..columns {
        let t = (column as f32 + 0.5) / columns as f32;
        let [r, g, b, a] = sampler.color_for_value(first.value + t * span);
        if a == 0 {
            continue;
        }
        let left = rect.left() + column as f32 * column_width;
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(left, rect.top()),
                egui::pos2((left + column_width + 0.5).min(rect.right()), rect.bottom()),
            ),
            0.0,
            egui::Color32::from_rgba_unmultiplied(r, g, b, a),
        );
    }
}

fn badge_color(badge: Badge) -> egui::Color32 {
    match badge {
        Badge::Default => egui::Color32::from_rgb(96, 160, 220),
        Badge::CvdSafe => egui::Color32::from_rgb(82, 190, 170),
        Badge::Classic => egui::Color32::from_rgb(158, 158, 158),
        Badge::Smooth => egui::Color32::from_rgb(176, 136, 224),
        Badge::HighContrast => egui::Color32::from_rgb(224, 166, 78),
        Badge::Research => egui::Color32::from_rgb(118, 196, 98),
    }
}

/// Paint one badge chip; returns the x just past its right edge.
fn paint_badge_chip(painter: &egui::Painter, left_center: egui::Pos2, badge: Badge) -> f32 {
    let color = badge_color(badge);
    let text_rect = painter.text(
        left_center + egui::vec2(5.0, 0.0),
        egui::Align2::LEFT_CENTER,
        badge.label(),
        egui::FontId::proportional(9.5),
        color,
    );
    painter.rect_stroke(
        text_rect.expand2(egui::vec2(4.0, 2.0)),
        4.0,
        egui::Stroke::new(1.0, color.gamma_multiply(0.55)),
        egui::StrokeKind::Outside,
    );
    text_rect.right() + 6.0
}

/// One picker row: swatch strip, name, and badge chips; hovering shows
/// the description plus the table summary. Returns true when clicked
/// (the caller applies the table). `width` lets callers reserve room for
/// trailing per-row buttons.
pub(crate) fn catalog_row(
    ui: &mut egui::Ui,
    table: &ColorTable,
    badges: &[Badge],
    description: &str,
    selected: bool,
    width: f32,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&response, selected);
        if selected || response.hovered() {
            ui.painter().rect_filled(rect, 4.0, visuals.weak_bg_fill);
        }
        let swatch = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 4.0, rect.center().y - 7.0),
            egui::vec2(64.0, 14.0),
        );
        paint_swatch_strip(ui.painter(), swatch, table);
        let name_rect = ui.painter().text(
            egui::pos2(swatch.right() + 8.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            table.name(),
            egui::FontId::proportional(13.0),
            visuals.text_color(),
        );
        let mut chip_x = name_rect.right() + 8.0;
        for badge in badges {
            chip_x = paint_badge_chip(ui.painter(), egui::pos2(chip_x, rect.center().y), *badge);
        }
    }
    let summary = crate::color_table_summary(table);
    let hover = if description.is_empty() {
        summary
    } else {
        format!("{description}\n{summary}")
    };
    response.on_hover_text(hover).clicked()
}

/// Reveal a path in the OS file browser (the "My tables" folder).
pub(crate) fn show_in_file_browser(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(path)
            .spawn()
            .map_err(|err| format!("Open folder failed for {}: {err}", path.display()))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|err| format!("Open folder failed for {}: {err}", path.display()))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|err| format!("Open folder failed for {}: {err}", path.display()))?;
    }
    Ok(())
}

// ------------------------------------------------------------------------
// The draft (§2.2)
// ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DraftMode {
    /// Plain rows ramp between stops (GR `Color:` semantics).
    Smooth,
    /// Every stop holds a hard band (GR `SolidColor:` semantics).
    Stepped,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DraftStop {
    /// In DECLARED units (kt stays kt; build pre-scales to internal).
    value: f32,
    color: [u8; 4],
    /// Two-color gradient across this band (GR 6/8-component row).
    end_color: Option<[u8; 4]>,
    /// Hard band: hold `color` to the next stop (GR `SolidColor:`).
    solid: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TableDraft {
    name: String,
    family: ColorTableFamily,
    /// Declared units string ("" = no Units header).
    units: String,
    rf: [u8; 4],
    mode: DraftMode,
    stops: Vec<DraftStop>,
}

/// Family-appropriate `Units:` suggestions; the first is the seed
/// default. Only kt/mph scale values — the rest are legend labels.
fn units_for_family(family: ColorTableFamily) -> &'static [&'static str] {
    match family {
        ColorTableFamily::Reflectivity => &["dBZ"],
        ColorTableFamily::Velocity => &["kt", "m/s", "mph"],
        ColorTableFamily::SpectrumWidth => &["m/s", "kt"],
        ColorTableFamily::CorrelationCoefficient => &[""],
        ColorTableFamily::DifferentialReflectivity => &["dB"],
        ColorTableFamily::EchoTops => &["m", "kft"],
        ColorTableFamily::Vil => &["kg/m^2"],
        ColorTableFamily::VilDensity => &["g/m^3"],
        ColorTableFamily::HailSize => &["mm"],
        ColorTableFamily::AzimuthalShear => &["1e-3/s"],
        ColorTableFamily::DifferentialPhase => &["deg"],
        ColorTableFamily::SpecificDifferentialPhase => &["deg/km"],
        ColorTableFamily::Generic => &["", "dBZ", "kt", "m/s"],
    }
}

fn default_units_for_family(family: ColorTableFamily) -> String {
    units_for_family(family)
        .first()
        .copied()
        .unwrap_or("")
        .to_owned()
}

/// Strip filename-hostile characters; the draft name doubles as the
/// `.pal` file stem in "My tables" (the scanner names tables by stem).
pub(crate) fn sanitize_file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if matches!(
                character,
                '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            ) {
                ' '
            } else {
                character
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn rgba8(color: [u8; 4]) -> Rgba8 {
    Rgba8::new(color[0], color[1], color[2], color[3])
}

impl TableDraft {
    /// Seed a NEW draft: the family's default table downsampled to eight
    /// stops — a sensible starting ramp instead of a blank canvas.
    fn seeded_from_family(family: ColorTableFamily) -> Self {
        let units = default_units_for_family(family);
        let scale = unit_scale_to_internal(&units);
        let source = builtin_tables_for_family(family)
            .into_iter()
            .next()
            .expect("every family has at least one built-in table");
        let mut stops = Vec::new();
        if let Some((first, last)) = source.stops().first().zip(source.stops().last()) {
            let sampler = source.sampler();
            const COUNT: usize = 8;
            for index in 0..COUNT {
                let t = index as f32 / (COUNT - 1) as f32;
                let value = first.value + t * (last.value - first.value);
                stops.push(DraftStop {
                    value: value / scale,
                    color: sampler.color_for_value(value),
                    end_color: None,
                    solid: false,
                });
            }
        }
        Self {
            name: format!("My {}", sanitize_file_stem(family.label())),
            family,
            units,
            rf: source.range_folded_color(),
            mode: DraftMode::Smooth,
            stops,
        }
    }

    /// Fork an existing table into a draft. GR-pal solid/gradient stops
    /// map onto the per-stop flags; stepped tables open in Stepped mode.
    /// (Quantized tables simplify to stop bands — noted by the caller.)
    fn from_table(table: &ColorTable, family: ColorTableFamily, name: String) -> Self {
        let units = table
            .units()
            .map(str::trim)
            .filter(|units| !units.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| default_units_for_family(family));
        let scale = unit_scale_to_internal(&units);
        let stepped = table.sample_mode_label() == "stepped" || table.step_size().is_some();
        let stops = table
            .stops()
            .iter()
            .map(|stop| DraftStop {
                value: stop.value / scale,
                color: stop.color.to_array(),
                end_color: match stop.end_color {
                    Some(end) if end != stop.color => Some(end.to_array()),
                    _ => None,
                },
                solid: stop.end_color == Some(stop.color),
            })
            .collect();
        Self {
            name,
            family,
            units,
            rf: table.range_folded_color(),
            mode: if stepped {
                DraftMode::Stepped
            } else {
                DraftMode::Smooth
            },
            stops,
        }
    }

    /// (min, max) of the draft's stop values, in declared units.
    fn value_range(&self) -> Option<(f32, f32)> {
        let mut values = self.stops.iter().map(|stop| stop.value);
        let first = values.next()?;
        let (mut min, mut max) = (first, first);
        for value in values {
            min = min.min(value);
            max = max.max(value);
        }
        Some((min, max))
    }

    /// Switching units keeps the table's MEANING: values rescale so the
    /// internal stops stay put (kt → m/s halves the displayed numbers).
    fn set_units(&mut self, units: &str) -> bool {
        if self.units == units {
            return false;
        }
        let old_scale = unit_scale_to_internal(&self.units);
        let new_scale = unit_scale_to_internal(units);
        if old_scale != new_scale && new_scale != 0.0 {
            let factor = old_scale / new_scale;
            for stop in &mut self.stops {
                stop.value *= factor;
            }
        }
        self.units = units.to_owned();
        true
    }

    fn signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.name.hash(&mut hasher);
        self.family.hash(&mut hasher);
        self.units.hash(&mut hasher);
        self.rf.hash(&mut hasher);
        self.mode.hash(&mut hasher);
        self.stops.len().hash(&mut hasher);
        for stop in &self.stops {
            stop.value.to_bits().hash(&mut hasher);
            stop.color.hash(&mut hasher);
            stop.end_color.hash(&mut hasher);
            stop.solid.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Build the working table: declared-unit stops pre-scaled to
    /// internal (the parser's `Units:` treatment), GR-pal sampling so the
    /// preview equals the saved file reloaded through `parse_gr_pal`.
    fn build(&self) -> Result<ColorTable, String> {
        let name = sanitize_file_stem(&self.name);
        let name = if name.is_empty() {
            "My table".to_owned()
        } else {
            name
        };
        let scale = unit_scale_to_internal(&self.units);
        let stops = self
            .stops
            .iter()
            .map(|stop| {
                let color = rgba8(stop.color);
                let solid = self.mode == DraftMode::Stepped || stop.solid;
                ColorStop {
                    value: stop.value * scale,
                    color,
                    end_color: if solid {
                        Some(color)
                    } else {
                        stop.end_color.map(rgba8)
                    },
                }
            })
            .collect();
        let units = self.units.trim();
        ColorTable::from_parts(
            name,
            product_code_for_family(self.family).map(str::to_owned),
            (!units.is_empty()).then(|| units.to_owned()),
            rgba8(self.rf),
            SampleMode::GrPal,
            stops,
        )
        .map_err(|error| error.to_string())
    }
}

// ------------------------------------------------------------------------
// Editor window state
// ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorAction {
    None,
    LivePreviewToggled,
    Save,
    Export,
    Revert,
    Close,
}

pub(crate) struct TableEditor {
    pub(crate) open: bool,
    draft: TableDraft,
    /// The draft as opened — Revert's target.
    initial: TableDraft,
    /// (family, the table that was live before preview touched it);
    /// restored on Close / live-preview-off, dropped on Save.
    snapshot: Option<(ColorTableFamily, ColorTable)>,
    live_preview: bool,
    /// Draft-signature-keyed build cache (rebuild on change only).
    built: Option<(u64, Result<ColorTable, String>)>,
    status: String,
}

impl Default for TableEditor {
    /// Deliberately cheap (no table parsing): `table_editor_window`
    /// `mem::take`s the editor every open frame, and real drafts come
    /// from the `open_table_editor_*` entry points anyway.
    fn default() -> Self {
        let draft = TableDraft {
            name: String::new(),
            family: ColorTableFamily::Reflectivity,
            units: "dBZ".to_owned(),
            rf: [126, 80, 196, 245],
            mode: DraftMode::Smooth,
            stops: vec![
                DraftStop {
                    value: 0.0,
                    color: [40, 40, 48, 255],
                    end_color: None,
                    solid: false,
                },
                DraftStop {
                    value: 75.0,
                    color: [240, 240, 244, 255],
                    end_color: None,
                    solid: false,
                },
            ],
        };
        Self {
            open: false,
            initial: draft.clone(),
            draft,
            snapshot: None,
            live_preview: true,
            built: None,
            status: String::new(),
        }
    }
}

impl TableEditor {
    fn build_cached(&mut self) -> Result<ColorTable, String> {
        let signature = self.draft.signature();
        let stale = self
            .built
            .as_ref()
            .map(|(cached, _)| *cached != signature)
            .unwrap_or(true);
        if stale {
            self.built = Some((signature, self.draft.build()));
        }
        self.built
            .as_ref()
            .map(|(_, result)| result.clone())
            .expect("just built")
    }

    /// The editor body. Returns (draft changed, requested action).
    fn ui(&mut self, ui: &mut egui::Ui) -> (bool, EditorAction) {
        let built = self.build_cached();
        let mut changed = false;
        let mut action = EditorAction::None;

        ui.horizontal(|ui| {
            ui.label("Name");
            changed |= ui
                .add(egui::TextEdit::singleline(&mut self.draft.name).desired_width(170.0))
                .on_hover_text("Doubles as the .pal file name in My tables")
                .changed();
            ui.label("Family");
            let mut family = self.draft.family;
            egui::ComboBox::from_id_salt("table_editor_family")
                .selected_text(family.label())
                .width(150.0)
                .show_ui(ui, |ui| {
                    for candidate in ColorTableFamily::ALL {
                        ui.selectable_value(&mut family, candidate, candidate.label());
                    }
                });
            if family != self.draft.family {
                self.draft.family = family;
                changed = true;
            }
            ui.label("Units");
            let selected_units = if self.draft.units.is_empty() {
                "(none)".to_owned()
            } else {
                self.draft.units.clone()
            };
            egui::ComboBox::from_id_salt("table_editor_units")
                .selected_text(selected_units)
                .width(70.0)
                .show_ui(ui, |ui| {
                    for units in units_for_family(self.draft.family) {
                        let label = if units.is_empty() { "(none)" } else { units };
                        if ui
                            .selectable_label(self.draft.units == *units, label)
                            .clicked()
                        {
                            changed |= self.draft.set_units(units);
                        }
                    }
                })
                .response
                .on_hover_text(
                    "Declared units written to the Units: header. Switching kt/m/s/mph rescales the numbers, not the table",
                );
        });

        ui.horizontal(|ui| {
            ui.label("Mode");
            changed |= ui
                .selectable_value(&mut self.draft.mode, DraftMode::Smooth, "Smooth")
                .on_hover_text("Ramp between stops (exports as GR Color: rows)")
                .changed();
            changed |= ui
                .selectable_value(&mut self.draft.mode, DraftMode::Stepped, "Stepped")
                .on_hover_text("Hard band per stop (exports as GR SolidColor: rows)")
                .changed();
            ui.separator();
            ui.label("RF");
            let mut rf = self.draft.rf;
            if ui
                .color_edit_button_srgba_unmultiplied(&mut rf)
                .on_hover_text("Range-folded gate color (the RF: header)")
                .changed()
            {
                self.draft.rf = rf;
                changed = true;
            }
            ui.separator();
            if ui
                .checkbox(&mut self.live_preview, "Live preview")
                .on_hover_text(
                    "Apply the draft to the map while editing (Close restores the previous table)",
                )
                .changed()
            {
                action = EditorAction::LivePreviewToggled;
            }
        });

        ui.add_space(4.0);
        changed |= gradient_bar_ui(ui, &mut self.draft, &built);
        if let Err(error) = &built {
            ui.colored_label(egui::Color32::from_rgb(235, 110, 100), error);
        }

        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .max_height(260.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                changed |= stops_ui(ui, &mut self.draft);
            });

        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .button("Save to My tables")
                .on_hover_text(
                    "Write a GR2Analyst-compatible .pal into the color_tables folder, apply it, and persist the binding",
                )
                .clicked()
            {
                action = EditorAction::Save;
            }
            #[cfg(any(windows, target_os = "macos"))]
            if ui
                .button("Export…")
                .on_hover_text("Write the same GR2Analyst-compatible .pal anywhere")
                .clicked()
            {
                action = EditorAction::Export;
            }
            if ui
                .button("Revert")
                .on_hover_text("Reset the draft to how it was opened")
                .clicked()
            {
                action = EditorAction::Revert;
            }
            if ui.button("Close").clicked() {
                action = EditorAction::Close;
            }
        });
        if !self.status.is_empty() {
            ui.weak(&self.status);
        }

        (changed, action)
    }
}

/// Full-width gradient preview: checkerboard-backed strip sampling the
/// built table across the draft's declared-unit range, value ticks
/// underneath, click-to-insert a stop at the sampled color.
fn gradient_bar_ui(
    ui: &mut egui::Ui,
    draft: &mut TableDraft,
    built: &Result<ColorTable, String>,
) -> bool {
    let mut changed = false;
    let width = ui.available_width().max(300.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::click());
    let painter = ui.painter().clone();
    let range = draft.value_range();
    if ui.is_rect_visible(rect) {
        paint_checkerboard(&painter, rect, 7.5);
        if let (Ok(table), Some((min_value, max_value))) = (built, range) {
            let scale = unit_scale_to_internal(&draft.units);
            let span = (max_value - min_value).max(f32::EPSILON);
            let sampler = table.sampler();
            let columns = (rect.width() / 2.0).ceil().max(1.0) as usize;
            let column_width = rect.width() / columns as f32;
            for column in 0..columns {
                let t = (column as f32 + 0.5) / columns as f32;
                let [r, g, b, a] = sampler.color_for_value((min_value + t * span) * scale);
                if a == 0 {
                    continue;
                }
                let left = rect.left() + column as f32 * column_width;
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(left, rect.top()),
                        egui::pos2((left + column_width + 0.5).min(rect.right()), rect.bottom()),
                    ),
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(r, g, b, a),
                );
            }
        }
    }
    // Value-axis ticks under the bar.
    let (tick_rect, _) = ui.allocate_exact_size(egui::vec2(width, 13.0), egui::Sense::hover());
    if let Some((min_value, max_value)) = range
        && ui.is_rect_visible(tick_rect)
    {
        let span = max_value - min_value;
        for index in 0..=4 {
            let t = index as f32 / 4.0;
            let x = rect.left() + t * rect.width();
            painter.vline(
                x.clamp(rect.left() + 0.5, rect.right() - 0.5),
                egui::Rangef::new(rect.bottom() - 5.0, rect.bottom()),
                egui::Stroke::new(1.0, egui::Color32::from_gray(210)),
            );
            let align = match index {
                0 => egui::Align2::LEFT_TOP,
                4 => egui::Align2::RIGHT_TOP,
                _ => egui::Align2::CENTER_TOP,
            };
            painter.text(
                egui::pos2(x, tick_rect.top()),
                align,
                format_tick(min_value + t * span),
                egui::FontId::proportional(9.5),
                egui::Color32::from_gray(150),
            );
        }
    }
    if let (true, Some(pointer), Some((min_value, max_value)), Ok(table)) = (
        response.clicked(),
        response.interact_pointer_pos(),
        range,
        built,
    ) {
        let span = (max_value - min_value).max(f32::EPSILON);
        let t = ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        // Snap to a tidy fraction of the range so inserted stops don't
        // carry 7-digit noise.
        let raw = min_value + t * span;
        let quantum = 10f32.powf((span.log10().floor() - 2.0).max(-3.0));
        let value = (raw / quantum).round() * quantum;
        let scale = unit_scale_to_internal(&draft.units);
        let color = table.sampler().color_for_value(value * scale);
        let index = draft
            .stops
            .iter()
            .position(|stop| stop.value > value)
            .unwrap_or(draft.stops.len());
        draft.stops.insert(
            index,
            DraftStop {
                value,
                color,
                end_color: None,
                solid: false,
            },
        );
        changed = true;
    }
    response.on_hover_text("Click to insert a stop here (picks up the sampled color)");
    changed
}

fn format_tick(value: f32) -> String {
    if value.abs() >= 100.0 {
        format!("{value:.0}")
    } else if value.abs() >= 1.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

/// The stop list: DragValue + color + optional end color + solid + delete,
/// plus the append button.
fn stops_ui(ui: &mut egui::Ui, draft: &mut TableDraft) -> bool {
    let mut changed = false;
    let mut remove: Option<usize> = None;
    let span = draft
        .value_range()
        .map(|(min, max)| (max - min).abs())
        .unwrap_or(100.0)
        .max(f32::EPSILON);
    let speed = f64::from(span) / 300.0;
    let can_delete = draft.stops.len() > 2;
    let smooth = draft.mode == DraftMode::Smooth;
    let suffix = if draft.units.is_empty() {
        String::new()
    } else {
        format!(" {}", draft.units)
    };
    for (index, stop) in draft.stops.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            changed |= ui
                .add(
                    egui::DragValue::new(&mut stop.value)
                        .speed(speed)
                        .suffix(&suffix),
                )
                .on_hover_text("Stop value (drag, or type)")
                .changed();
            // Pass copies to the color widgets and write back only on
            // change: the picker round-trips through premultiplied space
            // and would silently zero the RGB of alpha-0 stops.
            let mut color = stop.color;
            if ui
                .color_edit_button_srgba_unmultiplied(&mut color)
                .on_hover_text("Stop color (alpha = transparency)")
                .changed()
            {
                stop.color = color;
                changed = true;
            }
            if smooth {
                let mut has_end = stop.end_color.is_some();
                if ui
                    .checkbox(&mut has_end, "end")
                    .on_hover_text(
                        "Two-color gradient: ramp to an explicit end color across this band (GR 6-component Color: row) instead of the next stop",
                    )
                    .changed()
                {
                    stop.end_color = has_end.then_some(stop.color);
                    changed = true;
                }
                if let Some(end) = &mut stop.end_color {
                    let mut end_color = *end;
                    if ui
                        .color_edit_button_srgba_unmultiplied(&mut end_color)
                        .on_hover_text("Gradient end color for this band")
                        .changed()
                    {
                        *end = end_color;
                        changed = true;
                    }
                }
                changed |= ui
                    .checkbox(&mut stop.solid, "solid")
                    .on_hover_text("Hard band: hold this color to the next stop (GR SolidColor: row)")
                    .changed();
            }
            if ui
                .add_enabled(can_delete, egui::Button::new("✕").small())
                .on_hover_text("Delete this stop")
                .clicked()
            {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        draft.stops.remove(index);
        changed = true;
    }
    if ui
        .button("+ Add stop")
        .on_hover_text("Append a stop midway between the last two")
        .clicked()
    {
        let (value, color) = match draft.stops.len() {
            0 => (0.0, [255, 255, 255, 255]),
            1 => (draft.stops[0].value + 1.0, draft.stops[0].color),
            len => {
                let a = &draft.stops[len - 2];
                let b = &draft.stops[len - 1];
                let value = if a.value == b.value {
                    b.value + 1.0
                } else {
                    (a.value + b.value) / 2.0
                };
                let color = std::array::from_fn(|channel| {
                    ((u16::from(a.color[channel]) + u16::from(b.color[channel])) / 2) as u8
                });
                (value, color)
            }
        };
        let index = draft
            .stops
            .iter()
            .position(|stop| stop.value > value)
            .unwrap_or(draft.stops.len());
        draft.stops.insert(
            index,
            DraftStop {
                value,
                color,
                end_color: None,
                solid: false,
            },
        );
        changed = true;
    }
    changed
}

// ------------------------------------------------------------------------
// ViewerApp integration
// ------------------------------------------------------------------------

impl ViewerApp {
    /// "New table…": seed a draft from the family default and open the
    /// editor (live preview starts immediately).
    pub(crate) fn open_table_editor_new(&mut self, ctx: &egui::Context, family: ColorTableFamily) {
        let draft = TableDraft::seeded_from_family(family);
        self.start_table_editor(
            ctx,
            draft,
            format!("Seeded from {}'s default table", family.label()),
        );
    }

    /// "Edit a copy…": fork a table into the editor. Built-ins are
    /// immutable — `fork` renames to "My <name>" so Save can't shadow
    /// them; user tables open under their own name (Save overwrites).
    pub(crate) fn open_table_editor_copy(
        &mut self,
        ctx: &egui::Context,
        family: ColorTableFamily,
        table: &ColorTable,
        fork: bool,
    ) {
        let name = if fork {
            format!("My {}", table.name())
        } else {
            table.name().to_owned()
        };
        let status = if table.step_size().is_some() {
            "Note: step-quantization simplifies to plain stop bands in the editor".to_owned()
        } else {
            String::new()
        };
        let draft = TableDraft::from_table(table, family, name);
        self.start_table_editor(ctx, draft, status);
    }

    fn start_table_editor(&mut self, ctx: &egui::Context, draft: TableDraft, status: String) {
        // A new session replaces any previous one: undo its preview first.
        self.restore_table_editor_snapshot(ctx);
        self.table_editor = TableEditor {
            open: true,
            initial: draft.clone(),
            draft,
            snapshot: None,
            live_preview: true,
            built: None,
            status,
        };
        self.refresh_table_editor_preview(ctx);
    }

    /// Push the draft through the existing palette-switch path
    /// (`set_family` + `clear_texture`) so the viewport, LUT path, and
    /// colorbar all follow via `color_table_signature`. Build errors keep
    /// the last good preview (the error shows inline in the window).
    fn refresh_table_editor_preview(&mut self, ctx: &egui::Context) {
        if !self.table_editor.live_preview || !self.table_editor.open {
            return;
        }
        let family = self.table_editor.draft.family;
        if let Ok(table) = self.table_editor.build_cached() {
            self.ensure_table_editor_snapshot(family);
            self.color_tables.set_family(family, table);
            self.clear_texture();
            ctx.request_repaint();
        }
    }

    /// Snapshot the family slot once per family so Close restores it; a
    /// mid-edit family switch restores the old slot before snapshotting
    /// the new one.
    fn ensure_table_editor_snapshot(&mut self, family: ColorTableFamily) {
        match self.table_editor.snapshot.take() {
            None => {
                self.table_editor.snapshot =
                    Some((family, self.color_tables.for_family(family).clone()));
            }
            Some((old_family, old_table)) => {
                if old_family == family {
                    self.table_editor.snapshot = Some((old_family, old_table));
                } else {
                    self.color_tables.set_family(old_family, old_table);
                    self.table_editor.snapshot =
                        Some((family, self.color_tables.for_family(family).clone()));
                }
            }
        }
    }

    fn restore_table_editor_snapshot(&mut self, ctx: &egui::Context) {
        if let Some((family, table)) = self.table_editor.snapshot.take() {
            self.color_tables.set_family(family, table);
            self.clear_texture();
            ctx.request_repaint();
        }
    }

    /// The "Color table editor" window (deep-config surface = window per
    /// the UI proposal's rule). One call from `update()`.
    pub(crate) fn table_editor_window(&mut self, ctx: &egui::Context) {
        if !self.table_editor.open {
            return;
        }
        let mut editor = std::mem::take(&mut self.table_editor);
        let mut window_open = true;
        let mut changed = false;
        let mut action = EditorAction::None;
        egui::Window::new("Color table editor")
            .open(&mut window_open)
            .default_size([560.0, 600.0])
            .min_size([480.0, 380.0])
            .resizable(true)
            .show(ctx, |ui| {
                let (ui_changed, ui_action) = editor.ui(ui);
                changed = ui_changed;
                action = ui_action;
            });
        self.table_editor = editor;
        if !window_open {
            action = EditorAction::Close;
        }

        match action {
            EditorAction::None => {
                if changed {
                    self.refresh_table_editor_preview(ctx);
                }
            }
            EditorAction::LivePreviewToggled => {
                if self.table_editor.live_preview {
                    self.refresh_table_editor_preview(ctx);
                } else {
                    self.restore_table_editor_snapshot(ctx);
                }
            }
            EditorAction::Revert => {
                self.table_editor.draft = self.table_editor.initial.clone();
                self.table_editor.built = None;
                self.table_editor.status = "Draft reverted".to_owned();
                self.refresh_table_editor_preview(ctx);
            }
            EditorAction::Save => self.save_table_editor_draft(ctx),
            EditorAction::Export => self.export_table_editor_draft(),
            EditorAction::Close => {
                self.restore_table_editor_snapshot(ctx);
                self.table_editor.open = false;
            }
        }
    }

    /// Save to My tables: write the `.pal` via `to_gr_pal` into
    /// `settings::color_tables_dir()`, rescan, then apply + persist the
    /// binding through the standard palette path. The saved state becomes
    /// the editor's new baseline (Close no longer reverts it).
    fn save_table_editor_draft(&mut self, ctx: &egui::Context) {
        let sanitized = sanitize_file_stem(&self.table_editor.draft.name);
        if sanitized.is_empty() {
            self.table_editor.status = "Give the table a name first".to_owned();
            return;
        }
        if sanitized != self.table_editor.draft.name {
            self.table_editor.draft.name = sanitized.clone();
            self.table_editor.built = None;
        }
        let table = match self.table_editor.build_cached() {
            Ok(table) => table,
            Err(error) => {
                self.table_editor.status = format!("Fix the draft first: {error}");
                return;
            }
        };
        let directory = match settings::ensure_color_tables_dir() {
            Ok(directory) => directory,
            Err(error) => {
                self.table_editor.status = format!(
                    "Save failed: My tables folder unavailable at {}: {error}",
                    settings::color_tables_dir_path().display()
                );
                return;
            }
        };
        let path = directory.join(format!("{sanitized}.pal"));
        match std::fs::write(&path, to_gr_pal(&table)) {
            Ok(()) => {
                self.user_color_tables = crate::scan_user_color_tables(&directory);
                self.table_editor.snapshot = None;
                self.table_editor.initial = self.table_editor.draft.clone();
                let family = self.table_editor.draft.family;
                self.apply_family_table(ctx, family, table);
                self.table_editor.status = format!("Saved {}", path.display());
            }
            Err(error) => {
                self.table_editor.status = format!("Save failed for {}: {error}", path.display());
            }
        }
    }

    /// Export…: the same bytes Save writes, anywhere (rfd save dialog;
    /// Windows/macOS only, matching the Browse… pattern — Linux saves to
    /// My tables and copies from the folder).
    fn export_table_editor_draft(&mut self) {
        let table = match self.table_editor.build_cached() {
            Ok(table) => table,
            Err(error) => {
                self.table_editor.status = format!("Fix the draft first: {error}");
                return;
            }
        };
        #[cfg(any(windows, target_os = "macos"))]
        {
            let stem = sanitize_file_stem(&self.table_editor.draft.name);
            let file_name = if stem.is_empty() {
                "color-table.pal".to_owned()
            } else {
                format!("{stem}.pal")
            };
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("GR2Analyst color table", &["pal"])
                .set_file_name(file_name)
                .set_title("Export color table")
                .save_file()
            {
                match std::fs::write(&path, to_gr_pal(&table)) {
                    Ok(()) => {
                        self.table_editor.status = format!("Exported {}", path.display());
                    }
                    Err(error) => {
                        self.table_editor.status = format!("Export failed: {error}");
                    }
                }
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = table;
            self.table_editor.status =
                "Export needs a file dialog — use Save to My tables and copy the .pal".to_owned();
        }
    }

    /// Append a line to the editor's status readout (entry points use it
    /// for context notes, e.g. per-product-binding preview caveats).
    pub(crate) fn append_table_editor_status(&mut self, line: String) {
        if self.table_editor.status.is_empty() {
            self.table_editor.status = line;
        } else {
            self.table_editor.status.push('\n');
            self.table_editor.status.push_str(&line);
        }
    }

    /// Delete a "My tables" `.pal` by table name (= file stem, matching
    /// the scanner) and rescan. A stale persisted binding to the deleted
    /// table falls back to the default on next boot via
    /// `restore_palette_bindings`'s miss path.
    pub(crate) fn delete_user_color_table(&mut self, name: &str) {
        let directory = match settings::ensure_color_tables_dir() {
            Ok(directory) => directory,
            Err(error) => {
                self.color_table_status = format!(
                    "My tables folder unavailable at {}: {error}",
                    settings::color_tables_dir_path().display()
                );
                return;
            }
        };
        let Ok(entries) = std::fs::read_dir(&directory) else {
            self.color_table_status =
                format!("My tables folder is unreadable: {}", directory.display());
            return;
        };
        let target = entries.flatten().map(|entry| entry.path()).find(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pal"))
                && path.file_stem().and_then(|stem| stem.to_str()) == Some(name)
        });
        match target {
            Some(path) => match std::fs::remove_file(&path) {
                Ok(()) => {
                    self.user_color_tables = crate::scan_user_color_tables(&directory);
                    self.color_table_status = format!("Deleted {}", path.display());
                }
                Err(error) => {
                    self.color_table_status =
                        format!("Delete failed for {}: {error}", path.display());
                }
            },
            None => {
                self.color_table_status = format!("{name}.pal not found in My tables");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn velocity_draft() -> TableDraft {
        TableDraft {
            name: "My Velocity".to_owned(),
            family: ColorTableFamily::Velocity,
            units: "kt".to_owned(),
            rf: [126, 80, 196, 245],
            mode: DraftMode::Smooth,
            stops: vec![
                DraftStop {
                    value: -100.0,
                    color: [0, 80, 255, 255],
                    end_color: None,
                    solid: false,
                },
                DraftStop {
                    value: 0.0,
                    color: [120, 120, 120, 255],
                    end_color: None,
                    solid: false,
                },
                DraftStop {
                    value: 100.0,
                    color: [255, 40, 20, 255],
                    end_color: None,
                    solid: false,
                },
            ],
        }
    }

    /// §2.2: a velocity table authored in kt behaves exactly like a
    /// community .pal — values pre-scale to m/s through the same factor
    /// the parser applies to `Units: kt` files.
    #[test]
    fn kt_draft_builds_like_a_community_pal() {
        let table = velocity_draft().build().expect("draft builds");
        assert_eq!(table.units(), Some("kt"));
        assert_eq!(table.product(), Some("BV"));
        let scale = unit_scale_to_internal("kt");
        let values: Vec<f32> = table.stops().iter().map(|stop| stop.value).collect();
        assert_eq!(values, vec![-100.0 * scale, 0.0, 100.0 * scale]);
        // Saved + reloaded through the user-table path = identical sampling.
        let reloaded =
            ColorTable::parse_gr_pal("My Velocity", &to_gr_pal(&table)).expect("export reparses");
        for value in [-60.0f32, -10.0, 0.0, 10.0, 60.0] {
            assert_eq!(
                reloaded.color_for_value(value),
                table.color_for_value(value)
            );
        }
    }

    #[test]
    fn draft_validation_surfaces_not_enough_stops() {
        let mut draft = velocity_draft();
        draft.stops.truncate(1);
        let error = draft.build().expect_err("single stop must fail");
        assert!(error.contains("two color stops"), "{error}");
    }

    #[test]
    fn stepped_mode_makes_every_stop_a_solid_band() {
        let mut draft = velocity_draft();
        draft.mode = DraftMode::Stepped;
        let table = draft.build().expect("draft builds");
        // GR solid semantics: end_color == color on every stop.
        for stop in table.stops() {
            assert_eq!(stop.end_color, Some(stop.color));
        }
        // Sampling holds hard bands.
        let mid = table.color_for_value(-30.0 * unit_scale_to_internal("kt"));
        assert_eq!(mid, [0, 80, 255, 255]);
    }

    #[test]
    fn unit_switch_rescales_displayed_values_not_the_table() {
        let mut draft = velocity_draft();
        let before = draft.build().expect("draft builds");
        assert!(draft.set_units("m/s"));
        let after = draft.build().expect("draft still builds");
        // Displayed numbers changed…
        assert!((draft.stops[2].value - 100.0 * unit_scale_to_internal("kt")).abs() < 0.01);
        // …but the internal stops (and therefore sampling) stayed put.
        for (left, right) in before.stops().iter().zip(after.stops()) {
            assert!((left.value - right.value).abs() < 1e-3);
        }
    }

    #[test]
    fn fork_of_a_gr_pal_table_round_trips_solid_and_gradient_flags() {
        let source = ColorTable::parse_gr_pal(
            "community",
            "product: BR\nunits: dBZ\n\
             color: 0 10 20 30 40 50 60\n\
             solidcolor: 10 100 0 0\n\
             color: 20 200 0 0\n",
        )
        .expect("source parses");
        let draft = TableDraft::from_table(
            &source,
            ColorTableFamily::Reflectivity,
            "My community".to_owned(),
        );
        assert_eq!(draft.mode, DraftMode::Smooth);
        assert_eq!(draft.stops[0].end_color, Some([40, 50, 60, 255]));
        assert!(draft.stops[1].solid);
        assert!(!draft.stops[2].solid);
        let rebuilt = draft.build().expect("fork builds");
        for value in [0.0f32, 5.0, 10.0, 15.0, 20.0, 25.0] {
            assert_eq!(
                rebuilt.color_for_value(value),
                source.color_for_value(value),
                "fork diverges at {value}"
            );
        }
    }

    #[test]
    fn sanitize_file_stem_strips_filename_hostile_characters() {
        assert_eq!(sanitize_file_stem("My / Table: v2?"), "My Table v2");
        assert_eq!(sanitize_file_stem("  spaced   out  "), "spaced out");
        assert_eq!(sanitize_file_stem("\\/:*?\"<>|"), "");
    }

    #[test]
    fn new_draft_seeds_from_the_family_default() {
        let draft = TableDraft::seeded_from_family(ColorTableFamily::Reflectivity);
        assert_eq!(draft.stops.len(), 8);
        assert_eq!(draft.units, "dBZ");
        assert!(draft.build().is_ok());
    }
}
