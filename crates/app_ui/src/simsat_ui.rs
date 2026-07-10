//! Embedded SimSat pane and serialized render worker.
//!
//! SimSat's public render API is deliberately synchronous. This module keeps it off
//! the egui thread, admits only one job at a time, and reports cancellation honestly:
//! downloads may stop at a chunk boundary, while an in-flight render always finishes
//! its current frame before the worker checks the cancel flag again. Finished visible
//! and thermal frames go through SimSat's `rw-store` writer so the existing Satellite
//! player remains the one playback/display path. Raw scalar fields and rendered RGBA
//! retain their full per-pixel mesh for the native plot window.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use chrono::Utc;
use eframe::egui;
use simsat::api::{BlueMarble, FrameData, Product, RenderParams, RenderResult};
use simsat::camera::{ResolutionMode, SatellitePreset, ViewMode};
use simsat::clouds::StepQuality;
use simsat::derived::DerivedField;
use simsat::store_out::{self, IrFrame, VisibleFrame};
use simsat::wv::WvBand;

use crate::sat_plot::SatellitePlotSource;
use crate::simsat_hrrr::{HrrrNativeSpec, discover_native_files, download_native, latest_specs};

const ENGINE_CACHE_SUBDIR: &str = "engine";

/// A completion request owned by the app shell. The pane never reaches into the
/// Satellite or native-plot viewer state directly.
pub(crate) enum SimSatAction {
    /// A frame is durable in the normal satellite store. The shell refreshes the
    /// satellite worker, selects this run/time, and opens the Satellite viewer.
    SatelliteFrameWritten { key: rw_ui::SatRunKey, hhmm: u16 },
    /// Open the latest retained SimSat output in the native mesh plot window.
    OpenPlot(SatellitePlotSource),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceMode {
    Local,
    CachedHrrr,
    DownloadHrrr,
}

impl SourceMode {
    const ALL: [Self; 3] = [Self::Local, Self::CachedHrrr, Self::DownloadHrrr];

    fn label(self) -> &'static str {
        match self {
            Self::Local => "Local WRF / GRIB",
            Self::CachedHrrr => "Downloaded HRRR",
            Self::DownloadHrrr => "Download HRRR",
        }
    }
}

/// User-facing product list. Keeping this app-side enum makes routing and stable
/// store tokens explicit instead of depending on debug formatting of SimSat enums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimSatProduct {
    Visible,
    GeoColor,
    Sandwich,
    Ir13,
    Wv8,
    Wv9,
    Wv10,
    PrecipitableWater,
    CloudTopTemperature,
    CloudOpticalDepth,
}

