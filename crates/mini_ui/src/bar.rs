//! THE BAR (miniderecho-spec §4, pulled into M1 by §11a): the always-
//! visible control row — SITE, PRODUCT, TILT, loop transport, GO LIVE —
//! rendered desktop-first as a top bar of combos plus a bottom transport
//! strip (the §4 desktop blueprint). Controls emit [`BarAction`]s that
//! main.rs dispatches centrally (the `UnifiedPlayerAction` pattern);
//! nothing here mutates app state. Unavailable products are greyed WITH
//! the reason string the capability function returned (house trust-label
//! rule). M2 replaces the frame-position display with the full scrubber
//! widget.

use eframe::egui;
use ui_core::tiles::TileStyle;

use crate::products::{Product, TiltOption, elevation_tenths};

/// Bar metrics: touch-sized hit targets per the UX blueprint (44 pt rows
/// on touch; the desktop bar stays ≥ 32 pt so a touchscreen laptop is
/// never hostile).
pub const TOP_BAR_H: f32 = 40.0;
pub const TRANSPORT_BAR_H: f32 = 48.0;
const CONTROL_H: f32 = 30.0;
const TRANSPORT_CONTROL_H: f32 = 36.0;

/// Everything the bar reads, snapshotted by main.rs. `None` volumes/
/// frames grey the dependent controls honestly.
pub struct BarState<'a> {
    pub site_label: &'a str,
    pub site_enabled: bool,
    pub product: Product,
    /// Capability per product for the displayed volume: `Ok` = selectable,
    /// `Err(reason)` = greyed with the reason as hover text.
    pub products: Vec<(Product, Result<(), String>)>,
    /// Tilt list of the displayed volume for the current product.
    pub tilts: &'a [TiltOption],
    pub selected_tilt: Option<TiltOption>,
    pub playing: bool,
    pub at_live_edge: bool,
    /// `(1-based cursor, frame count)`.
    pub frame_position: Option<(usize, usize)>,
    /// DATA time of the displayed frame (never wall clock).
    pub data_time: Option<String>,
    pub tile_style: TileStyle,
    pub ip_geolocation: bool,
}

/// One user verb from the bar; dispatched centrally.
#[derive(Clone, Debug, PartialEq)]
pub enum BarAction {
    OpenSitePicker,
    SetProduct(Product),
    /// Preferred tilt in tenths of a degree.
    SetTilt(i32),
    TogglePlay,
    StepBack,
    StepForward,
    GoLive,
    SetTileStyle(TileStyle),
    SetGeolocEnabled(bool),
}

/// The live-edge chip text (§6: LIVE when following, the loop/pause glyph
/// when detached). Pure, so the truth table is testable.
///
/// Glyph note (screenshot-verified + cmap-checked against egui 0.34's
/// default fonts): the proportional family (Ubuntu-Light + NotoEmoji +
/// emoji-icon-font) covers `•` U+2022, `▶` U+25B6, `◀` U+25C0, `⏸`
/// U+23F8, `⚙` U+2699 — but NOT `●` U+25CF, `◉` U+25C9, `▾` U+25BE or
/// `‖` U+2016 (tofu until mini's M4 font pass; BowEcho's `●` works only
/// because of its own font setup).
pub fn live_chip_label(at_live_edge: bool, playing: bool) -> &'static str {
    match (at_live_edge, playing) {
        (true, false) => "• LIVE",
        (_, true) => "▶ LOOP",
        (false, false) => "⏸ PAUSED",
    }
}

