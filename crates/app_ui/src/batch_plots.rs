//! Batch "plot everything" renderer: production-quality PNG plots of every
//! field of a model run, for every forecast hour, written as a browsable
//! output tree — the WRF-Runner-style bulk product set, but for anything in
//! the model store.
//!
//! Output tree (under the brand screenshots folder, user-visible on purpose):
//!
//! ```text
//! <screenshots>/plots/<model>/<run>/<field>/f###.png
//! <screenshots>/plots/<model>/<run>/index.json
//! ```
//!
//! Rendering is the pinned `rustwx-render` headless pipeline — the same
//! full-furniture plot (colorbar, titles, Natural Earth basemap) the rw-ui
//! native plot viewer draws, re-implemented app-side because the viewer's
//! `render_field_plot` recipe is private to the pinned crate. No window, GPU
//! or egui paint is involved, so the job runs on a worker thread at
//! below-normal priority like the import workers.
//!
//! Coverage per hour:
//! - every `surface2d` store variable, and
//! - the synthesized per-level isobaric planes (temperature/dewpoint/RH/wind
//!   speed/height at 925–250 mb) exactly as the dock's picker offers them,
//!   loaded through [`crate::model_data::iso_fields::load_level_field`].
//!
//! Styling precedence mirrors the app: the store worker's resolution (user 🎨
//! override, then the operational production style — bucket `"production"` in
//! `index.json`), then the Solarpower07 fallback for local-WRF fields
//! (`"solar"`), then a clearly-labeled generic viridis ramp over the field's
//! own value range (`"generic"`, counted as `fallback_styled` in the summary)
//! so every field gets a plot even without a production counterpart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};

use color_tables::iso_levels::{ISO_PICKER_LEVELS_HPA, IsoLevelField, IsoLevelSpec};
use rustwx_core::{Field2D, GridShape, LatLonGrid, ProductKey};
use rustwx_render::{MapRenderRequest, ProductVisualMode, ProjectedMap};
use rw_store::format::RwsVariableMeta;
use rw_store::grid::GridFile;
use rw_ui::{FieldData, FieldKey, HourKey, StoreView, StyleOverrideSettings};

use crate::model_data::{attach_solar_fallback_style, display_var_name};

/// Default output size. Production plots are landscape 4:3 — big enough for
/// the colorbar/title furniture to read, small enough that a 100-field ×
/// 24-hour run stays in the low-GB range on disk.
const DEFAULT_PLOT_WIDTH: u32 = 1600;
const DEFAULT_PLOT_HEIGHT: u32 = 1200;

/// Cap on free-text notes carried in the summary (a run where every field
/// fails would otherwise produce thousands).
const MAX_NOTES: usize = 15;

/// Everything a batch plot job needs, captured at launch.
#[derive(Debug, Clone)]
pub(crate) struct BatchPlotRequest {
    pub store_root: PathBuf,
    pub model: String,
    pub run: String,
    /// Base output directory (`<screenshots>/plots`); the job appends
    /// `<model>/<run>/…`.
    pub plots_base: PathBuf,
    /// The 🎨 editor's current settings snapshot, so user palette bindings
    /// win exactly as they do in the viewer.
    pub overrides: StyleOverrideSettings,
    pub options: BatchPlotOptions,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BatchPlotOptions {
    pub width: u32,
    pub height: u32,
    /// Also render the synthesized per-level isobaric planes.
    pub include_iso: bool,
}

impl Default for BatchPlotOptions {
    fn default() -> Self {
        Self {
            width: DEFAULT_PLOT_WIDTH,
            height: DEFAULT_PLOT_HEIGHT,
            include_iso: true,
        }
    }
}

/// A running batch plot job — same shape as `WrfProcessTask`, plus the cancel
/// flag the import tasks never needed (imports are minutes; a full plot sweep
/// of a long run can be much longer and MUST be interruptible).
#[derive(Debug)]
pub(crate) struct BatchPlotTask {
    #[allow(dead_code)] // parity with the import tasks; useful for logs
    pub label: String,
    pub rx: Receiver<BatchPlotMessage>,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) enum BatchPlotMessage {
    Progress(String),
    Done(Result<BatchPlotSummary, String>),
}

#[derive(Debug, Clone)]
pub(crate) struct BatchPlotSummary {
    pub model: String,
    pub run: String,
    /// The run's output directory (`<plots_base>/<model>/<run>`).
    pub out_dir: PathBuf,
    /// PNGs written.
    pub written: usize,
    /// Subset of `written` that fell through to the generic ramp (no
    /// production/user/Solar style).
    pub fallback_styled: usize,
    /// Plots deliberately not attempted (hour grid mismatch).
    pub skipped: usize,
    /// Plots attempted but failed (read/render/IO error).
    pub failed: usize,
    /// The cancel flag stopped the job before the sweep finished.
    pub cancelled: bool,
    pub notes: Vec<String>,
}

impl BatchPlotSummary {
    /// One-line completion status for the dock.
    pub(crate) fn status_line(&self) -> String {
        let mut line = format!(
            "Plotted {} PNG(s) for {}/{} → {}",
            self.written,
            self.model,
            self.run,
            self.out_dir.display()
        );
        let mut extras = Vec::new();
        if self.fallback_styled > 0 {
            extras.push(format!("{} with generic style", self.fallback_styled));
        }
        if self.failed > 0 {
            extras.push(format!("{} failed", self.failed));
        }
        if self.skipped > 0 {
            extras.push(format!("{} skipped", self.skipped));
        }
        if !extras.is_empty() {
            line.push_str(&format!(" ({})", extras.join(", ")));
        }
        if self.cancelled {
            line.push_str(" — cancelled early");
        }
        line
    }
}

/// Which rung of the style ladder a plot used (recorded per variable in
/// `index.json`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StyleSource {
    /// Resolved by the store worker's chain: user 🎨 binding or the
    /// operational production style.
    Production,
    /// The Solarpower07 local-WRF fallback ([`attach_solar_fallback_style`]).
    Solar,
    /// Generic viridis ramp over the field's own value range.
    Generic,
}

