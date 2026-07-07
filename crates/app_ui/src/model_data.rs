//! Model data dock — rusty-weather's rw-ui panels mounted inside BowEcho.
//!
//! The panels (run browser, false-color field viewer, skew-T sounding) were
//! built to take a `&mut egui::Ui` from any egui host; all store IO runs on
//! rw-ui's own worker thread, so BowEcho's render loop never blocks. The
//! data source is an rw-store directory on disk (produced by rusty-weather
//! ingest, default `C:\Users\drew\rusty-weather\store`).

use eframe::egui;
use rw_ui::{
    ColorTableEditorPanel, FieldViewerEvent, FieldViewerPanel, HourKey, PlotViewerPanel,
    RunBrowserPanel, SoundingPanel, StoreRequest, StoreResponse, StoreTree, StoreView, StoreWorker,
    StyleOverrideSettings,
};
use std::path::PathBuf;

/// A running local WRF/NetCDF ingest, spawned from the dock's import controls.
/// Both variants write into the same model store the dock browses, so a
/// finished import is picked up by [`ModelDataDock::rescan`] and its runs then
/// sound through the existing skew-T path.
// Constructed only from the rfd file-dialog UI (cfg windows/macos); the
// Linux verify node would otherwise flag every variant dead.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
enum ImportJob {
    /// Light path (`local_import`): 2D surface fields + isobaric sounding
    /// volumes. Handles raw `wrfout`, post-processed climate wrfout, and plain
    /// NetCDF. Sends one final `Result<summary, error>`.
    Local(crate::local_import::LocalImportTask),
    /// Full path (`wrf_process`): the complete 2D diagnostic set (CAPE/severe/
    /// etc.) plus sounding volumes via `wrf-core`. Streams progress messages
    /// then a `Done`.
    Process(crate::wrf_process::WrfProcessTask),
    /// Synthetic-radar path (`wrf_radar`): forward-model each WRF forecast time
    /// into a simulated `RadarVolume` (REF + Vr). Unlike the other two, its
    /// result is NOT written to the model store — it is handed back to the app
    /// (via [`ModelDataDock::take_synthetic_radar`]) to LOOP in the radar
    /// viewer. Streams progress messages then a `Done`.
    SyntheticRadar(crate::wrf_radar::SyntheticRadarTask),
}

/// Editable + persisted state for the WRF "full diagnostics" processing-
/// options popover. The four booleans mirror [`crate::wrf_process::
/// WrfProcessOptions`] product groups; `only_text`/`skip_text` are the raw,
/// user-typed field filters (comma/space separated) kept as strings so they
/// round-trip through settings exactly and remain editable. Serialized opaque
/// into `AppSettings::wrf_process_options` (same pattern as the sounding view
/// state), so an older config with no entry restores today's defaults.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct WrfProcessUiState {
    #[serde(default = "wrf_default_true")]
    core_fields: bool,
    #[serde(default = "wrf_default_true")]
    diagnostics: bool,
    #[serde(default)]
    heavy_ecape: bool,
    #[serde(default = "wrf_default_true")]
    raw_extras: bool,
    #[serde(default)]
    only_text: String,
    #[serde(default)]
    skip_text: String,
}

fn wrf_default_true() -> bool {
    true
}

impl Default for WrfProcessUiState {
    fn default() -> Self {
        // Matches `WrfProcessOptions::default()`: everything but heavy eCAPE.
        Self {
            core_fields: true,
            diagnostics: true,
            heavy_ecape: false,
            raw_extras: true,
            only_text: String::new(),
            skip_text: String::new(),
        }
    }
}

impl WrfProcessUiState {
    /// Build the backend options from the current UI selection. `only`/`skip`
    /// pass through as single raw strings; `WrfProcessOptions::normalized`
    /// (called inside `spawn_process_paths`) splits and cleans the tokens.
    /// Only consumed by the desktop import UI, hence the dead-code allowance
    /// for headless/non-rfd targets.
    #[allow(dead_code)]
    fn to_options(&self) -> crate::wrf_process::WrfProcessOptions {
        let field_filter = |text: &str| {
            if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![text.to_string()]
            }
        };
        crate::wrf_process::WrfProcessOptions {
            core_fields: self.core_fields,
            diagnostics: self.diagnostics,
            heavy_ecape: self.heavy_ecape,
            raw_extras: self.raw_extras,
            only: field_filter(&self.only_text),
            skip: field_filter(&self.skip_text),
        }
        .normalized()
    }
}

/// Where the SIMULATED radar's antenna stands, for the "Virtual radar site"
/// popover. Serialized snake_case into settings; the default (domain centre)
/// is the historical behavior, so old configs are unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SynthPlacement {
    /// Antenna at the WRF domain centre (the historical default).
    #[default]
    DomainCenter,
    /// Explicit numeric lat/lon entry.
    LatLon,
    /// A real site id (e.g. KTLX), resolved through the app's compiled-in
    /// site catalog ([`data_source::sites`]) to its lat/lon.
    NexradSite,
}

/// Editable + persisted state for the "Virtual radar site & range" popover
/// next to the simulated-radar button: antenna placement plus the optional
/// max-range / gate-spacing overrides (groundwork for the wide CONUS-style
/// circle — up to 1000 km with proportionally coarser gates). Serialized
/// opaque into `AppSettings::wrf_synth_radar` (same pattern as
/// [`WrfProcessUiState`]), so an older config with no entry restores today's
/// defaults: domain centre, 230 km / 250 m.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct SyntheticRadarUiState {
    #[serde(default)]
    placement: SynthPlacement,
    /// Raw lat/lon entry text, kept as typed so partial edits round-trip
    /// through settings exactly (validated only when a run launches).
    #[serde(default)]
    lat_text: String,
    #[serde(default)]
    lon_text: String,
    /// Site-id entry text for [`SynthPlacement::NexradSite`].
    #[serde(default)]
    site_id_text: String,
    /// Maximum range (km). 230 = the classic WSR-88D Doppler range.
    #[serde(default = "default_synth_range_km")]
    max_range_km: f64,
    /// Manual gate spacing (m), used when `auto_gate_spacing` is off.
    #[serde(default = "default_synth_gate_m")]
    gate_spacing_m: f64,
    /// Scale gate spacing proportionally with range (keeps the gate count at
    /// the classic 920 of 230 km / 250 m), so a 1000 km circle costs the same
    /// memory as the default instead of 4× more.
    #[serde(default = "wrf_default_true")]
    auto_gate_spacing: bool,
}

fn default_synth_range_km() -> f64 {
    230.0
}

fn default_synth_gate_m() -> f64 {
    250.0
}

impl Default for SyntheticRadarUiState {
    fn default() -> Self {
        Self {
            placement: SynthPlacement::DomainCenter,
            lat_text: String::new(),
            lon_text: String::new(),
            site_id_text: String::new(),
            max_range_km: default_synth_range_km(),
            gate_spacing_m: default_synth_gate_m(),
            auto_gate_spacing: true,
        }
    }
}

impl SyntheticRadarUiState {
    const MIN_RANGE_KM: f64 = 50.0;
    const MAX_RANGE_KM: f64 = 1000.0;
    const MIN_GATE_M: f64 = 100.0;
    const MAX_GATE_M: f64 = 4000.0;
    /// Gate count of the classic default volume (230 km / 250 m); auto
    /// spacing preserves it as the range grows.
    const DEFAULT_GATE_COUNT: f64 = 920.0;

    fn clamped_range_km(&self) -> f64 {
        let range = self.max_range_km;
        if range.is_finite() {
            range.clamp(Self::MIN_RANGE_KM, Self::MAX_RANGE_KM)
        } else {
            default_synth_range_km()
        }
    }

    /// Effective gate spacing (m): proportionally coarser with range in auto
    /// mode (never finer than the classic 250 m), or the clamped manual value.
    fn effective_gate_spacing_m(&self) -> f64 {
        if self.auto_gate_spacing {
            (self.clamped_range_km() * 1000.0 / Self::DEFAULT_GATE_COUNT)
                .max(default_synth_gate_m())
        } else {
            let spacing = self.gate_spacing_m;
            if spacing.is_finite() {
                spacing.clamp(Self::MIN_GATE_M, Self::MAX_GATE_M)
            } else {
                default_synth_gate_m()
            }
        }
    }

    /// Resolve the current selection into a scan config. `Err` is a human
    /// message the import row shows INSTEAD of launching, so a typo'd site id
    /// or lat/lon never silently falls back to the domain centre.
    ///
    /// Antenna altitude is left `None` in every mode: the virtual antenna
    /// stands on the MODEL terrain at the resolved position (plus the short
    /// default tower) — the model world's own ground, which is what the beam
    /// samples.
    ///
    /// Only consumed by the desktop import UI (and tests), hence the
    /// dead-code allowance for headless/non-rfd targets — same pattern as
    /// [`WrfProcessUiState::to_options`].
    #[allow(dead_code)]
    fn to_config(&self) -> Result<crate::wrf_radar::SyntheticRadarConfig, String> {
        let mut config = crate::wrf_radar::SyntheticRadarConfig {
            max_range_m: self.clamped_range_km() * 1000.0,
            gate_spacing_m: self.effective_gate_spacing_m(),
            ..crate::wrf_radar::SyntheticRadarConfig::default()
        };
        match self.placement {
            SynthPlacement::DomainCenter => {}
            SynthPlacement::LatLon => {
                let lat = parse_synth_coord(&self.lat_text, -90.0, 90.0, "latitude")?;
                let lon = parse_synth_coord(&self.lon_text, -180.0, 180.0, "longitude")?;
                config.site_lat_deg = Some(lat);
                config.site_lon_deg = Some(lon);
                config.site_name = Some(format!("Simulated WRF radar @ {lat:.3}, {lon:.3}"));
            }
            SynthPlacement::NexradSite => {
                let id = self.site_id_text.trim().to_ascii_uppercase();
                if id.is_empty() {
                    return Err("Virtual radar site: enter a site id (e.g. KTLX)".to_string());
                }
                let record = data_source::sites::resolve(&data_source::sites::SiteRef::Us {
                    level2_id: id.clone(),
                })
                .ok_or_else(|| format!("Virtual radar site: “{id}” is not in the site catalog"))?;
                let (lat, lon) = record.lat_lon.ok_or_else(|| {
                    format!("Virtual radar site: {id} has no catalog coordinates")
                })?;
                config.site_lat_deg = Some(f64::from(lat));
                config.site_lon_deg = Some(f64::from(lon));
                config.site_id = id;
                config.site_name = Some(format!("Simulated WRF radar at {}", record.label));
            }
        }
        Ok(config)
    }
}

