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
    PlotViewerPanel, RunBrowserPanel, StoreRequest, StoreResponse, StoreTree,
    StoreView, StoreWorker, StyleOverrideSettings,
};
use std::path::{Path, PathBuf};

use crate::formula_lab::{
    FormulaLabPanel, FormulaLabSources, FormulaResultSource, FormulaSourceKind,
    RawWrfFormulaSource, StoreFormulaSource,
};
use crate::sat_plot::{SatellitePlotPanel, SatellitePlotSource};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum NativePlotContent {
    #[default]
    Model,
    Satellite,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SoundingRequestMode {
    #[default]
    None,
    Point,
    BoxPending,
    BoxApplied,
}

/// Background loader for the synthesized per-level isobaric map fields
/// (the rw-ui worker cannot read `pressure3d` planes). Crate-visible: the
/// batch plotter (`crate::batch_plots`) reuses `load_level_field` for its
/// per-level iso plane plots.
pub(crate) mod iso_fields;

/// A running local WRF/NetCDF ingest, spawned from the dock's import controls.
/// Both variants write into the same model store the dock browses, so a
/// finished import is picked up by [`ModelDataDock::rescan`] and its runs then
/// sound through the existing skew-T path.
// Constructed only from the native rfd file-dialog UI on supported desktops.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
enum ImportJob {
    /// Native NCAR CM1 scalar-plane import. CM1 has its own inventory,
    /// explicit local-Cartesian placement, exact-time store route, and
    /// provenance sidecar; it never passes through the WRF fallback reader.
    Cm1(crate::cm1_ui::Cm1ImportTask),
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
    /// Exact observed-geometry replay. The retained observed volume, generated
    /// synthetic volume, and exact-gate differences are installed together in
    /// the radar viewer's validation workspace.
    SyntheticRadarReplay(crate::wrf_radar::SyntheticRadarReplayTask),
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
    /// Batch-plot every field of a run to PNG when an import completes
    /// (wrench heavy, light 📄, and GDEX all land through the same
    /// completion arms). Persisted with the rest of this popover state; ON
    /// by default — the whole point of a bulk import is bulk output.
    #[serde(default = "wrf_default_true")]
    auto_plot: bool,
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
            auto_plot: true,
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

/// Outcome-oriented starting points for people who do not want to assemble a
/// virtual instrument one checkbox at a time. Recipes deliberately preserve
/// antenna placement, range, and gate geometry, but reset every interacting
/// presentation/physics/instrument control to a known-compatible set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyntheticRadarRecipe {
    StormView,
    CleanTruth,
    CleanDualPol,
    RealRadar,
    MaximumFidelity,
    PropertyTMatrixHybrid,
    PropertyTMatrixResearch,
}

impl SyntheticRadarRecipe {
    const ALL: [Self; 7] = [
        Self::StormView,
        Self::CleanTruth,
        Self::CleanDualPol,
        Self::RealRadar,
        Self::MaximumFidelity,
        Self::PropertyTMatrixHybrid,
        Self::PropertyTMatrixResearch,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::StormView => "Storm view (fast)",
            Self::CleanTruth => "Clean model truth",
            Self::CleanDualPol => "Clean dual-pol",
            Self::RealRadar => "Real radar (balanced) - recommended",
            Self::MaximumFidelity => "Maximum fidelity (slow)",
            Self::PropertyTMatrixHybrid => "P3 Hybrid - recommended",
            Self::PropertyTMatrixResearch => "Full P3/ISHMAEL T-matrix (experimental)",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::StormView => {
                "Sharp, readable reflectivity and clean winds for quickly browsing a run; no simulated hardware effects."
            }
            Self::CleanTruth => {
                "Artifact-free model REF/VEL for diagnosis and comparisons: no texture, noise, folding, blockage, or scan-time effects."
            }
            Self::CleanDualPol => {
                "All polarimetric products without noise, velocity folding, or terrain blockage, so the model microphysics is easiest to inspect."
            }
            Self::RealRadar => {
                "A practical virtual S-band radar with beam averaging, fall speed, terrain blockage, dual-pol propagation, sensitivity, timed rays, and folded velocity. The atmosphere stays frozen unless temporal interpolation is enabled manually."
            }
            Self::MaximumFidelity => {
                "The full virtual instrument with 27-point pulse-volume integration and adjacent-time atmosphere interpolation. Best for a short loop; source-model resolution still limits detail."
            }
            Self::PropertyTMatrixHybrid => {
                "Production-friendly P3/ISHMAEL dual-pol: native property T-matrix for supported cells and versioned bulk Rayleigh only for audited table-domain/shape omissions or the typed WRF 2 µm source-state mass gap. Defaults to embedded 2.8 GHz S and a frozen atmosphere."
            }
            Self::PropertyTMatrixResearch => {
                "Experimental full property T-matrix for exact supported P3/ISHMAEL files. It is strictly fail-closed, defaults to embedded 2.8 GHz S, and permits expert raw-state temporal interpolation or validated local S/C/X packs."
            }
        }
    }

    const fn products(self) -> &'static str {
        match self {
            Self::StormView | Self::CleanTruth => "REF / VEL",
            Self::CleanDualPol
            | Self::RealRadar
            | Self::MaximumFidelity
            | Self::PropertyTMatrixHybrid
            | Self::PropertyTMatrixResearch => {
                "REF / VEL / SW / ZDR / CC / KDP / PHI + attenuation/corrected fields"
            }
        }
    }
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
    /// Set the gate spacing equal to the WRF grid resolution (the file's `DX`),
    /// so a coarse grid is not oversampled. When on it overrides the range/gate
    /// controls above (auto and manual) with the per-file grid resolution,
    /// clamped 100 m–10 km; a file with no `DX` falls back to those. OFF by
    /// default — an older config with no entry restores it.
    #[serde(default)]
    match_gate_to_grid: bool,
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
    /// Named intent plus the forward-operator controls introduced by the
    /// deeper simulated-radar path. Every field is persisted explicitly so a
    /// tuned virtual instrument is reproducible across restarts; the serde
    /// defaults below deliberately reproduce the shipped Presentation mode
    /// for settings written before these controls existed.
    #[serde(default)]
    simulation_mode: crate::wrf_radar::SimulationMode,
    /// Preferred native P3/ISHMAEL property-T-matrix execution backend.
    /// `Auto` is backward-compatible and may use CUDA only when the cached
    /// runtime probe reports a qualified NVIDIA device.
    #[serde(default)]
    compute_preference: crate::wrf_radar::SyntheticRadarComputePreference,
    /// Physical scan pattern. Absent in older settings means the historical
    /// custom fourteen-cut ladder, preserving serde/backward compatibility.
    #[serde(default)]
    scan_strategy: crate::wrf_radar::SyntheticScanStrategy,
    #[serde(default)]
    reflectivity_sampling: crate::wrf_radar::ReflectivitySampling,
    #[serde(default)]
    beam_integration: crate::wrf_radar::BeamIntegration,
    #[serde(default = "default_synth_beam_width_deg")]
    beam_width_deg: f32,
    #[serde(default = "default_synth_pulse_width_us")]
    pulse_width_us: f32,
    #[serde(default = "default_synth_radar_frequency_mhz")]
    radar_frequency_mhz: u32,
    #[serde(default)]
    terminal_fall_speed: bool,
    #[serde(default)]
    terrain_blockage: bool,
    #[serde(default)]
    spectrum_width: bool,
    #[serde(default = "default_synth_spectrum_width_floor_mps")]
    spectrum_width_floor_mps: f32,
    #[serde(default)]
    dual_pol: bool,
    /// Polarimetric forward operator. Full T-matrix is fail-closed; Hybrid has
    /// a separately named, audited bulk-Rayleigh policy for table omissions.
    #[serde(default)]
    polarimetric_kernel: crate::wrf_radar::PolarimetricKernel,
    #[serde(default)]
    property_tmatrix_table_source: app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind,
    #[serde(default)]
    property_tmatrix_rain_sensitivity: crate::wrf_radar::PropertyTMatrixRainSensitivity,
    #[serde(default)]
    propagation: bool,
    /// Geometric beam propagation is separate from polarimetric attenuation.
    /// Older settings retain the standard 4/3-Earth path.
    #[serde(default)]
    propagation_geometry: crate::wrf_radar::PropagationGeometry,
    #[serde(default)]
    system_phidp_deg: f32,
    #[serde(default)]
    zdr_bias_db: f32,
    #[serde(default)]
    scan_timing: crate::wrf_radar::ScanTiming,
    /// Whether rays sample one frozen WRF scene or interpolate compatible
    /// adjacent scenes in linear received-power/wind/scattering space. Older
    /// settings remain frozen. The property-aware raw-microphysics path uses
    /// the same persisted choice once its stricter field contract is active.
    #[serde(default)]
    atmosphere_time_mode: app_ui::wrf_temporal::AtmosphereTimeMode,
    /// What to do when interpolation cannot obtain a complete later scene.
    /// Holding the anchor preserves the historical final frame.
    #[serde(default)]
    missing_neighbor_policy: app_ui::wrf_temporal::MissingNeighborPolicy,
    /// User-controlled preflight cap for the complete temporal build.
    #[serde(default = "default_synth_temporal_memory_budget_mib")]
    temporal_memory_budget_mib: usize,
    /// Migration marker introduced with the 64 GiB default. v0.33.1 wrote its
    /// then-default 8 GiB value into every settings file, so absence of this
    /// marker lets restore distinguish that legacy default from a deliberate
    /// 8 GiB choice made in the current UI.
    #[serde(default)]
    temporal_memory_budget_user_set: bool,
    #[serde(default = "default_synth_rotation_rate_deg_s")]
    rotation_rate_deg_s: f32,
    #[serde(default = "default_synth_transition_delay_s")]
    transition_delay_s: f32,
    #[serde(default = "default_synth_prf_hz")]
    prf_hz: f32,
    /// Opt-in physically coupled custom single-PRF instrument. Frequency,
    /// PRF, pulse width, dwell and sample count become one estimator contract
    /// instead of independent display metadata.
    #[serde(default)]
    coupled_single_prf_estimator: bool,
    #[serde(default = "default_synth_estimator_dwell_ms")]
    estimator_dwell_ms: f32,
    /// `None` derives pulse count from dwell * PRF.
    #[serde(default)]
    estimator_pulse_count: Option<u32>,
    #[serde(default = "default_synth_estimator_independent_sample_fraction")]
    estimator_independent_sample_fraction: f32,
    #[serde(default)]
    estimator_minimum_snr_db: f32,
    /// Add Ideal and Measured diagnostic moment grids beside the canonical
    /// Presented products for instrument/debug comparisons.
    #[serde(default)]
    emit_stage_diagnostics: bool,
    #[serde(default)]
    instrument_noise: bool,
    #[serde(default = "default_synth_sensitivity_dbz_at_1km")]
    sensitivity_dbz_at_1km: f32,
    /// Emit compact MCOV/TUNB/MSIG support fields with every synthetic frame.
    #[serde(default = "wrf_default_true")]
    emit_quality_fields: bool,
    /// Mask physical moments below this pulse-volume model-coverage fraction.
    /// Zero preserves every historically accepted gate.
    #[serde(default)]
    minimum_model_coverage_fraction: f32,
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
    /// Operational forecast-radar quick selection. Both may be enabled to
    /// build a two-frame f00/f01 loop from one latest HRRR cycle.
    #[serde(default = "wrf_default_true")]
    operational_f00: bool,
    #[serde(default)]
    operational_f01: bool,
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

fn default_synth_beam_width_deg() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().beam_width_deg
}

fn default_synth_pulse_width_us() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().pulse_width_us
}

fn default_synth_radar_frequency_mhz() -> u32 {
    crate::wrf_radar::SyntheticRadarConfig::default().radar_frequency_mhz
}

fn default_synth_spectrum_width_floor_mps() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().spectrum_width_floor_mps
}

fn default_synth_rotation_rate_deg_s() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().rotation_rate_deg_s
}

fn default_synth_transition_delay_s() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().transition_delay_s
}

fn default_synth_prf_hz() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().prf_hz
}

fn default_synth_estimator_dwell_ms() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().estimator_dwell_ms
}

fn default_synth_estimator_independent_sample_fraction() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().estimator_independent_sample_fraction
}

fn default_synth_temporal_memory_budget_mib() -> usize {
    65_536
}

const MIB_PER_GIB: f64 = 1024.0;

fn synth_temporal_budget_gib(memory_budget_mib: usize) -> f64 {
    memory_budget_mib as f64 / MIB_PER_GIB
}

fn synth_temporal_budget_mib_from_gib(memory_budget_gib: f64) -> usize {
    ((memory_budget_gib.clamp(1.0, 64.0) * MIB_PER_GIB).round() as usize).clamp(1024, 65_536)
}

