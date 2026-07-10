//! Shared native-plot surface for stored satellite frames and in-memory
//! SimSat products.
//!
//! The satellite player owns north-up, palette-colored preview images.  A
//! native plot must not recover science data from those images: stored scalar
//! values and baked RGB pixels stay in their original grid row order and are
//! projected with the matching latitude/longitude mesh here.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use rustwx_core::{Field2D, GridProjection, GridShape, LatLonGrid, ProductKey};
use rustwx_render::{
    Color, ColorScale, DiscreteColorScale, ExtendMode, MapRenderRequest, ProductVisualMode,
    RgbaGridField,
};
use rw_store::grid::GridFile;

const PREVIEW_QUALITY_SCALE: f32 = 1.5;
#[cfg(any(windows, target_os = "macos"))]
const EXPORT_WIDTH: u32 = 1600;
#[cfg(any(windows, target_os = "macos"))]
const EXPORT_HEIGHT: u32 = 1200;
const PALETTE_BINS: usize = 256;

/// A sampled scalar palette in the exact units carried by a plot source.
///
/// Satellite enhancement tables can contain duplicate breakpoints for hard
/// steps. Sampling them onto a dense, strictly increasing scale preserves
/// those steps while satisfying rustwx-render's discrete-scale contract.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SatellitePlotPalette {
    levels: Vec<f64>,
    colors: Vec<Color>,
}

impl SatellitePlotPalette {
    pub(crate) fn from_satellite_anchors(anchors: rw_sat::palette::Anchors) -> Self {
        let lo = anchors.first().map_or(0.0, |anchor| anchor.0);
        let hi = anchors.last().map_or(1.0, |anchor| anchor.0);
        Self::from_remapped_satellite_anchors(lo, hi, lo, hi, anchors)
    }

    /// Build a palette whose DATA range (`data_lo..data_hi`) is colored by a
    /// different range of an anchor table (`palette_lo..palette_hi`). This is
    /// the legacy pre-calibration AHI auto-stretch without changing the raw
    /// values passed to the plot/colorbar.
    pub(crate) fn from_remapped_satellite_anchors(
        data_lo: f32,
        data_hi: f32,
        palette_lo: f32,
        palette_hi: f32,
        anchors: rw_sat::palette::Anchors,
    ) -> Self {
        let (data_lo, data_hi) = finite_nonempty_range(data_lo, data_hi);
        let mut levels = Vec::with_capacity(PALETTE_BINS + 1);
        let mut colors = Vec::with_capacity(PALETTE_BINS);
        for step in 0..=PALETTE_BINS {
            levels.push(
                f64::from(data_lo)
                    + f64::from(data_hi - data_lo) * step as f64 / PALETTE_BINS as f64,
            );
        }
        for step in 0..PALETTE_BINS {
            let norm = (step as f32 + 0.5) / PALETTE_BINS as f32;
            let value = palette_lo + norm * (palette_hi - palette_lo);
            let [r, g, b, a] = rw_sat::palette::anchor_color(value, anchors);
            colors.push(Color::rgba(r, g, b, a));
        }
        Self { levels, colors }
    }

    fn scale(&self) -> ColorScale {
        ColorScale::Discrete(DiscreteColorScale {
            levels: self.levels.clone(),
            colors: self.colors.clone(),
            extend: ExtendMode::Both,
            mask_below: None,
        })
    }
}

/// Raw plot raster. Every vector is in storage/grid row order (row zero
/// first); no player/display row flip is permitted here.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SatellitePlotRaster {
    Scalar {
        variable: String,
        units: String,
        selector: serde_json::Value,
        values: Vec<f32>,
        palette: Option<SatellitePlotPalette>,
    },
    Rgba {
        pixels: Vec<Color>,
    },
}

/// One georeferenced satellite product ready for interactive plotting or PNG
/// export. Titles/subtitles are supplied by the producer so store frames can
/// identify model/run/time while direct SimSat arrays can carry their own
/// product identity.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SatellitePlotSource {
    pub(crate) title: String,
    pub(crate) subtitle_left: String,
    pub(crate) subtitle_right: String,
    pub(crate) nx: usize,
    pub(crate) ny: usize,
    pub(crate) grid: Arc<GridFile>,
    pub(crate) raster: SatellitePlotRaster,
}

