// SPDX-License-Identifier: Apache-2.0
//
// Pattern (NWP store field as a warped map layer with a cached raster)
// from BowEcho crates/app_ui/src/model_layer.rs @ 6dfcb9f; the science
// read path is wrf-core (FahrenheitResearch/wrf-rust @ 9874474d, the
// exact rev BowEcho's bowecho_cli pins) — never a parallel science stack.

//! Composite reflectivity (and later, any catalog product) rendered on
//! the map as committed wrfout frames arrive, plus the real-number hover
//! lookup. The VALUES and the cell positions both come from the wrfout
//! file itself (`maxdbz`, `lat`, `lon` through wrf-core's getvar);
//! Studio computes no science.

use std::path::{Path, PathBuf};

use eframe::egui;
use ui_core::worker_slot::{SlotPoll, WorkerSlot};

/// One loaded field: values + geolocation + a colorized raster.
pub struct LoadedField {
    pub path: PathBuf,
    pub name: String,
    pub units: String,
    /// Engine-provided description (product picker hover, end-game §2.9).
    #[allow(dead_code)]
    pub description: String,
    pub nx: usize,
    pub ny: usize,
    /// Row-major `[ny][nx]`, NaN = missing.
    pub values: Vec<f32>,
    /// Cell-center latitude/longitude, row-major `[ny][nx]`.
    pub lat: Vec<f32>,
    pub lon: Vec<f32>,
    /// Colorized raster (one texel per cell).
    pub image: egui::ColorImage,
}

impl LoadedField {
    /// Nearest-cell value for the hover readout: exact nearest cell by
    /// great-circle distance over a local search seeded from the file's
    /// own lat/lon grid (works for any grid the engine ran).
    pub fn value_at(&self, lat: f32, lon: f32) -> Option<f32> {
        if self.values.is_empty() {
            return None;
        }
        // Coarse scan (every 8th cell), then refine around the winner.
        let mut best_index = 0usize;
        let mut best_distance = f32::INFINITY;
        fn consider(
            field: &LoadedField,
            lat: f32,
            lon: f32,
            index: usize,
            best_distance: &mut f32,
            best_index: &mut usize,
        ) {
            let distance =
                arwen_map::geo::haversine_km(lat, lon, field.lat[index], field.lon[index]);
            if distance < *best_distance {
                *best_distance = distance;
                *best_index = index;
            }
        }
        let step = 8usize;
        for j in (0..self.ny).step_by(step) {
            for i in (0..self.nx).step_by(step) {
                consider(
                    self,
                    lat,
                    lon,
                    j * self.nx + i,
                    &mut best_distance,
                    &mut best_index,
                );
            }
        }
        let (bi, bj) = (best_index % self.nx, best_index / self.nx);
        for j in bj.saturating_sub(step)..(bj + step + 1).min(self.ny) {
            for i in bi.saturating_sub(step)..(bi + step + 1).min(self.nx) {
                consider(
                    self,
                    lat,
                    lon,
                    j * self.nx + i,
                    &mut best_distance,
                    &mut best_index,
                );
            }
        }
        // Off-grid guard: the nearest cell must be within ~one cell of
        // the cursor (cell spacing estimated from neighbors).
        let spacing = if self.nx > 1 {
            arwen_map::geo::haversine_km(
                self.lat[0],
                self.lon[0],
                self.lat[1.min(self.values.len() - 1)],
                self.lon[1.min(self.values.len() - 1)],
            )
        } else {
            1.0
        };
        if best_distance > spacing.max(0.5) * 1.5 {
            return None;
        }
        let value = self.values[best_index];
        value.is_finite().then_some(value)
    }
}

