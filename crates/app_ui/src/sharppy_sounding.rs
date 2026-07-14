//! SHARPpy-look sounding panel: the [`sharppyrs`] SPC sounding window
//! (skew-T, hodograph + locator, insets, index board — the exact
//! SHARPpy-Reimagined render) as the default sounding view, wrapping the
//! classic [`rw_ui::SoundingPanel`] behind a per-panel toggle.
//!
//! The wrapper mirrors the exact `SoundingPanel` surface the app already
//! uses (`set_loading` / `set_error` / `set_data` / `set_native_column` /
//! `clear` / `has_content` / view-state JSON / `ui`), feeding both views
//! from the same column so switching is lossless. Analysis (parcels,
//! effective inflow layer, every index) runs once per install, not per
//! frame.

use eframe::egui;
use rustwx_sounding::SoundingColumn;
use rw_ui::SoundingData;

const MS_TO_KT: f64 = 1.943_844_49;
const SHARPPY_CANVAS_MIN_WIDTH: f32 = 1_630.0;
const SHARPPY_CANVAS_MIN_HEIGHT: f32 = 900.0;
const SHARPPY_CANVAS_MAX_HEIGHT: f32 = 1_100.0;
const LEGACY_DEFAULT_LAYOUT_WITH_STP: &str =
    "speed,advection|hodograph|slinky,thetae,srwinds,locationmap|indexboard,streamwiseness,stp|250";

fn restored_layout_tokens(tokens: &str) -> Option<String> {
    let layout = sharppyrs::SoundingLayout::from_tokens(tokens)?;
    let canonical = layout.to_tokens();
    if canonical == LEGACY_DEFAULT_LAYOUT_WITH_STP {
        // v0.34 persisted the then-default STP bar. Treat that exact layout as
        // a default, not as a customization, so existing users receive the
        // wider index board introduced with Rusty Weather v0.4 as well.
        Some(sharppyrs::SoundingLayout::default().to_tokens())
    } else {
        Some(canonical)
    }
}

fn sounding_canvas_size(viewport: egui::Vec2) -> egui::Vec2 {
    egui::Vec2::new(
        viewport.x.max(SHARPPY_CANVAS_MIN_WIDTH),
        (viewport.y - 24.0).clamp(SHARPPY_CANVAS_MIN_HEIGHT, SHARPPY_CANVAS_MAX_HEIGHT),
    )
}

struct SharppyAnalysis {
    prof: sharppyrs::Profile,
    derived: sharppyrs::DerivedParams,
    title: String,
}

pub struct SharppySoundingPanel {
    inner: rw_ui::SoundingPanel,
    analysis: Option<Box<SharppyAnalysis>>,
    classic: bool,
    /// Last-seen SPC-window layout tokens ([`sharppyrs::SoundingLayout::to_tokens`]),
    /// mirrored out of egui memory during `ui()` so `view_state_json` (no ctx)
    /// can persist them.
    layout_tokens: Option<String>,
    /// Tokens applied from a saved view state, waiting for the next `ui()`
    /// (which has the ctx) to store them into egui memory.
    pending_layout_tokens: Option<String>,
}

impl SharppySoundingPanel {
    pub fn new() -> Self {
        Self {
            inner: rw_ui::SoundingPanel::new(),
            analysis: None,
            classic: false,
            layout_tokens: None,
            pending_layout_tokens: None,
        }
    }

    /// Stable egui-memory key for the SPC-window panel layout, pinned so the
    /// layout survives the widget moving between panes and so it can be
    /// read/written outside the widget for persistence.
    fn layout_memory_id() -> egui::Id {
        egui::Id::new("bowecho_sharppy_layout")
    }

    pub fn set_loading(&mut self) {
        self.inner.set_loading();
    }

    pub fn set_error(&mut self, message: String) {
        self.analysis = None;
        self.inner.set_error(message);
    }

    pub fn clear(&mut self) {
        self.analysis = None;
        self.inner.clear();
    }

    pub fn has_content(&self) -> bool {
        self.inner.has_content() || self.analysis.is_some()
    }

    /// The classic panel's view-state object with one added string key,
    /// `"sharppy_layout"`, carrying the SPC-window layout tokens
    /// ([`sharppyrs::SoundingLayout::to_tokens`]). Keeping the inner object's
    /// shape (rather than nesting it) preserves every key existing consumers
    /// patch directly — e.g. `model_data::patch_sounding_scene_zoom` writing
    /// `["zooms"]["scene"]` — and keeps old saves loadable as-is.
    pub fn view_state_json(&self) -> serde_json::Value {
        let mut value = self.inner.view_state_json();
        if let (Some(obj), Some(tokens)) = (value.as_object_mut(), &self.layout_tokens) {
            obj.insert(
                "sharppy_layout".to_owned(),
                serde_json::Value::String(tokens.clone()),
            );
        }
        value
    }