impl SatellitePlotSource {
    /// Constructor used by the satellite-store worker. Unlike the mesh
    /// constructor below, it retains the store selector and its resolved
    /// production palette.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scalar_from_store(
        title: impl Into<String>,
        subtitle_left: impl Into<String>,
        subtitle_right: impl Into<String>,
        variable: impl Into<String>,
        units: impl Into<String>,
        selector: serde_json::Value,
        values: Vec<f32>,
        grid: Arc<GridFile>,
        palette: Option<SatellitePlotPalette>,
    ) -> Result<Self, String> {
        let nx = grid.nx;
        let ny = grid.ny;
        validate_mesh(nx, ny, values.len(), grid.lat.len(), grid.lon.len())?;
        Ok(Self {
            title: title.into(),
            subtitle_left: subtitle_left.into(),
            subtitle_right: subtitle_right.into(),
            nx,
            ny,
            grid,
            raster: SatellitePlotRaster::Scalar {
                variable: variable.into(),
                units: units.into(),
                selector,
                values,
                palette,
            },
        })
    }

    /// Constructor used by the satellite-store worker for baked RGB frames.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rgba_from_store(
        title: impl Into<String>,
        subtitle_left: impl Into<String>,
        subtitle_right: impl Into<String>,
        pixels: Vec<Color>,
        grid: Arc<GridFile>,
    ) -> Result<Self, String> {
        let nx = grid.nx;
        let ny = grid.ny;
        validate_mesh(nx, ny, pixels.len(), grid.lat.len(), grid.lon.len())?;
        Ok(Self {
            title: title.into(),
            subtitle_left: subtitle_left.into(),
            subtitle_right: subtitle_right.into(),
            nx,
            ny,
            grid,
            raster: SatellitePlotRaster::Rgba { pixels },
        })
    }

    /// Constructor-friendly bridge for a raw scalar produced directly by
    /// SimSat. Values and coordinates must all be row-major in the same row
    /// order. A generic smooth viridis scale is used for derived fields whose
    /// product has no satellite-band selector.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scalar_from_mesh(
        title: impl Into<String>,
        subtitle_left: impl Into<String>,
        subtitle_right: impl Into<String>,
        units: impl Into<String>,
        nx: usize,
        ny: usize,
        values: Vec<f32>,
        lat: Vec<f32>,
        lon: Vec<f32>,
        projection: Option<GridProjection>,
    ) -> Result<Self, String> {
        validate_mesh(nx, ny, values.len(), lat.len(), lon.len())?;
        let title = title.into();
        let variable = product_slug(&title);
        let grid = Arc::new(in_memory_grid(nx, ny, lat, lon, projection));
        Self::scalar_from_store(
            title,
            subtitle_left,
            subtitle_right,
            variable,
            units,
            serde_json::Value::Null,
            values,
            grid,
            None,
        )
    }

    /// Constructor-friendly bridge for a baked RGBA product produced
    /// directly by SimSat. Transparent pixels remain transparent when the
    /// georeferenced grid is projected.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rgba_from_mesh(
        title: impl Into<String>,
        subtitle_left: impl Into<String>,
        subtitle_right: impl Into<String>,
        nx: usize,
        ny: usize,
        pixels: Vec<Color>,
        lat: Vec<f32>,
        lon: Vec<f32>,
        projection: Option<GridProjection>,
    ) -> Result<Self, String> {
        validate_mesh(nx, ny, pixels.len(), lat.len(), lon.len())?;
        let grid = Arc::new(in_memory_grid(nx, ny, lat, lon, projection));
        Self::rgba_from_store(title, subtitle_left, subtitle_right, pixels, grid)
    }

    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) fn suggested_file_name(&self) -> String {
        format!("{}.png", sanitize_file_component(&self.title))
    }

    /// Build the single source of truth used by preview rendering and PNG
    /// export. Composite pixels use `RgbaGridField`; the dummy scalar field is
    /// shape/geolocation scaffolding only and never reaches the rasterizer.
    pub(crate) fn build_render_request(
        &self,
        width: u32,
        height: u32,
    ) -> Result<MapRenderRequest, String> {
        if width == 0 || height == 0 {
            return Err("satellite plot dimensions must be non-zero".to_string());
        }
        let raster_len = match &self.raster {
            SatellitePlotRaster::Scalar { values, .. } => values.len(),
            SatellitePlotRaster::Rgba { pixels } => pixels.len(),
        };
        validate_mesh(
            self.nx,
            self.ny,
            raster_len,
            self.grid.lat.len(),
            self.grid.lon.len(),
        )?;
        let grid = LatLonGrid {
            shape: GridShape {
                nx: self.nx,
                ny: self.ny,
            },
            lat_deg: self.grid.lat.clone(),
            lon_deg: self.grid.lon.clone(),
        };
        let bounds = geographic_bounds(&self.grid.lat, &self.grid.lon)
            .ok_or_else(|| "satellite grid has no finite lat/lon bounds".to_string())?;
        let projected = rustwx_products::direct::build_projected_map_with_projection(
            &self.grid.lat,
            &self.grid.lon,
            self.grid.projection.as_ref(),
            bounds,
            width as f64 / height as f64,
        )
        .map_err(|error| error.to_string())?;

        let (variable, units, values, scale, rgba) = match &self.raster {
            SatellitePlotRaster::Scalar {
                variable,
                units,
                selector,
                values,
                palette,
            } => {
                // Retained as part of the raw plot contract for callers that
                // need satellite band/product metadata. Palette resolution
                // already consumed it in the store worker.
                let _selector = selector;
                (
                    variable.clone(),
                    units.clone(),
                    values.clone(),
                    palette
                        .as_ref()
                        .map(SatellitePlotPalette::scale)
                        .unwrap_or_else(|| generic_scale(values)),
                    None,
                )
            }
            SatellitePlotRaster::Rgba { pixels } => (
                product_slug(&self.title),
                "rgba8".to_string(),
                vec![0.0; self.cell_count()?],
                generic_scale(&[]),
                Some(pixels.clone()),
            ),
        };
        let field = Field2D::new(ProductKey::named(variable), units, grid.clone(), values)
            .map_err(|error| error.to_string())?;
        let mut request = MapRenderRequest::from_core_field(field, scale);
        rustwx_products::plot_design::StaticPlotDesign::new(
            bounds,
            ProductVisualMode::FilledMeteorology,
        )
        .apply_to_request(&mut request);
        request.apply_projected_map(&projected);
        request.title = Some(self.title.clone());
        request.subtitle_left = Some(self.subtitle_left.clone());
        request.subtitle_right = Some(self.subtitle_right.clone());
        request.width = width;
        request.height = height;
        request.supersample_factor = 1;
        if let Some(pixels) = rgba {
            request.set_rgba_grid(
                RgbaGridField::new(grid, pixels).map_err(|error| error.to_string())?,
            );
            request.colorbar = false;
        }
        Ok(request)
    }

    pub(crate) fn render_image(&self, width: u32, height: u32) -> Result<image::RgbaImage, String> {
        let request = self.build_render_request(width, height)?;
        rustwx_render::render_image(&request).map_err(|error| error.to_string())
    }

    #[cfg_attr(not(any(windows, target_os = "macos", test)), allow(dead_code))]
    pub(crate) fn save_png(&self, path: &Path, width: u32, height: u32) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        let request = self.build_render_request(width, height)?;
        rustwx_render::save_png(&request, path).map_err(|error| error.to_string())
    }

    fn cell_count(&self) -> Result<usize, String> {
        self.nx.checked_mul(self.ny).ok_or_else(|| {
            format!(
                "satellite grid {}x{} overflows cell count",
                self.nx, self.ny
            )
        })
    }
}