impl SimSatProduct {
    const ALL: [Self; 10] = [
        Self::Visible,
        Self::GeoColor,
        Self::Sandwich,
        Self::Ir13,
        Self::Wv8,
        Self::Wv9,
        Self::Wv10,
        Self::PrecipitableWater,
        Self::CloudTopTemperature,
        Self::CloudOpticalDepth,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Visible => "Visible true color",
            Self::GeoColor => "GeoColor day / night",
            Self::Sandwich => "Sandwich (visible + cold-top IR)",
            Self::Ir13 => "IR 10.3 um (band 13)",
            Self::Wv8 => "Water vapor 6.2 um (band 8)",
            Self::Wv9 => "Water vapor 6.9 um (band 9)",
            Self::Wv10 => "Water vapor 7.3 um (band 10)",
            Self::PrecipitableWater => "Precipitable water",
            Self::CloudTopTemperature => "Cloud-top temperature",
            Self::CloudOpticalDepth => "Cloud optical depth",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::GeoColor => "geocolor",
            Self::Sandwich => "sandwich",
            Self::Ir13 => "ir13",
            Self::Wv8 => "wv08",
            Self::Wv9 => "wv09",
            Self::Wv10 => "wv10",
            Self::PrecipitableWater => "pw",
            Self::CloudTopTemperature => "ctt",
            Self::CloudOpticalDepth => "cod",
        }
    }

    fn api_product(self) -> Product {
        match self {
            Self::Visible => Product::VisibleRgb,
            Self::GeoColor => Product::GeoColor,
            Self::Sandwich => Product::Sandwich,
            Self::Ir13 => Product::Ir,
            Self::Wv8 => Product::WaterVapor {
                band: WvBand::Upper,
            },
            Self::Wv9 => Product::WaterVapor { band: WvBand::Mid },
            Self::Wv10 => Product::WaterVapor { band: WvBand::Low },
            Self::PrecipitableWater => Product::Derived {
                field: DerivedField::PrecipitableWater,
            },
            Self::CloudTopTemperature => Product::Derived {
                field: DerivedField::CloudTopTemp,
            },
            Self::CloudOpticalDepth => Product::Derived {
                field: DerivedField::CloudOpticalDepth,
            },
        }
    }

    fn thermal_band(self) -> Option<u8> {
        match self {
            Self::Ir13 => Some(13),
            Self::Wv8 => Some(8),
            Self::Wv9 => Some(9),
            Self::Wv10 => Some(10),
            _ => None,
        }
    }

    fn uses_visible_ground(self) -> bool {
        matches!(self, Self::Visible | Self::GeoColor | Self::Sandwich)
    }

    fn is_visible_family(self) -> bool {
        self.uses_visible_ground()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputView {
    Geostationary,
    TopDown,
}

impl OutputView {
    const ALL: [Self; 2] = [Self::Geostationary, Self::TopDown];

    fn label(self) -> &'static str {
        match self {
            Self::Geostationary => "Geostationary (from space)",
            Self::TopDown => "Top-down map",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Geostationary => "geo",
            Self::TopDown => "topdown",
        }
    }

    fn api_view(self) -> ViewMode {
        match self {
            Self::Geostationary => ViewMode::Geostationary,
            Self::TopDown => ViewMode::TopDownMap,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SatelliteChoice {
    GoesEast,
    GoesWest,
    Himawari,
}

impl SatelliteChoice {
    const ALL: [Self; 3] = [Self::GoesEast, Self::GoesWest, Self::Himawari];

    fn label(self) -> &'static str {
        match self {
            Self::GoesEast => "GOES-East",
            Self::GoesWest => "GOES-West",
            Self::Himawari => "Himawari",
        }
    }

    fn api_satellite(self) -> SatellitePreset {
        match self {
            Self::GoesEast => SatellitePreset::GoesEast,
            Self::GoesWest => SatellitePreset::GoesWest,
            Self::Himawari => SatellitePreset::Himawari,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderQuality {
    Final,
    Preview,
}

impl RenderQuality {
    fn label(self) -> &'static str {
        match self {
            Self::Final => "Final (384 steps)",
            Self::Preview => "Preview (192 steps)",
        }
    }

    fn steps(self) -> StepQuality {
        match self {
            Self::Final => StepQuality::Offline,
            Self::Preview => StepQuality::Interactive,
        }
    }
}

#[derive(Clone, Debug)]
enum JobSource {
    Local(PathBuf),
    CachedHrrr {
        path: PathBuf,
        spec: Option<HrrrNativeSpec>,
    },
    DownloadHrrr {
        spec: HrrrNativeSpec,
        root: PathBuf,
    },
}

impl JobSource {
    fn group_base(&self) -> String {
        match self {
            Self::Local(path) => source_group_base(path),
            Self::CachedHrrr {
                spec: Some(spec), ..
            } => format!("hrrr_{}_t{:02}z", spec.date, spec.cycle),
            Self::CachedHrrr {
                path, spec: None, ..
            } => source_group_base(path),
            Self::DownloadHrrr { spec, .. } => {
                format!("hrrr_{}_t{:02}z", spec.date, spec.cycle)
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RenderJob {
    source: JobSource,
    product: SimSatProduct,
    view: OutputView,
    satellite: SatelliteChoice,
    quality: RenderQuality,
    margin_frac: f32,
    granulation: bool,
    bluemarble_download: bool,
    cache_root: PathBuf,
    store_root: PathBuf,
    sector: String,
}

#[derive(Clone, Debug)]
struct FrameInput {
    path: PathBuf,
    timestep: usize,
    sort_key: String,
    label: String,
}

#[derive(Clone, Debug)]
struct StoredFrame {
    key: rw_ui::SatRunKey,
    hhmm: u16,
}

#[derive(Clone, Debug, PartialEq)]
enum PlotPixels {
    Scalar { values: Vec<f32>, units: String },
    Rgba(Vec<u8>),
}

/// Worker-safe, constructor-neutral plot data. `SatellitePlotSource` is created on
/// the UI thread only when the user opens it.
#[derive(Clone, Debug, PartialEq)]
struct PlotPayload {
    title: String,
    subtitle_left: String,
    subtitle_right: String,
    nx: usize,
    ny: usize,
    lat: Vec<f32>,
    lon: Vec<f32>,
    pixels: PlotPixels,
}

impl PlotPayload {
    fn to_plot_source(&self) -> Result<SatellitePlotSource, String> {
        match &self.pixels {
            PlotPixels::Scalar { values, units } => SatellitePlotSource::scalar_from_mesh(
                self.title.clone(),
                self.subtitle_left.clone(),
                self.subtitle_right.clone(),
                units.clone(),
                self.nx,
                self.ny,
                values.clone(),
                self.lat.clone(),
                self.lon.clone(),
                None,
            ),
            PlotPixels::Rgba(rgba) => {
                let pixels_len = self
                    .nx
                    .checked_mul(self.ny)
                    .ok_or_else(|| "SimSat RGBA dimensions overflow.".to_owned())?;
                let bytes_len = pixels_len
                    .checked_mul(4)
                    .ok_or_else(|| "SimSat RGBA byte count overflows.".to_owned())?;
                if rgba.len() != bytes_len {
                    return Err(format!(
                        "SimSat RGBA has {} bytes, expected {bytes_len}",
                        rgba.len()
                    ));
                }
                let pixels = rgba
                    .chunks_exact(4)
                    .map(|px| rustwx_render::Color::rgba(px[0], px[1], px[2], px[3]))
                    .collect();
                SatellitePlotSource::rgba_from_mesh(
                    self.title.clone(),
                    self.subtitle_left.clone(),
                    self.subtitle_right.clone(),
                    self.nx,
                    self.ny,
                    pixels,
                    self.lat.clone(),
                    self.lon.clone(),
                    None,
                )
            }
        }
    }
}

enum WorkerEvent {
    Started {
        total: usize,
    },
    Progress {
        index: usize,
        total: usize,
        message: String,
    },
    FrameComplete {
        label: String,
        plot: Box<PlotPayload>,
        stored: Option<StoredFrame>,
        store_error: Option<String>,
        warning: Option<String>,
    },
    FrameFailed {
        label: String,
        error: String,
    },
    Finished {
        completed: usize,
        failed: usize,
        cancelled: bool,
    },
    Fatal(String),
}

struct RenderTask {
    rx: Receiver<WorkerEvent>,
    cancel: Arc<AtomicBool>,
}

/// State for the docked/floating SimSat pane. It owns no Satellite or plot viewer
/// internals; all cross-window effects leave as [`SimSatAction`].
pub(crate) struct SimSatPane {
    source_mode: SourceMode,
    local_path: String,
    cached_hrrr: Vec<crate::simsat_hrrr::CachedNativeInput>,
    cached_selected: usize,
    cached_loaded: bool,
    hrrr_date: String,
    hrrr_cycle: u8,
    hrrr_forecast_hour: u16,
    product: SimSatProduct,
    view: OutputView,
    satellite: SatelliteChoice,
    quality: RenderQuality,
    margin_frac: f32,
    granulation: bool,
    bluemarble_download: bool,
    task: Option<RenderTask>,
    status: String,
    error: Option<String>,
    total: usize,
    completed: usize,
    failed: usize,
    cancellation_requested: bool,
    last_plot: Option<PlotPayload>,
    last_plot_label: Option<String>,
    last_stored: Option<StoredFrame>,
}

impl Default for SimSatPane {
    fn default() -> Self {
        let now = Utc::now();
        let fallback_date = now.format("%Y%m%d").to_string();
        let fallback_cycle = now.format("%H").to_string().parse().unwrap_or(0);
        let latest = latest_specs(now, 0).into_iter().next();
        let (date, cycle) = latest
            .map(|spec| (spec.date, spec.cycle))
            .unwrap_or((fallback_date, fallback_cycle));
        Self {
            source_mode: SourceMode::Local,
            local_path: String::new(),
            cached_hrrr: Vec::new(),
            cached_selected: 0,
            cached_loaded: false,
            hrrr_date: date,
            hrrr_cycle: cycle,
            hrrr_forecast_hour: 0,
            product: SimSatProduct::Visible,
            view: OutputView::Geostationary,
            satellite: SatelliteChoice::GoesEast,
            quality: RenderQuality::Final,
            margin_frac: 0.0,
            granulation: true,
            bluemarble_download: true,
            task: None,
            status: "Choose a WRF/GRIB source or an HRRR native-level file.".to_owned(),
            error: None,
            total: 0,
            completed: 0,
            failed: 0,
            cancellation_requested: false,
            last_plot: None,
            last_plot_label: None,
            last_stored: None,
        }
    }
}

impl SimSatPane {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.task.is_some()
    }

    /// Poll even while the pane is hidden so long renders/downloads finish and the
    /// Satellite store refresh is never gated on window visibility.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> Vec<SimSatAction> {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(task) = &self.task {
            loop {
                match task.rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        let mut actions = Vec::new();
        let mut terminal = false;
        for event in events {
            match event {
                WorkerEvent::Started { total } => {
                    self.total = total;
                    self.status = format!("Discovered {total} frame(s). Starting render...");
                }
                WorkerEvent::Progress {
                    index,
                    total,
                    message,
                } => {
                    self.total = total;
                    self.status = format!("Frame {index}/{total}: {message}");
                }
                WorkerEvent::FrameComplete {
                    label,
                    plot,
                    stored,
                    store_error,
                    warning,
                } => {
                    self.completed += 1;
                    self.last_plot = Some(*plot);
                    self.last_plot_label = Some(label.clone());
                    if let Some(stored) = stored {
                        self.last_stored = Some(stored);
                    }
                    if let Some(err) = store_error {
                        self.failed += 1;
                        self.error = Some(format!("{label}: satellite-store write failed: {err}"));
                    } else if let Some(warning) = warning {
                        self.status = format!("{label}: {warning}");
                    } else {
                        self.status = format!("Rendered {label}.");
                    }
                }
                WorkerEvent::FrameFailed { label, error } => {
                    self.failed += 1;
                    self.error = Some(format!("{label}: {error}"));
                    self.status = format!("Skipped failed frame {label}; continuing.");
                }
                WorkerEvent::Finished {
                    completed,
                    failed,
                    cancelled,
                } => {
                    self.completed = completed;
                    self.failed = failed;
                    self.status = if cancelled {
                        format!(
                            "Cancelled at a frame boundary: {completed} complete, {failed} failed."
                        )
                    } else if failed > 0 {
                        format!("Finished: {completed} complete, {failed} failed.")
                    } else {
                        format!("Finished {completed} frame(s).")
                    };
                    terminal = true;
                }
                WorkerEvent::Fatal(error) => {
                    self.error = Some(error);
                    self.status = "SimSat job failed.".to_owned();
                    terminal = true;
                }
            }
        }

        if disconnected && !terminal && self.task.is_some() {
            self.error = Some("SimSat worker exited without a completion message.".to_owned());
            self.status = "SimSat worker stopped unexpectedly.".to_owned();
            terminal = true;
        }
        if terminal {
            if let Some(stored) = self.last_stored.clone() {
                actions.push(SimSatAction::SatelliteFrameWritten {
                    key: stored.key,
                    hhmm: stored.hhmm,
                });
            } else if let Some(plot) = &self.last_plot {
                match plot.to_plot_source() {
                    Ok(source) => actions.push(SimSatAction::OpenPlot(source)),
                    Err(error) => {
                        let plot_error =
                            format!("Rendered output could not open in the native plot: {error}");
                        self.error = Some(match self.error.take() {
                            Some(existing) => format!("{existing}\n{plot_error}"),
                            None => plot_error,
                        });
                    }
                }
            }
            self.task = None;
            self.cancellation_requested = false;
        }
        if self.task.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        actions
    }

    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) -> Vec<SimSatAction> {
        let mut actions = self.poll(ui.ctx());
        if !self.cached_loaded {
            self.refresh_cached_hrrr();
        }

        ui.heading("SimSat");
        ui.label(
            "Render physically based simulated satellite imagery from WRF or HRRR native levels. \
             Visible, IR, and water-vapor products land in BowEcho's normal satellite player; \
             derived scalar fields open in the native plot.",
        );
        ui.add_space(6.0);

        ui.group(|ui| {
            ui.strong("Source");
            ui.horizontal_wrapped(|ui| {
                for mode in SourceMode::ALL {
                    ui.radio_value(&mut self.source_mode, mode, mode.label());
                }
            });
            ui.add_space(4.0);
            match self.source_mode {
                SourceMode::Local => self.local_source_ui(ui),
                SourceMode::CachedHrrr => self.cached_hrrr_ui(ui),
                SourceMode::DownloadHrrr => self.download_hrrr_ui(ui),
            }
        });

        ui.add_space(6.0);
        ui.group(|ui| {
            ui.strong("Product and view");
            egui::Grid::new("simsat-product-view-grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Product");
                    egui::ComboBox::from_id_salt("simsat-product")
                        .selected_text(self.product.label())
                        .show_ui(ui, |ui| {
                            for product in SimSatProduct::ALL {
                                ui.selectable_value(&mut self.product, product, product.label());
                            }
                        });
                    ui.end_row();

                    ui.label("View");
                    egui::ComboBox::from_id_salt("simsat-view")
                        .selected_text(self.view.label())
                        .show_ui(ui, |ui| {
                            for view in OutputView::ALL {
                                ui.selectable_value(&mut self.view, view, view.label());
                            }
                        });
                    ui.end_row();

                    ui.label("Satellite");
                    ui.add_enabled_ui(self.view == OutputView::Geostationary, |ui| {
                        egui::ComboBox::from_id_salt("simsat-satellite")
                            .selected_text(self.satellite.label())
                            .show_ui(ui, |ui| {
                                for satellite in SatelliteChoice::ALL {
                                    ui.selectable_value(
                                        &mut self.satellite,
                                        satellite,
                                        satellite.label(),
                                    );
                                }
                            });
                    });
                    ui.end_row();
                });
            if self.view == OutputView::TopDown {
                ui.small(
                    "Top-down is map-registered; the satellite choice is not used by the camera.",
                );
            }
        });

        ui.add_space(6.0);
        egui::CollapsingHeader::new("Render controls")
            .id_salt("simsat-render-controls")
            .show(ui, |ui| {
                egui::ComboBox::from_id_salt("simsat-quality")
                    .selected_text(self.quality.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.quality,
                            RenderQuality::Final,
                            RenderQuality::Final.label(),
                        );
                        ui.selectable_value(
                            &mut self.quality,
                            RenderQuality::Preview,
                            RenderQuality::Preview.label(),
                        );
                    });
                ui.add(
                    egui::Slider::new(&mut self.margin_frac, 0.0..=0.75)
                        .text("earth margin")
                        .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
                );
                if self.product.uses_visible_ground() {
                    ui.checkbox(
                        &mut self.bluemarble_download,
                        "Download missing 2 km Blue Marble months",
                    );
                    ui.checkbox(&mut self.granulation, "Sub-grid cloud granulation (experimental)");
                }
                ui.small(
                    "Final/stored frames use the CPU path. First use ingests a reusable SimSat brick; \
                     a full HRRR native file can briefly require more than 2 GB of memory.",
                );
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if self.is_busy() {
                ui.spinner();
                if ui
                    .add_enabled(
                        !self.cancellation_requested,
                        egui::Button::new("Cancel after current frame"),
                    )
                    .clicked()
                {
                    self.request_cancel();
                }
            } else if ui.button("Render").clicked() {
                self.start_current_job(ui.ctx());
            }
            if self.total > 0 {
                let progress = self.completed.min(self.total) as f32 / self.total as f32;
                ui.add(
                    egui::ProgressBar::new(progress)
                        .desired_width(180.0)
                        .text(format!("{}/{}", self.completed, self.total)),
                );
            }
        });
        ui.label(&self.status);
        if self.cancellation_requested {
            ui.small(
                "Cancellation requested. A download may stop between chunks; an active render \
                 finishes this frame before the sequence stops.",
            );
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), error);
        }

        if let Some(label) = &self.last_plot_label {
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.strong(format!("Latest: {label}"));
                if ui
                    .add_enabled(
                        self.last_plot.is_some(),
                        egui::Button::new("Open native plot"),
                    )
                    .clicked()
                    && let Some(payload) = &self.last_plot
                {
                    match payload.to_plot_source() {
                        Ok(source) => actions.push(SimSatAction::OpenPlot(source)),
                        Err(error) => self.error = Some(format!("Could not open plot: {error}")),
                    }
                }
            });
            if let Some(stored) = &self.last_stored {
                ui.small(format!(
                    "Satellite frame: {} t{:04}",
                    stored.key, stored.hhmm
                ));
            }
        }

        actions
    }

    fn local_source_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.local_path)
                    .desired_width(360.0)
                    .hint_text("wrfout / GRIB2 file, SimSat run.json, or folder"),
            );
            #[cfg(any(windows, target_os = "macos"))]
            {
                if ui.button("File...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("Open WRF / HRRR input")
                        .add_filter(
                            "WRF / GRIB",
                            &["nc", "grib2", "grb2", "grib", "grb", "json"],
                        )
                        .pick_file()
                {
                    self.local_path = path.display().to_string();
                }
                if ui.button("Folder...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("Open WRF / HRRR sequence folder")
                        .pick_folder()
                {
                    self.local_path = path.display().to_string();
                }
            }
        });
        ui.small(
            "Folders are probed and sorted by valid time. HRRR requires wrfnat native-level \
             GRIB2; pressure/surface products cannot drive SimSat.",
        );
    }

    fn cached_hrrr_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let selected = self
                .cached_hrrr
                .get(self.cached_selected)
                .map(|entry| entry.label())
                .unwrap_or_else(|| "No cached wrfnat files".to_owned());
            egui::ComboBox::from_id_salt("simsat-cached-hrrr")
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for (index, entry) in self.cached_hrrr.iter().enumerate() {
                        ui.selectable_value(&mut self.cached_selected, index, entry.label());
                    }
                });
            if ui.button("Refresh").clicked() {
                self.refresh_cached_hrrr();
            }
        });
        ui.small(format!("SimSat inputs: {}", hrrr_input_dir().display()));
        ui.small(format!(
            "Also checks BowEcho's model cache: {}",
            settings::model_cache_dir().display()
        ));
    }

    fn download_hrrr_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Date");
            ui.add(
                egui::TextEdit::singleline(&mut self.hrrr_date)
                    .desired_width(90.0)
                    .hint_text("YYYYMMDD"),
            );
            ui.label("Cycle");
            ui.add(
                egui::DragValue::new(&mut self.hrrr_cycle)
                    .range(0..=23)
                    .suffix("z"),
            );
            ui.label("Forecast hour");
            ui.add(
                egui::DragValue::new(&mut self.hrrr_forecast_hour)
                    .range(0..=48)
                    .prefix("f"),
            );
            if ui.button("Latest candidate").clicked()
                && let Some(spec) = latest_specs(Utc::now(), self.hrrr_forecast_hour)
                    .into_iter()
                    .next()
            {
                self.hrrr_date = spec.date;
                self.hrrr_cycle = spec.cycle;
            }
        });
        ui.small(
            "Render downloads the selected HRRR wrfnat file first, with resumable cache reuse. \
             The pressure/surface model downloads remain separate.",
        );
    }

    fn refresh_cached_hrrr(&mut self) {
        self.cached_hrrr = discover_cached_hrrr();
        self.cached_selected = self
            .cached_selected
            .min(self.cached_hrrr.len().saturating_sub(1));
        self.cached_loaded = true;
    }

    fn selected_job_source(&self) -> Result<JobSource, String> {
        match self.source_mode {
            SourceMode::Local => {
                let trimmed = self.local_path.trim();
                if trimmed.is_empty() {
                    return Err("Choose a WRF/GRIB file or folder first.".to_owned());
                }
                let path = PathBuf::from(trimmed);
                if !path.exists() {
                    return Err(format!("Source does not exist: {}", path.display()));
                }
                Ok(JobSource::Local(path))
            }
            SourceMode::CachedHrrr => self
                .cached_hrrr
                .get(self.cached_selected)
                .map(|entry| JobSource::CachedHrrr {
                    path: entry.path.clone(),
                    spec: entry.spec.clone(),
                })
                .ok_or_else(|| "No cached HRRR wrfnat file is selected.".to_owned()),
            SourceMode::DownloadHrrr => {
                let spec = HrrrNativeSpec::new(
                    self.hrrr_date.trim().to_owned(),
                    self.hrrr_cycle,
                    self.hrrr_forecast_hour,
                )
                .map_err(|error| error.to_string())?;
                spec.validate().map_err(|error| error.to_string())?;
                Ok(JobSource::DownloadHrrr {
                    spec,
                    root: hrrr_input_dir(),
                })
            }
        }
    }

    fn start_current_job(&mut self, ctx: &egui::Context) {
        if self.task.is_some() {
            return;
        }
        let source = match self.selected_job_source() {
            Ok(source) => source,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let cache_root = settings::simsat_cache_dir().join(ENGINE_CACHE_SUBDIR);
        let store_root = settings::sat_store_dir();
        let sector = qualified_sector(&source.group_base(), self.product, self.view);
        let job = RenderJob {
            source,
            product: self.product,
            view: self.view,
            satellite: if self.view == OutputView::TopDown {
                SatelliteChoice::GoesEast
            } else {
                self.satellite
            },
            quality: self.quality,
            margin_frac: self.margin_frac,
            granulation: self.granulation,
            bluemarble_download: self.bluemarble_download,
            cache_root,
            store_root,
            sector,
        };
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let repaint = ctx.clone();
        let panic_tx = tx.clone();
        match std::thread::Builder::new()
            .name("bowecho-simsat".to_owned())
            .spawn(move || {
                simsat::platform::lower_worker_thread_priority();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_render_job(job, cancel_worker, tx, &repaint)
                }));
                if outcome.is_err() {
                    let _ = panic_tx.send(WorkerEvent::Fatal(
                        "SimSat worker panicked; no further frames were attempted.".to_owned(),
                    ));
                    repaint.request_repaint();
                }
            }) {
            Ok(_) => {
                self.task = Some(RenderTask { rx, cancel });
                self.total = 0;
                self.completed = 0;
                self.failed = 0;
                self.cancellation_requested = false;
                self.last_plot = None;
                self.last_plot_label = None;
                self.last_stored = None;
                self.error = None;
                self.status = "Preparing SimSat job...".to_owned();
                ctx.request_repaint();
            }
            Err(error) => {
                self.error = Some(format!("Could not start SimSat worker: {error}"));
            }
        }
    }

    fn request_cancel(&mut self) {
        if let Some(task) = &self.task {
            task.cancel.store(true, Ordering::Relaxed);
            self.cancellation_requested = true;
            self.status =
                "Cancellation requested; stopping the download or finishing the current frame."
                    .to_owned();
        }
    }
}