    /// Accepts both shapes: a plain classic-panel state (old saves) and one
    /// carrying the added `"sharppy_layout"` key (the classic panel ignores
    /// unknown keys). Layout tokens take effect on the next `ui()` frame,
    /// which has the ctx to write egui memory.
    pub fn apply_view_state_json(&mut self, value: &serde_json::Value) -> bool {
        if let Some(tokens) = value.get("sharppy_layout").and_then(|v| v.as_str())
            && let Some(tokens) = restored_layout_tokens(tokens)
        {
            self.layout_tokens = Some(tokens.clone());
            self.pending_layout_tokens = Some(tokens);
        }
        self.inner.apply_view_state_json(value)
    }

    #[allow(dead_code)] // parity with rw_ui::SoundingPanel's surface
    pub fn last_timings(&self) -> Option<(f32, f32)> {
        self.inner.last_timings()
    }

    pub fn set_data(&mut self, data: SoundingData) {
        self.analysis = rw_ui::skewt::build_sounding_column(&data)
            .ok()
            .and_then(|column| build_analysis(&data, &column));
        self.inner.set_data(data);
    }

    pub fn set_native_column(&mut self, data: SoundingData, column: SoundingColumn) {
        self.analysis = build_analysis(&data, &column);
        self.inner.set_native_column(data, column);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let layout_id = Self::layout_memory_id();
        // A layout restored from saved view state lands in egui memory here,
        // on the first frame with a ctx.
        if let Some(tokens) = self.pending_layout_tokens.take()
            && let Some(layout) = sharppyrs::SoundingLayout::from_tokens(&tokens)
        {
            sharppyrs::store_layout(ui.ctx(), layout_id, &layout);
        }
        if self.analysis.is_some() {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.classic, false, "SHARPpy");
                ui.selectable_value(&mut self.classic, true, "Classic");
            });
        }
        if !self.classic
            && let Some(analysis) = self.analysis.as_ref()
        {
            // Keep the complete SPC board in the same desktop-width coordinate
            // system as Rusty Weather v0.4. Narrow docks scroll instead of
            // squeezing fixed diagnostic columns over the skew-T.
            let size = sounding_canvas_size(ui.available_size());
            egui::ScrollArea::both()
                .id_salt("bowecho-sharppy-sounding-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::BLACK)
                        .show(ui, |ui| {
                            ui.add(
                                sharppyrs::SoundingView::new(&analysis.prof, &analysis.derived)
                                    .title(analysis.title.clone())
                                    .brand("BowEcho")
                                    .style(sharppyrs::SkewTStyle::space_grotesk())
                                    .layout_memory_id(layout_id)
                                    .size(size),
                            );
                        });
                });
        } else {
            self.inner.ui(ui);
        }
        // Mirror the (possibly gear-edited) layout back out so the ctx-less
        // `view_state_json` can persist it.
        if let Some(layout) = sharppyrs::stored_layout(ui.ctx(), layout_id) {
            self.layout_tokens = Some(layout.to_tokens());
        }
    }
}