/// Cached interactive surface mounted inside ModelDataDock's existing native
/// plot window. Model field state remains untouched when this source is set.
pub(crate) struct SatellitePlotPanel {
    source: Option<Arc<SatellitePlotSource>>,
    generation: u64,
    texture: Option<egui::TextureHandle>,
    cache_key: Option<(u64, u32, u32)>,
    error: Option<String>,
    status: Option<String>,
    last_render_ms: Option<f32>,
    last_upload_ms: Option<f32>,
}

impl Default for SatellitePlotPanel {
    fn default() -> Self {
        Self {
            source: None,
            generation: 0,
            texture: None,
            cache_key: None,
            error: None,
            status: None,
            last_render_ms: None,
            last_upload_ms: None,
        }
    }
}

impl SatellitePlotPanel {
    pub(crate) fn set_source(&mut self, source: SatellitePlotSource) {
        self.source = Some(Arc::new(source));
        self.generation = self.generation.wrapping_add(1);
        self.clear_render();
        self.status = None;
    }

    pub(crate) fn clear_source(&mut self) {
        self.source = None;
        self.generation = self.generation.wrapping_add(1);
        self.clear_render();
        self.status = None;
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> Option<&SatellitePlotSource> {
        self.source.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn save_png(&self, path: &Path, width: u32, height: u32) -> Result<(), String> {
        self.source
            .as_deref()
            .ok_or_else(|| "no satellite plot is loaded".to_string())?
            .save_png(path, width, height)
    }

    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) {
        let Some(source) = self.source.clone() else {
            ui.weak("Choose a satellite frame, then press Native plot.");
            return;
        };

        ui.horizontal_wrapped(|ui| {
            ui.strong(source.title.as_str());
            ui.separator();
            let save = ui
                .add_enabled(
                    cfg!(any(windows, target_os = "macos")),
                    egui::Button::new("Save PNG"),
                )
                .on_hover_text("Export this georeferenced native plot as a 1600x1200 PNG");
            if save.clicked() {
                self.save_dialog(&source);
            }
        });
        if let Some(status) = &self.status {
            ui.label(egui::RichText::new(status).small().weak());
        }

        let available = ui.available_size();
        let aspect = (source.nx as f32 / source.ny.max(1) as f32).clamp(0.4, 3.0);
        let (display_width, display_height) = quantized_plot_size(available, aspect);
        let width = ((display_width as f32 * PREVIEW_QUALITY_SCALE).round() as u32).max(1);
        let height = ((display_height as f32 * PREVIEW_QUALITY_SCALE).round() as u32).max(1);
        let key = (self.generation, width, height);
        if self.cache_key != Some(key) {
            self.render(ui, &source, key);
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
            return;
        }
        let Some(texture) = &self.texture else {
            ui.spinner();
            return;
        };
        ui.add(
            egui::Image::new(texture)
                .fit_to_exact_size(egui::vec2(display_width as f32, display_height as f32)),
        );
        if let Some(render_ms) = self.last_render_ms {
            ui.label(
                egui::RichText::new(format!(
                    "satellite native plot render {render_ms:.0} ms / upload {:.0} ms / {width}x{height}",
                    self.last_upload_ms.unwrap_or(0.0),
                ))
                .small()
                .weak(),
            );
        }
    }

    fn clear_render(&mut self) {
        self.texture = None;
        self.cache_key = None;
        self.error = None;
        self.last_render_ms = None;
        self.last_upload_ms = None;
    }

    fn render(&mut self, ui: &egui::Ui, source: &SatellitePlotSource, key: (u64, u32, u32)) {
        self.clear_render();
        self.cache_key = Some(key);
        let started = Instant::now();
        let image = match source.render_image(key.1, key.2) {
            Ok(image) => image,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        self.last_render_ms = Some(started.elapsed().as_secs_f32() * 1000.0);
        let upload = Instant::now();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        );
        self.texture = Some(ui.ctx().load_texture(
            "bowecho-satellite-native-plot",
            color_image,
            egui::TextureOptions {
                magnification: egui::TextureFilter::Linear,
                minification: egui::TextureFilter::Linear,
                ..Default::default()
            },
        ));
        self.last_upload_ms = Some(upload.elapsed().as_secs_f32() * 1000.0);
    }

    #[cfg(any(windows, target_os = "macos"))]
    fn save_dialog(&mut self, source: &SatellitePlotSource) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PNG image", &["png"])
            .set_file_name(source.suggested_file_name())
            .set_title("Save satellite native plot")
            .save_file()
        else {
            return;
        };
        self.status = Some(match source.save_png(&path, EXPORT_WIDTH, EXPORT_HEIGHT) {
            Ok(()) => format!("Saved PNG to {}", path.display()),
            Err(error) => format!("PNG export failed: {error}"),
        });
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn save_dialog(&mut self, _source: &SatellitePlotSource) {}
}

fn validate_mesh(
    nx: usize,
    ny: usize,
    raster_len: usize,
    lat_len: usize,
    lon_len: usize,
) -> Result<(), String> {
    if nx == 0 || ny == 0 {
        return Err(format!("degenerate satellite grid {nx}x{ny}"));
    }
    let expected = nx
        .checked_mul(ny)
        .ok_or_else(|| format!("satellite grid {nx}x{ny} overflows cell count"))?;
    if raster_len != expected || lat_len != expected || lon_len != expected {
        return Err(format!(
            "satellite grid {nx}x{ny} needs {expected} cells; raster={raster_len}, lat={lat_len}, lon={lon_len}"
        ));
    }
    Ok(())
}

fn in_memory_grid(
    nx: usize,
    ny: usize,
    lat: Vec<f32>,
    lon: Vec<f32>,
    projection: Option<GridProjection>,
) -> GridFile {
    let mut hasher = DefaultHasher::new();
    nx.hash(&mut hasher);
    ny.hash(&mut hasher);
    for value in lat.iter().chain(&lon) {
        value.to_bits().hash(&mut hasher);
    }
    GridFile {
        nx,
        ny,
        lat,
        lon,
        projection,
        hash: format!("in-memory-{:016x}", hasher.finish()),
    }
}

fn generic_scale(values: &[f32]) -> ColorScale {
    let (lo, hi) = finite_min_max(values).unwrap_or((0.0, 1.0));
    let (lo, hi) = finite_nonempty_range(lo, hi);
    const BINS: usize = 64;
    let levels = (0..=BINS)
        .map(|step| f64::from(lo) + f64::from(hi - lo) * step as f64 / BINS as f64)
        .collect();
    let colors = (0..BINS)
        .map(|step| {
            let [r, g, b, a] = rw_ui::colormap::VIRIDIS
                .sample((step as f32 + 0.5) / BINS as f32)
                .to_array();
            Color::rgba(r, g, b, a)
        })
        .collect();
    ColorScale::Discrete(DiscreteColorScale {
        levels,
        colors,
        extend: ExtendMode::Both,
        mask_below: None,
    })
}

fn finite_min_max(values: &[f32]) -> Option<(f32, f32)> {
    let mut range: Option<(f32, f32)> = None;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        range = Some(match range {
            Some((lo, hi)) => (lo.min(value), hi.max(value)),
            None => (value, value),
        });
    }
    range
}