impl StyleSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Solar => "solar",
            Self::Generic => "generic",
        }
    }
}

/// `index.json` document: what got plotted, where, with which style — so
/// external tooling (or a future in-app gallery) can walk a run's plots
/// without globbing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlotIndex {
    pub schema: String,
    pub model: String,
    pub run: String,
    pub generated_unix: u64,
    pub plots: Vec<PlotIndexVar>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlotIndexVar {
    /// Store variable name (or synthesized iso slug like `temperature_850`).
    pub var: String,
    /// Plot title (the style's title — the friendly label for fallbacks).
    pub title: String,
    /// Display units after the style's unit conversion.
    pub units: String,
    /// `"production"` | `"solar"` | `"generic"` (see [`StyleSource`]).
    pub style: String,
    pub frames: Vec<PlotIndexFrame>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlotIndexFrame {
    pub hour: u16,
    /// Path relative to the run's plot directory, forward slashes.
    pub path: String,
}

pub(crate) const PLOT_INDEX_SCHEMA: &str = "bowecho-plot-index/1";

/// Spawn the batch plot worker. Below-normal thread priority like the import
/// workers (the owner's machine has hard-crashed under all-core load), one
/// progress message per attempted plot, terminal `Done`.
pub(crate) fn spawn_batch_plot(
    request: BatchPlotRequest,
    repaint: eframe::egui::Context,
) -> BatchPlotTask {
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancel);
    let label = format!("Plot all fields {}/{}", request.model, request.run);
    std::thread::Builder::new()
        .name("batch-plots".to_owned())
        .spawn(move || {
            crate::wrf_process::lower_import_thread_priority();
            let mut progress = |message: String| {
                let _ = tx.send(BatchPlotMessage::Progress(message));
                repaint.request_repaint();
            };
            let result = run_batch_plot(&request, &cancel_flag, &mut progress);
            let _ = tx.send(BatchPlotMessage::Done(result));
            repaint.request_repaint();
        })
        .expect("spawn batch plot worker");
    BatchPlotTask { label, rx, cancel }
}

/// Per-run render context, built once: the projected Natural Earth basemap is
/// by far the most expensive prep step and is identical for every field and
/// hour of a run (same grid, same bounds, same aspect).
struct RunRenderCtx<'a> {
    request: &'a BatchPlotRequest,
    grid: Arc<GridFile>,
    lat_descending: bool,
    bounds: (f64, f64, f64, f64),
    projected: ProjectedMap,
    run_dir: PathBuf,
}

/// Mutable sweep state threaded through the per-field plotting.
struct RunState<'a> {
    summary: BatchPlotSummary,
    index: BTreeMap<String, PlotIndexVar>,
    progress: &'a mut dyn FnMut(String),
}

impl RunState<'_> {
    fn push_note(&mut self, note: String) {
        if self.summary.notes.len() < MAX_NOTES {
            self.summary.notes.push(note);
        } else if self.summary.notes.len() == MAX_NOTES {
            self.summary
                .notes
                .push("(further notes suppressed)".to_owned());
        }
    }
}