fn hrrr_input_dir() -> PathBuf {
    settings::simsat_input_dir()
}

fn discover_cached_hrrr() -> Vec<crate::simsat_hrrr::CachedNativeInput> {
    let mut inputs = discover_native_files(&hrrr_input_dir());
    inputs.extend(discover_native_files(&settings::model_cache_dir()));

    let mut seen = HashSet::new();
    inputs.retain(|input| seen.insert(input.path.clone()));
    inputs.sort_by(
        |left_input, right_input| match (&left_input.spec, &right_input.spec) {
            (Some(left_spec), Some(right_spec)) => right_spec
                .cmp(left_spec)
                .then_with(|| left_input.path.cmp(&right_input.path)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left_input.path.cmp(&right_input.path),
        },
    );
    inputs
}

fn run_render_job(
    job: RenderJob,
    cancel: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<WorkerEvent>,
    repaint: &egui::Context,
) {
    if let Err(error) = std::fs::create_dir_all(&job.cache_root) {
        let _ = tx.send(WorkerEvent::Fatal(format!(
            "Could not create SimSat cache {}: {error}",
            job.cache_root.display()
        )));
        repaint.request_repaint();
        return;
    }
    if let Err(error) = std::fs::create_dir_all(&job.store_root) {
        let _ = tx.send(WorkerEvent::Fatal(format!(
            "Could not create satellite store {}: {error}",
            job.store_root.display()
        )));
        repaint.request_repaint();
        return;
    }

    let source_path = match &job.source {
        JobSource::DownloadHrrr { spec, root } => {
            let _ = tx.send(WorkerEvent::Progress {
                index: 0,
                total: 0,
                message: format!("Downloading {}...", spec.filename()),
            });
            repaint.request_repaint();
            match download_native(spec, root, cancel.as_ref()) {
                Ok(outcome) if outcome.is_ready() => outcome.path,
                Ok(outcome) if outcome.is_cancelled() => {
                    let _ = tx.send(WorkerEvent::Finished {
                        completed: 0,
                        failed: 0,
                        cancelled: true,
                    });
                    repaint.request_repaint();
                    return;
                }
                Ok(outcome) => {
                    let _ = tx.send(WorkerEvent::Fatal(format!(
                        "HRRR download did not produce a ready file: {}",
                        outcome.path.display()
                    )));
                    repaint.request_repaint();
                    return;
                }
                Err(error) => {
                    let _ = tx.send(WorkerEvent::Fatal(format!("HRRR download failed: {error}")));
                    repaint.request_repaint();
                    return;
                }
            }
        }
        JobSource::Local(path) | JobSource::CachedHrrr { path, .. } => path.clone(),
    };

    if cancel.load(Ordering::Relaxed) {
        let _ = tx.send(WorkerEvent::Finished {
            completed: 0,
            failed: 0,
            cancelled: true,
        });
        repaint.request_repaint();
        return;
    }

    let mut inputs = match discover_frame_inputs(&source_path) {
        Ok(inputs) => inputs,
        Err(error) => {
            let _ = tx.send(WorkerEvent::Fatal(error));
            repaint.request_repaint();
            return;
        }
    };
    sort_frame_inputs(&mut inputs);
    let total = inputs.len();
    let _ = tx.send(WorkerEvent::Started { total });
    repaint.request_repaint();
    let mut completed = 0usize;
    let mut failed = 0usize;
    let mut cancelled = false;

    for (ordinal, frame) in inputs.into_iter().enumerate() {
        // The only render cancellation point: the previous frame is fully rendered and,
        // where applicable, durably stored before this check.
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let index = ordinal + 1;
        let _ = tx.send(WorkerEvent::Progress {
            index,
            total,
            message: format!("Rendering {}", frame.label),
        });
        repaint.request_repaint();

        let params = render_params_for(&job, &frame);
        match simsat::api::render(&params, job.product.api_product()) {
            Ok(result) => {
                let store = write_result_to_store(&job, &result);
                let warning = result_warning(&result);
                match plot_payload_from_result(job.product, &frame.label, result) {
                    Ok(plot) => {
                        let (stored, store_error) = match store {
                            Ok(stored) => (stored, None),
                            Err(error) => {
                                failed += 1;
                                (None, Some(error))
                            }
                        };
                        completed += 1;
                        let _ = tx.send(WorkerEvent::FrameComplete {
                            label: frame.label,
                            plot: Box::new(plot),
                            stored,
                            store_error,
                            warning,
                        });
                    }
                    Err(error) => {
                        failed += 1;
                        let _ = tx.send(WorkerEvent::FrameFailed {
                            label: frame.label,
                            error,
                        });
                    }
                }
            }
            Err(error) => {
                failed += 1;
                let _ = tx.send(WorkerEvent::FrameFailed {
                    label: frame.label,
                    error,
                });
            }
        }
        repaint.request_repaint();
    }

    let _ = tx.send(WorkerEvent::Finished {
        completed,
        failed,
        cancelled,
    });
    repaint.request_repaint();
}

fn render_params_for(job: &RenderJob, frame: &FrameInput) -> RenderParams {
    let mut params = RenderParams::new(frame.path.clone());
    params.timestep = frame.timestep;
    params.cache = job.cache_root.clone();
    params.satellite = job.satellite.api_satellite();
    params.view = job.view.api_view();
    params.resolution = ResolutionMode::Native;
    params.margin_frac = job.margin_frac;
    params.steps = job.quality.steps();
    params.multiscatter = true;
    params.clouds = true;
    params.granulation = Some(job.granulation && job.product.uses_visible_ground());
    params.derived_colormap = false;
    params.ir_enhancement = None;
    params.bluemarble = if job.product.uses_visible_ground() {
        BlueMarble::Seasonal {
            month_override: None,
            download: job.bluemarble_download,
        }
    } else {
        BlueMarble::FlatAlbedo
    };
    params
}

fn discover_frame_inputs(path: &Path) -> Result<Vec<FrameInput>, String> {
    if path.is_file() {
        return probe_source_file(path);
    }
    if !path.is_dir() {
        return Err(format!(
            "SimSat source is neither a file nor a folder: {}",
            path.display()
        ));
    }
    let manifest = path.join("run.json");
    if manifest.is_file() {
        return probe_source_file(&manifest);
    }
    let mut inputs = Vec::new();
    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("Could not read source folder {}: {error}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("Could not read source entry: {error}"))?;
        let candidate = entry.path();
        if !candidate.is_file() || !looks_like_sequence_source(&candidate) {
            continue;
        }
        // A mixed folder may contain unrelated files. A bad candidate is skipped; if
        // every candidate fails, the final empty-input error remains actionable.
        if let Ok(mut found) = probe_source_file(&candidate) {
            inputs.append(&mut found);
        }
    }
    if inputs.is_empty() {
        return Err(format!(
            "No readable wrfout or native-level GRIB2 frames found in {}",
            path.display()
        ));
    }
    Ok(inputs)
}

fn looks_like_sequence_source(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.starts_with("wrfout")
        || name.contains("wrfnat")
        || matches!(ext.as_str(), "nc" | "grib2" | "grb2" | "grib" | "grb")
}

fn probe_source_file(path: &Path) -> Result<Vec<FrameInput>, String> {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        let manifest = simsat::bricks::RunManifest::load(path)
            .map_err(|error| format!("Invalid SimSat run.json {}: {error}", path.display()))?;
        if manifest.timesteps.is_empty() {
            return Err(format!("SimSat run has no timesteps: {}", path.display()));
        }
        return Ok(manifest
            .timesteps
            .iter()
            .enumerate()
            .map(|(timestep, entry)| {
                let key = entry.time_iso.clone().unwrap_or_else(|| entry.key.clone());
                FrameInput {
                    path: path.to_owned(),
                    timestep,
                    sort_key: key.clone(),
                    label: key,
                }
            })
            .collect());
    }

    if simsat::ingest_grib::is_grib_input(path) {
        let probe = simsat::ingest_grib::probe_grib(path)
            .map_err(|error| format!("GRIB probe failed for {}: {error}", path.display()))?;
        return Ok(vec![FrameInput {
            path: path.to_owned(),
            timestep: 0,
            sort_key: probe.time_iso.clone(),
            label: probe.time_iso,
        }]);
    }

    let probe = simsat::ingest::probe_wrf(path)
        .map_err(|error| format!("WRF probe failed for {}: {error}", path.display()))?;
    let count = probe.nt.max(1);
    let fallback = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("wrfout")
        .to_owned();
    Ok((0..count)
        .map(|timestep| {
            let valid = probe
                .times
                .get(timestep)
                .cloned()
                .unwrap_or_else(|| format!("{fallback} t{timestep:04}"));
            FrameInput {
                path: path.to_owned(),
                timestep,
                sort_key: valid.clone(),
                label: valid,
            }
        })
        .collect())
}