/// Read one wrfout frame through wrf-core: composite reflectivity plus
/// the file's own cell latitudes/longitudes, colorized through the
/// shared color_tables reflectivity table.
pub fn load_composite_reflectivity(path: &Path) -> Result<LoadedField, String> {
    let file = wrf_core::WrfFile::open(path)
        .map_err(|error| format!("open {}: {error:?}", path.display()))?;
    let opts = wrf_core::ComputeOpts::default();
    let field = wrf_core::getvar(&file, "maxdbz", Some(0), &opts)
        .map_err(|error| format!("maxdbz: {error:?}"))?;
    let lat = wrf_core::getvar(&file, "lat", Some(0), &opts)
        .map_err(|error| format!("lat: {error:?}"))?;
    let lon = wrf_core::getvar(&file, "lon", Some(0), &opts)
        .map_err(|error| format!("lon: {error:?}"))?;
    let [ny, nx] = field.shape[..] else {
        return Err(format!("maxdbz shape {:?} is not 2-D", field.shape));
    };
    if lat.data.len() != ny * nx || lon.data.len() != ny * nx {
        return Err("lat/lon grids do not match the field grid".into());
    }
    let values: Vec<f32> = field.data.iter().map(|&value| value as f32).collect();

    // Colorize with the shared reflectivity table; below 5 dBZ is
    // transparent (standard radar display floor).
    let table = color_tables::builtin_reflectivity_table().with_display_threshold(Some(5.0), false);
    let mut pixels = Vec::with_capacity(ny * nx);
    for j in 0..ny {
        // egui images are top-row-first; WRF grids are south-row-first.
        let source_row = ny - 1 - j;
        for i in 0..nx {
            let value = values[source_row * nx + i];
            let color = if value.is_finite() {
                let rgba = table.color_for_value(value);
                egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3])
            } else {
                egui::Color32::TRANSPARENT
            };
            pixels.push(color);
        }
    }
    let image = egui::ColorImage {
        size: [nx, ny],
        source_size: egui::vec2(nx as f32, ny as f32),
        pixels,
    };
    Ok(LoadedField {
        path: path.to_path_buf(),
        name: "maxdbz".into(),
        units: field.units,
        description: field.description,
        nx,
        ny,
        values,
        lat: lat.data.iter().map(|&value| value as f32).collect(),
        lon: lon.data.iter().map(|&value| value as f32).collect(),
        image,
    })
}

/// The app-side layer state: what is wanted, what is loaded, what failed.
pub struct ModelLayer {
    slot: WorkerSlot<Result<LoadedField, String>>,
    pub loaded: Option<(LoadedField, egui::TextureHandle)>,
    /// The frame Studio wants on screen but could not load (missing file
    /// during fixture replays, unreadable file) — shown honestly.
    pub error: Option<(PathBuf, String)>,
}

impl Default for ModelLayer {
    fn default() -> Self {
        Self {
            slot: WorkerSlot::idle("model-field"),
            loaded: None,
            error: None,
        }
    }
}

impl ModelLayer {
    /// Keep the layer pointed at `wanted`; spawns at most one load.
    pub fn drive(&mut self, ctx: &egui::Context, wanted: Option<&Path>) {
        if let SlotPoll::Ready(result) = self.slot.poll() {
            match result {
                Ok(field) => {
                    let texture = ctx.load_texture(
                        format!("model-{}", field.path.display()),
                        field.image.clone(),
                        egui::TextureOptions::LINEAR,
                    );
                    self.error = None;
                    self.loaded = Some((field, texture));
                }
                Err(error) => {
                    // Path context arrives with the message.
                    self.error = Some((PathBuf::from(""), error));
                }
            }
        }
        let Some(wanted) = wanted else {
            self.loaded = None;
            self.error = None;
            return;
        };
        let already = self
            .loaded
            .as_ref()
            .map(|(field, _)| field.path == wanted)
            .unwrap_or(false);
        let failed = self
            .error
            .as_ref()
            .map(|(path, _)| path == wanted)
            .unwrap_or(false);
        if already || failed || self.slot.in_flight() {
            return;
        }
        if !wanted.exists() {
            self.error = Some((
                wanted.to_path_buf(),
                format!(
                    "committed frame not on disk: {} (fixture replays reference files that were never produced)",
                    wanted.display()
                ),
            ));
            return;
        }
        let path = wanted.to_path_buf();
        self.slot.spawn(ctx, move |tx| {
            let result = load_composite_reflectivity(&path);
            let _ = tx.send(result.map_err(|error| format!("{}: {error}", path.display())));
        });
        // Remember which path a failure belongs to.
        if let Some((error_path, _)) = &mut self.error {
            *error_path = wanted.to_path_buf();
        }
    }