/// The synchronous sweep — pure with respect to the UI (no egui), so tests
/// drive it directly. Cancellation is checked between plots: the current PNG
/// always finishes, nothing half-written is left behind.
pub(crate) fn run_batch_plot(
    request: &BatchPlotRequest,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<BatchPlotSummary, String> {
    let view = StoreView::new(&request.store_root);
    let tree = view.enumerate();
    let run_entry = tree
        .models
        .iter()
        .find(|entry| entry.model == request.model)
        .and_then(|entry| entry.runs.iter().find(|run| run.run == request.run))
        .ok_or_else(|| {
            format!(
                "run {}/{} not found in store {}",
                request.model,
                request.run,
                request.store_root.display()
            )
        })?;
    let hours: Vec<u16> = run_entry.hours.iter().map(|hour| hour.hour).collect();
    if hours.is_empty() {
        return Err(format!(
            "run {}/{} has no forecast hours",
            request.model, request.run
        ));
    }

    // The run grid is mandatory: without lat/lon there is no geography to
    // plot on (the interactive viewer degrades to a flat false-color image;
    // a production plot tree should not).
    let grid = view
        .open_grid(&request.model, &request.run)
        .map(Arc::new)
        .map_err(|err| {
            format!(
                "run {}/{} has no readable grid (grid.rwg): {err}",
                request.model, request.run
            )
        })?;
    let lat_descending = grid.lat_descending().unwrap_or(false);
    let bounds = geographic_bounds(&grid.lat, &grid.lon)
        .ok_or_else(|| "run grid has no finite lat/lon bounds".to_string())?;
    let projected = rustwx_products::direct::build_projected_map_with_projection(
        &grid.lat,
        &grid.lon,
        grid.projection.as_ref(),
        bounds,
        f64::from(request.options.width) / f64::from(request.options.height),
    )
    .map_err(|err| format!("basemap projection failed: {err}"))?;

    let run_dir = plot_run_dir(&request.plots_base, &request.model, &request.run);
    std::fs::create_dir_all(&run_dir)
        .map_err(|err| format!("could not create {}: {err}", run_dir.display()))?;

    let ctx = RunRenderCtx {
        request,
        grid,
        lat_descending,
        bounds,
        projected,
        run_dir: run_dir.clone(),
    };
    let mut state = RunState {
        summary: BatchPlotSummary {
            model: request.model.clone(),
            run: request.run.clone(),
            out_dir: run_dir.clone(),
            written: 0,
            fallback_styled: 0,
            skipped: 0,
            failed: 0,
            cancelled: false,
            notes: Vec::new(),
        },
        index: BTreeMap::new(),
        progress,
    };

    let hour_count = hours.len();
    'hours: for (hour_index, &hour) in hours.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            state.summary.cancelled = true;
            break;
        }
        let reader = match view.open_hour(&request.model, &request.run, hour) {
            Ok(reader) => reader,
            Err(err) => {
                state.summary.failed += 1;
                state.push_note(format!("f{hour:03}: open failed: {err}"));
                continue;
            }
        };
        let meta = reader.meta().clone();
        if meta.nx != ctx.grid.nx || meta.ny != ctx.grid.ny {
            let surface_vars = meta
                .variables
                .iter()
                .filter(|var| var.kind == "surface2d")
                .count();
            state.summary.skipped += surface_vars;
            state.push_note(format!(
                "f{hour:03}: hour grid {}x{} does not match run grid {}x{} — skipped",
                meta.nx, meta.ny, ctx.grid.nx, ctx.grid.ny
            ));
            continue;
        }
        let model_id = meta.model.parse::<rustwx_core::ModelId>().ok();
        let store_var_names: Vec<String> =
            meta.variables.iter().map(|var| var.name.clone()).collect();

        // Every surface 2-D variable — mirror of the rw-ui worker's
        // `load_field`: read, resolve style, convert units in place.
        for var in meta.variables.iter().filter(|var| var.kind == "surface2d") {
            if cancel.load(Ordering::Relaxed) {
                state.summary.cancelled = true;
                break 'hours;
            }
            let mut values = match reader.read_full_2d(&var.name) {
                Ok(values) => values,
                Err(err) => {
                    state.summary.failed += 1;
                    state.push_note(format!("{} f{hour:03}: read failed: {err}", var.name));
                    continue;
                }
            };
            let style = request.overrides.style_for_store_variable(
                &var.name,
                &var.selector,
                &var.units,
                model_id,
            );
            let units = match &style {
                Some(style) => {
                    if !style.convert.is_none() {
                        for value in &mut values {
                            *value = style.convert.apply(*value);
                        }
                    }
                    style.display_units.clone()
                }
                None => var.units.clone(),
            };
            let range = rw_ui::colormap::finite_min_max(&values);
            let field = FieldData {
                key: FieldKey {
                    hour: HourKey {
                        model: request.model.clone(),
                        run: request.run.clone(),
                        hour,
                    },
                    var: var.name.clone(),
                },
                units,
                nx: meta.nx,
                ny: meta.ny,
                values,
                range,
                grid: Some(Arc::clone(&ctx.grid)),
                lat_descending: ctx.lat_descending,
                style,
            };
            plot_one(
                &ctx,
                &mut state,
                field,
                &store_var_names,
                (hour_index, hour_count),
            );
        }

        // Synthesized per-level isobaric planes, exactly the set the dock's
        // picker offers for this hour.
        if request.options.include_iso {
            for spec in iso_plane_specs(&meta.variables) {
                if cancel.load(Ordering::Relaxed) {
                    state.summary.cancelled = true;
                    break 'hours;
                }
                let key = FieldKey {
                    hour: HourKey {
                        model: request.model.clone(),
                        run: request.run.clone(),
                        hour,
                    },
                    var: spec.slug(),
                };
                match crate::model_data::iso_fields::load_level_field(
                    &request.store_root,
                    &key,
                    spec,
                    &request.overrides,
                ) {
                    Ok(field) => plot_one(
                        &ctx,
                        &mut state,
                        field,
                        &store_var_names,
                        (hour_index, hour_count),
                    ),
                    Err(err) => {
                        state.summary.failed += 1;
                        state.push_note(format!("{} f{hour:03}: {err}", key.var));
                    }
                }
            }
        }
    }

    if state.summary.cancelled {
        state.push_note("cancelled — index.json lists only the frames rendered".to_owned());
    }
    let index = PlotIndex {
        schema: PLOT_INDEX_SCHEMA.to_owned(),
        model: request.model.clone(),
        run: request.run.clone(),
        generated_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0),
        plots: state.index.into_values().collect(),
    };
    if let Err(err) = write_index(&run_dir, &index) {
        state.summary.failed += 1;
        state.summary.notes.push(format!("index.json: {err}"));
    }
    Ok(state.summary)
}

