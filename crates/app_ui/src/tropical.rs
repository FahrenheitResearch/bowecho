//! Tropical-cyclone map layer.
//!
//! Polls active storms worldwide (NHC + GDACS, via `data_source::tropical`) on
//! an interval into a [`TropicalState`], progressively fetches each storm's
//! track/cone geometry, renders the storm cards, and draws the map overlay
//! (position glyph + forecast track + cone of uncertainty).
//!
//! The background fetches use the app's [`WorkerSlot`] idiom (one job in
//! flight, drained every frame). The overlay draw is an `impl crate::ViewerApp`
//! method here — a sibling module can reach the crate-root paint helpers
//! (`crate::push_solid_open_line`, …) and `self.lon_lat_to_screen`, exactly
//! like `tor_tracks.rs` does.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use data_source::tropical::{self, Category, StormGeometry, TropicalCyclone};
use eframe::egui;
use ui_core::worker_slot::{SlotPoll, WorkerSlot};

/// Re-poll cadence. NHC advisories update ~every 3–6 h (with intermediate
/// position updates) and GDACS a few times a day; 10 min keeps the card fresh
/// without hammering the sources.
pub const TROPICAL_REFRESH_SECONDS: u64 = 600;

type StormsResult = std::result::Result<Vec<TropicalCyclone>, String>;
type GeometryResult = std::result::Result<(String, StormGeometry), String>;

/// All state for the tropical layer, owned by `ViewerApp.tropical`.
pub struct TropicalState {
    storms_rx: WorkerSlot<StormsResult>,
    geometry_rx: WorkerSlot<GeometryResult>,
    /// Active storms, strongest first (the merge sorts them).
    pub storms: Vec<TropicalCyclone>,
    /// Per-storm track/cone, keyed by storm id, filled by the 2nd fetch.
    pub geometry: HashMap<String, StormGeometry>,
    /// Short human status for the panel header.
    pub status: String,
    last_refresh: Option<Instant>,
    /// A card asked the map to recenter here (lon, lat); ViewerApp drains it.
    pub focus_request: Option<(f32, f32)>,
}

impl Default for TropicalState {
    fn default() -> Self {
        Self {
            storms_rx: WorkerSlot::idle("tropical-cyclones"),
            geometry_rx: WorkerSlot::idle("tropical-geometry"),
            storms: Vec::new(),
            geometry: HashMap::new(),
            status: String::new(),
            last_refresh: None,
            focus_request: None,
        }
    }
}