/// Top bar: wordmark · SITE · PRODUCT · TILT · (right) gear · chip · time.
pub fn top_bar(ui: &mut egui::Ui, state: &BarState<'_>) -> Vec<BarAction> {
    let mut actions = Vec::new();
    egui::Panel::top("mini_top_bar")
        .exact_size(TOP_BAR_H)
        .show_inside(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().button_padding = egui::vec2(10.0, 6.0);
                ui.strong("miniDerecho");
                ui.separator();

                // SITE — opens the picker.
                let site = egui::Button::new(state.site_label).min_size(egui::vec2(0.0, CONTROL_H));
                if ui
                    .add_enabled(state.site_enabled, site)
                    .on_hover_text("Choose site (F)")
                    .clicked()
                {
                    actions.push(BarAction::OpenSitePicker);
                }

                // PRODUCT — greyed rows carry the capability reason.
                egui::ComboBox::from_id_salt("mini_product")
                    .selected_text(state.product.label())
                    .height(400.0)
                    .show_ui(ui, |ui| {
                        for (product, capability) in &state.products {
                            let text = format!("{}  ·  {}", product.label(), product.long_label());
                            match capability {
                                Ok(()) => {
                                    if ui
                                        .selectable_label(*product == state.product, text)
                                        .clicked()
                                    {
                                        actions.push(BarAction::SetProduct(*product));
                                    }
                                }
                                Err(reason) => {
                                    ui.add_enabled(false, egui::Button::new(text))
                                        .on_disabled_hover_text(reason.clone());
                                }
                            }
                        }
                    });

                // TILT — the volume's cut list, lowest-first.
                let tilt_text = state
                    .selected_tilt
                    .map(|tilt| format!("{:.1}°", tilt.elevation_deg))
                    .unwrap_or_else(|| "—".to_owned());
                egui::ComboBox::from_id_salt("mini_tilt")
                    .selected_text(tilt_text)
                    .show_ui(ui, |ui| {
                        for tilt in state.tilts {
                            let selected = state
                                .selected_tilt
                                .is_some_and(|current| current.cut_index == tilt.cut_index);
                            if ui
                                .selectable_label(selected, format!("{:.1}°", tilt.elevation_deg))
                                .clicked()
                            {
                                actions
                                    .push(BarAction::SetTilt(elevation_tenths(tilt.elevation_deg)));
                            }
                        }
                        if state.tilts.is_empty() {
                            ui.add_enabled(false, egui::Button::new("no tilts yet"));
                        }
                    });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("⚙", |ui| {
                        ui.label("Basemap");
                        for style in TileStyle::ALL {
                            if ui
                                .selectable_label(state.tile_style == style, style.label())
                                .clicked()
                            {
                                actions.push(BarAction::SetTileStyle(style));
                                ui.close();
                            }
                        }
                        ui.separator();
                        let mut geoloc = state.ip_geolocation;
                        if ui
                            .checkbox(&mut geoloc, "Locate nearest radar by IP on first run")
                            .on_hover_text(crate::geoloc::IP_GEOLOCATION_ENDPOINT)
                            .changed()
                        {
                            actions.push(BarAction::SetGeolocEnabled(geoloc));
                        }
                    })
                    .response
                    .on_hover_text("Settings");

                    // Liveness chip + DATA time (never wall clock).
                    if let Some(time) = &state.data_time {
                        ui.monospace(time);
                    }
                    let chip = live_chip_label(state.at_live_edge, state.playing);
                    let color = if state.at_live_edge && !state.playing {
                        egui::Color32::from_rgb(64, 210, 110)
                    } else {
                        egui::Color32::from_rgb(150, 158, 168)
                    };
                    ui.colored_label(color, chip);
                });
            });
        });
    actions
}

/// Bottom transport strip (scrubber-lite for M1): play/pause · step ·
/// frame position + data time · GO LIVE.
pub fn transport_bar(ui: &mut egui::Ui, state: &BarState<'_>) -> Vec<BarAction> {
    let mut actions = Vec::new();
    egui::Panel::bottom("mini_transport_bar")
        .exact_size(TRANSPORT_BAR_H)
        .show_inside(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().button_padding = egui::vec2(12.0, 8.0);
                let has_loop = state.frame_position.is_some_and(|(_, count)| count > 1);
                let control = |text: &str| {
                    egui::Button::new(text).min_size(egui::vec2(44.0, TRANSPORT_CONTROL_H))
                };

                let play_glyph = if state.playing { "⏸" } else { "▶" };
                if ui
                    .add_enabled(has_loop, control(play_glyph))
                    .on_hover_text("Play/pause the loop (Space)")
                    .clicked()
                {
                    actions.push(BarAction::TogglePlay);
                }
                if ui
                    .add_enabled(has_loop, control("◀"))
                    .on_hover_text("Step back (←)")
                    .clicked()
                {
                    actions.push(BarAction::StepBack);
                }
                if ui
                    .add_enabled(has_loop, control("▶|"))
                    .on_hover_text("Step forward (→)")
                    .clicked()
                {
                    actions.push(BarAction::StepForward);
                }
                match (state.frame_position, &state.data_time) {
                    (Some((position, count)), Some(time)) => {
                        ui.monospace(format!("frame {position}/{count} · {time}"));
                    }
                    _ => {
                        ui.weak("no frames yet");
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let go_live = egui::Button::new("GO LIVE")
                        .min_size(egui::vec2(88.0, TRANSPORT_CONTROL_H));
                    if ui
                        .add_enabled(!state.at_live_edge || state.playing, go_live)
                        .on_hover_text("Snap to the newest frame and follow live (L)")
                        .clicked()
                    {
                        actions.push(BarAction::GoLive);
                    }
                });
            });
        });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §6's chip posture: LIVE only when following at the edge and not
    /// looping; playback shows the loop glyph even at the edge.
    #[test]
    fn live_chip_truth_table() {
        assert_eq!(live_chip_label(true, false), "• LIVE");
        assert_eq!(live_chip_label(true, true), "▶ LOOP");
        assert_eq!(live_chip_label(false, true), "▶ LOOP");
        assert_eq!(live_chip_label(false, false), "⏸ PAUSED");
    }
}