/// Style the field through the fallback ladder, render it, and record the
/// outcome in the summary + index. One progress message per attempt.
fn plot_one(
    ctx: &RunRenderCtx<'_>,
    state: &mut RunState<'_>,
    mut field: FieldData,
    store_var_names: &[String],
    hour_position: (usize, usize),
) {
    let source = ensure_style(&mut field, store_var_names);
    let hour = field.key.hour.hour;
    let var_slug = sanitize_component(&field.key.var);
    let path = ctx.run_dir.join(&var_slug).join(frame_file_name(hour));
    match render_field_png(
        &field,
        &ctx.projected,
        ctx.bounds,
        ctx.request.options.width,
        ctx.request.options.height,
        &path,
    ) {
        Ok(()) => {
            state.summary.written += 1;
            if source == StyleSource::Generic {
                state.summary.fallback_styled += 1;
            }
            let entry = state
                .index
                .entry(var_slug.clone())
                .or_insert_with(|| PlotIndexVar {
                    var: field.key.var.clone(),
                    title: field
                        .style
                        .as_ref()
                        .map(|style| style.title.clone())
                        .unwrap_or_else(|| field.key.var.clone()),
                    units: field.units.clone(),
                    style: source.as_str().to_owned(),
                    frames: Vec::new(),
                });
            entry.frames.push(PlotIndexFrame {
                hour,
                path: format!("{var_slug}/{}", frame_file_name(hour)),
            });
        }
        Err(err) => {
            state.summary.failed += 1;
            state.push_note(format!("{} f{hour:03}: {err}", field.key.var));
        }
    }
    (state.progress)(format!(
        "Plotting f{hour:03} ({}/{} hours): {} — {} plotted",
        hour_position.0 + 1,
        hour_position.1,
        field.key.var,
        state.summary.written
    ));
}

/// The style ladder: keep a resolved production/user style, then the Solar
/// local-WRF fallback, then a generic ramp titled with the field's friendly
/// label so the plot is honest about being unstyled.
fn ensure_style(field: &mut FieldData, store_var_names: &[String]) -> StyleSource {
    if field.style.is_some() {
        return StyleSource::Production;
    }
    attach_solar_fallback_style(field, store_var_names);
    if field.style.is_some() {
        return StyleSource::Solar;
    }
    let title =
        display_var_name(&field.key.var, store_var_names).unwrap_or_else(|| field.key.var.clone());
    field.style = generic_ramp_style(&title, &field.units, &field.values);
    StyleSource::Generic
}