impl TropicalState {
    /// Kick a refresh if it's due and none is in flight; heartbeat so the
    /// interval keeps ticking on an otherwise-idle map.
    pub fn maybe_refresh(&mut self, ctx: &egui::Context) {
        let due = self
            .last_refresh
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(TROPICAL_REFRESH_SECONDS));
        if due && !self.storms_rx.in_flight() {
            self.last_refresh = Some(Instant::now());
            self.storms_rx.spawn(ctx, |tx| {
                let result = tropical_http_client()
                    .and_then(|client| tropical::fetch_active_cyclones(&client));
                let _ = tx.send(result);
            });
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    /// Drain finished fetches. Must run every frame or a delivered result never
    /// clears the slot.
    pub fn poll(&mut self) {
        match self.storms_rx.poll() {
            SlotPoll::Ready(Ok(storms)) => {
                self.status = match storms.len() {
                    0 => "No active tropical cyclones".to_owned(),
                    1 => "1 active tropical cyclone".to_owned(),
                    n => format!("{n} active tropical cyclones"),
                };
                // Drop geometry for storms that are gone.
                self.geometry
                    .retain(|id, _| storms.iter().any(|s| &s.id == id));
                self.storms = storms;
            }
            SlotPoll::Ready(Err(err)) => self.status = format!("Tropical fetch failed: {err}"),
            SlotPoll::Idle | SlotPoll::Pending | SlotPoll::Disconnected => {}
        }
        if let SlotPoll::Ready(Ok((id, geom))) = self.geometry_rx.poll() {
            self.geometry.insert(id, geom);
        }
    }

    /// Progressively fetch missing track/cone geometry, one storm at a time.
    /// Call each frame while the layer is visible.
    pub fn drive_geometry(&mut self, ctx: &egui::Context) {
        if self.geometry_rx.in_flight() {
            return;
        }
        let next = self
            .storms
            .iter()
            .find(|storm| storm.geometry_url.is_some() && !self.geometry.contains_key(&storm.id));
        if let Some(storm) = next {
            let id = storm.id.clone();
            let url = storm.geometry_url.clone().expect("checked is_some");
            self.geometry_rx.spawn(ctx, move |tx| {
                let result = tropical_http_client()
                    .and_then(|client| tropical::fetch_storm_geometry(&client, &url))
                    .map(|geom| (id, geom));
                let _ = tx.send(result);
            });
        }
    }

    /// The storm-cards panel body (rendered into a window/panel by ViewerApp).
    pub fn cards_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new(&self.status).weak());
        if self.storms.is_empty() {
            ui.add_space(4.0);
            ui.label("Quiet across every basin right now.");
            return;
        }
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .max_height(520.0)
            .show(ui, |ui| {
                let storms = std::mem::take(&mut self.storms);
                for storm in &storms {
                    self.storm_card(ui, storm);
                    ui.add_space(6.0);
                }
                self.storms = storms;
            });
    }

    fn storm_card(&mut self, ui: &mut egui::Ui, storm: &TropicalCyclone) {
        let color = category_color(storm.category);
        egui::Frame::group(ui.style())
            .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&storm.name)
                            .strong()
                            .size(16.0)
                            .color(color),
                    );
                    if let Some(level) = &storm.alert_level {
                        ui.label(alert_badge(level));
                    }
                });
                ui.label(egui::RichText::new(&storm.classification).color(color));

                if let Some(wind) = storm.wind_summary() {
                    vital(ui, "Wind", &wind);
                }
                if let Some(pressure) = storm.pressure_summary() {
                    vital(ui, "Pressure", &pressure);
                }
                if let Some(motion) = storm.motion_summary() {
                    vital(ui, "Moving", &motion);
                }
                if let Some(areas) = &storm.affected_areas {
                    vital(ui, "Threatens", areas);
                }
                vital(
                    ui,
                    "Position",
                    &format!(
                        "{:.1}°{}, {:.1}°{}",
                        storm.position.lat.abs(),
                        if storm.position.lat >= 0.0 { "N" } else { "S" },
                        storm.position.lon.abs(),
                        if storm.position.lon >= 0.0 { "E" } else { "W" },
                    ),
                );

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(storm.source.label()).small().weak());
                    if let Some(time) = storm.advisory_time {
                        let mins = (Utc::now() - time).num_minutes().max(0);
                        ui.label(
                            egui::RichText::new(format!("· updated {}", ago(mins)))
                                .small()
                                .weak(),
                        );
                    }
                });

                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("📍 Focus").clicked() {
                        self.focus_request = Some((storm.position.lon, storm.position.lat));
                    }
                    for (label, url) in external_links(storm) {
                        if ui.small_button(label).clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                        }
                    }
                });
            });
    }
}

fn vital(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{label}: ")).weak());
        ui.label(value);
    });
}

fn ago(mins: i64) -> String {
    if mins < 60 {
        format!("{mins} min ago")
    } else if mins < 60 * 24 {
        format!("{} h ago", mins / 60)
    } else {
        format!("{} d ago", mins / (60 * 24))
    }
}

/// External plot providers to open in the browser (credited, never scraped).
fn external_links(storm: &TropicalCyclone) -> Vec<(&'static str, String)> {
    let mut links = Vec::new();
    if let Some(report) = &storm.report_url {
        links.push(("📄 Advisory", report.clone()));
    }
    // Zoom Earth deep-links to a live-satellite view centered on the storm.
    links.push((
        "🛰 Zoom Earth",
        format!(
            "https://zoom.earth/#view={:.1},{:.1},6z",
            storm.position.lat, storm.position.lon
        ),
    ));
    links.push((
        "📈 Tropical Tidbits",
        "https://www.tropicaltidbits.com/storms/".to_owned(),
    ));
    links.push(("🌀 CyclonicWx", "https://www.cyclonicwx.com/".to_owned()));
    links
}