/// Parse one lat/lon entry, with a human message on failure. (Dead-code
/// allowance: reached only through [`SyntheticRadarUiState::to_config`].)
#[allow(dead_code)]
fn parse_synth_coord(text: &str, min: f64, max: f64, what: &str) -> Result<f64, String> {
    let trimmed = text.trim();
    let value: f64 = trimmed
        .parse()
        .map_err(|_| format!("Virtual radar site: “{trimmed}” is not a valid {what}"))?;
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(format!(
            "Virtual radar site: {what} {value} is outside [{min}, {max}]"
        ));
    }
    Ok(value)
}

/// A heavy full-diagnostics import parked behind an explicit confirmation
/// because the target looks LARGE (grid cells / file size) — the owner has
/// melted this machine by launching the heavy path on a 250 m grid expecting
/// the fast simulated-radar path. (Dead-code allowance: only the desktop
/// import UI constructs/reads it.)
#[allow(dead_code)]
struct PendingHeavyImport {
    files: Vec<PathBuf>,
    warning: String,
}

/// LARGE-import thresholds for the heavy path's confirm step.
#[allow(dead_code)]
const HEAVY_WARN_CELLS_3D: usize = 10_000_000;
#[allow(dead_code)]
const HEAVY_WARN_FILE_BYTES: u64 = 1 << 30; // 1 GiB

/// Cheap size probe for the heavy full-diagnostics import: flag LARGE targets
/// BEFORE launching. File sizes come from `fs::metadata` (free); grid dims
/// from opening ONE file's header (`WrfFile::open` reads dimensions only — no
/// field decompression), so this is safe on the UI thread right after the
/// folder dialog. Returns `None` when the target looks small enough to just
/// run. (Dead-code allowance: only the desktop import UI calls it.)
#[allow(dead_code)]
fn heavy_import_size_warning(files: &[PathBuf]) -> Option<String> {
    let max_bytes = files
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok().map(|meta| meta.len()))
        .max()
        .unwrap_or(0);
    let dims = files
        .first()
        .and_then(|path| wrf_core::WrfFile::open(path).ok())
        .map(|file| (file.nx, file.ny, file.nz, file.nt));
    let cells_3d = dims.map(|(nx, ny, nz, _)| nx * ny * nz).unwrap_or(0);
    if cells_3d < HEAVY_WARN_CELLS_3D && max_bytes < HEAVY_WARN_FILE_BYTES {
        return None;
    }
    let mut parts = Vec::new();
    if let Some((nx, ny, nz, nt)) = dims {
        let times = if nt > 1 {
            format!(", {nt} times/file")
        } else {
            String::new()
        };
        parts.push(format!(
            "{nx}×{ny}×{nz} grid (~{:.0}M cells{times})",
            (nx * ny * nz) as f64 / 1.0e6
        ));
    }
    if max_bytes > 0 {
        parts.push(format!("largest file {:.1} GB", max_bytes as f64 / 1.0e9));
    }
    Some(format!(
        "{} across {} file(s). Full diagnostics computes the ~117-field 2-D \
         suite through wrf-core — MINUTES per file and many GB of RAM on a \
         grid this size.",
        parts.join(", "),
        files.len()
    ))
}

pub struct ModelDataDock {
    worker: StoreWorker,
    /// egui context, kept so a background import can request repaints while it
    /// runs (its worker threads have no repaint hook of their own).
    repaint: egui::Context,
    store_root: PathBuf,
    /// Running local WRF/NetCDF import, if any (drained in `poll_import`).
    import_job: Option<ImportJob>,
    /// Finished synthetic-radar volumes waiting for the app to install them in
    /// the loop engine (one-shot, drained by [`Self::take_synthetic_radar`]).
    synthetic_radar_result: Option<crate::wrf_radar::SyntheticRadarOutput>,
    /// Last import status line shown under the import controls.
    import_message: Option<String>,
    tree: Option<StoreTree>,
    browser: RunBrowserPanel,
    viewer: FieldViewerPanel,
    sounding: SoundingPanel,
    /// Most recent loaded field (kept for the map layer).
    latest_field: Option<std::sync::Arc<rw_ui::FieldData>>,
    /// Most recent sounding data (kept for the native skew-T window).
    latest_sounding: Option<std::sync::Arc<rw_ui::SoundingData>>,
    /// One-shot: the user asked to put the current field on the radar map.
    map_request: Option<std::sync::Arc<rw_ui::FieldData>>,
    /// v0.2.3 custom-domain plot viewer: renders the selected field through
    /// rusty-weather's native plot pipeline over a user-chosen domain (shift-
    /// drag a box on the field viewer, or rotate a corner). Shown as a floating
    /// window when `show_plot_viewer` is set.
    plot_viewer: PlotViewerPanel,
    show_plot_viewer: bool,
    /// v0.2.3 user-editable model field-plot color tables (rw-ui). Distinct
    /// from the radar-side table editor: this edits the STYLE OVERRIDES the
    /// store worker resolves palettes through. Edits are pushed to the worker
    /// and the current field reloaded so the new palette shows.
    color_tables: ColorTableEditorPanel,
    show_color_tables: bool,
    /// User's WRF full-diagnostics processing selection (product groups +
    /// only/skip field filters). Edited in the import area's options popover,
    /// applied when the heavy "Full model import" launches, and persisted
    /// to settings so it survives restarts.
    wrf_options: WrfProcessUiState,
    /// User's virtual-radar placement + range selection for the simulated-
    /// radar import (edited in the "Virtual radar site & range" popover,
    /// persisted to settings like `wrf_options`).
    synth_radar: SyntheticRadarUiState,
    /// A heavy full-diagnostics import awaiting explicit confirmation because
    /// the chosen folder looks LARGE (see [`heavy_import_size_warning`]).
    // Read only by the rfd-gated (windows/macos) import UI.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    pending_heavy_import: Option<PendingHeavyImport>,
}

impl ModelDataDock {
    pub fn new(ctx: &egui::Context, store_root: PathBuf) -> Self {
        let repaint = ctx.clone();
        let worker = StoreWorker::spawn(StoreView::new(&store_root), move || {
            repaint.request_repaint();
        });
        worker.send(StoreRequest::Enumerate);
        Self {
            worker,
            repaint: ctx.clone(),
            store_root,
            import_job: None,
            synthetic_radar_result: None,
            import_message: None,
            tree: None,
            browser: RunBrowserPanel::new(),
            viewer: FieldViewerPanel::new(),
            sounding: SoundingPanel::new(),
            latest_field: None,
            latest_sounding: None,
            map_request: None,
            plot_viewer: PlotViewerPanel::new(),
            show_plot_viewer: false,
            color_tables: ColorTableEditorPanel::new(),
            show_color_tables: false,
            wrf_options: WrfProcessUiState::default(),
            synth_radar: SyntheticRadarUiState::default(),
            pending_heavy_import: None,
        }
    }

