//! Native RHI (range-height) display panel for mobile-radar sweeps.
//!
//! DOW/COW/RaXPol crews scan RHIs constantly; those sweeps hold a fixed
//! azimuth and sweep elevation, so the map's plan-view renderer shows them
//! as a single spoke. When the displayed volume declares an RHI scan mode
//! (DORADE RADD `scan_mode`, surfaced as [`radar_core::ScanMode`]) — or its
//! geometry looks like one — the app shows this bottom panel instead:
//! x = ground range from the radar, y = height above the radar, resampled by
//! [`render2d::rhi_section`] under the 4/3-Earth beam model (Doviak & Zrnić
//! 1993, eq. 2.28b/c) and colored with the app's moment color tables.

use std::sync::Arc;

use color_tables::ColorTableSet;
use eframe::egui;
use radar_core::{MomentType, RadarVolume, ScanMode};

/// `true` when the volume should be displayed as an RHI. The source's
/// declared scan mode wins; volumes without one (e.g. GR2-converted feeds)
/// fall back to the geometric heuristic on the first cut.
pub fn volume_is_rhi(volume: &RadarVolume) -> bool {
    match volume.metadata.scan_mode {
        Some(ScanMode::Rhi) => true,
        Some(_) => false,
        None => volume
            .cuts
            .first()
            .is_some_and(render2d::cut_looks_like_rhi),
    }
}

/// Range-height panel state: cached texture + recompute signature, mirroring
/// the cross-section panel's pattern.
pub struct RhiPanel {
    texture: Option<egui::TextureHandle>,
    signature: Option<u64>,
    status: String,
    top_m: f32,
    range_m: f32,
}

const RHI_TEXTURE_WIDTH: usize = 768;
const RHI_TEXTURE_HEIGHT: usize = 320;

impl RhiPanel {
    pub fn new() -> Self {
        Self {
            texture: None,
            signature: None,
            status: String::new(),
            top_m: 0.0,
            range_m: 0.0,
        }
    }

