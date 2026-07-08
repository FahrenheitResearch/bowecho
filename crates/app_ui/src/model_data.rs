//! Model data dock — rusty-weather's rw-ui panels mounted inside BowEcho.
//!
//! The panels (run browser, false-color field viewer, skew-T sounding) were
//! built to take a `&mut egui::Ui` from any egui host; all store IO runs on
//! rw-ui's own worker thread, so BowEcho's render loop never blocks. The
//! data source is an rw-store directory on disk (produced by rusty-weather
//! ingest, default `C:\Users\drew\rusty-weather\store`).

use eframe::egui;
use rw_ui::{
    ColorTableEditorPanel, CustomDomain, FieldViewerEvent, FieldViewerPanel, HourKey,
    PlotViewerPanel, RunBrowserPanel, SoundingPanel, StoreRequest, StoreResponse, StoreTree,
    StoreView, StoreWorker, StyleOverrideSettings,
};
use std::path::PathBuf;

/// Background loader for the synthesized per-level isobaric map fields
/// (the rw-ui worker cannot read `pressure3d` planes).
mod iso_fields;

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
    /// NetCDF. Streams per-stage progress messages then a `Done`, same shape
    /// as the heavy path — on a 250 m grid this is legitimately minutes of
    /// wrf-core compute per file, and a bare spinner reads as a hang.
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
    /// Gate texture on REFLECTIVITY: deterministic speckle on the simulated
    /// dBZ so it reads like real Level-II gates instead of a smooth model
    /// field. ON by default (owner: the smooth field "looks garbage without"
    /// it); an older config with no entry restores today's default (ON).
    #[serde(default = "wrf_default_true")]
    ref_gate_texture: bool,
    /// Gate texture on VELOCITY: a gentle ±0.5 m/s Vr wobble. OFF by default
    /// and kept opt-in — the clean forward-modelled Vr feeds the velocity
    /// dealias / GBVTD tools downstream, so a noisy Vr would pollute them. An
    /// older config with no entry restores OFF.
    #[serde(default)]
    vel_gate_texture: bool,
    /// Reflectivity operator: the model's own Thompson `REFL_10CM`
    /// (model native, the historical default) or the classic Stoelinga
    /// `CALCDBZ` community diagnostic. An older config with no entry restores
    /// model native.
    #[serde(default)]
    reflectivity_operator: crate::wrf_radar::ReflectivityOperator,
    /// Opt-in extra 0.1° tilt below the standard 0.5° lowest tilt (the
    /// community exports start here). Off restores the classic ladder.
    #[serde(default)]
    include_low_tilt: bool,
    /// Ground-clutter amount, 0.0..=1.0 (shown as a 0–100% slider). Our operator
    /// is pure physics with zero clutter; this dials in a fabricated near-radar
    /// ground-return look (the community WRF→GR2 export's `add_ground_clutter`).
    /// 0 (the default; an older config with no entry restores it) is the clean
    /// physics; 1 ≈ the community-script intensity.
    #[serde(default)]
    clutter_intensity: f32,
    /// Realistic Nyquist: fold the simulated radial velocity like a real
    /// pulse-pair radar (alias into `[-fold_nyquist_mps, +fold_nyquist_mps)`) so
    /// VEL folds instead of showing the true unfolded wind projection. OFF by
    /// default (an older config with no entry restores it) — the native Vr is the
    /// exact truth and a wide 320 m/s Nyquist is stamped so nothing folds.
    #[serde(default)]
    fold_velocity: bool,
    /// The folding Nyquist (m/s) when `fold_velocity` is on — the drag value in
    /// the popover, clamped to [`Self::MIN_FOLD_NYQUIST_MPS`]..=[`Self::MAX_FOLD_NYQUIST_MPS`].
    /// An older config with no entry restores the default 25 m/s.
    #[serde(default = "default_fold_nyquist_mps")]
    fold_nyquist_mps: f32,
}

fn default_synth_range_km() -> f64 {
    230.0
}

fn default_synth_gate_m() -> f64 {
    250.0
}

fn default_fold_nyquist_mps() -> f32 {
    crate::wrf_radar::DEFAULT_FOLD_NYQUIST_MPS
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
            ref_gate_texture: true,
            vel_gate_texture: false,
            reflectivity_operator: crate::wrf_radar::ReflectivityOperator::default(),
            include_low_tilt: false,
            clutter_intensity: 0.0,
            fold_velocity: false,
            fold_nyquist_mps: default_fold_nyquist_mps(),
        }
    }
}

impl SyntheticRadarUiState {
    const MIN_RANGE_KM: f64 = 50.0;
    const MAX_RANGE_KM: f64 = 1000.0;
    const MIN_GATE_M: f64 = 100.0;
    const MAX_GATE_M: f64 = 4000.0;
    /// Folding-Nyquist drag bounds (m/s): a sane real-radar range from a fast
    /// dual-PRF low value up to a coarse single-PRF high Nyquist.
    const MIN_FOLD_NYQUIST_MPS: f32 = 8.0;
    const MAX_FOLD_NYQUIST_MPS: f32 = 64.0;
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
            ref_gate_texture: self.ref_gate_texture,
            vel_gate_texture: self.vel_gate_texture,
            reflectivity_operator: self.reflectivity_operator,
            elevations_deg: crate::wrf_radar::elevation_ladder(self.include_low_tilt),
            clutter_intensity: self.clutter_intensity.clamp(0.0, 1.0),
            fold_velocity: self.fold_velocity,
            // The folding Nyquist, clamped to the sane drag range. Inert when
            // `fold_velocity` is off (the library stamps the historical 320 and
            // folds nothing), but always set so the data fingerprint tracks the
            // dial and a re-import rebuilds when it moves.
            nyquist_mps: self
                .fold_nyquist_mps
                .clamp(Self::MIN_FOLD_NYQUIST_MPS, Self::MAX_FOLD_NYQUIST_MPS),
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

/// A LIGHT import parked behind the same confirm-first flow: even the light
/// path interpolates every 3-D sounding field through wrf-core, which on a
/// 250 m grid is legitimately minutes per file — the owner hit exactly this
/// as an anonymous multi-minute spinner. (Dead-code allowance: only the
/// desktop import UI constructs/reads it.)
#[allow(dead_code)]
struct PendingLightImport {
    files: Vec<PathBuf>,
    warning: String,
}

/// LARGE-import thresholds shared by the heavy and light confirm steps (both
/// paths crunch the full 3-D grid through wrf-core).
#[allow(dead_code)]
const LARGE_WRF_WARN_CELLS_3D: usize = 10_000_000;
#[allow(dead_code)]
const LARGE_WRF_WARN_FILE_BYTES: u64 = 1 << 30; // 1 GiB

/// Cheap shared size probe: describe a LARGE WRF import target, or `None`
/// when it looks small enough to just run. File sizes come from
/// `fs::metadata` (free); grid dims from opening ONE file's header
/// (`WrfFile::open` reads dimensions only — no field decompression), so this
/// is safe on the UI thread right after the folder dialog. (Dead-code
/// allowance: only the desktop import UI calls it.)
#[allow(dead_code)]
fn wrf_import_size_description(files: &[PathBuf]) -> Option<String> {
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
    if cells_3d < LARGE_WRF_WARN_CELLS_3D && max_bytes < LARGE_WRF_WARN_FILE_BYTES {
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
        "{} across {} file(s)",
        parts.join(", "),
        files.len()
    ))
}

/// Size gate for the heavy full-diagnostics import. (Dead-code allowance:
/// only the desktop import UI calls it.)
#[allow(dead_code)]
fn heavy_import_size_warning(files: &[PathBuf]) -> Option<String> {
    Some(format!(
        "{}. Full diagnostics computes the ~117-field 2-D suite through \
         wrf-core — MINUTES per file and many GB of RAM on a grid this size. \
         The whole machine may feel heavily loaded while it runs; save other \
         work first.",
        wrf_import_size_description(files)?
    ))
}