/// Build the sharppyrs analysis from the exact column the classic panel
/// renders (store-native units: u/v m/s -> wdir/wspd kt).
fn build_analysis(data: &SoundingData, column: &SoundingColumn) -> Option<Box<SharppyAnalysis>> {
    let n = column.len();
    if n < 3 {
        return None;
    }
    let mut wdir = vec![f64::NAN; n];
    let mut wspd = vec![f64::NAN; n];
    for i in 0..n {
        let (u, v) = (column.u_ms[i], column.v_ms[i]);
        if u.is_finite() && v.is_finite() {
            let speed = (u * u + v * v).sqrt();
            let mut dir = (-u).atan2(-v).to_degrees();
            if dir < 0.0 {
                dir += 360.0;
            }
            wdir[i] = dir;
            wspd[i] = speed * MS_TO_KT;
        }
    }
    let meta = &column.metadata;
    let latitude = meta
        .latitude_deg
        .or_else(|| data.lat.map(f64::from))
        .unwrap_or(35.0);
    let station = sharppyrs::sharprs::profile::StationInfo {
        station_id: meta.station_id.clone(),
        latitude,
        longitude: meta
            .longitude_deg
            .or_else(|| data.lon.map(f64::from))
            .unwrap_or(f64::NAN),
        elevation: meta.elevation_m.unwrap_or(f64::NAN),
        datetime: meta.valid_time.clone(),
    };
    let sp = sharppyrs::sharprs::Profile::new(
        &column.pressure_hpa,
        &column.height_m_msl,
        &column.temperature_c,
        &column.dewpoint_c,
        &wdir,
        &wspd,
        &column.omega_pa_s,
        station,
    )
    .ok()?;
    let prof = sharppyrs::Profile::from_sharprs(sp);
    let derived = sharppyrs::DerivedParams::compute(&prof);

    // "HRRR 2026-06-25 06z F018  Valid: ... @36.68°N 95.66°W" style title.
    let mut title = format!(
        "{} {} F{:03}",
        data.hour.model.to_uppercase(),
        data.hour.run,
        data.hour.hour
    );
    if !meta.valid_time.is_empty() {
        title.push_str(&format!("  Valid: {}", meta.valid_time));
    }
    if let (Some(lat), Some(lon)) = (
        meta.latitude_deg.or_else(|| data.lat.map(f64::from)),
        meta.longitude_deg.or_else(|| data.lon.map(f64::from)),
    ) {
        let ns = if lat >= 0.0 { "N" } else { "S" };
        let ew = if lon >= 0.0 { "E" } else { "W" };
        title.push_str(&format!(
            "  @{:.2}\u{b0}{ns} {:.2}\u{b0}{ew}",
            lat.abs(),
            lon.abs()
        ));
    }

    Some(Box::new(SharppyAnalysis {
        prof,
        derived,
        title,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store-native column converts into a full sharppyrs analysis:
    /// winds become wdir/wspd kt, parcels lift, and the headline indices
    /// come out finite for a convective column.
    #[test]
    fn column_converts_to_analysis() {
        let pres = [
            1000.0, 925.0, 850.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0,
        ];
        let hght = [
            110.0, 780.0, 1500.0, 3100.0, 5800.0, 7500.0, 9600.0, 10900.0, 12300.0, 14100.0,
        ];
        let tmpc = [
            27.0, 22.0, 17.5, 8.0, -8.5, -20.0, -36.0, -46.0, -55.0, -60.0,
        ];
        let dwpc = [
            22.0, 19.0, 15.0, 4.0, -15.0, -30.0, -48.0, -58.0, -68.0, -75.0,
        ];
        let u = [2.0, 6.0, 9.0, 13.0, 18.0, 22.0, 27.0, 30.0, 32.0, 30.0];
        let v = [8.0, 10.0, 11.0, 12.0, 14.0, 15.0, 16.0, 16.0, 15.0, 14.0];
        let column = SoundingColumn {
            pressure_hpa: pres.to_vec(),
            height_m_msl: hght.to_vec(),
            temperature_c: tmpc.to_vec(),
            dewpoint_c: dwpc.to_vec(),
            u_ms: u.to_vec(),
            v_ms: v.to_vec(),
            omega_pa_s: vec![f64::NAN; pres.len()],
            metadata: rustwx_sounding::SoundingMetadata {
                station_id: "TEST".to_owned(),
                valid_time: "2026-06-26 00z".to_owned(),
                latitude_deg: Some(36.7),
                longitude_deg: Some(-95.7),
                elevation_m: Some(110.0),
                sample_method: None,
                box_radius_lat_deg: None,
                box_radius_lon_deg: None,
            },
        };
        let data = SoundingData {
            hour: rw_ui::HourKey {
                model: "hrrr".to_owned(),
                run: "2026-06-25 06z".to_owned(),
                hour: 18,
                exact_time: None,
            },
            fx: 0.0,
            fy: 0.0,
            lat: Some(36.7),
            lon: Some(-95.7),
            vars: Vec::new(),
            surface: Vec::new(),
            read_ms: 0.0,
        };
        let analysis = build_analysis(&data, &column).expect("analysis builds");
        assert!(
            analysis.prof.mupcl.bplus > 0.0,
            "convective column has CAPE"
        );
        assert!(analysis.derived.pwat.is_finite(), "PWAT computes");
        assert!(analysis.derived.srh1km.is_finite(), "SRH computes");
        assert!(analysis.title.starts_with("HRRR 2026-06-25 06z F018"));
    }

    /// Old saves (plain classic-panel state, no `sharppy_layout` key) still
    /// apply, and the emitted view state keeps the classic keys the app
    /// patches directly (`["zooms"]["scene"]`).
    #[test]
    fn view_state_stays_compatible_with_old_saves() {
        let mut panel = SharppySoundingPanel::new();
        let mut old_save = panel.inner.view_state_json();
        crate::model_data::patch_sounding_scene_zoom(&mut old_save, 1.25);
        assert!(panel.apply_view_state_json(&old_save));
        let back = panel.view_state_json();
        assert!((back["zooms"]["scene"].as_f64().unwrap() - 1.25).abs() < 1e-6);
        assert!(
            back.get("sharppy_layout").is_none(),
            "no layout seen yet, none emitted"
        );
        // The augmented shape must still be valid input for the classic panel.
        assert!(panel.inner.apply_view_state_json(&back));
    }

    /// The SPC-window layout tokens ride along in the view-state JSON and
    /// survive an apply -> emit round trip, without disturbing zoom patching.
    #[test]
    fn layout_tokens_round_trip_through_view_state() {
        let tokens =
            "hidden,advection|slinky|speed,thetae,srwinds,hazardtype|indexboard,ship,stp|180";
        let mut panel = SharppySoundingPanel::new();
        let mut save = panel.inner.view_state_json();
        save.as_object_mut()
            .unwrap()
            .insert("sharppy_layout".to_owned(), serde_json::json!(tokens));
        assert!(panel.apply_view_state_json(&save));
        let mut emitted = panel.view_state_json();
        assert_eq!(emitted["sharppy_layout"].as_str(), Some(tokens));
        crate::model_data::patch_sounding_scene_zoom(&mut emitted, 1.4);
        assert_eq!(emitted["sharppy_layout"].as_str(), Some(tokens));
        assert!((emitted["zooms"]["scene"].as_f64().unwrap() - 1.4).abs() < 1e-6);
        // Malformed tokens are dropped rather than persisted or applied.
        let mut bad = panel.inner.view_state_json();
        bad.as_object_mut()
            .unwrap()
            .insert("sharppy_layout".to_owned(), serde_json::json!("gibberish"));
        let mut fresh = SharppySoundingPanel::new();
        assert!(fresh.apply_view_state_json(&bad));
        assert!(fresh.view_state_json().get("sharppy_layout").is_none());
    }

    #[test]
    fn legacy_default_stp_layout_migrates_to_wider_index_board() {
        let mut panel = SharppySoundingPanel::new();
        let mut save = panel.inner.view_state_json();
        save.as_object_mut().unwrap().insert(
            "sharppy_layout".to_owned(),
            serde_json::json!(LEGACY_DEFAULT_LAYOUT_WITH_STP),
        );
        assert!(panel.apply_view_state_json(&save));

        let state = panel.view_state_json();
        let migrated = state["sharppy_layout"]
            .as_str()
            .expect("migrated layout tokens");
        assert_eq!(migrated, sharppyrs::SoundingLayout::default().to_tokens());
        assert!(migrated.contains("indexboard,streamwiseness,hidden"));
    }

    #[test]
    fn sounding_canvas_keeps_desktop_board_geometry_and_scrolls_small_hosts() {
        assert_eq!(
            sounding_canvas_size(egui::vec2(1_200.0, 700.0)),
            egui::vec2(1_630.0, 900.0)
        );
        assert_eq!(
            sounding_canvas_size(egui::vec2(1_800.0, 1_200.0)),
            egui::vec2(1_800.0, 1_100.0)
        );
    }

    /// A restored layout lands in egui memory on the next `ui()` frame under
    /// the pinned id, and `ui()` mirrors the in-memory layout back into the
    /// tokens the ctx-less `view_state_json` emits.
    #[test]
    fn pending_layout_lands_in_egui_memory_on_ui() {
        let tokens =
            "speed,advection|hodograph|slinky,thetae,srwinds,hazardtype|indexboard,ship,stp|300";
        let mut panel = SharppySoundingPanel::new();
        let mut save = panel.inner.view_state_json();
        save.as_object_mut()
            .unwrap()
            .insert("sharppy_layout".to_owned(), serde_json::json!(tokens));
        assert!(panel.apply_view_state_json(&save));

        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| panel.ui(ui));
        let stored = sharppyrs::stored_layout(&ctx, SharppySoundingPanel::layout_memory_id())
            .expect("layout stored under the pinned id");
        assert_eq!(stored.to_tokens(), tokens);
        assert_eq!(
            panel.view_state_json()["sharppy_layout"].as_str(),
            Some(tokens)
        );
    }
}