fn sort_frame_inputs(inputs: &mut [FrameInput]) {
    inputs.sort_by(|left, right| {
        left.sort_key
            .cmp(&right.sort_key)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.timestep.cmp(&right.timestep))
    });
}

fn write_result_to_store(
    job: &RenderJob,
    result: &RenderResult,
) -> Result<Option<StoredFrame>, String> {
    let hhmm = frame_hhmm(result);
    let common = (
        result.nx,
        result.ny,
        result.georef.lat.clone(),
        result.georef.lon.clone(),
        job.sector.clone(),
        job.satellite.api_satellite(),
        result.time.year,
        result.time.month,
        result.time.day,
        hhmm,
    );
    let written = if job.product.is_visible_family() {
        let FrameData::Visible { rgba, .. } = &result.data else {
            return Err("SimSat returned the wrong frame type for a visible product.".to_owned());
        };
        let n = result
            .nx
            .checked_mul(result.ny)
            .ok_or_else(|| "visible frame dimensions overflow.".to_owned())?;
        if rgba.len() != n * 4 {
            return Err(format!(
                "visible RGBA has {} bytes, expected {}",
                rgba.len(),
                n * 4
            ));
        }
        let mut rgb_r = vec![f32::NAN; n];
        let mut rgb_g = vec![f32::NAN; n];
        let mut rgb_b = vec![f32::NAN; n];
        for (index, pixel) in rgba.chunks_exact(4).enumerate() {
            if pixel[3] >= 128 {
                rgb_r[index] = pixel[0] as f32;
                rgb_g[index] = pixel[1] as f32;
                rgb_b[index] = pixel[2] as f32;
            }
        }
        let frame = VisibleFrame {
            nx: common.0,
            ny: common.1,
            rgb_r,
            rgb_g,
            rgb_b,
            lat: common.2,
            lon: common.3,
            sector: common.4,
            satellite: common.5,
            band: 2,
            year: common.6,
            month: common.7,
            day: common.8,
            hhmm: common.9,
        };
        Some(store_out::write_visible_frame(&job.store_root, &frame)?)
    } else if let Some(band) = job.product.thermal_band() {
        let FrameData::Ir { bt_kelvin, .. } = &result.data else {
            return Err("SimSat returned the wrong frame type for a thermal product.".to_owned());
        };
        let frame = IrFrame::new_band(
            common.0,
            common.1,
            bt_kelvin.clone(),
            common.2,
            common.3,
            common.4,
            common.5,
            band,
            common.6,
            common.7,
            common.8,
            common.9,
        );
        Some(store_out::write_ir_frame(&job.store_root, &frame)?)
    } else {
        None
    };
    Ok(written.map(|written| StoredFrame {
        key: rw_ui::SatRunKey {
            model: written.model,
            run: written.run,
        },
        hhmm: written.hhmm,
    }))
}