    /// Render the panel: header (azimuth, moment, extents) plus the
    /// range-height image with height/range axis labels.
    pub fn panel(
        &mut self,
        ui: &mut egui::Ui,
        volume: &Arc<RadarVolume>,
        selected_cut: usize,
        product_moment: &MomentType,
        color_tables: &ColorTableSet,
    ) {
        self.update_texture(ui.ctx(), volume, selected_cut, product_moment, color_tables);
        ui.horizontal(|ui| {
            ui.strong("RHI");
            ui.separator();
            ui.label(&self.status);
        });
        let avail = ui.available_size();
        if avail.y < 24.0 {
            return;
        }
        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(12, 14, 18));
        let Some(texture) = &self.texture else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "No data in this RHI sweep",
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(170, 178, 188),
            );
            return;
        };
        // Left gutter for height labels + bottom strip for range labels.
        let plot = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 38.0, rect.top() + 2.0),
            egui::pos2(rect.right() - 4.0, rect.bottom() - 16.0),
        );
        painter.image(
            texture.id(),
            plot,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        let label = egui::Color32::from_rgb(190, 196, 204);
        let font = egui::FontId::proportional(10.0);
        let top_km = self.top_m / 1000.0;
        for k in 0..=3 {
            let frac = k as f32 / 3.0;
            let y = plot.top() + plot.height() * frac;
            painter.text(
                egui::pos2(plot.left() - 4.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{:.1}", top_km * (1.0 - frac)),
                font.clone(),
                label,
            );
        }
        painter.text(
            egui::pos2(rect.left() + 2.0, plot.top()),
            egui::Align2::LEFT_TOP,
            "km",
            font.clone(),
            label,
        );
        let range_km = self.range_m / 1000.0;
        for k in 0..=4 {
            let frac = k as f32 / 4.0;
            let x = plot.left() + plot.width() * frac;
            let align = if k == 0 {
                egui::Align2::LEFT_BOTTOM
            } else if k == 4 {
                egui::Align2::RIGHT_BOTTOM
            } else {
                egui::Align2::CENTER_BOTTOM
            };
            painter.text(
                egui::pos2(x, rect.bottom() - 2.0),
                align,
                format!("{:.0}", range_km * frac),
                font.clone(),
                label,
            );
        }
        painter.text(
            egui::pos2(plot.center().x, rect.bottom() - 13.0),
            egui::Align2::CENTER_BOTTOM,
            "ground range from radar (km)",
            font,
            label,
        );
    }

    fn update_texture(
        &mut self,
        ctx: &egui::Context,
        volume: &Arc<RadarVolume>,
        selected_cut: usize,
        product_moment: &MomentType,
        color_tables: &ColorTableSet,
    ) {
        let Some(cut) = volume
            .cuts
            .get(selected_cut)
            .or_else(|| volume.cuts.first())
        else {
            self.texture = None;
            self.status = "No sweeps in volume".to_owned();
            return;
        };
        // The selected product's base moment when the sweep recorded it,
        // else reflectivity, else whatever the sweep has.
        let moment = if cut.moments.contains_key(product_moment) {
            product_moment.clone()
        } else if cut.moments.contains_key(&MomentType::Reflectivity) {
            MomentType::Reflectivity
        } else {
            match cut.moments.keys().next() {
                Some(moment) => moment.clone(),
                None => {
                    self.texture = None;
                    self.status = "Sweep has no moments".to_owned();
                    return;
                }
            }
        };
        let family = render2d::color_family_for_moment(&moment);

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (Arc::as_ptr(volume) as usize).hash(&mut hasher);
        selected_cut.hash(&mut hasher);
        moment.short_name().hash(&mut hasher);
        color_tables.signature_for_family(family).hash(&mut hasher);
        let signature = hasher.finish();
        if self.signature == Some(signature) && self.texture.is_some() {
            return;
        }

        let Some(grid) = cut.moments.get(&moment) else {
            return; // unreachable: moment chosen from cut.moments above
        };
        // Velocity panels show dealiased velocity, matching the map and
        // cross-section displays.
        let dealiased;
        let grid = if moment == MomentType::Velocity {
            dealiased = render2d::dealias_velocity_grid(cut, grid);
            &dealiased
        } else {
            grid
        };

        let coverage_top = render2d::rhi_coverage_top_m(cut, grid);
        let top_m = (coverage_top * 1.05).clamp(2_000.0, 20_000.0);
        let range_m = render2d::rhi_coverage_range_m(cut, grid).clamp(2_000.0, 300_000.0);
        let section = render2d::rhi_section(
            cut,
            grid,
            RHI_TEXTURE_WIDTH,
            RHI_TEXTURE_HEIGHT,
            top_m,
            range_m,
        );
        self.signature = Some(signature);
        let Some(section) = section else {
            self.texture = None;
            self.status = "No data in this RHI sweep".to_owned();
            return;
        };

        let table = color_tables.for_family(family);
        let mut rgba = vec![0u8; section.width * section.height * 4];
        for (cell, value) in rgba.chunks_exact_mut(4).zip(section.values.iter()) {
            if value.is_finite() {
                cell.copy_from_slice(&table.color_for_value(*value));
            }
        }
        // Unmultiplied: palette stops may carry partial alpha (see the
        // cross-section panel note).
        let image =
            egui::ColorImage::from_rgba_unmultiplied([section.width, section.height], &rgba);
        match &mut self.texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("rhi-panel", image, egui::TextureOptions::LINEAR));
            }
        }
        self.top_m = top_m;
        self.range_m = range_m;
        let azimuth = render2d::rhi_fixed_azimuth_deg(cut);
        let (elev_min, elev_max) =
            cut.radials
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), radial| {
                    (
                        low.min(radial.elevation_deg),
                        high.max(radial.elevation_deg),
                    )
                });
        self.status = format!(
            "{} · az {azimuth:.1}° · elev {elev_min:.1}–{elev_max:.1}° · {:.0} km × {:.1} km",
            moment.short_name(),
            range_m / 1000.0,
            top_m / 1000.0,
        );
    }
}

impl Default for RhiPanel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, RadarSite, Radial};

    fn rhi_volume(scan_mode: Option<ScanMode>) -> RadarVolume {
        let mut volume = RadarVolume::new(
            RadarSite::new("DOW7"),
            chrono::DateTime::from_timestamp(1_779_404_114, 0).unwrap(),
        );
        volume.metadata.scan_mode = scan_mode;
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 150,
            gate_count: 8,
        };
        let cut = volume.push_cut(271.0, Some(1));
        for k in 0..40 {
            cut.radials.push(Radial {
                azimuth_deg: 271.0,
                elevation_deg: 0.5 + k as f32 * 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        volume
    }

    #[test]
    fn declared_scan_mode_wins_over_heuristic() {
        assert!(volume_is_rhi(&rhi_volume(Some(ScanMode::Rhi))));
        // Declared PPI suppresses the heuristic even with RHI-shaped rays.
        assert!(!volume_is_rhi(&rhi_volume(Some(ScanMode::Ppi))));
        // No declaration: geometry decides.
        assert!(volume_is_rhi(&rhi_volume(None)));
    }

    #[test]
    fn ppi_volume_is_not_rhi() {
        let mut volume = RadarVolume::new(
            RadarSite::new("KEAX"),
            chrono::DateTime::from_timestamp(1_779_404_114, 0).unwrap(),
        );
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 250,
            gate_count: 8,
        };
        let cut = volume.push_cut(0.5, Some(1));
        for k in 0..360 {
            cut.radials.push(Radial {
                azimuth_deg: k as f32,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        assert!(!volume_is_rhi(&volume));
    }
}