fn default_synth_sensitivity_dbz_at_1km() -> f32 {
    crate::wrf_radar::SyntheticRadarConfig::default().sensitivity_dbz_at_1km
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SyntheticRadarWorkEstimate {
    tilt_count: usize,
    rays_per_tilt: usize,
    gates_per_ray: usize,
    samples_per_gate: usize,
    total_samples: u64,
}

impl SyntheticRadarWorkEstimate {
    fn from_state(state: &SyntheticRadarUiState) -> Self {
        let tilt_count = state
            .scan_strategy
            .definition()
            .map_or_else(
                || crate::wrf_radar::elevation_ladder(state.include_low_tilt).len(),
                |definition| definition.rows.len(),
            )
            .max(1);
        let rays_per_tilt = crate::wrf_radar::SyntheticRadarConfig::default()
            .azimuth_count
            .max(1);
        let gates_per_ray = ((state.clamped_range_km() * 1000.0 / state.effective_gate_spacing_m())
            .floor() as usize)
            .max(1);
        let samples_per_gate = state.beam_integration.pulse_volume_sample_count();
        let total_samples = (tilt_count as u64)
            .saturating_mul(rays_per_tilt as u64)
            .saturating_mul(gates_per_ray as u64)
            .saturating_mul(samples_per_gate as u64);
        Self {
            tilt_count,
            rays_per_tilt,
            gates_per_ray,
            samples_per_gate,
            total_samples,
        }
    }

    fn summary(self) -> String {
        format!(
            "Pulse-volume estimate: {} samples ({} per gate × {} gates × {} rays × {} tilts)",
            compact_sample_count(self.total_samples),
            self.samples_per_gate,
            self.gates_per_ray,
            self.rays_per_tilt,
            self.tilt_count,
        )
    }
}

fn compact_sample_count(samples: u64) -> String {
    if samples >= 1_000_000_000 {
        format!("{:.2} billion", samples as f64 / 1_000_000_000.0)
    } else if samples >= 1_000_000 {
        format!("{:.1} million", samples as f64 / 1_000_000.0)
    } else if samples >= 1_000 {
        format!("{:.1} thousand", samples as f64 / 1_000.0)
    } else {
        samples.to_string()
    }
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
            match_gate_to_grid: false,
            ref_gate_texture: true,
            vel_gate_texture: false,
            reflectivity_operator: crate::wrf_radar::ReflectivityOperator::default(),
            simulation_mode: crate::wrf_radar::SimulationMode::default(),
            compute_preference: crate::wrf_radar::SyntheticRadarComputePreference::default(),
            scan_strategy: crate::wrf_radar::SyntheticScanStrategy::default(),
            reflectivity_sampling: crate::wrf_radar::ReflectivitySampling::default(),
            beam_integration: crate::wrf_radar::BeamIntegration::default(),
            beam_width_deg: default_synth_beam_width_deg(),
            pulse_width_us: default_synth_pulse_width_us(),
            radar_frequency_mhz: default_synth_radar_frequency_mhz(),
            terminal_fall_speed: false,
            terrain_blockage: false,
            spectrum_width: false,
            spectrum_width_floor_mps: default_synth_spectrum_width_floor_mps(),
            dual_pol: false,
            polarimetric_kernel: crate::wrf_radar::PolarimetricKernel::default(),
            property_tmatrix_table_source:
                app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::default(),
            property_tmatrix_rain_sensitivity:
                crate::wrf_radar::PropertyTMatrixRainSensitivity::default(),
            propagation: false,
            propagation_geometry: crate::wrf_radar::PropagationGeometry::default(),
            system_phidp_deg: 0.0,
            zdr_bias_db: 0.0,
            scan_timing: crate::wrf_radar::ScanTiming::default(),
            atmosphere_time_mode: app_ui::wrf_temporal::AtmosphereTimeMode::default(),
            missing_neighbor_policy: app_ui::wrf_temporal::MissingNeighborPolicy::default(),
            temporal_memory_budget_mib: default_synth_temporal_memory_budget_mib(),
            temporal_memory_budget_user_set: false,
            rotation_rate_deg_s: default_synth_rotation_rate_deg_s(),
            transition_delay_s: default_synth_transition_delay_s(),
            prf_hz: default_synth_prf_hz(),
            coupled_single_prf_estimator: false,
            estimator_dwell_ms: default_synth_estimator_dwell_ms(),
            estimator_pulse_count: None,
            estimator_independent_sample_fraction:
                default_synth_estimator_independent_sample_fraction(),
            estimator_minimum_snr_db: 0.0,
            emit_stage_diagnostics: false,
            instrument_noise: false,
            sensitivity_dbz_at_1km: default_synth_sensitivity_dbz_at_1km(),
            emit_quality_fields: true,
            minimum_model_coverage_fraction: 0.0,
            include_low_tilt: false,
            clutter_intensity: 0.0,
            fold_velocity: false,
            fold_nyquist_mps: default_fold_nyquist_mps(),
            operational_f00: true,
            operational_f01: false,
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

    fn operational_forecast_hours(&self) -> Vec<u16> {
        let mut hours = Vec::with_capacity(2);
        if self.operational_f00 {
            hours.push(0);
        }
        if self.operational_f01 {
            hours.push(1);
        }
        hours
    }

    fn to_operational_config(&self) -> Result<crate::wrf_radar::SyntheticRadarConfig, String> {
        let mut config = self.to_config()?;
        // Operational HRRR/RRFS uses the portable bulk-Rayleigh path. Keep it
        // on the CPU contract even when the WRF property-T-matrix preference
        // is Auto/CUDA so that preference cannot leak across workflows.
        config.compute_preference = crate::wrf_radar::SyntheticRadarComputePreference::Cpu;
        config.reflectivity_sampling = crate::wrf_radar::ReflectivitySampling::LinearZ;
        config.polarimetric_kernel = crate::wrf_radar::PolarimetricKernel::BulkRayleighV1;
        config.dual_pol = true;
        config.terminal_fall_speed = true;
        config.atmosphere_time_mode = app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart;
        config.propagation_geometry =
            crate::wrf_radar::PropagationGeometry::StandardFourThirdsEarth;
        config.site_name = Some(match (config.site_lat_deg, config.site_lon_deg) {
            (Some(lat), Some(lon)) => {
                format!("Operational forecast radar @ {lat:.3}, {lon:.3}")
            }
            _ => "Operational forecast radar at native model domain centre".to_owned(),
        });
        if !matches!(self.placement, SynthPlacement::NexradSite) {
            config.site_id = "OPR".to_owned();
        }
        Ok(config)
    }

    fn to_cm1_config(&self) -> Result<crate::wrf_radar::SyntheticRadarConfig, String> {
        // CM1 owns an explicitly placed, usually small idealized domain. A
        // persisted WRF/NEXRAD site can be hundreds of kilometres outside it,
        // yielding an all-missing polar scan while the native scene is healthy.
        // Preserve shared scan/presentation controls, but always resolve the
        // CM1 antenna from the placed domain's center cell.
        let mut cm1_state = self.clone();
        cm1_state.placement = SynthPlacement::DomainCenter;
        let mut config = cm1_state.to_config()?;
        // CM1's first-class adapter samples a scalar native-dbz scene. Keep
        // every WRF-only or polarimetric branch impossible even when those
        // controls remain selected in the shared simulated-radar workspace.
        config.compute_preference = crate::wrf_radar::SyntheticRadarComputePreference::Cpu;
        config.reflectivity_operator = crate::wrf_radar::ReflectivityOperator::ModelNative;
        config.polarimetric_kernel = crate::wrf_radar::PolarimetricKernel::BulkRayleighV1;
        config.dual_pol = false;
        config.terminal_fall_speed = false;
        config.propagation = false;
        config.propagation_geometry =
            crate::wrf_radar::PropagationGeometry::StandardFourThirdsEarth;
        config.atmosphere_time_mode = app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart;
        config.exact_replay_template = None;
        config.site_lat_deg = None;
        config.site_lon_deg = None;
        config.site_id = "CM1".to_owned();
        config.site_name = Some("Simulated CM1 radar at placed domain centre".to_owned());
        Ok(config)
    }

    /// Apply only the controls owned by a named mode. This mirrors the
    /// backend preset exactly, but it runs solely in response to an explicit
    /// mode-button click; expert edits are otherwise left untouched.
    fn apply_mode_preset(&mut self, mode: crate::wrf_radar::SimulationMode) {
        let mut preset = crate::wrf_radar::SyntheticRadarConfig {
            simulation_mode: self.simulation_mode,
            reflectivity_sampling: self.reflectivity_sampling,
            beam_integration: self.beam_integration,
            terminal_fall_speed: self.terminal_fall_speed,
            terrain_blockage: self.terrain_blockage,
            spectrum_width: self.spectrum_width,
            dual_pol: self.dual_pol,
            polarimetric_kernel: self.polarimetric_kernel,
            property_tmatrix_table_source: self.property_tmatrix_table_source,
            property_tmatrix_rain_sensitivity: self.property_tmatrix_rain_sensitivity,
            propagation: self.propagation,
            propagation_geometry: self.propagation_geometry,
            scan_timing: self.scan_timing,
            atmosphere_time_mode: self.atmosphere_time_mode,
            missing_neighbor_policy: self.missing_neighbor_policy,
            temporal_memory_budget_mib: self.temporal_memory_budget_mib,
            coupled_single_prf_estimator: self.coupled_single_prf_estimator,
            estimator_dwell_ms: self.estimator_dwell_ms,
            estimator_pulse_count: self.estimator_pulse_count,
            estimator_independent_sample_fraction: self.estimator_independent_sample_fraction,
            estimator_minimum_snr_db: self.estimator_minimum_snr_db,
            emit_stage_diagnostics: self.emit_stage_diagnostics,
            instrument_noise: self.instrument_noise,
            emit_quality_fields: self.emit_quality_fields,
            minimum_model_coverage_fraction: self.minimum_model_coverage_fraction,
            ref_gate_texture: self.ref_gate_texture,
            vel_gate_texture: self.vel_gate_texture,
            clutter_intensity: self.clutter_intensity,
            fold_velocity: self.fold_velocity,
            ..crate::wrf_radar::SyntheticRadarConfig::default()
        };
        preset.apply_mode_preset(mode);
        self.simulation_mode = preset.simulation_mode;
        self.reflectivity_sampling = preset.reflectivity_sampling;
        self.beam_integration = preset.beam_integration;
        self.terminal_fall_speed = preset.terminal_fall_speed;
        self.terrain_blockage = preset.terrain_blockage;
        self.spectrum_width = preset.spectrum_width;
        self.dual_pol = preset.dual_pol;
        self.polarimetric_kernel = preset.polarimetric_kernel;
        self.property_tmatrix_table_source = preset.property_tmatrix_table_source;
        self.property_tmatrix_rain_sensitivity = preset.property_tmatrix_rain_sensitivity;
        self.propagation = preset.propagation;
        self.propagation_geometry = preset.propagation_geometry;
        self.scan_timing = preset.scan_timing;
        // Science modes should be usable on one WRF scene by default. Timed
        // adjacent-scene interpolation is a separate, expensive fidelity
        // choice; only the explicitly named Maximum Fidelity recipe enables it.
        self.atmosphere_time_mode = app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart;
        self.missing_neighbor_policy = preset.missing_neighbor_policy;
        // Resource policy belongs to the user, not to a science mode. Keep a
        // customized RAM cap intact when switching Truth/Instrument/Presentation.
        self.coupled_single_prf_estimator = preset.coupled_single_prf_estimator;
        self.estimator_dwell_ms = preset.estimator_dwell_ms;
        self.estimator_pulse_count = preset.estimator_pulse_count;
        self.estimator_independent_sample_fraction = preset.estimator_independent_sample_fraction;
        self.estimator_minimum_snr_db = preset.estimator_minimum_snr_db;
        self.emit_stage_diagnostics = preset.emit_stage_diagnostics;
        self.instrument_noise = preset.instrument_noise;
        self.emit_quality_fields = preset.emit_quality_fields;
        self.minimum_model_coverage_fraction = preset.minimum_model_coverage_fraction;
        self.ref_gate_texture = preset.ref_gate_texture;
        self.vel_gate_texture = preset.vel_gate_texture;
        self.clutter_intensity = preset.clutter_intensity;
        self.fold_velocity = preset.fold_velocity;
    }

    /// Apply a complete, human-facing recipe while leaving the chosen virtual
    /// radar location/range/gate geometry untouched.
    fn apply_recipe(&mut self, recipe: SyntheticRadarRecipe) {
        let defaults = crate::wrf_radar::SyntheticRadarConfig::default();

        // Reset knobs that the lower-level mode presets intentionally leave
        // editable. This prevents a stale custom PRF, bias, texture, or
        // sensitivity value from leaking into a newly selected recipe.
        self.reflectivity_operator = crate::wrf_radar::ReflectivityOperator::ModelNative;
        self.reflectivity_sampling = crate::wrf_radar::ReflectivitySampling::LinearZ;
        self.beam_width_deg = defaults.beam_width_deg;
        self.pulse_width_us = defaults.pulse_width_us;
        self.radar_frequency_mhz = defaults.radar_frequency_mhz;
        self.polarimetric_kernel = defaults.polarimetric_kernel;
        self.property_tmatrix_table_source = defaults.property_tmatrix_table_source;
        self.property_tmatrix_rain_sensitivity = defaults.property_tmatrix_rain_sensitivity;
        self.spectrum_width_floor_mps = defaults.spectrum_width_floor_mps;
        self.system_phidp_deg = 0.0;
        self.zdr_bias_db = 0.0;
        self.rotation_rate_deg_s = defaults.rotation_rate_deg_s;
        self.transition_delay_s = defaults.transition_delay_s;
        self.prf_hz = defaults.prf_hz;
        self.coupled_single_prf_estimator = defaults.coupled_single_prf_estimator;
        self.estimator_dwell_ms = defaults.estimator_dwell_ms;
        self.estimator_pulse_count = defaults.estimator_pulse_count;
        self.estimator_independent_sample_fraction = defaults.estimator_independent_sample_fraction;
        self.estimator_minimum_snr_db = defaults.estimator_minimum_snr_db;
        self.emit_stage_diagnostics = defaults.emit_stage_diagnostics;
        self.atmosphere_time_mode = app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart;
        self.missing_neighbor_policy = app_ui::wrf_temporal::MissingNeighborPolicy::HoldAnchor;
        // Resource policy belongs to the user, not to a science recipe. In
        // particular, selecting a temporal/P3 recipe must not silently reset a
        // previously saved build RAM cap.
        self.sensitivity_dbz_at_1km = defaults.sensitivity_dbz_at_1km;
        self.emit_quality_fields = defaults.emit_quality_fields;
        self.minimum_model_coverage_fraction = defaults.minimum_model_coverage_fraction;
        self.include_low_tilt = false;
        self.vel_gate_texture = false;
        self.clutter_intensity = 0.0;
        self.fold_nyquist_mps = defaults.nyquist_mps;

        match recipe {
            SyntheticRadarRecipe::StormView => {
                self.apply_mode_preset(crate::wrf_radar::SimulationMode::Presentation);
            }
            SyntheticRadarRecipe::CleanTruth => {
                self.apply_mode_preset(crate::wrf_radar::SimulationMode::Truth);
            }
            SyntheticRadarRecipe::CleanDualPol => {
                self.apply_mode_preset(crate::wrf_radar::SimulationMode::Instrument);
                self.terrain_blockage = false;
                self.scan_timing = crate::wrf_radar::ScanTiming::InstantaneousTruth;
                self.atmosphere_time_mode =
                    app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart;
                self.instrument_noise = false;
                self.fold_velocity = false;
            }
            SyntheticRadarRecipe::RealRadar => {
                self.apply_mode_preset(crate::wrf_radar::SimulationMode::Instrument);
                self.missing_neighbor_policy =
                    app_ui::wrf_temporal::MissingNeighborPolicy::HoldAnchor;
            }
            SyntheticRadarRecipe::MaximumFidelity => {
                self.apply_mode_preset(crate::wrf_radar::SimulationMode::Instrument);
                self.beam_integration = crate::wrf_radar::BeamIntegration::Reference;
                self.atmosphere_time_mode =
                    app_ui::wrf_temporal::AtmosphereTimeMode::LinearAdjacent;
                self.missing_neighbor_policy =
                    app_ui::wrf_temporal::MissingNeighborPolicy::HoldAnchor;
            }
            SyntheticRadarRecipe::PropertyTMatrixHybrid
            | SyntheticRadarRecipe::PropertyTMatrixResearch => {
                self.apply_mode_preset(crate::wrf_radar::SimulationMode::Instrument);
                self.polarimetric_kernel =
                    if matches!(recipe, SyntheticRadarRecipe::PropertyTMatrixHybrid) {
                        crate::wrf_radar::PolarimetricKernel::PropertyTMatrixHybridV1
                    } else {
                        crate::wrf_radar::PolarimetricKernel::PropertyTMatrixResearchV1
                    };
                self.property_tmatrix_table_source =
                    app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1;
                self.property_tmatrix_rain_sensitivity =
                    crate::wrf_radar::PropertyTMatrixRainSensitivity::FullProperty;
                self.radar_frequency_mhz =
                    crate::wrf_radar::PROPERTY_TMATRIX_RESEARCH_FREQUENCY_MHZ;
                self.reflectivity_sampling = crate::wrf_radar::ReflectivitySampling::LinearZ;
                self.beam_integration = crate::wrf_radar::BeamIntegration::Balanced;
                self.atmosphere_time_mode =
                    app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart;
                self.missing_neighbor_policy =
                    app_ui::wrf_temporal::MissingNeighborPolicy::HoldAnchor;
            }
        }
    }

    fn active_recipe(&self) -> Option<SyntheticRadarRecipe> {
        SyntheticRadarRecipe::ALL.into_iter().find(|recipe| {
            let mut expected = self.clone();
            expected.apply_recipe(*recipe);
            expected == *self
        })
    }

    /// Restore the opaque settings value with the one required budget
    /// migration. v0.33.1 serialized 8192 even when the user never touched the
    /// control; migrate only that unmarked legacy default. Any unmarked
    /// non-default value was customized and becomes marked, while a marked
    /// 8 GiB value remains an intentional current-version choice.
    fn from_persisted_value(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        let had_budget = value.get("temporal_memory_budget_mib").is_some();
        let had_user_set_marker = value.get("temporal_memory_budget_user_set").is_some();
        let mut state = serde_json::from_value::<Self>(value.clone())?;
        if !had_user_set_marker {
            if had_budget && state.temporal_memory_budget_mib == 8_192 {
                state.temporal_memory_budget_mib = default_synth_temporal_memory_budget_mib();
            } else if had_budget {
                state.temporal_memory_budget_user_set = true;
            }
        }
        Ok(state)
    }

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
            // The range/gate resolution here is the FALLBACK spacing: when
            // `match_gate_to_grid` is on the library resolves the effective gate
            // size from each file's DX at build time, and drops back to this value
            // for a file with no readable DX.
            gate_spacing_m: self.effective_gate_spacing_m(),
            match_gate_to_grid: self.match_gate_to_grid,
            ref_gate_texture: self.ref_gate_texture,
            vel_gate_texture: self.vel_gate_texture,
            reflectivity_operator: self.reflectivity_operator,
            simulation_mode: self.simulation_mode,
            compute_preference: self.compute_preference,
            scan_strategy: self.scan_strategy,
            reflectivity_sampling: self.reflectivity_sampling,
            beam_integration: self.beam_integration,
            beam_width_deg: self.beam_width_deg,
            pulse_width_us: self.pulse_width_us,
            radar_frequency_mhz: self.radar_frequency_mhz,
            terminal_fall_speed: self.terminal_fall_speed,
            terrain_blockage: self.terrain_blockage,
            spectrum_width: self.spectrum_width,
            spectrum_width_floor_mps: self.spectrum_width_floor_mps,
            dual_pol: self.dual_pol,
            polarimetric_kernel: self.polarimetric_kernel,
            property_tmatrix_table_source: self.property_tmatrix_table_source,
            property_tmatrix_rain_sensitivity: self.property_tmatrix_rain_sensitivity,
            propagation: self.propagation,
            propagation_geometry: self.propagation_geometry,
            system_phidp_deg: self.system_phidp_deg,
            zdr_bias_db: self.zdr_bias_db,
            // Adjacent-scene atmosphere/scattering interpolation is meaningful
            // only for a timed scan. Normalize stale/manually-edited settings
            // instead of emitting an internally contradictory configuration.
            scan_timing: if self.atmosphere_time_mode.uses_adjacent_scene() {
                crate::wrf_radar::ScanTiming::TimedVolume
            } else {
                self.scan_timing
            },
            atmosphere_time_mode: self.atmosphere_time_mode,
            missing_neighbor_policy: self.missing_neighbor_policy,
            temporal_memory_budget_mib: self.temporal_memory_budget_mib.clamp(1024, 65_536),
            rotation_rate_deg_s: self.rotation_rate_deg_s,
            transition_delay_s: self.transition_delay_s,
            prf_hz: self.prf_hz,
            coupled_single_prf_estimator: self.coupled_single_prf_estimator,
            estimator_dwell_ms: self.estimator_dwell_ms,
            estimator_pulse_count: self.estimator_pulse_count,
            estimator_independent_sample_fraction: self.estimator_independent_sample_fraction,
            estimator_minimum_snr_db: self.estimator_minimum_snr_db,
            emit_stage_diagnostics: self.emit_stage_diagnostics,
            instrument_noise: self.instrument_noise,
            sensitivity_dbz_at_1km: self.sensitivity_dbz_at_1km,
            emit_quality_fields: self.emit_quality_fields,
            minimum_model_coverage_fraction: self.minimum_model_coverage_fraction.clamp(0.0, 1.0),
            elevations_deg: self
                .scan_strategy
                .definition()
                .map(|definition| {
                    definition
                        .elevation_ladder_deg()
                        .into_iter()
                        .map(f64::from)
                        .collect()
                })
                .unwrap_or_else(|| crate::wrf_radar::elevation_ladder(self.include_low_tilt)),
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

/// Return the staged raw-WRF source only while it still names a real file.
/// A stale session path must fall through to the unrestricted source picker.
fn retained_namelist_source(path: Option<&Path>) -> Option<PathBuf> {
    path.filter(|path| path.is_file()).map(Path::to_path_buf)
}

pub struct ModelDataDock {
    worker: StoreWorker,
    /// egui context, kept so a background import can request repaints while it
    /// runs (its worker threads have no repaint hook of their own).
    repaint: egui::Context,
    store_root: PathBuf,
    /// Running local WRF/NetCDF import, if any (drained in `poll_import`).
    import_job: Option<ImportJob>,
    /// First-class native CM1 inspection/placement window. The panel owns
    /// only UI and metadata inspection state; store work joins `import_job`
    /// so the existing busy guard, pump, rescan, and plot handoff apply.
    cm1: crate::cm1_ui::Cm1ImportPanel,
    /// One-shot handoff after a successful CM1 store write. Main opens the
    /// shared Models surface outside any auxiliary-window borrow.
    cm1_open_models_requested: bool,
    /// Finished synthetic-radar volumes waiting for the app to install them in
    /// the loop engine (one-shot, drained by [`Self::take_synthetic_radar`]).
    synthetic_radar_result: Option<crate::wrf_radar::SyntheticRadarOutput>,
    /// First completed tilt from the active synthetic-radar build. This lets
    /// the radar view become useful while the remaining cuts keep processing.
    synthetic_radar_preview: Option<crate::wrf_radar::SyntheticRadarPreview>,
    /// Finished exact observed-geometry replay waiting for the app to install
    /// its Observed / Simulated / Difference workspace.
    synthetic_radar_replay_result: Option<crate::wrf_radar::SyntheticRadarReplayOutput>,
    /// Current radar-view volume when it is eligible to provide an exact
    /// observed acquisition template. This is an Arc snapshot only; no gate
    /// data is copied into the WRF dock.
    displayed_replay_source: Option<std::sync::Arc<radar_core::RadarVolume>>,
    /// Observed source retained with the last replay selection so Refresh can
    /// rerun the same scan after instrument/scattering tuning.
    synthetic_radar_replay_source: Option<std::sync::Arc<radar_core::RadarVolume>>,
    /// Retained handles to the most recent finished synthetic-radar frames so
    /// the export control can write them as CfRadial files AFTER the one-shot
    /// result above is drained into the loop engine. Arc clones of the loop
    /// frames — no volume data is duplicated. Replaced whole on the next
    /// finished build.
    // Read only by the native rfd export UI on supported desktops.
    #[cfg_attr(
        not(any(windows, target_os = "macos", target_os = "linux")),
        allow(dead_code)
    )]
    synthetic_export_frames: Vec<std::sync::Arc<radar_core::RadarVolume>>,
    /// Exact WRF source selection used by the most recent simulated-radar
    /// launch. Kept only for this app session so tuning controls can rebuild
    /// the current frame/loop without reopening a picker. A folder launch is
    /// deliberately a snapshot: refresh reruns the same files rather than
    /// silently adding later arrivals.
    #[cfg_attr(
        not(any(windows, target_os = "macos", target_os = "linux")),
        allow(dead_code)
    )]
    synthetic_radar_source_files: Vec<PathBuf>,
    /// Latest operational HRRR/RRFS request. Local/cached requests retain the
    /// exact file snapshot; latest-HRRR refresh deliberately re-resolves the
    /// selected forecast hour(s) through the shared resumable cache.
    operational_radar_source: Option<crate::wrf_radar::OperationalRadarSource>,
    operational_cached_inputs: Vec<crate::operational_radar_grib::CachedOperationalHrrrInput>,
    operational_cached_selected: usize,
    /// Last import status line shown under the import controls.
    import_message: Option<String>,
    tree: Option<StoreTree>,
    browser: RunBrowserPanel,
    viewer: FieldViewerPanel,
    sounding: crate::sharppy_sounding::SharppySoundingPanel,
    /// Most recent loaded field (kept for the map layer).
    latest_field: Option<std::sync::Arc<rw_ui::FieldData>>,
    /// Most recent sounding data (kept for the native skew-T window).
    latest_sounding: Option<std::sync::Arc<rw_ui::SoundingData>>,
    /// Which request owns the reusable sounding surface. This prevents a
    /// late point response from replacing a box mean (and vice versa).
    sounding_request_mode: SoundingRequestMode,
    /// First-class area-mean sounding: the next radar-map drag owns the
    /// pointer while armed, then this worker reads/averages store primitives.
    box_sounding_armed: bool,
    box_sounding_task: Option<crate::box_sounding::BoxSoundingTask>,
    box_sounding_pending: Option<(HourKey, crate::box_sounding::BoxBounds)>,
    box_sounding_summary: Option<crate::box_sounding::BoxSoundingSummary>,
    /// One-shot: the user asked to put the current field on the radar map.
    map_request: Option<std::sync::Arc<rw_ui::FieldData>>,
    /// v0.2.3 custom-domain plot viewer: renders the selected field through
    /// rusty-weather's native plot pipeline over a user-chosen domain (shift-
    /// drag a box on the field viewer, or rotate a corner — or draw the box on
    /// the radar map via the 📐 arm button / Ctrl+Shift+drag). Shown as a
    /// floating window when `show_plot_viewer` is set.
    plot_viewer: PlotViewerPanel,
    /// External real-satellite/SimSat source mounted in the SAME native-plot
    /// window without changing `browser`, `viewer`, or `latest_field`.
    satellite_plot: SatellitePlotPanel,
    native_plot_content: NativePlotContent,
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
    /// Safe formula compiler/evaluator from Rusty Weather. The window remains
    /// independently pollable while the Model window is closed.
    formula_lab: FormulaLabPanel,
    /// One-shot host requests. Formula Lab is a first-class workspace surface;
    /// this shared backend never owns its egui Window or dock tile directly.
    formula_lab_open_requested: bool,
    formula_lab_open_models_requested: bool,
    /// Last installed Formula Lab field, retained for result-card actions even
    /// if the Models viewer later browses a different stored field.
    formula_result_field: Option<std::sync::Arc<rw_ui::FieldData>>,
    /// Unstyled scientific Formula Lab output retained across hour changes.
    /// rw-ui intentionally drops its generated cache when another hour is
    /// selected; this copy lets Formula Lab reinstall exactly from raw values.
    formula_result_raw_field: Option<rw_ui::FieldData>,
    /// Explicit raw-WRF file staged for formulas that need WRF grid metrics,
    /// physical height, vectors, or horizontal calculus. The unrestricted
    /// picker accepts extensionless `wrfout_*` files from every domain.
    formula_raw_path: Option<PathBuf>,
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
    // Read only by the native rfd import UI on supported desktops.
    #[cfg_attr(
        not(any(windows, target_os = "macos", target_os = "linux")),
        allow(dead_code)
    )]
    pending_heavy_import: Option<PendingHeavyImport>,
    /// A light import awaiting the same explicit confirmation (see
    /// [`light_import_size_warning`]).
    // Read only by the native rfd import UI on supported desktops.
    #[cfg_attr(
        not(any(windows, target_os = "macos", target_os = "linux")),
        allow(dead_code)
    )]
    pending_light_import: Option<PendingLightImport>,
    /// Store-variable names of the hour currently in the viewer, captured
    /// from its `HourVars` response. Load routing + display translation
    /// guard: a name that IS a real store variable always loads through the
    /// rw-ui worker (and keeps its own name), even if it happens to look
    /// like a synthesized iso-level slug; only synthesized names go to the
    /// iso plane loader.
    hour_store_vars: Vec<String>,
    /// Full selected-hour metadata for Formula Lab discovery and preflight.
    hour_store_var_info: Vec<rw_ui::VarInfo>,
    /// In-flight background load of a synthesized per-level isobaric field
    /// (one at a time; drained in [`Self::poll_iso_load`]).
    iso_load: Option<iso_fields::IsoFieldLoadTask>,
    /// Newest iso-level load requested while one was in flight (slug-named;
    /// latest wins — mirrors the rw-ui worker's request coalescing).
    iso_load_pending: Option<rw_ui::FieldKey>,
    /// GDEX (NSF NCAR CONUS II) in-app catalog browser (Stage 1b). Its window
    /// opens from the import controls; its workers are drained in
    /// [`Self::poll_gdex`] and a completed download is handed to the SAME
    /// local-import pump as the manual model import.
    gdex: crate::gdex_ui::GdexBrowser,
    /// Completed GDEX downloads waiting to be imported (FIFO). Normally holds
    /// zero or one — GDEX downloads one file at a time — but a download that
    /// lands while a manual import is still running waits here instead of
    /// being dropped.
    gdex_import_queue: std::collections::VecDeque<PathBuf>,
    /// Running batch "plot everything" job (single slot like `import_job`;
    /// drained in [`Self::poll_plot_job`]).
    plot_job: Option<crate::batch_plots::BatchPlotTask>,
    /// Latest progress/status line from the plot job (shown under the plot
    /// controls, same idiom as `import_message`).
    plot_message: Option<String>,
    /// Last completed plot-job summary, kept so the "Open plots folder"
    /// button knows where the output landed.
    plot_done: Option<crate::batch_plots::BatchPlotSummary>,
    /// Base directory for batch plot output. Defaults to the unbranded
    /// screenshots folder; main.rs overrides with the brand-aware dir at
    /// dock construction ([`Self::set_plots_base`]).
    plots_base: PathBuf,
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
            cm1: crate::cm1_ui::Cm1ImportPanel::default(),
            cm1_open_models_requested: false,
            synthetic_radar_result: None,
            synthetic_radar_preview: None,
            synthetic_radar_replay_result: None,
            displayed_replay_source: None,
            synthetic_radar_replay_source: None,
            synthetic_export_frames: Vec::new(),
            synthetic_radar_source_files: Vec::new(),
            operational_radar_source: None,
            operational_cached_inputs: Vec::new(),
            operational_cached_selected: 0,
            import_message: None,
            tree: None,
            browser: RunBrowserPanel::new(),
            viewer: FieldViewerPanel::new(),
            sounding: crate::sharppy_sounding::SharppySoundingPanel::new(),
            latest_field: None,
            latest_sounding: None,
            sounding_request_mode: SoundingRequestMode::None,
            box_sounding_armed: false,
            box_sounding_task: None,
            box_sounding_pending: None,
            box_sounding_summary: None,
            map_request: None,
            plot_viewer: PlotViewerPanel::new(),
            satellite_plot: SatellitePlotPanel::default(),
            native_plot_content: NativePlotContent::Model,
            show_plot_viewer: false,
            native_plot_seeded_run: None,
            plot_domain_armed: false,
            color_tables: ColorTableEditorPanel::new(),
            show_color_tables: false,
            formula_lab: FormulaLabPanel::new(),
            formula_lab_open_requested: false,
            formula_lab_open_models_requested: false,
            formula_result_field: None,
            formula_result_raw_field: None,
            formula_raw_path: None,
            wrf_options: WrfProcessUiState::default(),
            synth_radar: SyntheticRadarUiState::default(),
            pending_heavy_import: None,
            pending_light_import: None,
            hour_store_vars: Vec::new(),
            hour_store_var_info: Vec::new(),
            iso_load: None,
            iso_load_pending: None,
            gdex: crate::gdex_ui::GdexBrowser::new(),
            gdex_import_queue: std::collections::VecDeque::new(),
            plot_job: None,
            plot_message: None,
            plot_done: None,
            plots_base: settings::screenshots_dir().join("plots"),
        }
    }

    /// Point batch plot output at the brand-aware screenshots folder
    /// (`<screenshots>/plots`). Called once from main.rs right after
    /// construction — the dock itself has no brand config.
    pub fn set_plots_base(&mut self, dir: PathBuf) {
        self.plots_base = dir;
    }

    /// Push edited color-table style overrides to the store worker and reload
    /// the current field so the new palette shows (mirrors the rusty-weather
    /// reference host). The `StyleOverridesApplied` ack is a no-op — the reload
    /// is what repaints.
    fn apply_color_table_changes(&mut self) {
        let settings = self.color_tables.settings().clone().normalized();
        self.worker
            .send(StoreRequest::SetStyleOverrides(settings.clone()));
        self.plot_viewer.clear();
        if let Some(field) = self.viewer.wanted_field() {
            if self.viewer.restyle_generated_field(&settings) {
                self.latest_field = self
                    .viewer
                    .current_field()
                    .cloned()
                    .map(std::sync::Arc::new);
                if let Some(latest) = self.latest_field.clone()
                    && self
                        .formula_result_field
                        .as_ref()
                        .is_some_and(|formula| formula.key == latest.key)
                {
                    self.formula_result_field = Some(latest);
                }
            } else {
                self.viewer.set_loading(&field.var);
                self.request_field_load(field);
            }
        }
    }

    /// Build the selected rw-store source with its complete time axis. Exact
    /// v2 runs use manifest valid times. Legacy v1 derives `hour * 3600` only
    /// for recognized operational model slugs with canonical cycle names;
    /// custom/local writers sometimes used sequential slots, so they remain
    /// pointwise-only until migrated to exact-time v2.
    fn formula_store_source(&self) -> Option<StoreFormulaSource> {
        let hour = self.browser.selected()?.clone();
        let run = self
            .tree
            .as_ref()?
            .models
            .iter()
            .find(|model| model.model == hour.model)?
            .runs
            .iter()
            .find(|run| run.run == hour.run)?;
        let exact_times = if let Some(axis) = run
            .exact_times()
            .filter(|axis| axis.get(&hour.hour) == hour.exact_time.as_ref())
            .filter(|axis| {
                axis.values()
                    .all(|exact| lead_seconds_exact_in_f64(exact.lead_seconds))
            }) {
            axis.into_iter()
                .map(|(slot, exact)| {
                    (
                        slot,
                        rw_formula::ExactStoreTime::new(
                            exact.lead_seconds as f64,
                            Some(format!(
                                "{} · {}",
                                rw_ui::format_lead_seconds(exact.lead_seconds),
                                rw_ui::format_valid_unix(exact.valid_unix)
                            )),
                        ),
                    )
                })
                .collect()
        } else if !run.exact_time_axis
            && hour.exact_time.is_none()
            && model_run_time_utc(&run.run).is_some()
            && hour.model.parse::<rustwx_core::ModelId>().is_ok()
        {
            run.hours
                .iter()
                .map(|entry| {
                    let seconds = u64::from(entry.hour) * 3_600;
                    (
                        entry.hour,
                        rw_formula::ExactStoreTime::new(
                            seconds as f64,
                            Some(format!(
                                "f{:03} · {}",
                                entry.hour,
                                rw_ui::format_lead_seconds(seconds)
                            )),
                        ),
                    )
                })
                .collect()
        } else {
            std::collections::BTreeMap::new()
        };
        let temporal_axis_verified = formula_axis_supports_adjacent_times(&exact_times);
        Some(StoreFormulaSource {
            store_root: self.store_root.clone(),
            hour,
            exact_times,
            temporal_axis_verified,
            variables: self.hour_store_var_info.clone(),
        })
    }

    fn formula_raw_source(&self) -> Option<RawWrfFormulaSource> {
        let path = self.formula_raw_path.clone()?;
        let run = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("raw_wrf")
            .to_owned();
        Some(RawWrfFormulaSource {
            path,
            initial_time_index: 0,
            display_hour: HourKey {
                model: "raw-wrf".to_owned(),
                run,
                hour: 0,
                exact_time: None,
            },
        })
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn stage_formula_raw_from_files(&mut self, files: &[PathBuf]) {
        if let Some(first) = files.first() {
            self.formula_raw_path = Some(first.clone());
        }
    }

    fn formula_evaluation_blocked(&self) -> bool {
        self.import_job.is_some()
            || self.plot_job.is_some()
            || self.pending_heavy_import.is_some()
            || self.pending_light_import.is_some()
    }

    /// Poll the Formula Lab worker and keep its independent window responsive.
    /// `pump()` calls this every frame even when the Model window itself is
    /// closed, so a long WRF diagnostic cannot lose its completion message.
    fn pump_formula_lab(&mut self) {
        let store = self.formula_store_source();
        let raw_wrf = self.formula_raw_source();
        let blocked = self.formula_evaluation_blocked().then_some(
            "a model import, size confirmation, synthetic-radar build, or batch plot is active",
        );
        let ctx = self.repaint.clone();
        let Some(result) = self.formula_lab.poll(FormulaLabSources {
            store: store.as_ref(),
            raw_wrf: raw_wrf.as_ref(),
            evaluation_blocked: blocked,
        }) else {
            if self.formula_lab.busy() {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
            return;
        };

        let raw_result = matches!(&result.source, FormulaResultSource::RawWrf { .. });
        let still_current = match &result.source {
            FormulaResultSource::Store { store_root, hour } => {
                self.formula_lab.source_kind() == FormulaSourceKind::Store
                    && store_root == &self.store_root
                    && self.browser.selected() == Some(hour)
            }
            FormulaResultSource::RawWrf {
                path, time_index, ..
            } => {
                self.formula_lab.source_kind() == FormulaSourceKind::RawWrf
                    && self.formula_raw_path.as_ref() == Some(path)
                    && self.formula_lab.raw_time_index() == *time_index
            }
        } && result.source.revision_is_current();

        if !still_current {
            self.formula_lab
                .note_result_discarded("the selected data source changed while it ran");
            return;
        }

        self.install_formula_field(result.field, raw_result);
    }

    fn install_formula_field(&mut self, field: rw_ui::FieldData, raw_result: bool) {
        self.plot_viewer.clear();
        self.native_plot_content = NativePlotContent::Model;
        self.native_plot_seeded_run = None;
        if raw_result {
            self.sounding.clear();
            self.latest_sounding = None;
        }
        let settings = self.color_tables.settings().clone().normalized();
        self.formula_result_raw_field = Some(field.clone());
        self.viewer.install_generated_field(field, &settings);
        // Keep the styled/display-unit field as the external map/native-plot
        // source. `install_generated_field` retains the raw scientific values
        // internally, so later palette edits always reconvert exactly once.
        self.latest_field = self
            .viewer
            .current_field()
            .cloned()
            .map(std::sync::Arc::new);
        self.formula_result_field = self.latest_field.clone();
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

    fn poll_box_sounding(&mut self) {
        let result = self
            .box_sounding_task
            .as_ref()
            .and_then(crate::box_sounding::BoxSoundingTask::try_recv);
        let Some(result) = result else {
            return;
        };
        self.box_sounding_task = None;
        self.box_sounding_pending = None;
        if self.sounding_request_mode != SoundingRequestMode::BoxPending {
            return;
        }
        match result {
            Ok(result) => {
                self.sounding_request_mode = SoundingRequestMode::BoxApplied;
                self.box_sounding_summary = Some(result.summary);
                self.latest_sounding = Some(std::sync::Arc::new(result.data.clone()));
                self.sounding.set_data(result.data);
            }
            Err(message) => {
                self.sounding_request_mode = SoundingRequestMode::None;
                self.box_sounding_summary = None;
                self.sounding.set_error(message);
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
        self.hour_store_vars.clear();
        self.hour_store_var_info.clear();
        self.worker.send(StoreRequest::LoadHour(key));
    }

    /// Drain worker responses into panel state (mirrors the rusty-weather
    /// reference host).
    fn handle_responses(&mut self) {
        self.poll_import();
        self.poll_plot_job();
        self.poll_iso_load();
        self.poll_gdex();
        self.poll_box_sounding();
        while let Some(response) = self.worker.try_recv() {
            match response {
                StoreResponse::Tree(tree) => {
                    let selection_changed = self.browser.reconcile(&tree);
                    let selected = self.browser.selected().cloned();
                    self.tree = Some(tree);
                    if selection_changed && let Some(key) = selected {
                        self.select_hour(key);
                    }
                }
                StoreResponse::HourVars(key, Ok(vars)) => {
                    if self.browser.selected() == Some(&key) {
                        // Raw wrf_* vars show their catalog labels in the
                        // picker, and the hour's `*_iso` sounding volumes
                        // gain per-level 2-D entries ("Temperature 850 mb");
                        // the load below translates back to store names /
                        // iso planes (`request_field_load`).
                        self.hour_store_vars = vars.iter().map(|var| var.name.clone()).collect();
                        self.hour_store_var_info = vars.clone();
                        self.viewer.set_hour(key, viewer_display_vars(vars));
                        if let Some(field) = self.viewer.wanted_field() {
                            if self.viewer.restore_generated_field(&field.var) {
                                self.latest_field = self
                                    .viewer
                                    .current_field()
                                    .cloned()
                                    .map(std::sync::Arc::new);
                            } else {
                                self.viewer.set_loading(&field.var);
                                self.request_field_load(field);
                            }
                        }
                    }
                }
                StoreResponse::HourVars(key, Err(message)) => {
                    if self.browser.selected() == Some(&key) {
                        self.hour_store_vars.clear();
                        self.hour_store_var_info.clear();
                        self.viewer.set_error(message);
                    }
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
                    if matches!(
                        self.sounding_request_mode,
                        SoundingRequestMode::None | SoundingRequestMode::Point
                    ) {
                        self.sounding_request_mode = SoundingRequestMode::Point;
                        self.box_sounding_summary = None;
                        self.latest_sounding = Some(std::sync::Arc::new(data.clone()));
                        self.sounding.set_data(data);
                    }
                }
                StoreResponse::Sounding(_, Err(message)) => {
                    if matches!(
                        self.sounding_request_mode,
                        SoundingRequestMode::None | SoundingRequestMode::Point
                    ) {
                        self.sounding.set_error(message);
                    }
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
        self.pump_formula_lab();
    }

    /// Render the first-class Formula Lab workspace. This shares the Models
    /// run browser and worker but owns all source selection it needs, including
    /// unrestricted extensionless wrfout picking.
    pub fn formula_lab_ui(&mut self, ui: &mut egui::Ui) {
        self.handle_responses();
        ui.horizontal_wrapped(|ui| {
            ui.label(model_section_heading("Formula Lab"));
            ui.label(
                egui::RichText::new(
                    "Build portable diagnostics across stored models, or use raw WRF for full grid-aware calculus.",
                )
                .small()
                .weak(),
            );
        });
        ui.add_space(4.0);

        let mut source_kind = self.formula_lab.source_kind();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Data source").strong());
                ui.selectable_value(&mut source_kind, FormulaSourceKind::Store, "Stored model");
                ui.selectable_value(&mut source_kind, FormulaSourceKind::RawWrf, "Raw WRF");
                ui.separator();
                if ui.small_button("Re-scan store").clicked() {
                    self.rescan();
                }
            });
            self.formula_lab.set_source_kind(source_kind);

            match source_kind {
                FormulaSourceKind::Store => {
                    if let Some(selected) = self.browser.selected() {
                        ui.label(
                            egui::RichText::new(format!(
                                "Selected: {} / {} / {}",
                                selected.model,
                                selected.run,
                                selected.time_label()
                            ))
                            .small(),
                        );
                    }
                    let mut picked = None;
                    ui.push_id("formula_lab_store_browser", |ui| match &self.tree {
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Scanning model store…");
                            });
                        }
                        Some(tree) if tree.models.is_empty() => {
                            ui.label("No stored model runs yet.");
                        }
                        Some(tree) => {
                            egui::CollapsingHeader::new("Choose model / run / time")
                                .default_open(self.browser.selected().is_none())
                                .show(ui, |ui| {
                                    egui::ScrollArea::vertical()
                                        .id_salt("run_list")
                                        .max_height(210.0)
                                        .auto_shrink([false, true])
                                        .show(ui, |ui| {
                                            picked = self.browser.ui(ui, tree);
                                        });
                                });
                        }
                    });
                    if let Some(key) = picked {
                        self.select_hour(key);
                    }
                }
                FormulaSourceKind::RawWrf => {
                    ui.horizontal_wrapped(|ui| {
                        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
                        if ui.button("Choose raw WRF file…").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .set_title("Choose any raw WRF file for Formula Lab")
                                .pick_file()
                        {
                            self.formula_raw_path = Some(path);
                        }
                        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
                        ui.add_enabled(false, egui::Button::new("Choose raw WRF file…"));
                        if self.formula_raw_path.is_some() && ui.small_button("Clear").clicked() {
                            self.formula_raw_path = None;
                        }
                        if let Some(path) = &self.formula_raw_path {
                            ui.label(egui::RichText::new(path.display().to_string()).small().weak());
                        } else {
                            ui.label(
                                egui::RichText::new(
                                    "No extension or filename filter: wrfout from any domain is accepted.",
                                )
                                .small()
                                .weak(),
                            );
                        }
                    });
                }
            }
        });

        let store = self.formula_store_source();
        let raw_wrf = self.formula_raw_source();
        let blocked = self.formula_evaluation_blocked().then_some(
            "a model import, size confirmation, synthetic-radar build, or batch plot is active",
        );
        self.formula_result_actions_ui(ui);
        self.formula_lab.ui(
            ui,
            FormulaLabSources {
                store: store.as_ref(),
                raw_wrf: raw_wrf.as_ref(),
                evaluation_blocked: blocked,
            },
        );
    }

    fn formula_result_actions_ui(&mut self, ui: &mut egui::Ui) {
        let Some(field) = self.formula_result_field.clone() else {
            return;
        };
        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(egui::RichText::new("Last Formula Lab result").strong());
            ui.label(format!(
                "{} · {}×{} · {} · {} / {} / {}",
                field.key.var,
                field.nx,
                field.ny,
                field.units,
                field.key.hour.model,
                field.key.hour.run,
                field.key.hour.time_label()
            ));
            ui.label(
                egui::RichText::new(match field.range {
                    Some((minimum, maximum)) => {
                        format!("Finite range: {minimum:.4} to {maximum:.4} {}", field.units)
                    }
                    None => "Finite range: no finite display values".to_owned(),
                })
                .small()
                .weak(),
            );
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open Models").clicked() {
                    self.activate_formula_result(&field);
                    self.formula_lab_open_models_requested = true;
                }
                if ui.button("Add to radar map").clicked() {
                    self.map_request = Some(field.clone());
                }
                if ui.button("Native plot").clicked() {
                    self.activate_formula_result(&field);
                    self.native_plot_content = NativePlotContent::Model;
                    self.show_plot_viewer = true;
                }
                if ui.button("Color tables").clicked() {
                    self.activate_formula_result(&field);
                    self.show_color_tables = true;
                }
            });
        });
    }

    fn activate_formula_result(&mut self, fallback: &std::sync::Arc<rw_ui::FieldData>) {
        if self.viewer.restore_generated_field(&fallback.key.var) {
            self.latest_field = self
                .viewer
                .current_field()
                .cloned()
                .map(std::sync::Arc::new);
            self.formula_result_field = self.latest_field.clone();
        } else if let Some(raw) = self.formula_result_raw_field.clone() {
            let settings = self.color_tables.settings().clone().normalized();
            self.viewer.install_generated_field(raw, &settings);
            self.latest_field = self
                .viewer
                .current_field()
                .cloned()
                .map(std::sync::Arc::new);
            self.formula_result_field = self.latest_field.clone();
        } else {
            // Defensive fallback: Formula Lab retains the styled display field
            // independently even if the viewer's generated cache was reset.
            self.latest_field = Some(fallback.clone());
        }
    }

    pub fn formula_lab_busy(&self) -> bool {
        self.formula_lab.busy()
    }

    pub fn request_formula_lab_open(&mut self) {
        self.formula_lab_open_requested = true;
    }

    pub fn take_formula_lab_open_requested(&mut self) -> bool {
        std::mem::take(&mut self.formula_lab_open_requested)
    }

    pub fn take_formula_lab_open_models_requested(&mut self) -> bool {
        std::mem::take(&mut self.formula_lab_open_models_requested)
    }

    /// Open the dedicated native-CM1 workflow. It is intentionally separate
    /// from the generic WRF/NetCDF picker because CM1 requires explicit local
    /// Cartesian placement before any store write.
    pub fn open_cm1_window(&mut self) {
        self.cm1.open();
        self.repaint.request_repaint();
    }

    pub fn cm1_window_open(&self) -> bool {
        self.cm1.is_open()
    }

    pub fn take_cm1_open_models_requested(&mut self) -> bool {
        std::mem::take(&mut self.cm1_open_models_requested)
    }

    pub fn formula_lab_state_json(&self) -> serde_json::Value {
        self.formula_lab.state_json()
    }

    pub fn apply_formula_lab_state_json(&mut self, value: &serde_json::Value) -> bool {
        self.formula_lab.apply_state_json(value)
    }

    /// Draw auxiliary windows that belong to the shared Models/WRF backend
    /// exactly once per app frame, independent of which owner surface is open.
    pub fn auxiliary_windows(&mut self, ctx: &egui::Context) {
        let import_message = self.import_message.clone();
        if let Some(request) = self.cm1.show_window(
            ctx,
            self.import_job.is_some() || self.formula_lab.busy(),
            import_message.as_deref(),
        ) && self.import_job.is_none()
            && !self.formula_lab.busy()
        {
            self.import_message = Some(format!(
                "Importing CM1 {} across {} selected output(s); opening output {}{}",
                request.variable,
                request.time_indices.len(),
                request.display_time_index,
                request
                    .level_index
                    .map(|level| format!(", native level {level}"))
                    .unwrap_or_default()
            ));
            self.import_job = Some(ImportJob::Cm1(crate::cm1_ui::spawn_import(
                request,
                self.store_root.clone(),
            )));
            self.repaint.request_repaint();
        }

        if let Some(request) = self.cm1.take_radar_request()
            && self.import_job.is_none()
            && !self.formula_lab.busy()
        {
            match self.synth_radar.to_cm1_config() {
                Ok(config) => {
                    self.synthetic_radar_source_files.clear();
                    self.operational_radar_source = None;
                    self.synthetic_radar_replay_source = None;
                    self.synthetic_radar_preview = None;
                    self.import_message = Some(format!(
                        "Building CM1 native REF/VEL from {} across {} record(s); selected record index {} is processed first...",
                        request.source_path.display(),
                        request.time_indices.len(),
                        request.display_time_index,
                    ));
                    self.import_job = Some(ImportJob::SyntheticRadar(
                        crate::wrf_radar::spawn_cm1_radar(request, config),
                    ));
                    self.repaint.request_repaint();
                }
                Err(error) => {
                    self.import_message = Some(format!("CM1 radar setup failed: {error}"))
                }
            }
        }

        if self.gdex.open {
            let cache_dir = settings::gdex_cache_dir();
            self.gdex.ui(ctx, &cache_dir);
        }

        // Native plot and color-table tools are app-wide auxiliaries. Keeping
        // them here makes Formula Lab actions work even when Models is closed
        // or docked behind another active tab, and guarantees one render per
        // app frame through the shared ModelDataDock owner.
        if self.show_plot_viewer {
            let model_plot = self.native_plot_content == NativePlotContent::Model;
            let field: Option<std::sync::Arc<rw_ui::FieldData>> = model_plot
                .then(|| {
                    store_named_current_field(
                        &self.viewer,
                        self.latest_field.as_deref(),
                        &self.hour_store_vars,
                    )
                    .is_some()
                    .then(|| self.latest_field.clone())
                    .flatten()
                })
                .flatten();
            if let Some(field) = &field {
                self.seed_native_plot_domain(field);
            }
            let mut open = true;
            egui::Window::new("Native plot")
                .open(&mut open)
                .default_size([560.0, 440.0])
                .show(ctx, |ui| {
                    if model_plot {
                        self.plot_viewer.ui(ui, field.as_deref());
                    } else {
                        self.satellite_plot.ui(ui);
                    }
                });
            if !open {
                self.show_plot_viewer = false;
            }
        }

        if self.show_color_tables {
            let mut open = true;
            let mut changed = false;
            {
                let field = store_named_current_field(
                    &self.viewer,
                    self.latest_field.as_deref(),
                    &self.hour_store_vars,
                );
                egui::Window::new("Color tables")
                    .open(&mut open)
                    .default_size([520.0, 520.0])
                    .show(ctx, |ui| {
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
        if armed {
            self.box_sounding_armed = false;
        }
    }

    /// Whether the next radar-map drag should build an area-mean sounding.
    pub fn box_sounding_armed(&self) -> bool {
        self.box_sounding_armed
    }

    /// Arm/disarm the map-side area-mean sounding selector. It is mutually
    /// exclusive with the native-plot domain selector because both own a box
    /// drag on the same map canvas.
    pub fn set_box_sounding_armed(&mut self, armed: bool) {
        self.box_sounding_armed = armed;
        if armed {
            self.plot_domain_armed = false;
        }
    }

    /// Start an exact finite-cell area mean for the model hour currently shown
    /// in the field viewer. All disk work stays on the dedicated worker.
    pub fn request_box_sounding(&mut self, bounds: (f64, f64, f64, f64)) -> Result<String, String> {
        let bounds = crate::box_sounding::BoxBounds::new(bounds)?;
        let hour = self
            .viewer
            .hour()
            .cloned()
            .ok_or_else(|| "Load a model field before drawing a box sounding".to_owned())?;
        let grid_ready = self.latest_field.as_ref().is_some_and(|field| {
            field.key.hour == hour
                && field
                    .grid
                    .as_ref()
                    .is_some_and(|grid| grid.nx > 0 && grid.ny > 0)
        });
        if !grid_ready {
            return Err(
                "The displayed model frame has no compatible latitude/longitude grid".to_owned(),
            );
        }
        box_sounding_readiness(&self.hour_store_var_info)?;

        self.box_sounding_armed = false;
        self.sounding_request_mode = SoundingRequestMode::BoxPending;
        self.box_sounding_summary = None;
        self.sounding.set_loading();
        self.box_sounding_pending = Some((hour.clone(), bounds));
        self.box_sounding_task = Some(crate::box_sounding::BoxSoundingTask::spawn(
            self.store_root.clone(),
            hour.clone(),
            bounds,
            self.repaint.clone(),
        ));
        Ok(format!(
            "Building box-mean sounding for {} over {}",
            hour,
            bounds.label()
        ))
    }

    /// A plot domain drawn on the RADAR MAP (📐 arm button or the
    /// Ctrl+Shift+drag shortcut): retarget the native plot viewer at it and
    /// auto-disarm — one box per arming, mirroring the field-viewer
    /// `DomainSelected` path in [`Self::ui`].
    pub fn apply_map_plot_domain(&mut self, domain: CustomDomain) {
        self.plot_domain_armed = false;
        self.native_plot_content = NativePlotContent::Model;
        self.show_plot_viewer = true;
        self.plot_viewer.set_active_domain(domain);
    }

    /// Open a raw real-satellite or SimSat product in the shared native plot
    /// window. Model browser/field state is intentionally untouched, so the
    /// user's current model selection is exactly where they left it when they
    /// return to the model plot.
    pub(crate) fn open_satellite_plot(&mut self, source: SatellitePlotSource) {
        self.satellite_plot.set_source(source);
        self.native_plot_content = NativePlotContent::Satellite;
        self.show_plot_viewer = true;
    }

    /// Clear the external source and return the native surface to the current
    /// model field (when one exists). This does not clear/reselect model data.
    #[allow(dead_code)] // lifecycle hook for future SimSat result-cache clearing
    pub(crate) fn clear_satellite_plot(&mut self) {
        self.satellite_plot.clear_source();
        if self.native_plot_content == NativePlotContent::Satellite {
            self.native_plot_content = NativePlotContent::Model;
            self.show_plot_viewer = self.latest_field.is_some();
        }
    }

    /// Non-dialog test hook. The panel's Save PNG button calls the same
    /// request builder in production.
    #[cfg(test)]
    pub(crate) fn save_satellite_plot_png(
        &self,
        path: &std::path::Path,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        self.satellite_plot.save_png(path, width, height)
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
        if let Some(summary) = &self.box_sounding_summary {
            box_sounding_summary_ui(ui, summary);
            ui.add_space(4.0);
        } else if let Some((hour, bounds)) = &self.box_sounding_pending {
            let theme = crate::ui_theme::theme();
            egui::Frame::new()
                .fill(theme.faint)
                .stroke(egui::Stroke::new(1.0, theme.hairline))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new("BOX-MEAN SOUNDING").strong());
                        ui.weak(format!("{} · {}", hour, bounds.label()));
                    });
                    ui.weak("Averaging primitive columns; diagnostics will run after the mean column is complete.");
                });
            ui.add_space(4.0);
        }
        self.sounding.ui(ui);
    }

    /// The latest sounding belongs to the box-mean path. The app uses this to
    /// keep point-only surface-observation adjustment away from an area mean.
    pub fn latest_sounding_is_box(&self) -> bool {
        self.sounding_request_mode == SoundingRequestMode::BoxApplied
            && self.box_sounding_summary.is_some()
    }

    pub fn sounding_view_state_json(&self) -> serde_json::Value {
        self.sounding.view_state_json()
    }

    pub fn apply_sounding_view_state_json(&mut self, value: &serde_json::Value) -> bool {
        self.sounding.apply_view_state_json(value)
    }

    /// Nudge the reusable sounding panel to a readable default canvas
    /// ("Canvas"/scene) zoom, used when a model sounding first docks beside
    /// the plot in the narrower docked column. Only the scene zoom changes;
    /// every other view-state field keeps its current value.
    pub fn set_default_sounding_scene_zoom(&mut self, zoom: f32) {
        let mut view_state = self.sounding.view_state_json();
        patch_sounding_scene_zoom(&mut view_state, zoom);
        self.sounding.apply_view_state_json(&view_state);
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
        match SyntheticRadarUiState::from_persisted_value(value) {
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
        // A re-enumerated run may keep the same model/run/hour identity while
        // its manifest and variables changed. Clear compatibility metadata
        // immediately and refresh that hour after the queued enumeration so
        // Formula Lab can never report Ready from the previous revision.
        self.hour_store_vars.clear();
        self.hour_store_var_info.clear();
        if let Some(selected) = self.browser.selected().cloned() {
            self.worker.send(StoreRequest::LoadHour(selected));
        }
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
                /// `(store_root, model, run)` of a run that just landed in
                /// the store — the auto-plot trigger. `None` for failures
                /// and for jobs with no store output.
                plot: Option<(PathBuf, String, String)>,
            },
            /// A CM1 plane landed with an exact hour key. Select it before
            /// opening Models so the user sees the requested field/run.
            FinishedCm1 {
                message: String,
                summary: crate::cm1_ui::Cm1ImportSummary,
            },
            /// A synthetic-radar job finished: its volumes go to the app to
            /// loop, not to the store, so this carries the output out.
            FinishedSynthetic {
                message: String,
                output: crate::wrf_radar::SyntheticRadarOutput,
            },
            /// The first cut is displayable while the same worker continues.
            SyntheticPreview {
                message: String,
                preview: crate::wrf_radar::SyntheticRadarPreview,
            },
            /// An exact observed-geometry replay finished. Unlike an ordinary
            /// synthetic loop this is consumed as a three-volume comparison.
            FinishedReplay {
                message: String,
                output: crate::wrf_radar::SyntheticRadarReplayOutput,
            },
            /// User-requested cancellation is a normal terminal state, not a
            /// failed science/backend run and never triggers a store rescan.
            Cancelled,
        }

        let result = match self.import_job.as_ref() {
            None => PollResult::Idle,
            Some(ImportJob::Cm1(task)) => {
                let mut latest = None;
                let mut done = None;
                loop {
                    match task.rx.try_recv() {
                        Ok(crate::cm1_ui::Cm1ImportMessage::Progress(message)) => {
                            latest = Some(message);
                        }
                        Ok(crate::cm1_ui::Cm1ImportMessage::Done(outcome)) => {
                            done = Some(outcome);
                            break;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            done = Some(Err("CM1 import worker stopped unexpectedly".to_owned()));
                            break;
                        }
                    }
                }
                match done {
                    Some(Ok(summary)) => PollResult::FinishedCm1 {
                        message: summary.completion_message(),
                        summary,
                    },
                    Some(Err(error)) => PollResult::Finished {
                        message: format!("CM1 import failed: {error}"),
                        plot: None,
                    },
                    None => match latest {
                        Some(message) => PollResult::Progress(message),
                        None => PollResult::Progress(String::new()),
                    },
                }
            }
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
                            "Imported {} hour(s) from {} file(s) -> run “{}” ({} variables){}",
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
                        plot: Some((summary.store_root, summary.model, summary.run)),
                    },
                    Some(Err(error)) => PollResult::Finished {
                        message: format!("Import failed: {error}"),
                        plot: None,
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
                            "Processed {} WRF hour(s) from {} file(s) -> run “{}” ({} variables)",
                            summary.hours_written,
                            summary.files_seen,
                            summary.run,
                            summary.variables.len()
                        ),
                        plot: Some((summary.store_root, summary.model, summary.run)),
                    },
                    Some(Err(error)) => PollResult::Finished {
                        message: format!("WRF processing failed: {error}"),
                        plot: None,
                    },
                    None => match latest {
                        Some(message) => PollResult::Progress(message),
                        None => PollResult::Progress(String::new()),
                    },
                }
            }
            Some(ImportJob::SyntheticRadar(task)) => {
                let mut latest = None;
                let mut preview = None;
                let mut done = None;
                loop {
                    match task.rx.try_recv() {
                        Ok(crate::wrf_radar::SyntheticRadarMessage::Progress(message)) => {
                            latest = Some(message);
                        }
                        Ok(crate::wrf_radar::SyntheticRadarMessage::Preview(update)) => {
                            preview = Some(update);
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
                    Some(_) if task.cancellation_requested() => PollResult::Cancelled,
                    Some(Ok(output)) => {
                        let ducting = output
                            .notes
                            .iter()
                            .find(|note| note.contains("DUCTING LAYER PRESENT"))
                            .map(|note| format!(" — {note}"))
                            .unwrap_or_default();
                        PollResult::FinishedSynthetic {
                            message: format!(
                                "Built {} radar frame(s) from {} — looping in the radar view{ducting}",
                                output.volumes.len(),
                                output.label
                            ),
                            output,
                        }
                    }
                    Some(Err(error)) if crate::wrf_radar::is_synthetic_radar_cancelled(&error) => {
                        PollResult::Cancelled
                    }
                    Some(Err(error)) => PollResult::Finished {
                        message: format!("Synthetic radar failed: {error}"),
                        plot: None,
                    },
                    None => match preview {
                        Some(preview) => PollResult::SyntheticPreview {
                            message: format!(
                                "Showing tilt {}/{} now; the remaining tilts are still processing",
                                preview.completed_cuts, preview.total_cuts
                            ),
                            preview,
                        },
                        None => match latest {
                            Some(message) => PollResult::Progress(message),
                            None => PollResult::Progress(String::new()),
                        },
                    },
                }
            }
            Some(ImportJob::SyntheticRadarReplay(task)) => {
                let mut latest = None;
                let mut done = None;
                loop {
                    match task.rx.try_recv() {
                        Ok(crate::wrf_radar::SyntheticRadarReplayMessage::Progress(message)) => {
                            latest = Some(message);
                        }
                        Ok(crate::wrf_radar::SyntheticRadarReplayMessage::Done(outcome)) => {
                            done = Some(outcome);
                            break;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            done = Some(Err(
                                "Exact radar replay worker stopped unexpectedly".to_owned()
                            ));
                            break;
                        }
                    }
                }
                match done {
                    Some(_) if task.cancellation_requested() => PollResult::Cancelled,
                    Some(Ok(output)) => PollResult::FinishedReplay {
                        message: format!(
                            "Replayed {} WRF frame(s) through the displayed observed scan",
                            output.frames.len()
                        ),
                        output,
                    },
                    Some(Err(error)) if crate::wrf_radar::is_synthetic_radar_cancelled(&error) => {
                        PollResult::Cancelled
                    }
                    Some(Err(error)) => PollResult::Finished {
                        message: format!("Exact radar replay failed: {error}"),
                        plot: None,
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
            PollResult::Finished { message, plot } => {
                self.import_message = Some(message);
                self.import_job = None;
                self.rescan();
                // Auto-plot: bulk imports produce bulk output when the
                // toggle is on. All store-writing completions (wrench
                // heavy, light 📄, GDEX via the local pump) land here.
                if self.wrf_options.auto_plot
                    && let Some((store_root, model, run)) = plot
                {
                    self.start_plot_job(store_root, model, run);
                }
            }
            PollResult::FinishedCm1 { message, summary } => {
                self.import_message = Some(message);
                self.import_job = None;
                self.select_hour_key(summary.hour.clone());
                self.rescan();
                // The button promises an open-in-Models handoff. Leaving the
                // floating CM1 workspace above the newly opened Models pane
                // made a successful import look as though nothing happened.
                self.cm1.close();
                self.cm1_open_models_requested = true;
                if self.wrf_options.auto_plot {
                    self.start_plot_job(summary.store_root, summary.model, summary.run);
                }
                self.repaint.request_repaint();
            }
            PollResult::FinishedSynthetic { message, output } => {
                self.import_message = Some(message);
                self.import_job = None;
                self.synthetic_radar_preview = None;
                // Retain Arc handles for the CfRadial export control — the
                // one-shot result below is drained away by the app.
                self.synthetic_export_frames = output.volumes.clone();
                // Hand the simulated volumes to the app (drained in
                // `poll_model_layer`); nothing was written to the store, so no
                // rescan.
                self.synthetic_radar_result = Some(output);
                self.repaint.request_repaint();
            }
            PollResult::SyntheticPreview { message, preview } => {
                self.import_message = Some(message);
                self.synthetic_radar_preview = Some(preview);
                // Keep `import_job`: later cuts continue on the same worker.
                self.repaint.request_repaint();
            }
            PollResult::FinishedReplay { message, output } => {
                self.import_message = Some(message);
                self.import_job = None;
                self.synthetic_export_frames = output
                    .frames
                    .iter()
                    .map(|frame| frame.simulated.clone())
                    .collect();
                self.synthetic_radar_replay_result = Some(output);
                self.repaint.request_repaint();
            }
            PollResult::Cancelled => {
                self.import_message = Some("Synthetic radar cancelled".to_owned());
                self.import_job = None;
                self.synthetic_radar_preview = None;
                self.repaint.request_repaint();
            }
        }
    }

    /// Drain the GDEX browser's background workers (catalog crawl / NCSS
    /// metadata / download). A completed download is queued for import and, if
    /// no import is already running, launched immediately through the SAME
    /// light-import pump as the manual model import. Runs every frame from
    /// `handle_responses` (thus `pump`), so a GDEX download finishes, imports,
    /// and plots even while the model window is closed.
    fn poll_gdex(&mut self) {
        let ctx = self.repaint.clone();
        if let Some(path) = self.gdex.poll(&ctx) {
            self.gdex_import_queue.push_back(path);
        }
        if self.import_job.is_none()
            && !self.formula_lab.busy()
            && let Some(path) = self.gdex_import_queue.pop_front()
        {
            self.launch_gdex_import(path);
        }
    }

    /// Import a file a GDEX download produced, reusing the existing light
    /// import (`local_import::spawn_import_paths`) — the same progress pump,
    /// store write, re-scan, and plot as the manual model import. Ungated: the
    /// GDEX browser is an in-app tree (no rfd), so this handoff compiles and
    /// works on every platform, including the headless verify nodes. Setting
    /// `import_job` also engages the live-refresh crash guard
    /// (`import_in_flight`) automatically.
    fn launch_gdex_import(&mut self, path: PathBuf) {
        if self.formula_lab.busy() {
            self.gdex_import_queue.push_front(path);
            self.import_message =
                Some("GDEX import is waiting for the Formula Lab evaluation to finish".to_owned());
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let task = crate::local_import::spawn_import_paths(vec![path], self.store_root.clone());
        self.import_message = Some(format!("Importing {name} (from GDEX)…"));
        self.import_job = Some(ImportJob::Local(task));
    }

    /// Launch the batch "plot everything" job for a run in the store —
    /// from the auto-plot trigger or the manual "Plot all fields…" button.
    /// Single slot: a second request while one runs is refused with a status
    /// line (matching the single-slot import job).
    fn start_plot_job(&mut self, store_root: PathBuf, model: String, run: String) {
        if self.formula_lab.busy() {
            self.plot_message =
                Some("Batch plotting cannot start while Formula Lab is evaluating".to_owned());
            return;
        }
        if self.plot_job.is_some() {
            self.plot_message =
                Some("A batch plot job is already running — cancel it first.".to_owned());
            return;
        }
        self.plot_done = None;
        self.plot_message = Some(format!("Plotting all fields of {model}/{run}…"));
        let task = crate::batch_plots::spawn_batch_plot(
            crate::batch_plots::BatchPlotRequest {
                store_root,
                model,
                run,
                plots_base: self.plots_base.clone(),
                overrides: self.color_tables.settings().clone().normalized(),
                options: crate::batch_plots::BatchPlotOptions::default(),
            },
            self.repaint.clone(),
        );
        self.plot_job = Some(task);
    }

    /// Drain the running batch plot job (same pattern as `poll_import`):
    /// newest progress line wins, a terminal `Done` clears the slot and
    /// keeps the summary for the "Open plots folder" button.
    fn poll_plot_job(&mut self) {
        let Some(task) = self.plot_job.as_ref() else {
            return;
        };
        let mut latest = None;
        let mut done = None;
        loop {
            match task.rx.try_recv() {
                Ok(crate::batch_plots::BatchPlotMessage::Progress(message)) => {
                    latest = Some(message);
                }
                Ok(crate::batch_plots::BatchPlotMessage::Done(outcome)) => {
                    done = Some(outcome);
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    done = Some(Err("Plot worker stopped unexpectedly".to_owned()));
                    break;
                }
            }
        }
        match done {
            Some(Ok(summary)) => {
                self.plot_message = Some(summary.status_line());
                self.plot_done = Some(summary);
                self.plot_job = None;
            }
            Some(Err(error)) => {
                self.plot_message = Some(format!("Batch plotting failed: {error}"));
                self.plot_job = None;
            }
            None => {
                if let Some(message) = latest {
                    self.plot_message = Some(message);
                    self.repaint.request_repaint();
                }
            }
        }
    }

    /// One-shot: take finished synthetic-radar volumes and their private,
    /// session-only source descriptors for installation into the loop engine.
    pub fn take_synthetic_radar(&mut self) -> Option<crate::wrf_radar::SyntheticRadarOutput> {
        self.synthetic_radar_result.take()
    }

    /// One-shot: take the first completed tilt while its worker continues.
    pub fn take_synthetic_radar_preview(
        &mut self,
    ) -> Option<crate::wrf_radar::SyntheticRadarPreview> {
        self.synthetic_radar_preview.take()
    }

    /// One-shot: take a completed exact observed-geometry replay. The app
    /// installs the first returned frame as a linked three-pane validation
    /// workspace; the replay action deliberately selects one WRF source file.
    pub fn take_synthetic_radar_replay(
        &mut self,
    ) -> Option<crate::wrf_radar::SyntheticRadarReplayOutput> {
        self.synthetic_radar_replay_result.take()
    }

    /// Update the exact-replay candidate from the radar viewer. Synthetic and
    /// difference volumes are intentionally excluded so Replay always starts
    /// from an independently observed acquisition.
    pub fn set_displayed_radar_for_replay(
        &mut self,
        volume: Option<std::sync::Arc<radar_core::RadarVolume>>,
    ) {
        self.displayed_replay_source = volume.filter(|volume| {
            let provenance_is_synthetic = volume
                .metadata
                .forward_operator
                .as_deref()
                .is_some_and(|value| value.contains("BowEcho WRF"))
                || volume
                    .metadata
                    .archive_version
                    .as_deref()
                    .is_some_and(|value| value.starts_with("simulated-wrf"));
            !provenance_is_synthetic
                && volume.site.latitude_deg.is_some()
                && volume.site.longitude_deg.is_some()
                && !volume.cuts.is_empty()
                && volume.cuts.iter().all(|cut| !cut.radials.is_empty())
        });
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
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        self.import_job = Some(ImportJob::SyntheticRadar(task));
    }

    /// The Models window now owns stored-run browsing and output. Raw WRF
    /// workflows live in [`Self::wrf_ui`] so neither surface becomes another
    /// wall of unrelated controls.
    fn model_library_controls(&mut self, ui: &mut egui::Ui) {
        ui.label(model_section_heading("Model output"));
        ui.label(
            egui::RichText::new(
                "Render the selected stored run. Raw wrfout processing and simulated radar now live in Windows > WRF.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("CM1 native output");
                ui.weak("Local Cartesian scalar inventory · explicit anchor · moving-domain placement");
            });
            ui.label(
                egui::RichText::new(
                    "Inspect cm1out directly, choose one output or its full exact-time loop and a native level, then store it under model=cm1. Native u/v/w are explicitly destaggered; CM1 files never pass through the WRF reader.",
                )
                .small()
                .weak(),
            );
            if ui.button("Open CM1 import…").clicked() {
                self.open_cm1_window();
            }
        });
        ui.add_space(8.0);
        model_subheading(ui, "Batch plots");
        self.plot_controls(ui);
    }

    /// Dedicated first-class WRF workspace. It deliberately shares this
    /// `ModelDataDock` with Models: one store worker, one selected run, one
    /// Formula Lab, and one synthetic-radar result queue.
    pub fn wrf_ui(&mut self, ui: &mut egui::Ui) {
        self.handle_responses();
        // Keep long-running job state and Cancel fixed above the scrolling
        // workspace so it cannot disappear below a tall recipe/control panel.
        self.import_status_ui(ui);
        egui::ScrollArea::vertical()
            .id_salt("wrf_workspace_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.label(model_section_heading("WRF workspace"));
                ui.label(
                    egui::RichText::new(
                        "Open raw wrfout data, compute wrf-rust diagnostics, build simulated radar, or create custom formulas. Finished fields remain available in Models for plotting.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(6.0);
                self.import_pickers(ui);

                ui.add_space(10.0);
                egui::Frame::group(ui.style()).show(ui, |ui| self.formula_controls(ui));

                ui.add_space(8.0);
                egui::CollapsingHeader::new(model_section_heading("WRF archives (GDEX)"))
                    .id_salt("wrf_gdex_source")
                    .default_open(false)
                    .show(ui, |ui| self.gdex_controls(ui));

            });
    }

    fn gdex_controls(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(
                "Browse NSF NCAR CONUS II regional-climate WRF data and import it into the shared model store.",
            )
            .small()
            .weak(),
        );
        ui.horizontal(|ui| {
            let gdex_busy = self.gdex.busy();
            let reserved = if gdex_busy {
                ui.spacing().item_spacing.x + 18.0
            } else {
                0.0
            };
            let response = ui
                .add_sized(
                    [(ui.available_width() - reserved).max(120.0), 26.0],
                    egui::Button::new("Browse GDEX catalog…"),
                )
                .on_hover_text(
                    "Browse the NSF NCAR GDEX online catalog — CONUS II regional climate WRF (present + future). Download a whole file or an NCSS subset; it imports into the model store like any other run.",
                );
            if response.clicked() {
                self.gdex.open = true;
            }
            if gdex_busy {
                ui.spinner();
            }
        });
    }

    fn import_status_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(message) = self.import_message.clone() {
            let cancel_requested = match self.import_job.as_ref() {
                Some(ImportJob::SyntheticRadar(task)) => task.cancellation_requested(),
                Some(ImportJob::SyntheticRadarReplay(task)) => task.cancellation_requested(),
                _ => false,
            };
            let cancellable = matches!(
                self.import_job.as_ref(),
                Some(ImportJob::SyntheticRadar(_)) | Some(ImportJob::SyntheticRadarReplay(_))
            );
            let mut cancel_clicked = false;
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if self.import_job.is_some() {
                    ui.spinner();
                }
                if cancellable {
                    cancel_clicked = ui
                        .add_enabled(
                            !cancel_requested,
                            egui::Button::new(if cancel_requested {
                                "Cancelling…"
                            } else {
                                "Cancel"
                            }),
                        )
                        .on_hover_text(
                            "Stop after the current bounded radar work unit; completed preview tilts are removed.",
                        )
                        .clicked();
                }
                crate::panel_kit::status_block(ui, &message, None);
            });
            if cancel_clicked {
                match self.import_job.as_ref() {
                    Some(ImportJob::SyntheticRadar(task)) => task.request_cancel(),
                    Some(ImportJob::SyntheticRadarReplay(task)) => task.request_cancel(),
                    _ => {}
                }
                self.import_message = Some("Cancelling synthetic radar…".to_owned());
                self.repaint.request_repaint();
            }
        }
    }

    /// Formula Lab launcher and optional unrestricted raw-WRF source picker.
    /// Kept together as one tool instead of two unrelated buttons in the
    /// acquisition button wall.
    fn formula_controls(&mut self, ui: &mut egui::Ui) {
        model_subheading(ui, "Formula Lab");
        ui.label(
            egui::RichText::new(
                "Build custom diagnostics from the selected run, or attach a raw WRF source \
                 for grid-aware formulas.",
            )
            .small()
            .weak(),
        );
        ui.horizontal(|ui| {
            let spacing = ui.spacing().item_spacing.x;
            let width = ((ui.available_width() - spacing) * 0.5).max(90.0);
            if ui
                .add_sized([width, 26.0], egui::Button::new("Open Formula Lab"))
                .on_hover_text(
                    "Compile safe custom diagnostics against the selected run from any model.",
                )
                .clicked()
            {
                self.request_formula_lab_open();
            }

            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            if ui
                .add_sized([width, 26.0], egui::Button::new("Choose WRF source…"))
                .on_hover_text(
                    "Choose any raw WRF file, including extensionless wrfout_* files from \
                     any domain. No extension or filename filter is applied.",
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("Choose any raw WRF file for Formula Lab")
                    .pick_file()
            {
                self.formula_raw_path = Some(path);
                self.formula_lab.set_source_kind(FormulaSourceKind::RawWrf);
                self.request_formula_lab_open();
            }

            #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
            ui.add_enabled_ui(false, |ui| {
                ui.add_sized([width, 26.0], egui::Button::new("Choose WRF source…"));
            });
        });
        if let Some(path) = self.formula_raw_path.clone() {
            let label = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("raw WRF file");
            let mut clear = false;
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!("Source: {label}"))
                        .small()
                        .weak(),
                )
                .on_hover_text(path.display().to_string());
                clear = ui.small_button("Clear").clicked();
            });
            if clear {
                self.formula_raw_path = None;
                if self.formula_lab.source_kind() == FormulaSourceKind::RawWrf {
                    self.formula_lab.set_source_kind(FormulaSourceKind::Store);
                }
            }
        }
    }

    /// Batch "plot everything" controls: the auto-plot toggle, a manual
    /// launch for the selected run (any model, not just WRF), progress and
    /// Cancel while the job runs, and the completion line with an "Open
    /// plots folder" button. Unconditional (no file dialog involved), so it
    /// works on every platform.
    fn plot_controls(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(
            &mut self.wrf_options.auto_plot,
            "Automatically plot new imports",
        )
        .on_hover_text(
            "When an import finishes (file, folder, or GDEX), render every field of the \
             new run — all forecast hours — to PNG under the screenshots folder.",
        );

        let selected = self.browser.selected().cloned();
        let can_plot = selected.is_some() && self.plot_job.is_none() && !self.formula_lab.busy();
        let hover = match &selected {
            Some(hour) => format!(
                "Render every field of {}/{} (all hours) to PNG now.",
                hour.model, hour.run
            ),
            None => "Select a model run in the library first.".to_owned(),
        };
        if ui
            .add_enabled_ui(can_plot, |ui| {
                ui.add_sized(
                    [ui.available_width().max(120.0), 26.0],
                    egui::Button::new("Plot selected run…"),
                )
            })
            .inner
            .on_hover_text(hover)
            .clicked()
            && let Some(hour) = selected
        {
            self.start_plot_job(self.store_root.clone(), hour.model, hour.run);
        }
        if let Some(task) = &self.plot_job {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("Rendering plots").small().weak());
                if ui
                    .button("Cancel")
                    .on_hover_text("Stop after the current plot; finished PNGs are kept.")
                    .clicked()
                {
                    task.cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
        if let Some(message) = &self.plot_message {
            crate::panel_kit::status_block(ui, message, None);
        }
        if self.plot_done.is_some()
            && ui
                .add_sized(
                    [ui.available_width().max(120.0), 24.0],
                    egui::Button::new("Open plots folder"),
                )
                .clicked()
            && let Some(summary) = &self.plot_done
            && let Err(error) = crate::table_editor::show_in_file_browser(&summary.out_dir)
        {
            self.plot_message = Some(error);
        }
    }

    /// Native file and folder pickers that spawn the ingest.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn import_pickers(&mut self, ui: &mut egui::Ui) {
        let busy = self.import_job.is_some() || self.formula_lab.busy();
        model_workflow_card(
            ui,
            "Open WRF / NetCDF",
            "Quickly load surface fields and sounding volumes from raw wrfout, climate WRF, or NetCDF data.",
            |ui| {
                ui.horizontal(|ui| {
                    let spacing = ui.spacing().item_spacing.x;
                    let width = ((ui.available_width() - spacing) * 0.5).max(82.0);
                    if ui
                        .add_enabled_ui(!busy, |ui| {
                            ui.add_sized([width, 26.0], egui::Button::new("Open file…"))
                        })
                        .inner
                        .on_hover_text(
                            "Import one WRF/NetCDF forecast hour: 2D surface fields and \
                             sounding volumes. Raw extensionless wrfout files are supported.",
                        )
                        .clicked()
                        && let Some(file) = rfd::FileDialog::new()
                            .set_title("Choose a WRF/NetCDF file to import")
                            .pick_file()
                    {
                        self.gate_or_launch_light_import(vec![file]);
                    }
                    if ui
                        .add_enabled_ui(!busy, |ui| {
                            ui.add_sized([width, 26.0], egui::Button::new("Open folder…"))
                        })
                        .inner
                        .on_hover_text(
                            "Import every supported WRF/NetCDF file in a folder; each file \
                             becomes a forecast hour.",
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
                });
                let retained_source = retained_namelist_source(self.formula_raw_path.as_deref());
                let source_hint = retained_source.as_ref().map_or_else(
                    || {
                        "Choose a raw wrfout, reconstruct the subset of namelist.input metadata it actually stores, then save the annotated result."
                            .to_owned()
                    },
                    |path| {
                        format!(
                            "Reconstruct from the retained raw WRF source {} and save the annotated result. The output is explicitly marked partial and non-reproducible.",
                            path.file_name()
                                .map(|name| name.to_string_lossy())
                                .unwrap_or_else(|| path.as_os_str().to_string_lossy())
                        )
                    },
                );
                if let Some(path) = retained_source.as_ref() {
                    let file_name = path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_else(|| path.as_os_str().to_string_lossy());
                    ui.label(
                        egui::RichText::new(format!("Namelist source: {file_name}"))
                            .small()
                            .weak(),
                    )
                    .on_hover_text(path.display().to_string());
                } else {
                    ui.label(
                        egui::RichText::new("No retained wrfout — asks on click")
                            .small()
                            .weak(),
                    );
                }
                if ui
                    .add_enabled_ui(!busy, |ui| {
                        ui.add_sized(
                            [ui.available_width().max(120.0), 24.0],
                            egui::Button::new("Extract namelist…"),
                        )
                    })
                    .inner
                    .on_hover_text(source_hint)
                    .clicked()
                {
                    self.extract_wrf_namelist_dialog();
                }
            },
        );

        ui.add_space(6.0);
        model_workflow_card(
            ui,
            "WRF full diagnostics",
            "Run the wrf-rust severe and thermodynamic suite: CAPE/CIN, shear, SRH, STP/SCP/EHI, LCL/LFC/EL, precipitation, and more.",
            |ui| {
                ui.horizontal(|ui| {
                    let spacing = ui.spacing().item_spacing.x;
                    let width = ((ui.available_width() - spacing) * 0.5).max(82.0);
                    if ui
                        .add_enabled_ui(!busy, |ui| {
                            ui.add_sized([width, 26.0], egui::Button::new("Process files…"))
                        })
                        .inner
                        .on_hover_text(
                            "Multi-select one or hundreds of raw wrfout files. Each becomes \
                             a forecast hour and receives the selected diagnostic suite.",
                        )
                        .clicked()
                        && let Some(picked) = rfd::FileDialog::new()
                            .set_title(
                                "Choose raw wrfout file(s) — multi-select processes 1 to hundreds",
                            )
                            .pick_files()
                    {
                        let files: Vec<PathBuf> = picked
                            .into_iter()
                            .filter(|path| crate::wrf_process::is_supported_wrf_file(path))
                            .collect();
                        self.gate_or_launch_heavy_import(files);
                    }
                    if ui
                        .add_enabled_ui(!busy, |ui| {
                            ui.add_sized([width, 26.0], egui::Button::new("Process folder…"))
                        })
                        .inner
                        .on_hover_text(
                            "Run the selected diagnostics over every raw wrfout file in a folder.",
                        )
                        .clicked()
                        && let Some(dir) = rfd::FileDialog::new()
                            .set_title("Choose a WRF folder for the wrf-rust severe/thermo import")
                            .pick_folder()
                    {
                        let files = crate::wrf_process::wrf_files_in_folder(&dir);
                        if files.is_empty() {
                            self.import_message =
                                Some(format!("No WRF files under {}", dir.display()));
                        } else {
                            self.gate_or_launch_heavy_import(files);
                        }
                    }
                });
                self.wrf_options_panel(ui);
            },
        );

        self.heavy_import_warning_ui(ui);
        self.light_import_warning_ui(ui);

        ui.add_space(6.0);
        model_workflow_card(
            ui,
            "WRF simulated radar",
            "Forward-model WRF hydrometeors and winds into a fast, loopable NEXRAD-style reflectivity and radial-velocity scan.",
            |ui| {
                self.synthetic_radar_recipe_ui(ui, busy);
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    let spacing = ui.spacing().item_spacing.x;
                    let width = ((ui.available_width() - spacing) * 0.5).max(82.0);
                    if ui
                        .add_enabled_ui(!busy, |ui| {
                            ui.add_sized([width, 26.0], egui::Button::new("Build from files…"))
                        })
                        .inner
                        .on_hover_text(
                            "Pick one or more wrfout files; each forecast time becomes a \
                             radar-loop frame. Nothing is written to the model store.",
                        )
                        .clicked()
                    {
                        match self.synth_radar.to_config() {
                            Err(message) => self.import_message = Some(message),
                            Ok(config) => {
                                if let Some(files) = rfd::FileDialog::new()
                                    .set_title(
                                        "Choose raw wrfout file(s) — multi-select builds a loop",
                                    )
                                    .pick_files()
                                {
                                    self.launch_synthetic_radar(files, config);
                                }
                            }
                        }
                    }
                    if ui
                        .add_enabled_ui(!busy, |ui| {
                            ui.add_sized([width, 26.0], egui::Button::new("Build from folder…"))
                        })
                        .inner
                        .on_hover_text("Build one radar-loop frame per wrfout file in a folder.")
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
                let can_replay = !busy && self.displayed_replay_source.is_some();
                let replay_hover = if self.displayed_replay_source.is_some() {
                    "Choose one raw wrfout file and simulate it through the displayed observed scan's exact cuts, rays, acquisition times, gate layout, split cuts, missing sectors, moment availability, Nyquist, and PRT. Opens linked Observed / Simulated / Difference panes."
                } else {
                    "Load an observed radar volume with valid site coordinates and ray geometry first. Synthetic and difference volumes cannot be replay templates."
                };
                if ui
                    .add_enabled_ui(can_replay, |ui| {
                        ui.add_sized(
                            [ui.available_width().max(120.0), 28.0],
                            egui::Button::new("Replay displayed observed scan…"),
                        )
                    })
                    .inner
                    .on_hover_text(replay_hover)
                    .clicked()
                {
                    match self.synth_radar.to_config() {
                        Err(message) => self.import_message = Some(message),
                        Ok(config) => {
                            if let Some(file) = rfd::FileDialog::new()
                                .set_title("Choose one raw wrfout file for exact scan replay")
                                .pick_file()
                            {
                                self.launch_synthetic_radar_replay(file, config);
                            }
                        }
                    }
                }
                let source_count = self.synthetic_radar_source_files.len();
                let can_refresh = !busy && source_count > 0;
                let refresh_hover = if source_count == 0 {
                    "Build simulated radar from files or a folder first; BowEcho will remember that frame set for this session."
                        .to_owned()
                } else {
                    format!(
                        "Rebuild the current radar frame/loop from the same {source_count} WRF source file(s), using every control as it is set now."
                    )
                };
                if ui
                    .add_enabled_ui(can_refresh, |ui| {
                        ui.add_sized(
                            [ui.available_width().max(120.0), 26.0],
                            egui::Button::new("Refresh current frame(s)"),
                        )
                    })
                    .inner
                    .on_hover_text(refresh_hover)
                    .clicked()
                {
                    self.refresh_synthetic_radar();
                }
                let frame_count = self.synthetic_export_frames.len();
                if ui
                    .add_enabled_ui(frame_count > 0, |ui| {
                        ui.add_sized(
                            [ui.available_width().max(120.0), 24.0],
                            egui::Button::new("Export latest as CfRadial…"),
                        )
                    })
                    .inner
                    .on_hover_text(
                        "Save the latest simulated-radar frame or loop as CfRadial-1 NetCDF \
                         for BowEcho, Py-ART, and other radar toolkits.",
                    )
                    .clicked()
                {
                    self.export_synthetic_radar_frames();
                }
                self.synthetic_radar_site_panel(ui);
            },
        );

        ui.add_space(6.0);
        model_workflow_card(
            ui,
            "Operational forecast radar",
            "Turn native HRRR or RRFS hybrid-level GRIB into ordinary loopable radar volumes using the same radar controls and export path as WRF.",
            |ui| self.operational_radar_ui(ui, busy),
        );
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn operational_radar_ui(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.label(
            egui::RichText::new(
                "Native category mass/number fields feed BowEcho's versioned bulk S-band operator. Polar products are dual-pol-like bulk assumptions — not T-matrix output or observed calibration.",
            )
            .small()
            .weak(),
        );
        let placement = match self.synth_radar.placement {
            SynthPlacement::DomainCenter => "native model domain centre".to_owned(),
            SynthPlacement::LatLon => format!(
                "lat {}, lon {}",
                self.synth_radar.lat_text.trim(),
                self.synth_radar.lon_text.trim()
            ),
            SynthPlacement::NexradSite => self.synth_radar.site_id_text.trim().to_uppercase(),
        };
        ui.label(
            egui::RichText::new(format!(
                "Radar: {placement} · {:.0} km range · scan/instrument controls shared with WRF above",
                self.synth_radar.max_range_km
            ))
            .small(),
        );

        ui.horizontal_wrapped(|ui| {
            ui.label("Latest HRRR frames:");
            ui.add_enabled_ui(!busy, |ui| {
                ui.toggle_value(&mut self.synth_radar.operational_f00, "f00");
                ui.toggle_value(&mut self.synth_radar.operational_f01, "f01");
            });
        });
        let forecast_hours = self.synth_radar.operational_forecast_hours();
        ui.horizontal(|ui| {
            let spacing = ui.spacing().item_spacing.x;
            let width = ((ui.available_width() - spacing) * 0.5).max(90.0);
            if ui
                .add_enabled_ui(!busy && !forecast_hours.is_empty(), |ui| {
                    ui.add_sized([width, 27.0], egui::Button::new("Latest HRRR · build"))
                })
                .inner
                .on_hover_text(
                    "Reuse a matching SimSat/model-cache native file when present; otherwise use SimSat's resumable NOMADS/AWS downloader. Selecting f00 + f01 builds a two-frame loop from one cycle.",
                )
                .clicked()
            {
                let source = crate::wrf_radar::OperationalRadarSource::LatestHrrr {
                    forecast_hours,
                    input_root: settings::simsat_input_dir(),
                    additional_cache_roots: vec![settings::model_cache_dir()],
                };
                match self.synth_radar.to_operational_config() {
                    Ok(config) => self.launch_operational_radar(source, config),
                    Err(error) => self.import_message = Some(error),
                }
            }
            if ui
                .add_enabled_ui(!busy, |ui| {
                    ui.add_sized([width, 27.0], egui::Button::new("Open local GRIB…"))
                })
                .inner
                .on_hover_text(
                    "Choose one or more native HRRR/RRFS GRIB files with no filename or extension restriction. Each compatible valid time becomes a frame.",
                )
                .clicked()
                && let Some(paths) = rfd::FileDialog::new()
                    .set_title("Choose native HRRR/RRFS GRIB file(s) for forecast radar")
                    .pick_files()
            {
                match self.synth_radar.to_operational_config() {
                    Ok(config) => self.launch_operational_radar(
                        crate::wrf_radar::OperationalRadarSource::Files(paths),
                        config,
                    ),
                    Err(error) => self.import_message = Some(error),
                }
            }
        });

        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("Scan shared cache"))
                .on_hover_text(
                    "Discover complete native HRRR files in the same SimSat input and model-cache directories; no second cache is created.",
                )
                .clicked()
            {
                self.refresh_operational_cache();
            }
            let selected_label = self
                .operational_cached_inputs
                .get(self.operational_cached_selected)
                .map(|input| input.label.as_str())
                .unwrap_or("No cached native file scanned");
            egui::ComboBox::from_id_salt("operational_radar_cached_hrrr")
                .selected_text(selected_label)
                .width(310.0)
                .show_ui(ui, |ui| {
                    for (index, input) in self.operational_cached_inputs.iter().enumerate() {
                        ui.selectable_value(
                            &mut self.operational_cached_selected,
                            index,
                            &input.label,
                        )
                        .on_hover_text(input.path.display().to_string());
                    }
                });
        });
        let selected_cached = self
            .operational_cached_inputs
            .get(self.operational_cached_selected)
            .map(|input| input.path.clone());
        ui.horizontal(|ui| {
            let spacing = ui.spacing().item_spacing.x;
            let width = ((ui.available_width() - spacing) * 0.5).max(90.0);
            if ui
                .add_enabled_ui(!busy && selected_cached.is_some(), |ui| {
                    ui.add_sized([width, 25.0], egui::Button::new("Build cached file"))
                })
                .inner
                .clicked()
                && let Some(path) = selected_cached
            {
                match self.synth_radar.to_operational_config() {
                    Ok(config) => self.launch_operational_radar(
                        crate::wrf_radar::OperationalRadarSource::Files(vec![path]),
                        config,
                    ),
                    Err(error) => self.import_message = Some(error),
                }
            }
            if ui
                .add_enabled_ui(!busy && self.operational_radar_source.is_some(), |ui| {
                    ui.add_sized([width, 25.0], egui::Button::new("Refresh operational frame(s)"))
                })
                .inner
                .on_hover_text(
                    "Rebuild local/cached files exactly, or re-resolve the selected latest-HRRR f00/f01 request, using the radar controls as tuned now.",
                )
                .clicked()
            {
                self.refresh_operational_radar();
            }
        });
        if ui
            .add_enabled_ui(!self.synthetic_export_frames.is_empty(), |ui| {
                ui.add_sized(
                    [ui.available_width().max(120.0), 24.0],
                    egui::Button::new("Export latest operational loop as CfRadial…"),
                )
            })
            .inner
            .on_hover_text(
                "Write the most recently built radar frame or loop through BowEcho's normal CfRadial-1 exporter.",
            )
            .clicked()
        {
            self.export_synthetic_radar_frames();
        }
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn refresh_operational_cache(&mut self) {
        let mut inputs = crate::operational_radar_grib::discover_cached_hrrr_inputs(
            &settings::simsat_input_dir(),
        );
        inputs.extend(crate::operational_radar_grib::discover_cached_hrrr_inputs(
            &settings::model_cache_dir(),
        ));
        let mut seen = std::collections::HashSet::new();
        inputs.retain(|input| seen.insert(input.path.clone()));
        inputs.sort_by(|left, right| right.label.cmp(&left.label));
        self.operational_cached_inputs = inputs;
        self.operational_cached_selected = self
            .operational_cached_selected
            .min(self.operational_cached_inputs.len().saturating_sub(1));
        self.import_message = Some(format!(
            "Found {} complete cached HRRR native file(s)",
            self.operational_cached_inputs.len()
        ));
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn synthetic_radar_recipe_ui(&mut self, ui: &mut egui::Ui, busy: bool) {
        let state = &mut self.synth_radar;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(
                egui::RichText::new("WHAT DO YOU WANT?")
                    .size(11.0)
                    .strong()
                    .color(crate::ui_theme::subhead_color()),
            );

            let current = state.active_recipe();
            let selected_text = current
                .map(SyntheticRadarRecipe::label)
                .unwrap_or("Custom tuning");
            let mut picked = None;
            ui.horizontal_wrapped(|ui| {
                ui.label("Preset:");
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("wrf_synth_radar_recipe")
                        .selected_text(selected_text)
                        .width(245.0)
                        .show_ui(ui, |ui| {
                            for recipe in SyntheticRadarRecipe::ALL {
                                if ui
                                    .selectable_label(current == Some(recipe), recipe.label())
                                    .on_hover_text(recipe.description())
                                    .clicked()
                                {
                                    picked = Some(recipe);
                                    ui.close();
                                }
                            }
                        });
                });
            });
            if let Some(recipe) = picked {
                state.apply_recipe(recipe);
            }

            ui.add_space(3.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Compute:");
                ui.add_enabled_ui(!busy, |ui| {
                    for preference in [
                        crate::wrf_radar::SyntheticRadarComputePreference::Auto,
                        crate::wrf_radar::SyntheticRadarComputePreference::Cpu,
                        crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda,
                    ] {
                        ui.selectable_value(
                            &mut state.compute_preference,
                            preference,
                            preference.label(),
                        )
                        .on_hover_text(match preference {
                            crate::wrf_radar::SyntheticRadarComputePreference::Auto => {
                                "Use a qualified NVIDIA CUDA device when available; otherwise use the CPU reference path."
                            }
                            crate::wrf_radar::SyntheticRadarComputePreference::Cpu => {
                                "Always use the portable CPU reference implementation."
                            }
                            crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda => {
                                "Prefer NVIDIA CUDA for supported native P3/ISHMAEL work; fall back to CPU if it is unavailable."
                            }
                        });
                    }
                });
            });
            let cuda = simradar_cuda::probe_cuda_cached();
            if let Some(device) = cuda.preferred_device() {
                ui.label(
                    egui::RichText::new(format!(
                        "CUDA ready: {} · compute {} · {:.1} GiB",
                        device.name,
                        device.compute_capability_label(),
                        device.total_memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    ))
                    .small()
                    .weak(),
                );
            } else if let Some(reason) = cuda.fallback_reason() {
                ui.label(
                    egui::RichText::new(format!("CUDA unavailable: {reason}; CPU fallback is ready"))
                        .small()
                        .weak(),
                );
            }
            let work = SyntheticRadarWorkEstimate::from_state(state);
            let mut work_text = work.summary();
            if state.match_gate_to_grid {
                work_text.push_str(" · fallback spacing shown; source DX sets the final gate count");
            }
            ui.label(egui::RichText::new(work_text).small().weak());
            if !state.polarimetric_kernel.is_property_tmatrix() {
                ui.label(
                    egui::RichText::new(
                        "CUDA applies to native P3/ISHMAEL property T-matrix processing; this preset uses CPU.",
                    )
                    .small()
                    .weak(),
                );
            }

            if let Some(recipe) = state.active_recipe() {
                ui.label(egui::RichText::new(recipe.description()).small().weak());
                ui.label(
                    egui::RichText::new(format!("Expected products: {}", recipe.products()))
                        .small(),
                );
                if matches!(
                    recipe,
                    SyntheticRadarRecipe::CleanDualPol
                        | SyntheticRadarRecipe::RealRadar
                        | SyntheticRadarRecipe::MaximumFidelity
                ) {
                    ui.label(
                        egui::RichText::new(
                            "Dual-pol uses supported raw bulk hydrometeors when present; unsupported inputs automatically fall back to REF/VEL with a status note.",
                        )
                        .small()
                        .weak(),
                    );
                } else if matches!(recipe, SyntheticRadarRecipe::PropertyTMatrixHybrid) {
                    ui.label(
                        egui::RichText::new(
                            "P3 Hybrid uses native property T-matrix where the exact tables and source state support the complete cell. Only explicit table-domain/shape omissions or the typed WRF 2 µm source-state mass gap use versioned bulk Rayleigh v1, with policy and cell/population counts stamped into the output.",
                        )
                        .small()
                        .weak(),
                    );
                } else if matches!(recipe, SyntheticRadarRecipe::PropertyTMatrixResearch) {
                    ui.label(
                        egui::RichText::new(
                            "Experimental Full P3 T-matrix requires exact P3 50-53 or ISHMAEL 55 property tuples and matching LUT axes. Any unsupported field, particle, temperature, beam, or table stops the run; it never substitutes Rayleigh.",
                        )
                        .small()
                        .weak(),
                    );
                }
            } else {
                ui.label(
                    egui::RichText::new(
                        "Custom tuning: one or more advanced controls differ from every safe preset.",
                    )
                    .small()
                    .weak(),
                );
            }
        });
    }

    /// Size-gate a wrf-rust severe/thermo import (park LARGE grids behind an
    /// explicit confirmation — the owner has melted this machine on a 250 m
    /// grid) or launch it directly. Shared by the file(s) and folder pickers so
    /// 1-to-hundreds of wrfouts take the identical safe path.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn gate_or_launch_heavy_import(&mut self, files: Vec<PathBuf>) {
        self.stage_formula_raw_from_files(&files);
        if self.formula_lab.busy() {
            self.import_message =
                Some("WRF processing cannot start while Formula Lab is evaluating".to_owned());
        } else if files.is_empty() {
            self.import_message = Some("No supported wrfout files selected".to_string());
        } else if let Some(warning) = heavy_import_size_warning(&files) {
            self.import_message = None;
            self.pending_heavy_import = Some(PendingHeavyImport { files, warning });
        } else {
            self.launch_heavy_import(files, self.wrf_options.to_options());
        }
    }

    /// Spawn the heavy full-diagnostics processing job (after any size gate).
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn launch_heavy_import(
        &mut self,
        files: Vec<PathBuf>,
        options: crate::wrf_process::WrfProcessOptions,
    ) {
        if self.formula_lab.busy() {
            self.import_message =
                Some("WRF processing cannot start while Formula Lab is evaluating".to_owned());
            return;
        }
        let count = files.len();
        let task = crate::wrf_process::spawn_process_paths(files, self.store_root.clone(), options);
        self.import_message = Some(format!("Processing {count} WRF file(s)…"));
        self.import_job = Some(ImportJob::Process(task));
    }

    /// Light-path counterpart of the heavy size gate: park a LARGE selection
    /// behind [`Self::light_import_warning_ui`], launch small ones directly.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn gate_or_launch_light_import(&mut self, files: Vec<PathBuf>) {
        self.stage_formula_raw_from_files(&files);
        if self.formula_lab.busy() {
            self.import_message =
                Some("Model import cannot start while Formula Lab is evaluating".to_owned());
        } else if let Some(warning) = light_import_size_warning(&files) {
            self.import_message = None;
            self.pending_light_import = Some(PendingLightImport { files, warning });
        } else {
            self.launch_light_import(files);
        }
    }

    /// Spawn the light import job (after any size gate).
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn launch_light_import(&mut self, files: Vec<PathBuf>) {
        if self.formula_lab.busy() {
            self.import_message =
                Some("Model import cannot start while Formula Lab is evaluating".to_owned());
            return;
        }
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

    /// Reconstruct the exact subset of `namelist.input` metadata retained by
    /// one wrfout, keeping inferred and unavailable values commented. Prefer
    /// the session's staged Formula Lab/raw-import source; only prompt for an
    /// input when that source is absent or no longer exists.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn extract_wrf_namelist_dialog(&mut self) {
        let retained = retained_namelist_source(self.formula_raw_path.as_deref());
        let retained_attempt = retained.as_ref().map(|source| {
            (
                source.clone(),
                crate::wrf_namelist::reconstruct_namelist_from_wrfout(source),
            )
        });
        let (source, reconstructed) = match retained_attempt {
            Some((source, Ok(reconstructed))) => (source, reconstructed),
            retained_failure => {
                let title = if retained_failure.is_some() {
                    "Retained source is not a readable wrfout; choose another raw WRF file"
                } else {
                    "Choose a raw wrfout for partial namelist reconstruction"
                };
                let Some(source) = rfd::FileDialog::new()
                    // Raw wrfout names are ordinarily extensionless.
                    .set_title(title)
                    .pick_file()
                else {
                    if let Some((_, Err(error))) = retained_failure {
                        self.import_message = Some(format!(
                            "Retained source could not reconstruct a namelist: {error}"
                        ));
                    }
                    return;
                };
                match crate::wrf_namelist::reconstruct_namelist_from_wrfout(&source) {
                    Ok(reconstructed) => (source, reconstructed),
                    Err(error) => {
                        self.import_message =
                            Some(format!("Namelist reconstruction failed: {error}"));
                        return;
                    }
                }
            }
        };
        // A newly selected, successfully decoded wrfout becomes the same
        // session-private raw source used by Formula Lab and later WRF tools.
        self.formula_raw_path = Some(source.clone());

        let Some(destination) = rfd::FileDialog::new()
            .set_title("Save partial reconstructed WRF namelist")
            .set_file_name("namelist.reconstructed.input")
            .save_file()
        else {
            return;
        };
        self.import_message = Some(
            match rw_store::atomic::atomic_write_bytes(&destination, reconstructed.as_bytes()) {
                Ok(()) => format!(
                    "Saved partial reconstructed namelist to {} — not the original and not sufficient to reproduce the run",
                    destination.display()
                ),
                Err(error) => format!(
                    "Could not save reconstructed namelist to {}: {error}",
                    destination.display()
                ),
            },
        );
    }

    /// Spawn the fast simulated-radar job over the picked file set.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn launch_synthetic_radar(
        &mut self,
        files: Vec<PathBuf>,
        config: crate::wrf_radar::SyntheticRadarConfig,
    ) {
        if self.import_job.is_some() {
            self.import_message =
                Some("Synthetic radar cannot start while another import is running".to_owned());
            return;
        }
        if self.formula_lab.busy() {
            self.import_message =
                Some("Synthetic radar cannot start while Formula Lab is evaluating".to_owned());
            return;
        }
        if files.is_empty() {
            self.import_message = Some("No WRF files selected for simulated radar".to_owned());
            return;
        }
        self.stage_formula_raw_from_files(&files);
        let count = files.len();
        self.synthetic_radar_source_files = files.clone();
        self.operational_radar_source = None;
        self.synthetic_radar_replay_source = None;
        self.synthetic_radar_preview = None;
        let task = crate::wrf_radar::spawn_synthetic_radar(files, config);
        self.import_message = Some(if count == 1 {
            "Simulating radar from 1 WRF file…".to_string()
        } else {
            format!("Simulating radar loop from {count} WRF files…")
        });
        self.import_job = Some(ImportJob::SyntheticRadar(task));
    }

    /// Spawn an exact observed-acquisition replay from one WRF source. The
    /// observed Arc and file snapshot are retained so Refresh remains the fast
    /// tuning loop users expect from ordinary synthetic radar.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn launch_synthetic_radar_replay(
        &mut self,
        file: PathBuf,
        config: crate::wrf_radar::SyntheticRadarConfig,
    ) {
        if self.import_job.is_some() {
            self.import_message =
                Some("Exact radar replay cannot start while another import is running".to_owned());
            return;
        }
        if self.formula_lab.busy() {
            self.import_message =
                Some("Exact radar replay cannot start while Formula Lab is evaluating".to_owned());
            return;
        }
        let Some(observed) = self.displayed_replay_source.clone() else {
            self.import_message = Some(
                "Load an eligible observed radar scan before starting exact replay".to_owned(),
            );
            return;
        };
        self.stage_formula_raw_from_files(std::slice::from_ref(&file));
        self.synthetic_radar_source_files = vec![file.clone()];
        self.operational_radar_source = None;
        self.synthetic_radar_replay_source = Some(observed.clone());
        let task = crate::wrf_radar::spawn_synthetic_radar_replay(vec![file], config, observed);
        self.import_message = Some(
            "Replaying WRF through the displayed scan's exact observed acquisition…".to_owned(),
        );
        self.import_job = Some(ImportJob::SyntheticRadarReplay(task));
    }

    /// Build a refresh request from the remembered source snapshot and the
    /// controls as they exist NOW. The old config is intentionally not cached:
    /// the entire point is fast experimentation with radar physics and
    /// presentation settings.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn synthetic_radar_refresh_request(
        &self,
    ) -> Result<(Vec<PathBuf>, crate::wrf_radar::SyntheticRadarConfig), String> {
        if self.import_job.is_some() {
            return Err(
                "Synthetic radar cannot refresh while another import is running".to_owned(),
            );
        }
        if self.formula_lab.busy() {
            return Err(
                "Synthetic radar cannot refresh while Formula Lab is evaluating".to_owned(),
            );
        }
        if self.synthetic_radar_source_files.is_empty() {
            return Err(
                "Build simulated radar from files or a folder before refreshing".to_owned(),
            );
        }
        Ok((
            self.synthetic_radar_source_files.clone(),
            self.synth_radar.to_config()?,
        ))
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn refresh_synthetic_radar(&mut self) {
        match self.synthetic_radar_refresh_request() {
            Ok((files, config)) => {
                if let Some(observed) = self.synthetic_radar_replay_source.clone() {
                    let Some(file) = files.into_iter().next() else {
                        self.import_message =
                            Some("Exact radar replay has no retained WRF source file".to_owned());
                        return;
                    };
                    self.displayed_replay_source = Some(observed);
                    self.launch_synthetic_radar_replay(file, config);
                } else {
                    self.launch_synthetic_radar(files, config);
                }
            }
            Err(message) => self.import_message = Some(message),
        }
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn launch_operational_radar(
        &mut self,
        source: crate::wrf_radar::OperationalRadarSource,
        config: crate::wrf_radar::SyntheticRadarConfig,
    ) {
        if self.import_job.is_some() {
            self.import_message = Some(
                "Operational forecast radar cannot start while another import is running"
                    .to_owned(),
            );
            return;
        }
        if self.formula_lab.busy() {
            self.import_message = Some(
                "Operational forecast radar cannot start while Formula Lab is evaluating"
                    .to_owned(),
            );
            return;
        }
        if matches!(&source, crate::wrf_radar::OperationalRadarSource::Files(paths) if paths.is_empty())
        {
            self.import_message = Some("No native HRRR/RRFS GRIB files selected".to_owned());
            return;
        }
        self.synthetic_radar_source_files.clear();
        self.synthetic_radar_replay_source = None;
        self.operational_radar_source = Some(source.clone());
        self.import_message = Some("Building operational forecast radar…".to_owned());
        self.import_job = Some(ImportJob::SyntheticRadar(
            crate::wrf_radar::spawn_operational_radar(source, config),
        ));
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn refresh_operational_radar(&mut self) {
        if self.import_job.is_some() || self.formula_lab.busy() {
            self.import_message = Some(
                "Operational forecast radar cannot refresh while another model task is running"
                    .to_owned(),
            );
            return;
        }
        let Some(source) = self.operational_radar_source.clone() else {
            self.import_message =
                Some("Build an operational forecast radar frame before refreshing".to_owned());
            return;
        };
        match self.synth_radar.to_operational_config() {
            Ok(config) => self.launch_operational_radar(source, config),
            Err(error) => self.import_message = Some(error),
        }
    }

    /// Export the retained simulated-radar frames as CfRadial-1 NetCDF files:
    /// one frame → save-file dialog; a loop → folder dialog, one file per
    /// frame named `{site}_{time}_simwrf.nc`. Writes synchronously on the UI
    /// thread — a frame is tens of MB and the writer streams through a
    /// BufWriter, so even a full loop is a brief, user-initiated pause.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn export_synthetic_radar_frames(&mut self) {
        // Arc clones only — frees `self` for the status-line writes below.
        let retained = self.synthetic_export_frames.clone();
        match retained.as_slice() {
            [] => {}
            [volume] => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("CfRadial NetCDF", &["nc"])
                    .set_file_name(crate::radar_export::export_file_name(volume))
                    .set_title("Export simulated radar volume (CfRadial)")
                    .save_file()
                {
                    self.import_message = Some(
                        match crate::radar_export::export_volume_cfradial(volume, &path) {
                            Ok(()) => {
                                format!("Exported simulated radar to {}", path.display())
                            }
                            Err(error) => format!("CfRadial export failed: {error}"),
                        },
                    );
                }
            }
            frames => {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title(format!(
                        "Choose a folder for {} CfRadial frame files",
                        frames.len()
                    ))
                    .pick_folder()
                {
                    self.import_message = Some(match crate::radar_export::export_volumes_cfradial(
                        frames, &dir,
                    ) {
                        Ok(count) => format!("Wrote {count} CfRadial file(s) to {}", dir.display()),
                        Err(error) => format!("CfRadial export failed: {error}"),
                    });
                }
            }
        }
    }

    /// Inline size-aware confirmation for a parked heavy import: the warning,
    /// a fast core-only alternative, an explicit "start anyway", and cancel.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn heavy_import_warning_ui(&mut self, ui: &mut egui::Ui) {
        let Some(pending) = &self.pending_heavy_import else {
            return;
        };
        let warning = pending.warning.clone();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(
                egui::RichText::new("Large WRF import")
                    .strong()
                    .color(crate::ui_theme::theme().warn),
            );
            ui.label(egui::RichText::new(warning).small());
            ui.label(
                egui::RichText::new(
                    "Narrow Fields to compute, start core-only, or use Simulated radar when \
                     you only need a fast radar-style view of the storm.",
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
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn light_import_warning_ui(&mut self, ui: &mut egui::Ui) {
        let Some(pending) = &self.pending_light_import else {
            return;
        };
        let warning = pending.warning.clone();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(
                egui::RichText::new("Large WRF import")
                    .strong()
                    .color(crate::ui_theme::theme().warn),
            );
            ui.label(egui::RichText::new(warning).small());
            ui.label(
                egui::RichText::new(
                    "For radar-style browsing, use Simulated radar instead: it takes seconds \
                     per file and loops in the radar view. Quick import is for model-store \
                     fields and skew-T soundings.",
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
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn synthetic_radar_site_panel(&mut self, ui: &mut egui::Ui) {
        let busy = self.import_job.is_some() || self.formula_lab.busy();
        let state = &mut self.synth_radar;
        egui::CollapsingHeader::new("Radar location & fine tuning (advanced)")
            .id_salt("wrf_synth_radar_site")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Where the simulated antenna stands and how far it scans. Applies \
                         to the next Simulated radar run. The antenna sits \
                         on the model terrain at the chosen spot (+10 m tower).",
                    )
                    .small()
                    .weak(),
                );
                ui.add_enabled_ui(!busy, |ui| {
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Technical mode").strong());
                        for (mode, label, help) in [
                            (
                                crate::wrf_radar::SimulationMode::Truth,
                                "Truth",
                                "Model truth: one model instant, center sampling, no virtual-instrument effects.",
                            ),
                            (
                                crate::wrf_radar::SimulationMode::Instrument,
                                "Instrument",
                                "Virtual S-band radar: beam integration, scatterer fall speed, blockage, dual-pol, propagation, scan timing, and sensitivity.",
                            ),
                            (
                                crate::wrf_radar::SimulationMode::Presentation,
                                "Presentation",
                                "Display-oriented output: linear-Z center sampling with reflectivity texture and no measurement effects.",
                            ),
                        ] {
                            if ui
                                .selectable_label(state.simulation_mode == mode, label)
                                .on_hover_text(format!(
                                    "{help} Clicking applies this preset once; controls remain independently editable afterward."
                                ))
                                .clicked()
                            {
                                state.apply_mode_preset(mode);
                            }
                        }
                    });
                    let mode_summary = match state.simulation_mode {
                        crate::wrf_radar::SimulationMode::Truth => {
                            "Truth preset active · direct model scene"
                        }
                        crate::wrf_radar::SimulationMode::Instrument => {
                            "Instrument preset active · virtual S-band measurement"
                        }
                        crate::wrf_radar::SimulationMode::Presentation => {
                            "Presentation preset active · display-oriented gates"
                        }
                    };
                    ui.label(egui::RichText::new(mode_summary).small().weak());
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Scan pattern").strong());
                        egui::ComboBox::from_id_salt("wrf_synth_scan_strategy")
                            .selected_text(state.scan_strategy.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut state.scan_strategy,
                                    crate::wrf_radar::SyntheticScanStrategy::CustomLegacy,
                                    "Custom legacy ladder",
                                );
                                ui.separator();
                                for strategy in
                                    crate::wrf_radar::SyntheticScanStrategy::BUILD_24
                                {
                                    ui.selectable_value(
                                        &mut state.scan_strategy,
                                        strategy,
                                        strategy.label(),
                                    );
                                }
                            });
                    });
                    if let Some(definition) = state.scan_strategy.definition() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} rev {} · RDA Build {} · {} physical rows · ~{:.2} min base cadence",
                                definition.source.document_number,
                                definition.source.revision,
                                definition.source.rda_build,
                                definition.rows.len(),
                                definition.nominal_cadence.minutes(),
                            ))
                            .small()
                            .weak(),
                        );
                        ui.label(
                            egui::RichText::new(
                                crate::wrf_radar::BUILD_24_NO_ADAPTATIONS_CAVEAT,
                            )
                            .small()
                            .color(crate::ui_theme::theme().warn),
                        );
                        ui.label(
                            egui::RichText::new(
                                "Catalog PRF values remain source-table codes/pulse counts; no Hz or standard PRT is inferred.",
                            )
                            .small()
                            .weak(),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new(
                                "Legacy custom elevation ladder · custom rate, transition, PRF, and optional 0.1° tilt.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                    ui.separator();
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
                                                    "Valid site: {}{coords}",
                                                    record.label
                                                ))
                                                .small()
                                                .weak(),
                                            );
                                        }
                                        None => {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "Site “{id}” was not found"
                                                ))
                                                .small()
                                                .color(crate::ui_theme::theme().warn),
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
                        // Grid-matching (below) resolves the gate size from each
                        // file's DX, so the range/gate controls are overridden and
                        // shown disabled while it is on.
                        let gate_controls_enabled = !state.match_gate_to_grid;
                        ui.add_enabled(
                            gate_controls_enabled,
                            egui::Checkbox::new(&mut state.auto_gate_spacing, "Auto gates"),
                        )
                        .on_hover_text(
                            "Coarsen gate spacing proportionally with range (keeps the \
                             classic 920-gate count), so a wide circle costs the same \
                             memory as the 230 km default. Overridden while “Match gate \
                             size to grid resolution” is on.",
                        );
                        if !state.auto_gate_spacing {
                            ui.add_enabled(
                                gate_controls_enabled,
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
                    ui.checkbox(
                        &mut state.match_gate_to_grid,
                        "Match gate size to grid resolution",
                    )
                    .on_hover_text(
                        "Set the gate spacing equal to the WRF grid resolution (the file's \
                         DX) so a coarse grid is not oversampled — 250 m gates on a 3 km grid \
                         imply ~12 redundant gates per model cell, more detail than the model \
                         has. The gate size is read from each file at import (clamped 100 m–10 \
                         km) and overrides the range/gate controls above; a file with no DX \
                         attribute falls back to them.",
                    );
                    if state.match_gate_to_grid {
                        ui.label(
                            egui::RichText::new(format!(
                                "{:.0} km range · gate size = grid resolution (file DX, \
                                 100 m–10 km); {:.0} m used if a file has no DX",
                                state.clamped_range_km(),
                                state.effective_gate_spacing_m(),
                            ))
                            .small()
                            .weak(),
                        );
                    } else {
                        let spacing = state.effective_gate_spacing_m();
                        let gates = (state.clamped_range_km() * 1000.0 / spacing).floor();
                        ui.label(
                            egui::RichText::new(format!(
                                "{:.0} km range at {spacing:.0} m gates -> {gates:.0} gates/radial",
                                state.clamped_range_km()
                            ))
                            .small()
                            .weak(),
                        );
                    }
                    egui::CollapsingHeader::new("Presentation & velocity")
                        .id_salt("wrf_synth_radar_presentation")
                        .default_open(false)
                        .show(ui, |ui| {
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
                        ui.add_enabled(
                            !state.coupled_single_prf_estimator,
                            egui::Checkbox::new(
                                &mut state.fold_velocity,
                                "Realistic Nyquist (velocity folds)",
                            ),
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
                            state.fold_velocity && !state.coupled_single_prf_estimator,
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
                    if state.coupled_single_prf_estimator {
                        ui.label(
                            egui::RichText::new(
                                "Velocity ambiguity is derived from exact frequency and PRF by the coupled estimator; the manual Nyquist control is inactive.",
                            )
                            .small()
                            .weak(),
                        );
                    }
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
                        });

                    egui::CollapsingHeader::new("Physics & moments")
                        .id_salt("wrf_synth_radar_physics")
                        .default_open(true)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Reflectivity sampling:");
                                ui.selectable_value(
                                    &mut state.reflectivity_sampling,
                                    crate::wrf_radar::ReflectivitySampling::LinearZ,
                                    "Linear Z",
                                )
                                .on_hover_text(
                                    "Average received power in linear reflectivity before converting to dBZ. This is the scientific default.",
                                );
                                ui.selectable_value(
                                    &mut state.reflectivity_sampling,
                                    crate::wrf_radar::ReflectivitySampling::LegacyDbz,
                                    "Legacy dBZ",
                                )
                                .on_hover_text(
                                    "Directly interpolate dBZ to reproduce older BowEcho simulated-radar renders.",
                                );
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Pulse volume:");
                                ui.selectable_value(
                                    &mut state.beam_integration,
                                    crate::wrf_radar::BeamIntegration::Center,
                                    "Center",
                                )
                                .on_hover_text("One center sample per gate; fastest.");
                                ui.selectable_value(
                                    &mut state.beam_integration,
                                    crate::wrf_radar::BeamIntegration::Balanced,
                                    "Balanced (9)",
                                )
                                .on_hover_text(
                                    "Nine deterministic Gaussian-weighted samples across the beam and pulse.",
                                );
                                ui.selectable_value(
                                    &mut state.beam_integration,
                                    crate::wrf_radar::BeamIntegration::Reference,
                                    "Reference (27)",
                                )
                                .on_hover_text(
                                    "Full 3 x 3 x 3 deterministic quadrature; highest fidelity and slowest.",
                                );
                            });
                            ui.checkbox(
                                &mut state.terminal_fall_speed,
                                "Scatterer-weighted Doppler + terminal fall speed",
                            )
                            .on_hover_text(
                                "Weight radial velocity by returned power and include hydrometeor terminal fall speed when raw model species are available.",
                            );
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut state.spectrum_width, "Spectrum width (SW)")
                                    .on_hover_text(
                                        "Emit SW from pulse-volume velocity variance, model TKE/fall-speed diversity when available, and the floor at right.",
                                    );
                                ui.add_enabled(
                                    state.spectrum_width,
                                    egui::DragValue::new(
                                        &mut state.spectrum_width_floor_mps,
                                    )
                                    .range(0.0..=10.0)
                                    .speed(0.1)
                                    .suffix(" m/s floor"),
                                );
                            });
                            ui.checkbox(&mut state.dual_pol, "Dual polarization")
                                .on_hover_text(
                                    "Derive ZH, ZDR, rhoHV, KDP, PhiDP and attenuation from raw WRF bulk hydrometeors when the microphysics scheme is supported.",
                                );
                            ui.add_enabled_ui(state.dual_pol, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Scattering:");
                                    ui.selectable_value(
                                        &mut state.polarimetric_kernel,
                                        crate::wrf_radar::PolarimetricKernel::BulkRayleighV1,
                                        "Bulk Rayleigh",
                                    )
                                    .on_hover_text(
                                        "Fast scheme-aware bulk approximation used by BowEcho v0.33.0. Unsupported schemes are explicitly labeled scalar fallbacks.",
                                    );
                                    let hybrid_clicked = ui
                                        .selectable_value(
                                            &mut state.polarimetric_kernel,
                                            crate::wrf_radar::PolarimetricKernel::PropertyTMatrixHybridV1,
                                            "P3 Hybrid (recommended)",
                                        )
                                        .on_hover_text(
                                            "Native property T-matrix for supported P3/ISHMAEL cells; explicit versioned bulk Rayleigh v1 only for audited table-domain/shape omissions or the typed WRF 2 µm source-state mass gap.",
                                        )
                                        .clicked();
                                    let strict_clicked = ui
                                        .selectable_value(
                                            &mut state.polarimetric_kernel,
                                            crate::wrf_radar::PolarimetricKernel::PropertyTMatrixResearchV1,
                                            "Full P3 T-matrix (experimental)",
                                        )
                                        .on_hover_text(
                                            "Experimental full property operator for exact supported P3/ISHMAEL tuples. It fails closed on every scheme, field, particle, frequency, geometry, or table mismatch.",
                                        )
                                        .clicked();
                                    if hybrid_clicked || strict_clicked {
                                        // Keep the coherent legacy recipe at 2.8 GHz while
                                        // preserving an explicit external exact-band choice.
                                        if matches!(
                                            state.property_tmatrix_table_source,
                                            app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1
                                        ) {
                                            state.radar_frequency_mhz = crate::wrf_radar::PROPERTY_TMATRIX_RESEARCH_FREQUENCY_MHZ;
                                        }
                                        state.reflectivity_sampling =
                                            crate::wrf_radar::ReflectivitySampling::LinearZ;
                                        if hybrid_clicked
                                            && matches!(
                                                state.atmosphere_time_mode,
                                                app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear
                                            )
                                        {
                                            state.atmosphere_time_mode =
                                                app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart;
                                        }
                                    }
                                });
                            });
                            let kernel_note = match state.polarimetric_kernel {
                                crate::wrf_radar::PolarimetricKernel::BulkRayleighV1 => {
                                    "Scheme-aware bulk S-band Rayleigh operator. Unsupported or incomplete conventional schemes fall back explicitly to REF/VEL."
                                }
                                crate::wrf_radar::PolarimetricKernel::PropertyTMatrixHybridV1 => {
                                    "Recommended P3/ISHMAEL operator. Native property T-matrix is retained for supported cells; only table-domain/shape omissions or the typed WRF 2 µm source-state mass gap use versioned bulk Rayleigh v1, with explicit policy and audit counts."
                                }
                                crate::wrf_radar::PolarimetricKernel::PropertyTMatrixResearchV1 => {
                                    "Experimental full property T-matrix, not independently validated for operational use. Requires an exact supported P3/ISHMAEL contract and exact S/C/X table pack; strict fail-closed with no Rayleigh fallback."
                                }
                            };
                            ui.label(egui::RichText::new(kernel_note).small().weak());
                            if state.polarimetric_kernel.is_property_tmatrix() {
                                ui.separator();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Table source:");
                                    if ui
                                        .selectable_value(
                                            &mut state.property_tmatrix_table_source,
                                            app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
                                            "Legacy embedded S",
                                        )
                                        .on_hover_text(
                                            "The shipped research-v1 tables. They exist only at exactly 2.8 GHz.",
                                        )
                                        .clicked()
                                    {
                                        state.radar_frequency_mhz = 2_800;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut state.property_tmatrix_table_source,
                                            app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::ExternalValidatedPack,
                                            "Validated local pack",
                                        )
                                        .on_hover_text(
                                            "Load one manifest-validated five-table pack at the exact selected frequency. Missing, invalid, unvalidated, or ambiguous packs fail closed.",
                                        )
                                        .clicked()
                                        && matches!(
                                            state.atmosphere_time_mode,
                                            app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear
                                        )
                                    {
                                        state.atmosphere_time_mode =
                                            app_ui::wrf_temporal::AtmosphereTimeMode::LinearAdjacent;
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Exact band:");
                                    for (frequency, label) in [
                                        (2_800, "S · 2.8 GHz"),
                                        (5_600, "C · 5.6 GHz"),
                                        (9_400, "X · 9.4 GHz"),
                                    ] {
                                        let enabled = frequency == 2_800
                                            || matches!(
                                                state.property_tmatrix_table_source,
                                                app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::ExternalValidatedPack
                                            );
                                        ui.add_enabled_ui(enabled, |ui| {
                                            ui.selectable_value(
                                                &mut state.radar_frequency_mhz,
                                                frequency,
                                                label,
                                            );
                                        });
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Rain / melting:");
                                    ui.selectable_value(
                                        &mut state.property_tmatrix_rain_sensitivity,
                                        crate::wrf_radar::PropertyTMatrixRainSensitivity::FullProperty,
                                        "Full property",
                                    )
                                    .on_hover_text(
                                        "Include standalone rain and qualified wet frozen/rain coexistence.",
                                    );
                                    if ui
                                        .selectable_value(
                                            &mut state.property_tmatrix_rain_sensitivity,
                                            crate::wrf_radar::PropertyTMatrixRainSensitivity::FrozenOnly,
                                            "Frozen-only",
                                        )
                                        .on_hover_text(
                                            "Deliberately omit rain and melting coexistence so only dry frozen categories contribute.",
                                        )
                                        .clicked()
                                        && matches!(
                                            state.atmosphere_time_mode,
                                            app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear
                                        )
                                    {
                                        state.atmosphere_time_mode =
                                            app_ui::wrf_temporal::AtmosphereTimeMode::LinearAdjacent;
                                    }
                                });
                                if matches!(
                                    state.property_tmatrix_table_source,
                                    app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::ExternalValidatedPack
                                ) {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Validated packs: {}",
                                            app_ui::wrf_tmatrix_assets::property_tmatrix_pack_cache_dir().display()
                                        ))
                                        .small()
                                        .monospace(),
                                    )
                                    .on_hover_text(
                                        "Place the exact pack directory and manifest here. The run error reports this same deterministic location and the failed validation gate.",
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(if state.polarimetric_kernel.is_hybrid() {
                                        "Hybrid is cell-audited and currently uses Frozen or additive adjacent-scene timing; Raw-state pre-closure is disabled."
                                    } else {
                                        "Raw-state pre-closure is limited to experimental Full P3/ISHMAEL, legacy embedded S · 2.8 GHz · Full property. External S/C/X uses Frozen or additive adjacent-scene timing."
                                    })
                                    .small()
                                    .weak(),
                                );
                            }
                            ui.separator();
                            ui.checkbox(
                                &mut state.emit_quality_fields,
                                "Gate support fields (MCOV / TUNB / MSIG)",
                            )
                            .on_hover_text(
                                "Emit compact model-coverage, terrain-unblocked, and meteorological-signal fractions so masked or weak gates remain auditable.",
                            );
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Minimum model coverage:");
                                ui.add(
                                    egui::Slider::new(
                                        &mut state.minimum_model_coverage_fraction,
                                        0.0..=1.0,
                                    )
                                    .show_value(false),
                                );
                                ui.label(format!(
                                    "{:.0}%",
                                    state.minimum_model_coverage_fraction.clamp(0.0, 1.0)
                                        * 100.0
                                ));
                            });
                            ui.label(
                                egui::RichText::new(
                                    "Physical moments are masked when less than this fraction of the configured pulse volume is inside the model. 0% preserves every historically accepted gate; quality fields, when enabled, still explain coverage/blockage/signal.",
                                )
                                .small()
                                .weak(),
                            );
                        });

                    egui::CollapsingHeader::new("Instrument & propagation")
                        .id_salt("wrf_synth_radar_instrument")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Frequency:");
                                let research_tmatrix =
                                    state.polarimetric_kernel.is_property_tmatrix();
                                ui.add_enabled(
                                    !research_tmatrix,
                                    egui::DragValue::new(&mut state.radar_frequency_mhz)
                                        .range(2_000..=4_000)
                                        .speed(10)
                                        .suffix(" MHz"),
                                )
                                .on_hover_text(
                                    if research_tmatrix {
                                        "Property T-matrix frequency is owned by the exact S/C/X table selection under Microphysics & pulse volume."
                                    } else {
                                        "Transmit frequency written to the volume and CfRadial provenance."
                                    },
                                );
                                ui.add(
                                    egui::DragValue::new(&mut state.beam_width_deg)
                                        .range(0.1..=5.0)
                                        .speed(0.05)
                                        .suffix(" deg beam"),
                                );
                                ui.add(
                                    egui::DragValue::new(&mut state.pulse_width_us)
                                        .range(0.1..=10.0)
                                        .speed(0.05)
                                        .suffix(" us pulse"),
                                );
                                ui.add_enabled(
                                    !state.scan_strategy.is_named_vcp(),
                                    egui::DragValue::new(&mut state.prf_hz)
                                        .range(100.0..=5_000.0)
                                        .speed(25.0)
                                        .suffix(" Hz PRF"),
                                );
                            });
                            let named_vcp = state.scan_strategy.is_named_vcp();
                            if named_vcp {
                                // Build-24 catalog values are PRF identifiers,
                                // not authoritative frequencies. Never retain a
                                // stale custom-frequency estimator across a
                                // switch to a named VCP.
                                state.coupled_single_prf_estimator = false;
                            }
                            ui.add_enabled(
                                !named_vcp,
                                egui::Checkbox::new(
                                    &mut state.coupled_single_prf_estimator,
                                    "Physically coupled single-PRF moment estimator",
                                ),
                            )
                            .on_hover_text(
                                "Use exact frequency, PRF, pulse width, dwell and pulse count as one instrument contract. Nyquist, unambiguous range, matched-filter pulse weighting, SNR uncertainty and velocity folding are then derived rather than independently dialed.",
                            );
                            if state.coupled_single_prf_estimator {
                                let frequency_hz =
                                    f64::from(state.radar_frequency_mhz) * 1.0e6;
                                let prf_hz = f64::from(state.prf_hz);
                                if frequency_hz.is_finite()
                                    && frequency_hz > 0.0
                                    && prf_hz.is_finite()
                                    && prf_hz > 0.0
                                {
                                    let wavelength_m = 299_792_458.0 / frequency_hz;
                                    let nyquist_mps = wavelength_m * prf_hz / 4.0;
                                    let unambiguous_range_km =
                                        299_792_458.0 / (2.0 * prf_hz) / 1_000.0;
                                    let pulse_resolution_m =
                                        299_792_458.0 * f64::from(state.pulse_width_us) * 1.0e-6
                                            / 2.0;
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Derived: λ {:.2} cm · Nyquist ±{nyquist_mps:.1} m/s · unambiguous range {unambiguous_range_km:.1} km · pulse resolution {pulse_resolution_m:.0} m",
                                            wavelength_m * 100.0,
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                }
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Estimator sampling:");
                                    ui.add(
                                        egui::DragValue::new(&mut state.estimator_dwell_ms)
                                            .range(0.1..=10_000.0)
                                            .speed(1.0)
                                            .suffix(" ms dwell"),
                                    );
                                    let mut derive_pulses =
                                        state.estimator_pulse_count.is_none();
                                    if ui
                                        .checkbox(&mut derive_pulses, "derive pulse count")
                                        .changed()
                                    {
                                        state.estimator_pulse_count = if derive_pulses {
                                            None
                                        } else {
                                            Some(64)
                                        };
                                    }
                                    if let Some(pulses) = state.estimator_pulse_count.as_mut() {
                                        ui.add(
                                            egui::DragValue::new(pulses)
                                                .range(1..=100_000)
                                                .speed(1.0)
                                                .suffix(" pulses"),
                                        );
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Independent samples:");
                                    ui.add(
                                        egui::Slider::new(
                                            &mut state.estimator_independent_sample_fraction,
                                            0.01..=1.0,
                                        )
                                        .custom_formatter(|value, _| {
                                            format!("{:.0}%", value * 100.0)
                                        }),
                                    );
                                    ui.add(
                                        egui::DragValue::new(
                                            &mut state.estimator_minimum_snr_db,
                                        )
                                        .range(-20.0..=30.0)
                                        .speed(0.25)
                                        .suffix(" dB min SNR"),
                                    );
                                });
                                ui.checkbox(
                                    &mut state.emit_stage_diagnostics,
                                    "Emit Ideal + Measured diagnostic moments",
                                )
                                .on_hover_text(
                                    "Keep canonical products as Presented moments and add opt-in I*/M* grids so Ideal, Measured and Presented values can be compared gate by gate. This increases output memory.",
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "Ideal = pulse-volume atmosphere; Measured = receiver/PRF/dwell/SNR effects; Presented = optional display texture and stylized clutter.",
                                    )
                                    .small()
                                    .weak(),
                                );
                            }
                            ui.checkbox(
                                &mut state.terrain_blockage,
                                "Terrain horizon + partial beam blockage",
                            )
                            .on_hover_text(
                                "Apply cumulative terrain-horizon blockage along each radial, including partial beam occultation.",
                            );
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Scan timing:");
                                if ui
                                    .selectable_label(
                                        matches!(
                                            state.scan_timing,
                                            crate::wrf_radar::ScanTiming::InstantaneousTruth
                                        ),
                                        "Instantaneous",
                                    )
                                    .clicked()
                                {
                                    state.scan_timing =
                                        crate::wrf_radar::ScanTiming::InstantaneousTruth;
                                    state.atmosphere_time_mode = app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart;
                                }
                                ui.selectable_value(
                                    &mut state.scan_timing,
                                    crate::wrf_radar::ScanTiming::TimedVolume,
                                    "Timed volume",
                                )
                                .on_hover_text(
                                    "Assign per-ray acquisition offsets. Atmosphere time below controls whether those rays hold one WRF scene or interpolate adjacent scenes.",
                                );
                                let custom_timed = matches!(
                                    state.scan_timing,
                                    crate::wrf_radar::ScanTiming::TimedVolume
                                ) && !state.scan_strategy.is_named_vcp();
                                ui.add_enabled(
                                    custom_timed,
                                    egui::DragValue::new(&mut state.rotation_rate_deg_s)
                                        .range(1.0..=60.0)
                                        .speed(0.5)
                                        .suffix(" deg/s"),
                                );
                                ui.add_enabled(
                                    custom_timed,
                                    egui::DragValue::new(&mut state.transition_delay_s)
                                        .range(0.0..=30.0)
                                        .speed(0.25)
                                    .suffix(" s transition"),
                                );
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Atmosphere time:");
                                ui.selectable_value(
                                    &mut state.atmosphere_time_mode,
                                    app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart,
                                    "Frozen",
                                )
                                .on_hover_text(
                                    "Every ray samples the WRF scene at volume start. This is the backward-compatible mode and is valid for instantaneous or timed scans.",
                                );
                                if ui
                                    .selectable_label(
                                        matches!(
                                            state.atmosphere_time_mode,
                                            app_ui::wrf_temporal::AtmosphereTimeMode::LinearAdjacent
                                        ),
                                        "Interpolate adjacent WRF scenes",
                                    )
                                    .on_hover_text(
                                        "Use each timed ray's acquisition time to interpolate compatible adjacent WRF scenes in linear received-power, wind, and additive-scattering space without extrapolation. Selecting this also enables Timed volume.",
                                    )
                                    .clicked()
                                {
                                    state.atmosphere_time_mode =
                                        app_ui::wrf_temporal::AtmosphereTimeMode::LinearAdjacent;
                                    state.scan_timing = crate::wrf_radar::ScanTiming::TimedVolume;
                                }
                                let raw_state_available = matches!(
                                    state.polarimetric_kernel,
                                    crate::wrf_radar::PolarimetricKernel::PropertyTMatrixResearchV1
                                ) && matches!(
                                    state.property_tmatrix_table_source,
                                    app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1
                                ) && state.radar_frequency_mhz == 2_800
                                    && matches!(
                                        state.property_tmatrix_rain_sensitivity,
                                        crate::wrf_radar::PropertyTMatrixRainSensitivity::FullProperty
                                    );
                                ui.add_enabled_ui(
                                    raw_state_available,
                                    |ui| {
                                        if ui
                                            .selectable_label(
                                                matches!(
                                                    state.atmosphere_time_mode,
                                                    app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear
                                                ),
                                                "Raw-state pre-closure",
                                            )
                                            .on_hover_text(
                                                "P3/ISHMAEL property T-matrix only. Blend each scene's real trilinear raw-state stencil and the timed-ray alpha before one nonlinear closure/scattering evaluation.",
                                            )
                                            .clicked()
                                        {
                                            state.atmosphere_time_mode =
                                                app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear;
                                            state.scan_timing =
                                                crate::wrf_radar::ScanTiming::TimedVolume;
                                        }
                                    },
                                );
                            });
                            if state.atmosphere_time_mode.uses_adjacent_scene() {
                                ui.label(
                                    egui::RichText::new(
                                        match state.atmosphere_time_mode {
                                            app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear => "Timed rays blend raw winds, thermodynamics, and native P3/ISHMAEL state across the full spatial stencil and adjacent model times before one closure/T-matrix evaluation. No representative-cell or additive-time shortcut is used.",
                                            _ => "Timed rays interpolate linear Z, winds, and additive polar scattering quantities; ratios such as ZDR and rhoHV are derived afterward. The renderer never extrapolates beyond the next compatible WRF scene.",
                                        },
                                    )
                                    .small()
                                    .weak(),
                                );
                                if matches!(
                                    state.atmosphere_time_mode,
                                    app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear
                                ) {
                                    let workload_warning = match state.compute_preference {
                                        crate::wrf_radar::SyntheticRadarComputePreference::Cpu => {
                                            "⚠ Heavy CPU computation: raw-state P3/ISHMAEL reconstructs and integrates the native PSD at every sampled gate and can use all available CPU cores. Tilt 1 appears as a preview as soon as it completes while the remaining tilts continue; the first tilt can still take several minutes on dense, multi-sample volumes."
                                        }
                                        crate::wrf_radar::SyntheticRadarComputePreference::Auto
                                        | crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda => {
                                            "⚠ Heavy computation: CUDA accelerates supported native dry P3/ISHMAEL LUT work; the CPU still owns reconstruction, ordered PSD reduction, and fallback. Tilt 1 appears as a preview as soon as it completes while the remaining tilts continue; the first tilt can still take several minutes on dense, multi-sample volumes."
                                        }
                                    };
                                    ui.label(
                                        egui::RichText::new(workload_warning)
                                        .small()
                                        .color(crate::ui_theme::theme().warn),
                                    );
                                }
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Final-frame policy:");
                                    ui.selectable_value(
                                        &mut state.missing_neighbor_policy,
                                        app_ui::wrf_temporal::MissingNeighborPolicy::HoldAnchor,
                                        "Hold last",
                                    )
                                    .on_hover_text(
                                        "Render the final frame from its anchor scene and record that temporal sampling was held.",
                                    );
                                    ui.selectable_value(
                                        &mut state.missing_neighbor_policy,
                                        app_ui::wrf_temporal::MissingNeighborPolicy::DropFrame,
                                        "Drop",
                                    )
                                    .on_hover_text(
                                        "Omit a frame that has no complete compatible later scene.",
                                    );
                                    ui.selectable_value(
                                        &mut state.missing_neighbor_policy,
                                        app_ui::wrf_temporal::MissingNeighborPolicy::Error,
                                        "Error",
                                    )
                                    .on_hover_text(
                                        "Stop the run instead of falling back when a complete temporal bracket is unavailable.",
                                    );
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Temporal build RAM cap:");
                                    let mut budget_gib = synth_temporal_budget_gib(
                                        state.temporal_memory_budget_mib,
                                    );
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut budget_gib)
                                                .range(1.0..=64.0)
                                                .speed(0.25)
                                                .fixed_decimals(2)
                                                .suffix(" GiB"),
                                        )
                                        .on_hover_text(
                                            "Safety cap for this WRF synthetic-radar build: input scenes, read/cut scratch, and every retained output frame. It is saved across restarts and is separate from the radar playback-loop RAM budget.",
                                        )
                                        .changed()
                                    {
                                        state.temporal_memory_budget_mib =
                                            synth_temporal_budget_mib_from_gib(budget_gib);
                                        state.temporal_memory_budget_user_set = true;
                                    }
                                });
                                ui.label(
                                    egui::RichText::new(
                                        "Saved WRF radar setting; separate from playback-loop RAM.",
                                    )
                                    .small()
                                    .weak(),
                                );
                            }
                            if state.scan_strategy.is_named_vcp() {
                                ui.label(
                                    egui::RichText::new(
                                        "Named VCP owns each physical row's azimuth rate and period; custom transition and PRF-Hz controls are disabled.",
                                    )
                                    .small()
                                    .weak(),
                                );
                            }
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Beam geometry:");
                                ui.selectable_value(
                                    &mut state.propagation_geometry,
                                    crate::wrf_radar::PropagationGeometry::StandardFourThirdsEarth,
                                    "Standard 4/3 Earth",
                                )
                                .on_hover_text(
                                    "Operational default. Uses the conventional effective-Earth-radius beam path.",
                                );
                                ui.selectable_value(
                                    &mut state.propagation_geometry,
                                    crate::wrf_radar::PropagationGeometry::WrfRefractivityResearch,
                                    "WRF refractivity (research)",
                                )
                                .on_hover_text(
                                    "Read P/PB/T/QVAPOR at the actual radar site and ray-trace every gate through the model refractivity profile. Invalid or uncovered profiles stop the build.",
                                );
                            });
                            if matches!(
                                state.propagation_geometry,
                                crate::wrf_radar::PropagationGeometry::WrfRefractivityResearch
                            ) {
                                ui.label(
                                    egui::RichText::new(
                                        "⚠ Research geometry: anomalous propagation and ducting are physical profile outcomes. BowEcho reports the gradient/regime and shows an explicit ducting warning; operational HRRR/RRFS stays on standard 4/3 Earth.",
                                    )
                                    .small()
                                    .color(egui::Color32::YELLOW),
                                );
                            }
                            ui.add_enabled_ui(state.dual_pol, |ui| {
                                ui.checkbox(
                                    &mut state.propagation,
                                    "Radial propagation (PhiDP + differential attenuation)",
                                );
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("Calibration:");
                                    ui.add(
                                        egui::DragValue::new(&mut state.system_phidp_deg)
                                            .range(-180.0..=180.0)
                                            .speed(0.25)
                                            .suffix(" deg PhiDP"),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut state.zdr_bias_db)
                                            .range(-5.0..=5.0)
                                            .speed(0.05)
                                            .suffix(" dB ZDR"),
                                    );
                                });
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.checkbox(
                                    &mut state.instrument_noise,
                                    "Range-dependent sensitivity",
                                );
                                ui.add_enabled(
                                    state.instrument_noise,
                                    egui::DragValue::new(
                                        &mut state.sensitivity_dbz_at_1km,
                                    )
                                    .range(-80.0..=20.0)
                                    .speed(0.5)
                                    .suffix(" dBZ at 1 km"),
                                );
                            });
                        });

                    ui.add_enabled(
                        !state.scan_strategy.is_named_vcp(),
                        egui::Checkbox::new(
                            &mut state.include_low_tilt,
                            "Include 0.1° low tilt (community lowest tilt)",
                        ),
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
                            "Restore Presentation mode: domain centre, 230 km / 250 m gates, \
                             linear-Z center sampling, model-native reflectivity, textured REF, \
                             clean unfolded velocity, and no dual-pol, propagation, blockage, \
                             scan timing, instrument noise, clutter, extra low tilt, or named VCP.",
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
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn wrf_options_panel(&mut self, ui: &mut egui::Ui) {
        let busy = self.import_job.is_some() || self.formula_lab.busy();
        let opts = &mut self.wrf_options;
        egui::CollapsingHeader::new("Fields to compute")
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
                        .on_hover_text(
                            "Everything but heavy eCAPE — the classic import. Does not change the separate Models > Automatically plot setting.",
                        )
                        .clicked()
                    {
                        let auto_plot = opts.auto_plot;
                        *opts = WrfProcessUiState::default();
                        opts.auto_plot = auto_plot;
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
                        .color(crate::ui_theme::theme().warn),
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

    /// Fallback for unsupported targets without a native dialog backend.
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    fn import_pickers(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("Native file and folder import is unavailable on this platform.")
                .small()
                .weak(),
        );
        ui.add_enabled(false, egui::Button::new("Extract namelist…"))
            .on_hover_text("Raw-WRF open/save dialogs are unavailable on this platform.");
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
        let hours: Vec<(u16, Option<rw_store::RwsExactTime>)> = tree
            .models
            .iter()
            .find(|m| m.model == current.model)
            .and_then(|m| m.runs.iter().find(|r| r.run == current.run))
            .map(|r| r.hours.iter().map(|h| (h.hour, h.exact_time)).collect())
            .unwrap_or_default();
        let Some(position) = hours.iter().position(|&(slot, _)| slot == current.hour) else {
            return;
        };
        let next = position as i64 + delta;
        if next < 0 || next as usize >= hours.len() {
            return;
        }
        let (hour, exact_time) = hours[next as usize];
        let key = HourKey {
            model: current.model,
            run: current.run,
            hour,
            exact_time,
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
            self.request_sounding_for(hour, fx, fy);
        }
    }

    /// Request a sounding from an EXPLICIT run/hour (independent of the
    /// browser selection) — used by callers that must not be stale.
    pub fn request_sounding_for(&mut self, hour: HourKey, fx: f64, fy: f64) {
        self.box_sounding_task = None;
        self.box_sounding_pending = None;
        self.box_sounding_summary = None;
        self.box_sounding_armed = false;
        self.sounding_request_mode = SoundingRequestMode::Point;
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
                exact_time: best.exact_time,
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

    /// Contextual actions for the currently displayed field. The primary map
    /// action and three view/edit modes read as one toolbar instead of four
    /// unrelated buttons floating above the image.
    fn viewer_toolbar(&mut self, ui: &mut egui::Ui) {
        if self.latest_field.is_none() {
            return;
        }
        let box_readiness = box_sounding_readiness(&self.hour_store_var_info);
        let box_ready = box_readiness.is_ok()
            && self.latest_field.as_ref().is_some_and(|field| {
                field.grid.is_some() && self.viewer.hour() == Some(&field.key.hour)
            });
        let box_unavailable_reason =
            box_readiness.as_ref().err().cloned().unwrap_or_else(|| {
                "The displayed field has no compatible geographic grid".to_owned()
            });
        let theme = crate::ui_theme::theme();
        egui::Frame::new()
            .fill(theme.faint)
            .stroke(egui::Stroke::new(1.0_f32, theme.hairline))
            .corner_radius(4)
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new("FIELD TOOLS")
                            .size(11.0)
                            .strong()
                            .color(crate::ui_theme::subhead_color()),
                    );
                    if ui
                        .button("Add to radar map")
                        .on_hover_text(
                            "Render this field as a layer under the radar; opacity remains in Layers.",
                        )
                        .clicked()
                    {
                        self.map_request = self.latest_field.clone();
                    }

                    let box_response = ui
                        .add_enabled(
                            box_ready,
                            egui::Button::selectable(
                                self.box_sounding_armed,
                                "Box sounding",
                            ),
                        )
                        .on_hover_text(if box_ready {
                            "Arm the radar map, then drag a rectangle. BowEcho averages the model's primitive sounding columns inside it before deriving any diagnostics."
                                .to_owned()
                        } else {
                            box_unavailable_reason.clone()
                        });
                    if box_response.clicked() {
                        self.set_box_sounding_armed(!self.box_sounding_armed);
                    }

                    let mut model_plot_open = self.show_plot_viewer
                        && self.native_plot_content == NativePlotContent::Model;
                    if ui
                        .toggle_value(&mut model_plot_open, "Native plot")
                        .on_hover_text(
                            "Render the selected field through rusty-weather's native plot \
                             pipeline. Shift-drag the field viewer to select a custom domain.",
                        )
                        .changed()
                    {
                        self.native_plot_content = NativePlotContent::Model;
                        self.show_plot_viewer = model_plot_open;
                    }

                    // Map-side domain drawing remains explicitly armed so it
                    // cannot collide with pan, loupe, sounding, or 3-D input.
                    let plot_changed = ui
                        .toggle_value(&mut self.plot_domain_armed, "Draw map domain")
                        .on_hover_text(
                            "Arm the radar map: the next click-drag draws the native-plot \
                             domain. Esc, right-click, or clicking this again cancels. \
                             Shortcut: Ctrl+Shift+drag the map.",
                        )
                        .changed();
                    if plot_changed && self.plot_domain_armed {
                        self.box_sounding_armed = false;
                    }
                    ui.toggle_value(&mut self.show_color_tables, "Color tables")
                        .on_hover_text(
                            "Edit model plot palettes and product bindings. Overrides apply \
                             in this viewer and on the radar-map layer.",
                        );
                });
            });
        ui.add_space(4.0);
    }

    /// The dock body — call inside an egui Window/panel. Returns false when
    /// the user asked to close.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.handle_responses();

        egui::Panel::left("model_runs")
            .resizable(true)
            .default_size(300.0)
            .min_size(260.0)
            .max_size(440.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(model_section_heading("Model library"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button("↻")
                            .on_hover_text("Re-scan the model store")
                            .clicked()
                        {
                            self.rescan();
                        }
                    });
                });
                let store_label = self.store_root.display().to_string();
                crate::panel_kit::status_block(ui, &store_label, None);
                ui.add_space(6.0);
                model_subheading(ui, "Runs");

                let mut picked = None;
                let available = ui.available_height();
                let list_height = if available.is_finite() {
                    (available * 0.52).clamp(170.0, 460.0)
                } else {
                    360.0
                };
                match &self.tree {
                    None => {
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), list_height),
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Scanning model store…");
                                });
                            },
                        );
                    }
                    Some(tree) if tree.models.is_empty() => {
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), list_height),
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| {
                                ui.label("No model runs yet.");
                                ui.label(
                                    egui::RichText::new(
                                        "Use Acquire model data above, or open Windows > WRF for raw wrfout workflows.",
                                    )
                                    .small()
                                    .weak(),
                                );
                            },
                        );
                    }
                    Some(tree) => {
                        let browser = &mut self.browser;
                        egui::ScrollArea::vertical()
                            .id_salt("model_runs_list")
                            .min_scrolled_height(list_height)
                            .max_height(list_height)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                picked = browser.ui(ui, tree);
                            });
                    }
                }
                if let Some(key) = picked {
                    self.select_hour(key);
                }
                ui.add_space(6.0);
                let actions_height = ui.available_height().max(120.0);
                egui::ScrollArea::vertical()
                    .id_salt("model_library_actions")
                    .max_height(actions_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| self.model_library_controls(ui));
            });

        // The sounding is NOT rendered here. A model-sounding load feeds BOTH
        // `self.latest_sounding` (which the app's `poll_native_sounding` routes
        // into the workspace Sounding pane docked beside this plot, or the
        // floating native window) AND `self.sounding` (this panel). Rendering it
        // here too put a SECOND copy inside the Model tile — one Alt-click showed
        // two soundings, and closing the docked pane left this internal panel
        // behind, swallowing the plot (owner report). The workspace pane / native
        // window is the single sounding surface; `self.sounding` is still the
        // backing panel, drawn there via `sounding_ui`. See main.rs
        // `dock_model_sounding_beside_plot`.

        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.viewer_toolbar(ui);
            match self.viewer.ui(ui) {
                Some(FieldViewerEvent::VarSelected(var)) => {
                    if self.viewer.restore_generated_field(&var) {
                        self.latest_field = self
                            .viewer
                            .current_field()
                            .cloned()
                            .map(std::sync::Arc::new);
                    } else {
                        self.viewer.set_loading(&var);
                        if let Some(field) = self.viewer.wanted_field() {
                            // The viewer selects by display label; real store
                            // vars load through the worker, synthesized iso
                            // levels through the plane loader.
                            self.request_field_load(field);
                        }
                    }
                }
                Some(FieldViewerEvent::PointClicked { fx, fy }) => {
                    if let Some(hour) = self.viewer.hour().cloned() {
                        self.request_sounding_for(hour, fx, fy);
                    }
                }
                // v0.2.3 custom-domain plot: shift-drag a box on the field
                // viewer to select an arbitrary plot domain, or drag a corner
                // to rotate it. Open the native plot viewer and retarget it.
                Some(FieldViewerEvent::DomainSelected(domain)) => {
                    self.native_plot_content = NativePlotContent::Model;
                    self.show_plot_viewer = true;
                    self.plot_viewer.set_active_domain(domain);
                }
                Some(FieldViewerEvent::DomainRotationChanged { rotation_deg }) => {
                    self.native_plot_content = NativePlotContent::Model;
                    self.show_plot_viewer = true;
                    self.plot_viewer.set_active_domain_rotation(rotation_deg);
                }
                None => {}
            }
        });
    }
}