    /// Mesh-paint the loaded raster warped onto the AEQD map: vertices at
    /// decimated cell centers from the FILE's lat/lon, UVs into the
    /// raster texture (tile-mesh discipline from map_paint/model_layer).
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect, view: &arwen_map::MapView) {
        let Some((field, texture)) = &self.loaded else {
            return;
        };
        let step_x = (field.nx / 48).max(1);
        let step_y = (field.ny / 48).max(1);
        let columns: Vec<usize> = (0..field.nx)
            .step_by(step_x)
            .chain(std::iter::once(field.nx - 1))
            .collect();
        let rows: Vec<usize> = (0..field.ny)
            .step_by(step_y)
            .chain(std::iter::once(field.ny - 1))
            .collect();
        let mut mesh = egui::epaint::Mesh::with_texture(texture.id());
        let tint = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 235);
        let mut finite = true;
        for &j in &rows {
            for &i in &columns {
                let index = j * field.nx + i;
                let pos = view.lon_lat_to_screen(rect, field.lon[index], field.lat[index]);
                if !pos.x.is_finite() || !pos.y.is_finite() {
                    finite = false;
                }
                // v flipped: the image is top-row-first.
                let uv = egui::pos2(
                    (i as f32 + 0.5) / field.nx as f32,
                    1.0 - (j as f32 + 0.5) / field.ny as f32,
                );
                mesh.vertices.push(egui::epaint::Vertex {
                    pos,
                    uv,
                    color: tint,
                });
            }
        }
        if !finite {
            return;
        }
        let stride = columns.len() as u32;
        for row in 0..(rows.len() as u32 - 1) {
            for column in 0..(stride - 1) {
                let a = row * stride + column;
                mesh.indices.extend_from_slice(&[
                    a,
                    a + 1,
                    a + stride,
                    a + 1,
                    a + stride + 1,
                    a + stride,
                ]);
            }
        }
        painter.add(egui::Shape::mesh(mesh));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_field() -> LoadedField {
        // A 4×3 grid around 35N 97.5W with 0.1° spacing, value = i + j*10.
        let (nx, ny) = (4usize, 3usize);
        let mut values = Vec::new();
        let mut lat = Vec::new();
        let mut lon = Vec::new();
        for j in 0..ny {
            for i in 0..nx {
                values.push((i + j * 10) as f32);
                lat.push(35.0 + j as f32 * 0.1);
                lon.push(-97.5 + i as f32 * 0.1);
            }
        }
        LoadedField {
            path: PathBuf::from("synthetic"),
            name: "maxdbz".into(),
            units: "dBZ".into(),
            description: "synthetic".into(),
            nx,
            ny,
            values,
            lat,
            lon,
            image: egui::ColorImage {
                size: [nx, ny],
                source_size: egui::vec2(nx as f32, ny as f32),
                pixels: vec![egui::Color32::BLACK; nx * ny],
            },
        }
    }

    #[test]
    fn hover_lookup_returns_the_nearest_cell_and_rejects_off_grid() {
        let field = synthetic_field();
        // Exactly on cell (2, 1): value 12.
        assert_eq!(field.value_at(35.1, -97.3), Some(12.0));
        // Slightly off-center still snaps to the nearest cell.
        assert_eq!(field.value_at(35.104, -97.296), Some(12.0));
        // Far off the grid: no value, never a made-up one.
        assert_eq!(field.value_at(20.0, -60.0), None);
    }

    /// The REAL read path, against a REAL wrfout artifact. Ignored by
    /// default because it needs a multi-hundred-MB file on this machine;
    /// run explicitly with:
    ///   cargo test -p arwen-studio real_wrfout -- --ignored
    /// after setting ARWEN_TEST_WRFOUT to a wrfout path.
    #[test]
    #[ignore = "needs a local wrfout (set ARWEN_TEST_WRFOUT)"]
    fn real_wrfout_composite_reflectivity_reads() {
        // Hand harness: needs a real wrfout on disk. Self-skips (with a
        // reason) so the one-command matrix (--include-ignored) stays a
        // truthful green without one.
        let Ok(path) = std::env::var("ARWEN_TEST_WRFOUT") else {
            eprintln!("skipped: set ARWEN_TEST_WRFOUT to a real wrfout to run this");
            return;
        };
        let field = load_composite_reflectivity(Path::new(&path)).unwrap();
        assert!(field.nx > 10 && field.ny > 10, "{}x{}", field.nx, field.ny);
        assert_eq!(field.values.len(), field.nx * field.ny);
        assert_eq!(field.units.to_lowercase(), "dbz");
        // The field must contain finite values and plausible dBZ range.
        let finite: Vec<f32> = field
            .values
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .collect();
        assert!(!finite.is_empty());
        let max = finite.iter().cloned().fold(f32::MIN, f32::max);
        assert!((-40.0..=90.0).contains(&max), "max dBZ {max}");
        // Geolocation is real: lats/lons in range and varying.
        assert!(field.lat.iter().all(|lat| (-90.0..=90.0).contains(lat)));
        assert!(field.lon.iter().all(|lon| (-180.0..=180.0).contains(lon)));
        let hover = field.value_at(
            field.lat[field.values.len() / 2],
            field.lon[field.values.len() / 2],
        );
        assert!(hover.is_some(), "hover misses the grid center");
        println!(
            "real wrfout OK: {}x{} max {max:.1} dBZ ({})",
            field.nx, field.ny, field.description
        );
    }
}
