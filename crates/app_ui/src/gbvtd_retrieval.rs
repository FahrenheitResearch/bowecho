//! Single-Doppler tropical-cyclone wind retrieval (GBVTD) — UI panel + overlay.
//!
//! Runs [`render2d::gbvtd`] on the currently displayed radar volume: dealiases
//! the lowest velocity cut, then retrieves the storm center, radius of maximum
//! wind, and the axisymmetric tangential/radial wind profile about a
//! user-clicked center (the user clicks the eye on the radar). The retrieved
//! center + RMW ring are drawn on the radar map, with a wind-profile readout.
//!
//! Method: Lee, Jou, Chang & Deng 1999 (Mon. Wea. Rev. 127) + Lee & Marks 2000
//! (simplex center) — see `render2d::gbvtd`. As with `tropical.rs`, the overlay
//! draw is an `impl crate::ViewerApp` method so it can reach the crate-root
//! paint helpers and `self.lon_lat_to_screen`.

use eframe::egui;
use radar_core::MomentType;
use render2d::{
    PolarVelocityField, TcCirculation, dealias_velocity_grid, find_center_and_retrieve,
};

const KM_PER_DEG_LAT: f32 = 111.0;
const MS_TO_KT: f32 = 1.943_844;
const KT_TO_MS: f32 = 0.514_444;
/// Only borrow a tropical cyclone's motion vector when its center is within
/// this range of the radar — beyond it the storm isn't the one on the scope and
/// its motion is irrelevant to this retrieval (a NEXRAD sees to ~230 km).
const MAX_TC_MOTION_RANGE_KM: f32 = 400.0;

#[derive(Default)]
pub struct GbvtdState {
    pub panel_open: bool,
    /// Armed: the next left-click on the radar sets the center seed (the
    /// hurricane eye). Cleared on click. This is the ONLY sensible way to
    /// seed a single-Doppler retrieval — the center must be near the eye,
    /// and the eye is wherever the user sees it, not the screen middle.
    pub place_mode: bool,
    result: Option<TcCirculation>,
    /// User-placed center seed (lon, lat). When set, the retrieval searches
    /// around here instead of the map center; drawn as a faint marker so the
    /// click and the snapped center are both visible.
    seed_lonlat: Option<(f32, f32)>,
    /// Radar site (lon, lat) the current `result` is referenced to.
    site_lonlat: Option<(f32, f32)>,
    status: String,
}

/// Saffir–Simpson-style label from a peak tangential wind in knots.
fn intensity_label(kt: f32) -> &'static str {
    match kt {
        k if k >= 137.0 => "Category 5",
        k if k >= 113.0 => "Category 4",
        k if k >= 96.0 => "Category 3",
        k if k >= 83.0 => "Category 2",
        k if k >= 64.0 => "Category 1",
        k if k >= 34.0 => "Tropical storm",
        _ => "Tropical depression",
    }
}