fn finite_nonempty_range(lo: f32, hi: f32) -> (f32, f32) {
    let lo = if lo.is_finite() { lo } else { 0.0 };
    let hi = if hi.is_finite() { hi } else { lo + 1.0 };
    if hi > lo { (lo, hi) } else { (lo, lo + 1.0) }
}

/// Bounds in the conventional `[-180, 180)` longitude domain. The longitude
/// interval is the smallest circular interval containing the finite mesh, so
/// Himawari/full-disk grids crossing the dateline return `west > east` instead
/// of a misleading nearly-360-degree box. The renderer understands that
/// wrapped-bounds convention.
fn geographic_bounds(lat: &[f32], lon: &[f32]) -> Option<(f64, f64, f64, f64)> {
    let mut south = f64::INFINITY;
    let mut north = f64::NEG_INFINITY;
    let mut longitudes = Vec::new();
    for (&lat, &lon) in lat.iter().zip(lon) {
        let lat = f64::from(lat);
        let lon = f64::from(lon);
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        south = south.min(lat);
        north = north.max(lat);
        longitudes.push(lon.rem_euclid(360.0));
    }
    if !south.is_finite() || !north.is_finite() || longitudes.is_empty() {
        return None;
    }
    longitudes.sort_by(f64::total_cmp);
    longitudes.dedup_by(|left, right| (*left - *right).abs() < 1.0e-9);
    if longitudes.len() == 1 {
        let lon = normalize_lon(longitudes[0]);
        return Some((lon, lon, south, north));
    }
    let mut largest_gap = f64::NEG_INFINITY;
    let mut gap_after = 0usize;
    for index in 0..longitudes.len() {
        let current = longitudes[index];
        let next = if index + 1 < longitudes.len() {
            longitudes[index + 1]
        } else {
            longitudes[0] + 360.0
        };
        let gap = next - current;
        if gap > largest_gap {
            largest_gap = gap;
            gap_after = index;
        }
    }
    let west = longitudes[(gap_after + 1) % longitudes.len()];
    let east = longitudes[gap_after];
    Some((normalize_lon(west), normalize_lon(east), south, north))
}