fn model_section_heading(title: &str) -> egui::RichText {
    egui::RichText::new(title.to_uppercase())
        .size(12.5)
        .strong()
        .color(crate::ui_theme::subhead_color())
}

fn box_sounding_readiness(vars: &[rw_ui::VarInfo]) -> Result<(), String> {
    let has_profile = |name: &str| {
        vars.iter()
            .any(|var| var.name == name && var.kind == rw_ui::VarKind::Pressure3D)
    };
    let mut missing = ["temperature_iso", "u_iso", "v_iso", "height_iso"]
        .into_iter()
        .filter(|name| !has_profile(name))
        .collect::<Vec<_>>();
    if !has_profile("dewpoint_iso") && !has_profile("rh_iso") {
        missing.push("dewpoint_iso or rh_iso");
    }
    let has_surface = |name: &str| {
        vars.iter()
            .any(|var| var.name == name && var.kind == rw_ui::VarKind::Surface2D)
    };
    for (exact, approximate) in [
        ("temperature_2m", "approx_temperature_2m"),
        ("dewpoint_2m", "approx_dewpoint_2m"),
        ("u_10m", "approx_u_10m"),
        ("v_10m", "approx_v_10m"),
        ("surface_pressure", "approx_surface_pressure"),
    ] {
        if !has_surface(exact) && !has_surface(approximate) {
            missing.push(exact);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "This model frame cannot build a sounding; missing {}",
            missing.join(", ")
        ))
    }
}