fn alert_badge(level: &str) -> egui::RichText {
    let color = match level.to_ascii_lowercase().as_str() {
        "red" => egui::Color32::from_rgb(232, 66, 66),
        "orange" => egui::Color32::from_rgb(240, 150, 40),
        "green" => egui::Color32::from_rgb(70, 190, 110),
        _ => egui::Color32::GRAY,
    };
    egui::RichText::new(format!(" {} ", level.to_uppercase()))
        .small()
        .strong()
        .background_color(color.gamma_multiply(0.35))
        .color(color)
}

/// Tropical intensity color (Saffir–Simpson-ish ramp), used by the cards and
/// the position glyph.
pub fn category_color(category: Option<Category>) -> egui::Color32 {
    match category {
        Some(Category::Five) => egui::Color32::from_rgb(255, 110, 245),
        Some(Category::Four) => egui::Color32::from_rgb(232, 66, 66),
        Some(Category::Three) => egui::Color32::from_rgb(245, 130, 50),
        Some(Category::Two) => egui::Color32::from_rgb(245, 200, 60),
        Some(Category::One) => egui::Color32::from_rgb(240, 240, 90),
        Some(Category::TropicalStorm) => egui::Color32::from_rgb(90, 210, 120),
        Some(Category::TropicalDepression) | None => egui::Color32::from_rgb(120, 190, 235),
    }
}

fn tropical_http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("BowEcho tropical layer (github.com/FahrenheitResearch/bowecho)")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|err| err.to_string())
}

impl crate::ViewerApp {
    /// Draw the tropical overlay: cone + forecast track (from cached geometry)
    /// under the current-position intensity glyph. Both map paint sites call
    /// this (single-pane and per-cell).
    pub(crate) fn draw_tropical(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.app_settings.show_tropical || self.tropical.storms.is_empty() {
            return;
        }
        // A legitimate track/cone can span the viewport; cull only segments
        // longer than that (AEQD antimeridian teleports for W-Pacific storms).
        let jump_px = rect.width().max(rect.height());

        let mut shapes: Vec<egui::Shape> = Vec::new();
        for storm in &self.tropical.storms {
            let Some(geom) = self.tropical.geometry.get(&storm.id) else {
                continue;
            };
            if geom.cone.len() >= 3 {
                let ring: Vec<egui::Pos2> = geom
                    .cone
                    .iter()
                    .map(|p| self.lon_lat_to_screen(rect, p.lon, p.lat))
                    .collect();
                if !crate::screen_polyline_has_jump(&ring, true, rect, jump_px)
                    && let Some(mesh) = crate::filled_polygon_mesh(
                        &ring,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 26),
                    )
                {
                    shapes.push(egui::Shape::mesh(mesh));
                }
                crate::push_solid_closed_line(
                    &mut shapes,
                    &ring,
                    egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 150),
                    ),
                    rect,
                    jump_px,
                );
            }
            if geom.track.len() >= 2 {
                let line: Vec<egui::Pos2> = geom
                    .track
                    .iter()
                    .map(|p| self.lon_lat_to_screen(rect, p.lon, p.lat))
                    .collect();
                crate::push_solid_open_line(
                    &mut shapes,
                    &line,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(250, 250, 250)),
                    rect,
                    jump_px,
                );
            }
        }
        painter.extend(shapes);

        for storm in &self.tropical.storms {
            let pos = self.lon_lat_to_screen(rect, storm.position.lon, storm.position.lat);
            if !rect.expand(60.0).contains(pos) {
                continue;
            }
            let color = category_color(storm.category);
            painter.circle_filled(pos, 6.0, color);
            painter.circle_stroke(pos, 6.0, egui::Stroke::new(1.5, egui::Color32::BLACK));
            crate::draw_halo_text(
                painter,
                pos + egui::vec2(10.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &storm.name,
                egui::FontId::proportional(12.0),
                color,
                egui::Color32::BLACK,
            );
        }
    }
}