/// Generic fallback style: a smooth viridis ramp spanning the field's own
/// finite value range (the same ramp the rw-ui viewer falls back to), built
/// through the user-table compiler so it renders identically to any other
/// store style.
fn generic_ramp_style(
    title: &str,
    units: &str,
    values: &[f32],
) -> Option<rustwx_products::viewer::StoreVariableStyle> {
    let (lo, hi) = rw_ui::colormap::finite_min_max(values).unwrap_or((0.0, 1.0));
    let (lo, hi) = (f64::from(lo), f64::from(hi));
    let (lo, hi) = if hi > lo { (lo, hi) } else { (lo, lo + 1.0) };
    const BINS: usize = 64;
    let levels: Vec<f64> = (0..=BINS)
        .map(|step| lo + (hi - lo) * step as f64 / BINS as f64)
        .collect();
    let colors: Vec<[u8; 4]> = (0..BINS)
        .map(|step| {
            rw_ui::colormap::VIRIDIS
                .sample((step as f32 + 0.5) / BINS as f32)
                .to_array()
        })
        .collect();
    let user = rw_ui::UserColorTable {
        name: title.to_owned(),
        title: title.to_owned(),
        display_units: units.to_owned(),
        convert: rw_ui::UserUnitConvert::None,
        legend_mode: rw_ui::UserLegendMode::SmoothRamp,
        extend: rw_ui::UserExtendMode::Both,
        mask_below: None,
        tick_step: None,
        levels,
        colors,
    };
    user.to_store_style(title, units)
}

/// The synthesized iso-plane set for one hour — the same rules as the dock
/// picker (`model_data::iso_level_entries`): a level is offered only when
/// every source volume carries it, and a real store variable claiming the
/// slug (or its `hpa`-suffixed spelling) wins over synthesis.
fn iso_plane_specs(vars: &[RwsVariableMeta]) -> Vec<IsoLevelSpec> {
    let volume = |name: &str| {
        vars.iter()
            .find(|var| var.kind == "pressure3d" && var.name == name)
    };
    let taken = |name: &str| vars.iter().any(|var| var.name == name);
    let mut specs = Vec::new();
    for field in IsoLevelField::ALL {
        let Some(sources) = field
            .source_volumes()
            .iter()
            .map(|name| volume(name))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        for level_hpa in ISO_PICKER_LEVELS_HPA {
            if !sources
                .iter()
                .all(|source| source.levels_hpa.contains(&level_hpa))
            {
                continue;
            }
            let spec = IsoLevelSpec { field, level_hpa };
            let slug = spec.slug();
            if taken(&slug) || taken(&format!("{slug}hpa")) {
                continue;
            }
            specs.push(spec);
        }
    }
    specs
}

/// One production plot to PNG — the rw-ui `render_field_plot` recipe
/// (plot_viewer.rs), full-grid variant, with the run's prebuilt projected
/// basemap applied instead of rebuilding it per field.
fn render_field_png(
    field: &FieldData,
    projected: &ProjectedMap,
    bounds: (f64, f64, f64, f64),
    width: u32,
    height: u32,
    path: &Path,
) -> Result<(), String> {
    let style = field
        .style
        .as_ref()
        .ok_or_else(|| "field has no plot style".to_string())?;
    let grid_file = field
        .grid
        .as_ref()
        .ok_or_else(|| "field has no readable run grid".to_string())?;
    if grid_file.nx != field.nx || grid_file.ny != field.ny {
        return Err(format!(
            "grid {}x{} does not match field {}x{}",
            grid_file.nx, grid_file.ny, field.nx, field.ny
        ));
    }
    let grid = LatLonGrid {
        shape: GridShape {
            nx: field.nx,
            ny: field.ny,
        },
        lat_deg: grid_file.lat.clone(),
        lon_deg: grid_file.lon.clone(),
    };
    let core_field = Field2D::new(
        ProductKey::named(field.key.var.clone()),
        field.units.clone(),
        grid,
        field.values.clone(),
    )
    .map_err(|err| err.to_string())?;

    let mut request = MapRenderRequest::from_core_field(core_field, style.scale.clone());
    rustwx_products::plot_design::StaticPlotDesign::new(
        bounds,
        ProductVisualMode::FilledMeteorology,
    )
    .apply_to_request(&mut request);
    request.apply_projected_map(projected);
    request.title = Some(style.title.clone());
    request.subtitle_left = Some(format!(
        "{} f{:03}",
        field.key.hour.run, field.key.hour.hour
    ));
    request.subtitle_right = Some(field.key.hour.model.to_ascii_uppercase());
    request.width = width;
    request.height = height;
    request.render_density = style.colormap_options.render_density;
    request.legend = style.colormap_options.legend;
    request.legend.mode = style.legend_mode;
    request.cbar_tick_step = style.cbar_tick_step;
    request.supersample_factor = 1;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    rustwx_render::save_png(&request, path).map_err(|err| err.to_string())
}