fn box_sounding_summary_ui(ui: &mut egui::Ui, summary: &crate::box_sounding::BoxSoundingSummary) {
    let theme = crate::ui_theme::theme();
    egui::Frame::new()
        .fill(theme.faint)
        .stroke(egui::Stroke::new(1.0, theme.hairline))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("BOX-MEAN SOUNDING").strong());
                ui.label(format!("{}", summary.hour));
                ui.weak(format!("{:.0} ms", summary.read_ms));
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Requested: {}", summary.requested.label()));
                ui.separator();
                ui.label(format!("Sampled: {}", summary.sampled.label()));
                if summary.clipped_to_grid {
                    ui.colored_label(theme.warn, "partly outside model coverage");
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "{} grid cells · {} complete surface cells",
                    summary.selected_cells, summary.surface_cells
                ));
                ui.separator();
                ui.label(format!(
                    "{} levels · {}–{} valid cells/level",
                    summary.usable_levels, summary.min_level_cells, summary.max_level_cells
                ));
                if summary.missing_surface_cells() > 0 {
                    ui.weak(format!(
                        "missing surface coverage: {} cell(s)",
                        summary.missing_surface_cells()
                    ));
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.weak(summary.method_label());
                ui.weak(format!("Moisture: {}.", summary.moisture_name));
                if summary.used_approx_surface {
                    ui.weak("Approximate surface fields filled unavailable exact fields.");
                }
            });
        });
}