impl crate::ViewerApp {
    /// Retrieve the TC circulation on the displayed volume, seeding the center
    /// search from the user-clicked eye (`seed_lonlat`), or the map center as a
    /// fallback when nothing has been placed yet.
    pub fn run_gbvtd_retrieval(&mut self) {
        self.gbvtd.result = None;
        let Some(volume) = self.volume.clone() else {
            self.gbvtd.status = "Load a radar volume first.".to_owned();
            return;
        };
        let (Some(lat0), Some(lon0)) = (volume.site.latitude_deg, volume.site.longitude_deg) else {
            self.gbvtd.status = "This volume has no radar-site location.".to_owned();
            return;
        };
        let Some(cut) = volume
            .cuts
            .iter()
            .filter(|cut| cut.moments.contains_key(&MomentType::Velocity))
            .min_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg))
        else {
            self.gbvtd.status = "No velocity data in this volume.".to_owned();
            return;
        };
        let velocity = cut
            .moments
            .get(&MomentType::Velocity)
            .expect("filtered for Velocity");
        let dealiased = dealias_velocity_grid(cut, velocity);
        let field = PolarVelocityField::from_dealiased_velocity(cut, &dealiased);

        // Seed the center search from the user's clicked eye when they
        // placed one; otherwise fall back to the map center (the old
        // behavior). Convert lon/lat -> radar-relative km (x east, y north).
        let (seed_lon, seed_lat) = self
            .gbvtd
            .seed_lonlat
            .unwrap_or((self.map_center_lon, self.map_center_lat));
        let cos_lat = lat0.to_radians().cos().max(0.01);
        let guess_x = (seed_lon - lon0) * KM_PER_DEG_LAT * cos_lat;
        let guess_y = (seed_lat - lat0) * KM_PER_DEG_LAT;
        let radii: Vec<f32> = (8..=110).step_by(4).map(|r| r as f32).collect();

        // Single-Doppler GBVTD cannot separate the environmental (storm-motion)
        // wind from the vortex; left in, it aliases and biases the retrieved VT
        // downward (e.g. a Cat-5 reads ~98 kt). Feed the nearest active TC's
        // best-track motion vector so `fit_ring` can remove it and de-alias VT
        // back toward true intensity. (0,0) => unchanged behavior.
        let (wind_ms, motion_note) = self.nearest_tc_motion_ms(lon0, lat0);

        match find_center_and_retrieve(&field, (guess_x, guess_y), 32.0, 3.0, &radii, wind_ms) {
            Some(circ) => {
                let vt = circ.vt_max.unwrap_or(0.0);
                let kt = vt * MS_TO_KT;
                let rmw = circ.rmw_km.unwrap_or(0.0);
                let motion = motion_note.map(|n| format!("  · {n}")).unwrap_or_default();
                self.gbvtd.status = format!(
                    "{} — peak {vt:.0} m/s ({kt:.0} kt) at RMW {rmw:.0} km  [{:.2}° tilt]{motion}",
                    intensity_label(kt),
                    cut.elevation_deg
                );
                self.gbvtd.result = Some(circ);
                self.gbvtd.site_lonlat = Some((lon0, lat0));
            }
            None => {
                self.gbvtd.status =
                    "No circulation found — click closer to the eye and retry.".to_owned();
            }
        }
    }

    /// Environmental (storm-motion) wind (Um east, Vm north, m/s) taken from the
    /// nearest active tropical cyclone to the radar site, plus a short note for
    /// the status line. Returns `((0,0), None)` when no storm is close enough or
    /// its motion is unknown — GBVTD then runs unchanged.
    ///
    /// `movement_dir_deg` is the heading the storm moves TOWARD (0° = N,
    /// clockwise), so the east/north components are `speed·sin θ` / `speed·cos θ`.
    fn nearest_tc_motion_ms(&self, lon0: f32, lat0: f32) -> ((f64, f64), Option<String>) {
        let cos_lat = lat0.to_radians().cos().max(0.01);
        let range_km2 = |lon: f32, lat: f32| -> f32 {
            let dx = (lon - lon0) * KM_PER_DEG_LAT * cos_lat;
            let dy = (lat - lat0) * KM_PER_DEG_LAT;
            dx * dx + dy * dy
        };
        let Some(storm) = self.tropical.storms.iter().min_by(|a, b| {
            range_km2(a.position.lon, a.position.lat)
                .total_cmp(&range_km2(b.position.lon, b.position.lat))
        }) else {
            return ((0.0, 0.0), None);
        };
        if range_km2(storm.position.lon, storm.position.lat)
            > MAX_TC_MOTION_RANGE_KM * MAX_TC_MOTION_RANGE_KM
        {
            return ((0.0, 0.0), None);
        }
        let (Some(dir), Some(speed_kt)) = (storm.movement_dir_deg, storm.movement_speed_kt) else {
            return ((0.0, 0.0), None);
        };
        let speed_ms = speed_kt * KT_TO_MS;
        let (sin_d, cos_d) = dir.to_radians().sin_cos();
        (
            ((speed_ms * sin_d) as f64, (speed_ms * cos_d) as f64),
            Some(format!(
                "de-aliased with {} motion {speed_kt:.0} kt",
                storm.name
            )),
        )
    }

    /// Arm click-to-place: the next radar click drops the center seed on the
    /// eye. Also opens the panel so the readout is visible.
    pub fn arm_gbvtd_place_mode(&mut self) {
        self.gbvtd.panel_open = true;
        self.gbvtd.place_mode = true;
        self.gbvtd.status = "Click the hurricane eye on the radar to place the center.".to_owned();
    }

    /// Place the center seed at a clicked (lon, lat) and run the retrieval.
    /// Called by the canvas when `place_mode` is armed and the user clicks.
    pub fn place_gbvtd_seed(&mut self, lon: f32, lat: f32) {
        self.gbvtd.seed_lonlat = Some((lon, lat));
        self.gbvtd.place_mode = false;
        self.run_gbvtd_retrieval();
    }

    /// Draw the retrieved center + RMW ring on the radar map.
    pub fn draw_gbvtd(&self, painter: &egui::Painter, rect: egui::Rect) {
        // Show the raw clicked seed (faint magenta dot) so the user can see
        // their click versus where the simplex snapped the center to.
        if let Some((lon, lat)) = self.gbvtd.seed_lonlat {
            let seed = self.lon_lat_to_screen(rect, lon, lat);
            painter.circle_filled(seed, 3.0, egui::Color32::from_rgb(230, 90, 230));
        }
        let (Some(circ), Some((lon0, lat0))) = (&self.gbvtd.result, self.gbvtd.site_lonlat) else {
            return;
        };
        let cos_lat = lat0.to_radians().cos().max(0.01);
        let to_screen = |x_km: f32, y_km: f32| -> egui::Pos2 {
            let lon = lon0 + x_km / (KM_PER_DEG_LAT * cos_lat);
            let lat = lat0 + y_km / KM_PER_DEG_LAT;
            self.lon_lat_to_screen(rect, lon, lat)
        };

        if let Some(rmw) = circ.rmw_km {
            let ring: Vec<egui::Pos2> = (0..=72)
                .map(|k| {
                    let a = std::f32::consts::TAU * k as f32 / 72.0;
                    to_screen(
                        circ.center_km.0 + rmw * a.cos(),
                        circ.center_km.1 + rmw * a.sin(),
                    )
                })
                .collect();
            painter.add(egui::Shape::closed_line(
                ring,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 210, 40)),
            ));
        }

        let center = to_screen(circ.center_km.0, circ.center_km.1);
        let arm = 9.0;
        let stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 60, 60));
        painter.line_segment(
            [center - egui::vec2(arm, 0.0), center + egui::vec2(arm, 0.0)],
            stroke,
        );
        painter.line_segment(
            [center - egui::vec2(0.0, arm), center + egui::vec2(0.0, arm)],
            stroke,
        );
        if let Some(vt) = circ.vt_max {
            painter.text(
                center + egui::vec2(11.0, -11.0),
                egui::Align2::LEFT_BOTTOM,
                format!("TC {:.0} kt", vt * MS_TO_KT),
                egui::FontId::proportional(12.0),
                egui::Color32::from_rgb(255, 230, 120),
            );
        }
    }

    /// GBVTD control + wind-profile readout panel.
    pub fn gbvtd_panel_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("Single-Doppler TC wind retrieval (GBVTD, Lee et al. 1999).");
        let place_button = if self.gbvtd.place_mode {
            ui.add(
                egui::Button::new("📍 Click the eye on the radar…")
                    .fill(egui::Color32::from_rgb(120, 40, 120)),
            )
        } else {
            ui.button("📍 Set center — click the eye")
        };
        place_button
            .on_hover_text(
                "The center must sit near the eye. Click where the eye is on the radar; \
                 the simplex snaps to the nearby wind maximum. Placing the seed on the \
                 radar site instead of the eye is what produces a bogus giant ring.",
            )
            .clicked()
            .then(|| self.arm_gbvtd_place_mode());
        if self.gbvtd.seed_lonlat.is_some()
            && ui
                .button("🌀 Re-run at current center")
                .on_hover_text(
                    "Recompute using the already-placed center (e.g. after stepping frames).",
                )
                .clicked()
        {
            self.run_gbvtd_retrieval();
        }
        if (self.gbvtd.result.is_some()
            || self.gbvtd.seed_lonlat.is_some()
            || self.gbvtd.place_mode)
            && ui
                .button("✖ Clear")
                .on_hover_text("Remove the TC-winds overlay (center, ring, label) and reset.")
                .clicked()
        {
            self.gbvtd.result = None;
            self.gbvtd.seed_lonlat = None;
            self.gbvtd.place_mode = false;
            self.gbvtd.status.clear();
        }
        if !self.gbvtd.status.is_empty() {
            ui.separator();
            ui.label(&self.gbvtd.status);
        }
        if let Some(circ) = &self.gbvtd.result {
            ui.separator();
            ui.label("Axisymmetric profile (VT tangential, VR radial; −VR = inflow):");
            egui::ScrollArea::vertical()
                .max_height(240.0)
                .show(ui, |ui| {
                    egui::Grid::new("gbvtd_rings").striped(true).show(ui, |ui| {
                        ui.label("r km");
                        ui.label("VT m/s");
                        ui.label("VT kt");
                        ui.label("VR m/s");
                        ui.label("n");
                        ui.end_row();
                        for ring in &circ.rings {
                            ui.label(format!("{:.0}", ring.radius_km));
                            ui.label(format!("{:.0}", ring.vt));
                            ui.label(format!("{:.0}", ring.vt * MS_TO_KT));
                            ui.label(format!("{:.0}", ring.vr));
                            ui.label(format!("{}", ring.samples));
                            ui.end_row();
                        }
                    });
                });
        }
    }
}