/// Size gate for the LIGHT import (same thresholds as the heavy path): the
/// 2D surface fields are cheap, but the isobaric sounding volumes interpolate
/// every 3-D field through wrf-core. (Dead-code allowance: only the desktop
/// import UI calls it.)
#[allow(dead_code)]
fn light_import_size_warning(files: &[PathBuf]) -> Option<String> {
    Some(format!(
        "{}. Even this light import interpolates every 3-D sounding field to \
         37 isobaric levels through wrf-core — expect minutes per file and \
         several GB of RAM on a grid this size. The whole machine may feel \
         heavily loaded while it runs; save other work first.",
        wrf_import_size_description(files)?
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
    /// drag a box on the field viewer, or rotate a corner — or draw the box on
    /// the radar map via the 📐 arm button / Ctrl+Shift+drag). Shown as a
    /// floating window when `show_plot_viewer` is set.
    plot_viewer: PlotViewerPanel,
    show_plot_viewer: bool,
    /// v0.30 RC4: the (model, run) whose NATIVE domain extent was last
    /// seeded as the plot viewer's active domain (see
    /// [`native_plot_domain`]). One-shot per run — after seeding, the
    /// panel's own domain controls (including its "Full grid" choice) are
    /// never fought.
    native_plot_seeded_run: Option<(String, String)>,
    /// v0.29.3 gesture-collision fix: while true, the NEXT drag on the radar
    /// map draws the plot domain — no modifiers, and none of the map's other
    /// gestures (pan, loupe, soundings, 3D box) fire. Armed by the 📐 button
    /// below; cleared by Esc / right-click / re-click / a completed box (the
    /// map-side state machine lives in `main.rs`, this is the single truth).
    plot_domain_armed: bool,
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
    /// A light import awaiting the same explicit confirmation (see
    /// [`light_import_size_warning`]).
    // Read only by the rfd-gated (windows/macos) import UI.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    pending_light_import: Option<PendingLightImport>,
    /// Store-variable names of the hour currently in the viewer, captured
    /// from its `HourVars` response. Load routing + display translation
    /// guard: a name that IS a real store variable always loads through the
    /// rw-ui worker (and keeps its own name), even if it happens to look
    /// like a synthesized iso-level slug; only synthesized names go to the
    /// iso plane loader.
    hour_store_vars: Vec<String>,
    /// In-flight background load of a synthesized per-level isobaric field
    /// (one at a time; drained in [`Self::poll_iso_load`]).
    iso_load: Option<iso_fields::IsoFieldLoadTask>,
    /// Newest iso-level load requested while one was in flight (slug-named;
    /// latest wins — mirrors the rw-ui worker's request coalescing).
    iso_load_pending: Option<rw_ui::FieldKey>,
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
            native_plot_seeded_run: None,
            plot_domain_armed: false,
            color_tables: ColorTableEditorPanel::new(),
            show_color_tables: false,
            wrf_options: WrfProcessUiState::default(),
            synth_radar: SyntheticRadarUiState::default(),
            pending_heavy_import: None,
            pending_light_import: None,
            hour_store_vars: Vec::new(),
            iso_load: None,
            iso_load_pending: None,
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
            self.request_field_load(field);
        }
    }

    /// Route a picker load: synthesized iso-level names go to the
    /// background plane loader (the rw-ui worker's `LoadField` reads
    /// `surface2d` tiles and cannot serve a `pressure3d` plane), everything
    /// else to the worker as before. `wanted` is display-named, exactly as
    /// [`FieldViewerPanel::wanted_field`] produces it; callers have already
    /// set the loading state.
    fn request_field_load(&mut self, wanted: rw_ui::FieldKey) {
        let store = store_field_key(wanted);
        match iso_route(&store.var, &self.hour_store_vars) {
            Some(spec) => {
                if self.iso_load.is_some() {
                    // One volume read at a time; the newest request wins
                    // when the in-flight one lands (worker-style
                    // coalescing, so hour scrubbing never queues a backlog).
                    self.iso_load_pending = Some(store);
                } else {
                    self.iso_load = Some(iso_fields::spawn_load(
                        self.store_root.clone(),
                        store,
                        spec,
                        self.color_tables.settings().clone().normalized(),
                        self.repaint.clone(),
                    ));
                }
            }
            None => self.worker.send(StoreRequest::LoadField(store)),
        }
    }

    /// Drain the iso-level plane loader — the loader-side mirror of the
    /// worker's `Field` response handling: stale results (no longer the
    /// viewer's wanted field) are dropped, `latest_field` keeps the slug
    /// (store-style) name for map layers / Solar palettes / 🎨 bindings,
    /// and only the viewer's copy carries the display label.
    fn poll_iso_load(&mut self) {
        let Some(task) = &self.iso_load else {
            return;
        };
        let result = match task.rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("Isobaric field loader stopped unexpectedly".to_string())
            }
        };
        let key = self.iso_load.take().expect("checked above").key;
        // Start the coalesced follow-up first, so a scrub burst keeps
        // exactly one read in flight with no idle gap.
        if let Some(pending) = self.iso_load_pending.take()
            && let Some(spec) = color_tables::parse_iso_slug(&pending.var)
        {
            self.iso_load = Some(iso_fields::spawn_load(
                self.store_root.clone(),
                pending,
                spec,
                self.color_tables.settings().clone().normalized(),
                self.repaint.clone(),
            ));
        }
        let display = display_field_key(key, &self.hour_store_vars);
        if self.viewer.wanted_field().as_ref() != Some(&display) {
            return; // stale — the selection moved on while we read
        }
        match result {
            Ok(mut field) => {
                attach_solar_fallback_style(&mut field, &self.hour_store_vars);
                self.latest_field = Some(std::sync::Arc::new(field.clone()));
                field.key = display;
                self.viewer.set_field(field);
            }
            Err(message) => {
                self.viewer.set_error(message);
            }
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
        self.poll_iso_load();
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
                        // Raw wrf_* vars show their catalog labels in the
                        // picker, and the hour's `*_iso` sounding volumes
                        // gain per-level 2-D entries ("Temperature 850 mb");
                        // the load below translates back to store names /
                        // iso planes (`request_field_load`).
                        self.hour_store_vars = vars.iter().map(|var| var.name.clone()).collect();
                        self.viewer.set_hour(key, viewer_display_vars(vars));
                        if let Some(field) = self.viewer.wanted_field() {
                            self.viewer.set_loading(&field.var);
                            self.request_field_load(field);
                        }
                    }
                }
                StoreResponse::HourVars(_, Err(message)) => {
                    self.viewer.set_error(message);
                }
                StoreResponse::Field(key, boxed) => match *boxed {
                    Ok(mut field) => {
                        // `latest_field` keeps the STORE name: every consumer
                        // outside the dock (map layers, Solar palette
                        // resolution, 🎨 bindings, OA) stays keyed by real
                        // store variables. Only the viewer's copy carries the
                        // display label, so its stale-check matches the
                        // label-named selection.
                        attach_solar_fallback_style(&mut field, &self.hour_store_vars);
                        self.latest_field = Some(std::sync::Arc::new(field.clone()));
                        field.key = display_field_key(field.key, &self.hour_store_vars);
                        self.viewer.set_field(field);
                    }
                    Err(message) => {
                        let key = display_field_key(key, &self.hour_store_vars);
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

    /// Whether the 📐 "Draw plot box" arm is on (the next radar-map drag
    /// draws the plot domain). Read by the map input routing every frame.
    pub fn plot_domain_armed(&self) -> bool {
        self.plot_domain_armed
    }

    /// Arm/disarm the map-side plot-box tool (Esc / right-click / a closed
    /// Model window disarm through here).
    pub fn set_plot_domain_armed(&mut self, armed: bool) {
        self.plot_domain_armed = armed;
    }

    /// A plot domain drawn on the RADAR MAP (📐 arm button or the
    /// Ctrl+Shift+drag shortcut): retarget the native plot viewer at it and
    /// auto-disarm — one box per arming, mirroring the field-viewer
    /// `DomainSelected` path in [`Self::ui`].
    pub fn apply_map_plot_domain(&mut self, domain: CustomDomain) {
        self.plot_domain_armed = false;
        self.show_plot_viewer = true;
        self.plot_viewer.set_active_domain(domain);
    }

    /// v0.30 RC4 fix — seed the run's NATIVE extent as the plot viewer's
    /// active domain, once per (model, run) and only while no domain is
    /// active. The pinned rw-ui panel derives its canvas aspect ONLY from
    /// an active domain; a domain-less "Full grid" plot uses a fixed
    /// default-wide canvas, which crammed the owner's square 800×800 local
    /// domain against a wall of dead whitespace. With the seed, the
    /// native-domain plot rides exactly the drawn-box request path
    /// (extent-derived aspect). Wide CONUS-scale runs seed nothing
    /// ([`native_plot_domain`]) and keep today's Full-grid request
    /// untouched; the one-shot marker means the panel's own domain
    /// controls (including "Full grid") are never fought afterwards.
    fn seed_native_plot_domain(&mut self, field: &rw_ui::FieldData) {
        let run = (field.key.hour.model.clone(), field.key.hour.run.clone());
        if self.native_plot_seeded_run.as_ref() == Some(&run) {
            return;
        }
        self.native_plot_seeded_run = Some(run);
        if self.plot_viewer.active_domain().is_none()
            && let Some(domain) = native_plot_domain(field)
        {
            self.plot_viewer.set_active_domain(domain);
        }
    }

    #[cfg(test)]
    pub(crate) fn plot_viewer_shown_for_test(&self) -> bool {
        self.show_plot_viewer
    }

    #[cfg(test)]
    pub(crate) fn active_plot_domain_for_test(&self) -> Option<&CustomDomain> {
        self.plot_viewer.active_domain()
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
            Some(ImportJob::Local(task)) => {
                // Drain the whole backlog and show only the newest progress
                // line — same pattern as the heavy path below.
                let mut latest = None;
                let mut done = None;
                loop {
                    match task.rx.try_recv() {
                        Ok(crate::local_import::LocalImportMessage::Progress(message)) => {
                            latest = Some(message);
                        }
                        Ok(crate::local_import::LocalImportMessage::Done(outcome)) => {
                            done = Some(outcome);
                            break;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            done = Some(Err("Import worker stopped unexpectedly".to_string()));
                            break;
                        }
                    }
                }
                match done {
                    Some(Ok(summary)) => PollResult::Finished {
                        message: format!(
                            "Imported {} hour(s) from {} file(s) → run “{}” ({} variables){}",
                            summary.hours_written,
                            summary.files_seen,
                            summary.run,
                            summary.variables.len(),
                            if summary.notes.is_empty() {
                                String::new()
                            } else {
                                format!(" — note: {}", summary.notes.join("; "))
                            }
                        ),
                    },
                    Some(Err(error)) => PollResult::Finished {
                        message: format!("Import failed: {error}"),
                    },
                    None => match latest {
                        Some(message) => PollResult::Progress(message),
                        None => PollResult::Progress(String::new()),
                    },
                }
            }
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
    ) -> Option<(String, u64, Vec<std::sync::Arc<radar_core::RadarVolume>>)> {
        let output = self.synthetic_radar_result.take()?;
        Some((output.label, output.config_fingerprint, output.volumes))
    }

    /// True while a WRF/NetCDF import or synthetic-radar build runs on the dock
    /// worker. The app pauses live radar auto-refresh while this holds: the
    /// owner's machine has hard-crashed under the combined all-core memory-
    /// bandwidth load of a large WRF import plus concurrent live-radar decode
    /// (the import workers already run below-normal priority). Only the network
    /// fetch pauses — the loop keeps playing existing frames.
    pub(crate) fn import_in_flight(&self) -> bool {
        self.import_job.is_some()
    }

    /// Test-only: park a dummy import job so the live-refresh crash guard
    /// (`ViewerApp::model_import_in_flight`) engages without spawning a worker.
    #[cfg(test)]
    pub(crate) fn mark_import_in_flight_for_test(&mut self) {
        let (_tx, rx) = std::sync::mpsc::channel();
        let task = crate::wrf_radar::SyntheticRadarTask {
            label: "test import".to_owned(),
            rx,
        };
        self.import_job = Some(ImportJob::SyntheticRadar(task));
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
                self.gate_or_launch_light_import(vec![file]);
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
                    self.gate_or_launch_light_import(files);
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
        self.light_import_warning_ui(ui);

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

    /// Light-path counterpart of the heavy size gate: park a LARGE selection
    /// behind [`Self::light_import_warning_ui`], launch small ones directly.
    #[cfg(any(windows, target_os = "macos"))]
    fn gate_or_launch_light_import(&mut self, files: Vec<PathBuf>) {
        if let Some(warning) = light_import_size_warning(&files) {
            self.import_message = None;
            self.pending_light_import = Some(PendingLightImport { files, warning });
        } else {
            self.launch_light_import(files);
        }
    }

    /// Spawn the light import job (after any size gate).
    #[cfg(any(windows, target_os = "macos"))]
    fn launch_light_import(&mut self, files: Vec<PathBuf>) {
        let message = if let [file] = files.as_slice() {
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.display().to_string());
            format!("Importing {name}…")
        } else {
            format!("Importing {} file(s)…", files.len())
        };
        let task = crate::local_import::spawn_import_paths(files, self.store_root.clone());
        self.import_message = Some(message);
        self.import_job = Some(ImportJob::Local(task));
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

    /// Inline size-aware confirmation for a parked LIGHT import: same
    /// confirm-first flow as [`Self::heavy_import_warning_ui`]. On a 250 m
    /// grid even the light path is minutes of wrf-core compute per file, so
    /// the user is told BEFORE it starts — with the pointer to the fast
    /// simulated-radar path for radar-style browsing.
    #[cfg(any(windows, target_os = "macos"))]
    fn light_import_warning_ui(&mut self, ui: &mut egui::Ui) {
        let Some(pending) = &self.pending_light_import else {
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
                    "Tip: if you just want radar-style browsing of this run, “🌩 \
                     Simulated radar from WRF (fast)” below is the fast path — seconds \
                     per file, loops in the radar view. Import here only for store \
                     fields + skew-T soundings; progress shows below while it runs.",
                )
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                if ui
                    .button("Import anyway")
                    .on_hover_text(
                        "Run the light import: 2D surface fields + isobaric sounding \
                         volumes, one forecast hour per file. Per-stage progress shows \
                         under the import buttons.",
                    )
                    .clicked()
                    && let Some(pending) = self.pending_light_import.take()
                {
                    self.launch_light_import(pending.files);
                }
                if ui.button("Cancel").clicked() {
                    self.pending_light_import = None;
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
                    ui.checkbox(&mut state.ref_gate_texture, "Gate texture (reflectivity)")
                        .on_hover_text(
                            "Add subtle, deterministic gate-to-gate speckle (a couple of \
                             dBZ, correlated along the radial) so the simulated reflectivity \
                             reads like real Level-II gates instead of a smooth model field. \
                             On by default. Off = the classic smooth look.",
                        );
                    ui.checkbox(
                        &mut state.vel_gate_texture,
                        "Velocity gate texture (±0.5 m/s wobble)",
                    )
                    .on_hover_text(
                        "Add a gentle ±0.5 m/s wobble to the simulated radial velocity. Off \
                         by default and kept opt-in: the clean forward-modelled Vr feeds the \
                         velocity dealias / GBVTD tools, and a noisy Vr would pollute them.",
                    );
                    ui.add(
                        egui::Slider::new(&mut state.clutter_intensity, 0.0..=1.0)
                            .text("Ground clutter")
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                    )
                    .on_hover_text(
                        "Simulate near-radar ground clutter: fabricated ground return \
                         concentrated within ~40 km on the lowest tilts, fading with range and \
                         beam height, with a few random hotspots — the look of the community \
                         WRF→GR2 export. Our operator is pure physics (no clutter) at 0%; 100% \
                         ≈ the community-script intensity. Clutter fills only gates weaker than \
                         it, so storms are never overwritten, and cluttered gates read \
                         near-zero velocity (stationary ground). Deterministic per forecast \
                         frame — a loop does not shimmer.",
                    );
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut state.fold_velocity,
                            "Realistic Nyquist (velocity folds)",
                        )
                        .on_hover_text(
                            "Fold the simulated radial velocity like a real pulse-pair radar: \
                             each gate is aliased into ±the Nyquist at right, so fast winds \
                             wrap around the velocity scale instead of reading the true value. \
                             Off (the default) stamps a wide 320 m/s Nyquist and shows the \
                             exact unfolded wind. Either way the plain VEL product renders the \
                             data as stored; DVEL, DSRV, or the 'Auto-dealias VEL' toggle unfold \
                             the folded field — a dealias practice ground on known ground truth.",
                        );
                        ui.add_enabled(
                            state.fold_velocity,
                            egui::DragValue::new(&mut state.fold_nyquist_mps)
                                .range(
                                    SyntheticRadarUiState::MIN_FOLD_NYQUIST_MPS
                                        ..=SyntheticRadarUiState::MAX_FOLD_NYQUIST_MPS,
                                )
                                .speed(0.5)
                                .suffix(" m/s"),
                        )
                        .on_hover_text(
                            "Nyquist velocity for the fold. Typical WSR-88D Doppler \
                             Nyquists run ~8-33 m/s; velocity wraps every twice this value.",
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Reflectivity:");
                        ui.selectable_value(
                            &mut state.reflectivity_operator,
                            crate::wrf_radar::ReflectivityOperator::ModelNative,
                            "Model native (REFL_10CM)",
                        )
                        .on_hover_text(
                            "Render the model's own Thompson 10-cm reflectivity (REFL_10CM) \
                             when the file carries it. Hotter/fatter cores in graupel and \
                             the melting layer — the model's native look. Falls back to \
                             computed dBZ if the file has no REFL_10CM.",
                        );
                        ui.selectable_value(
                            &mut state.reflectivity_operator,
                            crate::wrf_radar::ReflectivityOperator::ClassicStoelinga,
                            "Classic Stoelinga (community look)",
                        )
                        .on_hover_text(
                            "Always compute dBZ (Stoelinga 2005 / CALCDBZ, fixed \
                             Marshall-Palmer intercepts) even when the file carries \
                             REFL_10CM — matching the community wrf-python / GR2Analyst \
                             pipeline. Roughly 10–20 dB cooler in graupel/melting regions, \
                             so hooks stand out of moderate echo.",
                        );
                    });
                    ui.checkbox(
                        &mut state.include_low_tilt,
                        "Include 0.1° low tilt (community lowest tilt)",
                    )
                    .on_hover_text(
                        "Prepend a 0.1° tilt below the standard 0.5° lowest tilt, like the \
                         community exports. The lower beam samples roughly half the height \
                         at range, so a low-level hook is better defined. Adds one sweep to \
                         every volume. Off = the classic 0.5° lowest tilt.",
                    );
                    if ui
                        .button("Reset to defaults")
                        .on_hover_text(
                            "Domain centre, 230 km range, 250 m gates, textured reflectivity \
                             gates, clean velocity, model native reflectivity, no extra low \
                             tilt, no ground clutter, true unfolded velocity (no realistic \
                             Nyquist).",
                        )
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
                             domain; drag a selection corner to rotate. To draw the box on \
                             the radar map instead, arm 📐 Draw plot box (or Ctrl+Shift+drag \
                             the map).",
                        );
                    // v0.29.3 gesture-collision fix: Shift-drag on the MAP is
                    // contested (loupe, inspector pin, Shift+right-drag 3D
                    // box), so map-side domain drawing gets an explicit arm.
                    ui.toggle_value(&mut self.plot_domain_armed, "📐 Draw plot box")
                        .on_hover_text(
                            "Arm the radar map: the next click-drag on the map draws the \
                             custom plot domain — no modifier keys, and nothing else fires \
                             (no pan, loupe, sounding, or 3D box). Esc, right-click, or \
                             clicking this again cancels; a completed box disarms. \
                             Shortcut: Ctrl+Shift+drag the map.",
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
                        // The viewer selects by display label; real store
                        // vars load through the worker, synthesized iso
                        // levels through the plane loader.
                        self.request_field_load(field);
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
        // The STORE-NAMED twin of the viewer's field goes in (the native plot
        // pipeline styles by store variable, and the viewer's copy may carry
        // a display label).
        if self.show_plot_viewer {
            // Cloned out as an owned Arc (cheap) so the RC4 domain seed
            // below can borrow `self` mutably before the window closure.
            let field: Option<std::sync::Arc<rw_ui::FieldData>> = store_named_current_field(
                &self.viewer,
                self.latest_field.as_deref(),
                &self.hour_store_vars,
            )
            .is_some()
            .then(|| self.latest_field.clone())
            .flatten();
            if let Some(field) = &field {
                self.seed_native_plot_domain(field);
            }
            let mut open = true;
            egui::Window::new("🗺 Native plot")
                .open(&mut open)
                .default_size([560.0, 440.0])
                .show(ui.ctx(), |ui| {
                    self.plot_viewer.ui(ui, field.as_deref());
                });
            if !open {
                self.show_plot_viewer = false;
            }
        }

        // v0.2.3 editable color tables. The STORE-NAMED twin goes in so the
        // editor's product bindings stay keyed by real store variables (the
        // worker resolves overrides against those). The borrow is scoped and
        // dropped BEFORE apply — which reloads the field and thus needs
        // `self.viewer` mutably.
        if self.show_color_tables {
            let mut open = true;
            let mut changed = false;
            {
                let field = store_named_current_field(
                    &self.viewer,
                    self.latest_field.as_deref(),
                    &self.hour_store_vars,
                );
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

/// Display-time picker names: swap each raw `wrf_*` store variable for its
/// friendly catalog label (`color_tables::wrf_fields`) before the list
/// reaches the rw-ui field viewer, whose combo shows `name (units)` verbatim,
/// and append the synthesized per-level entries for the hour's `*_iso`
/// sounding volumes ([`iso_level_entries`]). Canonical and unknown names pass
/// through untouched. Nothing on disk is renamed — existing imported stores
/// get the labels AND the upper-air fields without a re-import.
fn viewer_display_vars(mut vars: Vec<rw_ui::VarInfo>) -> Vec<rw_ui::VarInfo> {
    let synthesized = iso_level_entries(&vars);
    for var in &mut vars {
        if let Some(label) = color_tables::wrf_display_label(&var.name) {
            var.name = label.to_owned();
        }
    }
    vars.extend(synthesized);
    vars
}

/// Synthesized per-level picker entries for an hour's `*_iso` sounding
/// volumes (field-major, high→low pressure): plain `Surface2D` `VarInfo`s
/// whose names are the [`color_tables::iso_levels`] LABELS, so the pinned
/// rw-ui picker lists them like any 2-D variable and echoes the label back
/// on load. A level is offered only when every source volume carries it
/// (wind speed needs u AND v), and an entry is skipped when the hour
/// already has a REAL variable claiming the slug or its `hpa`-suffixed
/// spelling (the downloaded models' extracted per-level 2-D fields) — real
/// store data always wins over synthesis.
fn iso_level_entries(vars: &[rw_ui::VarInfo]) -> Vec<rw_ui::VarInfo> {
    use color_tables::iso_levels::{ISO_PICKER_LEVELS_HPA, IsoLevelField, IsoLevelSpec};
    let volume = |name: &str| {
        vars.iter()
            .find(|var| var.kind == rw_ui::VarKind::Pressure3D && var.name == name)
    };
    let taken = |name: &str| vars.iter().any(|var| var.name == name);
    let mut entries = Vec::new();
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
            entries.push(rw_ui::VarInfo {
                name: spec.label(),
                units: sources[0].units.clone(),
                kind: rw_ui::VarKind::Surface2D,
                levels_hpa: Vec::new(),
            });
        }
    }
    entries
}

/// Inverse of [`viewer_display_vars`] for LOAD requests: the viewer selects
/// by display label, the loads run by store name (real store variables) or
/// synthesized slug (iso levels). Catalog and iso labels always contain a
/// character no store slug can (space/case/arrow — test-enforced in
/// `color_tables`), so a real store name can never be mistranslated here.
fn store_field_key(mut key: rw_ui::FieldKey) -> rw_ui::FieldKey {
    if let Some(store) = color_tables::wrf_store_name_for_label(&key.var) {
        key.var = store.to_owned();
    } else if let Some(spec) = color_tables::parse_iso_label(&key.var) {
        key.var = spec.slug();
    }
    key
}

/// Iso-loader routing: `Some(spec)` when a STORE-NAMED load key denotes a
/// synthesized per-level field — i.e. it parses as an iso slug AND is not a
/// real variable of the hour. The exotic store that genuinely carries a 2-D
/// `temperature_850` keeps loading it through the rw-ui worker.
fn iso_route(store_var: &str, hour_store_vars: &[String]) -> Option<color_tables::IsoLevelSpec> {
    if hour_store_vars.iter().any(|name| name == store_var) {
        return None;
    }
    color_tables::parse_iso_slug(store_var)
}

/// Display name for a store variable when one applies: `wrf_*` catalog
/// labels, then synthesized iso-level labels. A name that is a REAL
/// variable of the hour never takes the iso label (mirror of
/// [`iso_route`]'s guard), so such a variable round-trips as itself.
fn display_var_name(store_var: &str, hour_store_vars: &[String]) -> Option<String> {
    if let Some(label) = color_tables::wrf_display_label(store_var) {
        return Some(label.to_owned());
    }
    if hour_store_vars.iter().any(|name| name == store_var) {
        return None;
    }
    color_tables::parse_iso_slug(store_var).map(|spec| spec.label())
}

/// Store → display for keys headed INTO the viewer: loaded fields and error
/// keys must match the viewer's label-named selection or its stale-response
/// check drops them.
fn display_field_key(mut key: rw_ui::FieldKey, hour_store_vars: &[String]) -> rw_ui::FieldKey {
    if let Some(label) = display_var_name(&key.var, hour_store_vars) {
        key.var = label;
    }
    key
}

/// The store-named twin of the viewer's current field, for panels that must
/// see REAL store variables — the native plot pipeline and the 🎨 editor's
/// product bindings (both key styles/bindings by store name). `latest_field`
/// keeps store names, so hand it out gated on actually being the field the
/// viewer currently shows.
fn store_named_current_field<'a>(
    viewer: &'a FieldViewerPanel,
    latest: Option<&'a rw_ui::FieldData>,
    hour_store_vars: &[String],
) -> Option<&'a rw_ui::FieldData> {
    let current = viewer.current_field()?;
    latest.filter(|latest| {
        latest.key.hour == current.key.hour
            && display_var_name(&latest.key.var, hour_store_vars)
                .as_deref()
                .unwrap_or(&latest.key.var)
                == current.key.var
    })
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

/// v0.30 RC3 fix — give style-less local-WRF fields their Solarpower07
/// palette as the field's OWN style, so the dock FIELD VIEWER shows it.
///
/// Root cause of the "new fields render generic" report: the Solar palettes
/// existed only on the radar-map layer (`ModelMapLayer::model_table`,
/// resolved in main.rs `model_layer_solar_table`). The rw-ui viewer panel
/// knows nothing of them — it paints `field.style`'s production colormap or
/// falls to its normalized viridis ramp. On a local `wrf` store the worker
/// resolves real production styles for CANONICAL variables (`"wrf"` parses
/// to `ModelId::WrfGdex`, which rides the HRRR recipe set), but the raw
/// `wrf_*` passthroughs (7e6fdee) and the synthesized iso-level slugs
/// (5921e5e) resolve None — so exactly the two NEW field families rendered
/// viridis in the viewer while every old field looked styled.
///
/// Called on every loaded field (worker `Field` responses and the iso plane
/// loader) BEFORE `latest_field`/viewer fan-out. Precedence is preserved:
/// a 🎨 user binding or an operational production style arrives already
/// compiled into `field.style` (we never overwrite Some), downloaded models
/// (non-`wrf` slugs) are untouched, and on the MAP the layer's `model_table`
/// still outranks the production colormap this style compiles to — same
/// Solar table, so map pixels are unchanged.
///
/// v0.30 RC4 fix — the compiled style's `title` (what the rw-ui FIELD
/// VIEWER prints as the field heading, and what the native plot titles)
/// carries the field's FRIENDLY display label — the same label the picker
/// shows ([`display_var_name`]: wrf_fields catalog label for raw `wrf_*`
/// vars, iso-level label like "Temperature 850 mb" for synthesized slugs,
/// the store name itself otherwise). The Solar [`color_tables::ColorTable`]
/// name ("Solar Temperature", …) is an internal palette id and must never
/// surface user-facing.
fn attach_solar_fallback_style(field: &mut rw_ui::FieldData, hour_store_vars: &[String]) {
    if field.style.is_some() {
        return;
    }
    // Mirror of main.rs `is_local_wrf_field`: both import paths stamp the
    // store model slug `wrf` (local_import / wrf_process).
    if !field.key.hour.model.to_ascii_lowercase().starts_with("wrf") {
        return;
    }
    let Some(table) = color_tables::solar_model_field_table(&field.key.var, &field.units) else {
        return;
    };
    let title =
        display_var_name(&field.key.var, hour_store_vars).unwrap_or_else(|| field.key.var.clone());
    field.style = solar_table_store_style(&table, &field.units, &title);
}

/// Compile a Solar [`color_tables::ColorTable`] into the store-style form
/// the rw-ui viewer (and `rustwx_render::build_colormap`) consume: bin
/// levels with one color per bin, sampled from the table at bin midpoints
/// (exact for stepped tables, a fine quantization of interpolated ramps).
/// The bins are UNIFORM across the table's span because `LeveledColormap`
/// assigns palette entries by relative VALUE position (matplotlib
/// `ListedColormap` semantics) — non-uniform bins would shear the
/// color↔value correspondence. Extend=Both matches `ColorTable::sample`'s
/// clamp-at-both-ends, and transparent stops (e.g. the reflectivity
/// clear-air mask) carry through as transparent bin colors.
///
/// `title` is the field's friendly display label (see
/// [`attach_solar_fallback_style`]) — it becomes `StoreVariableStyle::title`,
/// the string the viewer panel and native plot print. The table's own name
/// is a palette id, not a title.
fn solar_table_store_style(
    table: &color_tables::ColorTable,
    units: &str,
    title: &str,
) -> Option<rustwx_products::viewer::StoreVariableStyle> {
    let stops = table.stops();
    let lo = f64::from(stops.first()?.value);
    let hi = f64::from(stops.last()?.value);
    if !lo.is_finite() || !hi.is_finite() || hi <= lo {
        return None;
    }
    // 256 uniform bins resolve even the half-degree block transitions the
    // Solar palettes use over their widest (~100-unit) spans.
    const BINS: usize = 256;
    let levels: Vec<f64> = (0..=BINS)
        .map(|step| lo + (hi - lo) * step as f64 / BINS as f64)
        .collect();
    let colors: Vec<[u8; 4]> = levels
        .windows(2)
        .map(|pair| table.sample(((pair[0] + pair[1]) * 0.5) as f32).to_array())
        .collect();
    let user = rw_ui::UserColorTable {
        name: title.to_owned(),
        title: title.to_owned(),
        display_units: units.to_owned(),
        convert: rw_ui::UserUnitConvert::None,
        legend_mode: rw_ui::UserLegendMode::Stepped,
        extend: rw_ui::UserExtendMode::Both,
        mask_below: None,
        tick_step: None,
        levels,
        colors,
    };
    user.to_store_style(title, units)
}

/// v0.30 RC4 fix — the native-domain plot's request extent.
///
/// Owner report: a native plot of his square 800×800 (250 m) domain drew
/// the map as a square crammed into a wide canvas with dead whitespace
/// before the far-right colorbar, while DRAWING a custom plot box over the
/// same data sized correctly. Divergence: the pinned rw-ui
/// `PlotViewerPanel` derives its canvas aspect from the ACTIVE domain's
/// extent (`domain_plot_aspect`), but a domain-less "Full grid" plot uses
/// a fixed default-wide (16:9) canvas. The renderer is pinned, so the fix
/// is what app_ui passes: the run's native grid extent, seeded as the
/// active domain ([`ModelDataDock::ui`]) so it rides the drawn-box request
/// path — square extent → square canvas + colorbar, no dead space.
///
/// Wide (CONUS-scale) extents return `None`: they already fill the default
/// canvas, and only the domain-less path gives them the pinned renderer's
/// full-domain projected frame (the classic CONUS look) — their request
/// params must stay exactly today's. The spans mirror rustwx-products'
/// `full_domain_projected_frame_default` thresholds (lat ≥ 25°, lon ≥ 45°).
fn native_plot_domain(field: &rw_ui::FieldData) -> Option<CustomDomain> {
    let grid = field.grid.as_ref()?;
    let (west, east, south, north) = grid_geographic_extent(&grid.lat, &grid.lon)?;
    if (north - south) >= 25.0 || (east - west) >= 45.0 {
        return None;
    }
    Some(CustomDomain::generated((west, east, south, north)))
}

/// Finite min/max lat/lon of a run grid — the same extent the pinned
/// plot pipeline computes for its domain-less render bounds (rw-ui
/// `geographic_bounds`), so the seeded domain frames exactly the data.
/// `None` when the grid has no finite points.
fn grid_geographic_extent(lat: &[f32], lon: &[f32]) -> Option<(f64, f64, f64, f64)> {
    let mut west = f64::INFINITY;
    let mut east = f64::NEG_INFINITY;
    let mut south = f64::INFINITY;
    let mut north = f64::NEG_INFINITY;
    for (&lat, &lon) in lat.iter().zip(lon) {
        let (lat, lon) = (f64::from(lat), f64::from(lon));
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        south = south.min(lat);
        north = north.max(lat);
        west = west.min(lon);
        east = east.max(lon);
    }
    (west.is_finite() && east.is_finite() && south.is_finite() && north.is_finite())
        .then_some((west, east, south, north))
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

    fn test_var(name: &str, units: &str) -> rw_ui::VarInfo {
        rw_ui::VarInfo {
            name: name.to_owned(),
            units: units.to_owned(),
            kind: rw_ui::VarKind::Surface2D,
            levels_hpa: Vec::new(),
        }
    }

    fn test_hour_key() -> HourKey {
        HourKey {
            model: "wrf".to_owned(),
            run: "local_wrf_19740403_090000".to_owned(),
            hour: 0,
        }
    }

    /// Display-time renaming: raw `wrf_*` names swap to their catalog labels
    /// for the picker (units untouched — the combo appends them), canonical
    /// and unknown names pass through byte-for-byte.
    #[test]
    fn picker_vars_show_catalog_labels_and_pass_unknowns_through() {
        let vars = viewer_display_vars(vec![
            test_var("wrf_swupt", "W m-2"),
            test_var("wrf_hfx", "W m-2"),
            test_var("temperature_2m", "K"),
            test_var("wrf_some_experimental_field", "1"),
        ]);
        let names: Vec<&str> = vars.iter().map(|var| var.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Shortwave ↑ TOA — reflected solar",
                "Sensible heat flux (HFX)",
                "temperature_2m",
                "wrf_some_experimental_field",
            ]
        );
        assert!(
            vars.iter().all(|var| !var.units.is_empty()),
            "units must ride along untouched"
        );
    }

    /// Load-key translation must invert the display rename for EVERY catalog
    /// entry (a picker label that failed to translate back would make the
    /// store worker load a nonexistent variable), and must leave canonical /
    /// unknown keys untouched in both directions.
    #[test]
    fn field_keys_round_trip_between_display_and_store_names() {
        for (store, info) in color_tables::wrf_field_catalog() {
            let displayed = display_field_key(
                rw_ui::FieldKey {
                    hour: test_hour_key(),
                    var: (*store).to_owned(),
                },
                &[],
            );
            assert_eq!(displayed.var, info.label, "{store}: store -> display");
            let restored = store_field_key(displayed);
            assert_eq!(restored.var, *store, "{store}: display -> store");
        }
        for var in ["temperature_2m", "sbcape", "wrf_not_in_the_catalog"] {
            let key = rw_ui::FieldKey {
                hour: test_hour_key(),
                var: var.to_owned(),
            };
            assert_eq!(display_field_key(key.clone(), &[]).var, var);
            assert_eq!(store_field_key(key).var, var);
        }
    }

    fn iso_volume_var(name: &str, units: &str, levels_hpa: &[u16]) -> rw_ui::VarInfo {
        rw_ui::VarInfo {
            name: name.to_owned(),
            units: units.to_owned(),
            kind: rw_ui::VarKind::Pressure3D,
            levels_hpa: levels_hpa.to_vec(),
        }
    }

    /// The canonical 37-level ladder as stored (descending, 1000 first).
    fn canonical_ladder() -> Vec<u16> {
        let mut levels: Vec<u16> = (100..=1000u16).step_by(25).collect();
        levels.reverse();
        levels
    }

    /// Entry synthesis from an hour's variable list: every `*_iso` volume
    /// present yields its curated per-level 2-D entries with the volume's
    /// units, appended as `Surface2D` (the only kind the pinned picker
    /// lists), while the real variables — including the Pressure3D volumes
    /// themselves — pass through untouched.
    #[test]
    fn iso_entries_synthesize_from_the_hour_vars() {
        let ladder = canonical_ladder();
        let vars = viewer_display_vars(vec![
            test_var("temperature_2m", "K"),
            iso_volume_var("temperature_iso", "K", &ladder),
            iso_volume_var("dewpoint_iso", "K", &ladder),
            iso_volume_var("u_iso", "m/s", &ladder),
            iso_volume_var("v_iso", "m/s", &ladder),
            iso_volume_var("height_iso", "gpm", &ladder),
        ]);

        // 4 fields × 6 curated levels appended (no rh_iso in this hour).
        let synthesized: Vec<&rw_ui::VarInfo> = vars
            .iter()
            .filter(|var| var.name.ends_with(" mb"))
            .collect();
        assert_eq!(synthesized.len(), 24);
        assert!(
            synthesized
                .iter()
                .all(|var| var.kind == rw_ui::VarKind::Surface2D && var.levels_hpa.is_empty()),
            "synthesized entries must be plain 2-D vars for the picker"
        );
        let entry = |label: &str| {
            synthesized
                .iter()
                .find(|var| var.name == label)
                .unwrap_or_else(|| panic!("{label} missing"))
        };
        assert_eq!(entry("Temperature 850 mb").units, "K");
        assert_eq!(entry("Dewpoint 700 mb").units, "K");
        assert_eq!(entry("Wind speed 500 mb").units, "m/s");
        assert_eq!(entry("Height 250 mb").units, "gpm");
        assert!(
            !vars.iter().any(|var| var.name.starts_with("RH ")),
            "no rh_iso volume -> no RH entries"
        );
        // Originals ride along: the 2-D field and the raw volumes.
        assert!(vars.iter().any(|var| var.name == "temperature_2m"));
        assert!(
            vars.iter()
                .any(|var| var.name == "temperature_iso" && var.kind == rw_ui::VarKind::Pressure3D)
        );
    }

    /// Synthesis guards: no `*_iso` volumes -> no entries; wind needs BOTH
    /// components; a level is offered only where the volume carries it; a
    /// real variable claiming the slug (or its `hpa` spelling) suppresses
    /// the synthesized twin.
    #[test]
    fn iso_entry_synthesis_respects_presence_and_collisions() {
        // No volumes at all.
        assert!(iso_level_entries(&[test_var("temperature_2m", "K")]).is_empty());

        // u without v: no wind entries, temperature still offered.
        let ladder = canonical_ladder();
        let entries = iso_level_entries(&[
            iso_volume_var("temperature_iso", "K", &ladder),
            iso_volume_var("u_iso", "m/s", &ladder),
        ]);
        assert!(entries.iter().any(|var| var.name == "Temperature 850 mb"));
        assert!(!entries.iter().any(|var| var.name.starts_with("Wind")));

        // A truncated volume (nothing above 500 hPa) skips the missing
        // levels but keeps the present ones.
        let entries = iso_level_entries(&[iso_volume_var(
            "temperature_iso",
            "K",
            &[1000, 925, 850, 700, 500],
        )]);
        let names: Vec<&str> = entries.iter().map(|var| var.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Temperature 925 mb",
                "Temperature 850 mb",
                "Temperature 700 mb",
                "Temperature 500 mb",
            ]
        );

        // Real per-level variables win over synthesis: the exact slug and
        // the downloaded models' `hpa`-suffixed spelling both suppress.
        let entries = iso_level_entries(&[
            iso_volume_var("temperature_iso", "K", &ladder),
            test_var("temperature_850", "K"),
            test_var("temperature_700hpa", "K"),
        ]);
        assert!(!entries.iter().any(|var| var.name == "Temperature 850 mb"));
        assert!(!entries.iter().any(|var| var.name == "Temperature 700 mb"));
        assert!(entries.iter().any(|var| var.name == "Temperature 500 mb"));
    }

    /// The load-key contract for synthesized fields: picker label -> slug
    /// (what the loader and every store-named consumer sees) -> label again
    /// for the viewer; and the real-variable guard on both the display
    /// translation and the loader routing.
    #[test]
    fn iso_keys_round_trip_and_real_variables_keep_the_worker_path() {
        let key = |var: &str| rw_ui::FieldKey {
            hour: test_hour_key(),
            var: var.to_owned(),
        };
        // Label -> slug -> label, for a sample of every field kind.
        for (label, slug) in [
            ("Temperature 850 mb", "temperature_850"),
            ("Dewpoint 700 mb", "dewpoint_700"),
            ("RH 500 mb", "relative_humidity_500"),
            ("Wind speed 300 mb", "wind_speed_300"),
            ("Height 250 mb", "height_250"),
        ] {
            let stored = store_field_key(key(label));
            assert_eq!(stored.var, slug, "{label}: label -> slug");
            assert_eq!(
                display_field_key(stored, &[]).var,
                label,
                "{slug}: slug -> label"
            );
        }
        // Routing: a synthesized slug goes to the iso loader…
        let spec = iso_route("temperature_850", &[]).expect("synthesized route");
        assert_eq!(spec.slug(), "temperature_850");
        assert_eq!(spec.level_hpa, 850);
        // …but a REAL store variable of the same name stays on the worker
        // path and keeps its own display name.
        let hour_vars = vec!["temperature_850".to_owned()];
        assert_eq!(iso_route("temperature_850", &hour_vars), None);
        assert_eq!(
            display_field_key(key("temperature_850"), &hour_vars).var,
            "temperature_850"
        );
        // Non-iso names never route to the loader.
        assert_eq!(iso_route("temperature_2m", &[]), None);
        assert_eq!(iso_route("temperature_850hpa", &[]), None);
    }

    /// Size-gate parity: the light import must warn on the same class of
    /// target the heavy path warns on (file bytes here — a sparse 1.5 GiB
    /// "wrfout"), both texts must carry the shared size description, and the
    /// light text must sell the compute honestly (isobaric interpolation).
    /// Small selections must pass without a gate.
    #[test]
    fn light_and_heavy_size_warnings_share_thresholds() {
        let dir = std::env::temp_dir().join(format!("bowecho-wrf-warn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let small = dir.join("wrfout_small");
        std::fs::write(&small, b"not a real wrfout").unwrap();
        assert_eq!(
            light_import_size_warning(std::slice::from_ref(&small)),
            None
        );
        assert_eq!(
            heavy_import_size_warning(std::slice::from_ref(&small)),
            None
        );

        let big = dir.join("wrfout_big");
        let file = std::fs::File::create(&big).unwrap();
        // Sparse: metadata length crosses the 1 GiB threshold without disk IO.
        file.set_len((1u64 << 30) + (1u64 << 29)).unwrap();
        drop(file);
        let light = light_import_size_warning(std::slice::from_ref(&big)).expect("light gates");
        let heavy = heavy_import_size_warning(std::slice::from_ref(&big)).expect("heavy gates");
        for warning in [&light, &heavy] {
            assert!(
                warning.contains("largest file 1.6 GB") && warning.contains("across 1 file(s)"),
                "size description missing: {warning}"
            );
        }
        assert!(
            light.contains("isobaric levels") && light.contains("minutes per file"),
            "light warning must explain the 3-D interpolation cost: {light}"
        );
        assert!(
            heavy.contains("Full diagnostics"),
            "heavy warning keeps its own cost text: {heavy}"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
    fn import_in_flight_reflects_a_running_import_job() {
        let ctx = egui::Context::default();
        let mut dock =
            ModelDataDock::new_for_test(&ctx, tree_with_runs("wrf", &[("20260707_00z", &[0])]));
        assert!(!dock.import_in_flight(), "idle dock reports no import");
        dock.mark_import_in_flight_for_test();
        assert!(
            dock.import_in_flight(),
            "a running import job is reported so the app pauses live radar decode"
        );
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

    /// v0.29.3 gesture-collision fix: the 📐 arm is one-shot — applying a
    /// map-drawn domain must disarm it, open the native plot viewer, and
    /// retarget the viewer at exactly the drawn bounds (rotation 0; corner
    /// rotation stays a field-viewer gesture).
    #[test]
    fn map_plot_domain_apply_disarms_and_opens_the_viewer() {
        let ctx = egui::Context::default();
        let mut dock =
            ModelDataDock::new_for_test(&ctx, tree_with_runs("hrrr", &[("20260618_00z", &[0])]));
        assert!(!dock.plot_domain_armed(), "arm starts off");
        dock.set_plot_domain_armed(true);
        assert!(dock.plot_domain_armed());
        assert!(!dock.show_plot_viewer);

        dock.apply_map_plot_domain(CustomDomain::generated((-100.0, -95.0, 30.0, 35.0)));

        assert!(!dock.plot_domain_armed(), "a completed box auto-disarms");
        assert!(dock.show_plot_viewer, "the native plot window opens on it");
        let domain = dock
            .plot_viewer
            .active_domain()
            .expect("map box becomes the active plot domain");
        assert_eq!(domain.bounds, (-100.0, -95.0, 30.0, 35.0));
        assert_eq!(domain.rotation_deg, 0.0);
    }

    /// Field with a synthetic run grid spanning `bounds` (west, east,
    /// south, north) — corner values are f32-exact so extent assertions
    /// can be equalities.
    fn plot_seed_field(model: &str, run: &str, bounds: (f64, f64, f64, f64)) -> rw_ui::FieldData {
        let (west, east, south, north) = bounds;
        const N: usize = 9;
        let mut lat = Vec::with_capacity(N * N);
        let mut lon = Vec::with_capacity(N * N);
        for row in 0..N {
            for col in 0..N {
                lat.push((south + (north - south) * row as f64 / (N - 1) as f64) as f32);
                lon.push((west + (east - west) * col as f64 / (N - 1) as f64) as f32);
            }
        }
        let mut field = override_test_field("dewpoint_2m");
        field.key.hour.model = model.to_owned();
        field.key.hour.run = run.to_owned();
        field.nx = N;
        field.ny = N;
        field.values = vec![0.0; N * N];
        field.grid = Some(std::sync::Arc::new(rw_store::grid::GridFile {
            nx: N,
            ny: N,
            lat,
            lon,
            projection: None,
            hash: "plot-seed-test".to_owned(),
        }));
        field
    }

    /// RC4 owner report: the native plot of a square 800×800 local domain
    /// letterboxed into the pinned panel's default-wide canvas. The
    /// request-side derivation must hand the drawn-box path the run's
    /// exact native extent for modest domains — implying a square canvas
    /// for a square domain — and must hand WIDE (CONUS-scale) extents
    /// NOTHING, so their Full-grid request params stay exactly today's.
    #[test]
    fn native_plot_domain_derives_square_extent_and_skips_wide_domains() {
        // (a) The owner's 800×800 @ 250 m shape: ~200 km square around
        // 38.4°N. Extent passes through exactly; the implied canvas aspect
        // (the same cos-weighted ratio the pinned drawn-box path derives)
        // is square.
        let square = plot_seed_field("wrf", "19740608_00z", (-100.0, -97.75, 37.5, 39.25));
        let domain = native_plot_domain(&square).expect("square native domain seeds");
        assert_eq!(domain.bounds, (-100.0, -97.75, 37.5, 39.25));
        assert_eq!(domain.rotation_deg, 0.0);
        assert!(
            domain.name.starts_with("domain "),
            "generated name, same as a drawn box: {}",
            domain.name
        );
        let (west, east, south, north) = domain.bounds;
        let implied_aspect =
            ((east - west) * ((south + north) * 0.5).to_radians().cos()) / (north - south);
        assert!(
            (implied_aspect - 1.0).abs() < 0.05,
            "square domain must imply a square canvas, got {implied_aspect}"
        );

        // (b) Wide extents: CONUS-scale lat span (≥ 25°) or lon span
        // (≥ 45°) — unchanged params vs today (no domain seeded, the
        // pinned Full-grid pipeline keeps its default canvas and
        // full-domain projected frame).
        let lat_wide = plot_seed_field("hrrr", "20260618_00z", (-122.0, -72.0, 21.0, 53.0));
        assert!(native_plot_domain(&lat_wide).is_none());
        let lon_wide = plot_seed_field("hrrr", "20260618_00z", (-120.0, -70.0, 30.0, 44.0));
        assert!(native_plot_domain(&lon_wide).is_none());

        // Degenerate inputs never seed: no grid / no finite points.
        let no_grid = override_test_field("dewpoint_2m");
        assert!(native_plot_domain(&no_grid).is_none());
        let mut nan_grid = plot_seed_field("wrf", "19740608_00z", (-100.0, -97.75, 37.5, 39.25));
        {
            let grid = std::sync::Arc::get_mut(nan_grid.grid.as_mut().expect("grid set"))
                .expect("sole owner");
            grid.lat.fill(f32::NAN);
            grid.lon.fill(f32::NAN);
        }
        assert!(native_plot_domain(&nan_grid).is_none());
    }

    /// RC4: the dock seeds the native extent as the plot domain ONCE per
    /// (model, run) and never fights an existing domain — a drawn box
    /// stays exactly what the user drew, and wide runs stay domain-less.
    #[test]
    fn native_plot_seed_is_one_shot_and_respects_user_domains() {
        let ctx = egui::Context::default();
        let mut dock =
            ModelDataDock::new_for_test(&ctx, tree_with_runs("wrf", &[("19740608_00z", &[0])]));
        let square = plot_seed_field("wrf", "19740608_00z", (-100.0, -97.75, 37.5, 39.25));

        dock.seed_native_plot_domain(&square);
        let seeded = dock
            .plot_viewer
            .active_domain()
            .expect("square native domain becomes the active plot domain");
        assert_eq!(seeded.bounds, (-100.0, -97.75, 37.5, 39.25));

        // A user-drawn box replaces it; re-showing the same run must NOT
        // re-seed over the user's choice.
        dock.apply_map_plot_domain(CustomDomain::generated((-99.0, -98.5, 38.0, 38.5)));
        dock.seed_native_plot_domain(&square);
        assert_eq!(
            dock.plot_viewer
                .active_domain()
                .expect("domain kept")
                .bounds,
            (-99.0, -98.5, 38.0, 38.5),
            "seeding is one-shot per run — user domains win"
        );

        // Wide runs: seeding is a no-op, the Full-grid (domain-less)
        // request keeps today's params.
        let ctx2 = egui::Context::default();
        let mut wide_dock =
            ModelDataDock::new_for_test(&ctx2, tree_with_runs("hrrr", &[("20260618_00z", &[0])]));
        let wide = plot_seed_field("hrrr", "20260618_00z", (-122.0, -72.0, 21.0, 53.0));
        wide_dock.seed_native_plot_domain(&wide);
        assert!(
            wide_dock.plot_viewer.active_domain().is_none(),
            "CONUS-scale native plots stay on the domain-less path"
        );
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
        assert!(
            empty.ref_gate_texture,
            "reflectivity gate texture restores ON (the shipped default)"
        );
        assert!(
            !empty.vel_gate_texture,
            "velocity gate texture restores OFF (opt-in)"
        );
        assert_eq!(
            empty.reflectivity_operator,
            crate::wrf_radar::ReflectivityOperator::ModelNative,
            "reflectivity operator restores model native"
        );
        assert!(!empty.include_low_tilt, "low tilt restores OFF");
        assert_eq!(
            empty.clutter_intensity, 0.0,
            "ground clutter restores 0 (the clean physics)"
        );
        assert!(
            !empty.fold_velocity,
            "realistic Nyquist restores OFF (true unfolded velocity)"
        );
        assert_eq!(
            empty.fold_nyquist_mps,
            crate::wrf_radar::DEFAULT_FOLD_NYQUIST_MPS,
            "folding Nyquist restores the default 25 m/s"
        );

        // A non-default selection survives the round trip.
        let custom = SyntheticRadarUiState {
            placement: SynthPlacement::NexradSite,
            site_id_text: "KTLX".to_string(),
            max_range_km: 460.0,
            auto_gate_spacing: false,
            gate_spacing_m: 500.0,
            ref_gate_texture: false,
            vel_gate_texture: true,
            reflectivity_operator: crate::wrf_radar::ReflectivityOperator::ClassicStoelinga,
            include_low_tilt: true,
            clutter_intensity: 0.5,
            fold_velocity: true,
            fold_nyquist_mps: 33.0,
            ..SyntheticRadarUiState::default()
        };
        let value = serde_json::to_value(&custom).unwrap();
        let back: SyntheticRadarUiState = serde_json::from_value(value).unwrap();
        assert_eq!(back, custom);
    }

    /// The default UI selection must produce EXACTLY the library's default
    /// scan config (domain centre, 230 km / 250 m, reflectivity texture on /
    /// velocity texture off) so the UI and the library agree on the shipped
    /// defaults.
    #[test]
    fn synth_radar_default_config_matches_the_library_default() {
        let config = SyntheticRadarUiState::default().to_config().unwrap();
        let historical = crate::wrf_radar::SyntheticRadarConfig::default();
        assert_eq!(config.site_id, historical.site_id);
        assert_eq!(config.site_lat_deg, None);
        assert_eq!(config.site_lon_deg, None);
        assert_eq!(config.antenna_msl_m, None);
        assert_eq!(config.max_range_m, historical.max_range_m);
        assert_eq!(config.gate_spacing_m, historical.gate_spacing_m);
        assert!(
            config.ref_gate_texture && historical.ref_gate_texture,
            "reflectivity texture ships ON by default"
        );
        assert!(
            !config.vel_gate_texture && !historical.vel_gate_texture,
            "velocity texture is opt-in — default keeps the clean Vr"
        );
        assert_eq!(
            config.reflectivity_operator,
            crate::wrf_radar::ReflectivityOperator::ModelNative,
            "default operator is model native"
        );
        assert_eq!(
            config.elevations_deg,
            crate::wrf_radar::DEFAULT_ELEVATIONS_DEG,
            "default ladder is the classic 0.5° lowest tilt"
        );
        assert!(
            !config.fold_velocity && !historical.fold_velocity,
            "realistic Nyquist is opt-in — default keeps the true unfolded velocity"
        );
        assert_eq!(
            config.stamped_nyquist_mps(),
            crate::wrf_radar::UNFOLDED_NYQUIST_MPS,
            "default (folding off) stamps the historical 320 m/s Nyquist"
        );
        assert_eq!(
            config.nyquist_mps, historical.nyquist_mps,
            "default folding Nyquist matches the library default"
        );
    }

    /// The realistic-Nyquist toggle and its folding-Nyquist drag flow from the
    /// UI state into the scan config: off keeps the true unfolded velocity (320
    /// stamped), on stamps the chosen Nyquist, and the drag is clamped to the
    /// sane [8, 64] m/s range in both modes.
    #[test]
    fn synth_radar_fold_velocity_flows_into_config() {
        // Default: folding off, and the (inert) Nyquist is the library default.
        let default = SyntheticRadarUiState::default().to_config().unwrap();
        assert!(!default.fold_velocity, "default is true unfolded velocity");
        assert_eq!(
            default.stamped_nyquist_mps(),
            crate::wrf_radar::UNFOLDED_NYQUIST_MPS
        );

        // Folding on with an in-range Nyquist: flows through and is the stamp.
        let folded = SyntheticRadarUiState {
            fold_velocity: true,
            fold_nyquist_mps: 33.0,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert!(folded.fold_velocity);
        assert_eq!(folded.nyquist_mps, 33.0);
        assert_eq!(folded.stamped_nyquist_mps(), 33.0);

        // The drag clamps to [MIN, MAX] regardless of the folding toggle.
        let too_high = SyntheticRadarUiState {
            fold_velocity: true,
            fold_nyquist_mps: 200.0,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(
            too_high.nyquist_mps,
            SyntheticRadarUiState::MAX_FOLD_NYQUIST_MPS,
            "clamped to 64 m/s"
        );
        let too_low = SyntheticRadarUiState {
            fold_velocity: true,
            fold_nyquist_mps: 2.0,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(
            too_low.nyquist_mps,
            SyntheticRadarUiState::MIN_FOLD_NYQUIST_MPS,
            "clamped to 8 m/s"
        );

        // Folding OFF with a custom Nyquist: the value is inert (still stamps
        // the historical 320) but the field is set so the fingerprint tracks it.
        let off_custom = SyntheticRadarUiState {
            fold_velocity: false,
            fold_nyquist_mps: 40.0,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(off_custom.nyquist_mps, 40.0);
        assert_eq!(
            off_custom.stamped_nyquist_mps(),
            crate::wrf_radar::UNFOLDED_NYQUIST_MPS,
            "folding off stamps 320 even with a custom Nyquist dialed in"
        );
    }

    /// Both gate-texture checkboxes flow independently into the scan config:
    /// reflectivity texture is ON by default, velocity texture OFF by default,
    /// and either can be toggled without touching the other.
    #[test]
    fn synth_radar_gate_texture_toggles_flow_into_config() {
        // Defaults: reflectivity texture on, velocity texture off.
        let defaults = SyntheticRadarUiState::default().to_config().unwrap();
        assert!(
            defaults.ref_gate_texture,
            "reflectivity texture on by default"
        );
        assert!(
            !defaults.vel_gate_texture,
            "velocity texture off by default"
        );

        // Turn reflectivity texture OFF (the smooth look) — velocity stays off.
        let smooth = SyntheticRadarUiState {
            ref_gate_texture: false,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert!(!smooth.ref_gate_texture);
        assert!(!smooth.vel_gate_texture);

        // Opt into velocity texture — reflectivity texture stays on.
        let state = SyntheticRadarUiState {
            vel_gate_texture: true,
            ..SyntheticRadarUiState::default()
        };
        let config = state.to_config().unwrap();
        assert!(config.ref_gate_texture);
        assert!(config.vel_gate_texture);
        assert_eq!(config.max_range_m, 230_000.0);
        assert_eq!(config.gate_spacing_m, 250.0);
    }

    /// The reflectivity-operator selector and the optional 0.1° low tilt flow
    /// from the UI state into the scan config; unset, they keep the historical
    /// defaults (model native, classic ladder).
    #[test]
    fn synth_radar_operator_and_low_tilt_flow_into_config() {
        use crate::wrf_radar::ReflectivityOperator;

        // Classic Stoelinga + low tilt selected.
        let state = SyntheticRadarUiState {
            reflectivity_operator: ReflectivityOperator::ClassicStoelinga,
            include_low_tilt: true,
            ..SyntheticRadarUiState::default()
        };
        let config = state.to_config().unwrap();
        assert_eq!(
            config.reflectivity_operator,
            ReflectivityOperator::ClassicStoelinga
        );
        assert_eq!(
            config.elevations_deg,
            crate::wrf_radar::elevation_ladder(true)
        );
        assert_eq!(config.elevations_deg[0], crate::wrf_radar::LOW_TILT_DEG);
        assert_eq!(
            config.elevations_deg.len(),
            crate::wrf_radar::DEFAULT_ELEVATIONS_DEG.len() + 1
        );

        // Model native + no low tilt = the historical config.
        let plain = SyntheticRadarUiState {
            reflectivity_operator: ReflectivityOperator::ModelNative,
            include_low_tilt: false,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(
            plain.reflectivity_operator,
            ReflectivityOperator::ModelNative
        );
        assert_eq!(
            plain.elevations_deg,
            crate::wrf_radar::DEFAULT_ELEVATIONS_DEG
        );
    }

    /// The ground-clutter slider flows from the UI state into the scan config
    /// and is clamped to 0..=1; the default (0) keeps the clean physics.
    #[test]
    fn synth_radar_clutter_intensity_flows_into_config() {
        // Default: no clutter.
        let default = SyntheticRadarUiState::default().to_config().unwrap();
        assert_eq!(
            default.clutter_intensity, 0.0,
            "default UI selection is the clean physics (no clutter)"
        );

        // A mid setting flows through unchanged.
        let dialed = SyntheticRadarUiState {
            clutter_intensity: 0.6,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert!((dialed.clutter_intensity - 0.6).abs() < 1e-6);

        // Out-of-range values are clamped to [0, 1].
        let over = SyntheticRadarUiState {
            clutter_intensity: 1.7,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(over.clutter_intensity, 1.0, "clamped to 1.0");
        let under = SyntheticRadarUiState {
            clutter_intensity: -0.5,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(under.clutter_intensity, 0.0, "clamped to 0.0");
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
        let fields =
            crate::wrf_radar::read_wrf_radar_fields(&file, 0, config.reflectivity_operator)
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

    // ── v0.30 RC3 Solar-fallback style seam ────────────────────────────────
    //
    // Fixture mirrors the owner's real 1974 store byte-for-byte where it
    // matters (verified against wrf/local_wrf_19740403_172000/f000.rws):
    // model "wrf", canonical t2m selector, raw passthrough selector
    // {"derived":"wrf_swdnb"} with units "W m-2", and `temperature_iso`
    // (units "K", selector {"field":...,"source":"wrf","vertical":"isobaric"}).

    const STYLE_RUN: &str = "local_wrf_19740403_172000";

    fn style_fixture_grid() -> rustwx_core::LatLonGrid {
        let (nx, ny) = (4usize, 3usize);
        let mut lat = Vec::with_capacity(nx * ny);
        let mut lon = Vec::with_capacity(nx * ny);
        for y in 0..ny {
            for x in 0..nx {
                lat.push(30.0 + 0.1 * y as f32);
                lon.push(-100.0 + 0.1 * x as f32);
            }
        }
        rustwx_core::LatLonGrid::new(rustwx_core::GridShape::new(nx, ny).expect("dims"), lat, lon)
            .expect("grid")
    }

    /// One-hour `wrf` store with a canonical 2-D field (grid carrier), the
    /// raw `wrf_swdnb` passthrough, and a two-level `temperature_iso` volume.
    fn write_style_fixture(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("bowecho-solar-style-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let grid = style_fixture_grid();
        let cells = grid.shape.len();
        let t2m = rustwx_core::SelectedField2D::new(
            rustwx_core::FieldSelector::height_agl(rustwx_core::CanonicalField::Temperature, 2),
            "K",
            grid,
            (0..cells).map(|c| 288.0 + c as f32).collect(),
        )
        .expect("values sized to the grid");
        let swdnb: Vec<f32> = (0..cells).map(|c| 40.0 * c as f32).collect();
        let temp_850: Vec<f32> = (0..cells).map(|c| 270.0 + c as f32).collect();
        let temp_500: Vec<f32> = (0..cells).map(|c| 250.0 + c as f32).collect();
        rw_store::write_hour_from_fields_with_derived(
            &root,
            "wrf",
            STYLE_RUN,
            0,
            &[("temperature_2m", &t2m)],
            &[rw_store::DerivedFieldInput {
                name: "wrf_swdnb",
                units: "W m-2",
                values: &swdnb,
            }],
            &[rw_store::PressureVolumeInput {
                name: "temperature_iso",
                units: "K",
                selector_template: serde_json::json!({
                    "field": "temperature_iso",
                    "source": "wrf",
                    "vertical": "isobaric",
                }),
                levels: vec![(850, temp_850.as_slice()), (500, temp_500.as_slice())],
            }],
            "solar-style-test",
            1_780_000_000,
        )
        .expect("write fixture hour");
        root
    }

    fn style_fixture_hour() -> HourKey {
        HourKey {
            model: "wrf".to_owned(),
            run: STYLE_RUN.to_owned(),
            hour: 0,
        }
    }

    /// Per-channel comparison of the compiled production colormap against
    /// the Solar table it was built from. The style quantizes each stop
    /// interval into ≥ 12 bins, so a small tolerance absorbs the midpoint
    /// sampling; anything looser means the wrong palette.
    #[track_caller]
    fn assert_style_tracks_table(
        style: &rustwx_products::viewer::StoreVariableStyle,
        table: &color_tables::ColorTable,
        probes: &[f32],
    ) {
        let cmap = rustwx_render::build_colormap(&style.scale, style.colormap_options);
        for &probe in probes {
            let want = table.sample(probe);
            let got = cmap.map(f64::from(probe));
            for (channel, (got, want)) in [
                (got.r, want.r),
                (got.g, want.g),
                (got.b, want.b),
                (got.a, want.a),
            ]
            .into_iter()
            .enumerate()
            {
                assert!(
                    got.abs_diff(want) <= 20,
                    "channel {channel} at {probe}: style {got} vs table {want}"
                );
            }
        }
    }

    /// The Solar fallback only claims style-less local-WRF fields: existing
    /// styles (🎨 user bindings / operational production) are never
    /// overwritten, downloaded models and Solar-less variables stay on
    /// their current look byte-for-byte.
    #[test]
    fn solar_fallback_respects_existing_styles_and_model_scope() {
        // Local WRF field with no style -> Solar attaches. The style's
        // title is the field's FRIENDLY label (what the picker shows), not
        // the color table's internal "Solar …" name (RC4 owner report: every
        // raw/iso field titled "Solar Temperature" in the viewer).
        let mut field = override_test_field("temperature_850");
        attach_solar_fallback_style(&mut field, &[]);
        let style = field.style.expect("temperature_850 (K) resolves Solar");
        let table = color_tables::solar_model_field_table("temperature_850", "K")
            .expect("resolver covers the slug");
        assert_eq!(style.title, "Temperature 850 mb");
        assert!(
            !style.title.contains("Solar"),
            "color-table names are palette ids, never user-facing titles"
        );
        assert_style_tracks_table(&style, &table, &[255.0, 270.0, 288.0]);

        // The owner's exact RC4 repro: a 300 mb temperature plot titled
        // "Solar Temperature". The pinned native plot pipeline prints
        // `style.title` verbatim as the generated plot's top-left title
        // (rw-ui `render_field_plot`: `style.title`, or
        // `"{style.title} - {domain.name}"` with an active plot domain) —
        // so the compiled style must already carry the friendly label.
        let mut t300 = override_test_field("temperature_300");
        attach_solar_fallback_style(&mut t300, &[]);
        let t300_style = t300.style.expect("temperature_300 (K) resolves Solar");
        assert_eq!(t300_style.title, "Temperature 300 mb");
        let plot_title = format!("{} - {}", t300_style.title, "domain 38.5 -98.8");
        assert_eq!(plot_title, "Temperature 300 mb - domain 38.5 -98.8");
        assert!(!plot_title.contains("Solar"));

        // A slug that is a REAL hour variable keeps its store name as the
        // title (mirror of the picker's pass-through for unknown/canonical
        // names — no iso label is invented for it).
        let mut real_850 = override_test_field("temperature_850");
        attach_solar_fallback_style(&mut real_850, &["temperature_850".to_owned()]);
        assert_eq!(
            real_850.style.expect("Solar still attaches").title,
            "temperature_850"
        );

        // An existing style is the user's/production truth — untouched.
        let mut styled = override_test_field("temperature_2m");
        styled.style = rw_ui::UserColorTable::simple("My temp", "My temp", "K")
            .to_store_style("temperature_2m", "K");
        attach_solar_fallback_style(&mut styled, &[]);
        assert_eq!(
            styled.style.expect("style kept").title,
            "My temp",
            "existing styles must never be repainted"
        );

        // Downloaded models keep their production/generic behavior.
        let mut hrrr = override_test_field("temperature_850");
        hrrr.key.hour.model = "hrrr".to_owned();
        attach_solar_fallback_style(&mut hrrr, &[]);
        assert!(hrrr.style.is_none(), "non-wrf models are out of scope");

        // A variable with no Solar counterpart keeps the generic ramp.
        let mut orography = override_test_field("orography");
        orography.units = "m".to_owned();
        attach_solar_fallback_style(&mut orography, &[]);
        assert!(orography.style.is_none());
    }

    /// RC3 regression (iso path): a synthesized per-level load, driven
    /// through the dock's REAL routing (`request_field_load` →
    /// `poll_iso_load`), must reach `latest_field` AND the viewer with a
    /// Solar production style. At 1104bb4 `style` stayed `None`, so the
    /// dock viewer painted the normalized viridis ramp — the owner's
    /// "default colormap" report on the 1974 store.
    #[test]
    fn iso_loads_reach_the_viewer_with_solar_styles() {
        let root = write_style_fixture("iso-dock");
        let ctx = egui::Context::default();
        let mut dock = ModelDataDock::new(&ctx, root.clone());
        let hour = style_fixture_hour();
        dock.hour_store_vars = vec![
            "temperature_2m".to_owned(),
            "wrf_swdnb".to_owned(),
            "temperature_iso".to_owned(),
        ];
        // The synthesized entry is the picker selection, so the loader's
        // stale-drop accepts the plane when it lands.
        dock.viewer
            .set_hour(hour.clone(), vec![test_var("Temperature 850 mb", "K")]);
        dock.request_field_load(rw_ui::FieldKey {
            hour,
            var: "Temperature 850 mb".to_owned(),
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while dock.latest_field.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "iso plane load timed out"
            );
            dock.poll_iso_load();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let field = dock.latest_field.as_deref().expect("checked above");
        assert_eq!(field.key.var, "temperature_850", "store-named for the map");
        assert_eq!(field.units, "K", "store-native units");
        let style = field
            .style
            .as_ref()
            .expect("Solar fallback style must be attached to the iso plane");
        // RC4 owner report: the viewer titled every iso field with the
        // COLOR TABLE's internal name ("Solar Temperature"). The style must
        // carry the same friendly label the picker shows instead.
        assert_eq!(style.title, "Temperature 850 mb");
        assert!(
            !style.title.contains("Solar"),
            "palette id must never surface as the field title"
        );
        let table = color_tables::solar_model_field_table("temperature_850", "K")
            .expect("resolver covers the slug");
        assert_style_tracks_table(style, &table, &[271.0, 274.0, 281.0]);

        // The viewer's display-named copy carries the same style — this is
        // what the field viewer actually paints (and titles).
        let viewer_field = dock.viewer.current_field().expect("viewer got the field");
        assert_eq!(viewer_field.key.var, "Temperature 850 mb");
        let viewer_style = viewer_field
            .style
            .as_ref()
            .expect("viewer paints Solar, not viridis");
        assert_eq!(viewer_style.title, "Temperature 850 mb");
        assert!(!viewer_style.title.contains("Solar"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// RC3 regression (worker path): the rw-ui store worker resolves NO
    /// production style for raw `wrf_*` passthroughs ({"derived": ...}
    /// selectors miss the operational catalog) — the app-level gap that let
    /// 7e6fdee's palettes go missing in the viewer. The dock seam must
    /// dress the worker's field in its Solar ramp.
    #[test]
    fn raw_wrf_worker_fields_gain_solar_styles_at_the_dock_seam() {
        let root = write_style_fixture("wrf2d");
        let worker = StoreWorker::spawn(StoreView::new(&root), || {});
        worker.send(StoreRequest::LoadField(rw_ui::FieldKey {
            hour: style_fixture_hour(),
            var: "wrf_swdnb".to_owned(),
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut field = loop {
            assert!(
                std::time::Instant::now() < deadline,
                "worker field load timed out"
            );
            match worker.try_recv() {
                Some(StoreResponse::Field(_, boxed)) => break (*boxed).expect("fixture load"),
                Some(_) => {}
                None => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        };
        assert_eq!(field.units, "W m-2");
        assert!(
            field.style.is_none(),
            "characterizes the worker gap the dock seam exists to fill"
        );

        let hour_store_vars = vec![
            "temperature_2m".to_owned(),
            "wrf_swdnb".to_owned(),
            "temperature_iso".to_owned(),
        ];
        attach_solar_fallback_style(&mut field, &hour_store_vars);
        let style = field.style.expect("Solar radiation ramp attached");
        // RC4: the style titles the field by its wrf_fields catalog label
        // (what the picker shows), never by the palette's "Solar …" name.
        // This string IS the generated plot's title (pinned rw-ui
        // `render_field_plot` prints `style.title` top-left) and the dock
        // viewer's heading.
        assert_eq!(
            style.title,
            color_tables::wrf_display_label("wrf_swdnb").expect("catalog label exists")
        );
        assert!(
            !style.title.contains("Solar"),
            "palette id must never surface as the plot title"
        );
        let table = color_tables::solar_model_field_table("wrf_swdnb", "W m-2")
            .expect("resolver covers the catalog entry");
        assert_style_tracks_table(&style, &table, &[100.0, 550.0, 1000.0]);

        let _ = std::fs::remove_dir_all(&root);
    }
}