fn model_subheading(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title.to_uppercase())
            .size(11.0)
            .strong()
            .color(crate::ui_theme::subhead_color()),
    );
}

fn model_workflow_card<R>(
    ui: &mut egui::Ui,
    title: &str,
    description: &str,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let theme = crate::ui_theme::theme();
    egui::Frame::new()
        .fill(theme.faint)
        .stroke(egui::Stroke::new(1.0_f32, theme.hairline))
        .corner_radius(4)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            model_subheading(ui, title);
            ui.label(egui::RichText::new(description).small().weak());
            ui.add_space(3.0);
            body(ui)
        })
        .inner
}

/// An integer is losslessly representable by binary64 when, after removing
/// trailing zero bits, its significant part fits the 53-bit significand.
fn lead_seconds_exact_in_f64(seconds: u64) -> bool {
    if seconds == 0 {
        return true;
    }
    let significant_bits = u64::BITS - seconds.leading_zeros() - seconds.trailing_zeros();
    significant_bits <= f64::MANTISSA_DIGITS
}

fn formula_axis_supports_adjacent_times(
    axis: &std::collections::BTreeMap<u16, rw_formula::ExactStoreTime>,
) -> bool {
    if axis.len() < 2 {
        return false;
    }
    axis.values()
        .map(|time| time.seconds)
        .try_fold(None, |previous, seconds| {
            if !seconds.is_finite() || previous.is_some_and(|prior| seconds <= prior) {
                Err(())
            } else {
                Ok(Some(seconds))
            }
        })
        .is_ok()
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
pub(crate) fn display_var_name(store_var: &str, hour_store_vars: &[String]) -> Option<String> {
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
            && (latest.key.var == current.key.var
                || display_var_name(&latest.key.var, hour_store_vars)
                    .as_deref()
                    .is_some_and(|display| display == current.key.var))
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
pub(crate) fn attach_solar_fallback_style(
    field: &mut rw_ui::FieldData,
    hour_store_vars: &[String],
) {
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

/// Patch ONLY the scene ("Canvas") zoom of a serialized `SoundingViewState`,
/// preserving every other field. Rebuilds a minimal object graph when the
/// value (or its `zooms` entry) is not a JSON object — e.g. the `Null` a
/// serialize failure returns — so the patch never panics on malformed input.
/// Shared by [`ModelDataDock::set_default_sounding_scene_zoom`] and the
/// host's native-only sounding panel.
pub(crate) fn patch_sounding_scene_zoom(view_state: &mut serde_json::Value, zoom: f32) {
    if !view_state.is_object() {
        *view_state = serde_json::Value::Object(serde_json::Map::new());
    }
    let zooms = view_state
        .as_object_mut()
        .expect("object ensured above")
        .entry("zooms")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !zooms.is_object() {
        *zooms = serde_json::Value::Object(serde_json::Map::new());
    }
    zooms
        .as_object_mut()
        .expect("object ensured above")
        .insert("scene".to_owned(), serde_json::json!(zoom));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn box_sounding_and_plot_domain_arms_are_mutually_exclusive() {
        let ctx = egui::Context::default();
        let mut dock = ModelDataDock::new_for_test(&ctx, StoreTree::default());

        dock.set_box_sounding_armed(true);
        assert!(dock.box_sounding_armed());
        assert!(!dock.plot_domain_armed());

        dock.set_plot_domain_armed(true);
        assert!(dock.plot_domain_armed());
        assert!(!dock.box_sounding_armed());
    }

    #[test]
    fn box_sounding_readiness_requires_primitive_profiles_and_surface_state() {
        let var = |name: &str, kind| rw_ui::VarInfo {
            name: name.to_owned(),
            units: String::new(),
            kind,
            levels_hpa: Vec::new(),
        };
        let mut vars = [
            "temperature_iso",
            "dewpoint_iso",
            "u_iso",
            "v_iso",
            "height_iso",
        ]
        .into_iter()
        .map(|name| var(name, rw_ui::VarKind::Pressure3D))
        .collect::<Vec<_>>();
        vars.extend(
            [
                "temperature_2m",
                "dewpoint_2m",
                "u_10m",
                "v_10m",
                "surface_pressure",
            ]
            .into_iter()
            .map(|name| var(name, rw_ui::VarKind::Surface2D)),
        );
        assert!(box_sounding_readiness(&vars).is_ok());
        vars.retain(|var| var.name != "surface_pressure");
        assert!(
            box_sounding_readiness(&vars)
                .unwrap_err()
                .contains("surface_pressure")
        );
    }

    /// The auto-plot toggle defaults ON, and a settings blob persisted
    /// BEFORE the toggle existed restores to ON too (serde default) — an
    /// old config must not silently disable the new behavior.
    #[test]
    fn auto_plot_toggle_defaults_on_and_survives_old_settings() {
        assert!(WrfProcessUiState::default().auto_plot);
        let restored: WrfProcessUiState = serde_json::from_value(serde_json::json!({
            "core_fields": true,
            "diagnostics": true,
            "heavy_ecape": false,
            "raw_extras": true,
            "only_text": "",
            "skip_text": ""
        }))
        .expect("pre-toggle settings blob parses");
        assert!(restored.auto_plot);
        let roundtrip: WrfProcessUiState = serde_json::from_value(
            serde_json::to_value(WrfProcessUiState {
                auto_plot: false,
                ..WrfProcessUiState::default()
            })
            .expect("serializes"),
        )
        .expect("roundtrips");
        assert!(!roundtrip.auto_plot, "an explicit OFF persists");
    }

    #[test]
    fn model_run_time_parses_operational_run_slug() {
        assert_eq!(
            model_run_time_utc("20260618_03z"),
            Some(chrono::Utc.with_ymd_and_hms(2026, 6, 18, 3, 0, 0).unwrap())
        );
        assert_eq!(model_run_time_utc("bad-run"), None);
    }

    #[test]
    fn patch_scene_zoom_sets_scene_and_preserves_siblings() {
        let mut value = serde_json::json!({
            "zooms": { "scene": 1.08, "skewt": { "zoom": 2.0 } },
            "overlays": { "wind_barbs": false }
        });
        patch_sounding_scene_zoom(&mut value, 1.25);
        assert!((value["zooms"]["scene"].as_f64().unwrap() - 1.25).abs() < 1e-6);
        // Untouched siblings survive the patch.
        assert!((value["zooms"]["skewt"]["zoom"].as_f64().unwrap() - 2.0).abs() < 1e-6);
        assert_eq!(value["overlays"]["wind_barbs"], serde_json::json!(false));
    }

    #[test]
    fn patch_scene_zoom_rebuilds_from_non_object() {
        let mut value = serde_json::Value::Null;
        patch_sounding_scene_zoom(&mut value, 1.4);
        assert!((value["zooms"]["scene"].as_f64().unwrap() - 1.4).abs() < 1e-6);
    }

    #[test]
    fn default_scene_zoom_round_trips_through_the_pinned_panel() {
        // Proof through OUR code path: the patched JSON must survive the
        // pinned rw_ui SoundingPanel's own view-state round-trip.
        let mut panel = rw_ui::SoundingPanel::new();
        let mut view_state = panel.view_state_json();
        patch_sounding_scene_zoom(&mut view_state, 1.25);
        assert!(panel.apply_view_state_json(&view_state));
        let back = panel.view_state_json();
        assert!((back["zooms"]["scene"].as_f64().unwrap() - 1.25).abs() < 1e-6);
    }

    fn override_test_field(var: &str) -> rw_ui::FieldData {
        rw_ui::FieldData {
            key: rw_ui::FieldKey {
                hour: HourKey {
                    model: "wrf".to_owned(),
                    run: "20260519_00z".to_owned(),
                    hour: 0,
                    exact_time: None,
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
            exact_time: None,
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
                "Shortwave up TOA — reflected solar",
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
                        exact_time_axis: false,
                        hours: hours
                            .iter()
                            .map(|&hour| rw_ui::HourEntry {
                                hour,
                                file: format!("f{hour:03}.rws"),
                                variable_count: 1,
                                written_unix: 0,
                                exact_time: None,
                            })
                            .collect(),
                    })
                    .collect(),
            }],
            warnings: Vec::new(),
        }
    }

    fn exact_formula_tree(times: &[(u16, rw_store::RwsExactTime)]) -> StoreTree {
        StoreTree {
            models: vec![rw_ui::ModelEntry {
                model: "wrf".to_owned(),
                runs: vec![rw_ui::RunEntry {
                    run: "research".to_owned(),
                    build: "test".to_owned(),
                    writer_version: "test".to_owned(),
                    nx: 2,
                    ny: 2,
                    exact_time_axis: true,
                    hours: times
                        .iter()
                        .map(|&(slot, exact_time)| rw_ui::HourEntry {
                            hour: slot,
                            file: format!("f{slot:03}.rws"),
                            variable_count: 1,
                            written_unix: 0,
                            exact_time: Some(exact_time),
                        })
                        .collect(),
                }],
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn formula_source_verifies_complete_exact_axis_and_fails_dt_closed() {
        let first = rw_store::RwsExactTime::new(31_680, 134_243_280);
        let second = rw_store::RwsExactTime::new(33_480, 134_245_080);
        let ctx = egui::Context::default();
        let mut dock =
            ModelDataDock::new_for_test(&ctx, exact_formula_tree(&[(0, first), (1, second)]));
        dock.browser.select(HourKey {
            model: "wrf".to_owned(),
            run: "research".to_owned(),
            hour: 0,
            exact_time: Some(first),
        });
        let source = dock.formula_store_source().expect("selected store source");
        assert!(source.temporal_axis_verified);
        assert_eq!(source.exact_times.len(), 2);
        assert_eq!(source.exact_times[&0].seconds, 31_680.0);
        assert!(
            source.exact_times[&0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("+08:48:00"))
        );

        // A stale selected time keeps pointwise store formulas available but
        // supplies no host approval for AdjacentTimes/dt.
        dock.browser.select(HourKey {
            model: "wrf".to_owned(),
            run: "research".to_owned(),
            hour: 0,
            exact_time: Some(second),
        });
        let stale = dock
            .formula_store_source()
            .expect("pointwise source remains");
        assert!(!stale.temporal_axis_verified);
        assert!(stale.exact_times.is_empty());

        let huge = rw_store::RwsExactTime::new(u64::MAX, 0);
        dock.tree = Some(exact_formula_tree(&[(0, huge)]));
        dock.browser.select(HourKey {
            model: "wrf".to_owned(),
            run: "research".to_owned(),
            hour: 0,
            exact_time: Some(huge),
        });
        let unrepresentable = dock
            .formula_store_source()
            .expect("pointwise source remains");
        assert!(!unrepresentable.temporal_axis_verified);
        assert!(unrepresentable.exact_times.is_empty());
        assert!(lead_seconds_exact_in_f64(1_u64 << 63));
        assert!(!lead_seconds_exact_in_f64(u64::MAX));
    }

    #[test]
    fn formula_source_derives_verified_v1_forecast_hour_axis() {
        let ctx = egui::Context::default();
        let mut dock = ModelDataDock::new_for_test(
            &ctx,
            tree_with_runs("hrrr", &[("20260711_00z", &[0, 1, 3])]),
        );
        dock.browser.select(HourKey {
            model: "hrrr".to_owned(),
            run: "20260711_00z".to_owned(),
            hour: 1,
            exact_time: None,
        });
        let source = dock.formula_store_source().expect("v1 store source");
        assert!(source.temporal_axis_verified);
        assert_eq!(source.exact_times.len(), 3);
        assert_eq!(source.exact_times[&0].seconds, 0.0);
        assert_eq!(source.exact_times[&1].seconds, 3_600.0);
        assert_eq!(source.exact_times[&3].seconds, 10_800.0);
        assert_eq!(
            source.exact_times[&3].label.as_deref(),
            Some("f003 · +03:00:00")
        );

        dock.tree = Some(tree_with_runs("hrrr", &[("20260711_00z", &[0])]));
        dock.browser.select(HourKey {
            model: "hrrr".to_owned(),
            run: "20260711_00z".to_owned(),
            hour: 0,
            exact_time: None,
        });
        let singleton = dock.formula_store_source().expect("single-frame source");
        assert_eq!(singleton.exact_times.len(), 1);
        assert!(
            !singleton.temporal_axis_verified,
            "one timestamp is honest metadata but cannot satisfy AdjacentTimes"
        );

        dock.tree = Some(tree_with_runs("wrf", &[("local-research-run", &[0, 1, 2])]));
        dock.browser.select(HourKey {
            model: "wrf".to_owned(),
            run: "local-research-run".to_owned(),
            hour: 1,
            exact_time: None,
        });
        let local = dock.formula_store_source().expect("local v1 source");
        assert!(local.exact_times.is_empty());
        assert!(
            !local.temporal_axis_verified,
            "sequential local-WRF v1 slots are not proven forecast hours"
        );

        dock.tree = Some(tree_with_runs(
            "custom_model",
            &[("20260711_00z", &[0, 1, 2])],
        ));
        dock.browser.select(HourKey {
            model: "custom_model".to_owned(),
            run: "20260711_00z".to_owned(),
            hour: 1,
            exact_time: None,
        });
        let custom = dock.formula_store_source().expect("custom v1 source");
        assert!(custom.exact_times.is_empty());
        assert!(!custom.temporal_axis_verified);
    }

    #[test]
    fn formula_temporal_axis_requires_strictly_increasing_distinct_times() {
        let increasing = std::collections::BTreeMap::from([
            (0, rw_formula::ExactStoreTime::new(0.0, None)),
            (1, rw_formula::ExactStoreTime::new(1_800.0, None)),
        ]);
        assert!(formula_axis_supports_adjacent_times(&increasing));
        let duplicate = std::collections::BTreeMap::from([
            (0, rw_formula::ExactStoreTime::new(0.0, None)),
            (1, rw_formula::ExactStoreTime::new(0.0, None)),
        ]);
        assert!(!formula_axis_supports_adjacent_times(&duplicate));
    }

    #[test]
    fn rescan_clears_formula_inventory_before_same_key_refresh() {
        let ctx = egui::Context::default();
        let mut dock =
            ModelDataDock::new_for_test(&ctx, tree_with_runs("hrrr", &[("20260711_00z", &[0])]));
        dock.browser.select(HourKey {
            model: "hrrr".to_owned(),
            run: "20260711_00z".to_owned(),
            hour: 0,
            exact_time: None,
        });
        dock.hour_store_vars = vec!["stale_field".to_owned()];
        dock.hour_store_var_info = vec![rw_ui::VarInfo {
            name: "stale_field".to_owned(),
            units: "1".to_owned(),
            kind: rw_ui::VarKind::Surface2D,
            levels_hpa: Vec::new(),
        }];
        dock.rescan();
        assert!(dock.hour_store_vars.is_empty());
        assert!(dock.hour_store_var_info.is_empty());
    }

    #[test]
    fn formula_raw_source_accepts_extensionless_non_d01_wrfout() {
        let ctx = egui::Context::default();
        let mut dock = ModelDataDock::new_for_test(&ctx, StoreTree::default());
        let path = PathBuf::from("wrfout_d37_2026-07-10_20:00:00");
        dock.formula_raw_path = Some(path.clone());
        let source = dock.formula_raw_source().expect("staged raw WRF source");
        assert_eq!(source.path, path);
        assert_eq!(source.display_hour.run, "wrfout_d37_2026-07-10_20:00:00");
    }

    #[test]
    fn namelist_extraction_prefers_only_an_existing_retained_raw_source() {
        let path = std::env::temp_dir().join(format!(
            "bowecho-namelist-source-{}-wrfout_d37_2026-07-10_20_00_00",
            std::process::id()
        ));
        std::fs::write(&path, b"fixture marker").unwrap();
        assert_eq!(
            retained_namelist_source(Some(&path)),
            Some(path.clone()),
            "an existing staged source bypasses the open dialog"
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            retained_namelist_source(Some(&path)),
            None,
            "a stale staged source falls through to the open dialog"
        );
        assert_eq!(retained_namelist_source(None), None);
    }

    #[test]
    fn formula_result_installs_auto_style_and_becomes_native_plot_source() {
        let ctx = egui::Context::default();
        let mut dock = ModelDataDock::new_for_test(&ctx, StoreTree::default());
        let hour = HourKey {
            model: "wrf".to_owned(),
            run: "research".to_owned(),
            hour: 0,
            exact_time: None,
        };
        dock.install_formula_field(
            rw_ui::FieldData {
                key: rw_ui::FieldKey {
                    hour,
                    var: "custom_diagnostic".to_owned(),
                },
                units: "m/s".to_owned(),
                nx: 3,
                ny: 1,
                values: vec![-4.0, 2.0, 1_000.0],
                range: Some((-4.0, 1_000.0)),
                grid: None,
                lat_descending: false,
                style: None,
            },
            false,
        );
        assert_eq!(dock.native_plot_content, NativePlotContent::Model);
        let installed = dock.latest_field().expect("external Formula field");
        let style = installed.style.as_ref().expect("automatic Formula style");
        assert!(style.title.contains("auto, full finite range"));
        let scale = style.scale.resolved_discrete();
        assert_eq!(scale.levels.first(), Some(&-4.0));
        assert_eq!(scale.levels.last(), Some(&1_000.0));
    }

    #[test]
    fn formula_result_survives_hour_change_and_iso_like_output_name() {
        let ctx = egui::Context::default();
        let mut dock = ModelDataDock::new_for_test(&ctx, StoreTree::default());
        let formula_hour = HourKey {
            model: "wrf".to_owned(),
            run: "research".to_owned(),
            hour: 0,
            exact_time: None,
        };
        dock.install_formula_field(
            rw_ui::FieldData {
                key: rw_ui::FieldKey {
                    hour: formula_hour.clone(),
                    var: "temperature_850".to_owned(),
                },
                units: "K".to_owned(),
                nx: 2,
                ny: 1,
                values: vec![280.0, 281.0],
                range: Some((280.0, 281.0)),
                grid: None,
                lat_descending: false,
                style: None,
            },
            false,
        );
        assert!(
            store_named_current_field(
                &dock.viewer,
                dock.latest_field.as_deref(),
                &dock.hour_store_vars
            )
            .is_some()
        );
        let card_field = dock
            .formula_result_field
            .clone()
            .expect("result card field");

        dock.viewer.set_hour(
            HourKey {
                model: "hrrr".to_owned(),
                run: "20260711_00z".to_owned(),
                hour: 1,
                exact_time: None,
            },
            vec![test_var("temperature_2m", "K")],
        );
        assert!(
            !dock.viewer.restore_generated_field("temperature_850"),
            "rw-ui drops generated cache when another hour is selected"
        );

        dock.activate_formula_result(&card_field);
        let current = dock.viewer.current_field().expect("formula restored");
        assert_eq!(current.key.hour, formula_hour);
        assert_eq!(current.key.var, "temperature_850");
        assert!(
            store_named_current_field(
                &dock.viewer,
                dock.latest_field.as_deref(),
                &dock.hour_store_vars
            )
            .is_some()
        );
        let raw = dock
            .formula_result_raw_field
            .as_ref()
            .expect("raw scientific result retained");
        assert_eq!(raw.values, [280.0, 281.0]);
        assert_eq!(raw.units, "K");
    }

    #[test]
    fn satellite_plot_routes_without_changing_current_model_field() {
        let ctx = egui::Context::default();
        let mut dock =
            ModelDataDock::new_for_test(&ctx, tree_with_runs("wrf", &[("20260707_00z", &[0])]));
        let model_field = std::sync::Arc::new(override_test_field("temperature_2m"));
        dock.latest_field = Some(std::sync::Arc::clone(&model_field));
        let source = SatellitePlotSource::scalar_from_mesh(
            "SimSat precipitable water",
            "HRRR 20260710 20Z",
            "f001",
            "mm",
            2,
            2,
            vec![20.0, 21.0, 30.0, 31.0],
            vec![30.0, 30.0, 31.0, 31.0],
            vec![-101.0, -100.0, -101.0, -100.0],
            None,
        )
        .unwrap();
        dock.open_satellite_plot(source);

        assert_eq!(dock.native_plot_content, NativePlotContent::Satellite);
        assert!(dock.show_plot_viewer);
        assert!(dock.satellite_plot.source().is_some());
        assert!(
            std::sync::Arc::ptr_eq(dock.latest_field.as_ref().unwrap(), &model_field),
            "opening an external plot must preserve the selected model field"
        );

        let out = std::env::temp_dir().join(format!(
            "bowecho-satellite-native-plot-{}.png",
            std::process::id()
        ));
        dock.save_satellite_plot_png(&out, 320, 240).unwrap();
        assert!(out.is_file(), "Save PNG writes the requested path");
        let image = image::open(&out).unwrap();
        assert_eq!((image.width(), image.height()), (320, 240));
        let _ = std::fs::remove_file(out);

        dock.clear_satellite_plot();
        assert_eq!(dock.native_plot_content, NativePlotContent::Model);
        assert!(
            dock.show_plot_viewer,
            "the preserved model field takes over"
        );
        assert!(dock.satellite_plot.source().is_none());
        assert!(std::sync::Arc::ptr_eq(
            dock.latest_field.as_ref().unwrap(),
            &model_field
        ));
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
    fn synthetic_radar_cancel_wins_over_queued_success_and_deep_wrapped_cancel_is_normal() {
        let ctx = egui::Context::default();
        let mut dock = ModelDataDock::new_for_test(&ctx, StoreTree::default());

        let (tx, rx) = std::sync::mpsc::channel();
        let task = crate::wrf_radar::SyntheticRadarTask {
            label: "cancel race".to_owned(),
            rx,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        task.request_cancel();
        assert!(task.cancellation_requested());
        tx.send(crate::wrf_radar::SyntheticRadarMessage::Done(Ok(
            crate::wrf_radar::SyntheticRadarOutput {
                label: "must not install".to_owned(),
                volumes: Vec::new(),
                notes: Vec::new(),
                config_fingerprint: 0,
                frame_sources: Vec::new(),
            },
        )))
        .unwrap();
        dock.import_job = Some(ImportJob::SyntheticRadar(task));
        dock.poll_import();
        assert_eq!(
            dock.import_message.as_deref(),
            Some("Synthetic radar cancelled")
        );
        assert!(dock.import_job.is_none());
        assert!(dock.synthetic_radar_result.is_none());

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(crate::wrf_radar::SyntheticRadarMessage::Done(Err(
            "wrfout time 0: build property T-matrix scene: evaluate selected property-scattering tables: synthetic-radar T-matrix scene build cancelled"
                .to_owned(),
        )))
        .unwrap();
        dock.import_job = Some(ImportJob::SyntheticRadar(
            crate::wrf_radar::SyntheticRadarTask {
                label: "deep cancel".to_owned(),
                rx,
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        ));
        dock.poll_import();
        assert_eq!(
            dock.import_message.as_deref(),
            Some("Synthetic radar cancelled")
        );
        assert!(
            !dock
                .import_message
                .as_deref()
                .is_some_and(|message| message.contains("failed"))
        );
    }

    #[test]
    fn synth_radar_refresh_reuses_sources_with_the_current_controls() {
        let ctx = egui::Context::default();
        let mut dock =
            ModelDataDock::new_for_test(&ctx, tree_with_runs("wrf", &[("20260707_00z", &[0])]));
        assert!(
            dock.synthetic_radar_refresh_request().is_err(),
            "refresh stays unavailable until a source frame set has been built"
        );

        let files = vec![
            PathBuf::from("wrfout_d03_2026-07-07_01_00_00"),
            PathBuf::from("wrfout_d03_2026-07-07_02_00_00"),
        ];
        dock.synthetic_radar_source_files = files.clone();
        dock.synth_radar.max_range_km = 480.0;
        dock.synth_radar.dual_pol = true;

        let (refresh_files, config) = dock
            .synthetic_radar_refresh_request()
            .expect("remembered sources make refresh available");
        assert_eq!(refresh_files, files, "the exact frame snapshot is reused");
        assert_eq!(config.max_range_m, 480_000.0);
        assert!(
            config.dual_pol,
            "refresh builds a fresh config from controls edited after the original run"
        );
        let persisted = dock.wrf_synth_radar_json().to_string();
        assert!(
            !persisted.contains("wrfout_d03"),
            "source paths stay session-only instead of becoming stale settings"
        );
    }

    #[test]
    fn synth_radar_refresh_never_replaces_an_active_import() {
        let ctx = egui::Context::default();
        let mut dock =
            ModelDataDock::new_for_test(&ctx, tree_with_runs("wrf", &[("20260707_00z", &[0])]));
        let files = vec![PathBuf::from("wrfout_d02_2026-07-07_00_00_00")];
        dock.synthetic_radar_source_files = files.clone();
        dock.mark_import_in_flight_for_test();

        let error = dock
            .synthetic_radar_refresh_request()
            .expect_err("an active import must own the single worker slot");
        assert!(error.contains("another import"));
        assert!(dock.import_job.is_some(), "the existing task is untouched");
        assert_eq!(
            dock.synthetic_radar_source_files, files,
            "busy rejection keeps the current frame set available for later"
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
            !empty.match_gate_to_grid,
            "match-gate-to-grid restores OFF (an older config uses the configured gates)"
        );
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
        assert_eq!(
            empty.simulation_mode,
            crate::wrf_radar::SimulationMode::Presentation
        );
        assert_eq!(
            empty.compute_preference,
            crate::wrf_radar::SyntheticRadarComputePreference::Auto,
            "older settings restore automatic CPU/CUDA selection"
        );
        assert_eq!(
            empty.scan_strategy,
            crate::wrf_radar::SyntheticScanStrategy::CustomLegacy,
            "older settings restore the historical custom ladder"
        );
        assert_eq!(
            empty.reflectivity_sampling,
            crate::wrf_radar::ReflectivitySampling::LinearZ
        );
        assert_eq!(
            empty.beam_integration,
            crate::wrf_radar::BeamIntegration::Center
        );
        assert_eq!(empty.beam_width_deg, default_synth_beam_width_deg());
        assert_eq!(empty.pulse_width_us, default_synth_pulse_width_us());
        assert_eq!(
            empty.radar_frequency_mhz,
            default_synth_radar_frequency_mhz()
        );
        assert!(!empty.terminal_fall_speed);
        assert!(!empty.terrain_blockage);
        assert!(!empty.spectrum_width);
        assert_eq!(
            empty.spectrum_width_floor_mps,
            default_synth_spectrum_width_floor_mps()
        );
        assert!(!empty.dual_pol);
        assert_eq!(
            empty.polarimetric_kernel,
            crate::wrf_radar::PolarimetricKernel::BulkRayleighV1,
            "older settings retain the compatible bulk Rayleigh operator"
        );
        assert_eq!(
            empty.property_tmatrix_table_source,
            app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1
        );
        assert_eq!(
            empty.property_tmatrix_rain_sensitivity,
            crate::wrf_radar::PropertyTMatrixRainSensitivity::FullProperty
        );
        assert!(!empty.propagation);
        assert_eq!(
            empty.propagation_geometry,
            crate::wrf_radar::PropagationGeometry::StandardFourThirdsEarth
        );
        assert_eq!(empty.system_phidp_deg, 0.0);
        assert_eq!(empty.zdr_bias_db, 0.0);
        assert_eq!(
            empty.scan_timing,
            crate::wrf_radar::ScanTiming::InstantaneousTruth
        );
        assert_eq!(
            empty.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart,
            "older settings keep one frozen WRF scene"
        );
        assert_eq!(
            empty.missing_neighbor_policy,
            app_ui::wrf_temporal::MissingNeighborPolicy::HoldAnchor,
            "the backward-compatible final-frame policy holds the anchor"
        );
        assert_eq!(
            empty.temporal_memory_budget_mib, 65_536,
            "older settings receive the 64 GiB temporal-build RAM cap"
        );
        assert_eq!(
            empty.rotation_rate_deg_s,
            default_synth_rotation_rate_deg_s()
        );
        assert_eq!(empty.transition_delay_s, default_synth_transition_delay_s());
        assert_eq!(empty.prf_hz, default_synth_prf_hz());
        assert!(!empty.coupled_single_prf_estimator);
        assert_eq!(empty.estimator_dwell_ms, default_synth_estimator_dwell_ms());
        assert_eq!(empty.estimator_pulse_count, None);
        assert_eq!(
            empty.estimator_independent_sample_fraction,
            default_synth_estimator_independent_sample_fraction()
        );
        assert_eq!(empty.estimator_minimum_snr_db, 0.0);
        assert!(!empty.emit_stage_diagnostics);
        assert!(!empty.instrument_noise);
        assert_eq!(
            empty.sensitivity_dbz_at_1km,
            default_synth_sensitivity_dbz_at_1km()
        );
        assert!(empty.emit_quality_fields);
        assert_eq!(empty.minimum_model_coverage_fraction, 0.0);
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
        assert!(empty.operational_f00);
        assert!(!empty.operational_f01);

        // A non-default selection survives the round trip.
        let custom = SyntheticRadarUiState {
            placement: SynthPlacement::NexradSite,
            site_id_text: "KTLX".to_string(),
            max_range_km: 460.0,
            auto_gate_spacing: false,
            gate_spacing_m: 500.0,
            match_gate_to_grid: true,
            ref_gate_texture: false,
            vel_gate_texture: true,
            reflectivity_operator: crate::wrf_radar::ReflectivityOperator::ClassicStoelinga,
            simulation_mode: crate::wrf_radar::SimulationMode::Instrument,
            compute_preference: crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda,
            scan_strategy: crate::wrf_radar::SyntheticScanStrategy::Build24Vcp212,
            reflectivity_sampling: crate::wrf_radar::ReflectivitySampling::LegacyDbz,
            beam_integration: crate::wrf_radar::BeamIntegration::Reference,
            beam_width_deg: 1.25,
            pulse_width_us: 2.0,
            radar_frequency_mhz: 2_900,
            terminal_fall_speed: true,
            terrain_blockage: true,
            spectrum_width: true,
            spectrum_width_floor_mps: 1.25,
            dual_pol: true,
            polarimetric_kernel: crate::wrf_radar::PolarimetricKernel::PropertyTMatrixResearchV1,
            propagation: true,
            propagation_geometry: crate::wrf_radar::PropagationGeometry::WrfRefractivityResearch,
            system_phidp_deg: 12.5,
            zdr_bias_db: -0.35,
            scan_timing: crate::wrf_radar::ScanTiming::TimedVolume,
            atmosphere_time_mode: app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear,
            missing_neighbor_policy: app_ui::wrf_temporal::MissingNeighborPolicy::DropFrame,
            temporal_memory_budget_mib: 16_384,
            rotation_rate_deg_s: 24.0,
            transition_delay_s: 4.25,
            prf_hz: 1_200.0,
            coupled_single_prf_estimator: true,
            estimator_dwell_ms: 75.0,
            estimator_pulse_count: Some(64),
            estimator_independent_sample_fraction: 0.75,
            estimator_minimum_snr_db: 2.5,
            emit_stage_diagnostics: true,
            instrument_noise: true,
            sensitivity_dbz_at_1km: -37.5,
            property_tmatrix_table_source:
                app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::ExternalValidatedPack,
            property_tmatrix_rain_sensitivity:
                crate::wrf_radar::PropertyTMatrixRainSensitivity::FrozenOnly,
            emit_quality_fields: false,
            minimum_model_coverage_fraction: 0.65,
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
            !config.match_gate_to_grid && !historical.match_gate_to_grid,
            "match-gate-to-grid is opt-in — default uses the configured gate spacing"
        );
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
        assert_eq!(config.simulation_mode, historical.simulation_mode);
        assert_eq!(config.compute_preference, historical.compute_preference);
        assert_eq!(config.scan_strategy, historical.scan_strategy);
        assert_eq!(
            config.reflectivity_sampling,
            historical.reflectivity_sampling
        );
        assert_eq!(config.beam_integration, historical.beam_integration);
        assert_eq!(config.beam_width_deg, historical.beam_width_deg);
        assert_eq!(config.pulse_width_us, historical.pulse_width_us);
        assert_eq!(config.radar_frequency_mhz, historical.radar_frequency_mhz);
        assert_eq!(config.terminal_fall_speed, historical.terminal_fall_speed);
        assert_eq!(config.terrain_blockage, historical.terrain_blockage);
        assert_eq!(config.spectrum_width, historical.spectrum_width);
        assert_eq!(
            config.spectrum_width_floor_mps,
            historical.spectrum_width_floor_mps
        );
        assert_eq!(config.dual_pol, historical.dual_pol);
        assert_eq!(config.polarimetric_kernel, historical.polarimetric_kernel);
        assert_eq!(
            config.property_tmatrix_table_source,
            historical.property_tmatrix_table_source
        );
        assert_eq!(
            config.property_tmatrix_rain_sensitivity,
            historical.property_tmatrix_rain_sensitivity
        );
        assert_eq!(config.propagation, historical.propagation);
        assert_eq!(config.system_phidp_deg, historical.system_phidp_deg);
        assert_eq!(config.zdr_bias_db, historical.zdr_bias_db);
        assert_eq!(config.scan_timing, historical.scan_timing);
        assert_eq!(config.atmosphere_time_mode, historical.atmosphere_time_mode);
        assert_eq!(
            config.missing_neighbor_policy,
            historical.missing_neighbor_policy
        );
        assert_eq!(
            config.temporal_memory_budget_mib,
            historical.temporal_memory_budget_mib
        );
        assert_eq!(config.rotation_rate_deg_s, historical.rotation_rate_deg_s);
        assert_eq!(config.transition_delay_s, historical.transition_delay_s);
        assert_eq!(config.prf_hz, historical.prf_hz);
        assert_eq!(
            config.coupled_single_prf_estimator,
            historical.coupled_single_prf_estimator
        );
        assert_eq!(config.estimator_dwell_ms, historical.estimator_dwell_ms);
        assert_eq!(
            config.estimator_pulse_count,
            historical.estimator_pulse_count
        );
        assert_eq!(
            config.estimator_independent_sample_fraction,
            historical.estimator_independent_sample_fraction
        );
        assert_eq!(
            config.estimator_minimum_snr_db,
            historical.estimator_minimum_snr_db
        );
        assert_eq!(
            config.emit_stage_diagnostics,
            historical.emit_stage_diagnostics
        );
        assert_eq!(config.instrument_noise, historical.instrument_noise);
        assert_eq!(
            config.sensitivity_dbz_at_1km,
            historical.sensitivity_dbz_at_1km
        );
        assert_eq!(config.emit_quality_fields, historical.emit_quality_fields);
        assert_eq!(
            config.minimum_model_coverage_fraction,
            historical.minimum_model_coverage_fraction
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

    #[test]
    fn operational_radar_uses_selected_f00_f01_and_fixed_bulk_contract() {
        let mut state = SyntheticRadarUiState {
            polarimetric_kernel: crate::wrf_radar::PolarimetricKernel::PropertyTMatrixResearchV1,
            atmosphere_time_mode: app_ui::wrf_temporal::AtmosphereTimeMode::RawStateLinear,
            compute_preference: crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda,
            dual_pol: false,
            terminal_fall_speed: false,
            operational_f00: true,
            operational_f01: true,
            ..SyntheticRadarUiState::default()
        };
        assert_eq!(state.operational_forecast_hours(), vec![0, 1]);
        let config = state.to_operational_config().unwrap();
        assert_eq!(
            config.polarimetric_kernel,
            crate::wrf_radar::PolarimetricKernel::BulkRayleighV1
        );
        assert_eq!(
            config.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart
        );
        assert_eq!(
            config.propagation_geometry,
            crate::wrf_radar::PropagationGeometry::StandardFourThirdsEarth
        );
        assert!(config.dual_pol && config.terminal_fall_speed);
        assert_eq!(
            config.compute_preference,
            crate::wrf_radar::SyntheticRadarComputePreference::Cpu,
            "operational bulk-radar builds stay on the fixed CPU contract"
        );

        state.operational_f00 = false;
        assert_eq!(state.operational_forecast_hours(), vec![1]);
    }

    #[test]
    fn synth_radar_deep_controls_flow_into_config() {
        use crate::wrf_radar::{
            BeamIntegration, PolarimetricKernel, ReflectivitySampling, ScanTiming, SimulationMode,
        };

        let state = SyntheticRadarUiState {
            simulation_mode: SimulationMode::Instrument,
            compute_preference: crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda,
            reflectivity_sampling: ReflectivitySampling::LegacyDbz,
            beam_integration: BeamIntegration::Reference,
            beam_width_deg: 1.2,
            pulse_width_us: 2.4,
            radar_frequency_mhz: 2_800,
            terminal_fall_speed: true,
            terrain_blockage: true,
            spectrum_width: true,
            spectrum_width_floor_mps: 1.1,
            dual_pol: true,
            polarimetric_kernel: PolarimetricKernel::PropertyTMatrixResearchV1,
            property_tmatrix_table_source:
                app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::ExternalValidatedPack,
            property_tmatrix_rain_sensitivity:
                crate::wrf_radar::PropertyTMatrixRainSensitivity::FrozenOnly,
            propagation: true,
            system_phidp_deg: 18.5,
            zdr_bias_db: -0.4,
            scan_timing: ScanTiming::TimedVolume,
            atmosphere_time_mode: app_ui::wrf_temporal::AtmosphereTimeMode::LinearAdjacent,
            missing_neighbor_policy: app_ui::wrf_temporal::MissingNeighborPolicy::Error,
            temporal_memory_budget_mib: 12_288,
            rotation_rate_deg_s: 22.0,
            transition_delay_s: 4.5,
            prf_hz: 1_350.0,
            coupled_single_prf_estimator: true,
            estimator_dwell_ms: 80.0,
            estimator_pulse_count: Some(72),
            estimator_independent_sample_fraction: 0.6,
            estimator_minimum_snr_db: 3.0,
            emit_stage_diagnostics: true,
            instrument_noise: true,
            sensitivity_dbz_at_1km: -36.0,
            emit_quality_fields: false,
            minimum_model_coverage_fraction: 0.7,
            ..SyntheticRadarUiState::default()
        };
        let config = state.to_config().unwrap();

        assert_eq!(config.simulation_mode, SimulationMode::Instrument);
        assert_eq!(
            config.compute_preference,
            crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda
        );
        assert_eq!(
            config.reflectivity_sampling,
            ReflectivitySampling::LegacyDbz
        );
        assert_eq!(config.beam_integration, BeamIntegration::Reference);
        assert_eq!(config.beam_width_deg, 1.2);
        assert_eq!(config.pulse_width_us, 2.4);
        assert_eq!(config.radar_frequency_mhz, 2_800);
        assert!(config.terminal_fall_speed);
        assert!(config.terrain_blockage);
        assert!(config.spectrum_width);
        assert_eq!(config.spectrum_width_floor_mps, 1.1);
        assert!(config.dual_pol);
        assert_eq!(
            config.polarimetric_kernel,
            PolarimetricKernel::PropertyTMatrixResearchV1
        );
        assert_eq!(
            config.property_tmatrix_table_source,
            app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::ExternalValidatedPack
        );
        assert_eq!(
            config.property_tmatrix_rain_sensitivity,
            crate::wrf_radar::PropertyTMatrixRainSensitivity::FrozenOnly
        );
        assert!(config.propagation);
        assert_eq!(config.system_phidp_deg, 18.5);
        assert_eq!(config.zdr_bias_db, -0.4);
        assert_eq!(config.scan_timing, ScanTiming::TimedVolume);
        assert_eq!(
            config.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::LinearAdjacent
        );
        assert_eq!(
            config.missing_neighbor_policy,
            app_ui::wrf_temporal::MissingNeighborPolicy::Error
        );
        assert_eq!(config.temporal_memory_budget_mib, 12_288);
        assert_eq!(config.rotation_rate_deg_s, 22.0);
        assert_eq!(config.transition_delay_s, 4.5);
        assert_eq!(config.prf_hz, 1_350.0);
        assert!(config.coupled_single_prf_estimator);
        assert_eq!(config.estimator_dwell_ms, 80.0);
        assert_eq!(config.estimator_pulse_count, Some(72));
        assert_eq!(config.estimator_independent_sample_fraction, 0.6);
        assert_eq!(config.estimator_minimum_snr_db, 3.0);
        assert!(config.emit_stage_diagnostics);
        assert!(config.instrument_noise);
        assert_eq!(config.sensitivity_dbz_at_1km, -36.0);
        assert!(!config.emit_quality_fields);
        assert_eq!(config.minimum_model_coverage_fraction, 0.7);
    }

    #[test]
    fn synth_radar_temporal_interpolation_requires_timed_scan_and_clamps_budget() {
        use crate::wrf_radar::ScanTiming;
        use app_ui::wrf_temporal::{AtmosphereTimeMode, MissingNeighborPolicy};

        // A stale or hand-edited settings entry cannot launch the contradictory
        // combination of adjacent-scene interpolation and instantaneous rays.
        let low_budget = SyntheticRadarUiState {
            scan_timing: ScanTiming::InstantaneousTruth,
            atmosphere_time_mode: AtmosphereTimeMode::LinearAdjacent,
            missing_neighbor_policy: MissingNeighborPolicy::DropFrame,
            temporal_memory_budget_mib: 64,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(low_budget.scan_timing, ScanTiming::TimedVolume);
        assert_eq!(
            low_budget.atmosphere_time_mode,
            AtmosphereTimeMode::LinearAdjacent
        );
        assert_eq!(
            low_budget.missing_neighbor_policy,
            MissingNeighborPolicy::DropFrame
        );
        assert_eq!(low_budget.temporal_memory_budget_mib, 1024);

        let high_budget = SyntheticRadarUiState {
            temporal_memory_budget_mib: usize::MAX,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(high_budget.scan_timing, ScanTiming::InstantaneousTruth);
        assert_eq!(
            high_budget.atmosphere_time_mode,
            AtmosphereTimeMode::FrozenAtVolumeStart
        );
        assert_eq!(high_budget.temporal_memory_budget_mib, 65_536);

        let raw_state = SyntheticRadarUiState {
            scan_timing: ScanTiming::InstantaneousTruth,
            atmosphere_time_mode: AtmosphereTimeMode::RawStateLinear,
            dual_pol: true,
            polarimetric_kernel: crate::wrf_radar::PolarimetricKernel::PropertyTMatrixResearchV1,
            radar_frequency_mhz: crate::wrf_radar::PROPERTY_TMATRIX_RESEARCH_FREQUENCY_MHZ,
            reflectivity_sampling: crate::wrf_radar::ReflectivitySampling::LinearZ,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(raw_state.scan_timing, ScanTiming::TimedVolume);
        assert!(raw_state.validate_science_contract().is_ok());

        let invalid_raw_bulk = SyntheticRadarUiState {
            atmosphere_time_mode: AtmosphereTimeMode::RawStateLinear,
            ..SyntheticRadarUiState::default()
        }
        .to_config()
        .unwrap();
        assert!(
            invalid_raw_bulk
                .validate_science_contract()
                .unwrap_err()
                .contains("only with the P3/ISHMAEL property T-matrix")
        );
    }

    #[test]
    fn synth_radar_mode_preset_is_explicit_and_coherent() {
        use crate::wrf_radar::{BeamIntegration, ScanTiming, SimulationMode};

        let mut state = SyntheticRadarUiState {
            beam_width_deg: 1.35,
            pulse_width_us: 2.1,
            radar_frequency_mhz: 2_850,
            ..SyntheticRadarUiState::default()
        };
        state.apply_mode_preset(SimulationMode::Instrument);
        let instrument = state.to_config().unwrap();
        assert_eq!(instrument.simulation_mode, SimulationMode::Instrument);
        assert_eq!(instrument.beam_integration, BeamIntegration::Balanced);
        assert!(instrument.terminal_fall_speed);
        assert!(instrument.terrain_blockage);
        assert!(instrument.spectrum_width);
        assert!(instrument.dual_pol);
        assert!(instrument.propagation);
        assert_eq!(instrument.scan_timing, ScanTiming::TimedVolume);
        assert_eq!(
            instrument.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart
        );
        assert!(instrument.instrument_noise);
        assert!(instrument.fold_velocity);
        assert!(!instrument.ref_gate_texture);
        assert_eq!(instrument.beam_width_deg, 1.35);
        assert_eq!(instrument.pulse_width_us, 2.1);
        assert_eq!(instrument.radar_frequency_mhz, 2_850);

        // Expert edits stay put until another mode button invokes the helper.
        state.dual_pol = false;
        assert!(!state.to_config().unwrap().dual_pol);
    }

    #[test]
    fn synth_radar_recipes_reset_interacting_knobs_but_preserve_geometry() {
        use crate::wrf_radar::{BeamIntegration, ScanTiming, SimulationMode};

        let mut state = SyntheticRadarUiState {
            placement: SynthPlacement::NexradSite,
            site_id_text: "KTLX".to_owned(),
            max_range_km: 310.0,
            gate_spacing_m: 500.0,
            auto_gate_spacing: false,
            match_gate_to_grid: true,
            beam_width_deg: 4.0,
            prf_hz: 4_500.0,
            system_phidp_deg: 82.0,
            zdr_bias_db: 1.5,
            clutter_intensity: 0.9,
            include_low_tilt: true,
            compute_preference: crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda,
            ..SyntheticRadarUiState::default()
        };

        state.apply_recipe(SyntheticRadarRecipe::RealRadar);
        let real = state.to_config().unwrap();
        let defaults = crate::wrf_radar::SyntheticRadarConfig::default();
        assert_eq!(real.simulation_mode, SimulationMode::Instrument);
        assert_eq!(real.beam_integration, BeamIntegration::Balanced);
        assert!(real.dual_pol && real.propagation && real.spectrum_width);
        assert!(real.terrain_blockage && real.instrument_noise && real.fold_velocity);
        assert_eq!(real.scan_timing, ScanTiming::TimedVolume);
        assert_eq!(
            real.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart
        );
        assert_eq!(
            real.missing_neighbor_policy,
            app_ui::wrf_temporal::MissingNeighborPolicy::HoldAnchor
        );
        assert_eq!(real.beam_width_deg, defaults.beam_width_deg);
        assert_eq!(real.prf_hz, defaults.prf_hz);
        assert_eq!(real.system_phidp_deg, 0.0);
        assert_eq!(real.zdr_bias_db, 0.0);
        assert_eq!(real.clutter_intensity, 0.0);
        assert_eq!(real.site_id, "KTLX");
        assert_eq!(real.max_range_m, 310_000.0);
        assert_eq!(real.gate_spacing_m, 500.0);
        assert!(real.match_gate_to_grid);

        state.apply_recipe(SyntheticRadarRecipe::CleanDualPol);
        let clean = state.to_config().unwrap();
        assert!(clean.dual_pol && clean.propagation && clean.spectrum_width);
        assert!(!clean.terrain_blockage);
        assert!(!clean.instrument_noise);
        assert!(!clean.fold_velocity);
        assert_eq!(clean.scan_timing, ScanTiming::InstantaneousTruth);
        assert_eq!(
            clean.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart
        );

        state.apply_recipe(SyntheticRadarRecipe::CleanTruth);
        let truth = state.to_config().unwrap();
        assert_eq!(truth.scan_timing, ScanTiming::InstantaneousTruth);
        assert_eq!(
            truth.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart
        );

        state.apply_recipe(SyntheticRadarRecipe::MaximumFidelity);
        let maximum = state.to_config().unwrap();
        assert_eq!(maximum.beam_integration, BeamIntegration::Reference);
        assert_eq!(
            maximum.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::LinearAdjacent
        );

        state.apply_recipe(SyntheticRadarRecipe::PropertyTMatrixHybrid);
        let hybrid = state.to_config().unwrap();
        assert_eq!(
            hybrid.polarimetric_kernel,
            crate::wrf_radar::PolarimetricKernel::PropertyTMatrixHybridV1
        );
        assert_eq!(
            hybrid.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart
        );
        assert_eq!(
            hybrid.polarimetric_kernel.scattering_policy(),
            app_ui::wrf_tmatrix_scene::WrfTMatrixScatteringPolicy::HybridBulkRayleighV1
        );
        assert!(hybrid.validate_science_contract().is_ok());

        state.apply_recipe(SyntheticRadarRecipe::PropertyTMatrixResearch);
        let research = state.to_config().unwrap();
        assert_eq!(
            research.polarimetric_kernel,
            crate::wrf_radar::PolarimetricKernel::PropertyTMatrixResearchV1
        );
        assert_eq!(
            research.radar_frequency_mhz,
            crate::wrf_radar::PROPERTY_TMATRIX_RESEARCH_FREQUENCY_MHZ
        );
        assert_eq!(
            research.property_tmatrix_table_source,
            app_ui::wrf_tmatrix_assets::PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1
        );
        assert_eq!(
            research.property_tmatrix_rain_sensitivity,
            crate::wrf_radar::PropertyTMatrixRainSensitivity::FullProperty
        );
        assert_eq!(research.beam_integration, BeamIntegration::Balanced);
        assert_eq!(
            research.atmosphere_time_mode,
            app_ui::wrf_temporal::AtmosphereTimeMode::FrozenAtVolumeStart
        );
        assert!(research.dual_pol && research.propagation);
        assert_eq!(
            research.compute_preference,
            crate::wrf_radar::SyntheticRadarComputePreference::NvidiaCuda,
            "recipes preserve the user's execution preference"
        );
        assert!(research.emit_quality_fields);
        assert_eq!(research.minimum_model_coverage_fraction, 0.0);
        assert!(research.validate_science_contract().is_ok());
    }

    #[test]
    fn synth_radar_temporal_ram_cap_round_trips_and_survives_presets() {
        use crate::wrf_radar::SimulationMode;

        assert_eq!(default_synth_temporal_memory_budget_mib(), 65_536);
        assert_eq!(synth_temporal_budget_gib(12_288), 12.0);
        assert_eq!(synth_temporal_budget_mib_from_gib(12.25), 12_544);
        assert_eq!(synth_temporal_budget_mib_from_gib(0.0), 1_024);
        assert_eq!(synth_temporal_budget_mib_from_gib(100.0), 65_536);

        let customized = SyntheticRadarUiState {
            temporal_memory_budget_mib: 24_576,
            temporal_memory_budget_user_set: true,
            ..SyntheticRadarUiState::default()
        };
        let persisted = serde_json::to_value(&customized).unwrap();
        let mut restored: SyntheticRadarUiState = serde_json::from_value(persisted).unwrap();
        assert_eq!(restored.temporal_memory_budget_mib, 24_576);

        for mode in [
            SimulationMode::Truth,
            SimulationMode::Instrument,
            SimulationMode::Presentation,
        ] {
            restored.apply_mode_preset(mode);
            assert_eq!(
                restored.temporal_memory_budget_mib, 24_576,
                "science mode selection must not own the user's RAM cap"
            );
        }
        for recipe in SyntheticRadarRecipe::ALL {
            restored.apply_recipe(recipe);
            assert_eq!(
                restored.temporal_memory_budget_mib,
                24_576,
                "recipe {} must not reset the user's RAM cap",
                recipe.label()
            );
        }

        // Upgrade behavior: an older settings object without the field gets
        // today's 64 GiB default. v0.33.1's serialized 8 GiB default is
        // migrated, but every other legacy value is treated as a customization.
        let absent = SyntheticRadarUiState::from_persisted_value(&serde_json::json!({})).unwrap();
        assert_eq!(absent.temporal_memory_budget_mib, 65_536);
        assert!(!absent.temporal_memory_budget_user_set);

        let migrated = SyntheticRadarUiState::from_persisted_value(&serde_json::json!({
            "temporal_memory_budget_mib": 8_192
        }))
        .unwrap();
        assert_eq!(migrated.temporal_memory_budget_mib, 65_536);
        assert!(!migrated.temporal_memory_budget_user_set);

        let legacy_custom = SyntheticRadarUiState::from_persisted_value(&serde_json::json!({
            "temporal_memory_budget_mib": 24_576
        }))
        .unwrap();
        assert_eq!(legacy_custom.temporal_memory_budget_mib, 24_576);
        assert!(legacy_custom.temporal_memory_budget_user_set);

        let intentional_eight = SyntheticRadarUiState::from_persisted_value(&serde_json::json!({
            "temporal_memory_budget_mib": 8_192,
            "temporal_memory_budget_user_set": true
        }))
        .unwrap();
        assert_eq!(intentional_eight.temporal_memory_budget_mib, 8_192);
        assert!(intentional_eight.temporal_memory_budget_user_set);
    }

    #[test]
    fn synth_radar_recipe_detection_marks_manual_edits_custom() {
        let mut state = SyntheticRadarUiState {
            compute_preference: crate::wrf_radar::SyntheticRadarComputePreference::Cpu,
            ..SyntheticRadarUiState::default()
        };
        assert_eq!(state.active_recipe(), Some(SyntheticRadarRecipe::StormView));
        state.apply_recipe(SyntheticRadarRecipe::RealRadar);
        assert_eq!(state.active_recipe(), Some(SyntheticRadarRecipe::RealRadar));
        state.apply_recipe(SyntheticRadarRecipe::PropertyTMatrixResearch);
        assert_eq!(
            state.active_recipe(),
            Some(SyntheticRadarRecipe::PropertyTMatrixResearch)
        );
        state.zdr_bias_db = 0.25;
        assert_eq!(state.active_recipe(), None);
    }

    #[test]
    fn synth_radar_work_estimate_tracks_geometry_and_pulse_volume_rule() {
        let default = SyntheticRadarWorkEstimate::from_state(&SyntheticRadarUiState::default());
        assert_eq!(
            default,
            SyntheticRadarWorkEstimate {
                tilt_count: 14,
                rays_per_tilt: 720,
                gates_per_ray: 920,
                samples_per_gate: 1,
                total_samples: 9_273_600,
            }
        );
        assert!(default.summary().contains("9.3 million"));

        let maximum = SyntheticRadarUiState {
            max_range_km: 1_000.0,
            auto_gate_spacing: false,
            gate_spacing_m: 100.0,
            include_low_tilt: true,
            beam_integration: crate::wrf_radar::BeamIntegration::Reference,
            ..SyntheticRadarUiState::default()
        };
        let maximum = SyntheticRadarWorkEstimate::from_state(&maximum);
        assert_eq!(maximum.tilt_count, 15);
        assert_eq!(maximum.gates_per_ray, 10_000);
        assert_eq!(maximum.samples_per_gate, 27);
        assert_eq!(maximum.total_samples, 2_916_000_000);
        assert!(maximum.summary().contains("2.92 billion"));
    }

    /// The "Match gate size to grid resolution" checkbox flows from the UI state
    /// into the scan config, independent of the range/gate controls: default is
    /// off (the configured gate spacing is used), and turning it on sets the flag
    /// while leaving the fallback `gate_spacing_m` intact for a file with no DX.
    #[test]
    fn synth_radar_match_gate_to_grid_flows_into_config() {
        let default = SyntheticRadarUiState::default().to_config().unwrap();
        assert!(
            !default.match_gate_to_grid,
            "default is off — the configured gate spacing is used"
        );

        // On: the flag propagates, and the manual gate spacing rides along as the
        // build-time fallback for files without a DX attribute. `off` differs
        // ONLY by the flag so the fingerprint check isolates it.
        let base = SyntheticRadarUiState {
            auto_gate_spacing: false,
            gate_spacing_m: 500.0,
            ..SyntheticRadarUiState::default()
        };
        let matched = SyntheticRadarUiState {
            match_gate_to_grid: true,
            ..base.clone()
        }
        .to_config()
        .unwrap();
        assert!(matched.match_gate_to_grid);
        assert_eq!(
            matched.gate_spacing_m, 500.0,
            "the configured spacing is preserved as the no-DX fallback"
        );

        // Toggling only the flag moves the data fingerprint so a re-import rebuilds.
        let off = SyntheticRadarUiState {
            match_gate_to_grid: false,
            ..base
        }
        .to_config()
        .unwrap();
        assert!(!off.match_gate_to_grid);
        assert_eq!(
            off.gate_spacing_m, matched.gate_spacing_m,
            "same fallback spacing"
        );
        assert_ne!(
            matched.data_fingerprint(),
            off.data_fingerprint(),
            "toggling grid-matching must rebuild the volume on re-import"
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

    #[test]
    fn synth_radar_named_vcp_state_round_trips_and_owns_the_scan_ladder() {
        use crate::wrf_radar::SyntheticScanStrategy;

        let state = SyntheticRadarUiState {
            scan_strategy: SyntheticScanStrategy::Build24Vcp112,
            // These custom-only settings stay persisted for when the user
            // returns to Custom, but they do not alter a named row plan.
            include_low_tilt: true,
            rotation_rate_deg_s: 3.0,
            transition_delay_s: 29.0,
            prf_hz: 4_900.0,
            ..SyntheticRadarUiState::default()
        };
        let json = serde_json::to_value(&state).unwrap();
        let restored: SyntheticRadarUiState = serde_json::from_value(json).unwrap();
        assert_eq!(restored, state);

        let config = restored.to_config().unwrap();
        let definition = SyntheticScanStrategy::Build24Vcp112.definition().unwrap();
        assert_eq!(config.scan_strategy, SyntheticScanStrategy::Build24Vcp112);
        assert_eq!(config.physical_scan_legs().len(), definition.rows.len());
        assert_eq!(
            config.elevations_deg,
            definition
                .elevation_ladder_deg()
                .into_iter()
                .map(f64::from)
                .collect::<Vec<_>>()
        );
        assert_ne!(config.elevations_deg[0], crate::wrf_radar::LOW_TILT_DEG);
        assert!(config.scan_strategy.is_named_vcp());
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

    #[test]
    fn cm1_radar_ignores_stale_wrf_site_and_uses_placed_domain_center() {
        let state = SyntheticRadarUiState {
            placement: SynthPlacement::NexradSite,
            // Deliberately invalid: a hidden WRF-only choice must neither
            // block CM1 nor leak an out-of-domain antenna into its scan.
            site_id_text: "NOT-A-SITE".to_owned(),
            max_range_km: 80.0,
            ..SyntheticRadarUiState::default()
        };
        assert!(
            state.to_config().is_err(),
            "ordinary WRF launch stays strict"
        );

        let config = state.to_cm1_config().expect("CM1 owns centered placement");
        assert_eq!(config.site_id, "CM1");
        assert_eq!(config.site_lat_deg, None);
        assert_eq!(config.site_lon_deg, None);
        assert_eq!(
            config.site_name.as_deref(),
            Some("Simulated CM1 radar at placed domain centre")
        );
        assert_eq!(config.max_range_m, 80_000.0);
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
            exact_time: None,
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