fn normalize_lon(lon: f64) -> f64 {
    ((lon + 180.0).rem_euclid(360.0)) - 180.0
}

fn product_slug(title: &str) -> String {
    let slug = title
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let slug = slug.trim_matches('_').to_string();
    if slug.is_empty() {
        "satellite_product".to_string()
    } else {
        slug
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn sanitize_file_component(value: &str) -> String {
    let component = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let component = component.trim_matches('_');
    if component.is_empty() {
        "satellite-plot".to_string()
    } else {
        component.to_string()
    }
}

fn quantized_plot_size(available: egui::Vec2, aspect: f32) -> (u32, u32) {
    let aspect = aspect.clamp(0.4, 3.0);
    let max_width = available.x.max(480.0).min(1600.0);
    let max_height = available.y.max(320.0).min(1200.0);
    let mut width = max_width;
    let mut height = width / aspect;
    if height > max_height {
        height = max_height;
        width = height * aspect;
    }
    let quantize =
        |value: f32, min: u32| (((value.max(min as f32) / 32.0).round() as u32) * 32).max(min);
    (quantize(width, 320), quantize(height, 240))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh() -> (Vec<f32>, Vec<f32>) {
        // Storage row zero is SOUTH. Keeping this order through the request is
        // the invariant: the projected renderer follows the mesh, not a
        // north-up preview flip.
        (
            vec![30.0, 30.0, 31.0, 31.0],
            vec![-101.0, -100.0, -101.0, -100.0],
        )
    }

    #[test]
    fn scalar_request_preserves_storage_rows_and_uses_a_colorbar() {
        let (lat, lon) = mesh();
        let source = SatellitePlotSource::scalar_from_mesh(
            "SimSat cloud-top temperature",
            "wrf 20260710_00z",
            "0030Z",
            "K",
            2,
            2,
            vec![280.0, 281.0, 220.0, 221.0],
            lat.clone(),
            lon.clone(),
            None,
        )
        .unwrap();
        let request = source.build_render_request(640, 480).unwrap();
        assert_eq!(request.field.values, vec![280.0, 281.0, 220.0, 221.0]);
        assert_eq!(request.field.grid.lat_deg, lat);
        assert_eq!(request.field.grid.lon_deg, lon);
        assert!(request.rgba_grid.is_none());
        assert!(request.colorbar);
    }

    #[test]
    fn rgba_request_uses_projected_rgba_grid_without_a_colorbar() {
        let (lat, lon) = mesh();
        let pixels = vec![
            Color::rgba(255, 0, 0, 255),
            Color::rgba(0, 255, 0, 255),
            Color::rgba(0, 0, 255, 255),
            Color::TRANSPARENT,
        ];
        let source = SatellitePlotSource::rgba_from_mesh(
            "SimSat GeoColor",
            "hrrr 20260710_00z",
            "f001",
            2,
            2,
            pixels.clone(),
            lat,
            lon,
            None,
        )
        .unwrap();
        let request = source.build_render_request(640, 480).unwrap();
        assert_eq!(request.rgba_grid.as_ref().unwrap().pixels, pixels);
        assert!(!request.colorbar);
        assert_eq!(request.title.as_deref(), Some("SimSat GeoColor"));
    }

    #[test]
    fn mesh_constructors_reject_misaligned_arrays() {
        let (lat, lon) = mesh();
        let error = SatellitePlotSource::scalar_from_mesh(
            "bad",
            "",
            "",
            "K",
            2,
            2,
            vec![1.0, 2.0, 3.0],
            lat,
            lon,
            None,
        )
        .unwrap_err();
        assert!(error.contains("needs 4 cells"), "{error}");
    }

    #[test]
    fn geographic_bounds_wrap_the_dateline_for_full_disk_meshes() {
        let bounds =
            geographic_bounds(&[0.0, 0.0, 1.0, 1.0], &[170.0, -170.0, 175.0, -175.0]).unwrap();
        assert!(
            bounds.0 > bounds.1,
            "wrapped west/east expected: {bounds:?}"
        );
        let span = bounds.1 + 360.0 - bounds.0;
        assert!(
            (span - 20.0).abs() < 1.0e-6,
            "span={span}, bounds={bounds:?}"
        );
    }
}