fn plot_payload_from_result(
    product: SimSatProduct,
    source_label: &str,
    result: RenderResult,
) -> Result<PlotPayload, String> {
    let subtitle_right = format!(
        "{:04}-{:02}-{:02} {:04} UTC · {}",
        result.time.year,
        result.time.month,
        result.time.day,
        frame_hhmm(&result),
        result.georef.view.slug()
    );
    plot_payload_from_parts(
        product,
        source_label.to_owned(),
        subtitle_right,
        result.nx,
        result.ny,
        result.data,
        result.georef.lat,
        result.georef.lon,
    )
}

#[allow(clippy::too_many_arguments)]
fn plot_payload_from_parts(
    product: SimSatProduct,
    subtitle_left: String,
    subtitle_right: String,
    nx: usize,
    ny: usize,
    data: FrameData,
    lat: Vec<f32>,
    lon: Vec<f32>,
) -> Result<PlotPayload, String> {
    let expected = nx
        .checked_mul(ny)
        .ok_or_else(|| "SimSat plot dimensions overflow.".to_owned())?;
    let expected_rgba = expected
        .checked_mul(4)
        .ok_or_else(|| "SimSat RGBA byte count overflows.".to_owned())?;
    if lat.len() != expected || lon.len() != expected {
        return Err(format!(
            "SimSat georef mesh is {} / {} values, expected {expected}",
            lat.len(),
            lon.len()
        ));
    }
    let pixels = match data {
        FrameData::Visible { rgba, .. } if product.is_visible_family() => {
            if rgba.len() != expected_rgba {
                return Err(format!(
                    "SimSat RGBA has {} bytes, expected {}",
                    rgba.len(),
                    expected_rgba
                ));
            }
            PlotPixels::Rgba(rgba)
        }
        FrameData::Ir { bt_kelvin, .. } if product.thermal_band().is_some() => {
            if bt_kelvin.len() != expected {
                return Err(format!(
                    "SimSat thermal field has {} values, expected {expected}",
                    bt_kelvin.len()
                ));
            }
            PlotPixels::Scalar {
                values: bt_kelvin,
                units: "K".to_owned(),
            }
        }
        FrameData::Scalar { values, field, .. }
            if matches!(product.api_product(), Product::Derived { .. }) =>
        {
            if values.len() != expected {
                return Err(format!(
                    "SimSat scalar field has {} values, expected {expected}",
                    values.len()
                ));
            }
            PlotPixels::Scalar {
                values,
                units: field.units().to_owned(),
            }
        }
        _ => {
            return Err(
                "SimSat returned a frame type that does not match the requested product."
                    .to_owned(),
            );
        }
    };
    Ok(PlotPayload {
        title: product.label().to_owned(),
        subtitle_left,
        subtitle_right,
        nx,
        ny,
        lat,
        lon,
        pixels,
    })
}

