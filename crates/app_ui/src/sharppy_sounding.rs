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

struct SharppyAnalysis {
    prof: sharppyrs::Profile,
    derived: sharppyrs::DerivedParams,
    title: String,
}

pub struct SharppySoundingPanel {
    inner: rw_ui::SoundingPanel,
    analysis: Option<Box<SharppyAnalysis>>,
    classic: bool,
}

impl SharppySoundingPanel {
    pub fn new() -> Self {
        Self {
            inner: rw_ui::SoundingPanel::new(),
            analysis: None,
            classic: false,
        }
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

    pub fn view_state_json(&self) -> serde_json::Value {
        self.inner.view_state_json()
    }

    pub fn apply_view_state_json(&mut self, value: &serde_json::Value) -> bool {
        self.inner.apply_view_state_json(value)
    }

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
        if self.analysis.is_some() {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.classic, false, "SHARPpy");
                ui.selectable_value(&mut self.classic, true, "Classic");
            });
        }
        if self.classic || self.analysis.is_none() {
            self.inner.ui(ui);
            return;
        }
        let analysis = self.analysis.as_ref().expect("checked above");
        // The SPC window is composed for a roughly 3:2 canvas; keep the
        // aspect sane inside arbitrary panes and let it scale with the pane.
        let avail = ui.available_size();
        let size = egui::Vec2::new(avail.x.max(480.0), avail.y.max(360.0));
        egui::Frame::new()
            .fill(egui::Color32::BLACK)
            .show(ui, |ui| {
                ui.add(
                    sharppyrs::SoundingView::new(&analysis.prof, &analysis.derived)
                        .title(analysis.title.clone())
                        .brand("BowEcho")
                        .style(sharppyrs::SkewTStyle::space_grotesk())
                        .size(size),
                );
            });
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
        let pres = [1000.0, 925.0, 850.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0];
        let hght = [110.0, 780.0, 1500.0, 3100.0, 5800.0, 7500.0, 9600.0, 10900.0, 12300.0, 14100.0];
        let tmpc = [27.0, 22.0, 17.5, 8.0, -8.5, -20.0, -36.0, -46.0, -55.0, -60.0];
        let dwpc = [22.0, 19.0, 15.0, 4.0, -15.0, -30.0, -48.0, -58.0, -68.0, -75.0];
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
        assert!(analysis.prof.mupcl.bplus > 0.0, "convective column has CAPE");
        assert!(analysis.derived.pwat.is_finite(), "PWAT computes");
        assert!(analysis.derived.srh1km.is_finite(), "SRH computes");
        assert!(analysis.title.starts_with("HRRR 2026-06-25 06z F018"));
    }
}