    /// Push edited color-table style overrides to the store worker and reload
    /// the current field so the new palette shows (mirrors the rusty-weather
    /// reference host). The `StyleOverridesApplied` ack is a no-op — the reload
    /// is what repaints.
    fn apply_color_table_changes(&mut self) {
        let settings = self.color_tables.settings().clone().normalized();
        self.worker.send(StoreRequest::SetStyleOverrides(settings));
        self.plot_viewer.clear();
        if let Some(field) = self.viewer.wanted_field() {
            self.viewer.set_loading(&field.var);
            self.worker.send(StoreRequest::LoadField(field));
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(ctx: &egui::Context, tree: StoreTree) -> Self {
        let mut dock = Self::new(ctx, std::env::temp_dir().join("bowecho-model-dock-test"));
        dock.tree = Some(tree);
        dock
    }

    fn select_hour(&mut self, key: HourKey) {
        self.worker.send(StoreRequest::LoadHour(key));
    }

    /// Drain worker responses into panel state (mirrors the rusty-weather
    /// reference host).
    fn handle_responses(&mut self) {
        self.poll_import();
        while let Some(response) = self.worker.try_recv() {
            match response {
                StoreResponse::Tree(tree) => {
                    if self.browser.selected().is_none() {
                        let first = tree.models.first().and_then(|model| {
                            model.runs.first().and_then(|run| {
                                run.hours.first().map(|hour| HourKey {
                                    model: model.model.clone(),
                                    run: run.run.clone(),
                                    hour: hour.hour,
                                })
                            })
                        });
                        if let Some(key) = first {
                            self.browser.select(key.clone());
                            self.select_hour(key);
                        }
                    }
                    self.tree = Some(tree);
                }
                StoreResponse::HourVars(key, Ok(vars)) => {
                    if self.browser.selected() == Some(&key) {
                        self.viewer.set_hour(key, vars);
                        if let Some(field) = self.viewer.wanted_field() {
                            self.viewer.set_loading(&field.var);
                            self.worker.send(StoreRequest::LoadField(field));
                        }
                    }
                }
                StoreResponse::HourVars(_, Err(message)) => {
                    self.viewer.set_error(message);
                }
                StoreResponse::Field(key, boxed) => match *boxed {
                    Ok(field) => {
                        self.latest_field = Some(std::sync::Arc::new(field.clone()));
                        self.viewer.set_field(field);
                    }
                    Err(message) => {
                        if self.viewer.wanted_field().as_ref() == Some(&key) {
                            self.viewer.set_error(message);
                        }
                    }
                },
                StoreResponse::Sounding(_, Ok(data)) => {
                    self.latest_sounding = Some(std::sync::Arc::new(data.clone()));
                    self.sounding.set_data(data);
                }
                StoreResponse::Sounding(_, Err(message)) => {
                    self.sounding.set_error(message);
                }
                // v0.2.3: worker ack that the style overrides were applied.
                // No-op by design — `apply_color_table_changes` already
                // reloads the field, and that reload is what repaints.
                StoreResponse::StyleOverridesApplied => {}
            }
        }
    }

    /// Drain worker responses even while the window is closed — keeps the
    /// store browser, LUT, and sounding flows alive for map interactions.
    pub fn pump(&mut self) {
        self.handle_responses();
    }

    /// One-shot map request (the app installs it as a radar-map layer).
    pub fn take_map_request(&mut self) -> Option<std::sync::Arc<rw_ui::FieldData>> {
        self.map_request.take()
    }

    /// The most recently loaded field (for layer auto-refresh).
    pub fn latest_field(&self) -> Option<&std::sync::Arc<rw_ui::FieldData>> {
        self.latest_field.as_ref()
    }

    /// Selected model hour in the store browser.
    pub fn selected_hour(&self) -> Option<&rw_ui::HourKey> {
        self.browser.selected()
    }

    /// Select an exact store hour, requesting its variable list if it is a
    /// real change. Returns true when a new hour was requested.
    pub fn select_hour_key(&mut self, key: HourKey) -> bool {
        if self.browser.selected() == Some(&key) {
            return false;
        }
        self.browser.select(key.clone());
        self.select_hour(key);
        true
    }

    /// The most recent sounding (for the native skew-T window).
    pub fn latest_sounding(&self) -> Option<&std::sync::Arc<rw_ui::SoundingData>> {
        self.latest_sounding.as_ref()
    }

    /// Whether the reusable rw-ui sounding panel has model sounding content
    /// ready for an external host pane/window.
    pub fn sounding_has_content(&self) -> bool {
        self.sounding.has_content()
    }

    /// Render the reusable rw-ui sounding panel outside the Model Data dock.
    pub fn sounding_ui(&mut self, ui: &mut egui::Ui) {
        self.handle_responses();
        self.sounding.ui(ui);
    }

    pub fn sounding_view_state_json(&self) -> serde_json::Value {
        self.sounding.view_state_json()
    }

    pub fn apply_sounding_view_state_json(&mut self, value: &serde_json::Value) -> bool {
        self.sounding.apply_view_state_json(value)
    }

    /// Whether the 🎨 Color-tables editor currently binds a USER palette to
    /// this field — i.e. `field.style` (and thus the map layer's production
    /// colormap) IS the user's table, not the operational default. The map
    /// layer resolution uses this to rank the user's palette above the
    /// built-in Solar WRF tables: user override → Solar → production →
    /// generic. See [`user_style_override_active`].
    pub fn user_style_override_for(&self, field: &rw_ui::FieldData) -> bool {
        user_style_override_active(self.color_tables.settings(), field)
    }

    /// Serialize the current model field-plot color-table overrides for
    /// persistence (opaque JSON; kept in app settings like the sounding state).
    pub fn style_overrides_json(&self) -> serde_json::Value {
        serde_json::to_value(self.color_tables.settings()).unwrap_or(serde_json::Value::Null)
    }

    /// Restore persisted color-table overrides: load them into the editor and
    /// push them to the store worker so field palettes resolve through them.
    /// Returns false on malformed JSON (older/newer schema) — left at defaults.
    pub fn apply_style_overrides_json(&mut self, value: &serde_json::Value) -> bool {
        match serde_json::from_value::<StyleOverrideSettings>(value.clone()) {
            Ok(settings) => {
                self.color_tables.set_settings(settings.clone());
                self.worker
                    .send(StoreRequest::SetStyleOverrides(settings.normalized()));
                true
            }
            Err(_) => false,
        }
    }

    /// Serialize the current WRF full-diagnostics processing selection for
    /// persistence (opaque JSON in `AppSettings::wrf_process_options`, same as
    /// the sounding/style states).
    pub fn wrf_process_options_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.wrf_options).unwrap_or(serde_json::Value::Null)
    }

    /// Restore a persisted WRF processing selection into the import popover.
    /// Returns false on malformed JSON (older/newer schema) — the selection is
    /// left at today's defaults, so a bad entry never breaks the import.
    pub fn apply_wrf_process_options_json(&mut self, value: &serde_json::Value) -> bool {
        match serde_json::from_value::<WrfProcessUiState>(value.clone()) {
            Ok(state) => {
                self.wrf_options = state;
                true
            }
            Err(_) => false,
        }
    }