fn result_warning(result: &RenderResult) -> Option<String> {
    if result.time_is_fallback {
        return Some(
            "source had no parseable valid time; SimSat used its documented fallback date"
                .to_owned(),
        );
    }
    if !result.ground_status.is_empty() {
        return Some(result.ground_status.join(" · "));
    }
    None
}

fn frame_hhmm(result: &RenderResult) -> u16 {
    let total_minutes = (result.time.ut * 60.0).round().clamp(0.0, 1439.0) as u16;
    (total_minutes / 60) * 100 + total_minutes % 60
}

/// One sector for the whole job. Product and view are always in the key, preventing
/// plain-visible/GeoColor/Sandwich overwrites at the same valid time.
fn qualified_sector(base: &str, product: SimSatProduct, view: OutputView) -> String {
    store_out::sanitize_store_token(&format!("{}_{}_{}", base, product.slug(), view.slug()))
}

fn source_group_base(path: &Path) -> String {
    if path.is_dir() {
        return path
            .file_name()
            .and_then(|value| value.to_str())
            .map(store_out::sanitize_store_token)
            .unwrap_or_else(|| "simsat".to_owned());
    }
    let raw = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("simsat");
    let lower = raw.to_ascii_lowercase();
    let cut = lower
        .find(".wrfnatf")
        .or_else(|| lower.find("_wrfnatf"))
        .or_else(|| lower.find("wrfnatf"))
        .or_else(|| lower.find("_20"))
        .or_else(|| lower.find("-20"))
        .unwrap_or(raw.len());
    let trimmed = raw[..cut]
        .trim_end_matches(['.', '_', '-'])
        .trim_end_matches("wrfout");
    let token = store_out::sanitize_store_token(trimmed);
    if token == "unknown" {
        "simsat".to_owned()
    } else {
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_maps_every_public_product_to_the_expected_family() {
        assert!(matches!(
            SimSatProduct::Visible.api_product(),
            Product::VisibleRgb
        ));
        assert!(matches!(
            SimSatProduct::GeoColor.api_product(),
            Product::GeoColor
        ));
        assert!(matches!(
            SimSatProduct::Sandwich.api_product(),
            Product::Sandwich
        ));
        assert!(matches!(SimSatProduct::Ir13.api_product(), Product::Ir));
        assert!(matches!(
            SimSatProduct::Wv8.api_product(),
            Product::WaterVapor {
                band: WvBand::Upper
            }
        ));
        assert!(matches!(
            SimSatProduct::PrecipitableWater.api_product(),
            Product::Derived {
                field: DerivedField::PrecipitableWater
            }
        ));
        assert_eq!(SimSatProduct::Wv10.thermal_band(), Some(10));
        assert_eq!(SimSatProduct::CloudOpticalDepth.thermal_band(), None);
    }

    #[test]
    fn sector_tokens_are_product_view_qualified_and_constant_source_based() {
        let visible = qualified_sector(
            "hrrr_20260710_t20z",
            SimSatProduct::Visible,
            OutputView::Geostationary,
        );
        let geocolor = qualified_sector(
            "hrrr_20260710_t20z",
            SimSatProduct::GeoColor,
            OutputView::Geostationary,
        );
        let topdown = qualified_sector(
            "hrrr_20260710_t20z",
            SimSatProduct::Visible,
            OutputView::TopDown,
        );
        assert_eq!(visible, "hrrr_20260710_t20z_visible_geo");
        assert_ne!(visible, geocolor);
        assert_ne!(visible, topdown);
        assert_eq!(
            source_group_base(Path::new("hrrr.t20z.wrfnatf17.grib2")),
            "hrrr_t20z"
        );
        assert_eq!(
            source_group_base(Path::new("wrfout_d03_2025-06-21_02-15-00")),
            "wrfout_d03"
        );
    }

    #[test]
    fn sequence_sort_is_valid_time_then_path_then_timestep() {
        let mut frames = vec![
            FrameInput {
                path: PathBuf::from("b"),
                timestep: 0,
                sort_key: "2026-07-10T03:00:00Z".to_owned(),
                label: "three".to_owned(),
            },
            FrameInput {
                path: PathBuf::from("a"),
                timestep: 1,
                sort_key: "2026-07-10T01:00:00Z".to_owned(),
                label: "one-b".to_owned(),
            },
            FrameInput {
                path: PathBuf::from("a"),
                timestep: 0,
                sort_key: "2026-07-10T01:00:00Z".to_owned(),
                label: "one-a".to_owned(),
            },
        ];
        sort_frame_inputs(&mut frames);
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.label.as_str())
                .collect::<Vec<_>>(),
            vec!["one-a", "one-b", "three"]
        );
    }

    #[test]
    fn scalar_result_conversion_preserves_raw_values_and_mesh() {
        let payload = plot_payload_from_parts(
            SimSatProduct::PrecipitableWater,
            "source".to_owned(),
            "time".to_owned(),
            2,
            1,
            FrameData::Scalar {
                values: vec![12.5, f32::NAN],
                rgb: None,
                field: DerivedField::PrecipitableWater,
            },
            vec![40.0, 40.0],
            vec![-100.0, -99.0],
        )
        .unwrap();
        assert_eq!(payload.nx, 2);
        assert_eq!(payload.lat, vec![40.0, 40.0]);
        let PlotPixels::Scalar { values, units } = payload.pixels else {
            panic!("expected scalar payload");
        };
        assert_eq!(values[0], 12.5);
        assert!(values[1].is_nan());
        assert_eq!(units, "mm");
    }

    #[test]
    fn rgba_result_conversion_preserves_alpha_and_rejects_bad_shape() {
        let rgba = vec![10, 20, 30, 255, 40, 50, 60, 0];
        let payload = plot_payload_from_parts(
            SimSatProduct::Visible,
            "source".to_owned(),
            "time".to_owned(),
            2,
            1,
            FrameData::Visible {
                rgb: vec![10, 20, 30, 0, 0, 0],
                rgba: rgba.clone(),
            },
            vec![40.0, f32::NAN],
            vec![-100.0, f32::NAN],
        )
        .unwrap();
        assert_eq!(payload.pixels, PlotPixels::Rgba(rgba));

        let error = plot_payload_from_parts(
            SimSatProduct::Visible,
            "source".to_owned(),
            "time".to_owned(),
            2,
            1,
            FrameData::Visible {
                rgb: Vec::new(),
                rgba: vec![0; 4],
            },
            vec![40.0, 40.0],
            vec![-100.0, -99.0],
        )
        .unwrap_err();
        assert!(error.contains("expected 8"));

        let payload = PlotPayload {
            title: "bad rgba".to_owned(),
            subtitle_left: String::new(),
            subtitle_right: String::new(),
            nx: 1,
            ny: 1,
            lat: vec![40.0],
            lon: vec![-100.0],
            pixels: PlotPixels::Rgba(vec![1, 2, 3, 4, 5]),
        };
        assert!(payload.to_plot_source().unwrap_err().contains("expected 4"));
    }
}