/// Finite lat/lon bounds `(west, east, south, north)` — the recipe's private
/// helper, longitudes normalized to [-180, 180).
fn geographic_bounds(lat: &[f32], lon: &[f32]) -> Option<(f64, f64, f64, f64)> {
    let mut south = f64::INFINITY;
    let mut north = f64::NEG_INFINITY;
    let mut west = f64::INFINITY;
    let mut east = f64::NEG_INFINITY;
    for (&lat, &lon) in lat.iter().zip(lon) {
        let lat = f64::from(lat);
        let lon = normalize_lon(f64::from(lon));
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        south = south.min(lat);
        north = north.max(lat);
        west = west.min(lon);
        east = east.max(lon);
    }
    if south.is_finite() && north.is_finite() && west.is_finite() && east.is_finite() {
        Some((west, east, south, north))
    } else {
        None
    }
}

fn normalize_lon(lon: f64) -> f64 {
    ((lon + 180.0).rem_euclid(360.0)) - 180.0
}

/// `<plots_base>/<model>/<run>` with filesystem-safe components.
pub(crate) fn plot_run_dir(plots_base: &Path, model: &str, run: &str) -> PathBuf {
    plots_base
        .join(sanitize_component(model))
        .join(sanitize_component(run))
}

/// Frame file name inside the variable directory: `f###.png` (three digits
/// minimum, widening naturally past f999).
fn frame_file_name(hour: u16) -> String {
    format!("f{hour:03}.png")
}