    /// Serialize the virtual-radar placement/range selection for persistence
    /// (opaque JSON in `AppSettings::wrf_synth_radar`, same pattern as
    /// [`Self::wrf_process_options_json`]).
    pub fn wrf_synth_radar_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.synth_radar).unwrap_or(serde_json::Value::Null)
    }

    /// Restore a persisted virtual-radar selection into the site popover.
    /// Returns false on malformed JSON — left at today's defaults (domain
    /// centre, 230 km / 250 m), so a bad entry never breaks the import.
    pub fn apply_wrf_synth_radar_json(&mut self, value: &serde_json::Value) -> bool {
        match serde_json::from_value::<SyntheticRadarUiState>(value.clone()) {
            Ok(state) => {
                self.synth_radar = state;
                true
            }
            Err(_) => false,
        }
    }

    /// Newest (model, run, hour-count) in the store tree — freshness display.
    pub fn newest_run(&self) -> Option<(String, String, usize)> {
        let tree = self.tree.as_ref()?;
        let model = tree.models.first()?;
        let run = model.runs.last()?;
        Some((model.model.clone(), run.run.clone(), run.hours.len()))
    }

    /// Re-scan the store (after an ingest finishes).
    pub fn rescan(&mut self) {
        self.worker.send(StoreRequest::Enumerate);
    }

    /// Drain a running local WRF/NetCDF import. On completion the store is
    /// re-scanned so the new run appears in the browser (and thus sounds).
    /// Called every frame from `handle_responses` — including via `pump`, so
    /// an import finishes and refreshes even while the dock window is closed.
    fn poll_import(&mut self) {
        // What to do once the borrow of `import_job` is released. `rescan` is
        // set on any completion so a partially-written run still shows.
        enum PollResult {
            Idle,
            Progress(String),
            Finished {
                message: String,
            },
            /// A synthetic-radar job finished: its volumes go to the app to
            /// loop, not to the store, so this carries the output out.
            FinishedSynthetic {
                message: String,
                output: crate::wrf_radar::SyntheticRadarOutput,
            },
        }

        let result = match self.import_job.as_ref() {
            None => PollResult::Idle,
            Some(ImportJob::Local(task)) => match task.rx.try_recv() {
                Ok(Ok(summary)) => PollResult::Finished {
                    message: format!(
                        "Imported {} hour(s) from {} file(s) → run “{}” ({} variables)",
                        summary.hours_written,
                        summary.files_seen,
                        summary.run,
                        summary.variables.len()
                    ),
                },
                Ok(Err(error)) => PollResult::Finished {
                    message: format!("Import failed: {error}"),
                },
                Err(std::sync::mpsc::TryRecvError::Empty) => PollResult::Progress(String::new()),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => PollResult::Finished {
                    message: "Import worker stopped unexpectedly".to_string(),
                },
            },
            Some(ImportJob::Process(task)) => {
                let mut latest = None;
                let mut done = None;
                loop {
                    match task.rx.try_recv() {
                        Ok(crate::wrf_process::WrfProcessMessage::Progress(message)) => {
                            latest = Some(message);
                        }
                        Ok(crate::wrf_process::WrfProcessMessage::Done(outcome)) => {
                            done = Some(outcome);
                            break;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            done = Some(Err("WRF worker stopped unexpectedly".to_string()));
                            break;
                        }
                    }
                }
                match done {
                    Some(Ok(summary)) => PollResult::Finished {
                        message: format!(
                            "Processed {} WRF hour(s) from {} file(s) → run “{}” ({} variables)",
                            summary.hours_written,
                            summary.files_seen,
                            summary.run,
                            summary.variables.len()
                        ),
                    },
                    Some(Err(error)) => PollResult::Finished {
                        message: format!("WRF processing failed: {error}"),
                    },
                    None => match latest {
                        Some(message) => PollResult::Progress(message),
                        None => PollResult::Progress(String::new()),
                    },
                }
            }
            Some(ImportJob::SyntheticRadar(task)) => {
                let mut latest = None;
                let mut done = None;
                loop {
                    match task.rx.try_recv() {
                        Ok(crate::wrf_radar::SyntheticRadarMessage::Progress(message)) => {
                            latest = Some(message);
                        }
                        Ok(crate::wrf_radar::SyntheticRadarMessage::Done(outcome)) => {
                            done = Some(outcome);
                            break;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            done = Some(Err(
                                "Synthetic-radar worker stopped unexpectedly".to_string()
                            ));
                            break;
                        }
                    }
                }
                match done {
                    Some(Ok(output)) => PollResult::FinishedSynthetic {
                        message: format!(
                            "Simulated {} radar frame(s) from WRF — looping in the radar view",
                            output.volumes.len()
                        ),
                        output,
                    },
                    Some(Err(error)) => PollResult::Finished {
                        message: format!("Synthetic radar failed: {error}"),
                    },
                    None => match latest {
                        Some(message) => PollResult::Progress(message),
                        None => PollResult::Progress(String::new()),
                    },
                }
            }
        };

        match result {
            PollResult::Idle => {}
            PollResult::Progress(message) => {
                if !message.is_empty() {
                    self.import_message = Some(message);
                }
                // No repaint hook on the import worker thread — keep the UI
                // ticking so progress and completion show promptly.
                self.repaint.request_repaint();
            }
            PollResult::Finished { message } => {
                self.import_message = Some(message);
                self.import_job = None;
                self.rescan();
            }
            PollResult::FinishedSynthetic { message, output } => {
                self.import_message = Some(message);
                self.import_job = None;
                // Hand the simulated volumes to the app (drained in
                // `poll_model_layer`); nothing was written to the store, so no
                // rescan.
                self.synthetic_radar_result = Some(output);
                self.repaint.request_repaint();
            }
        }
    }

    /// One-shot: take finished synthetic-radar volumes for the app to install
    /// into the loop engine. Returns `(status label, one volume per WRF time)`.
    pub fn take_synthetic_radar(
        &mut self,
    ) -> Option<(String, Vec<std::sync::Arc<radar_core::RadarVolume>>)> {
        let output = self.synthetic_radar_result.take()?;
        Some((output.label, output.volumes))
    }

    /// Import controls (WRF/NetCDF folder pickers + status), rendered in the
    /// dock's left "Runs" column. Spawns the ingest onto a worker thread;
    /// `poll_import` finishes it and re-scans the store.
    fn import_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Import");
            if self.import_job.is_some() {
                ui.spinner();
            }
        });
        self.import_pickers(ui);
        if let Some(message) = &self.import_message {
            ui.label(egui::RichText::new(message).small().weak());
        }
    }

    /// Native folder pickers that spawn the ingest (rfd is Windows/macOS-only,
    /// matching the rest of the app's local-file UI).
    #[cfg(any(windows, target_os = "macos"))]
    fn import_pickers(&mut self, ui: &mut egui::Ui) {
        let busy = self.import_job.is_some();
        ui.horizontal_wrapped(|ui| {
            // Single-file import — the common case; no need to point at a whole
            // folder / batch. `spawn_import_paths` already takes a path list, so
            // a one-element vec imports exactly the chosen file.
            if ui
                .add_enabled(!busy, egui::Button::new("📄 WRF/NetCDF file…"))
                .on_hover_text(
                    "Import a SINGLE WRF/NetCDF file into the model store (one forecast \
                     hour). Handles raw wrfout, post-processed climate wrfout, and plain \
                     NetCDF. Click a point in the field viewer afterwards to sound it.",
                )
                .clicked()
                && let Some(file) = rfd::FileDialog::new()
                    .set_title("Choose a WRF/NetCDF file to import")
                    .pick_file()
            {
                let name = file
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| file.display().to_string());
                let task =
                    crate::local_import::spawn_import_paths(vec![file], self.store_root.clone());
                self.import_message = Some(format!("Importing {name}…"));
                self.import_job = Some(ImportJob::Local(task));
            }

            if ui
                .add_enabled(!busy, egui::Button::new("📥 WRF/NetCDF folder…"))
                .on_hover_text(
                    "Read a folder of WRF/NetCDF files into the model store: 2D surface \
                     fields plus skew-T sounding volumes. Each file becomes one forecast \
                     hour. Handles raw wrfout, post-processed climate wrfout, and plain \
                     NetCDF. Click a point in the field viewer afterwards to sound it.",
                )
                .clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a WRF/NetCDF folder to import")
                    .pick_folder()
            {
                let files = crate::local_import::supported_files_in_folder(&dir);
                if files.is_empty() {
                    self.import_message =
                        Some(format!("No WRF/NetCDF files under {}", dir.display()));
                } else {
                    let count = files.len();
                    let task =
                        crate::local_import::spawn_import_paths(files, self.store_root.clone());
                    self.import_message = Some(format!("Importing {count} file(s)…"));
                    self.import_job = Some(ImportJob::Local(task));
                }
            }

            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("🛠 Full model import (heavy — all diagnostics)…"),
                )
                .on_hover_text(
                    "HEAVY ingest for raw wrfout files: computes the full ~117-field 2D \
                     diagnostic suite (CAPE / severe / etc.) plus sounding volumes via \
                     wrf-core — MINUTES per file on large grids. NOT the simulated-radar \
                     button (that one is 🌩 below, and takes seconds). Use “🛠 WRF \
                     full-diagnostics fields” below to narrow what gets processed — the \
                     default writes everything but heavy eCAPE.",
                )
                .clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a WRF folder for the HEAVY full-diagnostics import")
                    .pick_folder()
            {
                let files = crate::wrf_process::wrf_files_in_folder(&dir);
                if files.is_empty() {
                    self.import_message = Some(format!("No WRF files under {}", dir.display()));
                } else if let Some(warning) = heavy_import_size_warning(&files) {
                    // LARGE grid/file: park the launch behind an explicit,
                    // size-aware confirmation instead of melting the machine.
                    self.import_message = None;
                    self.pending_heavy_import = Some(PendingHeavyImport { files, warning });
                } else {
                    self.launch_heavy_import(files, self.wrf_options.to_options());
                }
            }
        });
        // Product selector for the heavy "Full model import" above. The light
        // single-file/folder path (`local_import`) writes a FIXED 2D-surface +
        // isobaric-sounding set with no options struct, so it is not wired
        // here; if per-field selection is ever wanted there too, this same
        // popover could drive a `local_import` options argument.
        self.wrf_options_panel(ui);
        self.heavy_import_warning_ui(ui);

        ui.separator();
        // The FAST simulated-radar path: loops in the radar view, writes
        // NOTHING to the model store. Kept visually apart from the store
        // imports above — the owner has repeatedly launched the heavy import
        // expecting this one.
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("🌩 Simulated radar from WRF (fast)…"),
                )
                .on_hover_text(
                    "FAST path (~seconds per file): forward-model the raw wrfout \
                     hydrometeors + winds into a SIMULATED NEXRAD-style scan (REF + radial \
                     velocity) that renders and LOOPS in the radar view — colormaps, \
                     cross-sections, GBVTD and all. Pick one or MORE wrfout files: each \
                     file / forecast time becomes a loop frame, sorted by model time. \
                     Writes nothing to the model store. NOT the heavy full-diagnostics \
                     import (that one is 🛠 above).",
                )
                .clicked()
            {
                match self.synth_radar.to_config() {
                    Err(message) => self.import_message = Some(message),
                    Ok(config) => {
                        if let Some(files) = rfd::FileDialog::new()
                            .set_title("Choose raw wrfout file(s) — multi-select builds a loop")
                            .pick_files()
                        {
                            self.launch_synthetic_radar(files, config);
                        }
                    }
                }
            }
            if ui
                .add_enabled(!busy, egui::Button::new("🌩 …whole folder"))
                .on_hover_text(
                    "Same FAST simulated-radar path, over EVERY wrfout in a folder — one \
                     loop frame per file / forecast time, sorted by model time.",
                )
                .clicked()
            {
                match self.synth_radar.to_config() {
                    Err(message) => self.import_message = Some(message),
                    Ok(config) => {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_title("Choose a folder of raw wrfout files to simulate")
                            .pick_folder()
                        {
                            let files = crate::wrf_process::wrf_files_in_folder(&dir);
                            if files.is_empty() {
                                self.import_message =
                                    Some(format!("No WRF files under {}", dir.display()));
                            } else {
                                self.launch_synthetic_radar(files, config);
                            }
                        }
                    }
                }
            }
        });
        self.synthetic_radar_site_panel(ui);
    }

    /// Spawn the heavy full-diagnostics processing job (after any size gate).
    #[cfg(any(windows, target_os = "macos"))]
    fn launch_heavy_import(
        &mut self,
        files: Vec<PathBuf>,
        options: crate::wrf_process::WrfProcessOptions,
    ) {
        let count = files.len();
        let task = crate::wrf_process::spawn_process_paths(files, self.store_root.clone(), options);
        self.import_message = Some(format!("Processing {count} WRF file(s)…"));
        self.import_job = Some(ImportJob::Process(task));
    }

    /// Spawn the fast simulated-radar job over the picked file set.
    #[cfg(any(windows, target_os = "macos"))]
    fn launch_synthetic_radar(
        &mut self,
        files: Vec<PathBuf>,
        config: crate::wrf_radar::SyntheticRadarConfig,
    ) {
        let count = files.len();
        let task = crate::wrf_radar::spawn_synthetic_radar(files, config);
        self.import_message = Some(if count == 1 {
            "Simulating radar from 1 WRF file…".to_string()
        } else {
            format!("Simulating radar loop from {count} WRF files…")
        });
        self.import_job = Some(ImportJob::SyntheticRadar(task));
    }

    /// Inline size-aware confirmation for a parked heavy import: the warning,
    /// a fast core-only alternative, an explicit "start anyway", and cancel.
    #[cfg(any(windows, target_os = "macos"))]
    fn heavy_import_warning_ui(&mut self, ui: &mut egui::Ui) {
        let Some(pending) = &self.pending_heavy_import else {
            return;
        };
        let warning = pending.warning.clone();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(
                egui::RichText::new("⚠ Large WRF import")
                    .strong()
                    .color(egui::Color32::from_rgb(0xd0, 0x8a, 0x30)),
            );
            ui.label(egui::RichText::new(warning).small());
            ui.label(
                egui::RichText::new(
                    "Tip: narrow the field selection with “🛠 WRF full-diagnostics fields” \
                     above, start core-only, or — if you just want to SEE the storm — use \
                     “🌩 Simulated radar from WRF (fast)” below instead.",
                )
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                if ui
                    .button("Start core-only (faster)")
                    .on_hover_text(
                        "Process ONLY the core surface fields + sounding volumes — skips \
                         the severe/thermo diagnostic suite and raw extras. Does not \
                         change your saved field selection.",
                    )
                    .clicked()
                    && let Some(pending) = self.pending_heavy_import.take()
                {
                    let options = crate::wrf_process::WrfProcessOptions {
                        core_fields: true,
                        diagnostics: false,
                        heavy_ecape: false,
                        raw_extras: false,
                        only: Vec::new(),
                        skip: Vec::new(),
                    };
                    self.launch_heavy_import(pending.files, options);
                }
                if ui
                    .button("Start full anyway")
                    .on_hover_text("Run the full selection above — this can take a while.")
                    .clicked()
                    && let Some(pending) = self.pending_heavy_import.take()
                {
                    let options = self.wrf_options.to_options();
                    self.launch_heavy_import(pending.files, options);
                }
                if ui.button("Cancel").clicked() {
                    self.pending_heavy_import = None;
                }
            });
        });
    }

    /// Compact "Virtual radar site & range" popover for the simulated-radar
    /// import: where the antenna stands (domain centre / explicit lat-lon /
    /// real NEXRAD site id via the app's site catalog) and the optional
    /// max-range + gate-spacing overrides. Edits mutate `self.synth_radar`,
    /// which the 🌩 buttons read and `persist_wrf_synth_radar` (app side)
    /// saves.
    #[cfg(any(windows, target_os = "macos"))]
    fn synthetic_radar_site_panel(&mut self, ui: &mut egui::Ui) {
        let busy = self.import_job.is_some();
        let state = &mut self.synth_radar;
        egui::CollapsingHeader::new("📡 Virtual radar site & range")
            .id_salt("wrf_synth_radar_site")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Where the simulated antenna stands and how far it scans. Applies \
                         to the next “🌩 Simulated radar from WRF” run. The antenna sits \
                         on the model terrain at the chosen spot (+10 m tower).",
                    )
                    .small()
                    .weak(),
                );
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut state.placement,
                            SynthPlacement::DomainCenter,
                            "Domain center",
                        )
                        .on_hover_text("Antenna at the WRF domain centre (the default).");
                        ui.selectable_value(
                            &mut state.placement,
                            SynthPlacement::LatLon,
                            "Lat/Lon",
                        )
                        .on_hover_text("Type an explicit antenna latitude / longitude.");
                        ui.selectable_value(
                            &mut state.placement,
                            SynthPlacement::NexradSite,
                            "NEXRAD site",
                        )
                        .on_hover_text(
                            "Place the virtual antenna at a real radar site (e.g. KTLX), \
                             resolved through the app's site catalog — compare the model \
                             directly against what that radar would see.",
                        );
                    });
                    match state.placement {
                        SynthPlacement::DomainCenter => {}
                        SynthPlacement::LatLon => {
                            ui.horizontal(|ui| {
                                ui.label("Lat:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.lat_text)
                                        .hint_text("e.g. 46.62")
                                        .desired_width(80.0),
                                );
                                ui.label("Lon:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.lon_text)
                                        .hint_text("e.g. -97.60")
                                        .desired_width(80.0),
                                );
                            });
                        }
                        SynthPlacement::NexradSite => {
                            ui.horizontal(|ui| {
                                ui.label("Site id:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.site_id_text)
                                        .hint_text("e.g. KTLX")
                                        .desired_width(70.0),
                                );
                                // Live resolution preview so a typo shows
                                // before launch, not as a failed run.
                                let id = state.site_id_text.trim().to_ascii_uppercase();
                                if !id.is_empty() {
                                    match data_source::sites::resolve(
                                        &data_source::sites::SiteRef::Us {
                                            level2_id: id.clone(),
                                        },
                                    ) {
                                        Some(record) => {
                                            let coords = record
                                                .lat_lon
                                                .map(|(lat, lon)| {
                                                    format!(" — {lat:.3}°, {lon:.3}°")
                                                })
                                                .unwrap_or_default();
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "✔ {}{coords}",
                                                    record.label
                                                ))
                                                .small()
                                                .weak(),
                                            );
                                        }
                                        None => {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "✘ “{id}” not in the site catalog"
                                                ))
                                                .small()
                                                .color(egui::Color32::from_rgb(0xd0, 0x8a, 0x30)),
                                            );
                                        }
                                    }
                                }
                            });
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.label("Max range:");
                        ui.add(
                            egui::DragValue::new(&mut state.max_range_km)
                                .range(
                                    SyntheticRadarUiState::MIN_RANGE_KM
                                        ..=SyntheticRadarUiState::MAX_RANGE_KM,
                                )
                                .speed(5)
                                .suffix(" km"),
                        )
                        .on_hover_text(
                            "Scan radius. 230 km is the classic WSR-88D Doppler range; up \
                             to 1000 km for a wide CONUS-style circle.",
                        );
                        ui.checkbox(&mut state.auto_gate_spacing, "Auto gates")
                            .on_hover_text(
                                "Coarsen gate spacing proportionally with range (keeps the \
                                 classic 920-gate count), so a wide circle costs the same \
                                 memory as the 230 km default.",
                            );
                        if !state.auto_gate_spacing {
                            ui.add(
                                egui::DragValue::new(&mut state.gate_spacing_m)
                                    .range(
                                        SyntheticRadarUiState::MIN_GATE_M
                                            ..=SyntheticRadarUiState::MAX_GATE_M,
                                    )
                                    .speed(10)
                                    .suffix(" m"),
                            )
                            .on_hover_text("Gate spacing (default 250 m).");
                        }
                    });
                    let spacing = state.effective_gate_spacing_m();
                    let gates = (state.clamped_range_km() * 1000.0 / spacing).floor();
                    ui.label(
                        egui::RichText::new(format!(
                            "{:.0} km range at {spacing:.0} m gates → {gates:.0} gates/radial",
                            state.clamped_range_km()
                        ))
                        .small()
                        .weak(),
                    );
                    if ui
                        .button("Reset to defaults")
                        .on_hover_text("Domain centre, 230 km range, 250 m gates.")
                        .clicked()
                    {
                        *state = SyntheticRadarUiState::default();
                    }
                });
            });
    }

    /// Collapsible options popover for the heavy WRF ingest: toggle the product
    /// GROUPS (core / diagnostics / heavy eCAPE / raw extras) and optionally
    /// narrow with free-text ONLY / SKIP field filters, then a live preview of
    /// exactly which store fields the selection will write. Edits mutate
    /// `self.wrf_options`, which the "WRF full diagnostics…" button reads and
    /// `persist_wrf_process_options` (app side) saves.
    #[cfg(any(windows, target_os = "macos"))]
    fn wrf_options_panel(&mut self, ui: &mut egui::Ui) {
        let busy = self.import_job.is_some();
        let opts = &mut self.wrf_options;
        egui::CollapsingHeader::new("🛠 WRF full-diagnostics fields")
            .id_salt("wrf_full_diag_fields")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Pick what the “WRF full diagnostics…” import writes. Toggle product \
                         groups, and/or narrow to specific fields with the filters below.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_enabled_ui(!busy, |ui| {
                    ui.checkbox(&mut opts.core_fields, "Core surface fields + sounding")
                        .on_hover_text(
                            "T2/Td2/RH, 10 m winds, MSLP, surface pressure, PWAT, composite \
                             reflectivity, UH, precip, terrain — plus the isobaric sounding \
                             volumes that feed the skew-T.",
                        );
                    ui.checkbox(&mut opts.diagnostics, "Severe / thermo diagnostics")
                        .on_hover_text(
                            "getvar 2D diagnostics: CAPE/CIN, SRH, bulk shear, STP/SCP/EHI, \
                             LCL/LFC/EL, and the rest of the severe suite.",
                        );
                    ui.checkbox(&mut opts.heavy_ecape, "Heavy eCAPE (slow)")
                        .on_hover_text(
                            "Entrainment-CAPE family (sbeCAPE/mleCAPE/mueCAPE, eCAPE-STP/EHI/SCP). \
                             Off by default — noticeably slower on large grids.",
                        );
                    ui.checkbox(&mut opts.raw_extras, "Raw model extras")
                        .on_hover_text(
                            "Raw wrfout fields pulled verbatim: PBLH, surface fluxes (HFX/LH), \
                             radiation (SWDOWN/GLW/OLR), skin/sea-surface temps, snow/graupel.",
                        );
                    ui.horizontal(|ui| {
                        ui.label("Only:");
                        ui.add(
                            egui::TextEdit::singleline(&mut opts.only_text)
                                .hint_text("e.g. sbcape, srh, shear")
                                .desired_width(200.0),
                        )
                        .on_hover_text(
                            "Optional allow-list: process ONLY fields whose name matches one of \
                             these tokens (comma/space separated). Empty = no restriction.",
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Skip:");
                        ui.add(
                            egui::TextEdit::singleline(&mut opts.skip_text)
                                .hint_text("e.g. ecape, olr")
                                .desired_width(200.0),
                        )
                        .on_hover_text(
                            "Optional deny-list: drop any field whose name matches one of these \
                             tokens (comma/space separated). Applied after Only.",
                        );
                    });
                    if ui
                        .button("Reset to defaults")
                        .on_hover_text("Everything but heavy eCAPE — the classic import.")
                        .clicked()
                    {
                        *opts = WrfProcessUiState::default();
                    }
                });

                // Live preview of the resulting store fields, so the user sees
                // the selection is narrowed (not the full default set).
                let planned = opts.to_options().planned_store_fields();
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("{} field(s) will be written", planned.len()))
                        .small()
                        .strong(),
                );
                if planned.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "Nothing selected — enable a group or widen the Only filter.",
                        )
                        .small()
                        .color(egui::Color32::from_rgb(0xd0, 0x8a, 0x30)),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("wrf_planned_fields")
                        .max_height(110.0)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(planned.join(", ")).small().weak());
                        });
                }
            });
    }

    /// Non-desktop fallback: native folder dialogs (rfd) are unavailable, so
    /// there is nothing to pick here.
    #[cfg(not(any(windows, target_os = "macos")))]
    fn import_pickers(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("Folder import needs Windows or macOS.")
                .small()
                .weak(),
        );
    }

    /// Step the selected forecast hour within the current run; the viewer
    /// re-requests its current variable automatically when the hour lands.
    pub fn step_hour(&mut self, delta: i64) {
        let Some(current) = self.browser.selected().cloned() else {
            return;
        };
        let Some(tree) = &self.tree else {
            return;
        };
        let hours: Vec<u16> = tree
            .models
            .iter()
            .find(|m| m.model == current.model)
            .and_then(|m| m.runs.iter().find(|r| r.run == current.run))
            .map(|r| r.hours.iter().map(|h| h.hour).collect())
            .unwrap_or_default();
        let Some(position) = hours.iter().position(|&h| h == current.hour) else {
            return;
        };
        let next = position as i64 + delta;
        if next < 0 || next as usize >= hours.len() {
            return;
        }
        let key = HourKey {
            model: current.model,
            run: current.run,
            hour: hours[next as usize],
        };
        self.browser.select(key.clone());
        self.select_hour(key);
    }

    /// Model slug of the hour selected in the store browser — what
    /// `request_sounding_at` would sample. Callers holding grid coords
    /// from a specific model's LUT use this to detect cross-model
    /// mismatches in mixed hrrr+gfs stores.
    pub fn browsed_hour_model(&self) -> Option<String> {
        self.viewer.hour().map(|hour| hour.model.clone())
    }

    /// Request a sounding at storage-order grid coordinates (map click).
    pub fn request_sounding_at(&mut self, fx: f64, fy: f64) {
        if let Some(hour) = self.viewer.hour().cloned() {
            self.sounding.set_loading();
            self.worker
                .send(StoreRequest::LoadSounding { hour, fx, fy });
        }
    }

    /// Request a sounding from an EXPLICIT run/hour (independent of the
    /// browser selection) — used by callers that must not be stale.
    pub fn request_sounding_for(&mut self, hour: HourKey, fx: f64, fy: f64) {
        self.sounding.set_loading();
        self.worker
            .send(StoreRequest::LoadSounding { hour, fx, fy });
    }

    /// The hour key in the NEWEST run COVERING `target` whose valid time
    /// is closest to it — run slugs parse as "YYYYMMDD_HHz", valid =
    /// run + fhr. Era guard: a run is only eligible when `target` falls
    /// inside its plausible forecast coverage (init <= target <= init +
    /// the model's max forecast horizon), so a mixed archive+live store
    /// never pins a 2013 event time to today's run — or a live time to
    /// an archived event's run. Returns None when no run covers `target`.
    /// Returns (key, valid time, run age at `target`).
    ///
    /// `preferred_model` pins the lookup to one model's runs (callers
    /// holding grid coordinates from a specific model's LUT must not mix
    /// grids in an hrrr+gfs store); `None` keeps the historical
    /// first-model behavior.
    pub fn newest_hour_valid_near(
        &self,
        target: chrono::DateTime<chrono::Utc>,
        preferred_model: Option<&str>,
    ) -> Option<(HourKey, chrono::DateTime<chrono::Utc>, chrono::Duration)> {
        let tree = self.tree.as_ref()?;
        let model = match preferred_model {
            Some(slug) => tree.models.iter().find(|entry| entry.model == slug)?,
            None => tree.models.first()?,
        };
        // Runs are sorted newest first (StoreTree contract), so the first
        // run covering `target` is the newest eligible one.
        let (run, run_time) = model.runs.iter().find_map(|run| {
            let run_time = model_run_time_utc(&run.run)?;
            let horizon = chrono::Duration::hours(model_max_forecast_horizon_hours(
                &model.model,
                chrono::Timelike::hour(&run_time) as u8,
            ));
            (run_time <= target && target <= run_time + horizon).then_some((run, run_time))
        })?;
        let best = run.hours.iter().min_by_key(|hour| {
            (run_time + chrono::Duration::hours(hour.hour as i64) - target)
                .num_seconds()
                .abs()
        })?;
        let valid = run_time + chrono::Duration::hours(best.hour as i64);
        Some((
            HourKey {
                model: model.model.clone(),
                run: run.run.clone(),
                hour: best.hour,
            },
            valid,
            target - run_time,
        ))
    }

    /// Select the newest-run forecast hour valid closest to `target`.
    /// Existing map layers auto-refresh when their variable lands, so this
    /// is the bridge used by BowEcho's unified timeline player.
    pub fn select_newest_hour_valid_near(
        &mut self,
        target: chrono::DateTime<chrono::Utc>,
        preferred_model: Option<&str>,
    ) -> Option<(
        HourKey,
        chrono::DateTime<chrono::Utc>,
        chrono::Duration,
        bool,
    )> {
        let (key, valid, run_age) = self.newest_hour_valid_near(target, preferred_model)?;
        let changed = self.select_hour_key(key.clone());
        Some((key, valid, run_age, changed))
    }

    /// The dock body — call inside an egui Window/panel. Returns false when
    /// the user asked to close.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.handle_responses();

        egui::Panel::left("model_runs")
            .resizable(true)
            .default_size(230.0)
            .show_inside(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.strong("Runs");
                    if ui.button("⟳").on_hover_text("Re-scan the store").clicked() {
                        self.worker.send(StoreRequest::Enumerate);
                    }
                });
                ui.label(
                    egui::RichText::new(self.store_root.display().to_string())
                        .small()
                        .weak(),
                );
                ui.separator();
                self.import_controls(ui);
                ui.separator();
                let mut picked = None;
                match &self.tree {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("scanning store…");
                        });
                    }
                    Some(tree) if tree.models.is_empty() => {
                        ui.label(format!(
                            "No model runs under\n{}",
                            self.store_root.display()
                        ));
                        ui.label(
                            egui::RichText::new(
                                "Run rusty-weather ingest, or point the store path at an rw-store directory.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                    Some(tree) => {
                        let browser = &mut self.browser;
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            picked = browser.ui(ui, tree);
                        });
                    }
                }
                if let Some(key) = picked {
                    self.select_hour(key);
                }
            });

        if self.sounding.has_content() {
            egui::Panel::right("model_sounding")
                .resizable(true)
                .default_size(520.0)
                .show_inside(ui, |ui| {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.strong("Sounding");
                        if ui.button("✕").on_hover_text("Close sounding").clicked() {
                            self.sounding.clear();
                        }
                    });
                    ui.separator();
                    self.sounding.ui(ui);
                });
        }

        egui::CentralPanel::default().show_inside(ui, |ui| {
            if self.latest_field.is_some() {
                ui.horizontal(|ui| {
                    if ui
                        .button("Show on radar map")
                        .on_hover_text(
                            "Render this field as a layer under the radar (opacity in Layers)",
                        )
                        .clicked()
                    {
                        self.map_request = self.latest_field.clone();
                    }
                    ui.toggle_value(&mut self.show_plot_viewer, "🗺 Native plot")
                        .on_hover_text(
                            "Render the selected field through rusty-weather's native plot \
                             pipeline. Shift-drag a box on the field viewer to plot a custom \
                             domain; drag a selection corner to rotate.",
                        );
                    ui.toggle_value(&mut self.show_color_tables, "🎨 Color tables")
                        .on_hover_text(
                            "Edit model field-plot color tables: bind a product to a palette, \
                             edit its levels and colors; the field reloads with your palette — \
                             in the dock viewer AND on the radar-map layer, where your binding \
                             outranks the built-in Solar WRF palettes.",
                        );
                });
            }
            match self.viewer.ui(ui) {
                Some(FieldViewerEvent::VarSelected(var)) => {
                    self.viewer.set_loading(&var);
                    if let Some(field) = self.viewer.wanted_field() {
                        self.worker.send(StoreRequest::LoadField(field));
                    }
                }
                Some(FieldViewerEvent::PointClicked { fx, fy }) => {
                    if let Some(hour) = self.viewer.hour().cloned() {
                        self.sounding.set_loading();
                        self.worker
                            .send(StoreRequest::LoadSounding { hour, fx, fy });
                    }
                }
                // v0.2.3 custom-domain plot: shift-drag a box on the field
                // viewer to select an arbitrary plot domain, or drag a corner
                // to rotate it. Open the native plot viewer and retarget it.
                Some(FieldViewerEvent::DomainSelected(domain)) => {
                    self.show_plot_viewer = true;
                    self.plot_viewer.set_active_domain(domain);
                }
                Some(FieldViewerEvent::DomainRotationChanged { rotation_deg }) => {
                    self.show_plot_viewer = true;
                    self.plot_viewer.set_active_domain_rotation(rotation_deg);
                }
                None => {}
            }
        });

        // v0.2.3 custom-domain native plot, as a floating window. Rendered
        // after the field viewer so a domain selected this frame shows at once.
        // `current_field()` borrows `self.viewer` immutably while the closure
        // holds `&mut self.plot_viewer` — disjoint fields, so this is sound.
        if self.show_plot_viewer {
            let field = self.viewer.current_field();
            let mut open = true;
            egui::Window::new("🗺 Native plot")
                .open(&mut open)
                .default_size([560.0, 440.0])
                .show(ui.ctx(), |ui| {
                    self.plot_viewer.ui(ui, field);
                });
            if !open {
                self.show_plot_viewer = false;
            }
        }

        // v0.2.3 editable color tables. `current_field()` borrows `self.viewer`
        // for the panel, so it is scoped and dropped BEFORE apply — which
        // reloads the field and thus needs `self.viewer` mutably.
        if self.show_color_tables {
            let mut open = true;
            let mut changed = false;
            {
                let field = self.viewer.current_field();
                egui::Window::new("🎨 Color tables")
                    .open(&mut open)
                    .default_size([520.0, 520.0])
                    .show(ui.ctx(), |ui| {
                        self.color_tables.ui(ui, field);
                        changed = self.color_tables.take_changed();
                    });
            }
            if changed {
                self.apply_color_table_changes();
            }
            if !open {
                self.show_color_tables = false;
            }
        }
    }
}