/// Filesystem-safe path component: ASCII alphanumerics, `-`, `_`, `.` pass;
/// everything else becomes `_`. Windows-hostile trailing dots/spaces are
/// trimmed and an empty result becomes `_` (two different variables CAN
/// sanitize to the same directory — store slugs are ASCII in practice, and a
/// collision only merges their index entries rather than losing files).
pub(crate) fn sanitize_component(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.ends_with('.') {
        out.pop();
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

fn write_index(run_dir: &Path, index: &PlotIndex) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(index).map_err(|err| err.to_string())?;
    std::fs::write(run_dir.join("index.json"), bytes).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{CanonicalField, FieldSelector, SelectedField2D};
    use rw_store::ingest::PressureVolumeInput;
    use rw_store::write_hour_from_fields;

    const NX: usize = 24;
    const NY: usize = 18;
    /// Written ascending on purpose (matches the iso_fields fixture): the
    /// store sorts descending; 500/700/850 are all in ISO_PICKER_LEVELS_HPA.
    const LEVELS: [u16; 3] = [500, 700, 850];

    fn cells() -> usize {
        NX * NY
    }

    /// Deterministic non-uniform plane: `base + cell index`.
    fn plane(base: f32) -> Vec<f32> {
        (0..cells()).map(|c| base + c as f32).collect()
    }

    fn grid() -> rustwx_core::LatLonGrid {
        let mut lat = Vec::with_capacity(cells());
        let mut lon = Vec::with_capacity(cells());
        for y in 0..NY {
            for x in 0..NX {
                lat.push(35.0 + 0.1 * y as f32); // south-to-north
                lon.push(-98.0 + 0.1 * x as f32);
            }
        }
        rustwx_core::LatLonGrid::new(rustwx_core::GridShape::new(NX, NY).expect("dims"), lat, lon)
            .expect("grid")
    }

    /// Write a `wrf`-slug store: per hour, a canonical `temperature_2m`
    /// (real production/Solar styling exists), a `wrf_widget` plane carried
    /// on a GeopotentialHeight surface selector (production explicitly
    /// resolves None for heights — the guaranteed generic-ramp case, same
    /// trick grib_import uses), and a `temperature_iso` sounding volume so
    /// the synthesized per-level planes render too.
    fn write_fixture(tag: &str, hours: &[u16]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("bowecho-batch-plots-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for &hour in hours {
            let t2m = SelectedField2D::new(
                FieldSelector::height_agl(CanonicalField::Temperature, 2),
                "K",
                grid(),
                plane(280.0 + f32::from(hour)),
            )
            .expect("t2m sized to the grid");
            let widget = SelectedField2D::new(
                FieldSelector::surface(CanonicalField::GeopotentialHeight),
                "widgets",
                grid(),
                plane(1.0 + f32::from(hour)),
            )
            .expect("widget sized to the grid");
            let temp_planes: Vec<(u16, Vec<f32>)> = LEVELS
                .iter()
                .map(|&level| (level, plane(f32::from(level))))
                .collect();
            let volumes = [PressureVolumeInput {
                name: "temperature_iso",
                units: "K",
                selector_template: serde_json::json!({
                    "source": "wrf",
                    "field": "temperature_iso",
                    "vertical": "isobaric",
                }),
                levels: temp_planes
                    .iter()
                    .map(|(hpa, plane)| (*hpa, plane.as_slice()))
                    .collect(),
            }];
            write_hour_from_fields(
                &root,
                "wrf",
                "local_wrf_19740403_090000",
                hour,
                &[("temperature_2m", &t2m), ("wrf_widget", &widget)],
                &volumes,
                "batch-plots-test",
                1_780_000_000,
            )
            .expect("write fixture hour");
        }
        root
    }

    fn request(root: &Path, plots_base: &Path, include_iso: bool) -> BatchPlotRequest {
        BatchPlotRequest {
            store_root: root.to_path_buf(),
            model: "wrf".to_owned(),
            run: "local_wrf_19740403_090000".to_owned(),
            plots_base: plots_base.to_path_buf(),
            overrides: StyleOverrideSettings::default(),
            options: BatchPlotOptions {
                width: 320,
                height: 240,
                include_iso,
            },
        }
    }

    fn temp_out(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bowecho-batch-plots-out-{tag}-{}",
            std::process::id()
        ))
    }

    /// Path policy: safe components, WRF-Runner-style `f###` naming, and the
    /// run/frame tree composition.
    #[test]
    fn path_policy_sanitizes_and_formats_frames() {
        assert_eq!(sanitize_component("temperature_2m"), "temperature_2m");
        assert_eq!(sanitize_component("Temp 850 mb: max*"), "Temp_850_mb__max_");
        assert_eq!(sanitize_component("run."), "run");
        assert_eq!(sanitize_component("///"), "___");
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(frame_file_name(0), "f000.png");
        assert_eq!(frame_file_name(7), "f007.png");
        assert_eq!(frame_file_name(1000), "f1000.png");
        let dir = plot_run_dir(Path::new("base"), "wrf", "run:a/b");
        assert_eq!(dir, Path::new("base").join("wrf").join("run_a_b"));
    }

    /// The style ladder: an already-resolved style stays (production bucket),
    /// a style-less canonical wrf field picks up the Solar fallback, and
    /// unknown / non-wrf fields get the generic ramp spanning their values.
    #[test]
    fn fallback_style_ladder_decides_correctly() {
        let field = |model: &str, var: &str, units: &str| FieldData {
            key: FieldKey {
                hour: HourKey {
                    model: model.to_owned(),
                    run: "r".to_owned(),
                    hour: 0,
                },
                var: var.to_owned(),
            },
            units: units.to_owned(),
            nx: 2,
            ny: 2,
            values: vec![1.0, 2.0, 3.0, 4.0],
            range: Some((1.0, 4.0)),
            grid: None,
            lat_descending: false,
            style: None,
        };
        let store_vars = vec!["temperature_2m".to_owned(), "wrf_widget".to_owned()];

        // Pre-resolved style: untouched, production bucket.
        let mut styled = field("wrf", "temperature_2m", "K");
        styled.style = rw_ui::UserColorTable::simple("t", "t", "K").to_store_style("t", "K");
        assert_eq!(
            ensure_style(&mut styled, &store_vars),
            StyleSource::Production
        );

        // Canonical wrf var without a style: Solarpower07 fallback.
        let mut solar = field("wrf", "temperature_2m", "K");
        assert_eq!(ensure_style(&mut solar, &store_vars), StyleSource::Solar);
        assert!(solar.style.is_some(), "solar fallback compiled a style");

        // Unknown wrf var: generic ramp spanning the data, titled by name.
        let mut generic = field("wrf", "wrf_widget", "widgets");
        assert_eq!(
            ensure_style(&mut generic, &store_vars),
            StyleSource::Generic
        );
        let style = generic.style.expect("generic ramp built");
        assert_eq!(style.title, "wrf_widget");
        assert_eq!(style.display_units, "widgets");

        // Non-wrf model: the Solar gate must not fire — generic.
        let mut other = field("gfs", "temperature_2m", "K");
        assert_eq!(ensure_style(&mut other, &store_vars), StyleSource::Generic);
    }

    /// Full sweep over a tiny two-hour store: every surface field and every
    /// synthesized iso plane gets a decodable, non-uniform PNG of the
    /// requested size, and index.json describes exactly what was written.
    #[test]
    fn batch_plot_renders_full_run_tree_with_index() {
        let root = write_fixture("full", &[0, 1]);
        let out = temp_out("full");
        let _ = std::fs::remove_dir_all(&out);
        let request = request(&root, &out, true);

        let mut messages = Vec::new();
        let cancel = AtomicBool::new(false);
        let summary = run_batch_plot(&request, &cancel, &mut |message| messages.push(message))
            .expect("batch plot runs");

        // 2 surface vars + 3 iso temperature planes (500/700/850), 2 hours.
        assert_eq!(summary.written, 10, "notes: {:?}", summary.notes);
        assert_eq!(summary.failed, 0, "notes: {:?}", summary.notes);
        assert_eq!(summary.skipped, 0);
        assert!(!summary.cancelled);
        // Only wrf_widget lacks any real styling (t2m resolves production or
        // Solar; the iso temperature slugs hit the Solar level tables).
        assert_eq!(summary.fallback_styled, 2);
        assert_eq!(messages.len(), 10, "one progress message per plot");

        let run_dir = plot_run_dir(&out, "wrf", "local_wrf_19740403_090000");
        assert_eq!(summary.out_dir, run_dir);
        for var in ["temperature_2m", "wrf_widget", "temperature_850"] {
            for hour in [0u16, 1] {
                let path = run_dir.join(var).join(frame_file_name(hour));
                let image = image::open(&path)
                    .unwrap_or_else(|err| panic!("decode {}: {err}", path.display()));
                let image = image.to_rgba8();
                assert_eq!((image.width(), image.height()), (320, 240), "{var} f{hour}");
                let mut min = [u8::MAX; 3];
                let mut max = [u8::MIN; 3];
                for pixel in image.pixels() {
                    for channel in 0..3 {
                        min[channel] = min[channel].min(pixel[channel]);
                        max[channel] = max[channel].max(pixel[channel]);
                    }
                }
                assert!(min != max, "{var} f{hour}: PNG is uniform color");
            }
        }

        let index: PlotIndex = serde_json::from_slice(
            &std::fs::read(run_dir.join("index.json")).expect("index.json written"),
        )
        .expect("index.json parses");
        assert_eq!(index.schema, PLOT_INDEX_SCHEMA);
        assert_eq!(index.model, "wrf");
        assert_eq!(index.run, "local_wrf_19740403_090000");
        assert_eq!(index.plots.len(), 5, "2 surface + 3 iso variables");
        let entry = |var: &str| {
            index
                .plots
                .iter()
                .find(|plot| plot.var == var)
                .unwrap_or_else(|| panic!("{var} missing from index"))
        };
        let widget = entry("wrf_widget");
        assert_eq!(widget.style, "generic");
        assert_eq!(widget.units, "widgets");
        assert_eq!(widget.frames.len(), 2);
        assert_eq!(widget.frames[0].path, "wrf_widget/f000.png");
        let t2m = entry("temperature_2m");
        assert_ne!(t2m.style, "generic", "canonical t2m has real styling");
        assert_eq!(t2m.frames.len(), 2);
        let iso = entry("temperature_850");
        assert_eq!(iso.frames.len(), 2);
        for plot in &index.plots {
            for frame in &plot.frames {
                assert!(
                    run_dir.join(&frame.path).is_file(),
                    "index path {} exists",
                    frame.path
                );
            }
        }

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The cancel flag stops the sweep between plots: cancelling after the
    /// first progress message leaves exactly one PNG and a summary that says
    /// so.
    #[test]
    fn cancel_flag_stops_mid_run() {
        let root = write_fixture("cancel", &[0]);
        let out = temp_out("cancel");
        let _ = std::fs::remove_dir_all(&out);
        let request = request(&root, &out, false);

        let cancel = AtomicBool::new(false);
        let mut seen = 0usize;
        let summary = {
            let cancel_ref = &cancel;
            run_batch_plot(&request, cancel_ref, &mut |_message| {
                seen += 1;
                cancel_ref.store(true, Ordering::Relaxed);
            })
            .expect("cancelled run still summarizes")
        };
        assert_eq!(seen, 1, "cancel took effect after the first plot");
        assert_eq!(summary.written, 1);
        assert!(summary.cancelled);
        assert!(
            summary.status_line().contains("cancelled early"),
            "status: {}",
            summary.status_line()
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&out);
    }
}