/// Max forecast horizon (hours past init) a stored run can plausibly
/// cover — the last supported forecast hour from the model's ingest spec
/// (`rustwx_models::supported_forecast_hours`). Unknown store slugs fall
/// back to the longest built-in horizon (GFS/GEFS, 384 h) so the era
/// guard still separates archive runs from live ones.
fn model_max_forecast_horizon_hours(model: &str, cycle_hour_utc: u8) -> i64 {
    model
        .parse::<rustwx_core::ModelId>()
        .ok()
        .and_then(|id| {
            rustwx_models::supported_forecast_hours(id, cycle_hour_utc)
                .last()
                .copied()
        })
        .map_or(384, i64::from)
}

fn model_run_time_utc(run: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let (date, cycle) = run.split_once('_')?;
    let naive = chrono::NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
    let cycle_hour: u32 = cycle.trim_end_matches('z').parse().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
        naive.and_hms_opt(cycle_hour, 0, 0)?,
        chrono::Utc,
    ))
}

/// Whether these 🎨 editor settings bind an explicit user palette to
/// `field`'s product — mirroring the lookup the store worker performs in
/// `StyleOverrideSettings::style_for_store_variable` when it resolves
/// `FieldData::style`: candidate product keys → bound table → the table
/// compiles to a usable style. When this returns true, `field.style` IS the
/// user's table, so the map layer must let its production colormap win over
/// the built-in Solar WRF palette (audit #11: user edits were silently
/// repainted with Solar).
///
/// The editor keys bindings by the field's var name
/// (`normalize_product_key(field.key.var)`), which the candidate list always
/// contains; the worker-side stored selector only ADDS derived-slug aliases
/// the editor UI cannot create, so a `Null` selector resolves every binding
/// this app can produce.
pub(crate) fn user_style_override_active(
    settings: &StyleOverrideSettings,
    field: &rw_ui::FieldData,
) -> bool {
    let candidates =
        rw_ui::style_overrides::product_candidates(&field.key.var, &serde_json::Value::Null);
    settings
        .bound_table_for_candidates(&candidates)
        .is_some_and(|(_, table)| table.to_store_style(&field.key.var, &field.units).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn model_run_time_parses_operational_run_slug() {
        assert_eq!(
            model_run_time_utc("20260618_03z"),
            Some(chrono::Utc.with_ymd_and_hms(2026, 6, 18, 3, 0, 0).unwrap())
        );
        assert_eq!(model_run_time_utc("bad-run"), None);
    }

    fn override_test_field(var: &str) -> rw_ui::FieldData {
        rw_ui::FieldData {
            key: rw_ui::FieldKey {
                hour: HourKey {
                    model: "wrf".to_owned(),
                    run: "20260519_00z".to_owned(),
                    hour: 0,
                },
                var: var.to_owned(),
            },
            units: "K".to_owned(),
            nx: 1,
            ny: 1,
            values: vec![273.15],
            range: Some((273.15, 273.15)),
            grid: None,
            lat_descending: true,
            style: None,
        }
    }

    /// Audit #11: detection of a 🎨 editor binding must mirror the store
    /// worker's `style_for_store_variable` lookup — bound product wins,
    /// unbound products don't, the `wrf_`-stripped alias matches, and a
    /// deleted table leaves no dangling override.
    #[test]
    fn color_table_editor_binding_is_a_user_override() {
        let mut settings = StyleOverrideSettings::default();
        let field = override_test_field("temperature_2m");
        assert!(!user_style_override_active(&settings, &field));

        // Bind a user table to the product exactly as the editor does
        // (keyed by the normalized var name).
        settings.upsert_table(rw_ui::UserColorTable::simple("My temp", "My temp", "K"));
        settings.bind_product(&rw_ui::normalize_product_key("temperature_2m"), "My temp");
        assert!(user_style_override_active(&settings, &field));

        // A different variable stays on its default (keeps Solar).
        assert!(!user_style_override_active(
            &settings,
            &override_test_field("dewpoint_2m")
        ));

        // Worker parity: a binding on the `wrf_`-stripped alias also
        // resolves for the prefixed store variable.
        settings.bind_product("srh1", "My temp");
        assert!(user_style_override_active(
            &settings,
            &override_test_field("wrf_srh1")
        ));

        // Deleting the table prunes its bindings — no dangling override.
        settings.remove_table("My temp");
        assert!(!user_style_override_active(&settings, &field));
    }

    #[test]
    fn model_max_forecast_horizon_follows_the_ingest_spec() {
        assert_eq!(model_max_forecast_horizon_hours("hrrr", 0), 48);
        assert_eq!(model_max_forecast_horizon_hours("hrrr", 17), 18);
        assert_eq!(model_max_forecast_horizon_hours("gfs", 12), 384);
        // Unknown store slugs keep the era guard working via the fallback.
        assert_eq!(model_max_forecast_horizon_hours("mystery-model", 0), 384);
    }

    /// StoreTree contract: runs sorted descending (newest first),
    /// hours ascending — mirrors rw-ui's StoreView::enumerate.
    fn tree_with_runs(model: &str, runs: &[(&str, &[u16])]) -> StoreTree {
        StoreTree {
            models: vec![rw_ui::ModelEntry {
                model: model.to_owned(),
                runs: runs
                    .iter()
                    .map(|(run, hours)| rw_ui::RunEntry {
                        run: (*run).to_owned(),
                        build: "test".to_owned(),
                        writer_version: "test".to_owned(),
                        nx: 2,
                        ny: 2,
                        hours: hours
                            .iter()
                            .map(|&hour| rw_ui::HourEntry {
                                hour,
                                file: format!("f{hour:03}.rws"),
                                variable_count: 1,
                                written_unix: 0,
                            })
                            .collect(),
                    })
                    .collect(),
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn era_guard_picks_the_run_covering_the_target_time() {
        let ctx = egui::Context::default();
        // Mixed store: today's live run alongside an archived event's run.
        let dock = ModelDataDock::new_for_test(
            &ctx,
            tree_with_runs(
                "hrrr",
                &[("20260618_00z", &[0, 1, 2]), ("20130520_18z", &[0, 1, 2])],
            ),
        );

        // Archive workflow: a 2013 event time must land in the 2013 run.
        let event = chrono::Utc.with_ymd_and_hms(2013, 5, 20, 20, 5, 0).unwrap();
        let (key, valid, run_age) = dock
            .newest_hour_valid_near(event, Some("hrrr"))
            .expect("2013 run covers the event time");
        assert_eq!(key.run, "20130520_18z");
        assert_eq!(key.hour, 2);
        assert_eq!(
            valid,
            chrono::Utc.with_ymd_and_hms(2013, 5, 20, 20, 0, 0).unwrap()
        );
        assert_eq!(run_age, chrono::Duration::minutes(125));

        // Live workflow: a current target must land in the live run, not
        // the archived one.
        let live = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 1, 40, 0).unwrap();
        let (key, _, _) = dock
            .newest_hour_valid_near(live, Some("hrrr"))
            .expect("live run covers the live time");
        assert_eq!(key.run, "20260618_00z");
        assert_eq!(key.hour, 2);

        // A between-eras target is covered by neither run: never silently
        // pin a run whose forecast horizon can't reach the target.
        let uncovered = chrono::Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(dock.newest_hour_valid_near(uncovered, Some("hrrr")), None);
    }

    #[test]
    fn synth_radar_state_round_trips_and_serde_defaults() {
        // Default → JSON → back is identity (what settings persistence does).
        let state = SyntheticRadarUiState::default();
        let value = serde_json::to_value(&state).unwrap();
        let back: SyntheticRadarUiState = serde_json::from_value(value).unwrap();
        assert_eq!(back, state);

        // An EMPTY object (older config / partial entry) restores every
        // default — the serde-defaulted contract.
        let empty: SyntheticRadarUiState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty, SyntheticRadarUiState::default());
        assert_eq!(empty.placement, SynthPlacement::DomainCenter);
        assert_eq!(empty.max_range_km, 230.0);
        assert_eq!(empty.gate_spacing_m, 250.0);
        assert!(empty.auto_gate_spacing);

        // A non-default selection survives the round trip.
        let custom = SyntheticRadarUiState {
            placement: SynthPlacement::NexradSite,
            site_id_text: "KTLX".to_string(),
            max_range_km: 460.0,
            auto_gate_spacing: false,
            gate_spacing_m: 500.0,
            ..SyntheticRadarUiState::default()
        };
        let value = serde_json::to_value(&custom).unwrap();
        let back: SyntheticRadarUiState = serde_json::from_value(value).unwrap();
        assert_eq!(back, custom);
    }

    /// The default selection must produce EXACTLY the historical config
    /// (domain centre, 230 km / 250 m) so shipping this UI changes nothing
    /// for existing users.
    #[test]
    fn synth_radar_default_config_matches_the_historical_default() {
        let config = SyntheticRadarUiState::default().to_config().unwrap();
        let historical = crate::wrf_radar::SyntheticRadarConfig::default();
        assert_eq!(config.site_id, historical.site_id);
        assert_eq!(config.site_lat_deg, None);
        assert_eq!(config.site_lon_deg, None);
        assert_eq!(config.antenna_msl_m, None);
        assert_eq!(config.max_range_m, historical.max_range_m);
        assert_eq!(config.gate_spacing_m, historical.gate_spacing_m);
    }

    /// NEXRAD-id placement resolves through the app's compiled-in site
    /// catalog to the real site coordinates (KTLX Norman / KMVX Fargo-Grand
    /// Forks rows of the embedded Level-II table), case-insensitively; a
    /// typo'd id is an error the import row shows instead of launching.
    #[test]
    fn synth_radar_nexrad_site_resolves_catalog_coords() {
        let state = SyntheticRadarUiState {
            placement: SynthPlacement::NexradSite,
            site_id_text: "ktlx".to_string(),
            ..SyntheticRadarUiState::default()
        };
        let config = state.to_config().expect("KTLX resolves");
        assert_eq!(config.site_id, "KTLX");
        assert!((config.site_lat_deg.unwrap() - 35.3331).abs() < 1e-3);
        assert!((config.site_lon_deg.unwrap() - -97.2777).abs() < 1e-3);
        assert_eq!(
            config.antenna_msl_m, None,
            "antenna stands on model terrain"
        );
        assert!(config.site_name.as_deref().unwrap().contains("KTLX Norman"));

        let kmvx = SyntheticRadarUiState {
            placement: SynthPlacement::NexradSite,
            site_id_text: " KMVX ".to_string(),
            ..SyntheticRadarUiState::default()
        };
        let config = kmvx.to_config().expect("KMVX resolves");
        assert!((config.site_lat_deg.unwrap() - 47.5281).abs() < 1e-3);
        assert!((config.site_lon_deg.unwrap() - -97.3250).abs() < 1e-3);

        for bad in ["", "ZZZZ"] {
            let state = SyntheticRadarUiState {
                placement: SynthPlacement::NexradSite,
                site_id_text: bad.to_string(),
                ..SyntheticRadarUiState::default()
            };
            assert!(state.to_config().is_err(), "{bad:?} must not launch");
        }
    }

    #[test]
    fn synth_radar_latlon_parses_and_validates() {
        let state = SyntheticRadarUiState {
            placement: SynthPlacement::LatLon,
            lat_text: " 46.62 ".to_string(),
            lon_text: "-97.60".to_string(),
            ..SyntheticRadarUiState::default()
        };
        let config = state.to_config().expect("valid lat/lon");
        assert_eq!(config.site_lat_deg, Some(46.62));
        assert_eq!(config.site_lon_deg, Some(-97.60));
        assert_eq!(config.site_id, "WRF", "explicit lat/lon keeps the WRF id");

        for (lat, lon) in [
            ("", "-97.6"),
            ("46.6", "abc"),
            ("95.0", "-97.6"),
            ("46.6", "-200"),
        ] {
            let state = SyntheticRadarUiState {
                placement: SynthPlacement::LatLon,
                lat_text: lat.to_string(),
                lon_text: lon.to_string(),
                ..SyntheticRadarUiState::default()
            };
            assert!(state.to_config().is_err(), "({lat}, {lon}) must not launch");
        }
    }

    /// Range/gate overrides: default preserved, wide ranges auto-coarsen the
    /// gates (constant ~920-gate budget), manual spacing honored, everything
    /// clamped into the supported envelope.
    #[test]
    fn synth_radar_range_and_gate_overrides_scale_and_clamp() {
        // Default: exactly 230 km / 250 m → 920 gates.
        let state = SyntheticRadarUiState::default();
        assert_eq!(state.effective_gate_spacing_m(), 250.0);

        // 1000 km auto: proportionally coarser (~1087 m), same gate count.
        let wide = SyntheticRadarUiState {
            max_range_km: 1000.0,
            ..SyntheticRadarUiState::default()
        };
        let spacing = wide.effective_gate_spacing_m();
        assert!((spacing - 1086.96).abs() < 1.0, "spacing {spacing}");
        let gates = (1000.0 * 1000.0 / spacing).floor() as usize;
        assert_eq!(gates, 920, "auto mode preserves the classic gate budget");

        // 460 km auto = 500 m gates.
        let double = SyntheticRadarUiState {
            max_range_km: 460.0,
            ..SyntheticRadarUiState::default()
        };
        assert_eq!(double.effective_gate_spacing_m(), 500.0);

        // Manual spacing is honored (and clamped).
        let manual = SyntheticRadarUiState {
            max_range_km: 460.0,
            auto_gate_spacing: false,
            gate_spacing_m: 250.0,
            ..SyntheticRadarUiState::default()
        };
        assert_eq!(manual.effective_gate_spacing_m(), 250.0);
        let silly = SyntheticRadarUiState {
            auto_gate_spacing: false,
            gate_spacing_m: 5.0,
            ..SyntheticRadarUiState::default()
        };
        assert_eq!(silly.effective_gate_spacing_m(), 100.0);

        // Range clamps into [50, 1000] km and flows into the config in m.
        let huge = SyntheticRadarUiState {
            max_range_km: 5000.0,
            ..SyntheticRadarUiState::default()
        };
        assert_eq!(huge.clamped_range_km(), 1000.0);
        assert_eq!(huge.to_config().unwrap().max_range_m, 1_000_000.0);
    }

    /// REAL-data proof of the NEXRAD-id placement (project rule: prove on
    /// real data). Gated on `BOWECHO_WRF_RADAR_FIXTURE` = a real Enderlin-
    /// domain wrfout. Places the virtual antenna at KMVX (the real
    /// Fargo/Grand Forks WSR-88D that watched the Enderlin storm), builds one
    /// simulated volume, and asserts the volume is stamped with KMVX's
    /// catalog coordinates and still sees the storm's echo from there. Run in
    /// RELEASE.
    #[test]
    fn real_wrfout_kmvx_placement_stamps_catalog_coords_and_sees_echo() {
        let Some(path) = std::env::var_os("BOWECHO_WRF_RADAR_FIXTURE") else {
            return;
        };
        let state = SyntheticRadarUiState {
            placement: SynthPlacement::NexradSite,
            site_id_text: "kmvx".to_string(),
            ..SyntheticRadarUiState::default()
        };
        let config = state.to_config().expect("KMVX resolves");

        let file = wrf_core::WrfFile::open(std::path::Path::new(&path)).expect("open wrfout");
        let fields = crate::wrf_radar::read_wrf_radar_fields(&file, 0, config.prefer_refl_10cm)
            .expect("read WRF radar fields");
        let time = chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap();
        let volume = crate::wrf_radar::build_synthetic_volume(&fields, time, &config);

        assert_eq!(volume.site.id, "KMVX");
        let lat = volume.site.latitude_deg.unwrap();
        let lon = volume.site.longitude_deg.unwrap();
        assert!((lat - 47.5281).abs() < 1e-3, "site lat {lat}");
        assert!((lon - -97.3250).abs() < 1e-3, "site lon {lon}");

        // The relocated radar still sees the storm (Enderlin is ~100 km from
        // KMVX — well inside the 230 km scan).
        let mut finite_ref = 0usize;
        for cut in &volume.cuts {
            if let Some(grid) = cut.moments.get(&radar_core::MomentType::Reflectivity)
                && let radar_core::MomentStorage::F32(values) = &grid.storage
            {
                finite_ref += values.iter().filter(|value| value.is_finite()).count();
            }
        }
        eprintln!(
            "[kmvx] site ({lat:.4}, {lon:.4}), antenna {:?} m MSL, {finite_ref} echo gates",
            volume.site.elevation_m
        );
        assert!(
            finite_ref > 1000,
            "KMVX-placed scan sees too little echo: {finite_ref}"
        );
    }

    #[test]
    fn era_guard_prefers_the_newest_covering_run() {
        let ctx = egui::Context::default();
        let dock = ModelDataDock::new_for_test(
            &ctx,
            tree_with_runs(
                "hrrr",
                &[
                    ("20260618_00z", &[0, 1]),
                    ("20260617_18z", &[0, 1, 2, 3, 4, 5, 6, 7]),
                ],
            ),
        );
        // Both runs cover 00:40z on the 18th; the newest one wins.
        let target = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 0, 40, 0).unwrap();
        let (key, _, _) = dock
            .newest_hour_valid_near(target, Some("hrrr"))
            .expect("both runs cover the target");
        assert_eq!(key.run, "20260618_00z");
        assert_eq!(key.hour, 1);
    }
}
