//! Native "synthetic radar from WRF" — turn an ingested WRF forecast hour
//! into a [`radar_core::RadarVolume`] of SIMULATED reflectivity (and radial
//! velocity) sampled onto a polar range/azimuth/elevation grid, so the model
//! output renders and LOOPS through the existing radar viewer (colormaps,
//! cross-sections, GBVTD, loop engine) with no new file format.
//!
//! The scan is virtual: a synthetic NEXRAD-like antenna is placed over the WRF
//! domain and, for every (elevation, azimuth, range) gate, the beam centre is
//! traced through the 4/3-earth model to a model-space (lat, lon, MSL height),
//! where the model's 3-D reflectivity and earth-relative winds are
//! trilinearly sampled. The result is stored as true `f32` dBZ / m·s⁻¹
//! (`MomentStorage::F32`, scale 1, offset 0) so the render/dealias/GBVTD F32
//! paths and the standard REF/VEL colour tables consume it unchanged.
//!
//! Physics / algorithm references:
//! - Beam geometry (height + ground range under the 4/3-earth effective-radius
//!   refraction model): Doviak & Zrnić (1993), *Doppler Radar and Weather
//!   Observations* (2nd ed.), eq. 2.28b/c — via
//!   [`radar_core::beam_height_above_radar_m`] /
//!   [`radar_core::beam_ground_range_m`].
//! - Simulated reflectivity Z from hydrometeor mixing ratios: Stoelinga (2005),
//!   "Simulated equivalent reflectivity factor as currently formulated in the
//!   WRF model" (WRF microphysics tech note); Thompson et al. (2008), *Mon.
//!   Wea. Rev.* 136, 5095–5115 (variable-intercept option) — computed inside
//!   `wrf-core`'s `dbz` (`CALCDBZ`) diagnostic, or read directly from the
//!   model's own `REFL_10CM` field when present.
//! - Radial velocity as the projection of the 3-D wind onto the beam unit
//!   vector: Sun & Crook (1997), *J. Atmos. Sci.* 54, 1642–1661 (radar radial
//!   velocity in a variational/forward-operator context).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, RadarSite, RadarVolume, Radial,
    ScanLegMetadata, ScanMode, VcpInfo, VolumeMetadata, beam_ground_range_m,
    beam_height_above_radar_m,
};
use rayon::prelude::*;
use wrf_core::{ComputeOpts, WrfFile, getvar};

use crate::model_layer::{
    InverseLut, neighboring_cell_starts, solve_bilinear_coords, unwrap_lon_near,
};
use crate::wrf_radar_estimator::{
    CustomSinglePrf, IdealMoments, MatchedFilterRangeResponse, MeasuredMoments,
    MomentEstimatorConfig, NoiseKey, PresentationConfig, PresentedMoments, PrfSpecification,
    RadarInstrument, RadarMomentValues, ResolvedEstimatorSampling, ResolvedSinglePrf,
    estimate_measured_moments, present_measured_moments, resolve_estimator_sampling, resolve_prf,
};
use ui_core::geo::aeqd_inverse_km;

use app_ui::vcp_catalog::{
    BUILD_24_SOURCE, Build24Vcp, DopplerPrfValue, MomentCoverage, PhysicalScanRow, PulseLength,
    VcpDefinition, Waveform, build_24_definition,
};
use app_ui::wrf_radar_validation::{
    CompactPolarPrecisionAudit, ExactScanTemplate, GateQuality, GateQualityFractions,
    QualityMoment, UnavailableObservedMoment, build_difference_volume_overlap,
    compact_quality_grid, encode_quality_fraction, relative_error,
};
use app_ui::wrf_scene_adapter::inventory_selected_wrf_paths;
use app_ui::wrf_scene_inventory::{WrfScene, WrfSceneGroup, WrfSceneTime, WrfSourceIdentity};
use app_ui::wrf_temporal::{
    AtmosphereTimeMode, HoldReason, MissingNeighborPolicy, RawGateState, RawStateLinearEndpoint,
    TemporalMemoryEstimate, TemporalSamplingOutcome, TemporalScenePlan, TwoSceneCache,
    interpolate_raw_state_linear, plan_for_scene,
};

/// Operational adaptations intentionally outside the checked base-pattern
/// catalog and this synthetic renderer.
pub const BUILD_24_NO_ADAPTATIONS_CAVEAT: &str = "Base pattern only: SAILS, MRLE, AVSET, Add-MPDA, and site-specific low-tilt adaptations are absent.";

/// Default WSR-88D-like elevation ladder (deg). Covers the low tilts that
/// dominate a plan-view display plus enough high tilts for cross-sections.
pub const DEFAULT_ELEVATIONS_DEG: &[f64] = &[
    0.5, 0.9, 1.3, 1.8, 2.4, 3.1, 4.0, 5.1, 6.4, 8.0, 10.0, 12.5, 15.6, 19.5,
];

/// Optional sub-0.5° tilt (deg) prepended below [`DEFAULT_ELEVATIONS_DEG`] when
/// the user opts in. The community wrf-python/GR2 exports start their ladder
/// here; the 0.1° beam samples roughly half the height of our standard 0.5°
/// lowest tilt at range, so a hook echo is better defined near the ground.
pub const LOW_TILT_DEG: f64 = 0.1;

/// Build the elevation ladder for a synthetic scan. With `include_low_tilt`
/// the [`LOW_TILT_DEG`] tilt is prepended below the standard
/// [`DEFAULT_ELEVATIONS_DEG`] ladder; otherwise the standard ladder is
/// returned unchanged (bit-identical to the historical default).
pub fn elevation_ladder(include_low_tilt: bool) -> Vec<f64> {
    if include_low_tilt {
        std::iter::once(LOW_TILT_DEG)
            .chain(DEFAULT_ELEVATIONS_DEG.iter().copied())
            .collect()
    } else {
        DEFAULT_ELEVATIONS_DEG.to_vec()
    }
}

/// Reflectivity source label stamped when the model's own Thompson `REFL_10CM`
/// field is used directly.
pub const REFL_10CM_SOURCE: &str = "REFL_10CM";
/// Reflectivity source label stamped when the model-native operator falls back
/// to the computed `dbz` because the file carries no `REFL_10CM`.
pub const CALCDBZ_SOURCE: &str = "dbz/CALCDBZ";
/// Reflectivity source label stamped when the classic Stoelinga operator is
/// chosen, forcing the computed `dbz` (CALCDBZ) even when the file carries
/// `REFL_10CM`.
pub const STOELINGA_SOURCE: &str = "dbz/CALCDBZ (Stoelinga)";
/// Reflectivity source stamped when raw WRF hydrometeors feed the internally
/// consistent bulk S-band polarimetric scattering state.
pub const BULK_S_BAND_SOURCE: &str = "scheme-aware bulk S-band ZH (Rayleigh v1)";
pub const PROPERTY_TMATRIX_S_BAND_SOURCE: &str =
    "P3/ISHMAEL property-aware S-band T-matrix ZH (research v1)";

/// Which reflectivity operator a synthetic scan uses. Both diagnostics are
/// legitimate: `REFL_10CM` is the model's own Thompson-native 10-cm
/// reflectivity (hotter/fatter in graupel/big-drop/melting regions of a
/// supercell), while the classic Stoelinga `dbz` (wrf-core `CALCDBZ`,
/// fixed Marshall-Palmer intercepts) is what the community wrf-python/GR2
/// pipeline renders. The choice is a per-import operator difference of roughly
/// +10..+20 dB in those regions — not a rendering artifact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectivityOperator {
    /// Prefer the model's own `REFL_10CM` when the file carries it, else fall
    /// back to the computed `dbz` (`CALCDBZ`). The historical default.
    #[default]
    ModelNative,
    /// Always compute `dbz` (wrf-core `CALCDBZ`, Stoelinga 2005 fixed
    /// intercepts) — the community wrf-python/GR2 look — even when the file
    /// carries `REFL_10CM`.
    ClassicStoelinga,
}

/// Domain used while interpolating model reflectivity onto a radar pulse.
/// `LinearZ` is the physically meaningful received-power average; `LegacyDbz`
/// remains available so old presentation renders can be reproduced exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectivitySampling {
    LegacyDbz,
    #[default]
    LinearZ,
}

/// Polarimetric scattering implementation selected for synthetic radar.
///
/// The T-matrix variant is intentionally explicit about its evidence status:
/// its bundled tables are reproducible research assets, not independently
/// validated operational calibration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolarimetricKernel {
    #[default]
    BulkRayleighV1,
    PropertyTMatrixResearchV1,
}

/// Deterministic antenna/pulse quadrature. The balanced rule is a symmetric
/// nine-point cubature; reference is the full 3x3x3 tensor rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeamIntegration {
    #[default]
    Center,
    Balanced,
    Reference,
}

/// User intent for a synthetic volume. Fine-grained controls remain editable,
/// but named modes make it explicit whether a volume is model truth, a virtual
/// instrument measurement, or a display-oriented presentation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulationMode {
    Truth,
    Instrument,
    #[default]
    Presentation,
}

/// Whether all rays represent one model instant or carry acquisition times for
/// a rotating volume scan. Atmosphere sampling is configured independently by
/// [`SyntheticRadarConfig::atmosphere_time_mode`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanTiming {
    #[default]
    InstantaneousTruth,
    TimedVolume,
}

/// Scan strategy used by the WRF synthetic-radar forward operator.
///
/// `CustomLegacy` is deliberately the serde/default variant: settings written
/// before the Build 24 catalog existed, and programmatic callers that use
/// [`SyntheticRadarConfig::default`], retain the historical fourteen-cut
/// ladder and every custom timing/PRF knob exactly as before.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntheticScanStrategy {
    #[default]
    CustomLegacy,
    Build24Vcp12,
    Build24Vcp34,
    Build24Vcp35,
    Build24Vcp112,
    Build24Vcp212,
    Build24Vcp215,
}

impl SyntheticScanStrategy {
    pub const BUILD_24: [Self; 6] = [
        Self::Build24Vcp12,
        Self::Build24Vcp34,
        Self::Build24Vcp35,
        Self::Build24Vcp112,
        Self::Build24Vcp212,
        Self::Build24Vcp215,
    ];

    pub const fn vcp(self) -> Option<Build24Vcp> {
        match self {
            Self::CustomLegacy => None,
            Self::Build24Vcp12 => Some(Build24Vcp::Vcp12),
            Self::Build24Vcp34 => Some(Build24Vcp::Vcp34),
            Self::Build24Vcp35 => Some(Build24Vcp::Vcp35),
            Self::Build24Vcp112 => Some(Build24Vcp::Vcp112),
            Self::Build24Vcp212 => Some(Build24Vcp::Vcp212),
            Self::Build24Vcp215 => Some(Build24Vcp::Vcp215),
        }
    }

    pub fn definition(self) -> Option<&'static VcpDefinition> {
        build_24_definition(self.vcp()?.number())
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::CustomLegacy => "Custom legacy ladder",
            Self::Build24Vcp12 => "Build 24 VCP 12",
            Self::Build24Vcp34 => "Build 24 VCP 34",
            Self::Build24Vcp35 => "Build 24 VCP 35",
            Self::Build24Vcp112 => "Build 24 VCP 112",
            Self::Build24Vcp212 => "Build 24 VCP 212",
            Self::Build24Vcp215 => "Build 24 VCP 215",
        }
    }

    pub const fn is_named_vcp(self) -> bool {
        !matches!(self, Self::CustomLegacy)
    }
}

/// One physical antenna rotation to sample. Named VCPs produce one leg for
/// every catalog row, including equal-elevation split cuts. The legacy custom
/// mode produces one all-moment leg per configured elevation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntheticScanLeg {
    pub elevation_deg: f64,
    pub azimuth_rate_deg_per_second: f32,
    pub source_period_seconds: f32,
    pub transition_after_seconds: f32,
    pub moments: MomentCoverage,
    pub waveform: Option<Waveform>,
    pub source_row_index: Option<usize>,
    pub source_row: Option<&'static PhysicalScanRow>,
}

impl ReflectivityOperator {
    /// Whether to prefer the model's own `REFL_10CM` when the file carries it.
    /// Only the model-native operator does; Stoelinga always recomputes.
    pub fn prefers_refl_10cm(self) -> bool {
        matches!(self, Self::ModelNative)
    }

    /// The source label stamped when this operator uses the computed `dbz`
    /// (`CALCDBZ`) path — distinguishing a deliberate Stoelinga choice from a
    /// model-native fallback.
    pub fn computed_dbz_source(self) -> &'static str {
        match self {
            Self::ModelNative => CALCDBZ_SOURCE,
            Self::ClassicStoelinga => STOELINGA_SOURCE,
        }
    }
}

/// The reflectivity source label a synthetic scan will stamp, given the
/// operator choice and whether the file carries `REFL_10CM`. Assumes the
/// `REFL_10CM` read succeeds when attempted; a failed/short read falls through
/// to the computed `dbz`, mirrored exactly by [`read_reflectivity`]. This is
/// the pure decision the reflectivity read makes, factored out so both operator
/// branches are testable without a WRF file on disk.
pub fn planned_ref_source(
    operator: ReflectivityOperator,
    file_has_refl_10cm: bool,
) -> &'static str {
    if operator.prefers_refl_10cm() && file_has_refl_10cm {
        REFL_10CM_SOURCE
    } else {
        operator.computed_dbz_source()
    }
}

/// Nyquist velocity (m/s) stamped on every radial when velocity folding is OFF
/// — deliberately large so the native, forward-modelled Vr is treated as
/// already-unfolded TRUE velocity by the dealias/readout code (it never folds).
/// This is the historical stamp the community WRF→GR2 script also implied by
/// leaving the data unfolded.
pub const UNFOLDED_NYQUIST_MPS: f32 = 320.0;

/// Default FOLDING Nyquist (m/s) when the realistic-Nyquist option is enabled —
/// a typical WSR-88D low-PRF Doppler Nyquist. Real VELocity Nyquists cluster
/// around here (roughly 8–33 m/s depending on VCP/PRF).
pub const DEFAULT_FOLD_NYQUIST_MPS: f32 = 25.0;

/// Exact transmit frequency represented by the v0.33.1 property-aware
/// research T-matrix tables.
pub const PROPERTY_TMATRIX_RESEARCH_FREQUENCY_MHZ: u32 = 2_800;
/// View-angle applicability includes every Build-24/optional-cut center plus
/// the default one-degree beam's ±sigma pulse-volume quadrature offsets.
pub const PROPERTY_TMATRIX_MIN_VIEW_ELEVATION_DEG: f64 = -0.5;
pub const PROPERTY_TMATRIX_MAX_VIEW_ELEVATION_DEG: f64 = 20.0;

#[inline]
fn gaussian_beam_sigma_deg(fwhm_deg: f64) -> f64 {
    fwhm_deg / (2.0 * (2.0 * std::f64::consts::LN_2).sqrt())
}

/// Configuration for one synthetic scan.
#[derive(Clone, Debug)]
pub struct SyntheticRadarConfig {
    /// Named operating intent. This is also stamped in export provenance.
    pub simulation_mode: SimulationMode,
    /// Physical scan definition. The historical custom ladder remains the
    /// default; a Build 24 selection owns its rows, rates, periods, waveform,
    /// moment coverage and PRF-code provenance.
    pub scan_strategy: SyntheticScanStrategy,
    /// Exact observed acquisition geometry for Level-II replay. This preserves
    /// source cuts/rays/gates/times/moment availability verbatim and takes
    /// precedence over `scan_strategy`; it is not a VCP reconstruction.
    pub exact_replay_template: Option<Arc<ExactScanTemplate>>,
    /// Site id stamped on the volume (drives the loop-engine cross-site guard —
    /// every hour of one run must share it).
    pub site_id: String,
    pub site_name: Option<String>,
    /// Antenna position. `None` places it at the WRF domain centre.
    pub site_lat_deg: Option<f64>,
    pub site_lon_deg: Option<f64>,
    /// Antenna altitude MSL (m). `None` uses the model terrain at the site plus
    /// [`DEFAULT_TOWER_M`].
    pub antenna_msl_m: Option<f64>,
    pub elevations_deg: Vec<f64>,
    /// Azimuth samples per sweep, clockwise from north (e.g. 360 → 1.0°, 720 →
    /// 0.5°).
    pub azimuth_count: usize,
    pub gate_spacing_m: f64,
    pub max_range_m: f64,
    /// Set the effective gate spacing equal to the WRF grid resolution (the
    /// file's `DX` global attribute, metres) instead of [`Self::gate_spacing_m`],
    /// so a coarse grid is not oversampled — 250 m gates on a 3 km grid imply
    /// ~12 redundant gates per model cell, more resolution than the model has.
    /// Resolved per file at build time by [`effective_gate_spacing`] and clamped
    /// to `[GRID_GATE_MIN_M, GRID_GATE_MAX_M]` so a garbage `DX` cannot produce an
    /// absurd gate count; a file with a missing/invalid `DX` falls back to
    /// [`Self::gate_spacing_m`]. OFF by default — an OFF build uses the configured
    /// gate spacing exactly as before (bit-identical to a build without this).
    pub match_gate_to_grid: bool,
    /// Reflectivity floor (dBZ): gates below this — and their velocity — are
    /// left NaN so clear air renders transparent, like a real scope.
    pub ref_floor_dbz: f32,
    /// The FOLDING Nyquist velocity (m/s). Used ONLY when [`Self::fold_velocity`]
    /// is set: each gate's Vr is aliased into `[-nyquist_mps, +nyquist_mps)` and
    /// this value is stamped on every radial, so the dealiaser/readout see the
    /// real velocity ambiguity. When folding is OFF the historical
    /// [`UNFOLDED_NYQUIST_MPS`] (320) is stamped instead (via
    /// [`Self::stamped_nyquist_mps`]), so the native forward-modelled Vr reads as
    /// already-unfolded TRUE velocity that never folds. Default
    /// [`DEFAULT_FOLD_NYQUIST_MPS`].
    pub nyquist_mps: f32,
    /// Fold the forward-modelled radial velocity like a real pulse-pair radar:
    /// alias every velocity gate into the [`Self::nyquist_mps`] co-interval so VEL
    /// FOLDS (and the velocity dealiaser has genuine work — a practice ground on
    /// known ground truth). OFF by default — the native Vr is the exact TRUE wind
    /// projection and [`UNFOLDED_NYQUIST_MPS`] is stamped, so an OFF build is
    /// bit-identical to a build without the feature.
    pub fold_velocity: bool,
    /// Which reflectivity operator populates the sampled `dbz`: the model's own
    /// Thompson-native `REFL_10CM` ([`ReflectivityOperator::ModelNative`]) or the
    /// classic Stoelinga `CALCDBZ` community diagnostic
    /// ([`ReflectivityOperator::ClassicStoelinga`]).
    pub reflectivity_operator: ReflectivityOperator,
    /// Interpolate/average received power in linear Z (scientific default) or
    /// reproduce the historical direct-dBZ interpolation.
    pub reflectivity_sampling: ReflectivitySampling,
    /// Antenna/pulse-volume integration tier.
    pub beam_integration: BeamIntegration,
    /// Horizontal/vertical one-way 3-dB beam width (degrees).
    pub beam_width_deg: f32,
    /// Transmitted pulse duration (microseconds).
    pub pulse_width_us: f32,
    /// Radar transmit frequency. 2.8 GHz is a representative S-band system.
    pub radar_frequency_mhz: u32,
    /// Use hydrometeor/scatterer fall speed in the Doppler projection.
    pub terminal_fall_speed: bool,
    /// Apply cumulative terrain-horizon partial beam blockage.
    pub terrain_blockage: bool,
    /// Emit Doppler spectrum width from pulse-volume velocity variance, model
    /// TKE when available, terminal-speed diversity, and the instrument floor.
    pub spectrum_width: bool,
    pub spectrum_width_floor_mps: f32,
    /// Build scheme-aware S-band ZH/ZV/covariance/KDP/attenuation from raw WRF
    /// hydrometeors. Unsupported schemes fall back to scalar REF/VEL with an
    /// explicit note instead of fabricating polarimetric fields.
    pub dual_pol: bool,
    /// Scattering kernel behind dual-pol fields. Research T-matrix mode is
    /// fail-closed when the file/category/table applicability contract does
    /// not match exactly; it never silently falls back to Rayleigh.
    pub polarimetric_kernel: PolarimetricKernel,
    /// Integrate KDP/Ah/Av along each radial into PhiDP and attenuation.
    pub propagation: bool,
    /// Optional synthetic-system calibration offsets.
    pub system_phidp_deg: f32,
    pub zdr_bias_db: f32,
    /// Acquisition timing and nominal instrument cadence.
    pub scan_timing: ScanTiming,
    /// Sample every ray from the anchor WRF scene or interpolate its atmosphere
    /// from the adjacent compatible model scene at the ray acquisition time.
    pub atmosphere_time_mode: AtmosphereTimeMode,
    /// Explicit behavior when a later compatible WRF scene is unavailable or
    /// cannot cover the complete scan without extrapolation.
    pub missing_neighbor_policy: MissingNeighborPolicy,
    /// Peak-memory ceiling for a rolling two-scene temporal build. The worker
    /// preflights before reading a second scene and never silently exceeds it.
    pub temporal_memory_budget_mib: usize,
    pub rotation_rate_deg_s: f32,
    pub transition_delay_s: f32,
    /// Pulse repetition frequency used for CfRadial metadata. Velocity folding
    /// remains controlled independently by `nyquist_mps`.
    pub prf_hz: f32,
    /// Opt into the physically coupled custom single-PRF instrument. When on,
    /// exact frequency + PRF jointly determine wavelength, Nyquist and
    /// unambiguous range; the moment estimator owns sensitivity, uncertainty,
    /// bias and folding. Named VCP PRF codes fail closed because they are not
    /// frequencies. Off preserves the v0.33.1 path bit-for-bit.
    pub coupled_single_prf_estimator: bool,
    /// Nominal dwell used by the coupled moment estimator.
    pub estimator_dwell_ms: f32,
    /// Optional authoritative transmitted-pulse count. `None` derives
    /// `floor(dwell * PRF)`.
    pub estimator_pulse_count: Option<u32>,
    /// Fraction of transmitted pulses treated as statistically independent.
    pub estimator_independent_sample_fraction: f32,
    /// Minimum received SNR admitted by the coupled estimator.
    pub estimator_minimum_snr_db: f32,
    /// Emit opt-in f32 Ideal (`I*`) and Measured (`M*`) diagnostic fields for
    /// REF/VEL/SW/ZDR/rhoHV/KDP. Canonical fields remain Presented.
    pub emit_stage_diagnostics: bool,
    /// Range-dependent sensitivity/noise model. The threshold follows the
    /// radar equation from this 1-km dBZ reference when enabled.
    pub instrument_noise: bool,
    pub sensitivity_dbz_at_1km: f32,
    /// "Gate texture" on REFLECTIVITY: deterministic, range-correlated speckle
    /// added to the sampled dBZ so the synthetic gates read like real Level-II
    /// texture instead of a perfectly smooth trilinear field. ON is the product
    /// default (owner: the smooth field "looks garbage without" it). When OFF,
    /// the reflectivity perturbation code never touches a value, so an OFF build
    /// is bit-identical to a build without the feature.
    pub ref_gate_texture: bool,
    /// "Gate texture" on VELOCITY: a gentle ±0.5 m/s wobble on the forward-
    /// modelled radial velocity. OFF by default and kept opt-in — the clean Vr
    /// feeds the velocity dealias / GBVTD consumers downstream, and a noisy Vr
    /// would pollute them. Separate from [`Self::ref_gate_texture`] on purpose:
    /// reflectivity wants the speckle, velocity does not.
    pub vel_gate_texture: bool,
    /// Ground-clutter amount, 0.0..=1.0. Our forward operator is pure physics
    /// and produces ZERO clutter; this dials in a fabricated near-radar
    /// ground-return look modelled on the community WRF→GR2 export script
    /// (`add_ground_clutter`). `0.0` (the default) skips the clutter path
    /// entirely, so the output is bit-identical to a build without the feature;
    /// `1.0` ≈ the community-script intensity. Intermediate values scale the
    /// clutter *probability* and *brightness* together. The pattern is
    /// deterministic per forecast frame (seeded from site id + valid time +
    /// tilt) so a loop never shimmers between rebuilds of the same hour. See the
    /// ground-clutter section (`ground_clutter_dbz`) for the model.
    pub clutter_intensity: f32,
    /// Emit compact per-gate pulse-volume support diagnostics (MCOV, TUNB,
    /// MSIG). These never alter the physical samples and use three bytes per
    /// gate in the finished volume.
    pub emit_quality_fields: bool,
    /// Mask physical moments where less than this fraction of the configured
    /// quadrature support is covered by the model domain. Quality grids remain
    /// present so a masked gate can still explain why it was rejected.
    pub minimum_model_coverage_fraction: f32,
}

/// Antenna height above model terrain when no explicit MSL altitude is given.
pub const DEFAULT_TOWER_M: f64 = 10.0;

impl Default for SyntheticRadarConfig {
    fn default() -> Self {
        Self {
            simulation_mode: SimulationMode::Presentation,
            scan_strategy: SyntheticScanStrategy::CustomLegacy,
            exact_replay_template: None,
            site_id: "WRF".to_string(),
            site_name: Some("Simulated WRF radar".to_string()),
            site_lat_deg: None,
            site_lon_deg: None,
            antenna_msl_m: None,
            elevations_deg: DEFAULT_ELEVATIONS_DEG.to_vec(),
            azimuth_count: 720,
            gate_spacing_m: 250.0,
            max_range_m: 230_000.0,
            // Off by default: the effective gate spacing is the configured
            // `gate_spacing_m`, so the default build is bit-identical to a build
            // without the grid-matching feature.
            match_gate_to_grid: false,
            ref_floor_dbz: 0.0,
            // The folding Nyquist (used only when `fold_velocity` is set). With
            // folding OFF (the default) `UNFOLDED_NYQUIST_MPS` (320) is stamped
            // and no gate folds, so the default build is bit-identical to before.
            nyquist_mps: DEFAULT_FOLD_NYQUIST_MPS,
            fold_velocity: false,
            // Default operator. Flip this one line to ClassicStoelinga to
            // re-default the community look after the owner's comparison.
            reflectivity_operator: ReflectivityOperator::ModelNative,
            reflectivity_sampling: ReflectivitySampling::LinearZ,
            beam_integration: BeamIntegration::Center,
            beam_width_deg: 0.95,
            pulse_width_us: 1.57,
            radar_frequency_mhz: 2_800,
            terminal_fall_speed: false,
            terrain_blockage: false,
            spectrum_width: false,
            spectrum_width_floor_mps: 0.5,
            dual_pol: false,
            polarimetric_kernel: PolarimetricKernel::BulkRayleighV1,
            propagation: false,
            system_phidp_deg: 0.0,
            zdr_bias_db: 0.0,
            scan_timing: ScanTiming::InstantaneousTruth,
            atmosphere_time_mode: AtmosphereTimeMode::FrozenAtVolumeStart,
            missing_neighbor_policy: MissingNeighborPolicy::HoldAnchor,
            temporal_memory_budget_mib: 8_192,
            rotation_rate_deg_s: 18.0,
            transition_delay_s: 3.5,
            prf_hz: 1_000.0,
            coupled_single_prf_estimator: false,
            estimator_dwell_ms: 50.0,
            estimator_pulse_count: None,
            estimator_independent_sample_fraction: 0.5,
            estimator_minimum_snr_db: 0.0,
            emit_stage_diagnostics: false,
            instrument_noise: false,
            sensitivity_dbz_at_1km: -40.0,
            // Reflectivity texture ON by default (owner verdict: a smooth
            // simulated field "looks garbage without" it); velocity texture
            // OFF so the clean Vr keeps feeding dealias/GBVTD.
            ref_gate_texture: true,
            vel_gate_texture: false,
            // No clutter by default: the operator stays pure physics and the
            // output is bit-identical to a build without the feature.
            clutter_intensity: 0.0,
            emit_quality_fields: true,
            minimum_model_coverage_fraction: 0.0,
        }
    }
}

impl SyntheticRadarConfig {
    /// Resolve configuration into physical antenna rotations before any model
    /// sampling begins. This is intentionally separate from the renderer loop:
    /// duplicate split cuts must remain duplicate cuts, not be collapsed into
    /// a unique elevation ladder.
    pub fn physical_scan_legs(&self) -> Vec<SyntheticScanLeg> {
        if let Some(definition) = self.scan_strategy.definition() {
            return definition
                .rows
                .iter()
                .enumerate()
                .map(|(source_row_index, row)| SyntheticScanLeg {
                    elevation_deg: f64::from(row.elevation_deg),
                    azimuth_rate_deg_per_second: row.azimuth_rate_deg_per_second,
                    source_period_seconds: row.source_period_seconds,
                    transition_after_seconds: 0.0,
                    moments: row.moments,
                    waveform: Some(row.waveform),
                    source_row_index: Some(source_row_index),
                    source_row: Some(row),
                })
                .collect();
        }

        let rate = self.rotation_rate_deg_s.max(0.1);
        self.elevations_deg
            .iter()
            .copied()
            .map(|elevation_deg| SyntheticScanLeg {
                elevation_deg,
                azimuth_rate_deg_per_second: rate,
                source_period_seconds: 360.0 / rate,
                transition_after_seconds: self.transition_delay_s.max(0.0),
                moments: MomentCoverage::ALL,
                waveform: None,
                source_row_index: None,
                source_row: None,
            })
            .collect()
    }

    /// Reject a research-table request before decoding a multi-gigabyte WRF
    /// scene. These are exact applicability checks, not automatic clamps or
    /// fallback rules.
    pub fn validate_science_contract(&self) -> Result<(), String> {
        if self.exact_replay_template.is_some() && self.coupled_single_prf_estimator {
            return Err(
                "exact observed replay cannot use custom coupled-PRF timing; the source radial Nyquist metadata owns replay ambiguity"
                    .to_string(),
            );
        }
        if self.emit_stage_diagnostics && !self.coupled_single_prf_estimator {
            return Err(
                "Ideal/Measured stage diagnostics require the coupled single-PRF estimator"
                    .to_string(),
            );
        }
        if self.coupled_single_prf_estimator {
            validate_coupled_estimator_inputs(self)?;
        }
        let property_tmatrix = matches!(
            self.polarimetric_kernel,
            PolarimetricKernel::PropertyTMatrixResearchV1
        );
        if matches!(
            self.atmosphere_time_mode,
            AtmosphereTimeMode::RawStateLinear
        ) && !property_tmatrix
        {
            return Err(
                "RawStateLinear atmosphere timing is available only with the P3/ISHMAEL property T-matrix kernel"
                    .to_string(),
            );
        }
        if !property_tmatrix {
            return Ok(());
        }
        if !self.dual_pol {
            return Err(
                "Property T-matrix research mode requires S-band dual polarization".to_string(),
            );
        }
        if self.radar_frequency_mhz != PROPERTY_TMATRIX_RESEARCH_FREQUENCY_MHZ {
            return Err(format!(
                "Property T-matrix research mode requires exactly {} MHz, got {} MHz",
                PROPERTY_TMATRIX_RESEARCH_FREQUENCY_MHZ, self.radar_frequency_mhz,
            ));
        }
        if !matches!(self.reflectivity_sampling, ReflectivitySampling::LinearZ) {
            return Err("Property T-matrix research mode requires linear-Z sampling".to_string());
        }
        let beam_width_deg = f64::from(self.beam_width_deg);
        if !beam_width_deg.is_finite() || beam_width_deg <= 0.0 {
            return Err(format!(
                "Property T-matrix beam width must be finite and positive, got {beam_width_deg}°"
            ));
        }
        let legs = self.physical_scan_legs();
        if legs.is_empty() {
            return Err("Property T-matrix scan has no physical cuts".to_string());
        }
        // Match the exact lower bound used by sample_gate so validation and
        // runtime can never disagree at a table-elevation boundary.
        let effective_beam_width_deg = beam_width_deg.max(0.05);
        let beam_sigma_deg = gaussian_beam_sigma_deg(effective_beam_width_deg);
        for leg in legs {
            for point in quadrature_points(self.beam_integration) {
                let view_elevation_deg = leg.elevation_deg + point.el_sigma * beam_sigma_deg;
                if !(PROPERTY_TMATRIX_MIN_VIEW_ELEVATION_DEG
                    ..=PROPERTY_TMATRIX_MAX_VIEW_ELEVATION_DEG)
                    .contains(&view_elevation_deg)
                {
                    return Err(format!(
                        "Property T-matrix view elevation {view_elevation_deg:.3}° for the {:.3}° cut is outside the exact [{:.1}°, {:.1}°] table range; reduce beam width/change cuts or use Bulk Rayleigh",
                        leg.elevation_deg,
                        PROPERTY_TMATRIX_MIN_VIEW_ELEVATION_DEG,
                        PROPERTY_TMATRIX_MAX_VIEW_ELEVATION_DEG,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Apply a coherent named preset while leaving site/range/geometry choices
    /// alone. The UI calls this only when the user selects a different mode;
    /// subsequent expert edits are intentionally preserved.
    pub fn apply_mode_preset(&mut self, mode: SimulationMode) {
        self.simulation_mode = mode;
        match mode {
            SimulationMode::Truth => {
                self.reflectivity_sampling = ReflectivitySampling::LinearZ;
                self.beam_integration = BeamIntegration::Center;
                self.terminal_fall_speed = false;
                self.terrain_blockage = false;
                self.spectrum_width = false;
                self.dual_pol = false;
                self.polarimetric_kernel = PolarimetricKernel::BulkRayleighV1;
                self.propagation = false;
                self.scan_timing = ScanTiming::InstantaneousTruth;
                self.atmosphere_time_mode = AtmosphereTimeMode::FrozenAtVolumeStart;
                self.instrument_noise = false;
                self.ref_gate_texture = false;
                self.vel_gate_texture = false;
                self.clutter_intensity = 0.0;
                self.fold_velocity = false;
            }
            SimulationMode::Instrument => {
                self.reflectivity_sampling = ReflectivitySampling::LinearZ;
                self.beam_integration = BeamIntegration::Balanced;
                self.terminal_fall_speed = true;
                self.terrain_blockage = true;
                self.spectrum_width = true;
                self.dual_pol = true;
                self.polarimetric_kernel = PolarimetricKernel::BulkRayleighV1;
                self.propagation = true;
                self.scan_timing = ScanTiming::TimedVolume;
                self.atmosphere_time_mode = AtmosphereTimeMode::LinearAdjacent;
                self.instrument_noise = true;
                self.ref_gate_texture = false;
                self.vel_gate_texture = false;
                self.clutter_intensity = 0.0;
                self.fold_velocity = true;
            }
            SimulationMode::Presentation => {
                self.reflectivity_sampling = ReflectivitySampling::LinearZ;
                self.beam_integration = BeamIntegration::Center;
                self.terminal_fall_speed = false;
                self.terrain_blockage = false;
                self.spectrum_width = false;
                self.dual_pol = false;
                self.polarimetric_kernel = PolarimetricKernel::BulkRayleighV1;
                self.propagation = false;
                self.scan_timing = ScanTiming::InstantaneousTruth;
                self.atmosphere_time_mode = AtmosphereTimeMode::FrozenAtVolumeStart;
                self.instrument_noise = false;
                self.ref_gate_texture = true;
                self.vel_gate_texture = false;
                self.fold_velocity = false;
            }
        }
    }

    /// A 64-bit fingerprint of every field that changes the SAMPLED DATA of a
    /// synthetic scan. The loop-engine dedupe keys a re-import on scan time +
    /// site id (see [`crate`]'s install path); that alone cannot tell a
    /// genuinely re-configured build from an identical rebuild, so the install
    /// path folds THIS fingerprint into the per-frame history path key. An
    /// UNCHANGED config re-imports to the same key and the engine reuses the
    /// stored volume (upsert rule (b)); ANY change here yields a new key, so the
    /// freshly-built volume replaces the stale one (rule (c), equal status +
    /// different path) and flows to the display.
    ///
    /// Included — everything that alters a gate value or its position: the
    /// antenna placement (`site_id`, `site_lat_deg`, `site_lon_deg`,
    /// `antenna_msl_m`), the elevation ladder (which already encodes the
    /// optional 0.1° low tilt), `azimuth_count`, `gate_spacing_m`, `max_range_m`,
    /// `match_gate_to_grid` (the BOOL, not the resolved spacing: the effective
    /// spacing is derived from the file's `DX` at build time and the file is fixed
    /// for a run, so the same file + bool always resolves to the same spacing —
    /// hashing the bool is sufficient to force a rebuild when it toggles),
    /// `ref_floor_dbz`, `nyquist_mps`, `fold_velocity` (toggling folding
    /// re-aliases every velocity gate and re-stamps the Nyquist, so it must
    /// rebuild), `reflectivity_operator`, both gate-texture flags, and
    /// `clutter_intensity` (so moving the clutter slider rebuilds and replaces
    /// the stale volume on re-import). Deliberately EXCLUDED: `site_name` (a
    /// label — never changes a sample).
    pub fn data_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.site_id.hash(&mut hasher);
        if let Some(template) = self.exact_replay_template.as_deref() {
            "exact-observed-replay-v1".hash(&mut hasher);
            template.geometry_fingerprint().hash(&mut hasher);
        }
        // Do not hash the CustomLegacy discriminant: omitting that no-op value
        // preserves the pre-catalog default fingerprint exactly. A named VCP,
        // however, is version-locked to its primary source and every physical
        // row so a catalog revision cannot silently reuse cached scan data.
        if let Some(definition) = self.scan_strategy.definition() {
            "build-24-vcp".hash(&mut hasher);
            definition.vcp.number().hash(&mut hasher);
            definition.source.document_number.hash(&mut hasher);
            definition.source.revision.hash(&mut hasher);
            definition.source.rda_build.hash(&mut hasher);
            definition.rows.len().hash(&mut hasher);
            for row in definition.rows {
                hash_vcp_row(row, &mut hasher);
            }
        }
        hash_opt_f64(self.site_lat_deg, &mut hasher);
        hash_opt_f64(self.site_lon_deg, &mut hasher);
        hash_opt_f64(self.antenna_msl_m, &mut hasher);
        self.elevations_deg.len().hash(&mut hasher);
        for elevation in &self.elevations_deg {
            elevation.to_bits().hash(&mut hasher);
        }
        self.azimuth_count.hash(&mut hasher);
        self.gate_spacing_m.to_bits().hash(&mut hasher);
        self.max_range_m.to_bits().hash(&mut hasher);
        self.match_gate_to_grid.hash(&mut hasher);
        self.ref_floor_dbz.to_bits().hash(&mut hasher);
        self.nyquist_mps.to_bits().hash(&mut hasher);
        self.fold_velocity.hash(&mut hasher);
        (self.reflectivity_operator as u8).hash(&mut hasher);
        (self.simulation_mode as u8).hash(&mut hasher);
        (self.reflectivity_sampling as u8).hash(&mut hasher);
        (self.beam_integration as u8).hash(&mut hasher);
        self.beam_width_deg.to_bits().hash(&mut hasher);
        self.pulse_width_us.to_bits().hash(&mut hasher);
        self.radar_frequency_mhz.hash(&mut hasher);
        self.terminal_fall_speed.hash(&mut hasher);
        self.terrain_blockage.hash(&mut hasher);
        self.spectrum_width.hash(&mut hasher);
        self.spectrum_width_floor_mps.to_bits().hash(&mut hasher);
        self.dual_pol.hash(&mut hasher);
        (self.polarimetric_kernel as u8).hash(&mut hasher);
        self.propagation.hash(&mut hasher);
        self.system_phidp_deg.to_bits().hash(&mut hasher);
        self.zdr_bias_db.to_bits().hash(&mut hasher);
        (self.scan_timing as u8).hash(&mut hasher);
        (self.atmosphere_time_mode as u8).hash(&mut hasher);
        (self.missing_neighbor_policy as u8).hash(&mut hasher);
        self.rotation_rate_deg_s.to_bits().hash(&mut hasher);
        self.transition_delay_s.to_bits().hash(&mut hasher);
        self.prf_hz.to_bits().hash(&mut hasher);
        self.coupled_single_prf_estimator.hash(&mut hasher);
        self.estimator_dwell_ms.to_bits().hash(&mut hasher);
        self.estimator_pulse_count.hash(&mut hasher);
        self.estimator_independent_sample_fraction
            .to_bits()
            .hash(&mut hasher);
        self.estimator_minimum_snr_db.to_bits().hash(&mut hasher);
        self.emit_stage_diagnostics.hash(&mut hasher);
        self.instrument_noise.hash(&mut hasher);
        self.sensitivity_dbz_at_1km.to_bits().hash(&mut hasher);
        self.ref_gate_texture.hash(&mut hasher);
        self.vel_gate_texture.hash(&mut hasher);
        self.clutter_intensity.to_bits().hash(&mut hasher);
        self.emit_quality_fields.hash(&mut hasher);
        self.minimum_model_coverage_fraction
            .to_bits()
            .hash(&mut hasher);
        hasher.finish()
    }

    pub fn clamped_minimum_model_coverage_fraction(&self) -> f32 {
        if self.minimum_model_coverage_fraction.is_finite() {
            self.minimum_model_coverage_fraction.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// The Nyquist velocity (m/s) stamped on every radial of the built volume.
    /// With folding ON it is the folding Nyquist [`Self::nyquist_mps`] — the
    /// truth the dealiaser/readout/fold-warnings must see. With folding OFF it is
    /// the historical [`UNFOLDED_NYQUIST_MPS`] (320), so the unfolded TRUE Vr is
    /// treated as already-dealiased (the dealiaser becomes a no-op because no
    /// gate exceeds 320).
    pub fn stamped_nyquist_mps(&self) -> f32 {
        if self.fold_velocity {
            self.nyquist_mps
        } else {
            UNFOLDED_NYQUIST_MPS
        }
    }

    /// Latest ray-acquisition offset in the configured physical scan. This is
    /// the duration supplied to temporal planning, not the end of an unused
    /// transition after the final cut.
    pub fn planned_scan_duration_ms(&self) -> i64 {
        if let Some(template) = self.exact_replay_template.as_deref() {
            return template.latest_acquisition_offset_ms().max(0);
        }
        if !matches!(self.scan_timing, ScanTiming::TimedVolume) {
            return 0;
        }
        let legs = self.physical_scan_legs();
        let mut cut_start_ms = 0i32;
        let mut latest_ray_ms = 0i32;
        for (cut_index, leg) in legs.iter().enumerate() {
            let rays = plan_synthetic_rays(
                cut_index,
                legs.len(),
                self.azimuth_count.max(1),
                self.scan_timing,
                leg.azimuth_rate_deg_per_second,
                cut_start_ms,
            );
            latest_ray_ms =
                latest_ray_ms.max(rays.last().map_or(cut_start_ms, |ray| ray.time_offset_ms));
            cut_start_ms = advance_cut_start_ms(self, leg, cut_start_ms);
        }
        i64::from(latest_ray_ms)
    }
}

const IDEAL_STAGE_DEFINITION: &str = "Ideal=pulse-volume moments after propagation, before receiver censoring, uncertainty, bias, ambiguity, texture, or clutter";
const MEASURED_STAGE_DEFINITION: &str = "Measured=Ideal plus PRF/dwell/pulse-count SNR estimator, receiver censoring, moment bias/uncertainty, and PRF-derived velocity ambiguity";
const PRESENTED_STAGE_DEFINITION: &str = "Presented=Measured plus optional deterministic display texture and stylized ground clutter; canonical output fields use this stage";

fn validate_coupled_estimator_inputs(config: &SyntheticRadarConfig) -> Result<(), String> {
    if let Some(definition) = config.scan_strategy.definition() {
        return Err(format!(
            "coupled single-PRF estimator cannot use named VCP {}: Appendix C PRF codes are identifiers, not frequencies",
            definition.vcp.number()
        ));
    }
    for (name, value, positive) in [
        ("radar_frequency_mhz", config.radar_frequency_mhz, true),
        ("pulse_width_us", config.pulse_width_us, true),
        ("prf_hz", config.prf_hz, true),
        ("estimator_dwell_ms", config.estimator_dwell_ms, true),
        (
            "estimator_independent_sample_fraction",
            config.estimator_independent_sample_fraction,
            true,
        ),
        (
            "sensitivity_dbz_at_1km",
            config.sensitivity_dbz_at_1km,
            false,
        ),
        (
            "estimator_minimum_snr_db",
            config.estimator_minimum_snr_db,
            false,
        ),
        ("zdr_bias_db", config.zdr_bias_db, false),
    ] {
        if !value.is_finite() || (positive && value <= 0.0) {
            return Err(format!("coupled estimator {name} is invalid: {value}"));
        }
    }
    if config.estimator_independent_sample_fraction > 1.0 {
        return Err(format!(
            "coupled estimator independent-sample fraction exceeds 1: {}",
            config.estimator_independent_sample_fraction
        ));
    }
    if config.estimator_pulse_count == Some(0) {
        return Err("coupled estimator pulse count must be positive".to_string());
    }
    if config.estimator_pulse_count.is_none()
        && f64::from(config.estimator_dwell_ms) * 1.0e-3 * f64::from(config.prf_hz) < 1.0
    {
        return Err(
            "coupled estimator dwell and PRF resolve to fewer than one transmitted pulse"
                .to_string(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct PhysicalQuadraturePoint {
    az_sigma: f64,
    el_sigma: f64,
    range_offset_m: f64,
    weight: f64,
}

struct CoupledInstrumentContext {
    instrument: RadarInstrument,
    timing: ResolvedSinglePrf,
    estimator_config: MomentEstimatorConfig,
    sampling: ResolvedEstimatorSampling,
    range_resolution_m: f64,
    balanced_quadrature: Vec<PhysicalQuadraturePoint>,
    reference_quadrature: Vec<PhysicalQuadraturePoint>,
}

impl CoupledInstrumentContext {
    fn quadrature(&self, mode: BeamIntegration) -> Option<&[PhysicalQuadraturePoint]> {
        match mode {
            BeamIntegration::Center => None,
            BeamIntegration::Balanced => Some(&self.balanced_quadrature),
            BeamIntegration::Reference => Some(&self.reference_quadrature),
        }
    }

    fn stamped_nyquist_mps(&self) -> f32 {
        self.timing.nyquist_velocity_mps as f32
    }
}

fn resolve_coupled_instrument(
    config: &SyntheticRadarConfig,
) -> Result<Option<CoupledInstrumentContext>, String> {
    if !config.coupled_single_prf_estimator {
        return Ok(None);
    }
    if let Some(definition) = config.scan_strategy.definition() {
        let source_code = definition
            .rows
            .iter()
            .find_map(|row| {
                row.doppler_prfs
                    .iter()
                    .find(|cell| cell.is_default)
                    .map(|cell| cell.code)
                    .or_else(|| row.surveillance_prf.map(|prf| prf.code))
            })
            .unwrap_or(0);
        return match resolve_prf(
            &RadarInstrument::new(
                "BowEcho virtual radar",
                f64::from(config.radar_frequency_mhz) * 1.0e6,
                f64::from(config.pulse_width_us) * 1.0e-6,
            )
            .map_err(|error| format!("resolve coupled radar instrument: {error}"))?,
            PrfSpecification::NamedVcpCode {
                vcp: definition.vcp.number(),
                code: source_code,
            },
        ) {
            Ok(_) => {
                Err("named VCP PRF code unexpectedly resolved as a physical frequency".to_string())
            }
            Err(error) => Err(format!("resolve coupled single-PRF estimator: {error}")),
        };
    }

    let instrument = RadarInstrument::new(
        "BowEcho virtual radar",
        f64::from(config.radar_frequency_mhz) * 1.0e6,
        f64::from(config.pulse_width_us) * 1.0e-6,
    )
    .map_err(|error| format!("resolve coupled radar instrument: {error}"))?;
    let custom_prf = CustomSinglePrf::new(f64::from(config.prf_hz))
        .map_err(|error| format!("resolve coupled custom PRF: {error}"))?;
    let timing = resolve_prf(&instrument, PrfSpecification::CustomSinglePrf(custom_prf))
        .map_err(|error| format!("resolve coupled custom PRF: {error}"))?;
    let estimator_config = MomentEstimatorConfig {
        dwell_s: f64::from(config.estimator_dwell_ms) * 1.0e-3,
        pulse_count: config.estimator_pulse_count,
        independent_sample_fraction: f64::from(config.estimator_independent_sample_fraction),
        sensitivity_dbz_at_1km: f64::from(config.sensitivity_dbz_at_1km),
        minimum_snr_db: f64::from(config.estimator_minimum_snr_db),
        zdr_system_bias_db: f64::from(config.zdr_bias_db),
        kdp_baseline_km: (config.gate_spacing_m / 1_000.0).max(0.001),
    };
    let sampling = resolve_estimator_sampling(&timing, &estimator_config)
        .map_err(|error| format!("resolve coupled moment-estimator sampling: {error}"))?;
    // Five response nodes include the zero-weight support endpoints; the
    // three interior nodes form a normalized center/half-pulse quadrature.
    let response = MatchedFilterRangeResponse::new(instrument.pulse_width_s, 5)
        .map_err(|error| format!("resolve matched-filter range response: {error}"))?;
    let samples = &response.samples()[1..4];
    let negative = samples[0];
    let center = samples[1];
    let positive = samples[2];
    let mut balanced_quadrature = Vec::with_capacity(9);
    balanced_quadrature.push(PhysicalQuadraturePoint {
        az_sigma: 0.0,
        el_sigma: 0.0,
        range_offset_m: center.offset_m,
        weight: center.weight,
    });
    for &(az_sigma, el_sigma) in &[(-1.0, -1.0), (-1.0, 1.0), (1.0, -1.0), (1.0, 1.0)] {
        for range in [negative, positive] {
            balanced_quadrature.push(PhysicalQuadraturePoint {
                az_sigma,
                el_sigma,
                range_offset_m: range.offset_m,
                weight: range.weight / 4.0,
            });
        }
    }

    let coordinates = [-1.0, 0.0, 1.0];
    let angular_weights = [0.25, 0.5, 0.25];
    let mut reference_quadrature = Vec::with_capacity(27);
    for (az_index, az_sigma) in coordinates.iter().copied().enumerate() {
        for (el_index, el_sigma) in coordinates.iter().copied().enumerate() {
            for range in samples {
                reference_quadrature.push(PhysicalQuadraturePoint {
                    az_sigma,
                    el_sigma,
                    range_offset_m: range.offset_m,
                    weight: angular_weights[az_index] * angular_weights[el_index] * range.weight,
                });
            }
        }
    }
    Ok(Some(CoupledInstrumentContext {
        instrument,
        timing,
        estimator_config,
        sampling,
        range_resolution_m: response.range_resolution_m,
        balanced_quadrature,
        reference_quadrature,
    }))
}

fn advance_cut_start_ms(
    config: &SyntheticRadarConfig,
    leg: &SyntheticScanLeg,
    cut_start_ms: i32,
) -> i32 {
    let (sweep_ms, transition_ms) = if config.scan_strategy.is_named_vcp() {
        (
            1_000.0 * leg.source_period_seconds.max(0.0),
            1_000.0 * leg.transition_after_seconds.max(0.0),
        )
    } else {
        // Preserve the historical custom-mode arithmetic order: the f32
        // rounding of `360_000 / rate` is part of existing ray timestamps.
        (
            360_000.0 / config.rotation_rate_deg_s.max(0.1),
            1_000.0 * config.transition_delay_s.max(0.0),
        )
    };
    (f64::from(cut_start_ms) + f64::from(sweep_ms + transition_ms))
        .round()
        .clamp(0.0, f64::from(i32::MAX)) as i32
}

/// Lower clamp for a grid-matched gate spacing (m). A WRF `DX` finer than this
/// is treated as this value so an unusually fine nest cannot blow up the gate
/// count; also the floor for a garbage-but-positive `DX`.
pub const GRID_GATE_MIN_M: f64 = 100.0;
/// Upper clamp for a grid-matched gate spacing (m). A `DX` coarser than this is
/// capped so a bogus attribute cannot collapse the scan to a handful of gates.
pub const GRID_GATE_MAX_M: f64 = 10_000.0;

/// The WRF `DX` (metres) if it is a usable grid resolution — finite and
/// positive — else `None`. Shared by [`effective_gate_spacing`] and the import
/// note so both agree on whether the grid resolution was actually applied.
fn valid_grid_dx(dx_m: Option<f64>) -> Option<f64> {
    dx_m.filter(|dx| dx.is_finite() && *dx > 0.0)
}

fn hash_vcp_row(row: &PhysicalScanRow, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;

    row.elevation_deg.to_bits().hash(hasher);
    row.azimuth_rate_deg_per_second.to_bits().hash(hasher);
    row.source_period_seconds.to_bits().hash(hasher);
    row.waveform.abbreviation().hash(hasher);
    for present in [
        row.moments.has_reflectivity(),
        row.moments.has_velocity(),
        row.moments.has_spectrum_width(),
        row.moments.has_differential_reflectivity(),
        row.moments.has_correlation_coefficient(),
        row.moments.has_differential_phase(),
    ] {
        present.hash(hasher);
    }
    row.surveillance_prf
        .map(|prf| (prf.code, prf.pulse_count))
        .hash(hasher);
    row.doppler_prfs.len().hash(hasher);
    for cell in row.doppler_prfs {
        cell.code.hash(hasher);
        cell.is_default.hash(hasher);
        match cell.value {
            DopplerPrfValue::PulseCount(count) => {
                0u8.hash(hasher);
                count.hash(hasher);
            }
            DopplerPrfValue::AzimuthRateDegPerSecond(rate) => {
                1u8.hash(hasher);
                rate.to_bits().hash(hasher);
            }
        }
    }
}

fn scan_leg_metadata(leg: &SyntheticScanLeg) -> ScanLegMetadata {
    let Some(row) = leg.source_row else {
        return ScanLegMetadata::default();
    };
    let default_doppler = row.doppler_prfs.iter().find(|cell| cell.is_default);
    ScanLegMetadata {
        source_row_index: leg
            .source_row_index
            .and_then(|index| u16::try_from(index).ok()),
        elevation_deg: Some(row.elevation_deg),
        azimuth_rate_deg_per_second: Some(row.azimuth_rate_deg_per_second),
        source_period_seconds: Some(row.source_period_seconds),
        waveform: Some(row.waveform.abbreviation().to_owned()),
        moment_coverage: Some(
            if row.moments == MomentCoverage::SURVEILLANCE {
                "surveillance"
            } else if row.moments == MomentCoverage::DOPPLER {
                "doppler"
            } else {
                "all"
            }
            .to_owned(),
        ),
        surveillance_prf_code: row.surveillance_prf.map(|prf| prf.code),
        surveillance_pulse_count: row.surveillance_prf.map(|prf| prf.pulse_count),
        doppler_prf_code: default_doppler.map(|cell| cell.code),
        doppler_pulse_count: default_doppler.and_then(|cell| match cell.value {
            DopplerPrfValue::PulseCount(count) => Some(count),
            DopplerPrfValue::AzimuthRateDegPerSecond(_) => None,
        }),
    }
}

/// The gate spacing (m) one synthetic scan actually uses, given its config and
/// the source file's `DX` global attribute (`None` when absent/unreadable).
///
/// - [`SyntheticRadarConfig::match_gate_to_grid`] OFF → the configured
///   [`SyntheticRadarConfig::gate_spacing_m`] (today's behaviour, unchanged).
/// - ON with a finite, positive `DX` → that `DX`, clamped to
///   `[GRID_GATE_MIN_M, GRID_GATE_MAX_M]`, so the gate spacing matches the model
///   grid instead of oversampling it.
/// - ON with a missing / non-finite / non-positive `DX` → falls back to the
///   configured `gate_spacing_m`.
///
/// Pure (no file handle) so the resolution is unit-testable in isolation.
pub fn effective_gate_spacing(config: &SyntheticRadarConfig, dx_m: Option<f64>) -> f64 {
    if config.match_gate_to_grid
        && let Some(dx) = valid_grid_dx(dx_m)
    {
        dx.clamp(GRID_GATE_MIN_M, GRID_GATE_MAX_M)
    } else {
        config.gate_spacing_m
    }
}

/// Whether the effective gate spacing came from the grid `DX` (matching is on
/// AND the file supplied a usable `DX`) — drives the self-documenting import
/// note, distinguishing a genuine grid match from a matched-but-fell-back run.
fn matched_grid_dx(config: &SyntheticRadarConfig, dx_m: Option<f64>) -> bool {
    config.match_gate_to_grid && valid_grid_dx(dx_m).is_some()
}

/// Alias one true (unfolded) radial velocity `v` into the Nyquist co-interval
/// `[-nyquist, +nyquist)`, exactly as a real pulse-pair velocity estimator
/// reports it (WSR-88D Level II convention: `-nyquist` is representable,
/// `+nyquist` aliases to `-nyquist`).
///
/// The alias is `(v + nyquist).rem_euclid(2·nyquist) − nyquist`, the same wrap
/// the velocity dealiaser's own fixtures assume for real folded data (`render2d`
/// `dealias_v4`), so synthetic folds and the unfolder speak one language. This is
/// algebraically identical to the textbook `v − 2·nyquist·round(v/(2·nyquist))`
/// everywhere except the measure-zero boundary `v = −nyquist`: round-half-away
/// sends it to `+nyquist`, while the half-open interval keeps it at `−nyquist`.
///
/// A velocity ALREADY inside `[−nyquist, +nyquist)` is returned bit-for-bit
/// unchanged — a real estimator aliases only out-of-Nyquist velocities, so
/// in-Nyquist gates (clutter/near-zero returns, sub-Nyquist winds) are never even
/// perturbed by float rounding. Only genuinely out-of-range gates take the wrap.
///
/// A non-finite `v` (NaN / ±inf), or a non-finite / non-positive `nyquist`,
/// passes through UNCHANGED, so missing/blank gates and a degenerate Nyquist
/// never fabricate a value.
pub fn fold_velocity_mps(v: f32, nyquist: f32) -> f32 {
    if !v.is_finite() || !nyquist.is_finite() || nyquist <= 0.0 {
        return v;
    }
    // In-Nyquist velocities are reported exactly (no rounding); only out-of-range
    // gates are aliased into the co-interval.
    if (-nyquist..nyquist).contains(&v) {
        return v;
    }
    (v + nyquist).rem_euclid(2.0 * nyquist) - nyquist
}

/// Hash an optional `f64` by its bit pattern (floats are not `Hash`), with a
/// present/absent tag so `Some(0.0)` and `None` never collide.
fn hash_opt_f64(value: Option<f64>, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    match value {
        Some(value) => {
            1u8.hash(hasher);
            value.to_bits().hash(hasher);
        }
        None => 0u8.hash(hasher),
    }
}

/// The 3-D model fields one synthetic scan samples, read once per forecast
/// time and flattened to `f32` on the WRF unstaggered grid.
///
/// All 3-D arrays are row-major `[nz, ny, nx]` (index `k * ny*nx + j*nx + i`);
/// lat/lon are `[ny, nx]`. `dbz` is dBZ, winds are earth-relative m·s⁻¹,
/// `height_msl` is metres MSL.
pub struct WrfRadarFields {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub lat: Vec<f32>,
    pub lon: Vec<f32>,
    pub height_msl: Vec<f32>,
    pub dbz: Vec<f32>,
    pub u: Vec<f32>,
    pub v: Vec<f32>,
    pub w: Vec<f32>,
    pub terrain_m: Vec<f32>,
    /// Sparse additive scattering precomputed from native P3/ISHMAEL state for
    /// the opt-in research T-matrix kernel. Raw model-property arrays are
    /// dropped after closure and LUT evaluation; gate sampling interpolates
    /// only additive scattering quantities.
    property_scattering: Option<Arc<app_ui::wrf_tmatrix_scene::WrfTMatrixScene>>,
    /// Exact normalized P3/ISHMAEL source state retained only for
    /// RawStateLinear. Spatial and temporal weights are applied before the
    /// single per-quadrature closure/T-matrix evaluation.
    raw_property_scene: Option<Arc<app_ui::wrf_property_reader::WrfPropertyScene>>,
    /// Compact scheme-aware polarimetric state. Seven one-byte planes and one
    /// signed two-byte KDP plane retain
    /// the ratios/propagation/fall moments while `dbz` remains the full-precision
    /// ZH plane, avoiding several additional 193 MiB f32 volumes on the
    /// documented 800x800x79 nest.
    polarimetric: Option<CompactPolarFields>,
    /// Why requested dual-pol was unavailable (unsupported scheme or missing
    /// required raw fields). Scalar REF/VEL remain usable in this case.
    pub dual_pol_status: Option<String>,
    /// Optional one-byte turbulent kinetic energy (0.1 m2/s2 increments).
    tke_tenths_m2s2: Option<Vec<u8>>,
    /// Which reflectivity source populated `dbz` ("REFL_10CM" or "dbz/CALCDBZ").
    pub ref_source: &'static str,
    /// The source file's `DX` global attribute (grid resolution, metres) if the
    /// file carries a readable one, else `None`. Read once with the fields (they
    /// come from the same file) so [`build_synthetic_volume_reporting`] can size
    /// gates to the grid via [`effective_gate_spacing`] without re-opening it.
    pub dx_m: Option<f64>,
    lut: InverseLut,
}

const COMPACT_ZDR_STEP_DB: f32 = 0.05;
const COMPACT_PHASE_STEP_DEG: f32 = 0.1;
const COMPACT_KDP_STEP_DEG_KM: f32 = 0.1;
const COMPACT_ATTEN_STEP_DB_KM: f32 = 0.001;
const COMPACT_FALL_STEP_MPS: f32 = 0.1;
const COMPACT_FALL_STD_STEP_MPS: f32 = 0.05;

struct CompactPolarFields {
    profile: crate::wrf_radar_physics::SchemeProfile,
    present_fields: Vec<String>,
    zdr: Vec<i8>,
    rho: Vec<u8>,
    covariance_phase: Vec<i8>,
    kdp: Vec<i16>,
    ah: Vec<u8>,
    adp: Vec<i8>,
    fall_speed: Vec<u8>,
    fall_speed_std: Vec<u8>,
    precision_audit: CompactPolarPrecisionAudit,
}

impl CompactPolarFields {
    fn new(
        len: usize,
        profile: crate::wrf_radar_physics::SchemeProfile,
        present_fields: Vec<String>,
    ) -> Self {
        Self {
            profile,
            present_fields,
            zdr: vec![0; len],
            rho: vec![u8::MAX; len],
            covariance_phase: vec![0; len],
            kdp: vec![0; len],
            ah: vec![0; len],
            adp: vec![0; len],
            fall_speed: vec![0; len],
            fall_speed_std: vec![0; len],
            precision_audit: CompactPolarPrecisionAudit::default(),
        }
    }

    fn contribution_at(&self, index: usize, zh: f32) -> crate::wrf_radar_physics::BulkContribution {
        if !zh.is_finite() || zh <= 0.0 || index >= self.zdr.len() {
            return crate::wrf_radar_physics::BulkContribution::default();
        }
        let zdr = self.zdr[index] as f32 * COMPACT_ZDR_STEP_DB;
        let zv = zh / 10.0f32.powf(zdr * 0.1);
        let rho = self.rho[index] as f32 / u8::MAX as f32;
        let covariance = rho * (zh * zv).max(0.0).sqrt();
        let phase = (self.covariance_phase[index] as f32 * COMPACT_PHASE_STEP_DEG).to_radians();
        let ah = self.ah[index] as f32 * COMPACT_ATTEN_STEP_DB_KM;
        let adp = self.adp[index] as f32 * COMPACT_ATTEN_STEP_DB_KM;
        let fall_std = self.fall_speed_std[index] as f32 * COMPACT_FALL_STD_STEP_MPS;
        crate::wrf_radar_physics::BulkContribution {
            zh,
            zv,
            cov_re: covariance * phase.cos(),
            cov_im: covariance * phase.sin(),
            kdp_deg_km: self.kdp[index] as f32 * COMPACT_KDP_STEP_DEG_KM,
            ah_db_km: ah,
            av_db_km: (ah - adp).max(0.0),
            fall_speed_mps: self.fall_speed[index] as f32 * COMPACT_FALL_STEP_MPS,
            fall_speed_variance_m2s2: fall_std * fall_std,
        }
    }

    fn store(&mut self, index: usize, sample: crate::wrf_radar_physics::IntrinsicPolarSample) {
        self.zdr[index] = quantize_i8(sample.zdr_db, COMPACT_ZDR_STEP_DB);
        self.rho[index] = (sample.rho_hv.clamp(0.0, 1.0) * u8::MAX as f32).round() as u8;
        let phase_deg = sample.cov_im.atan2(sample.cov_re).to_degrees();
        self.covariance_phase[index] = quantize_i8(phase_deg, COMPACT_PHASE_STEP_DEG);
        self.kdp[index] = quantize_i16(sample.kdp_deg_km, COMPACT_KDP_STEP_DEG_KM);
        self.ah[index] = quantize_u8(sample.ah_db_km, COMPACT_ATTEN_STEP_DB_KM);
        self.adp[index] = quantize_i8(sample.ah_db_km - sample.av_db_km, COMPACT_ATTEN_STEP_DB_KM);
        self.fall_speed[index] = quantize_u8(sample.fall_speed_mps, COMPACT_FALL_STEP_MPS);
        self.fall_speed_std[index] = quantize_u8(
            sample.fall_speed_variance_m2s2.max(0.0).sqrt(),
            COMPACT_FALL_STD_STEP_MPS,
        );

        // Audit the exact compact representation after encoding. The stored
        // codes and reconstruction path above are unchanged; this only records
        // loss/saturation so production runs can prove whether the memory
        // compression remained inside its declared precision envelope.
        let reconstructed_zdr = self.zdr[index] as f32 * COMPACT_ZDR_STEP_DB;
        let reconstructed_zv = sample.zh / 10.0f32.powf(reconstructed_zdr * 0.1);
        let reconstructed_rho = self.rho[index] as f32 / u8::MAX as f32;
        let reconstructed_covariance =
            reconstructed_rho * (sample.zh * reconstructed_zv).max(0.0).sqrt();
        let reconstructed_phase = self.covariance_phase[index] as f32 * COMPACT_PHASE_STEP_DEG;
        let source_phase = sample.cov_im.atan2(sample.cov_re).to_degrees();
        let source_adp = sample.ah_db_km - sample.av_db_km;
        let reconstructed_kdp = self.kdp[index] as f32 * COMPACT_KDP_STEP_DEG_KM;
        let reconstructed_ah = self.ah[index] as f32 * COMPACT_ATTEN_STEP_DB_KM;
        let encoded_adp = self.adp[index] as f32 * COMPACT_ATTEN_STEP_DB_KM;
        let reconstructed_av = (reconstructed_ah - encoded_adp).max(0.0);
        let reconstructed_adp = reconstructed_ah - reconstructed_av;
        let reconstructed_fall = self.fall_speed[index] as f32 * COMPACT_FALL_STEP_MPS;
        let source_fall_std = sample.fall_speed_variance_m2s2.max(0.0).sqrt();
        let reconstructed_fall_std = self.fall_speed_std[index] as f32 * COMPACT_FALL_STD_STEP_MPS;
        let reconstructed_fall_variance = reconstructed_fall_std * reconstructed_fall_std;

        self.precision_audit.zdr_db.observe(
            sample.zdr_db,
            reconstructed_zdr,
            i8::MIN as f32 * COMPACT_ZDR_STEP_DB,
            i8::MAX as f32 * COMPACT_ZDR_STEP_DB,
        );
        self.precision_audit
            .rho_hv
            .observe(sample.rho_hv, reconstructed_rho, 0.0, 1.0);
        self.precision_audit.covariance_phase_deg.observe(
            source_phase,
            reconstructed_phase,
            i8::MIN as f32 * COMPACT_PHASE_STEP_DEG,
            i8::MAX as f32 * COMPACT_PHASE_STEP_DEG,
        );
        self.precision_audit.kdp_deg_km.observe(
            sample.kdp_deg_km,
            reconstructed_kdp,
            i16::MIN as f32 * COMPACT_KDP_STEP_DEG_KM,
            i16::MAX as f32 * COMPACT_KDP_STEP_DEG_KM,
        );
        self.precision_audit.ah_db_km.observe(
            sample.ah_db_km,
            reconstructed_ah,
            0.0,
            u8::MAX as f32 * COMPACT_ATTEN_STEP_DB_KM,
        );
        self.precision_audit.adp_db_km.observe(
            source_adp,
            reconstructed_adp,
            i8::MIN as f32 * COMPACT_ATTEN_STEP_DB_KM,
            i8::MAX as f32 * COMPACT_ATTEN_STEP_DB_KM,
        );
        self.precision_audit.fall_speed_mps.observe(
            sample.fall_speed_mps,
            reconstructed_fall,
            0.0,
            u8::MAX as f32 * COMPACT_FALL_STEP_MPS,
        );
        self.precision_audit.fall_speed_std_mps.observe(
            source_fall_std,
            reconstructed_fall_std,
            0.0,
            u8::MAX as f32 * COMPACT_FALL_STD_STEP_MPS,
        );
        self.precision_audit.max_zv_relative_error = self
            .precision_audit
            .max_zv_relative_error
            .max(relative_error(sample.zv, reconstructed_zv));
        self.precision_audit.max_covariance_magnitude_relative_error = self
            .precision_audit
            .max_covariance_magnitude_relative_error
            .max(relative_error(
                sample.covariance_magnitude,
                reconstructed_covariance,
            ));
        self.precision_audit.max_av_abs_error_db_km = self
            .precision_audit
            .max_av_abs_error_db_km
            .max((reconstructed_av - sample.av_db_km).abs());
        self.precision_audit.max_fall_variance_abs_error_m2s2 = self
            .precision_audit
            .max_fall_variance_abs_error_m2s2
            .max((reconstructed_fall_variance - sample.fall_speed_variance_m2s2).abs());
    }
}

fn quantize_u8(value: f32, step: f32) -> u8 {
    if value.is_finite() && step > 0.0 {
        (value.max(0.0) / step).round().clamp(0.0, u8::MAX as f32) as u8
    } else {
        0
    }
}

fn quantize_i8(value: f32, step: f32) -> i8 {
    if value.is_finite() && step > 0.0 {
        (value / step).round().clamp(i8::MIN as f32, i8::MAX as f32) as i8
    } else {
        0
    }
}

fn quantize_i16(value: f32, step: f32) -> i16 {
    if value.is_finite() && step > 0.0 {
        (value / step)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16
    } else {
        0
    }
}

impl WrfRadarFields {
    fn cells(&self) -> usize {
        self.nx * self.ny
    }

    fn has_polarimetric_input(&self) -> bool {
        self.polarimetric.is_some()
            || self.property_scattering.is_some()
            || self.raw_property_scene.is_some()
    }

    fn base_retained_bytes_without_property_scattering(&self) -> usize {
        let f32_values = self
            .lat
            .len()
            .saturating_add(self.lon.len())
            .saturating_add(self.height_msl.len())
            .saturating_add(self.dbz.len())
            .saturating_add(self.u.len())
            .saturating_add(self.v.len())
            .saturating_add(self.w.len())
            .saturating_add(self.terrain_m.len());
        let compact_polar_bytes = self.polarimetric.as_ref().map_or(0, |polar| {
            polar.zdr.len()
                + polar.rho.len()
                + polar.covariance_phase.len()
                + polar.kdp.len() * std::mem::size_of::<i16>()
                + polar.ah.len()
                + polar.adp.len()
                + polar.fall_speed.len()
                + polar.fall_speed_std.len()
        });
        std::mem::size_of::<Self>()
            .saturating_add(f32_values.saturating_mul(std::mem::size_of::<f32>()))
            .saturating_add(compact_polar_bytes)
            .saturating_add(
                self.tke_tenths_m2s2
                    .as_ref()
                    .map_or(0, |values| values.len()),
            )
            .saturating_add(self.lut.retained_bytes())
            .saturating_add(
                self.dual_pol_status
                    .as_ref()
                    .map_or(0, std::string::String::len),
            )
    }

    fn retained_bytes_estimate(&self) -> usize {
        self.base_retained_bytes_without_property_scattering()
            .saturating_add(
                self.property_scattering
                    .as_deref()
                    .map_or(0, |scene| scene.memory_estimate().retained_bytes()),
            )
            .saturating_add(
                self.raw_property_scene
                    .as_deref()
                    .map_or(0, |scene| scene.memory_estimate().retained_bytes()),
            )
    }

    /// Domain-centre grid cell (used for the default antenna position).
    fn center_cell(&self) -> usize {
        (self.ny / 2) * self.nx + (self.nx / 2)
    }
}

fn remaining_property_tmatrix_build_budget_bytes(
    config: &SyntheticRadarConfig,
    fields: &WrfRadarFields,
    expected_cells: usize,
    reserved_memory_bytes: usize,
) -> Result<usize, String> {
    let budget_bytes = config
        .temporal_memory_budget_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "Simulated-radar memory budget overflows address space".to_string())?;
    checked_property_tmatrix_build_remainder(
        budget_bytes,
        fields.base_retained_bytes_without_property_scattering(),
        if config.spectrum_width {
            expected_cells
        } else {
            0
        },
        app_ui::wrf_tmatrix_assets::embedded_lut_memory_bytes(),
        reserved_memory_bytes,
        minimum_property_tmatrix_owned_peak_bytes(expected_cells)?,
    )
}

fn ensure_raw_property_retention_budget(
    config: &SyntheticRadarConfig,
    fields: &WrfRadarFields,
    property_scene: &app_ui::wrf_property_reader::WrfPropertyScene,
    expected_cells: usize,
    reserved_memory_bytes: usize,
) -> Result<(), String> {
    let budget_bytes = config
        .temporal_memory_budget_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "Raw-state temporal memory budget overflows address space".to_string())?;
    let tke_bytes = if config.spectrum_width {
        expected_cells
    } else {
        0
    };
    let required = fields
        .base_retained_bytes_without_property_scattering()
        .checked_add(property_scene.memory_estimate().retained_bytes())
        .and_then(|value| {
            value.checked_add(app_ui::wrf_tmatrix_assets::embedded_lut_memory_bytes())
        })
        .and_then(|value| value.checked_add(tke_bytes))
        .and_then(|value| value.checked_add(reserved_memory_bytes))
        .ok_or_else(|| "Raw-state temporal retained-memory estimate overflowed".to_string())?;
    if required > budget_bytes {
        return Err(format!(
            "RawStateLinear needs {:.2} GiB for retained atmosphere, exact property state, embedded tables, TKE, outputs, and cache, above the configured {:.2} GiB budget",
            required as f64 / 1024.0_f64.powi(3),
            budget_bytes as f64 / 1024.0_f64.powi(3),
        ));
    }
    Ok(())
}

fn minimum_property_tmatrix_owned_peak_bytes(expected_cells: usize) -> Result<usize, String> {
    // The raw source owns dense temperature, pressure, moist-density, and
    // dry-density f32 planes.
    // While that source is retained, the scattering scene must also allocate
    // one dense u32 full-cell -> sparse-row map. Sparse categories, additive
    // output rows, provenance, and worker scratch only increase this bound.
    let unavoidable_dense_bytes = expected_cells
        .checked_mul(4 * std::mem::size_of::<f32>() + std::mem::size_of::<u32>())
        .ok_or_else(|| "Property T-matrix minimum source-memory estimate overflowed".to_string())?;
    unavoidable_dense_bytes
        .checked_add(std::mem::size_of::<
            app_ui::wrf_property_reader::WrfPropertyScene,
        >())
        .and_then(|value| {
            value.checked_add(std::mem::size_of::<
                app_ui::wrf_tmatrix_scene::WrfTMatrixScene,
            >())
        })
        .ok_or_else(|| "Property T-matrix minimum source-memory estimate overflowed".to_string())
}

fn checked_property_tmatrix_build_remainder(
    budget_bytes: usize,
    retained_field_bytes: usize,
    spectrum_width_bytes: usize,
    embedded_lut_bytes: usize,
    reserved_memory_bytes: usize,
    minimum_owned_peak_bytes: usize,
) -> Result<usize, String> {
    let current_base_bytes = retained_field_bytes
        .checked_add(spectrum_width_bytes)
        .and_then(|value| value.checked_add(embedded_lut_bytes))
        .and_then(|value| value.checked_add(reserved_memory_bytes))
        .ok_or_else(|| "Property T-matrix build-memory reservation overflowed".to_string())?;
    let remaining = budget_bytes.saturating_sub(current_base_bytes);
    if remaining >= minimum_owned_peak_bytes {
        Ok(remaining)
    } else {
        Err(format!(
            "Property T-matrix raw read and dense lookup need at least {:.2} GiB after retained fields, tables, outputs, and cache, but only {:.2} GiB remains inside the configured {:.2} GiB budget",
            minimum_owned_peak_bytes as f64 / 1024.0_f64.powi(3),
            remaining as f64 / 1024.0_f64.powi(3),
            budget_bytes as f64 / 1024.0_f64.powi(3),
        ))
    }
}

/// Read the WRF 3-D reflectivity + earth-relative winds + height for one time.
///
/// Reflectivity: `REFL_10CM` (the model's own Thompson 10-cm reflectivity) when
/// present and the operator is [`ReflectivityOperator::ModelNative`], else
/// `wrf-core`'s `dbz` (`CALCDBZ`, Stoelinga 2005) — the same diagnostic
/// BowEcho's composite reflectivity uses, so a synthetic scan co-locates with
/// the model's own composite. [`ReflectivityOperator::ClassicStoelinga`] forces
/// the `CALCDBZ` path even when the file carries `REFL_10CM`. Raw wrfout carries
/// the hydrometeor mixing ratios `dbz` needs; a post-processed / climate wrfout
/// may not — this returns an `Err` (empty/absent reflectivity) so the caller can
/// warn rather than emit an all-NaN scan.
pub fn read_wrf_radar_fields(
    file: &WrfFile,
    timeidx: usize,
    operator: ReflectivityOperator,
) -> Result<WrfRadarFields, String> {
    read_wrf_radar_fields_reporting(file, timeidx, operator, &|_| {})
}

/// Read one time's fields, streaming stage labels through `progress` so the UI
/// can show "Reading …" instead of freezing.
///
/// PERF: the four heavy 3-D fields (height, reflectivity, earth-relative winds,
/// vertical velocity) are the whole cost of a synthetic scan — on a 250 m
/// 800×800×79 wrfout the NetCDF decompress dominates (~7 s serial), while the
/// polar sampling that follows is <0.1 s. Each is an independent variable, so
/// they are read/decompressed on separate threads with `std::thread::scope`.
/// The pure-Rust HDF5 reader guards only the file handle with a mutex and
/// decompresses (the expensive part) without it, so the inflates overlap:
/// wall time drops to the single longest field (~2.5–3 s here, ~2.5× faster).
/// Each thread calls the exact same `getvar`/`read_var` entry points as before,
/// so the sampled output is byte-for-byte unchanged — this is a speed change,
/// not an accuracy change.
pub fn read_wrf_radar_fields_reporting(
    file: &WrfFile,
    timeidx: usize,
    operator: ReflectivityOperator,
    progress: &dyn Fn(&str),
) -> Result<WrfRadarFields, String> {
    read_wrf_radar_fields_reporting_inner(file, timeidx, operator, progress, true)
}

fn read_wrf_radar_fields_reporting_inner(
    file: &WrfFile,
    timeidx: usize,
    operator: ReflectivityOperator,
    progress: &dyn Fn(&str),
    require_native_reflectivity: bool,
) -> Result<WrfRadarFields, String> {
    let nx = file.nx;
    let ny = file.ny;
    let nz = file.nz;
    let cells = nx * ny;
    if cells == 0 || nz == 0 {
        return Err("WRF grid has zero cells".to_string());
    }

    // Grid resolution (metres) for the optional "match gate size to grid" mode.
    // Read here where the file handle lives and carried on the fields; a missing
    // or unreadable attribute is `None` and the builder falls back to the
    // configured gate spacing. WRF writes `DX` in metres.
    let dx_m = file.global_attr_f64("DX").ok();

    let lat = file
        .xlat(timeidx)
        .map_err(|err| format!("read XLAT: {err}"))?;
    let lon = file
        .xlong(timeidx)
        .map_err(|err| format!("read XLONG: {err}"))?;
    if lat.len() != cells || lon.len() != cells {
        return Err(format!(
            "WRF lat/lon size mismatch: expected {cells}, got lat {} lon {}",
            lat.len(),
            lon.len()
        ));
    }

    progress(if require_native_reflectivity {
        "reading model fields (reflectivity, winds, height)…"
    } else {
        "reading model fields (winds and height; reflectivity comes from property T-matrix)…"
    });

    // Read the four heavy 3-D fields concurrently. Placeholders are overwritten
    // inside the scope; the scope join guarantees they are all set on exit.
    let mut height_res: Result<Vec<f32>, String> = Err("height not read".to_string());
    let mut refl_res: Result<(Vec<f32>, &'static str), String> = Err("refl not read".to_string());
    let mut winds_res: Result<(Vec<f32>, Vec<f32>), String> = Err("winds not read".to_string());
    let mut w_res: Result<Vec<f32>, String> = Err("wa not read".to_string());
    let mut terrain_m: Vec<f32> = Vec::new();
    std::thread::scope(|scope| {
        let th_height = scope.spawn(|| read_3d(file, "height", timeidx, nz * cells));
        let th_refl = scope.spawn(|| {
            if require_native_reflectivity {
                read_reflectivity(file, timeidx, nz * cells, operator)
            } else {
                Ok((Vec::new(), PROPERTY_TMATRIX_S_BAND_SOURCE))
            }
        });
        let th_winds = scope.spawn(|| read_earth_relative_winds(file, timeidx, nz * cells));
        let th_w = scope.spawn(|| read_3d(file, "wa", timeidx, nz * cells));
        let th_terrain = scope.spawn(|| read_terrain_m(file, timeidx, cells));

        height_res = join_read(th_height, "height");
        refl_res = join_read(th_refl, "reflectivity");
        winds_res = join_read(th_winds, "winds");
        w_res = join_read(th_w, "wa");
        terrain_m = th_terrain.join().unwrap_or_else(|_| vec![0.0; cells]);
    });

    let height = height_res?;
    let (dbz, ref_source) = refl_res?;
    if require_native_reflectivity && dbz.iter().all(|value| !value.is_finite()) {
        return Err(format!(
            "WRF reflectivity ({ref_source}) is entirely missing — is this a \
             post-processed/climate wrfout without hydrometeor mixing ratios?"
        ));
    }
    let (u, v) = winds_res?;
    let w = w_res?;

    progress("building geolocation index…");
    let lat_f32 = to_f32(&lat);
    let lon_f32 = to_f32(&lon);
    // Domain-bounded: a WRF grid's row/col perimeter IS its true data edge,
    // so gates past that edge (but still inside the rectangular lat/lon
    // bbox) must sample nothing — without the bound, the LUT's hole-fill
    // dilation leaked nearest-edge values into a ~3-bin smeared ring around
    // the domain boundary.
    let lut = InverseLut::build_with_shape_domain_bounded(&lat_f32, &lon_f32, nx, ny)
        .ok_or_else(|| "failed to build WRF inverse geolocation LUT".to_string())?;

    Ok(WrfRadarFields {
        nx,
        ny,
        nz,
        lat: lat_f32,
        lon: lon_f32,
        height_msl: height,
        dbz,
        u,
        v,
        w,
        terrain_m,
        property_scattering: None,
        raw_property_scene: None,
        polarimetric: None,
        dual_pol_status: None,
        tke_tenths_m2s2: None,
        ref_source,
        dx_m,
        lut,
    })
}

/// Config-aware field reader used by the production worker. The public
/// operator-only reader above remains the compatibility/fixture seam; advanced
/// modes add compact microphysical scattering and optional TKE here.
fn read_wrf_radar_fields_for_config_reporting(
    file: &WrfFile,
    source_identity: &WrfSourceIdentity,
    timeidx: usize,
    config: &SyntheticRadarConfig,
    progress: &dyn Fn(&str),
    reserved_memory_bytes: usize,
) -> Result<WrfRadarFields, String> {
    let property_tmatrix = matches!(
        config.polarimetric_kernel,
        PolarimetricKernel::PropertyTMatrixResearchV1
    );
    let mut fields = read_wrf_radar_fields_reporting_inner(
        file,
        timeidx,
        config.reflectivity_operator,
        progress,
        !property_tmatrix,
    )?;
    let expected = fields.nx * fields.ny * fields.nz;

    if property_tmatrix {
        if !config.dual_pol {
            return Err(
                "Property T-matrix research mode requires S-band dual polarization".to_string(),
            );
        }
        let raw_state_linear = matches!(
            config.atmosphere_time_mode,
            AtmosphereTimeMode::RawStateLinear
        );
        let maximum_owned_peak_bytes = if raw_state_linear {
            None
        } else {
            Some(remaining_property_tmatrix_build_budget_bytes(
                config,
                &fields,
                expected,
                reserved_memory_bytes,
            )?)
        };
        progress("validating embedded property T-matrix tables…");
        app_ui::wrf_tmatrix_assets::preload_embedded_property_tmatrix_luts()
            .map_err(|error| format!("preload embedded property T-matrix bundle: {error}"))?;
        progress("reading native P3/ISHMAEL property state…");
        let property_scene = app_ui::wrf_property_reader::read_wrf_property_scene(
            file,
            source_identity.clone(),
            timeidx,
        )
        .map_err(|error| format!("read native P3/ISHMAEL property state: {error}"))?;
        if property_scene.cell_count() != expected {
            return Err(format!(
                "property scene has {} cells, expected {expected}",
                property_scene.cell_count()
            ));
        }
        let fields_read = property_scene
            .source_fields()
            .iter()
            .map(app_ui::wrf_property_reader::SourceFieldProvenance::source_name)
            .collect::<Vec<_>>()
            .join(",");
        if raw_state_linear {
            progress("retaining raw property state for pre-closure interpolation…");
        } else {
            progress("evaluating property T-matrix scene…");
        }
        fields.ref_source = PROPERTY_TMATRIX_S_BAND_SOURCE;
        if raw_state_linear {
            ensure_raw_property_retention_budget(
                config,
                &fields,
                &property_scene,
                expected,
                reserved_memory_bytes,
            )?;
            fields.dual_pol_status = Some(format!(
                "property-aware T-matrix raw_state_linear input (mp_physics={}; raw fields={fields_read}; retained raw state {:.2} GiB)",
                property_scene.microphysics_scheme_id(),
                property_scene.memory_estimate().retained_bytes() as f64 / 1024.0_f64.powi(3),
            ));
            fields.raw_property_scene = Some(Arc::new(property_scene));
        } else {
            let built = app_ui::wrf_tmatrix_assets::build_embedded_property_tmatrix_scene(
                &property_scene,
                maximum_owned_peak_bytes.expect("non-raw property build has a budget"),
            )
            .map_err(|error| format!("build property T-matrix scene: {error}"))?;
            let property_scattering = built.scene;
            fields.dual_pol_status = Some(format!(
                "property-aware T-matrix research input (mp_physics={}; raw fields={fields_read}; build peak {:.2} GiB)",
                property_scattering.microphysics_scheme_id(),
                built.peak.estimated_peak_bytes as f64 / 1024.0_f64.powi(3),
            ));
            fields.property_scattering = Some(Arc::new(property_scattering));
        }
    } else if config.dual_pol || config.terminal_fall_speed {
        progress("deriving scheme-aware hydrometeor scattering…");
        match build_compact_polar_fields(file, timeidx, expected, progress) {
            Ok((polar, bulk_zh_dbz)) => {
                let audit = &polar.precision_audit;
                fields.dual_pol_status = Some(format!(
                    "{} ({:?}{}; compact audit: clamps={}, quantized-to-zero={}, max error ZDR={:.4} dB rho={:.5} KDP={:.4} deg/km AH={:.4} dB/km)",
                    polar.profile.name,
                    polar.profile.capability,
                    if polar.profile.assumption_heavy {
                        "; assumed PSD parameters"
                    } else {
                        ""
                    },
                    audit.total_clamps(),
                    audit.total_quantized_to_zero(),
                    audit.zdr_db.max_abs_reconstruction_error,
                    audit.rho_hv.max_abs_reconstruction_error,
                    audit.kdp_deg_km.max_abs_reconstruction_error,
                    audit.ah_db_km.max_abs_reconstruction_error,
                ));
                if config.dual_pol {
                    fields.dbz = bulk_zh_dbz;
                    fields.ref_source = BULK_S_BAND_SOURCE;
                }
                fields.polarimetric = Some(polar);
            }
            Err(reason) => {
                fields.dual_pol_status = Some(reason);
            }
        }
    }

    if config.spectrum_width {
        fields.tke_tenths_m2s2 = read_compact_tke(file, timeidx, expected);
    }
    Ok(fields)
}

const MICROPHYSICS_FIELD_NAMES: &[&str] = &[
    "QCLOUD",
    "QRAIN",
    "QICE",
    "QICE2",
    "QICE3",
    "QSNOW",
    "QGRAUP",
    "QGRAUPEL",
    "QHAIL",
    "QNDROP",
    "QNCLOUD",
    "QNRAIN",
    "QNICE",
    "QNICE2",
    "QNICE3",
    "QNSNOW",
    "QNGRAUP",
    "QNGRAUPEL",
    "QNHAIL",
    "QVGRAUPEL",
    "QVHAIL",
];

struct SpeciesReadSpec {
    kind: crate::wrf_radar_physics::HydrometeorKind,
    mass: &'static [&'static str],
    number: &'static [&'static str],
    volume: &'static [&'static str],
}

const SPECIES_READ_SPECS: &[SpeciesReadSpec] = &[
    SpeciesReadSpec {
        kind: crate::wrf_radar_physics::HydrometeorKind::CloudWater,
        mass: &["QCLOUD"],
        number: &["QNCLOUD", "QNDROP"],
        volume: &[],
    },
    SpeciesReadSpec {
        kind: crate::wrf_radar_physics::HydrometeorKind::Rain,
        mass: &["QRAIN"],
        number: &["QNRAIN"],
        volume: &[],
    },
    SpeciesReadSpec {
        kind: crate::wrf_radar_physics::HydrometeorKind::CloudIce,
        mass: &["QICE", "QICE2", "QICE3"],
        number: &["QNICE", "QNICE2", "QNICE3"],
        volume: &[],
    },
    SpeciesReadSpec {
        kind: crate::wrf_radar_physics::HydrometeorKind::Snow,
        mass: &["QSNOW"],
        number: &["QNSNOW"],
        volume: &[],
    },
    SpeciesReadSpec {
        kind: crate::wrf_radar_physics::HydrometeorKind::Graupel,
        mass: &["QGRAUP", "QGRAUPEL"],
        number: &["QNGRAUPEL", "QNGRAUP"],
        volume: &["QVGRAUPEL"],
    },
    SpeciesReadSpec {
        kind: crate::wrf_radar_physics::HydrometeorKind::Hail,
        mass: &["QHAIL"],
        number: &["QNHAIL"],
        volume: &["QVHAIL"],
    },
];

fn build_compact_polar_fields(
    file: &WrfFile,
    timeidx: usize,
    expected: usize,
    progress: &dyn Fn(&str),
) -> Result<(CompactPolarFields, Vec<f32>), String> {
    let present_fields: Vec<String> = MICROPHYSICS_FIELD_NAMES
        .iter()
        .filter(|name| file.has_var(name))
        .map(|name| (*name).to_string())
        .collect();
    let scheme_id = file.global_attr_i32("MP_PHYSICS").ok();
    let profile = crate::wrf_radar_physics::detect_scheme(scheme_id, &present_fields);
    if profile.capability == crate::wrf_radar_physics::MicrophysicsCapability::Unsupported {
        return Err(format!(
            "dual-pol unavailable: {} cannot be represented by the bulk-category operator",
            profile.name
        ));
    }

    let temperature = file
        .temperature(timeidx)
        .map_err(|error| format!("dual-pol temperature: {error}"))?
        .iter()
        .map(|value| *value as f32)
        .collect::<Vec<_>>();
    file.clear_cache();
    if temperature.len() != expected {
        return Err(format!(
            "dual-pol temperature has {} values, expected {expected}",
            temperature.len()
        ));
    }

    let qv = if file.has_var("QVAPOR") {
        file.read_var("QVAPOR", timeidx)
            .map_err(|error| format!("dual-pol QVAPOR: {error}"))?
            .into_iter()
            .map(|value| value as f32)
            .collect::<Vec<_>>()
    } else {
        vec![0.0; expected]
    };
    let pressure = file
        .full_pressure(timeidx)
        .map_err(|error| format!("dual-pol pressure: {error}"))?;
    if pressure.len() != expected || qv.len() != expected {
        file.clear_cache();
        return Err("dual-pol pressure/QVAPOR grid shape mismatch".to_string());
    }
    let air_density = pressure
        .iter()
        .zip(&temperature)
        .zip(&qv)
        .map(|((&pressure, &temperature), &qv)| {
            crate::wrf_radar_physics::air_density(pressure as f32, temperature, qv)
        })
        .collect::<Vec<_>>();
    drop(pressure);
    file.clear_cache();

    let mut compact = CompactPolarFields::new(expected, profile.clone(), present_fields);
    let mut zh_linear = vec![0.0f32; expected];
    let mut species_used = 0usize;
    for spec in SPECIES_READ_SPECS {
        let Some(mass_name) = first_existing_var(file, spec.mass) else {
            continue;
        };
        progress(&format!("scattering {mass_name}…"));
        let number = read_optional_first_var_f32(file, timeidx, spec.number, expected)?;
        let volume = read_optional_first_var_f32(file, timeidx, spec.volume, expected)?;
        let mass = file
            .read_var(mass_name, timeidx)
            .map_err(|error| format!("dual-pol {mass_name}: {error}"))?;
        if mass.len() != expected {
            return Err(format!(
                "dual-pol {mass_name} has {} values, expected {expected}",
                mass.len()
            ));
        }
        species_used += 1;
        for index in 0..expected {
            let q = mass[index] as f32;
            if !q.is_finite() || q <= 0.0 {
                continue;
            }
            let contribution = crate::wrf_radar_physics::bulk_sband_contribution(
                crate::wrf_radar_physics::BulkSpeciesInput {
                    kind: spec.kind,
                    q_kgkg: q,
                    number_per_kg: number.as_ref().map(|values| values[index]),
                    volume_m3_per_kg: volume.as_ref().map(|values| values[index]),
                    temperature_k: temperature[index],
                    air_density_kgm3: air_density[index],
                },
                &profile,
            );
            if contribution.zh <= 0.0 {
                continue;
            }
            let mut accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
            if zh_linear[index] > 0.0 {
                accumulator.add(1.0, compact.contribution_at(index, zh_linear[index]));
            }
            accumulator.add(1.0, contribution);
            let combined = accumulator.finalize();
            zh_linear[index] = combined.zh;
            compact.store(index, combined);
        }
    }
    if species_used == 0 || zh_linear.iter().all(|value| *value <= 0.0) {
        return Err(format!(
            "dual-pol unavailable: {} contains no usable bulk hydrometeor mass fields",
            profile.name
        ));
    }
    let zh_dbz = zh_linear
        .into_iter()
        .map(|value| {
            if value.is_finite() && value > 0.0 {
                crate::wrf_radar_physics::z_to_dbz(value)
            } else {
                f32::NAN
            }
        })
        .collect();
    Ok((compact, zh_dbz))
}

fn first_existing_var<'a>(file: &WrfFile, names: &'a [&str]) -> Option<&'a str> {
    names.iter().copied().find(|name| file.has_var(name))
}

fn read_optional_first_var_f32(
    file: &WrfFile,
    timeidx: usize,
    names: &[&str],
    expected: usize,
) -> Result<Option<Vec<f32>>, String> {
    let Some(name) = first_existing_var(file, names) else {
        return Ok(None);
    };
    let values = file
        .read_var(name, timeidx)
        .map_err(|error| format!("dual-pol {name}: {error}"))?;
    if values.len() != expected {
        return Err(format!(
            "dual-pol {name} has {} values, expected {expected}",
            values.len()
        ));
    }
    Ok(Some(values.into_iter().map(|value| value as f32).collect()))
}

fn read_compact_tke(file: &WrfFile, timeidx: usize, expected: usize) -> Option<Vec<u8>> {
    let name = first_existing_var(file, &["TKE_PBL", "TKE", "QKE"])?;
    let values = file.read_var(name, timeidx).ok()?;
    if values.len() != expected {
        return None;
    }
    let qke_halving = if name == "QKE" { 0.5 } else { 1.0 };
    Some(
        values
            .into_iter()
            .map(|value| quantize_u8((value as f32 * qke_halving).max(0.0), 0.1))
            .collect(),
    )
}

/// Earth-relative winds. `uvmet` returns `[u_earth.., v_earth..]`
/// (2 * nz * cells); fall back to grid-relative `ua`/`va` if unavailable.
/// Extracted verbatim from the original inline logic so the values match.
fn read_earth_relative_winds(
    file: &WrfFile,
    timeidx: usize,
    expected: usize,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    match getvar(file, "uvmet", Some(timeidx), &ComputeOpts::default()) {
        Ok(uvmet) if uvmet.data.len() == 2 * expected => {
            let (ue, ve) = uvmet.data.split_at(expected);
            Ok((to_f32(ue), to_f32(ve)))
        }
        _ => {
            let ua = read_3d(file, "ua", timeidx, expected)?;
            let va = read_3d(file, "va", timeidx, expected)?;
            Ok((ua, va))
        }
    }
}

fn read_terrain_m(file: &WrfFile, timeidx: usize, cells: usize) -> Vec<f32> {
    file.terrain(timeidx)
        .map(|ter| ter.iter().map(|value| *value as f32).collect::<Vec<_>>())
        .unwrap_or_else(|_| vec![0.0; cells])
}

/// Join a scoped read thread, turning a thread panic into a readable error.
fn join_read<T>(
    handle: std::thread::ScopedJoinHandle<'_, Result<T, String>>,
    what: &str,
) -> Result<T, String> {
    match handle.join() {
        Ok(inner) => inner,
        Err(_) => Err(format!("WRF {what} read thread panicked")),
    }
}

fn read_reflectivity(
    file: &WrfFile,
    timeidx: usize,
    expected: usize,
    operator: ReflectivityOperator,
) -> Result<(Vec<f32>, &'static str), String> {
    // The same decision the tests assert through `planned_ref_source`: only the
    // model-native operator reads REFL_10CM, and only when the file carries it.
    if planned_ref_source(operator, file.has_var("REFL_10CM")) == REFL_10CM_SOURCE
        && let Ok(raw) = file.read_var("REFL_10CM", timeidx)
        && raw.len() == expected
    {
        return Ok((to_f32(&raw), REFL_10CM_SOURCE));
    }
    // wrf-core `dbz` = CALCDBZ (Stoelinga 2005), the same source BowEcho's
    // composite reflectivity uses. Constant intercepts / no bright-band
    // correction (ComputeOpts default) to match that composite exactly.
    // The classic-Stoelinga operator forces this path even when the file
    // carries REFL_10CM; the source label records which case applied.
    let dbz = read_3d(file, "dbz", timeidx, expected)
        .map_err(|err| format!("no REFL_10CM and computed dbz failed: {err}"))?;
    Ok((dbz, operator.computed_dbz_source()))
}

fn read_3d(
    file: &WrfFile,
    name: &str,
    timeidx: usize,
    expected: usize,
) -> Result<Vec<f32>, String> {
    let out = getvar(file, name, Some(timeidx), &ComputeOpts::default())
        .map_err(|err| format!("read WRF {name}: {err}"))?;
    if out.data.len() != expected {
        return Err(format!(
            "WRF {name} has {} values, expected {expected}",
            out.data.len()
        ));
    }
    Ok(to_f32(&out.data))
}

fn to_f32(values: &[f64]) -> Vec<f32> {
    values.iter().map(|value| *value as f32).collect()
}

/// Build one synthetic [`RadarVolume`] from pre-read [`WrfRadarFields`].
///
/// `valid_time` is the volume's scan time (the WRF forecast valid time), which
/// keys the frame in a loop.
pub fn build_synthetic_volume(
    fields: &WrfRadarFields,
    valid_time: DateTime<Utc>,
    config: &SyntheticRadarConfig,
) -> RadarVolume {
    build_synthetic_volume_reporting(fields, valid_time, config, &|_| {})
}

/// As [`build_synthetic_volume`], but streams a per-tilt progress label so the
/// UI shows "building tilt k/n…" instead of freezing while the polar volume is
/// traced. The per-tilt work itself is unchanged.
pub fn build_synthetic_volume_reporting(
    fields: &WrfRadarFields,
    valid_time: DateTime<Utc>,
    config: &SyntheticRadarConfig,
    progress: &dyn Fn(&str),
) -> RadarVolume {
    try_build_synthetic_volume_reporting(fields, valid_time, config, progress)
        .expect("validated single-scene synthetic-radar render")
}

/// Exact-geometry replay products returned together so the caller cannot
/// accidentally compare against a separately regridded synthetic volume.
pub struct ExactReplayProducts {
    pub observed: Arc<RadarVolume>,
    pub simulated: RadarVolume,
    pub difference: RadarVolume,
    pub unavailable_observed_moments: Vec<UnavailableObservedMoment>,
}

fn is_dual_pol_replay_moment(moment: &MomentType) -> bool {
    matches!(
        moment,
        MomentType::DifferentialReflectivity
            | MomentType::CorrelationCoefficient
            | MomentType::DifferentialPhase
            | MomentType::SpecificDifferentialPhase
    )
}

fn normalize_replay_config_for_observed(config: &mut SyntheticRadarConfig, observed: &RadarVolume) {
    // The explicit Replay action intentionally replaces custom instrument
    // ambiguity with source radial Nyquist. Direct hand-built contradictory
    // configs still fail closed in `validate_science_contract`.
    config.coupled_single_prf_estimator = false;
    config.emit_stage_diagnostics = false;
    config.fold_velocity = false;
    if observed
        .cuts
        .iter()
        .any(|cut| cut.moments.contains_key(&MomentType::SpectrumWidth))
    {
        config.spectrum_width = true;
    }
    if observed
        .cuts
        .iter()
        .flat_map(|cut| cut.moments.keys())
        .any(is_dual_pol_replay_moment)
    {
        config.dual_pol = true;
    }
}

/// Render the WRF atmosphere on an observed scan's exact acquisition geometry
/// and return both the simulated and `simulated - observed` volumes. This is a
/// scan replay, not a reconstruction of the numbered VCP carried by the file.
pub fn build_exact_replay_products(
    fields: &WrfRadarFields,
    observed: Arc<RadarVolume>,
    config: &SyntheticRadarConfig,
) -> Result<ExactReplayProducts, String> {
    let template = ExactScanTemplate::from_volume(&observed)
        .map_err(|error| format!("extract exact observed scan template: {error}"))?;
    let mut replay_config = config.clone();
    normalize_replay_config_for_observed(&mut replay_config, &observed);
    replay_config.exact_replay_template = Some(Arc::new(template));
    let simulated = try_build_synthetic_volume_reporting(
        fields,
        observed.volume_time,
        &replay_config,
        &|_| {},
    )?;
    let overlap = build_difference_volume_overlap(&observed, &simulated)
        .map_err(|error| format!("build exact-geometry replay difference: {error}"))?;
    Ok(ExactReplayProducts {
        observed,
        simulated,
        difference: overlap.volume,
        unavailable_observed_moments: overlap.unavailable_observed_moments,
    })
}

/// Background form of [`build_exact_replay_products`]. The observed volume is
/// retained by `Arc` through completion and returned in the product bundle.
pub fn spawn_exact_replay_worker(
    fields: Arc<WrfRadarFields>,
    observed: Arc<RadarVolume>,
    config: SyntheticRadarConfig,
) -> Receiver<Result<ExactReplayProducts, String>> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("wrf-exact-radar-replay".to_string())
        .spawn(move || {
            let result = build_exact_replay_products(&fields, observed, &config);
            let _ = tx.send(result);
        })
        .expect("spawn exact WRF radar replay worker");
    rx
}

fn try_build_synthetic_volume_reporting(
    fields: &WrfRadarFields,
    valid_time: DateTime<Utc>,
    config: &SyntheticRadarConfig,
    progress: &dyn Fn(&str),
) -> Result<RadarVolume, String> {
    build_synthetic_volume_reporting_inner(fields, valid_time, config, progress, None)
}

struct TemporalRenderContext<'a> {
    neighbor: &'a WrfRadarFields,
    plan: &'a TemporalScenePlan,
}

fn build_synthetic_volume_reporting_temporal(
    fields: &WrfRadarFields,
    neighbor: &WrfRadarFields,
    valid_time: DateTime<Utc>,
    config: &SyntheticRadarConfig,
    progress: &dyn Fn(&str),
    plan: &TemporalScenePlan,
) -> Result<RadarVolume, String> {
    build_synthetic_volume_reporting_inner(
        fields,
        valid_time,
        config,
        progress,
        Some(TemporalRenderContext { neighbor, plan }),
    )
}

fn build_synthetic_volume_reporting_inner(
    fields: &WrfRadarFields,
    valid_time: DateTime<Utc>,
    config: &SyntheticRadarConfig,
    progress: &dyn Fn(&str),
    temporal: Option<TemporalRenderContext<'_>>,
) -> Result<RadarVolume, String> {
    config.validate_science_contract()?;
    if let Some(template) = config.exact_replay_template.as_deref() {
        return build_exact_replay_volume(
            fields,
            valid_time,
            config,
            template,
            progress,
            temporal.as_ref(),
        );
    }
    let coupled_instrument = resolve_coupled_instrument(config)?;
    let cells = fields.cells();
    let center = fields.center_cell();
    let site_lat = config
        .site_lat_deg
        .unwrap_or_else(|| fields.lat[center] as f64);
    let site_lon = config
        .site_lon_deg
        .unwrap_or_else(|| fields.lon[center] as f64);
    let antenna_msl = config.antenna_msl_m.unwrap_or_else(|| {
        // Default antenna height: MODEL terrain under the ANTENNA plus a
        // short tower — a virtual site placed off-centre (explicit lat/lon
        // or a real NEXRAD id) must stand on its own ground, not the domain
        // centre's. A site outside the domain falls back to centre terrain.
        let site_cell = fields
            .lut
            .lookup(site_lat as f32, site_lon as f32)
            .unwrap_or(center);
        fields.terrain_m[site_cell] as f64 + DEFAULT_TOWER_M
    });

    let naz = config.azimuth_count.max(1);
    // Match-gate-to-grid resolves against the file's DX (carried on `fields`);
    // off, this is the configured `gate_spacing_m`, so the build is unchanged.
    let spacing = effective_gate_spacing(config, fields.dx_m).max(1.0);
    let gate_count = ((config.max_range_m / spacing).floor() as usize).max(1);
    let gate_range = GateRange {
        first_gate_m: 0,
        gate_spacing_m: spacing.round() as i32,
        gate_count,
    };

    let mut site = RadarSite::new(config.site_id.clone());
    site.name = config.site_name.clone();
    site.latitude_deg = Some(site_lat as f32);
    site.longitude_deg = Some(site_lon as f32);
    site.elevation_m = Some(antenna_msl as f32);

    let mut volume = RadarVolume::new(site, valid_time);
    let scan_legs = config.physical_scan_legs();
    let microphysics_inventory = fields
        .property_scattering
        .as_ref()
        .map(|scene| {
            scene
                .source_fields()
                .iter()
                .map(app_ui::wrf_property_reader::SourceFieldProvenance::source_name)
                .collect::<Vec<_>>()
                .join(",")
        })
        .or_else(|| {
            fields.raw_property_scene.as_ref().map(|scene| {
                scene
                    .source_fields()
                    .iter()
                    .map(app_ui::wrf_property_reader::SourceFieldProvenance::source_name)
                    .collect::<Vec<_>>()
                    .join(",")
            })
        })
        .or_else(|| {
            fields
                .polarimetric
                .as_ref()
                .map(|polar| polar.present_fields.join(","))
        })
        .unwrap_or_default();
    let named_vcp = config.scan_strategy.definition();
    if let Some(definition) = named_vcp {
        volume.vcp = Some(VcpInfo {
            pattern: definition.vcp.number(),
        });
    }
    let mut forward_operator_config = format!(
        "mode={:?}; reflectivity_sampling={:?}; beam_integration={:?}; \
         fall_speed={}; terrain_blockage={}; spectrum_width={}; dual_pol={}; polarimetric_kernel={:?}; \
         propagation={}; quality_fields={}; minimum_model_coverage_fraction={:.4}; microphysics_fields={microphysics_inventory}",
        config.simulation_mode,
        config.reflectivity_sampling,
        config.beam_integration,
        config.terminal_fall_speed,
        config.terrain_blockage,
        config.spectrum_width,
        config.dual_pol,
        config.polarimetric_kernel,
        config.propagation,
        config.emit_quality_fields,
        config.clamped_minimum_model_coverage_fraction(),
    );
    if let Some(polar) = fields.polarimetric.as_ref() {
        use std::fmt::Write as _;
        let audit = &polar.precision_audit;
        let _ = write!(
            forward_operator_config,
            "; compact_dual_pol_audit={}; compact_dual_pol_clamps={}; compact_dual_pol_quantized_to_zero={}; compact_dual_pol_max_zv_relative_error={:.6}; compact_dual_pol_max_covariance_relative_error={:.6}; compact_dual_pol_max_av_error_db_km={:.6}; compact_dual_pol_max_fall_variance_error_m2s2={:.6}",
            audit.provenance_fragment(),
            audit.total_clamps(),
            audit.total_quantized_to_zero(),
            audit.max_zv_relative_error,
            audit.max_covariance_magnitude_relative_error,
            audit.max_av_abs_error_db_km,
            audit.max_fall_variance_abs_error_m2s2,
        );
    }
    if let Some(coupled) = coupled_instrument.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(
            forward_operator_config,
            "; estimator=CustomSinglePrfV1; frequency_hz={:.3}; wavelength_m={:.12}; \
             prf_hz={:.6}; prt_s={:.9}; nyquist_mps={:.6}; unambiguous_range_km={:.6}; \
             pulse_range_response=matched_filter_triangular; pulse_range_resolution_m={:.6}; \
             dwell_ms={:.3}; transmitted_pulses={}; independent_samples={:.3}; \
             minimum_snr_db={:.3}; velocity_folding=always_prf_derived; \
             stage_diagnostics={}; stage_definitions={} | {} | {}",
            coupled.timing.frequency_hz,
            coupled.timing.wavelength_m,
            coupled.timing.prf_hz,
            coupled.timing.prt_s,
            coupled.timing.nyquist_velocity_mps,
            coupled.timing.unambiguous_range_m / 1_000.0,
            coupled.range_resolution_m,
            coupled.sampling.dwell_s * 1_000.0,
            coupled.sampling.transmitted_pulses,
            coupled.sampling.independent_samples,
            coupled.estimator_config.minimum_snr_db,
            config.emit_stage_diagnostics,
            IDEAL_STAGE_DEFINITION,
            MEASURED_STAGE_DEFINITION,
            PRESENTED_STAGE_DEFINITION,
        );
    }
    if let Some(definition) = named_vcp {
        use std::fmt::Write as _;
        let _ = write!(
            forward_operator_config,
            "; scan_strategy=Build24Vcp{}; vcp_source={} rev {}; physical_rows={}; config_fingerprint={:016x}",
            definition.vcp.number(),
            definition.source.document_number,
            definition.source.revision,
            definition.rows.len(),
            config.data_fingerprint(),
        );
    }
    if let Some(scene) = fields.property_scattering.as_deref() {
        use std::fmt::Write as _;
        let provenance = scene.provenance();
        let tables = provenance
            .tables
            .iter()
            .map(|table| format!("{}={}@{}", table.role, table.table_id, table.file_sha256))
            .collect::<Vec<_>>()
            .join(",");
        let counts = provenance.counts;
        let _ = write!(
            forward_operator_config,
            "; tmatrix_status={}; tmatrix_frequency_hz={:.0}; tmatrix_orientation={:?}; \
             tmatrix_rain_mode={:?}; tmatrix_fall_policy_closed={:?}; \
             tmatrix_fall_policy_wet={:?}; tmatrix_tables={tables}; \
             tmatrix_populations=source_cells:{},characteristic_frozen:{},scheme_native_psd:{},dry_frozen:{},wet_frozen:{},residual_rain:{},standalone_rain:{}; \
             tmatrix_interpolation=source_cell_closure_then_additive_space_beam_and_time",
            provenance.status,
            provenance.frequency_hz,
            provenance.orientation,
            provenance.rain_mode,
            provenance.fall_moment_policy.closed_category,
            provenance.fall_moment_policy.diagnostic_wet_category,
            counts.source_cells,
            counts.characteristic_frozen_populations,
            counts.scheme_native_psd_populations,
            counts.dry_frozen_populations,
            counts.wet_frozen_populations,
            counts.residual_rain_populations,
            counts.standalone_rain_populations,
        );
    }
    if let Some(scene) = fields.raw_property_scene.as_deref() {
        use std::fmt::Write as _;
        let _ = write!(
            forward_operator_config,
            "; tmatrix_status=research_only_unvalidated; tmatrix_frequency_hz={:.0}; \
             tmatrix_raw_mp_physics={}; tmatrix_raw_source={}:time{}; \
             tmatrix_interpolation=raw_state_linear_spatial_and_temporal_preclosure_then_single_scattering_evaluation",
            app_ui::wrf_tmatrix_scene::PROPERTY_TMATRIX_FREQUENCY_HZ,
            scene.microphysics_scheme_id(),
            scene.identity().source_identity.0.as_str(),
            scene.identity().time_index,
        );
    }
    volume.metadata = VolumeMetadata {
        source_path: None,
        archive_version: Some("simulated-wrf".to_string()),
        compression: None,
        message_count: 0,
        decoded_radial_count: 0,
        skipped_message_count: 0,
        scan_mode: Some(ScanMode::Ppi),
        radar_frequency_mhz: Some(config.radar_frequency_mhz),
        beam_width_h_deg: Some(config.beam_width_deg),
        beam_width_v_deg: Some(config.beam_width_deg),
        pulse_width_us: Some(config.pulse_width_us),
        // Appendix C supplies numbered PRF codes, not Hz. Never turn those
        // codes into a fictitious standard PRT/unambiguous range.
        prt_s: coupled_instrument.as_ref().map_or_else(
            || {
                named_vcp
                    .is_none()
                    .then(|| {
                        (config.prf_hz.is_finite() && config.prf_hz > 0.0)
                            .then_some(1.0 / config.prf_hz)
                    })
                    .flatten()
            },
            |coupled| Some(coupled.timing.prt_s as f32),
        ),
        unambiguous_range_km: coupled_instrument.as_ref().map_or_else(
            || {
                named_vcp
                    .is_none()
                    .then(|| {
                        (config.prf_hz.is_finite() && config.prf_hz > 0.0)
                            .then_some(299_792.47 / (2.0 * config.prf_hz))
                    })
                    .flatten()
            },
            |coupled| Some((coupled.timing.unambiguous_range_m / 1_000.0) as f32),
        ),
        scan_name: Some(if let Some(definition) = named_vcp {
            format!("Build 24 VCP {} baseline", definition.vcp.number())
        } else {
            match config.scan_timing {
                ScanTiming::InstantaneousTruth => "BowEcho instantaneous model volume".to_string(),
                ScanTiming::TimedVolume => "BowEcho timed synthetic volume".to_string(),
            }
        }),
        scan_id: Some(if let Some(definition) = named_vcp {
            format!(
                "bowecho-build24-vcp{}-{}-rev{}-{}rows",
                definition.vcp.number(),
                BUILD_24_SOURCE.document_number,
                BUILD_24_SOURCE.revision,
                definition.rows.len(),
            )
        } else {
            "bowecho-synthetic-14-cut".to_string()
        }),
        vcp_source_document: named_vcp
            .map(|definition| definition.source.document_number.to_owned()),
        vcp_source_revision: named_vcp.map(|definition| definition.source.revision.to_owned()),
        vcp_source_rda_build: named_vcp.map(|definition| definition.source.rda_build.to_owned()),
        vcp_source_figure: named_vcp.map(|definition| definition.source_figure.to_owned()),
        vcp_pulse_length: named_vcp.map(|definition| match definition.pulse_length {
            PulseLength::Short => "short".to_owned(),
            PulseLength::Long => "long".to_owned(),
        }),
        vcp_adaptations: named_vcp.map(|_| BUILD_24_NO_ADAPTATIONS_CAVEAT.to_owned()),
        scan_legs: named_vcp
            .map(|_| scan_legs.iter().map(scan_leg_metadata).collect())
            .unwrap_or_default(),
        polarization: Some(if config.dual_pol && fields.has_polarimetric_input() {
            "simultaneous horizontal/vertical".to_string()
        } else {
            "horizontal".to_string()
        }),
        calibration: Some(format!(
            "ZDR bias={:.2} dB; system PhiDP={:.2} deg",
            config.zdr_bias_db, config.system_phidp_deg
        )),
        forward_operator: Some(if coupled_instrument.is_some() {
            "BowEcho WRF polar-volume forward operator v4 (coupled single-PRF estimator)"
                .to_string()
        } else {
            "BowEcho WRF polar-volume forward operator v3".to_string()
        }),
        forward_operator_config: Some(forward_operator_config),
        source_model: Some("WRF".to_string()),
        microphysics_scheme: fields
            .property_scattering
            .as_ref()
            .map(|scene| format!("WRF mp_physics={}", scene.microphysics_scheme_id()))
            .or_else(|| {
                fields
                    .raw_property_scene
                    .as_ref()
                    .map(|scene| format!("WRF mp_physics={}", scene.microphysics_scheme_id()))
            })
            .or_else(|| {
                fields
                    .polarimetric
                    .as_ref()
                    .map(|polar| format!("{} ({:?})", polar.profile.name, polar.profile.capability))
            }),
        scattering_model: if config.dual_pol && fields.has_polarimetric_input() {
            Some(match config.polarimetric_kernel {
                PolarimetricKernel::BulkRayleighV1 => format!(
                    "{} (scheme-aware bulk S-band Rayleigh; not T-matrix)",
                    crate::wrf_radar_physics::bulk_sband_model_id()
                ),
                PolarimetricKernel::PropertyTMatrixResearchV1 => {
                    "PyTMatrix 0.3.3 property-aware characteristic-particle LUT v1 (research_only_unvalidated; not PSD-integrated)".to_string()
                }
            })
        } else {
            None
        },
    };

    // One deterministic seed per forecast frame, folded into every clutter
    // draw so the same hour rebuilds bit-identically (no loop shimmer) while
    // distinct frames get a distinct clutter pattern (the community operator's
    // per-time-step variation). Site id + valid time; the tilt index is mixed
    // in per cut. Cheap to compute even when clutter is off (unused then).
    let frame_seed = clutter_frame_seed(&config.site_id, valid_time);

    let terrain_horizon = config.terrain_blockage.then(|| {
        progress("tracing terrain horizon…");
        TerrainHorizon::build(
            fields,
            site_lat,
            site_lon,
            antenna_msl,
            naz,
            gate_count,
            0,
            spacing,
        )
    });

    let mut decoded_radials = 0usize;
    let mut cut_start_ms = 0i32;
    let tilt_total = scan_legs.len();
    for (cut_index, leg) in scan_legs.iter().enumerate() {
        let elevation_deg = leg.elevation_deg;
        progress(&format!(
            "building tilt {}/{tilt_total} ({elevation_deg:.1}°)…",
            cut_index + 1
        ));
        let cut = build_cut(
            fields,
            temporal.as_ref(),
            cells,
            site_lat,
            site_lon,
            antenna_msl,
            elevation_deg,
            cut_index,
            naz,
            spacing,
            gate_range.clone(),
            config,
            leg,
            tilt_total,
            frame_seed,
            terrain_horizon.as_ref(),
            cut_start_ms,
            coupled_instrument.as_ref(),
            None,
        )?;
        decoded_radials += cut.radials.len();
        volume.cuts.push(cut);
        if matches!(config.scan_timing, ScanTiming::TimedVolume) {
            cut_start_ms = advance_cut_start_ms(config, leg, cut_start_ms);
        }
    }
    volume.metadata.decoded_radial_count = decoded_radials;
    Ok(volume)
}

fn build_exact_replay_volume(
    fields: &WrfRadarFields,
    model_valid_time: DateTime<Utc>,
    config: &SyntheticRadarConfig,
    template: &ExactScanTemplate,
    progress: &dyn Fn(&str),
    temporal: Option<&TemporalRenderContext<'_>>,
) -> Result<RadarVolume, String> {
    let cells = fields.cells();
    let site_lat = f64::from(
        template
            .site
            .latitude_deg
            .ok_or_else(|| format!("exact replay site {} has no latitude", template.site.id))?,
    );
    let site_lon = f64::from(
        template
            .site
            .longitude_deg
            .ok_or_else(|| format!("exact replay site {} has no longitude", template.site.id))?,
    );
    let center = fields.center_cell();
    let antenna_msl = template.site.elevation_m.map(f64::from).unwrap_or_else(|| {
        let site_cell = fields
            .lut
            .lookup(site_lat as f32, site_lon as f32)
            .unwrap_or(center);
        f64::from(fields.terrain_m[site_cell]) + DEFAULT_TOWER_M
    });
    let geometry_fingerprint = template.geometry_fingerprint();
    let mut volume = RadarVolume::new(template.site.clone(), template.volume_time);
    volume.vcp = template.vcp.clone();
    volume.metadata = VolumeMetadata {
        archive_version: Some("simulated-wrf-exact-observed-replay-v1".to_string()),
        decoded_radial_count: template.cuts.iter().map(|cut| cut.rays.len()).sum(),
        scan_mode: Some(ScanMode::Ppi),
        radar_frequency_mhz: Some(config.radar_frequency_mhz),
        beam_width_h_deg: Some(config.beam_width_deg),
        beam_width_v_deg: Some(config.beam_width_deg),
        pulse_width_us: Some(config.pulse_width_us),
        prt_s: template.source_prt_s,
        unambiguous_range_km: template.source_unambiguous_range_km,
        scan_name: Some("Exact observed scan-geometry replay (not a VCP reconstruction)".into()),
        scan_id: Some(format!("bowecho-exact-replay-{geometry_fingerprint:016x}")),
        polarization: Some(if config.dual_pol && fields.has_polarimetric_input() {
            "simultaneous horizontal/vertical".to_string()
        } else {
            "horizontal".to_string()
        }),
        calibration: Some(format!(
            "ZDR bias={:.2} dB; system PhiDP={:.2} deg",
            config.zdr_bias_db, config.system_phidp_deg
        )),
        forward_operator: Some("BowEcho WRF exact observed-geometry replay v1".to_string()),
        forward_operator_config: Some(format!(
            "scan_source=exact_observed_geometry; geometry_fingerprint={geometry_fingerprint:016x}; \
             vcp_reconstruction=false; cut_order=source; split_cuts=preserved; missing_sectors=preserved; \
             radial_geometry=source; moment_availability=source; acquisition_offsets=source_normalized_utc; \
             radial_nyquist=source; volume_prt=source; source_time_encoding={:?}; \
             model_valid_time={}; config_fingerprint={:016x}",
            template.time_encoding,
            model_valid_time.to_rfc3339(),
            config.data_fingerprint(),
        )),
        source_model: Some("WRF".to_string()),
        microphysics_scheme: fields
            .property_scattering
            .as_ref()
            .map(|scene| format!("WRF mp_physics={}", scene.microphysics_scheme_id()))
            .or_else(|| {
                fields
                    .raw_property_scene
                    .as_ref()
                    .map(|scene| format!("WRF mp_physics={}", scene.microphysics_scheme_id()))
            })
            .or_else(|| {
                fields
                    .polarimetric
                    .as_ref()
                    .map(|polar| format!("{} ({:?})", polar.profile.name, polar.profile.capability))
            }),
        ..VolumeMetadata::default()
    };

    let frame_seed = clutter_frame_seed(&template.site.id, model_valid_time);
    let mut render_config = config.clone();
    render_config.exact_replay_template = None;
    render_config.scan_strategy = SyntheticScanStrategy::CustomLegacy;
    render_config.site_id = template.site.id.clone();
    render_config.site_name = template.site.name.clone();
    render_config.site_lat_deg = Some(site_lat);
    render_config.site_lon_deg = Some(site_lon);
    render_config.antenna_msl_m = Some(antenna_msl);
    // Replay ambiguity is applied from each observed radial's own Nyquist.
    render_config.fold_velocity = false;
    render_config.coupled_single_prf_estimator = false;
    render_config.emit_stage_diagnostics = false;
    if template
        .cuts
        .iter()
        .flat_map(|cut| &cut.moments)
        .any(|plan| plan.moment == MomentType::SpectrumWidth)
    {
        render_config.spectrum_width = true;
    }
    if template
        .cuts
        .iter()
        .flat_map(|cut| &cut.moments)
        .any(|plan| is_dual_pol_replay_moment(&plan.moment))
    {
        render_config.dual_pol = true;
    }
    // Replay always retains pulse-volume support fields. They are synthetic-
    // only diagnostics and the exact-geometry difference builder ignores them.
    render_config.emit_quality_fields = true;
    let replay_leg = SyntheticScanLeg {
        elevation_deg: 0.0,
        azimuth_rate_deg_per_second: render_config.rotation_rate_deg_s,
        source_period_seconds: 0.0,
        transition_after_seconds: 0.0,
        moments: MomentCoverage::ALL,
        waveform: None,
        source_row_index: None,
        source_row: None,
    };
    let mut unavailable_observed_moments = Vec::new();

    for (cut_index, replay_cut) in template.cuts.iter().enumerate() {
        progress(&format!(
            "replaying observed cut {}/{} ({:.2} deg)...",
            cut_index + 1,
            template.cuts.len(),
            replay_cut.elevation_deg
        ));
        let mut output_cut =
            ElevationCut::new(replay_cut.elevation_deg, replay_cut.elevation_number);
        for ray in &replay_cut.rays {
            let time_offset_ms = i32::try_from(ray.acquisition_offset_ms).map_err(|_| {
                format!(
                    "exact replay cut {} radial {} acquisition offset {} ms exceeds i32",
                    cut_index + 1,
                    ray.source_radial_index + 1,
                    ray.acquisition_offset_ms
                )
            })?;
            output_cut.radials.push(Radial {
                azimuth_deg: ray.azimuth_deg,
                elevation_deg: ray.elevation_deg,
                time_offset_ms,
                gate_range: ray.gate_range.clone(),
                nyquist_velocity_mps: ray.nyquist_velocity_mps,
                radial_status: ray.radial_status,
            });
        }

        let quality_source_index = replay_cut
            .moments
            .iter()
            .position(|plan| plan.moment == MomentType::Reflectivity)
            .or_else(|| (!replay_cut.moments.is_empty()).then_some(0));
        for (moment_index, moment_plan) in replay_cut.moments.iter().enumerate() {
            let supported = match &moment_plan.moment {
                MomentType::Reflectivity | MomentType::Velocity => true,
                MomentType::SpectrumWidth => render_config.spectrum_width,
                moment if is_dual_pol_replay_moment(moment) => {
                    render_config.dual_pol && fields.has_polarimetric_input()
                }
                MomentType::Unknown(_) => true,
                _ => false,
            };
            if !supported && !matches!(moment_plan.moment, MomentType::Unknown(_)) {
                unavailable_observed_moments.push(format!(
                    "cut{}:{}",
                    cut_index + 1,
                    moment_plan.moment.short_name()
                ));
            }
            if !supported && quality_source_index != Some(moment_index) {
                continue;
            }
            let gate_count = moment_plan.gate_range.gate_count;
            let mut values = vec![f32::NAN; moment_plan.radial_indices.len() * gate_count];
            if !moment_plan.radial_indices.is_empty() {
                let mut ray_plan = Vec::with_capacity(moment_plan.radial_indices.len());
                for &source_index in &moment_plan.radial_indices {
                    let source = replay_cut.rays.get(source_index).ok_or_else(|| {
                        format!(
                            "exact replay cut {} moment {} refers to missing radial {}",
                            cut_index + 1,
                            moment_plan.moment.short_name(),
                            source_index
                        )
                    })?;
                    let sampling_offset_ms =
                        temporal.map_or(source.acquisition_offset_ms, |context| {
                            (source.acquisition_time_utc.to_owned()
                                - context.plan.anchor_time.to_owned())
                            .num_milliseconds()
                        });
                    ray_plan.push(SyntheticRayPlan {
                        source_index,
                        azimuth_deg: source.azimuth_deg,
                        elevation_deg: source.elevation_deg,
                        time_offset_ms: i32::try_from(sampling_offset_ms).map_err(|_| {
                            format!(
                                "exact replay cut {} radial {} temporal acquisition offset exceeds i32",
                                cut_index + 1,
                                source_index + 1
                            )
                        })?,
                        atmosphere_alpha: 0.0,
                        radial_status: source
                            .radial_status
                            .unwrap_or(radar_core::RadialStatus::Intermediate),
                    });
                }
                let spacing = f64::from(moment_plan.gate_range.gate_spacing_m);
                let terrain_horizon = render_config.terrain_blockage.then(|| {
                    TerrainHorizon::build(
                        fields,
                        site_lat,
                        site_lon,
                        antenna_msl,
                        // Fixed azimuth lookup sampled by each ray's actual
                        // angle; never treat irregular source indices as bins.
                        720,
                        gate_count,
                        moment_plan.gate_range.first_gate_m,
                        spacing,
                    )
                });
                let mut rendered = build_cut(
                    fields,
                    temporal,
                    cells,
                    site_lat,
                    site_lon,
                    antenna_msl,
                    f64::from(replay_cut.elevation_deg),
                    cut_index,
                    replay_cut.rays.len(),
                    spacing,
                    moment_plan.gate_range.clone(),
                    &render_config,
                    &replay_leg,
                    template.cuts.len(),
                    frame_seed,
                    terrain_horizon.as_ref(),
                    0,
                    None,
                    Some(&ray_plan),
                )?;
                if quality_source_index == Some(moment_index) {
                    for quality in QualityMoment::ALL {
                        let moment = quality.moment_type();
                        if let Some(mut grid) = rendered.moments.remove(&moment) {
                            grid.radial_indices = moment_plan.radial_indices.clone();
                            output_cut.moments.insert(moment, grid);
                        }
                    }
                }
                if let Some(grid) = rendered.moments.remove(&moment_plan.moment)
                    && let MomentStorage::F32(rendered_values) = grid.storage
                    && rendered_values.len() == values.len()
                {
                    values = rendered_values;
                }
            }
            if moment_plan.moment == MomentType::Velocity {
                for (row, &source_index) in moment_plan.radial_indices.iter().enumerate() {
                    let Some(nyquist) = replay_cut.rays[source_index]
                        .nyquist_velocity_mps
                        .filter(|value| value.is_finite() && *value > 0.0)
                    else {
                        continue;
                    };
                    for value in &mut values[row * gate_count..(row + 1) * gate_count] {
                        if value.is_finite() {
                            *value = fold_velocity_mps(*value, nyquist);
                        }
                    }
                }
            }
            if supported {
                output_cut.moments.insert(
                    moment_plan.moment.clone(),
                    f32_grid(
                        moment_plan.moment.clone(),
                        moment_plan.gate_range.clone(),
                        moment_plan.radial_indices.clone(),
                        values,
                    ),
                );
            }
        }
        volume.cuts.push(output_cut);
    }
    if !unavailable_observed_moments.is_empty()
        && let Some(provenance) = volume.metadata.forward_operator_config.as_mut()
    {
        provenance.push_str("; unavailable_observed_moments=");
        provenance.push_str(&unavailable_observed_moments.join(","));
    }
    Ok(volume)
}

/// Cumulative apparent terrain elevation for every azimuth/range cell. The
/// running maximum is the terrain horizon seen by the virtual antenna; storing
/// it once makes blockage independent of elevation-cut count.
struct TerrainHorizon {
    azimuth_count: usize,
    gate_count: usize,
    elevation_deg: Vec<f32>,
}

impl TerrainHorizon {
    #[allow(clippy::too_many_arguments)]
    fn build(
        fields: &WrfRadarFields,
        site_lat: f64,
        site_lon: f64,
        antenna_msl: f64,
        azimuth_count: usize,
        gate_count: usize,
        first_gate_m: i32,
        spacing_m: f64,
    ) -> Self {
        let rows: Vec<Vec<f32>> = (0..azimuth_count)
            .into_par_iter()
            .map(|iaz| {
                let azimuth_deg = iaz as f64 * 360.0 / azimuth_count as f64;
                let azimuth = azimuth_deg.to_radians();
                let mut horizon = f32::NEG_INFINITY;
                let mut row = Vec::with_capacity(gate_count);
                for gate in 0..gate_count {
                    let slant_m = f64::from(first_gate_m) + gate as f64 * spacing_m;
                    let ground_m = beam_ground_range_m(slant_m, 0.0);
                    if ground_m < spacing_m.max(1.0) {
                        row.push(horizon);
                        continue;
                    }
                    let east_km = ground_m * azimuth.sin() / 1_000.0;
                    let north_km = ground_m * azimuth.cos() / 1_000.0;
                    let (lat, lon) = aeqd_inverse_km(site_lat, site_lon, east_km, north_km);
                    if let Some(terrain_m) = sample_terrain(fields, lat as f32, lon as f32) {
                        let ae = radar_core::EFFECTIVE_EARTH_RADIUS_M;
                        let phi = ground_m / ae;
                        let terrain_radius = ae + f64::from(terrain_m);
                        let horizontal = terrain_radius * phi.sin();
                        let vertical = terrain_radius * phi.cos() - (ae + antenna_msl);
                        let apparent = vertical.atan2(horizontal).to_degrees() as f32;
                        if apparent.is_finite() {
                            horizon = horizon.max(apparent);
                        }
                    }
                    row.push(horizon);
                }
                row
            })
            .collect();
        Self {
            azimuth_count,
            gate_count,
            elevation_deg: rows.into_iter().flatten().collect(),
        }
    }

    fn at(&self, azimuth_deg: f64, gate: usize) -> f32 {
        if self.azimuth_count == 0 || self.gate_count == 0 {
            return f32::NEG_INFINITY;
        }
        let az = azimuth_deg.rem_euclid(360.0) * self.azimuth_count as f64 / 360.0;
        let lo = az.floor() as usize % self.azimuth_count;
        let hi = (lo + 1) % self.azimuth_count;
        let t = (az - az.floor()) as f32;
        let gate = gate.min(self.gate_count - 1);
        let a = self.elevation_deg[lo * self.gate_count + gate];
        let b = self.elevation_deg[hi * self.gate_count + gate];
        if a.is_finite() && b.is_finite() {
            a + t * (b - a)
        } else if a.is_finite() {
            a
        } else {
            b
        }
    }
}

fn sample_terrain(fields: &WrfRadarFields, lat: f32, lon: f32) -> Option<f32> {
    let mut weighted = 0.0f32;
    let mut weight_sum = 0.0f32;
    for (column, weight) in horizontal_stencil(fields, lat, lon)? {
        let value = fields.terrain_m.get(column).copied()?;
        if weight > 0.0 && value.is_finite() {
            weighted += weight * value;
            weight_sum += weight;
        }
    }
    (weight_sum > 0.0).then_some(weighted / weight_sum)
}

#[derive(Clone, Copy)]
struct QuadraturePoint {
    az_sigma: f64,
    el_sigma: f64,
    range_gate: f64,
    weight: f64,
}

const CENTER_QUADRATURE: [QuadraturePoint; 1] = [QuadraturePoint {
    az_sigma: 0.0,
    el_sigma: 0.0,
    range_gate: 0.0,
    weight: 1.0,
}];

// Center plus all eight corners of a symmetric 3-D pulse cubature. The center
// receives four times one corner's weight; normalization happens per gate.
const BALANCED_QUADRATURE: [QuadraturePoint; 9] = [
    QuadraturePoint {
        az_sigma: 0.0,
        el_sigma: 0.0,
        range_gate: 0.0,
        weight: 4.0,
    },
    QuadraturePoint {
        az_sigma: -1.0,
        el_sigma: -1.0,
        range_gate: -0.35,
        weight: 1.0,
    },
    QuadraturePoint {
        az_sigma: -1.0,
        el_sigma: -1.0,
        range_gate: 0.35,
        weight: 1.0,
    },
    QuadraturePoint {
        az_sigma: -1.0,
        el_sigma: 1.0,
        range_gate: -0.35,
        weight: 1.0,
    },
    QuadraturePoint {
        az_sigma: -1.0,
        el_sigma: 1.0,
        range_gate: 0.35,
        weight: 1.0,
    },
    QuadraturePoint {
        az_sigma: 1.0,
        el_sigma: -1.0,
        range_gate: -0.35,
        weight: 1.0,
    },
    QuadraturePoint {
        az_sigma: 1.0,
        el_sigma: -1.0,
        range_gate: 0.35,
        weight: 1.0,
    },
    QuadraturePoint {
        az_sigma: 1.0,
        el_sigma: 1.0,
        range_gate: -0.35,
        weight: 1.0,
    },
    QuadraturePoint {
        az_sigma: 1.0,
        el_sigma: 1.0,
        range_gate: 0.35,
        weight: 1.0,
    },
];

const fn make_reference_quadrature() -> [QuadraturePoint; 27] {
    let coordinates = [-1.0, 0.0, 1.0];
    let weights = [1.0, 2.0, 1.0];
    let mut points = [CENTER_QUADRATURE[0]; 27];
    let mut index = 0usize;
    let mut iaz = 0usize;
    while iaz < 3 {
        let mut iel = 0usize;
        while iel < 3 {
            let mut irange = 0usize;
            while irange < 3 {
                points[index] = QuadraturePoint {
                    az_sigma: coordinates[iaz],
                    el_sigma: coordinates[iel],
                    range_gate: 0.35 * coordinates[irange],
                    weight: weights[iaz] * weights[iel] * weights[irange],
                };
                index += 1;
                irange += 1;
            }
            iel += 1;
        }
        iaz += 1;
    }
    points
}

const REFERENCE_QUADRATURE: [QuadraturePoint; 27] = make_reference_quadrature();

fn quadrature_points(mode: BeamIntegration) -> &'static [QuadraturePoint] {
    match mode {
        BeamIntegration::Center => &CENTER_QUADRATURE,
        BeamIntegration::Balanced => &BALANCED_QUADRATURE,
        BeamIntegration::Reference => &REFERENCE_QUADRATURE,
    }
}

struct GatePhysicalSample {
    z_linear: f32,
    velocity_mps: f32,
    spectrum_width_mps: f32,
    polar: Option<crate::wrf_radar_physics::IntrinsicPolarSample>,
}

struct GateSampleResult {
    physical: Option<GatePhysicalSample>,
    quality: GateQualityFractions,
}

#[allow(clippy::too_many_arguments)]
fn sample_gate_with_quality_instrument(
    fields: &WrfRadarFields,
    neighbor_fields: Option<&WrfRadarFields>,
    atmosphere_alpha: f64,
    cells: usize,
    site_lat: f64,
    site_lon: f64,
    antenna_msl: f64,
    center_azimuth_deg: f64,
    center_elevation_deg: f64,
    center_slant_m: f64,
    gate_index: usize,
    spacing_m: f64,
    config: &SyntheticRadarConfig,
    terrain_horizon: Option<&TerrainHorizon>,
    coupled_instrument: Option<&CoupledInstrumentContext>,
) -> Result<GateSampleResult, String> {
    let beam_sigma_deg = gaussian_beam_sigma_deg(f64::from(config.beam_width_deg.max(0.05)));
    let physical_points =
        coupled_instrument.and_then(|instrument| instrument.quadrature(config.beam_integration));
    let total_weight = physical_points.map_or_else(
        || {
            quadrature_points(config.beam_integration)
                .iter()
                .map(|point| point.weight)
                .sum::<f64>()
        },
        |points| points.iter().map(|point| point.weight).sum::<f64>(),
    );
    let mut valid_weight = 0.0f64;
    let mut terrain_unblocked_weight = 0.0f64;
    let mut signal_weight = 0.0f64;
    let mut sum_z = 0.0f64;
    let mut sum_z_vr = 0.0f64;
    let mut sum_z_vr2 = 0.0f64;
    let mut sum_z_subgrid_variance = 0.0f64;
    let mut polar_accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
    let mut any_polar = false;

    let mut accumulate_point =
        |az_sigma: f64, el_sigma: f64, range_offset_m: f64, weight: f64| -> Result<(), String> {
            let azimuth_deg = center_azimuth_deg + az_sigma * beam_sigma_deg;
            let elevation_deg = center_elevation_deg + el_sigma * beam_sigma_deg;
            let slant_m = (center_slant_m + range_offset_m).max(0.0);
            let beam_height_m = beam_height_above_radar_m(slant_m, elevation_deg);
            let z_msl = antenna_msl + beam_height_m;
            let ground_m = beam_ground_range_m(slant_m, elevation_deg);
            let azimuth = azimuth_deg.to_radians();
            let east_km = ground_m * azimuth.sin() / 1_000.0;
            let north_km = ground_m * azimuth.cos() / 1_000.0;
            let (lat, lon) = aeqd_inverse_km(site_lat, site_lon, east_km, north_km);
            let Some(sample) = sample_column_temporal(
                fields,
                neighbor_fields,
                atmosphere_alpha,
                cells,
                lat as f32,
                lon as f32,
                z_msl as f32,
                elevation_deg,
                config.atmosphere_time_mode,
                config.reflectivity_sampling,
            )?
            else {
                return Ok(());
            };
            valid_weight += weight;

            let blocked = terrain_horizon.is_some_and(|horizon| {
                let horizon_deg = horizon.at(azimuth_deg, gate_index);
                horizon_deg.is_finite() && elevation_deg as f32 <= horizon_deg
            });
            if blocked {
                return Ok(());
            }
            terrain_unblocked_weight += weight;

            let el = elevation_deg.to_radians();
            let fall_speed = if config.terminal_fall_speed {
                sample.polar.map_or(0.0, |polar| polar.fall_speed_mps)
            } else {
                0.0
            };
            let vr = f64::from(sample.u) * azimuth.sin() * el.cos()
                + f64::from(sample.v) * azimuth.cos() * el.cos()
                + f64::from(sample.w - fall_speed) * el.sin();
            let z = f64::from(sample.z_linear);
            if !z.is_finite() || z <= 0.0 || !vr.is_finite() {
                return Ok(());
            }
            signal_weight += weight;
            sum_z += weight * z;
            sum_z_vr += weight * z * vr;
            sum_z_vr2 += weight * z * vr * vr;
            let terminal_variance = if config.terminal_fall_speed {
                sample
                    .polar
                    .map_or(0.0, |polar| f64::from(polar.fall_speed_variance_m2s2))
                    * el.sin().powi(2)
            } else {
                0.0
            };
            let turbulent_variance = if config.spectrum_width {
                // Isotropic one-dimensional variance from TKE = 1/2(u'^2+v'^2+w'^2).
                f64::from((2.0 / 3.0) * sample.tke_m2s2.max(0.0))
            } else {
                0.0
            };
            sum_z_subgrid_variance += weight * z * (terminal_variance + turbulent_variance);
            if let Some(polar) = sample.polar {
                any_polar = true;
                polar_accumulator.add(weight as f32, intrinsic_as_contribution(polar));
            }
            Ok(())
        };
    if let Some(points) = physical_points {
        for point in points {
            accumulate_point(
                point.az_sigma,
                point.el_sigma,
                point.range_offset_m,
                point.weight,
            )?;
        }
    } else {
        for point in quadrature_points(config.beam_integration) {
            accumulate_point(
                point.az_sigma,
                point.el_sigma,
                point.range_gate * spacing_m,
                point.weight,
            )?;
        }
    }

    let quality = GateQuality {
        total_weight,
        model_covered_weight: valid_weight,
        terrain_unblocked_weight,
        meteorological_signal_weight: signal_weight,
    }
    .fractions();
    if valid_weight <= 0.0 || signal_weight <= 0.0 || sum_z <= 0.0 {
        return Ok(GateSampleResult {
            physical: None,
            quality,
        });
    }
    // Missing model-domain quadrature points are renormalized away. Terrain-
    // blocked points remain in `valid_weight` but contribute no signal, so
    // partial blockage correctly reduces received power.
    let mut polar =
        any_polar.then(|| normalize_intrinsic(polar_accumulator.finalize(), valid_weight as f32));
    let z_linear = polar
        .filter(|sample| sample.zh > 0.0)
        .map_or((sum_z / valid_weight) as f32, |sample| sample.zh);
    let velocity = sum_z_vr / sum_z;
    let variance =
        (sum_z_vr2 / sum_z - velocity * velocity).max(0.0) + sum_z_subgrid_variance / sum_z;
    let floor = if config.spectrum_width {
        f64::from(config.spectrum_width_floor_mps.max(0.0))
    } else {
        0.0
    };
    Ok(GateSampleResult {
        physical: Some(GatePhysicalSample {
            z_linear,
            velocity_mps: velocity as f32,
            spectrum_width_mps: (variance + floor * floor).sqrt() as f32,
            polar: polar.take(),
        }),
        quality,
    })
}

#[allow(clippy::too_many_arguments)]
fn sample_gate_with_quality(
    fields: &WrfRadarFields,
    neighbor_fields: Option<&WrfRadarFields>,
    atmosphere_alpha: f64,
    cells: usize,
    site_lat: f64,
    site_lon: f64,
    antenna_msl: f64,
    center_azimuth_deg: f64,
    center_elevation_deg: f64,
    center_slant_m: f64,
    gate_index: usize,
    spacing_m: f64,
    config: &SyntheticRadarConfig,
    terrain_horizon: Option<&TerrainHorizon>,
) -> Result<GateSampleResult, String> {
    sample_gate_with_quality_instrument(
        fields,
        neighbor_fields,
        atmosphere_alpha,
        cells,
        site_lat,
        site_lon,
        antenna_msl,
        center_azimuth_deg,
        center_elevation_deg,
        center_slant_m,
        gate_index,
        spacing_m,
        config,
        terrain_horizon,
        None,
    )
}

/// Compatibility seam for focused physics tests and callers that need only
/// the sampled moments. Production cut building uses the quality-bearing form.
#[allow(clippy::too_many_arguments)]
fn sample_gate(
    fields: &WrfRadarFields,
    neighbor_fields: Option<&WrfRadarFields>,
    atmosphere_alpha: f64,
    cells: usize,
    site_lat: f64,
    site_lon: f64,
    antenna_msl: f64,
    center_azimuth_deg: f64,
    center_elevation_deg: f64,
    center_slant_m: f64,
    gate_index: usize,
    spacing_m: f64,
    config: &SyntheticRadarConfig,
    terrain_horizon: Option<&TerrainHorizon>,
) -> Result<Option<GatePhysicalSample>, String> {
    Ok(sample_gate_with_quality(
        fields,
        neighbor_fields,
        atmosphere_alpha,
        cells,
        site_lat,
        site_lon,
        antenna_msl,
        center_azimuth_deg,
        center_elevation_deg,
        center_slant_m,
        gate_index,
        spacing_m,
        config,
        terrain_horizon,
    )?
    .physical)
}

struct CutMomentRow {
    reflectivity: Vec<f32>,
    velocity: Vec<f32>,
    spectrum_width: Vec<f32>,
    zdr: Vec<f32>,
    rho_hv: Vec<f32>,
    phi_dp: Vec<f32>,
    kdp: Vec<f32>,
    ah: Vec<f32>,
    pia: Vec<f32>,
    ref_corrected: Vec<f32>,
    adp: Vec<f32>,
    pida: Vec<f32>,
    zdr_corrected: Vec<f32>,
    model_coverage: Vec<u8>,
    terrain_unblocked: Vec<u8>,
    meteorological_signal: Vec<u8>,
    stage_diagnostics: Option<StageDiagnosticRow>,
}

struct StageDiagnosticRow {
    ideal_reflectivity: Vec<f32>,
    ideal_velocity: Vec<f32>,
    ideal_spectrum_width: Vec<f32>,
    ideal_zdr: Vec<f32>,
    ideal_rho_hv: Vec<f32>,
    ideal_kdp: Vec<f32>,
    measured_reflectivity: Vec<f32>,
    measured_velocity: Vec<f32>,
    measured_spectrum_width: Vec<f32>,
    measured_zdr: Vec<f32>,
    measured_rho_hv: Vec<f32>,
    measured_kdp: Vec<f32>,
}

fn diagnostic_value(value: Option<f64>) -> f32 {
    value
        .filter(|value| value.is_finite())
        .map_or(f32::NAN, |value| value as f32)
}

impl StageDiagnosticRow {
    fn with_len(len: usize) -> Self {
        let blank = || vec![f32::NAN; len];
        Self {
            ideal_reflectivity: blank(),
            ideal_velocity: blank(),
            ideal_spectrum_width: blank(),
            ideal_zdr: blank(),
            ideal_rho_hv: blank(),
            ideal_kdp: blank(),
            measured_reflectivity: blank(),
            measured_velocity: blank(),
            measured_spectrum_width: blank(),
            measured_zdr: blank(),
            measured_rho_hv: blank(),
            measured_kdp: blank(),
        }
    }

    fn with_capacity(capacity: usize) -> Self {
        let values = || Vec::with_capacity(capacity);
        Self {
            ideal_reflectivity: values(),
            ideal_velocity: values(),
            ideal_spectrum_width: values(),
            ideal_zdr: values(),
            ideal_rho_hv: values(),
            ideal_kdp: values(),
            measured_reflectivity: values(),
            measured_velocity: values(),
            measured_spectrum_width: values(),
            measured_zdr: values(),
            measured_rho_hv: values(),
            measured_kdp: values(),
        }
    }

    fn record(&mut self, gate: usize, ideal: IdealMoments, measured: MeasuredMoments) {
        self.ideal_reflectivity[gate] = diagnostic_value(ideal.values.reflectivity_dbz);
        self.ideal_velocity[gate] = diagnostic_value(ideal.values.velocity_mps);
        self.ideal_spectrum_width[gate] = diagnostic_value(ideal.values.spectrum_width_mps);
        self.ideal_zdr[gate] = diagnostic_value(ideal.values.zdr_db);
        self.ideal_rho_hv[gate] = diagnostic_value(ideal.values.rho_hv);
        self.ideal_kdp[gate] = diagnostic_value(ideal.values.kdp_deg_km);
        self.measured_reflectivity[gate] = diagnostic_value(measured.values.reflectivity_dbz);
        self.measured_velocity[gate] = diagnostic_value(measured.values.velocity_mps);
        self.measured_spectrum_width[gate] = diagnostic_value(measured.values.spectrum_width_mps);
        self.measured_zdr[gate] = diagnostic_value(measured.values.zdr_db);
        self.measured_rho_hv[gate] = diagnostic_value(measured.values.rho_hv);
        self.measured_kdp[gate] = diagnostic_value(measured.values.kdp_deg_km);
    }

    fn append(&mut self, row: Self) {
        self.ideal_reflectivity.extend(row.ideal_reflectivity);
        self.ideal_velocity.extend(row.ideal_velocity);
        self.ideal_spectrum_width.extend(row.ideal_spectrum_width);
        self.ideal_zdr.extend(row.ideal_zdr);
        self.ideal_rho_hv.extend(row.ideal_rho_hv);
        self.ideal_kdp.extend(row.ideal_kdp);
        self.measured_reflectivity.extend(row.measured_reflectivity);
        self.measured_velocity.extend(row.measured_velocity);
        self.measured_spectrum_width
            .extend(row.measured_spectrum_width);
        self.measured_zdr.extend(row.measured_zdr);
        self.measured_rho_hv.extend(row.measured_rho_hv);
        self.measured_kdp.extend(row.measured_kdp);
    }
}

/// Acquisition geometry and timing resolved before gate sampling begins.
///
/// Temporal WRF sampling needs one interpolation weight for the whole ray, so
/// acquisition time cannot be invented after the parallel sampler has already
/// read the atmosphere. Keeping this plan separate also makes the timestamp
/// written to [`Radial`] exactly the timestamp used by the forward operator.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SyntheticRayPlan {
    source_index: usize,
    azimuth_deg: f32,
    elevation_deg: f32,
    time_offset_ms: i32,
    atmosphere_alpha: f64,
    radial_status: radar_core::RadialStatus,
}

fn plan_synthetic_rays(
    cut_index: usize,
    cut_count: usize,
    naz: usize,
    scan_timing: ScanTiming,
    azimuth_rate_deg_per_second: f32,
    cut_start_ms: i32,
) -> Vec<SyntheticRayPlan> {
    (0..naz)
        .map(|source_index| {
            let azimuth_deg = source_index as f32 * 360.0 / naz as f32;
            let time_offset_ms = match scan_timing {
                ScanTiming::InstantaneousTruth => 0,
                ScanTiming::TimedVolume => {
                    let radial_ms = 1_000.0 * azimuth_deg / azimuth_rate_deg_per_second.max(0.1);
                    cut_start_ms.saturating_add(radial_ms.round() as i32)
                }
            };
            let radial_status = if source_index == 0 && cut_index == 0 {
                radar_core::RadialStatus::StartVolume
            } else if source_index == 0 {
                radar_core::RadialStatus::StartElevation
            } else if source_index + 1 == naz && cut_index + 1 == cut_count {
                radar_core::RadialStatus::EndVolume
            } else if source_index + 1 == naz {
                radar_core::RadialStatus::EndElevation
            } else {
                radar_core::RadialStatus::Intermediate
            };
            SyntheticRayPlan {
                source_index,
                azimuth_deg,
                elevation_deg: 0.0,
                time_offset_ms,
                atmosphere_alpha: 0.0,
                radial_status,
            }
        })
        .collect()
}

impl CutMomentRow {
    fn blank(gates: usize, emit_stage_diagnostics: bool) -> Self {
        let blank = || vec![f32::NAN; gates];
        Self {
            reflectivity: blank(),
            velocity: blank(),
            spectrum_width: blank(),
            zdr: blank(),
            rho_hv: blank(),
            phi_dp: blank(),
            kdp: blank(),
            ah: blank(),
            pia: blank(),
            ref_corrected: blank(),
            adp: blank(),
            pida: blank(),
            zdr_corrected: blank(),
            model_coverage: vec![0; gates],
            terrain_unblocked: vec![0; gates],
            meteorological_signal: vec![0; gates],
            stage_diagnostics: emit_stage_diagnostics.then(|| StageDiagnosticRow::with_len(gates)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_cut(
    fields: &WrfRadarFields,
    temporal: Option<&TemporalRenderContext<'_>>,
    cells: usize,
    site_lat: f64,
    site_lon: f64,
    antenna_msl: f64,
    elevation_deg: f64,
    cut_index: usize,
    naz: usize,
    spacing: f64,
    gate_range: GateRange,
    config: &SyntheticRadarConfig,
    scan_leg: &SyntheticScanLeg,
    scan_leg_count: usize,
    frame_seed: u32,
    terrain_horizon: Option<&TerrainHorizon>,
    cut_start_ms: i32,
    coupled_instrument: Option<&CoupledInstrumentContext>,
    ray_plan_override: Option<&[SyntheticRayPlan]>,
) -> Result<ElevationCut, String> {
    let gate_count = gate_range.gate_count;
    // Instrument mode already applies its range-varying radar-equation
    // sensitivity gate below; a fixed 0 dBZ presentation floor would erase
    // valid weak nearby returns a real instrument can see.
    let floor = if config.instrument_noise {
        f32::NEG_INFINITY
    } else {
        config.ref_floor_dbz
    };

    // Ground clutter (opt-in). When the amount is 0 the whole clutter path is
    // skipped and the output is bit-identical to a build without the feature.
    // The near-radar hotspots are precomputed once per tilt (serial, ≤8 rects)
    // so the parallel per-gate work only does an O(hotspots) membership test.
    let clutter_amount = config.clutter_intensity.clamp(0.0, 1.0);
    let clutter_on = clutter_amount > 0.0 && cut_index < CLUTTER_TILT_LIMIT;
    let hotspots = if clutter_on {
        clutter_hotspots(frame_seed, cut_index, naz, gate_count)
    } else {
        Vec::new()
    };

    let mut ray_plan = if let Some(override_plan) = ray_plan_override {
        override_plan.to_vec()
    } else {
        let mut planned = plan_synthetic_rays(
            cut_index,
            scan_leg_count,
            naz,
            config.scan_timing,
            scan_leg.azimuth_rate_deg_per_second,
            cut_start_ms,
        );
        for ray in &mut planned {
            ray.elevation_deg = elevation_deg as f32;
        }
        planned
    };
    if let Some(temporal) = temporal {
        for ray in &mut ray_plan {
            ray.atmosphere_alpha = temporal
                .plan
                .ray_alpha(i64::from(ray.time_offset_ms))
                .map_err(|error| {
                    format!(
                        "temporal weight for cut {} ray {} at {} ms: {error}",
                        cut_index + 1,
                        ray.source_index + 1,
                        ray.time_offset_ms
                    )
                })?;
        }
    }
    let rendered_ray_count = ray_plan.len();

    // One row per radial, sampled in parallel. Each row is `gate_count` REF and
    // `gate_count` VEL f32 values (NaN = no data / below floor / off-domain).
    let rows: Vec<CutMomentRow> = ray_plan
        .par_iter()
        .map(|ray| -> Result<CutMomentRow, String> {
            let iaz = ray.source_index;
            let az_deg = f64::from(ray.azimuth_deg);
            let ray_elevation_deg = f64::from(ray.elevation_deg);
            let mut row = CutMomentRow::blank(gate_count, config.emit_stage_diagnostics);
            let dr_km = (spacing / 1_000.0).max(0.0) as f32;
            let mut previous_kdp = 0.0f32;
            let mut previous_ah = 0.0f32;
            let mut previous_adp = 0.0f32;
            let mut phi_path = 0.0f32;
            let mut tau_h = 0.0f32;
            let mut tau_dp = 0.0f32;
            for gate in 0..gate_count {
                let slant_m = f64::from(gate_range.first_gate_m) + gate as f64 * spacing;
                // Doviak & Zrnić (1993) eq. 2.28b/c under the 4/3-earth model.
                let beam_height_m = beam_height_above_radar_m(slant_m, ray_elevation_deg);
                let ground_m = beam_ground_range_m(slant_m, ray_elevation_deg);
                let sampled = sample_gate_with_quality_instrument(
                    fields,
                    temporal.map(|context| context.neighbor),
                    ray.atmosphere_alpha,
                    cells,
                    site_lat,
                    site_lon,
                    antenna_msl,
                    az_deg,
                    ray_elevation_deg,
                    slant_m,
                    gate,
                    spacing,
                    config,
                    terrain_horizon,
                    coupled_instrument,
                )?;
                if config.emit_quality_fields {
                    row.model_coverage[gate] =
                        encode_quality_fraction(sampled.quality.model_coverage_fraction);
                    row.terrain_unblocked[gate] =
                        encode_quality_fraction(sampled.quality.terrain_unblocked_fraction);
                    row.meteorological_signal[gate] =
                        encode_quality_fraction(sampled.quality.meteorological_signal_fraction);
                }
                if sampled.quality.model_coverage_fraction
                    < config.clamped_minimum_model_coverage_fraction()
                {
                    continue;
                }
                let Some(sample) = sampled.physical else {
                    continue;
                };
                // Reflectivity gate texture (default ON): perturb BEFORE the
                // floor test so echo edges go ragged the way marginal-SNR gates
                // do on a real scope. When off, `dbz` is `sample.dbz` untouched
                // — the output stays bit-identical to a textureless build.
                let intrinsic_dbz = z_to_dbz(sample.z_linear);
                let mut intrinsic_zdr = f32::NAN;
                let mut rho_hv = f32::NAN;
                let mut kdp = f32::NAN;
                let mut ah = f32::NAN;
                let mut adp = f32::NAN;
                let mut pia = f32::NAN;
                let mut pida = f32::NAN;
                let mut phi_dp = f32::NAN;
                if let Some(polar) = sample.polar {
                    intrinsic_zdr = polar.zdr_db;
                    rho_hv = polar.rho_hv;
                    kdp = polar.kdp_deg_km;
                    ah = polar.ah_db_km;
                    adp = polar.ah_db_km - polar.av_db_km;
                    if config.propagation && gate > 0 {
                        phi_path += (previous_kdp + kdp) * dr_km;
                        tau_h += 0.5 * (previous_ah + ah) * dr_km;
                        tau_dp += 0.5 * (previous_adp + adp) * dr_km;
                    }
                    previous_kdp = kdp;
                    previous_ah = ah;
                    previous_adp = adp;
                    pia = if config.propagation { 2.0 * tau_h } else { 0.0 };
                    pida = if config.propagation {
                        2.0 * tau_dp
                    } else {
                        0.0
                    };
                    phi_dp =
                        config.system_phidp_deg + if config.propagation { phi_path } else { 0.0 };
                }
                if let Some(coupled) = coupled_instrument {
                    let propagated_reflectivity =
                        intrinsic_dbz - if pia.is_finite() { pia } else { 0.0 };
                    let ideal = IdealMoments {
                        values: RadarMomentValues {
                            reflectivity_dbz: propagated_reflectivity
                                .is_finite()
                                .then_some(f64::from(propagated_reflectivity)),
                            velocity_mps: sample
                                .velocity_mps
                                .is_finite()
                                .then_some(f64::from(sample.velocity_mps)),
                            spectrum_width_mps: (config.spectrum_width
                                && sample.spectrum_width_mps.is_finite())
                            .then_some(f64::from(sample.spectrum_width_mps.max(0.0))),
                            zdr_db: (config.dual_pol && intrinsic_zdr.is_finite()).then_some(
                                f64::from(
                                    intrinsic_zdr - if pida.is_finite() { pida } else { 0.0 },
                                ),
                            ),
                            rho_hv: (config.dual_pol && rho_hv.is_finite())
                                .then_some(f64::from(rho_hv.clamp(0.0, 1.0))),
                            kdp_deg_km: (config.dual_pol && kdp.is_finite())
                                .then_some(f64::from(kdp)),
                        },
                    };
                    let noise_key = NoiseKey {
                        seed: u64::from(frame_seed),
                        frame: 0,
                        cut: u16::try_from(cut_index).unwrap_or(u16::MAX),
                        ray: u32::try_from(iaz).unwrap_or(u32::MAX),
                        gate: u32::try_from(gate).unwrap_or(u32::MAX),
                    };
                    let measured = estimate_measured_moments(
                        ideal,
                        &coupled.instrument,
                        &coupled.timing,
                        &coupled.estimator_config,
                        slant_m,
                        noise_key,
                    )
                    .map_err(|error| {
                        format!(
                            "estimate coupled moments for cut {} ray {} gate {}: {error}",
                            cut_index + 1,
                            iaz + 1,
                            gate + 1
                        )
                    })?;
                    if let Some(diagnostics) = row.stage_diagnostics.as_mut() {
                        diagnostics.record(gate, ideal, measured);
                    }
                    let clutter_reflectivity_dbz = clutter_on
                        .then(|| {
                            ground_clutter_dbz(
                                frame_seed,
                                cut_index,
                                iaz,
                                gate,
                                az_deg as f32,
                                (ground_m / 1_000.0) as f32,
                                beam_height_m as f32,
                                in_clutter_hotspot(&hotspots, iaz, gate),
                                clutter_amount,
                            )
                        })
                        .flatten();
                    let presentation = PresentationConfig {
                        reflectivity_texture_sigma_db: if config.ref_gate_texture {
                            f64::from(REF_TEXTURE_CORRELATED_DB.hypot(REF_TEXTURE_JITTER_DB))
                        } else {
                            0.0
                        },
                        velocity_texture_sigma_mps: if config.vel_gate_texture {
                            f64::from(VEL_TEXTURE_MPS)
                        } else {
                            0.0
                        },
                        zdr_display_bias_db: 0.0,
                        reflectivity_display_floor_dbz: None,
                        clutter_reflectivity_dbz: clutter_reflectivity_dbz.map(f64::from),
                        clutter_velocity_mps: if clutter_reflectivity_dbz.is_some() {
                            f64::from(clutter_velocity_mps(frame_seed, cut_index, iaz, gate))
                        } else {
                            0.0
                        },
                    };
                    let presented: PresentedMoments =
                        present_measured_moments(measured, &presentation, noise_key);
                    let mut presented_values = presented.values;
                    if let Some(velocity) = presented_values.velocity_mps.as_mut() {
                        *velocity = f64::from(fold_velocity_mps(
                            *velocity as f32,
                            coupled.stamped_nyquist_mps(),
                        ));
                    }
                    if presented.adjustment.clutter_replaced
                        && let Some(width) = presented_values.spectrum_width_mps.as_mut()
                    {
                        *width = (*width).max(f64::from(config.spectrum_width_floor_mps.max(0.0)));
                    }
                    if let Some(value) = presented_values.reflectivity_dbz {
                        row.reflectivity[gate] = value as f32;
                    }
                    if let Some(value) = presented_values.velocity_mps {
                        row.velocity[gate] = value as f32;
                    }
                    if let Some(value) = presented_values.spectrum_width_mps {
                        row.spectrum_width[gate] = value as f32;
                    }
                    if config.dual_pol
                        && !presented.adjustment.clutter_replaced
                        && presented_values.reflectivity_dbz.is_some()
                    {
                        if let Some(value) = presented_values.zdr_db {
                            row.zdr[gate] = value as f32;
                        }
                        if let Some(value) = presented_values.rho_hv {
                            row.rho_hv[gate] = value as f32;
                        }
                        if let Some(value) = presented_values.kdp_deg_km {
                            row.kdp[gate] = value as f32;
                        }
                        row.phi_dp[gate] = phi_dp;
                        row.ah[gate] = ah;
                        row.pia[gate] = pia;
                        row.ref_corrected[gate] = intrinsic_dbz;
                        row.adp[gate] = adp;
                        row.pida[gate] = pida;
                        row.zdr_corrected[gate] = intrinsic_zdr;
                    }
                    continue;
                }
                let mut dbz = intrinsic_dbz - if pia.is_finite() { pia } else { 0.0 };
                if config.ref_gate_texture {
                    dbz += ref_gate_texture_db(cut_index, iaz, gate);
                }
                let range_km = (slant_m / 1_000.0).max(1.0) as f32;
                let sensitivity = config.sensitivity_dbz_at_1km + 20.0 * range_km.log10();
                if config.instrument_noise {
                    let snr_db = dbz - sensitivity;
                    if !snr_db.is_finite() || snr_db < 0.0 {
                        continue;
                    }
                    let sigma_db = (1.5 / (1.0 + snr_db / 6.0)).clamp(0.12, 1.5);
                    dbz += sigma_db * clutter_signed(frame_seed, cut_index, iaz, gate, 0x534e_525a);
                }
                // Physics reflectivity + velocity for this gate. Both stay NaN
                // below the floor (clear air) so ground clutter can fill them,
                // exactly as the community operator fills the clear-air gates
                // near the radar. With clutter off, `ref_val`/`vel_val` are
                // committed unchanged, so the output is bit-identical.
                let mut ref_val = f32::NAN;
                let mut vel_val = f32::NAN;
                let mut sw_val = f32::NAN;
                let mut clutter_replaced = false;
                if dbz.is_finite() && dbz >= floor {
                    ref_val = dbz;
                    // Radial velocity: wind projected onto the beam unit vector
                    // (east, north, up) = (sinAz·cosEl, cosAz·cosEl, sinEl), with
                    // azimuth clockwise from north. Positive = away from the radar
                    // (NEXRAD convention). Sun & Crook (1997).
                    let mut vr = sample.velocity_mps;
                    let mut velocity_noise_variance = 0.0f32;
                    if config.instrument_noise {
                        let snr_db = (dbz - sensitivity).max(0.0);
                        let sigma_velocity = (1.2 / (1.0 + snr_db / 8.0)).clamp(0.08, 1.2);
                        velocity_noise_variance = sigma_velocity * sigma_velocity;
                        vr += sigma_velocity
                            * clutter_signed(frame_seed, cut_index, iaz, gate, 0x534e_5256);
                    }
                    if vr.is_finite() {
                        // Velocity gate texture (default OFF): opt-in wobble; the
                        // clean Vr is what dealias/GBVTD consume.
                        vel_val = if config.vel_gate_texture {
                            vr + vel_gate_texture_mps(cut_index, iaz, gate)
                        } else {
                            vr
                        };
                    }
                    if config.spectrum_width && sample.spectrum_width_mps.is_finite() {
                        sw_val = (sample.spectrum_width_mps.max(0.0).powi(2)
                            + velocity_noise_variance)
                            .sqrt();
                    }
                }

                // Ground clutter (opt-in), applied AFTER the reflectivity floor
                // and only into gates weaker than the clutter value, so storms
                // are never overwritten (the community rule).
                if clutter_on
                    && let Some(cv) = ground_clutter_dbz(
                        frame_seed,
                        cut_index,
                        iaz,
                        gate,
                        az_deg as f32,
                        (ground_m / 1000.0) as f32,
                        beam_height_m as f32,
                        in_clutter_hotspot(&hotspots, iaz, gate),
                        clutter_amount,
                    )
                    && (!ref_val.is_finite() || ref_val < cv)
                {
                    clutter_replaced = true;
                    ref_val = cv;
                    // The ground is stationary: where clutter dominates a gate
                    // that already carried a velocity, replace the wind
                    // projection with a near-zero return. A clear-air clutter
                    // gate had no Vr to speak of, so it is left blank (velocity
                    // is gated on data existing).
                    if vel_val.is_finite() {
                        vel_val = clutter_velocity_mps(frame_seed, cut_index, iaz, gate);
                        if config.spectrum_width {
                            sw_val = sw_val.max(config.spectrum_width_floor_mps.max(0.0));
                        }
                    }
                }

                // Realistic Nyquist (opt-in): alias the true Vr into the folding
                // co-interval, AFTER the clutter VEL replacement. Applied last so
                // it folds exactly what the display will store. Clutter gates are
                // ~0 ± 0.5 m/s — well inside any sane Nyquist — so folding leaves
                // them untouched. With folding OFF this branch never runs, so the
                // output is bit-identical to a build without the feature.
                if config.fold_velocity && vel_val.is_finite() {
                    vel_val = fold_velocity_mps(vel_val, config.nyquist_mps);
                }

                if ref_val.is_finite() {
                    row.reflectivity[gate] = ref_val;
                }
                if vel_val.is_finite() {
                    row.velocity[gate] = vel_val;
                }
                if sw_val.is_finite() {
                    row.spectrum_width[gate] = sw_val;
                }
                if config.dual_pol
                    && !clutter_replaced
                    && ref_val.is_finite()
                    && intrinsic_zdr.is_finite()
                {
                    row.zdr[gate] = intrinsic_zdr + config.zdr_bias_db - pida;
                    row.rho_hv[gate] = rho_hv.clamp(0.0, 1.0);
                    row.phi_dp[gate] = phi_dp;
                    row.kdp[gate] = kdp;
                    row.ah[gate] = ah;
                    row.pia[gate] = pia;
                    row.ref_corrected[gate] = intrinsic_dbz;
                    row.adp[gate] = adp;
                    row.pida[gate] = pida;
                    row.zdr_corrected[gate] = intrinsic_zdr;
                }
            }
            Ok(row)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut cut = ElevationCut::new(elevation_deg as f32, u8::try_from(cut_index + 1).ok());
    let mut ref_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut vel_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut sw_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut zdr_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut rho_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut phi_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut kdp_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut ah_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut pia_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut refc_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut adp_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut pida_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut zdrc_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut model_coverage_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut terrain_unblocked_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut meteorological_signal_values = Vec::with_capacity(rendered_ray_count * gate_count);
    let mut stage_diagnostics = config
        .emit_stage_diagnostics
        .then(|| StageDiagnosticRow::with_capacity(rendered_ray_count * gate_count));
    for (ray, row) in ray_plan.into_iter().zip(rows) {
        cut.radials.push(Radial {
            azimuth_deg: ray.azimuth_deg,
            elevation_deg: ray.elevation_deg,
            time_offset_ms: ray.time_offset_ms,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: Some(coupled_instrument.map_or_else(
                || config.stamped_nyquist_mps(),
                CoupledInstrumentContext::stamped_nyquist_mps,
            )),
            radial_status: Some(ray.radial_status),
        });
        ref_values.extend(row.reflectivity);
        vel_values.extend(row.velocity);
        sw_values.extend(row.spectrum_width);
        zdr_values.extend(row.zdr);
        rho_values.extend(row.rho_hv);
        phi_values.extend(row.phi_dp);
        kdp_values.extend(row.kdp);
        ah_values.extend(row.ah);
        pia_values.extend(row.pia);
        refc_values.extend(row.ref_corrected);
        adp_values.extend(row.adp);
        pida_values.extend(row.pida);
        zdrc_values.extend(row.zdr_corrected);
        model_coverage_values.extend(row.model_coverage);
        terrain_unblocked_values.extend(row.terrain_unblocked);
        meteorological_signal_values.extend(row.meteorological_signal);
        if let (Some(all_diagnostics), Some(row_diagnostics)) =
            (stage_diagnostics.as_mut(), row.stage_diagnostics)
        {
            all_diagnostics.append(row_diagnostics);
        }
    }

    let radial_indices: Vec<usize> = (0..rendered_ray_count).collect();
    if scan_leg.moments.has_reflectivity() {
        cut.moments.insert(
            MomentType::Reflectivity,
            f32_grid(
                MomentType::Reflectivity,
                gate_range.clone(),
                radial_indices.clone(),
                ref_values,
            ),
        );
    }
    if scan_leg.moments.has_velocity() {
        cut.moments.insert(
            MomentType::Velocity,
            f32_grid(
                MomentType::Velocity,
                gate_range.clone(),
                radial_indices.clone(),
                vel_values,
            ),
        );
    }
    if config.spectrum_width && scan_leg.moments.has_spectrum_width() {
        cut.moments.insert(
            MomentType::SpectrumWidth,
            f32_grid(
                MomentType::SpectrumWidth,
                gate_range.clone(),
                radial_indices.clone(),
                sw_values,
            ),
        );
    }
    if config.dual_pol && fields.has_polarimetric_input() && scan_leg.moments.has_reflectivity() {
        let polar_moments = [
            (MomentType::DifferentialReflectivity, zdr_values),
            (MomentType::CorrelationCoefficient, rho_values),
            (MomentType::DifferentialPhase, phi_values),
            (MomentType::SpecificDifferentialPhase, kdp_values),
            (MomentType::Unknown("AH".to_string()), ah_values),
            (MomentType::Unknown("PIA".to_string()), pia_values),
            (MomentType::Unknown("REFC".to_string()), refc_values),
            (MomentType::Unknown("ADP".to_string()), adp_values),
            (MomentType::Unknown("PIDA".to_string()), pida_values),
            (MomentType::Unknown("ZDRC".to_string()), zdrc_values),
        ];
        for (moment, values) in polar_moments {
            cut.moments.insert(
                moment.clone(),
                f32_grid(moment, gate_range.clone(), radial_indices.clone(), values),
            );
        }
    }
    if config.emit_quality_fields {
        for (quality, values) in [
            (QualityMoment::ModelCoverage, model_coverage_values),
            (QualityMoment::TerrainUnblocked, terrain_unblocked_values),
            (
                QualityMoment::MeteorologicalSignal,
                meteorological_signal_values,
            ),
        ] {
            let grid =
                compact_quality_grid(quality, gate_range.clone(), radial_indices.clone(), values)?;
            cut.moments.insert(grid.moment.clone(), grid);
        }
    }
    if let Some(diagnostics) = stage_diagnostics {
        for (name, values) in [
            ("IREF", diagnostics.ideal_reflectivity),
            ("IVEL", diagnostics.ideal_velocity),
            ("ISW", diagnostics.ideal_spectrum_width),
            ("IZDR", diagnostics.ideal_zdr),
            ("IRHO", diagnostics.ideal_rho_hv),
            ("IKDP", diagnostics.ideal_kdp),
            ("MREF", diagnostics.measured_reflectivity),
            ("MVEL", diagnostics.measured_velocity),
            ("MSW", diagnostics.measured_spectrum_width),
            ("MZDR", diagnostics.measured_zdr),
            ("MRHO", diagnostics.measured_rho_hv),
            ("MKDP", diagnostics.measured_kdp),
        ] {
            let moment = MomentType::Unknown(name.to_string());
            cut.moments.insert(
                moment.clone(),
                f32_grid(moment, gate_range.clone(), radial_indices.clone(), values),
            );
        }
    }
    Ok(cut)
}

fn f32_grid(
    moment: MomentType,
    gate_range: GateRange,
    radial_indices: Vec<usize>,
    values: Vec<f32>,
) -> MomentGrid {
    // True physical units: dBZ / m·s⁻¹ stored directly (scale 1, offset 0), so
    // the render/dealias/GBVTD F32 paths and the standard colour tables read
    // them without a raw→scaled conversion.
    MomentGrid {
        moment,
        gate_range,
        scale: 1.0,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        radial_indices,
        storage: MomentStorage::F32(values),
    }
}

/// One trilinearly-sampled model column value at a gate.
struct ColumnSample {
    /// Equivalent reflectivity factor in linear mm^6 m^-3. Keeping received
    /// power linear through every interpolation and pulse-volume average is the
    /// core scientific correction; convert to dBZ only at the finished gate.
    z_linear: f32,
    u: f32,
    v: f32,
    w: f32,
    polar: Option<crate::wrf_radar_physics::IntrinsicPolarSample>,
    tke_m2s2: f32,
}

// These inputs are kept explicit because they cross the spatial, beam and
// atmosphere-time sampling boundary; bundling them would hide unit contracts.
#[allow(clippy::too_many_arguments)]
fn sample_column_temporal(
    fields: &WrfRadarFields,
    neighbor_fields: Option<&WrfRadarFields>,
    alpha: f64,
    cells: usize,
    lat: f32,
    lon: f32,
    z_msl: f32,
    beam_elevation_deg: f64,
    atmosphere_time_mode: AtmosphereTimeMode,
    reflectivity_sampling: ReflectivitySampling,
) -> Result<Option<ColumnSample>, String> {
    if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
        return Err(format!(
            "temporal atmosphere weight {alpha} is outside the closed [0,1] interval"
        ));
    }
    if matches!(atmosphere_time_mode, AtmosphereTimeMode::RawStateLinear) {
        return sample_column_raw_state_linear(
            fields,
            neighbor_fields,
            alpha,
            cells,
            lat,
            lon,
            z_msl,
            beam_elevation_deg,
        );
    }
    if alpha <= 0.0 {
        return sample_column(
            fields,
            cells,
            lat,
            lon,
            z_msl,
            beam_elevation_deg,
            reflectivity_sampling,
        );
    }
    let neighbor = neighbor_fields.ok_or_else(|| {
        "temporal sampling has a positive atmosphere weight but no adjacent WRF scene".to_string()
    })?;
    if alpha >= 1.0 {
        return sample_column(
            neighbor,
            cells,
            lat,
            lon,
            z_msl,
            beam_elevation_deg,
            reflectivity_sampling,
        );
    }
    let Some(left) = sample_column(
        fields,
        cells,
        lat,
        lon,
        z_msl,
        beam_elevation_deg,
        reflectivity_sampling,
    )?
    else {
        return Ok(None);
    };
    let Some(right) = sample_column(
        neighbor,
        cells,
        lat,
        lon,
        z_msl,
        beam_elevation_deg,
        reflectivity_sampling,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(blend_column_samples(left, right, alpha as f32)))
}

/// Blend quantities that remain linear/additive through the radar operator.
/// ZH/ZV/covariance/KDP/attenuation are combined as additive scattering
/// quantities; ratios such as ZDR and rhoHV are derived only afterwards.
fn blend_column_samples(left: ColumnSample, right: ColumnSample, alpha: f32) -> ColumnSample {
    let alpha = alpha.clamp(0.0, 1.0);
    let beta = 1.0 - alpha;
    let polar = match (left.polar, right.polar) {
        (Some(left), Some(right)) => {
            let mut accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
            accumulator.add(beta, intrinsic_as_contribution(left));
            accumulator.add(alpha, intrinsic_as_contribution(right));
            Some(accumulator.finalize())
        }
        (Some(left), None) => {
            let mut accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
            accumulator.add(beta, intrinsic_as_contribution(left));
            Some(accumulator.finalize())
        }
        (None, Some(right)) => {
            let mut accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
            accumulator.add(alpha, intrinsic_as_contribution(right));
            Some(accumulator.finalize())
        }
        (None, None) => None,
    };
    ColumnSample {
        z_linear: beta * left.z_linear + alpha * right.z_linear,
        u: beta * left.u + alpha * right.u,
        v: beta * left.v + alpha * right.v,
        w: beta * left.w + alpha * right.w,
        polar,
        tke_m2s2: beta * left.tke_m2s2 + alpha * right.tke_m2s2,
    }
}

struct RawSpatialEndpoint {
    property_weights: [(usize, f64); 8],
    property_weight_count: usize,
    gate_state: RawGateState,
    tke_m2s2: f32,
}

#[allow(clippy::too_many_arguments)]
fn sample_column_raw_state_linear(
    fields: &WrfRadarFields,
    neighbor_fields: Option<&WrfRadarFields>,
    alpha: f64,
    cells: usize,
    lat: f32,
    lon: f32,
    z_msl: f32,
    beam_elevation_deg: f64,
) -> Result<Option<ColumnSample>, String> {
    let left = if alpha < 1.0 {
        raw_spatial_endpoint(fields, cells, lat, lon, z_msl)?
    } else {
        None
    };
    let right = if alpha > 0.0 {
        let neighbor = neighbor_fields.ok_or_else(|| {
            "RawStateLinear has a positive temporal weight but no adjacent WRF scene".to_string()
        })?;
        raw_spatial_endpoint(neighbor, cells, lat, lon, z_msl)?
    } else {
        None
    };
    if (alpha < 1.0 && left.is_none()) || (alpha > 0.0 && right.is_none()) {
        return Ok(None);
    }
    let left_endpoint = left.as_ref().map(|endpoint| {
        RawStateLinearEndpoint::with_spatial_weights(
            fields
                .raw_property_scene
                .as_deref()
                .expect("raw spatial endpoint requires a raw property scene"),
            &endpoint.property_weights[..endpoint.property_weight_count],
            &endpoint.gate_state,
        )
    });
    let right_endpoint = right.as_ref().map(|endpoint| {
        RawStateLinearEndpoint::with_spatial_weights(
            neighbor_fields
                .and_then(|neighbor| neighbor.raw_property_scene.as_deref())
                .expect("raw adjacent endpoint requires a raw property scene"),
            &endpoint.property_weights[..endpoint.property_weight_count],
            &endpoint.gate_state,
        )
    });
    let blended = interpolate_raw_state_linear(left_endpoint, right_endpoint, alpha)
        .map_err(|error| format!("RawStateLinear pre-closure blend: {error}"))?;
    let tke_m2s2 = match (left.as_ref(), right.as_ref()) {
        (Some(left), Some(right)) => {
            (f64::from(left.tke_m2s2)
                + (f64::from(right.tke_m2s2) - f64::from(left.tke_m2s2)) * alpha) as f32
        }
        (Some(left), None) => left.tke_m2s2,
        (None, Some(right)) => right.tke_m2s2,
        (None, None) => return Ok(None),
    };
    let polar = app_ui::wrf_tmatrix_assets::evaluate_embedded_raw_property_cell(
        &blended.property_cell,
        beam_elevation_deg,
    )?;
    let polar = polar.map(|quantities| {
        let mut accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
        accumulator.add(1.0, tmatrix_as_contribution(quantities));
        accumulator.finalize()
    });
    Ok(Some(ColumnSample {
        z_linear: polar.map_or(0.0, |sample| sample.zh),
        u: blended.gate_state.wind_u_mps,
        v: blended.gate_state.wind_v_mps,
        w: blended.gate_state.wind_w_mps,
        polar,
        tke_m2s2,
    }))
}

fn raw_spatial_endpoint(
    fields: &WrfRadarFields,
    cells: usize,
    lat: f32,
    lon: f32,
    z_msl: f32,
) -> Result<Option<RawSpatialEndpoint>, String> {
    let property_scene = fields.raw_property_scene.as_deref().ok_or_else(|| {
        "RawStateLinear requires retained native P3/ISHMAEL property state".to_string()
    })?;
    if property_scene.cell_count() != fields.nx * fields.ny * fields.nz {
        return Err(format!(
            "RawStateLinear property coverage has {} cells, expected {}",
            property_scene.cell_count(),
            fields.nx * fields.ny * fields.nz
        ));
    }
    let Some(stencil) = horizontal_stencil(fields, lat, lon) else {
        return Ok(None);
    };
    let mut property_weights = [(0usize, 0.0f64); 8];
    let mut property_weight_count = 0usize;
    let mut wind_u = 0.0f64;
    let mut wind_v = 0.0f64;
    let mut wind_w = 0.0f64;
    let mut tke = 0.0f64;
    let mut weight_sum = 0.0f64;
    for (column, horizontal_weight) in stencil {
        if horizontal_weight <= 0.0 {
            continue;
        }
        let Some((k, vertical_fraction)) = bracket_height(fields, cells, column, z_msl) else {
            continue;
        };
        let lower = k * cells + column;
        let upper = (k + 1) * cells + column;
        let (Some(&u0), Some(&u1), Some(&v0), Some(&v1), Some(&w0), Some(&w1)) = (
            fields.u.get(lower),
            fields.u.get(upper),
            fields.v.get(lower),
            fields.v.get(upper),
            fields.w.get(lower),
            fields.w.get(upper),
        ) else {
            return Err("RawStateLinear wind coverage is internally inconsistent".to_string());
        };
        if [u0, u1, v0, v1, w0, w1]
            .into_iter()
            .any(|value| !value.is_finite())
        {
            continue;
        }
        let lower_weight = f64::from(horizontal_weight) * f64::from(1.0 - vertical_fraction);
        let upper_weight = f64::from(horizontal_weight) * f64::from(vertical_fraction);
        for (index, weight, u, v, w) in [
            (lower, lower_weight, u0, v0, w0),
            (upper, upper_weight, u1, v1, w1),
        ] {
            if weight <= 0.0 {
                continue;
            }
            if property_weight_count >= property_weights.len() {
                return Err("RawStateLinear spatial stencil exceeded eight cells".to_string());
            }
            property_weights[property_weight_count] = (index, weight);
            property_weight_count += 1;
            weight_sum += weight;
            wind_u += weight * f64::from(u);
            wind_v += weight * f64::from(v);
            wind_w += weight * f64::from(w);
            if let Some(tke_grid) = &fields.tke_tenths_m2s2 {
                let value = tke_grid.get(index).copied().ok_or_else(|| {
                    "RawStateLinear TKE coverage is internally inconsistent".to_string()
                })?;
                tke += weight * f64::from(value) * 0.1;
            }
        }
    }
    if property_weight_count == 0 || !weight_sum.is_finite() || weight_sum <= 0.0 {
        return Ok(None);
    }
    for (_, weight) in &mut property_weights[..property_weight_count] {
        *weight /= weight_sum;
    }
    let normalized_sum = property_weights[..property_weight_count]
        .iter()
        .map(|(_, weight)| *weight)
        .sum::<f64>();
    property_weights[property_weight_count - 1].1 += 1.0 - normalized_sum;

    let endpoint_samples = property_weights[..property_weight_count]
        .iter()
        .map(|&(cell_index, weight)| {
            app_ui::wrf_property_reader::WeightedRawPropertyCell::new(
                property_scene,
                cell_index,
                weight,
            )
        })
        .collect::<Vec<_>>();
    let endpoint_property =
        app_ui::wrf_property_reader::blend_raw_property_cells(&endpoint_samples)
            .map_err(|error| format!("RawStateLinear spatial property blend: {error}"))?;
    Ok(Some(RawSpatialEndpoint {
        property_weights,
        property_weight_count,
        gate_state: RawGateState {
            wind_u_mps: (wind_u / weight_sum) as f32,
            wind_v_mps: (wind_v / weight_sum) as f32,
            wind_w_mps: (wind_w / weight_sum) as f32,
            temperature_k: endpoint_property.environment().temperature_k() as f32,
            pressure_pa: endpoint_property.pressure_pa() as f32,
            air_density_kgm3: endpoint_property.environment().air_density_kg_m3() as f32,
            fields: Default::default(),
        },
        tke_m2s2: (tke / weight_sum) as f32,
    }))
}

fn intrinsic_as_contribution(
    sample: crate::wrf_radar_physics::IntrinsicPolarSample,
) -> crate::wrf_radar_physics::BulkContribution {
    crate::wrf_radar_physics::BulkContribution {
        zh: sample.zh,
        zv: sample.zv,
        cov_re: sample.cov_re,
        cov_im: sample.cov_im,
        kdp_deg_km: sample.kdp_deg_km,
        ah_db_km: sample.ah_db_km,
        av_db_km: sample.av_db_km,
        fall_speed_mps: sample.fall_speed_mps,
        fall_speed_variance_m2s2: sample.fall_speed_variance_m2s2,
    }
}

fn tmatrix_as_contribution(
    sample: radar_scattering::PolarAccumulatorQuantities,
) -> crate::wrf_radar_physics::BulkContribution {
    crate::wrf_radar_physics::BulkContribution {
        zh: sample.zh,
        zv: sample.zv,
        cov_re: sample.cov_re,
        cov_im: sample.cov_im,
        kdp_deg_km: sample.kdp_deg_km,
        ah_db_km: sample.ah_db_km,
        av_db_km: sample.av_db_km,
        fall_speed_mps: sample.fall_speed_mps,
        fall_speed_variance_m2s2: sample.fall_speed_variance_m2s2,
    }
}

fn normalize_intrinsic(
    mut sample: crate::wrf_radar_physics::IntrinsicPolarSample,
    divisor: f32,
) -> crate::wrf_radar_physics::IntrinsicPolarSample {
    if divisor.is_finite() && divisor > 0.0 {
        sample.zh /= divisor;
        sample.zv /= divisor;
        sample.cov_re /= divisor;
        sample.cov_im /= divisor;
        sample.covariance_magnitude /= divisor;
        sample.kdp_deg_km /= divisor;
        sample.ah_db_km /= divisor;
        sample.av_db_km /= divisor;
    }
    sample
}

/// Sample the 3-D model fields at (lat, lon, MSL height) by horizontal 2×2
/// bilinear weights (over the curvilinear WRF grid) combined with a
/// per-corner vertical bracket in MSL height — i.e. trilinear. Returns `None`
/// off the domain or when the height sits below terrain / above the model top
/// at every contributing corner.
fn sample_column(
    fields: &WrfRadarFields,
    cells: usize,
    lat: f32,
    lon: f32,
    z_msl: f32,
    beam_elevation_deg: f64,
    reflectivity_sampling: ReflectivitySampling,
) -> Result<Option<ColumnSample>, String> {
    let Some(stencil) = horizontal_stencil(fields, lat, lon) else {
        return Ok(None);
    };

    let mut wsum = 0.0f32;
    let mut reflectivity = 0.0f32;
    let mut u = 0.0f32;
    let mut v = 0.0f32;
    let mut w = 0.0f32;
    let mut tke = 0.0f32;
    let mut polar_accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
    let property_scattering = fields.property_scattering.as_deref();
    let mut property_weights = [(0usize, 0.0f64); 8];
    let mut property_weight_count = 0usize;
    for (col, weight) in stencil {
        if weight <= 0.0 {
            continue;
        }
        let Some((k, t)) = bracket_height(fields, cells, col, z_msl) else {
            continue;
        };
        let i0 = k * cells + col;
        let i1 = (k + 1) * cells + col;
        let (Some(su), Some(sv), Some(sw)) = (
            lerp(fields.u[i0], fields.u[i1], t),
            lerp(fields.v[i0], fields.v[i1], t),
            lerp(fields.w[i0], fields.w[i1], t),
        ) else {
            continue;
        };

        if property_scattering.is_some() {
            let lower_weight = f64::from(weight) * f64::from(1.0 - t);
            if lower_weight > 0.0 {
                property_weights[property_weight_count] = (i0, lower_weight);
                property_weight_count += 1;
            }
            let upper_weight = f64::from(weight) * f64::from(t);
            if upper_weight > 0.0 {
                property_weights[property_weight_count] = (i1, upper_weight);
                property_weight_count += 1;
            }
        } else {
            let Some(d) = (match reflectivity_sampling {
                ReflectivitySampling::LegacyDbz => lerp(fields.dbz[i0], fields.dbz[i1], t),
                ReflectivitySampling::LinearZ => {
                    lerp(dbz_to_z(fields.dbz[i0]), dbz_to_z(fields.dbz[i1]), t)
                }
            }) else {
                continue;
            };
            reflectivity += weight * d;
        }

        if property_scattering.is_none()
            && let Some(polar) = &fields.polarimetric
        {
            let mut vertical = crate::wrf_radar_physics::PolarAccumulator::default();
            vertical.add(1.0 - t, polar.contribution_at(i0, dbz_to_z(fields.dbz[i0])));
            vertical.add(t, polar.contribution_at(i1, dbz_to_z(fields.dbz[i1])));
            polar_accumulator.add(weight, intrinsic_as_contribution(vertical.finalize()));
        }
        if let Some(tke_grid) = &fields.tke_tenths_m2s2 {
            let t0 = tke_grid[i0] as f32 * 0.1;
            let t1 = tke_grid[i1] as f32 * 0.1;
            tke += weight * (t0 + t * (t1 - t0));
        }
        wsum += weight;
        u += weight * su;
        v += weight * sv;
        w += weight * sw;
    }
    if wsum <= 0.0 {
        return Ok(None);
    }
    let (z_linear, polar) = if let Some(property) = property_scattering {
        if property_weight_count == 0 {
            return Ok(None);
        }
        let weights = &mut property_weights[..property_weight_count];
        let weight_sum = weights.iter().map(|(_, weight)| *weight).sum::<f64>();
        if !weight_sum.is_finite() || weight_sum <= 0.0 {
            return Err(format!(
                "property T-matrix trilinear weights have invalid sum {weight_sum}"
            ));
        }
        for (_, weight) in weights.iter_mut() {
            *weight /= weight_sum;
        }
        let normalized_sum = weights.iter().map(|(_, weight)| *weight).sum::<f64>();
        if let Some((_, last_weight)) = weights.last_mut() {
            *last_weight += 1.0 - normalized_sum;
        }
        let quantities = property
            .weighted_polar_at(weights, beam_elevation_deg)
            .map_err(|error| format!("query weighted property T-matrix column: {error}"))?;
        let mut accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
        accumulator.add(1.0, tmatrix_as_contribution(quantities));
        let sample = accumulator.finalize();
        (sample.zh, (sample.zh > 0.0).then_some(sample))
    } else {
        let reflectivity = reflectivity / wsum;
        let z_linear = match reflectivity_sampling {
            ReflectivitySampling::LegacyDbz => dbz_to_z(reflectivity),
            ReflectivitySampling::LinearZ => reflectivity,
        };
        let polar = fields.polarimetric.as_ref().and_then(|_| {
            let sample = normalize_intrinsic(polar_accumulator.finalize(), wsum);
            (sample.zh > 0.0).then_some(sample)
        });
        (z_linear, polar)
    };
    Ok(Some(ColumnSample {
        z_linear: polar.map_or(z_linear, |sample| sample.zh),
        u: u / wsum,
        v: v / wsum,
        w: w / wsum,
        polar,
        tke_m2s2: tke / wsum,
    }))
}

#[inline]
fn dbz_to_z(dbz: f32) -> f32 {
    if dbz.is_finite() {
        crate::wrf_radar_physics::dbz_to_z(dbz)
    } else {
        f32::NAN
    }
}

#[inline]
fn z_to_dbz(z: f32) -> f32 {
    if z.is_finite() && z > 0.0 {
        10.0 * z.log10()
    } else {
        f32::NAN
    }
}

/// Up to four `(column index, horizontal weight)` pairs for the WRF cell
/// containing (lat, lon), via the inverse LUT + a 2×2 bilinear solve. Falls
/// back to nearest-neighbour when the point is not cleanly inside a cell.
fn horizontal_stencil(fields: &WrfRadarFields, lat: f32, lon: f32) -> Option<[(usize, f32); 4]> {
    let nx = fields.nx;
    let ny = fields.ny;
    let nearest = fields.lut.lookup(lat, lon)?;
    if nx < 2 || ny < 2 {
        return Some([
            (nearest, 1.0),
            (nearest, 0.0),
            (nearest, 0.0),
            (nearest, 0.0),
        ]);
    }
    let row = nearest / nx;
    let col = nearest % nx;
    let target_lon = f64::from(lon);
    let target_lat = f64::from(lat);
    for y0 in neighboring_cell_starts(row, ny).into_iter().flatten() {
        for x0 in neighboring_cell_starts(col, nx).into_iter().flatten() {
            let i00 = y0 * nx + x0;
            let i10 = i00 + 1;
            let i01 = i00 + nx;
            let i11 = i01 + 1;
            let corners = [
                (
                    unwrap_lon_near(f64::from(fields.lon[i00]), target_lon),
                    f64::from(fields.lat[i00]),
                ),
                (
                    unwrap_lon_near(f64::from(fields.lon[i10]), target_lon),
                    f64::from(fields.lat[i10]),
                ),
                (
                    unwrap_lon_near(f64::from(fields.lon[i01]), target_lon),
                    f64::from(fields.lat[i01]),
                ),
                (
                    unwrap_lon_near(f64::from(fields.lon[i11]), target_lon),
                    f64::from(fields.lat[i11]),
                ),
            ];
            let Some((uu, vv)) = solve_bilinear_coords(corners, target_lon, target_lat) else {
                continue;
            };
            if !((-0.02..=1.02).contains(&uu) && (-0.02..=1.02).contains(&vv)) {
                continue;
            }
            let uu = uu.clamp(0.0, 1.0) as f32;
            let vv = vv.clamp(0.0, 1.0) as f32;
            return Some([
                (i00, (1.0 - uu) * (1.0 - vv)),
                (i10, uu * (1.0 - vv)),
                (i01, (1.0 - uu) * vv),
                (i11, uu * vv),
            ]);
        }
    }
    Some([
        (nearest, 1.0),
        (nearest, 0.0),
        (nearest, 0.0),
        (nearest, 0.0),
    ])
}

/// Bracket a target MSL height in a WRF column (height increases with model
/// level index k). Returns the lower level and the linear weight, or `None`
/// when the target is below the lowest level (below terrain) or above the top.
fn bracket_height(
    fields: &WrfRadarFields,
    cells: usize,
    col: usize,
    z: f32,
) -> Option<(usize, f32)> {
    let nz = fields.nz;
    let h0 = fields.height_msl[col];
    let htop = fields.height_msl[(nz - 1) * cells + col];
    if !h0.is_finite() || !htop.is_finite() || z < h0 || z > htop {
        return None;
    }
    // WRF mass-level heights are monotonic in ordinary columns. Binary search
    // matters once a pulse gate carries 9 or 27 quadrature points: the former
    // O(nz) scan otherwise dominates the forward operator on deep nests.
    let mut lo = 0usize;
    let mut hi = nz - 1;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        let hm = fields.height_msl[mid * cells + col];
        if !hm.is_finite() {
            break;
        }
        if hm <= z {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let hlo = fields.height_msl[lo * cells + col];
    let hhi = fields.height_msl[(lo + 1) * cells + col];
    if hlo.is_finite() && hhi.is_finite() && hhi > hlo && z >= hlo && z <= hhi {
        return Some((lo, (z - hlo) / (hhi - hlo)));
    }

    // A malformed/non-monotonic column is uncommon but should not turn into a
    // fabricated gate. Fall back to the former defensive linear search.
    (0..nz - 1).find_map(|k| {
        let hk = fields.height_msl[k * cells + col];
        let hk1 = fields.height_msl[(k + 1) * cells + col];
        (hk.is_finite() && hk1.is_finite() && hk1 > hk && z >= hk && z <= hk1)
            .then_some((k, (z - hk) / (hk1 - hk)))
    })
}

fn lerp(a: f32, b: f32, t: f32) -> Option<f32> {
    (a.is_finite() && b.is_finite()).then_some(a + t * (b - a))
}

// ── Gate texture (opt-in speckle) ─────────────────────────────────────────────
//
// Deliberately hash-based, NOT an RNG: every perturbation is a pure function
// of (tilt, azimuth, gate), so rebuilding a frame is bit-identical and a loop
// never shimmers between rebuilds of the same hour. Magnitudes are tuned
// against real Level-II: reflectivity texture on the order of ±2 dB,
// correlated over a few gates in range (the pulse volume smears neighbours)
// plus a smaller fully independent per-gate jitter; velocity gets only a
// gentle ±0.5 m/s wobble — pulse-pair Vr estimates are far smoother than
// reflectivity at decent SNR, and a noisy Vr would pollute the dealias/GBVTD
// consumers downstream.

/// Peak amplitude (dB) of the range-correlated reflectivity texture.
const REF_TEXTURE_CORRELATED_DB: f32 = 1.8;
/// Peak amplitude (dB) of the independent per-gate reflectivity jitter.
const REF_TEXTURE_JITTER_DB: f32 = 0.7;
/// Peak amplitude (m/s) of the (correlated) velocity wobble.
const VEL_TEXTURE_MPS: f32 = 0.5;
/// Range correlation length of the correlated components, in gates.
const TEXTURE_CORR_GATES: usize = 3;

const TEXTURE_SALT_REF: u32 = 0x5265_6631; // "Ref1"
const TEXTURE_SALT_REF_JITTER: u32 = 0x5265_664A; // "RefJ"
const TEXTURE_SALT_VEL: u32 = 0x5665_6C31; // "Vel1"

/// 32-bit avalanche mix (splitmix32 finalizer) — platform-independent.
fn texture_mix(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// Uniform noise in [-1, 1) from (tilt, azimuth, knot) + a salt.
fn texture_noise(tilt: usize, az: usize, knot: usize, salt: u32) -> f32 {
    let mixed = texture_mix(
        (tilt as u32).wrapping_mul(0x9e37_79b9)
            ^ (az as u32).wrapping_mul(0x85eb_ca6b)
            ^ (knot as u32).wrapping_mul(0xc2b2_ae35)
            ^ salt,
    );
    ((mixed >> 8) as f32) * (2.0 / 16_777_216.0) - 1.0
}

/// Range-correlated value noise in [-1, 1): hashed knots every
/// [`TEXTURE_CORR_GATES`] gates, linearly interpolated between them.
fn texture_correlated_noise(tilt: usize, az: usize, gate: usize, salt: u32) -> f32 {
    let knot = gate / TEXTURE_CORR_GATES;
    let t = (gate % TEXTURE_CORR_GATES) as f32 / TEXTURE_CORR_GATES as f32;
    let n0 = texture_noise(tilt, az, knot, salt);
    let n1 = texture_noise(tilt, az, knot + 1, salt);
    n0 + t * (n1 - n0)
}

/// Reflectivity gate-texture perturbation (dB), peak ±2.5 dB.
fn ref_gate_texture_db(tilt: usize, az: usize, gate: usize) -> f32 {
    REF_TEXTURE_CORRELATED_DB * texture_correlated_noise(tilt, az, gate, TEXTURE_SALT_REF)
        + REF_TEXTURE_JITTER_DB * texture_noise(tilt, az, gate, TEXTURE_SALT_REF_JITTER)
}

/// Velocity gate-texture perturbation (m/s), peak ±0.5 m/s.
fn vel_gate_texture_mps(tilt: usize, az: usize, gate: usize) -> f32 {
    VEL_TEXTURE_MPS * texture_correlated_noise(tilt, az, gate, TEXTURE_SALT_VEL)
}

// ── Ground clutter (opt-in, fabricated) ───────────────────────────────────────
//
// Our forward operator is pure physics and produces ZERO clutter. The community
// WRF→GR2 export script fabricates a near-radar ground-return look
// (`add_ground_clutter`): an exponential range falloff (hard-capped at 40 km),
// a height-AGL falloff, an elevation falloff on the lowest ~7 tilts, a
// sinusoidal azimuthal ripple, and a handful of random near-in hotspots, applied
// only where the existing echo is weaker. This is a native port of that look,
// dialled by `SyntheticRadarConfig::clutter_intensity` (0 = off, 1 ≈ the script).
//
// Like the gate texture above, every draw is a pure hash of
// (frame seed, tilt, azimuth, gate) — deterministic, so a rebuilt frame is
// bit-identical and a loop never shimmers. UNLIKE the texture, the FRAME SEED
// (site id + valid time) is folded in, so distinct forecast hours get distinct
// clutter, reproducing the script's per-time-step variation without a live RNG.
//
// Antenna height: the script keys clutter on beam height AGL. We do not have a
// per-gate terrain lookup inside the sampling loop, so we use the beam height
// ABOVE THE ANTENNA as the AGL proxy — exact at the radar and a good
// approximation within the 40 km clutter cap, where terrain relief is a
// second-order effect on near-in ground return.

/// Clutter is confined to the lowest tilts. The community script applies it to
/// the first 7 elevation cuts (`n < 7`); we mirror that by cut index, so it
/// tracks the near-ground tilts whether or not the optional 0.1° tilt is added.
const CLUTTER_TILT_LIMIT: usize = 7;
/// Maximum ground range for any clutter (km). Beyond this the script produces
/// none; near-radar concentration is the whole point.
const CLUTTER_MAX_RANGE_KM: f32 = 40.0;
/// Base clutter reflectivity (dBZ) before the combined-factor scaling, matching
/// the script's `clutter_base = 22 * combined_factor`.
const CLUTTER_BASE_DBZ: f32 = 22.0;
/// Multiplier applied inside a hotspot rectangle (the script's `*= 1.8`).
const CLUTTER_HOTSPOT_BOOST: f32 = 1.8;

const CLUTTER_SALT_ROLL: u32 = 0x436c_5250; // "ClRP" — apply/skip roll
const CLUTTER_SALT_NOISE: u32 = 0x436c_4e5a; // "ClNZ" — dBZ noise
const CLUTTER_SALT_TEXTURE: u32 = 0x436c_5458; // "ClTX" — dBZ texture
const CLUTTER_SALT_VEL: u32 = 0x436c_564c; // "ClVL" — near-zero velocity jitter
const CLUTTER_SALT_HOTSPOT: u32 = 0x436c_4853; // "ClHS" — hotspot stream seed

/// One deterministic seed per forecast frame: site id + valid-time seconds,
/// avalanche-mixed. Folded into every clutter draw so the SAME hour rebuilds
/// identically while DISTINCT hours vary (the script's `time_step` role).
fn clutter_frame_seed(site_id: &str, valid_time: DateTime<Utc>) -> u32 {
    // FNV-1a over the site id bytes, then fold in the timestamp seconds.
    let mut acc = 0x811c_9dc5u32;
    for byte in site_id.as_bytes() {
        acc ^= u32::from(*byte);
        acc = acc.wrapping_mul(0x0100_0193);
    }
    let secs = valid_time.timestamp();
    acc ^= (secs as u64 as u32).wrapping_mul(0x9e37_79b9);
    acc ^= ((secs as u64 >> 32) as u32).wrapping_mul(0x85eb_ca6b);
    texture_mix(acc)
}

/// A tiny counter-based splitmix32 stream (state += golden ratio, then the
/// [`texture_mix`] avalanche) for the SERIAL per-tilt hotspot layout. Reusing
/// the gate-texture finalizer keeps every random draw on one platform-
/// independent mixer.
struct SplitMix32 {
    state: u32,
}

impl SplitMix32 {
    fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_add(0x9e37_79b9);
        texture_mix(self.state)
    }

    /// Uniform in [0, 1).
    fn next_unit(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    /// Uniform integer in [0, n); 0 when `n == 0`.
    fn next_below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            ((self.next_unit() * n as f32) as usize).min(n - 1)
        }
    }
}

/// A near-radar hotspot rectangle in (azimuth index, gate index) space — the
/// script's `hotspot_mask[ray_min:ray_max, gate_min:gate_max]`.
struct ClutterRect {
    ray_min: usize,
    ray_max: usize,
    gate_min: usize,
    gate_max: usize,
}

impl ClutterRect {
    /// Whether gate (azimuth index, range index) falls inside this rectangle.
    fn contains(&self, az: usize, gate: usize) -> bool {
        let in_az = (self.ray_min..self.ray_max).contains(&az);
        let in_gate = (self.gate_min..self.gate_max).contains(&gate);
        in_az && in_gate
    }
}

/// The 3–8 near-radar hotspots for one tilt, laid out deterministically from
/// the frame seed + tilt index. Positions/sizes shrink with elevation, mirroring
/// the script (`max_gate = max(10, 100*(1 - n/8))`, ray/gate sizes `* (1 - n/8)`).
fn clutter_hotspots(
    frame_seed: u32,
    tilt: usize,
    naz: usize,
    gate_count: usize,
) -> Vec<ClutterRect> {
    let tilt_seed =
        texture_mix(frame_seed ^ (tilt as u32).wrapping_mul(0x9e37_79b9) ^ CLUTTER_SALT_HOTSPOT);
    let mut rng = SplitMix32::new(tilt_seed);
    let count = 3 + rng.next_below(6); // 3..=8, like the script
    let shrink = (1.0 - tilt as f32 / 8.0).max(0.0);
    let max_gate = ((100.0 * shrink) as usize).max(10).min(gate_count.max(1));
    let ray_size = ((20.0 * shrink) as usize).max(4);
    let gate_size = ((15.0 * shrink) as usize).max(4);
    let gate_lo = 5.min(max_gate.saturating_sub(1));
    let mut rects = Vec::with_capacity(count);
    for _ in 0..count {
        let ray = rng.next_below(naz.max(1));
        let center_gate = gate_lo + rng.next_below((max_gate - gate_lo).max(1));
        rects.push(ClutterRect {
            ray_min: ray.saturating_sub(ray_size / 2),
            ray_max: (ray + ray_size / 2 + 1).min(naz),
            gate_min: center_gate.saturating_sub(gate_size / 2),
            gate_max: (center_gate + gate_size / 2 + 1).min(gate_count),
        });
    }
    rects
}

/// Whether gate (az index, range index) falls inside any hotspot rectangle.
fn in_clutter_hotspot(hotspots: &[ClutterRect], az: usize, gate: usize) -> bool {
    hotspots.iter().any(|h| h.contains(az, gate))
}

/// A hashed uniform in [0, 1) from (frame seed, tilt, az, gate) + a salt — the
/// parallel-safe per-gate analogue of [`texture_noise`], order-independent so it
/// is identical whatever order rayon visits the radials in.
fn clutter_unit(frame_seed: u32, tilt: usize, az: usize, gate: usize, salt: u32) -> f32 {
    let mixed = texture_mix(
        frame_seed
            ^ (tilt as u32).wrapping_mul(0x9e37_79b9)
            ^ (az as u32).wrapping_mul(0x85eb_ca6b)
            ^ (gate as u32).wrapping_mul(0xc2b2_ae35)
            ^ salt,
    );
    (mixed >> 8) as f32 * (1.0 / 16_777_216.0)
}

/// Hashed signed noise in [-1, 1).
fn clutter_signed(frame_seed: u32, tilt: usize, az: usize, gate: usize, salt: u32) -> f32 {
    clutter_unit(frame_seed, tilt, az, gate, salt) * 2.0 - 1.0
}

/// The fabricated ground-clutter reflectivity (dBZ) for one gate, or `None` when
/// no clutter falls here (out of range, past the tilt limit, or the probability
/// roll misses). `intensity` (0..=1) scales BOTH the probability and the
/// brightness, so intermediate values are rarer AND dimmer; at 1.0 the model
/// matches the community script (bar its live-RNG boil, which the frame seed
/// reproduces deterministically). Native port of `add_ground_clutter`.
#[allow(clippy::too_many_arguments)]
fn ground_clutter_dbz(
    frame_seed: u32,
    tilt: usize,
    az: usize,
    gate: usize,
    az_deg: f32,
    ground_range_km: f32,
    beam_height_agl_m: f32,
    in_hotspot: bool,
    intensity: f32,
) -> Option<f32> {
    if tilt >= CLUTTER_TILT_LIMIT || ground_range_km > CLUTTER_MAX_RANGE_KM {
        return None;
    }
    // Combined intensity factor (the script's product of falloffs).
    let tilt_factor = (-(tilt as f32) * 0.8).exp();
    let distance_factor = (-ground_range_km / 15.0).exp();
    let height_factor = (-beam_height_agl_m.max(0.0) / 100.0).exp();
    let azimuthal = (az_deg * 3.0).to_radians().sin() * 0.3 + 0.7;
    let mut combined = distance_factor * height_factor * tilt_factor * azimuthal;
    if in_hotspot {
        combined *= CLUTTER_HOTSPOT_BOOST;
    }
    if combined <= 0.0 {
        return None;
    }

    // Probability of a clutter gate (the script's `combined * (0.9 - n*0.08)`),
    // scaled by the user amount. A per-gate roll decides apply vs skip.
    let base_prob = (combined * (0.9 - tilt as f32 * 0.08)).max(0.0);
    let prob = base_prob * intensity;
    if clutter_unit(frame_seed, tilt, az, gate, CLUTTER_SALT_ROLL) >= prob {
        return None;
    }

    // Base ≈ 22·combined dBZ + range-dependent noise, clipped 5..35, plus a
    // little texture — then scaled by the user amount so lower settings are
    // dimmer as well as rarer.
    let noise_factor = 4.0 - 3.0 * tilt as f32 / 7.0;
    let noise = clutter_signed(frame_seed, tilt, az, gate, CLUTTER_SALT_NOISE) * noise_factor;
    let mut value = (CLUTTER_BASE_DBZ * combined + noise).clamp(5.0, 35.0);
    value += clutter_signed(frame_seed, tilt, az, gate, CLUTTER_SALT_TEXTURE) * combined;
    Some(value * intensity)
}

/// The near-zero radial velocity (m/s) stamped on a clutter-dominated gate:
/// the ground is stationary, so ~0 with a small deterministic ±0.5 m/s jitter.
fn clutter_velocity_mps(frame_seed: u32, tilt: usize, az: usize, gate: usize) -> f32 {
    0.5 * clutter_signed(frame_seed, tilt, az, gate, CLUTTER_SALT_VEL)
}

/// Parse a WRF `Times` string ("YYYY-MM-DD_HH:MM:SS") to a UTC scan time.
fn parse_wrf_time(raw: &str) -> Option<DateTime<Utc>> {
    let cleaned = raw.trim().replace('_', " ");
    NaiveDateTime::parse_from_str(&cleaned, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// The loop-engine history path key for one synthetic forecast frame. The key
/// is the dedupe/refresh discriminator the upsert rules key on
/// ([`ui_core::loop_engine::LoopEngine::install_batch`]): a re-import whose
/// [`SyntheticRadarConfig::data_fingerprint`] matches produces the SAME key (the
/// engine reuses the stored volume, rule (b)), while ANY config change produces
/// a DIFFERENT key so the freshly-built volume replaces the stale one (rule (c),
/// equal status + different path). `index` + `stamp` keep it unique per frame
/// within one build; `site_id` keeps it human-readable in status/telemetry.
pub fn synthetic_frame_path(
    site_id: &str,
    config_fingerprint: u64,
    index: usize,
    stamp: &str,
) -> PathBuf {
    PathBuf::from(format!(
        "wrf-synth://{site_id}/{config_fingerprint:016x}/{index:04}_{stamp}"
    ))
}

// ── Background job: WRF file(s) → Vec<Arc<RadarVolume>> ───────────────────────

#[derive(Debug)]
pub enum SyntheticRadarMessage {
    Progress(String),
    Done(Result<SyntheticRadarOutput, String>),
}

/// Result of a finished synthetic-radar job: one volume per WRF forecast time,
/// ready to feed the loop engine as a looping sequence.
pub struct SyntheticRadarOutput {
    pub label: String,
    pub volumes: Vec<Arc<RadarVolume>>,
    pub notes: Vec<String>,
    /// Effective build fingerprint: [`SyntheticRadarConfig::data_fingerprint`]
    /// plus the run/domain/grid and basename-free scene identities/time indices
    /// that built these volumes. The install path folds it into each frame's
    /// history key so either changed settings OR changed WRF input replaces a
    /// stale result without exposing private absolute paths.
    pub config_fingerprint: u64,
}

impl std::fmt::Debug for SyntheticRadarOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntheticRadarOutput")
            .field("label", &self.label)
            .field("volumes", &self.volumes.len())
            .field("notes", &self.notes)
            .field("config_fingerprint", &self.config_fingerprint)
            .finish()
    }
}

pub struct SyntheticRadarTask {
    pub label: String,
    pub rx: Receiver<SyntheticRadarMessage>,
}

pub enum SyntheticRadarReplayMessage {
    Progress(String),
    Done(Result<SyntheticRadarReplayOutput, String>),
}

pub struct SyntheticRadarReplayFrame {
    pub observed: Arc<RadarVolume>,
    pub simulated: Arc<RadarVolume>,
    pub difference: Arc<RadarVolume>,
    pub unavailable_observed_moments: Vec<UnavailableObservedMoment>,
}

pub struct SyntheticRadarReplayOutput {
    pub label: String,
    pub frames: Vec<SyntheticRadarReplayFrame>,
    pub notes: Vec<String>,
    pub config_fingerprint: u64,
}

pub struct SyntheticRadarReplayTask {
    pub label: String,
    pub rx: Receiver<SyntheticRadarReplayMessage>,
}

/// Existing-file worker seam for the ThreeStacked validation workspace. It
/// reuses the normal inventory, WRF field-read, temporal-plan, and progress
/// worker, then attaches the retained observed source and exact differences
/// without rereading either WRF or observed gate values.
pub fn spawn_synthetic_radar_replay(
    paths: Vec<PathBuf>,
    mut config: SyntheticRadarConfig,
    observed: Arc<RadarVolume>,
) -> SyntheticRadarReplayTask {
    let label = format!("Exact radar replay from {} WRF source(s)", paths.len());
    let (tx, rx) = channel();
    let template = match ExactScanTemplate::from_volume(&observed) {
        Ok(template) => template,
        Err(error) => {
            let _ = tx.send(SyntheticRadarReplayMessage::Done(Err(format!(
                "extract exact observed scan template: {error}"
            ))));
            return SyntheticRadarReplayTask { label, rx };
        }
    };
    normalize_replay_config_for_observed(&mut config, &observed);
    config.exact_replay_template = Some(Arc::new(template));
    let base = spawn_synthetic_radar(paths, config);
    std::thread::Builder::new()
        .name("wrf-exact-replay-products".to_string())
        .spawn(move || {
            while let Ok(message) = base.rx.recv() {
                match message {
                    SyntheticRadarMessage::Progress(progress) => {
                        let _ = tx.send(SyntheticRadarReplayMessage::Progress(progress));
                    }
                    SyntheticRadarMessage::Done(result) => {
                        let replay_result = result.and_then(|output| {
                            let mut frames = Vec::with_capacity(output.volumes.len());
                            let mut notes = output.notes;
                            notes.push(
                                "Exact replay uses observed radial Nyquist/PRT; custom coupled PRF, stage diagnostics, and manual folding were disabled"
                                    .to_string(),
                            );
                            for simulated in output.volumes {
                                let overlap = build_difference_volume_overlap(&observed, &simulated)
                                    .map_err(|error| {
                                        format!(
                                            "build exact-geometry replay difference: {error}"
                                        )
                                    })?;
                                if !overlap.unavailable_observed_moments.is_empty() {
                                    notes.push(format!(
                                        "Replay unavailable observed moments: {}",
                                        overlap
                                            .unavailable_observed_moments
                                            .iter()
                                            .map(|entry| format!(
                                                "cut {} {} ({})",
                                                entry.cut, entry.moment, entry.reason
                                            ))
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    ));
                                }
                                frames.push(SyntheticRadarReplayFrame {
                                    observed: Arc::clone(&observed),
                                    simulated,
                                    difference: Arc::new(overlap.volume),
                                    unavailable_observed_moments: overlap
                                        .unavailable_observed_moments,
                                });
                            }
                            Ok(SyntheticRadarReplayOutput {
                                label: output.label,
                                frames,
                                notes,
                                config_fingerprint: output.config_fingerprint,
                            })
                        });
                        let _ = tx.send(SyntheticRadarReplayMessage::Done(replay_result));
                        break;
                    }
                }
            }
        })
        .expect("spawn exact replay product bridge");
    SyntheticRadarReplayTask { label, rx }
}

/// Spawn a worker that turns each forecast time of the given wrfout file(s)
/// into a simulated [`RadarVolume`]. Streams progress, then a `Done`.
pub fn spawn_synthetic_radar(
    paths: Vec<PathBuf>,
    config: SyntheticRadarConfig,
) -> SyntheticRadarTask {
    let mut label = if paths.len() == 1 {
        format!("Simulated radar from {}", display_name(&paths[0]))
    } else {
        format!("Simulated radar from {} WRF files", paths.len())
    };
    if matches!(
        config.polarimetric_kernel,
        PolarimetricKernel::PropertyTMatrixResearchV1
    ) {
        label.push_str(" [T-matrix research]");
    }
    let (tx, rx) = channel();
    let label_for_thread = label.clone();
    std::thread::Builder::new()
        .name("rw-ui-wrf-synth-radar".to_string())
        .spawn(move || {
            let result = build_synthetic_from_paths(&paths, &config, &label_for_thread, &tx);
            let _ = tx.send(SyntheticRadarMessage::Done(result));
        })
        .expect("spawn WRF synthetic-radar worker");
    SyntheticRadarTask { label, rx }
}

fn build_synthetic_from_paths(
    paths: &[PathBuf],
    config: &SyntheticRadarConfig,
    label: &str,
    tx: &Sender<SyntheticRadarMessage>,
) -> Result<SyntheticRadarOutput, String> {
    config.validate_science_contract()?;
    if paths.is_empty() {
        return Err("No WRF files selected".to_string());
    }
    let files: Vec<PathBuf> = paths
        .iter()
        .filter(|path| crate::wrf_process::is_supported_wrf_file(path))
        .cloned()
        .collect();
    if files.is_empty() {
        return Err("No supported WRF files selected".to_string());
    }

    let _ = tx.send(SyntheticRadarMessage::Progress(
        "Inventorying WRF run, domain, grid, and internal times…".to_owned(),
    ));
    let selected = inventory_selected_wrf_paths(&files)?;
    let scenes = selected.group.scenes.clone();
    let config_fingerprint = scene_build_fingerprint(config, &selected.group);
    let property_build_reservation_bytes = if matches!(
        config.polarimetric_kernel,
        PolarimetricKernel::PropertyTMatrixResearchV1
    ) {
        let estimate = temporal_memory_estimate(&selected.group, config, scenes.len())?;
        estimate
            .shared_static_bytes
            .checked_add(estimate.output_bytes)
            .ok_or_else(|| "Property T-matrix output-memory reservation overflowed".to_string())?
    } else {
        0
    };

    let mut notes = selected
        .notes
        .into_iter()
        .map(|note| format!("{}: {}", note.source_name, note.message))
        .collect::<Vec<_>>();
    if matches!(
        config.polarimetric_kernel,
        PolarimetricKernel::PropertyTMatrixResearchV1
    ) {
        notes.push(if matches!(
            config.atmosphere_time_mode,
            AtmosphereTimeMode::RawStateLinear
        ) {
            "T-matrix output is research_only_unvalidated; raw_state_linear blends full spatial/temporal P3/ISHMAEL state before one closure/scattering evaluation"
                .to_string()
        } else {
            "T-matrix output is research_only_unvalidated; P3 is characteristic-particle and ISHMAEL is scheme-native PSD; not operational calibration"
                .to_string()
        });
    }
    if config.atmosphere_time_mode.uses_adjacent_scene() {
        return build_temporal_synthetic_from_scenes(
            &selected.group,
            config,
            label,
            tx,
            notes,
            config_fingerprint,
        );
    }

    let mut volumes = Vec::new();
    let one_scene_per_file = {
        let mut identities = std::collections::BTreeSet::new();
        scenes
            .iter()
            .all(|scene| identities.insert(scene.source_identity.clone()))
    };
    let frame_total = scenes.len();
    let mut opened_path: Option<PathBuf> = None;
    let mut opened_file: Option<WrfFile> = None;
    for (frame_index, scene) in scenes.iter().enumerate() {
        if opened_path.as_ref() != Some(&scene.path) {
            opened_file = Some(WrfFile::open(&scene.path).map_err(|error| {
                format!(
                    "Open inventoried WRF {}: {error}",
                    display_name(&scene.path)
                )
            })?);
            opened_path = Some(scene.path.clone());
        }
        let file = opened_file
            .as_ref()
            .expect("inventoried WRF file opened above");
        let name = display_name(&scene.path);
        let timeidx = scene.time_index;
        let frame_prefix = if one_scene_per_file && frame_total > 1 {
            // Preserve the established multi-file progress vocabulary.
            format!("file {}/{frame_total} ({name}): ", frame_index + 1)
        } else if frame_total > 1 {
            format!(
                "frame {}/{frame_total} ({name}, time {}/{}): ",
                frame_index + 1,
                timeidx + 1,
                file.nt
            )
        } else if file.nt > 1 {
            format!("Simulating {name} (time {}/{}): ", timeidx + 1, file.nt)
        } else {
            format!("Simulating {name}: ")
        };
        let progress = |stage: &str| {
            let _ = tx.send(SyntheticRadarMessage::Progress(format!(
                "{frame_prefix}{stage}"
            )));
        };
        progress("reading…");
        let fields = match read_wrf_radar_fields_for_config_reporting(
            file,
            &scene.source_identity,
            timeidx,
            config,
            &progress,
            property_build_reservation_bytes,
        ) {
            Ok(fields) => fields,
            Err(error) => {
                record_or_propagate_scene_failure(
                    config,
                    &mut notes,
                    format!("{name} time {timeidx}: {error}"),
                )?;
                continue;
            }
        };
        let valid_time = scene
            .time
            .valid_time()
            .cloned()
            .expect("scene adapter rejects untimed scenes");
        let mut volume =
            match try_build_synthetic_volume_reporting(&fields, valid_time, config, &progress) {
                Ok(volume) => volume,
                Err(error) => {
                    record_or_propagate_scene_failure(
                        config,
                        &mut notes,
                        format!("{name} time {timeidx}: {error}"),
                    )?;
                    continue;
                }
            };
        append_scene_provenance(&mut volume, scene);
        // Self-document the gate spacing when grid-matching is on: record the
        // effective size and whether it came from the grid DX or fell back.
        let gate_note = if config.match_gate_to_grid {
            let eff = effective_gate_spacing(config, fields.dx_m);
            if matched_grid_dx(config, fields.dx_m) {
                format!(", gate {eff:.0} m (grid DX)")
            } else {
                format!(", gate {eff:.0} m (grid DX unavailable)")
            }
        } else {
            String::new()
        };
        let polar_note = fields
            .dual_pol_status
            .as_deref()
            .map(|status| format!(", {status}"))
            .unwrap_or_default();
        notes.push(format!(
            "{name} time {timeidx} ({}): {} radials from {}{gate_note}{polar_note}",
            scene_time_authority(&scene.time),
            volume.metadata.decoded_radial_count,
            fields.ref_source,
        ));
        volumes.push(Arc::new(volume));
    }

    if volumes.is_empty() {
        return Err(if notes.is_empty() {
            "WRF produced no simulated radar volumes".to_string()
        } else {
            format!(
                "WRF produced no simulated radar volumes: {}",
                notes.join("; ")
            )
        });
    }
    // `WrfSceneInventory` already supplies strict chronological order from
    // internal Times; retain it exactly so timeidx/provenance stay aligned.
    Ok(SyntheticRadarOutput {
        label: label.to_string(),
        volumes,
        notes,
        config_fingerprint,
    })
}

fn record_or_propagate_scene_failure(
    config: &SyntheticRadarConfig,
    notes: &mut Vec<String>,
    message: String,
) -> Result<(), String> {
    if matches!(
        config.polarimetric_kernel,
        PolarimetricKernel::PropertyTMatrixResearchV1
    ) {
        Err(message)
    } else {
        notes.push(message);
        Ok(())
    }
}

fn build_temporal_synthetic_from_scenes(
    group: &WrfSceneGroup,
    config: &SyntheticRadarConfig,
    label: &str,
    tx: &Sender<SyntheticRadarMessage>,
    mut notes: Vec<String>,
    config_fingerprint: u64,
) -> Result<SyntheticRadarOutput, String> {
    if !matches!(config.scan_timing, ScanTiming::TimedVolume) {
        return Err(
            "Adjacent-scene atmosphere interpolation requires Timed volume scan timing".to_string(),
        );
    }
    let scan_duration_ms = config.planned_scan_duration_ms();
    let scan_duration = Duration::milliseconds(scan_duration_ms);
    let mut plans = Vec::with_capacity(group.scenes.len());
    for (frame_index, scene) in group.scenes.iter().enumerate() {
        plans.push(
            plan_for_scene(
                group,
                frame_index,
                scan_duration,
                config.atmosphere_time_mode,
                config.missing_neighbor_policy,
            )
            .map_err(|error| {
                format!(
                    "Plan temporal sampling for {}: {error}",
                    display_name(&scene.path)
                )
            })?,
        );
    }
    let emitted_frame_count = plans.iter().filter(|plan| plan.is_some()).count();
    let requires_two_scenes = plans
        .iter()
        .flatten()
        .any(|plan| matches!(plan.outcome, TemporalSamplingOutcome::LinearAdjacent));
    let estimate = temporal_memory_estimate(group, config, emitted_frame_count)?;
    let budget_bytes = config
        .temporal_memory_budget_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "Temporal memory budget overflows address space".to_string())?;
    let required_bytes = temporal_required_peak_bytes(estimate, requires_two_scenes)
        .ok_or_else(|| "Temporal memory estimate overflowed".to_string())?;
    if required_bytes <= budget_bytes {
        let scene_label = if requires_two_scenes {
            "two-scene"
        } else {
            "single-scene held"
        };
        let property_suffix = matches!(
            config.polarimetric_kernel,
            PolarimetricKernel::PropertyTMatrixResearchV1
        )
        .then_some(" minimum; exact sparse T-matrix cache is checked after each scene read")
        .unwrap_or_default();
        notes.push(format!(
            "Temporal {scene_label} preflight: {:.2} GiB estimated peak within {:.2} GiB budget{property_suffix}",
            required_bytes as f64 / 1024.0_f64.powi(3),
            budget_bytes as f64 / 1024.0_f64.powi(3),
        ));
    } else {
        return Err(format!(
            "Temporal build needs an estimated {:.2} GiB, above the configured {:.2} GiB budget",
            required_bytes as f64 / 1024.0_f64.powi(3),
            budget_bytes as f64 / 1024.0_f64.powi(3),
        ));
    }

    let mut cache: TwoSceneCache<(String, usize), Arc<WrfRadarFields>> = TwoSceneCache::default();
    let mut volumes = Vec::new();
    for (frame_index, (scene, plan)) in group.scenes.iter().zip(plans).enumerate() {
        let Some(plan) = plan else {
            notes.push(format!(
                "{} time {} dropped: no adjacent WRF scene covers the timed scan",
                display_name(&scene.path),
                scene.time_index
            ));
            continue;
        };

        let frame_prefix = format!(
            "frame {}/{} ({}, time {}): ",
            frame_index + 1,
            group.scenes.len(),
            display_name(&scene.path),
            scene.time_index + 1,
        );
        let progress = |stage: &str| {
            let _ = tx.send(SyntheticRadarMessage::Progress(format!(
                "{frame_prefix}{stage}"
            )));
        };
        let anchor = match cached_temporal_fields(
            &mut cache,
            scene,
            config,
            &progress,
            estimate,
            budget_bytes,
        ) {
            Ok(fields) => fields,
            Err(error) => {
                record_or_propagate_scene_failure(
                    config,
                    &mut notes,
                    format!(
                        "{} time {}: {error}",
                        display_name(&scene.path),
                        scene.time_index
                    ),
                )?;
                continue;
            }
        };
        if let Some(status) = anchor.dual_pol_status.as_deref() {
            notes.push(format!(
                "{} time {} microphysics: {status}",
                display_name(&scene.path),
                scene.time_index
            ));
        }
        if (config.dual_pol || config.terminal_fall_speed) && !anchor.has_polarimetric_input() {
            notes.push(format!(
                "{} time {}: requested polarimetric/fall-speed physics unavailable; emitting explicitly labeled scalar fallback",
                display_name(&scene.path),
                scene.time_index
            ));
        }
        let valid_time = scene
            .time
            .valid_time()
            .cloned()
            .expect("scene adapter rejects untimed scenes");

        let (mut volume, runtime_outcome) = match plan.outcome {
            TemporalSamplingOutcome::LinearAdjacent => {
                let neighbor_locator = plan
                    .neighbor
                    .as_ref()
                    .expect("linear temporal plan carries a neighbor");
                let neighbor_scene = group
                    .scenes
                    .iter()
                    .find(|candidate| {
                        candidate.source_identity == neighbor_locator.source_identity
                            && candidate.time_index == neighbor_locator.time_index
                    })
                    .expect("temporal planner selected a scene from this group");
                let neighbor = match cached_temporal_fields(
                    &mut cache,
                    neighbor_scene,
                    config,
                    &progress,
                    estimate,
                    budget_bytes,
                ) {
                    Ok(fields) => TemporalNeighborResolution::Fields(fields),
                    Err(error) => match config.missing_neighbor_policy {
                        MissingNeighborPolicy::HoldAnchor => {
                            notes.push(format!(
                                "{} time {} held at anchor: adjacent scene read failed ({error})",
                                display_name(&scene.path),
                                scene.time_index
                            ));
                            let volume = try_build_synthetic_volume_reporting(
                                &anchor, valid_time, config, &progress,
                            )?;
                            TemporalNeighborResolution::Held(
                                volume,
                                "held_anchor_neighbor_read_error".to_string(),
                            )
                        }
                        MissingNeighborPolicy::DropFrame => {
                            notes.push(format!(
                                "{} time {} dropped: adjacent scene read failed ({error})",
                                display_name(&scene.path),
                                scene.time_index
                            ));
                            continue;
                        }
                        MissingNeighborPolicy::Error => {
                            return Err(format!(
                                "Read adjacent WRF scene for {}: {error}",
                                display_name(&scene.path)
                            ));
                        }
                    },
                };
                match neighbor {
                    TemporalNeighborResolution::Held(volume, outcome) => (volume, outcome),
                    TemporalNeighborResolution::Fields(neighbor) => {
                        if let Some(status) = neighbor.dual_pol_status.as_deref() {
                            notes.push(format!(
                                "{} time {} adjacent microphysics: {status}",
                                display_name(&neighbor_scene.path),
                                neighbor_scene.time_index
                            ));
                        }
                        if let Err(error) = validate_temporal_field_pair(&anchor, &neighbor) {
                            match config.missing_neighbor_policy {
                                MissingNeighborPolicy::HoldAnchor => {
                                    notes.push(format!(
                                        "{} time {} held at anchor: {error}",
                                        display_name(&scene.path),
                                        scene.time_index
                                    ));
                                    (
                                        try_build_synthetic_volume_reporting(
                                            &anchor, valid_time, config, &progress,
                                        )?,
                                        "held_anchor_property_mismatch".to_string(),
                                    )
                                }
                                MissingNeighborPolicy::DropFrame => {
                                    notes.push(format!(
                                        "{} time {} dropped: {error}",
                                        display_name(&scene.path),
                                        scene.time_index
                                    ));
                                    continue;
                                }
                                MissingNeighborPolicy::Error => return Err(error),
                            }
                        } else {
                            (
                                build_synthetic_volume_reporting_temporal(
                                    &anchor, &neighbor, valid_time, config, &progress, &plan,
                                )?,
                                config.atmosphere_time_mode.provenance_name().to_string(),
                            )
                        }
                    }
                }
            }
            TemporalSamplingOutcome::Frozen => (
                try_build_synthetic_volume_reporting(&anchor, valid_time, config, &progress)?,
                "frozen".to_string(),
            ),
            TemporalSamplingOutcome::HeldAnchor(reason) => (
                try_build_synthetic_volume_reporting(&anchor, valid_time, config, &progress)?,
                match reason {
                    HoldReason::NoLaterScene => "held_anchor_no_later_scene".to_string(),
                    HoldReason::ScanCrossesNeighbor => {
                        "held_anchor_scan_crosses_neighbor".to_string()
                    }
                },
            ),
        };
        append_scene_provenance(&mut volume, scene);
        append_temporal_provenance(&mut volume, &plan, &runtime_outcome, config, &anchor);
        notes.push(format!(
            "{} time {}: {} radials, atmosphere {runtime_outcome}",
            display_name(&scene.path),
            scene.time_index,
            volume.metadata.decoded_radial_count,
        ));
        volumes.push(Arc::new(volume));
    }

    if volumes.is_empty() {
        return Err(if notes.is_empty() {
            "WRF temporal build produced no simulated radar volumes".to_string()
        } else {
            format!(
                "WRF temporal build produced no simulated radar volumes: {}",
                notes.join("; ")
            )
        });
    }
    Ok(SyntheticRadarOutput {
        label: label.to_string(),
        volumes,
        notes,
        config_fingerprint,
    })
}

// This short-lived control value favors clear ownership at the scan boundary;
// it is never stored in a collection or retained per ray/gate.
#[allow(clippy::large_enum_variant)]
enum TemporalNeighborResolution {
    Fields(Arc<WrfRadarFields>),
    Held(RadarVolume, String),
}

fn temporal_memory_estimate(
    group: &WrfSceneGroup,
    config: &SyntheticRadarConfig,
    emitted_frame_count: usize,
) -> Result<TemporalMemoryEstimate, String> {
    let grid = &group.key.grid_signature;
    let nz = grid.nz.ok_or_else(|| {
        "WRF grid has no vertical-size metadata for temporal preflight".to_string()
    })?;
    let horizontal_cells = grid
        .nx
        .checked_mul(grid.ny)
        .ok_or_else(|| "WRF horizontal grid size overflowed".to_string())?;
    let cells_per_scene = horizontal_cells
        .checked_mul(nz)
        .ok_or_else(|| "WRF 3-D grid size overflowed".to_string())?;
    let mut compact_bytes_per_scene = horizontal_cells
        .checked_mul(28)
        .ok_or_else(|| "WRF geolocation memory estimate overflowed".to_string())?;
    if matches!(
        config.polarimetric_kernel,
        PolarimetricKernel::PropertyTMatrixResearchV1
    ) {
        // RawStateLinear retains dense temperature, pressure, moist density,
        // and dry density. LinearAdjacent retains the compact scene's dense
        // full-cell -> sparse-row lookup. Sparse category/output bytes are
        // added from exact runtime memory estimates after each scene read.
        let property_dense_bytes_per_cell = if matches!(
            config.atmosphere_time_mode,
            AtmosphereTimeMode::RawStateLinear
        ) {
            4 * std::mem::size_of::<f32>()
        } else {
            std::mem::size_of::<u32>()
        };
        compact_bytes_per_scene = compact_bytes_per_scene
            .checked_add(
                cells_per_scene
                    .checked_mul(property_dense_bytes_per_cell)
                    .ok_or_else(|| "WRF property memory estimate overflowed".to_string())?,
            )
            .ok_or_else(|| "WRF property memory estimate overflowed".to_string())?;
    } else if config.dual_pol || config.terminal_fall_speed {
        compact_bytes_per_scene = compact_bytes_per_scene
            .checked_add(
                cells_per_scene
                    .checked_mul(9)
                    .ok_or_else(|| "WRF polar memory estimate overflowed".to_string())?,
            )
            .ok_or_else(|| "WRF polar memory estimate overflowed".to_string())?;
    }
    if config.spectrum_width {
        compact_bytes_per_scene = compact_bytes_per_scene
            .checked_add(cells_per_scene)
            .ok_or_else(|| "WRF TKE memory estimate overflowed".to_string())?;
    }

    let dx_m = grid.dx_millimeters.map(|value| value as f64 / 1_000.0);
    let spacing_m = effective_gate_spacing(config, dx_m).max(1.0);
    let gate_count = ((config.max_range_m / spacing_m).floor() as usize).max(1);
    let radial_count = config
        .physical_scan_legs()
        .len()
        .checked_mul(config.azimuth_count.max(1))
        .ok_or_else(|| "Synthetic radial count overflowed".to_string())?;
    let f32_moment_count = 2usize
        + usize::from(config.spectrum_width)
        + if config.dual_pol { 10 } else { 0 }
        + if config.emit_stage_diagnostics { 12 } else { 0 };
    let gate_samples = radial_count
        .checked_mul(gate_count)
        .ok_or_else(|| "Synthetic output sample count overflowed".to_string())?;
    let one_output_bytes = gate_samples
        .checked_mul(f32_moment_count)
        .and_then(|value| value.checked_mul(std::mem::size_of::<f32>()))
        .and_then(|value| {
            value.checked_add(if config.emit_quality_fields {
                gate_samples.saturating_mul(QualityMoment::ALL.len())
            } else {
                0
            })
        })
        .and_then(|value| value.checked_add(radial_count.saturating_mul(96)))
        .ok_or_else(|| "Synthetic output memory estimate overflowed".to_string())?;
    let output_bytes = one_output_bytes
        .checked_mul(emitted_frame_count)
        .ok_or_else(|| "Retained loop-output memory estimate overflowed".to_string())?;
    let terrain_horizon_bytes = config
        .azimuth_count
        .max(1)
        .checked_mul(gate_count)
        .and_then(|value| value.checked_mul(std::mem::size_of::<f32>()))
        .ok_or_else(|| "Terrain-horizon memory estimate overflowed".to_string())?;
    // `CutMomentRow` owns thirteen canonical f32 moment arrays plus three
    // compact quality arrays until a cut is assembled, regardless of final VCP
    // availability. Opt-in diagnostics add twelve more f32 stage arrays.
    let cut_row_bytes_per_gate = (13 + if config.emit_stage_diagnostics { 12 } else { 0 })
        * std::mem::size_of::<f32>()
        + QualityMoment::ALL.len();
    let cut_row_scratch_bytes = config
        .azimuth_count
        .max(1)
        .checked_mul(gate_count)
        .and_then(|value| value.checked_mul(cut_row_bytes_per_gate))
        .ok_or_else(|| "Cut-row scratch memory estimate overflowed".to_string())?;
    // Concurrent WRF reads temporarily retain f64 decoder buffers while the
    // five f32 model planes are narrowed. Six planes is a conservative bound
    // for winds that arrive as two components plus height/REF/W.
    let read_conversion_scratch_bytes = cells_per_scene
        .checked_mul(6)
        .and_then(|value| value.checked_mul(std::mem::size_of::<f64>()))
        .ok_or_else(|| "WRF read-scratch memory estimate overflowed".to_string())?;
    let shared_static_bytes = terrain_horizon_bytes
        .checked_add(cut_row_scratch_bytes)
        .and_then(|value| value.checked_add(read_conversion_scratch_bytes))
        .ok_or_else(|| "Temporal shared-memory estimate overflowed".to_string())?;

    Ok(TemporalMemoryEstimate {
        cells_per_scene,
        dense_fields_per_scene: 5,
        bytes_per_dense_value: std::mem::size_of::<f32>(),
        compact_bytes_per_scene,
        shared_static_bytes,
        output_bytes,
    })
}

fn temporal_required_peak_bytes(
    estimate: TemporalMemoryEstimate,
    requires_two_scenes: bool,
) -> Option<usize> {
    let scene_count = if requires_two_scenes { 2 } else { 1 };
    estimate
        .scene_bytes()?
        .checked_mul(scene_count)?
        .checked_add(estimate.shared_static_bytes)?
        .checked_add(estimate.output_bytes)
}

fn ensure_temporal_runtime_cache_budget(
    cache: &TwoSceneCache<(String, usize), Arc<WrfRadarFields>>,
    estimate: TemporalMemoryEstimate,
    budget_bytes: usize,
) -> Result<(), String> {
    let cached_scene_count = cache.len();
    let has_property_scattering = cache
        .values()
        .any(|fields| fields.property_scattering.is_some() || fields.raw_property_scene.is_some());
    let required_bytes = temporal_runtime_reserved_bytes(cache, estimate)?
        .checked_add(if has_property_scattering {
            app_ui::wrf_tmatrix_assets::embedded_lut_memory_bytes()
        } else {
            0
        })
        .ok_or_else(|| "Temporal runtime memory estimate overflowed".to_string())?;
    if required_bytes > budget_bytes {
        return Err(format!(
            "Temporal sparse T-matrix cache needs {:.2} GiB with {cached_scene_count} retained scene(s), above the configured {:.2} GiB budget",
            required_bytes as f64 / 1024.0_f64.powi(3),
            budget_bytes as f64 / 1024.0_f64.powi(3),
        ));
    }
    Ok(())
}

fn temporal_runtime_reserved_bytes(
    cache: &TwoSceneCache<(String, usize), Arc<WrfRadarFields>>,
    estimate: TemporalMemoryEstimate,
) -> Result<usize, String> {
    let shared_and_output = estimate
        .shared_static_bytes
        .checked_add(estimate.output_bytes)
        .ok_or_else(|| "Temporal runtime retained-memory estimate overflowed".to_string())?;
    cache
        .values()
        .try_fold(shared_and_output, |total, fields| {
            total.checked_add(fields.retained_bytes_estimate())
        })
        .ok_or_else(|| "Temporal runtime retained-memory estimate overflowed".to_string())
}

fn cached_temporal_fields(
    cache: &mut TwoSceneCache<(String, usize), Arc<WrfRadarFields>>,
    scene: &WrfScene,
    config: &SyntheticRadarConfig,
    progress: &dyn Fn(&str),
    estimate: TemporalMemoryEstimate,
    budget_bytes: usize,
) -> Result<Arc<WrfRadarFields>, String> {
    let key = (scene.source_identity.0.clone(), scene.time_index);
    if let Some(fields) = cache.get(&key) {
        return Ok(Arc::clone(fields));
    }
    progress(&format!(
        "reading WRF atmosphere at {}â€¦",
        scene
            .time
            .valid_time()
            .map(DateTime::<Utc>::to_rfc3339)
            .unwrap_or_else(|| "unknown time".to_string())
    ));
    let file = WrfFile::open(&scene.path).map_err(|error| {
        format!(
            "Open inventoried WRF {}: {error}",
            display_name(&scene.path)
        )
    })?;
    let reserved_memory_bytes = temporal_runtime_reserved_bytes(cache, estimate)?;
    let fields = Arc::new(read_wrf_radar_fields_for_config_reporting(
        &file,
        &scene.source_identity,
        scene.time_index,
        config,
        progress,
        reserved_memory_bytes,
    )?);
    let evicted = cache.insert(key.clone(), Arc::clone(&fields));
    if let Err(error) = ensure_temporal_runtime_cache_budget(cache, estimate, budget_bytes) {
        let _ = cache.remove(&key);
        if let Some((evicted_key, evicted_fields)) = evicted {
            cache.insert(evicted_key, evicted_fields);
        }
        return Err(error);
    }
    Ok(fields)
}

fn validate_temporal_field_pair(
    anchor: &WrfRadarFields,
    neighbor: &WrfRadarFields,
) -> Result<(), String> {
    if (anchor.nx, anchor.ny, anchor.nz) != (neighbor.nx, neighbor.ny, neighbor.nz) {
        return Err(format!(
            "Adjacent WRF field shape changed from {}x{}x{} to {}x{}x{}",
            anchor.nx, anchor.ny, anchor.nz, neighbor.nx, neighbor.ny, neighbor.nz
        ));
    }
    if anchor.ref_source != neighbor.ref_source {
        return Err(format!(
            "Adjacent WRF reflectivity source changed from {} to {}",
            anchor.ref_source, neighbor.ref_source
        ));
    }
    match (&anchor.polarimetric, &neighbor.polarimetric) {
        (None, None) => {}
        (Some(left), Some(right))
            if left.profile == right.profile && left.present_fields == right.present_fields => {}
        (Some(left), Some(right)) => {
            return Err(format!(
                "Adjacent WRF microphysics inventory changed from {} [{}] to {} [{}]",
                left.profile.name,
                left.present_fields.join(","),
                right.profile.name,
                right.present_fields.join(",")
            ));
        }
        _ => {
            return Err(
                "Adjacent WRF scene has incompatible polarimetric field availability".to_string(),
            );
        }
    }
    match (&anchor.property_scattering, &neighbor.property_scattering) {
        (None, None) => {}
        (Some(left), Some(right))
            if left.required_field_signature() == right.required_field_signature()
                && left.microphysics_scheme_id() == right.microphysics_scheme_id()
                && property_scattering_contract_matches(left, right) => {}
        (Some(left), Some(right)) => {
            return Err(format!(
                "Adjacent WRF property-scattering contracts differ (mp_physics {} vs {})",
                left.microphysics_scheme_id(),
                right.microphysics_scheme_id(),
            ));
        }
        _ => {
            return Err(
                "Adjacent WRF scene has incompatible P3/ISHMAEL property-scattering availability"
                    .to_string(),
            );
        }
    }
    match (&anchor.raw_property_scene, &neighbor.raw_property_scene) {
        (None, None) => {}
        (Some(left), Some(right))
            if left.microphysics_scheme_id() == right.microphysics_scheme_id()
                && left.required_field_signature() == right.required_field_signature() => {}
        (Some(left), Some(right)) => {
            return Err(format!(
                "Adjacent RawStateLinear property inventories differ (mp_physics {} vs {})",
                left.microphysics_scheme_id(),
                right.microphysics_scheme_id(),
            ));
        }
        _ => {
            return Err(
                "Adjacent WRF scene has incompatible RawStateLinear property coverage".to_string(),
            );
        }
    }
    if anchor.tke_tenths_m2s2.is_some() != neighbor.tke_tenths_m2s2.is_some() {
        return Err("Adjacent WRF scene has incompatible TKE availability".to_string());
    }
    Ok(())
}

fn property_scattering_contract_matches(
    left: &app_ui::wrf_tmatrix_scene::WrfTMatrixScene,
    right: &app_ui::wrf_tmatrix_scene::WrfTMatrixScene,
) -> bool {
    let left_provenance = left.provenance();
    let right_provenance = right.provenance();
    left_provenance.status == right_provenance.status
        && left_provenance.frequency_hz == right_provenance.frequency_hz
        && left_provenance.orientation == right_provenance.orientation
        && left_provenance.frozen_scattering == right_provenance.frozen_scattering
        && left_provenance.fall_moment_policy == right_provenance.fall_moment_policy
        && left_provenance.rain_mode == right_provenance.rain_mode
        && left_provenance.tables == right_provenance.tables
        && left.radar_elevations_deg() == right.radar_elevations_deg()
}

fn append_temporal_provenance(
    volume: &mut RadarVolume,
    plan: &TemporalScenePlan,
    runtime_outcome: &str,
    config: &SyntheticRadarConfig,
    anchor: &WrfRadarFields,
) {
    let neighbor = plan
        .neighbor
        .as_ref()
        .map(|scene| format!("{}:time{}", scene.source_identity.0, scene.time_index))
        .unwrap_or_else(|| "none".to_string());
    let neighbor_time = plan
        .neighbor_time
        .as_ref()
        .map(DateTime::<Utc>::to_rfc3339)
        .unwrap_or_else(|| "none".to_string());
    let policy = match config.missing_neighbor_policy {
        MissingNeighborPolicy::HoldAnchor => "hold_anchor",
        MissingNeighborPolicy::DropFrame => "drop_frame",
        MissingNeighborPolicy::Error => "error",
    };
    let physics_status = anchor
        .dual_pol_status
        .as_deref()
        .unwrap_or("not_requested")
        .replace(';', ",");
    let interpolation_space = match config.atmosphere_time_mode {
        AtmosphereTimeMode::RawStateLinear => {
            "raw_thermodynamics_winds_and_scheme_microphysics_spatial_plus_temporal_preclosure"
        }
        AtmosphereTimeMode::LinearAdjacent if anchor.property_scattering.is_some() => {
            "source_cell_closure_then_additive_scattering_and_wind"
        }
        _ => "derived_linear_z_wind_and_additive_polar",
    };
    let provenance = format!(
        "atmosphere_time_mode={}; temporal_interpolation_space={interpolation_space}; temporal_policy={policy}; temporal_outcome={runtime_outcome}; temporal_neighbor={neighbor}; temporal_neighbor_time={neighbor_time}; temporal_scan_duration_ms={}; temporal_reflectivity_source={}; temporal_polar_available={}; temporal_microphysics_status={physics_status}",
        config.atmosphere_time_mode.provenance_name(),
        plan.scan_duration_ms,
        anchor.ref_source,
        anchor.has_polarimetric_input(),
    );
    match volume.metadata.forward_operator_config.as_mut() {
        Some(config) => {
            config.push_str("; ");
            config.push_str(&provenance);
        }
        None => volume.metadata.forward_operator_config = Some(provenance),
    }
}

fn scene_time_authority(time: &WrfSceneTime) -> &'static str {
    match time {
        WrfSceneTime::InternalTimes { .. } => "internal Times",
        WrfSceneTime::FilenameFallback { .. } => "filename fallback",
        WrfSceneTime::Unavailable { .. } => "unavailable",
    }
}

fn append_scene_provenance(volume: &mut RadarVolume, scene: &WrfScene) {
    let valid_time = scene
        .time
        .valid_time()
        .expect("scene adapter rejects untimed scenes");
    let provenance = format!(
        "scene_source={}; scene_time_index={}; scene_time_authority={}; scene_valid_time={}",
        scene.source_identity.0,
        scene.time_index,
        scene_time_authority(&scene.time),
        valid_time.to_rfc3339(),
    );
    match volume.metadata.forward_operator_config.as_mut() {
        Some(config) => {
            config.push_str("; ");
            config.push_str(&provenance);
        }
        None => volume.metadata.forward_operator_config = Some(provenance),
    }
}

fn scene_build_fingerprint(config: &SyntheticRadarConfig, group: &WrfSceneGroup) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.data_fingerprint().hash(&mut hasher);
    group.key.hash(&mut hasher);
    for scene in &group.scenes {
        scene.source_identity.hash(&mut hasher);
        scene.time_index.hash(&mut hasher);
        scene
            .time
            .valid_time()
            .map(DateTime::<Utc>::timestamp_millis)
            .hash(&mut hasher);
        scene_time_authority(&scene.time).hash(&mut hasher);
    }
    hasher.finish()
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_build_budget_reserves_every_owned_category_before_raw_read() {
        assert_eq!(
            checked_property_tmatrix_build_remainder(1_000, 100, 200, 300, 50, 300).unwrap(),
            350
        );
        assert!(checked_property_tmatrix_build_remainder(650, 100, 200, 300, 50, 1).is_err());
        assert!(checked_property_tmatrix_build_remainder(651, 100, 200, 300, 50, 2).is_err());
        assert!(
            checked_property_tmatrix_build_remainder(usize::MAX, usize::MAX, 1, 0, 0, 1,).is_err()
        );
    }

    #[test]
    fn property_scene_failures_propagate_while_bulk_failures_remain_skippable() {
        let research = SyntheticRadarConfig {
            polarimetric_kernel: PolarimetricKernel::PropertyTMatrixResearchV1,
            ..SyntheticRadarConfig::default()
        };
        let mut notes = Vec::new();
        assert_eq!(
            record_or_propagate_scene_failure(
                &research,
                &mut notes,
                "property scene failed".to_string(),
            )
            .unwrap_err(),
            "property scene failed"
        );
        assert!(notes.is_empty());

        let bulk = SyntheticRadarConfig::default();
        record_or_propagate_scene_failure(&bulk, &mut notes, "bulk scene failed".to_string())
            .unwrap();
        assert_eq!(notes, vec!["bulk scene failed".to_string()]);
    }

    fn fingerprint_scene(
        path: &str,
        source_identity: &str,
        time_index: usize,
    ) -> app_ui::wrf_scene_inventory::WrfScene {
        use app_ui::wrf_scene_inventory::{
            WrfDomainId, WrfGridSignature, WrfRunDomain, WrfRunId, WrfSceneTime, WrfSourceIdentity,
        };
        use chrono::TimeZone;

        app_ui::wrf_scene_inventory::WrfScene {
            path: PathBuf::from(path),
            time_index,
            run_domain: WrfRunDomain {
                run: WrfRunId("2026-07-12_00:00:00".to_owned()),
                domain: WrfDomainId(3),
            },
            grid_signature: WrfGridSignature::from_meters(
                400,
                300,
                Some(50),
                Some(3_000.0),
                Some(3_000.0),
                "lambert",
                0x1234,
            ),
            source_identity: WrfSourceIdentity(source_identity.to_owned()),
            time: WrfSceneTime::InternalTimes {
                valid_time: Utc.with_ymd_and_hms(2026, 7, 12, 1, 0, 0).unwrap(),
                raw: "2026-07-12_01:00:00".to_owned(),
            },
        }
    }

    fn fingerprint_group(
        scene: app_ui::wrf_scene_inventory::WrfScene,
    ) -> app_ui::wrf_scene_inventory::WrfSceneGroup {
        app_ui::wrf_scene_inventory::WrfSceneInventory::from_scenes([scene])
            .groups
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn temporal_preflight_counts_retained_frames_scratch_and_actual_scene_count() {
        let group = fingerprint_group(fingerprint_scene(
            "C:/private/a/wrfout_d03",
            "sha256:source-a",
            0,
        ));
        let config = SyntheticRadarConfig {
            elevations_deg: vec![0.5],
            azimuth_count: 12,
            gate_spacing_m: 1_000.0,
            max_range_m: 10_000.0,
            ..SyntheticRadarConfig::default()
        };
        let one = temporal_memory_estimate(&group, &config, 1).unwrap();
        let three = temporal_memory_estimate(&group, &config, 3).unwrap();
        assert_eq!(three.output_bytes, one.output_bytes * 3);
        assert!(one.shared_static_bytes > 12 * 10 * 13 * std::mem::size_of::<f32>());
        let single_peak = temporal_required_peak_bytes(one, false).unwrap();
        let two_scene_peak = temporal_required_peak_bytes(one, true).unwrap();
        assert_eq!(two_scene_peak - single_peak, one.scene_bytes().unwrap());
    }

    #[test]
    fn temporal_preflight_counts_nine_compact_polar_bytes_per_cell() {
        let group = fingerprint_group(fingerprint_scene(
            "C:/private/a/wrfout_d03",
            "sha256:source-a",
            0,
        ));
        let scalar_config = SyntheticRadarConfig::default();
        let polar_config = SyntheticRadarConfig {
            dual_pol: true,
            ..scalar_config.clone()
        };
        let scalar = temporal_memory_estimate(&group, &scalar_config, 1).unwrap();
        let polar = temporal_memory_estimate(&group, &polar_config, 1).unwrap();
        assert_eq!(
            polar.compact_bytes_per_scene - scalar.compact_bytes_per_scene,
            polar.cells_per_scene * 9
        );
    }

    #[test]
    fn scene_build_fingerprint_tracks_input_identity_without_private_paths() {
        let config = SyntheticRadarConfig::default();
        let first = fingerprint_group(fingerprint_scene(
            "C:/private/a/wrfout_d03",
            "sha256:source-a",
            0,
        ));
        let moved = fingerprint_group(fingerprint_scene(
            "D:/moved/wrfout_d03",
            "sha256:source-a",
            0,
        ));
        let changed_source = fingerprint_group(fingerprint_scene(
            "C:/private/a/wrfout_d03",
            "sha256:source-b",
            0,
        ));
        let changed_time_index = fingerprint_group(fingerprint_scene(
            "C:/private/a/wrfout_d03",
            "sha256:source-a",
            1,
        ));

        let first_fingerprint = scene_build_fingerprint(&config, &first);
        assert_eq!(
            first_fingerprint,
            scene_build_fingerprint(&config, &moved),
            "absolute paths are deliberately outside refresh/cache identity"
        );
        assert_ne!(
            first_fingerprint,
            scene_build_fingerprint(&config, &changed_source)
        );
        assert_ne!(
            first_fingerprint,
            scene_build_fingerprint(&config, &changed_time_index)
        );
    }

    /// REAL-data proof for the multi-frame loop path (project rule: prove on
    /// real data). Gated on `BOWECHO_WRF_RADAR_MULTI_FIXTURE` = a `;`-joined
    /// list of real wrfout paths (e.g. the five Enderlin tornado files).
    /// Deliberately feeds the paths in REVERSED order, then asserts:
    ///  - one volume per file, in strictly ascending scan time;
    ///  - per-file progress streamed ("file 2/5 … building tilt …");
    ///  - the volumes install into the real loop engine as one batch, land in
    ///    time order, and the loop cursor advances through all frames and
    ///    wraps — the exact contract `install_synthetic_radar_volumes` relies
    ///    on. Run in RELEASE (`cargo test -p app_ui --release … -- --nocapture`).
    #[test]
    fn real_multi_file_loop_builds_time_ordered_frames_and_advances() {
        use ui_core::loop_engine::{
            DecodedLoad, DecodedLoadBatch, EngineId, EngineRole, FeedSource, FrameStatus,
            LoadTimings, LoopEngine, SelectionPolicy, StepOutcome, SweepContext,
        };

        let Some(raw) = std::env::var_os("BOWECHO_WRF_RADAR_MULTI_FIXTURE") else {
            return;
        };
        let raw = raw.to_string_lossy().into_owned();
        let mut paths: Vec<PathBuf> = raw
            .split(';')
            .filter(|part| !part.trim().is_empty())
            .map(PathBuf::from)
            .collect();
        assert!(
            paths.len() >= 2,
            "need at least two real wrfout paths, got {}",
            paths.len()
        );
        // Hand the worker the WRONG order on purpose: the sort must fix it.
        paths.reverse();
        let expected_frames = paths.len();

        let config = SyntheticRadarConfig::default();
        let (tx, rx) = channel();
        let output = build_synthetic_from_paths(&paths, &config, "multi-file test", &tx)
            .expect("build synthetic volumes from real multi-file fixture");
        drop(tx);

        // One frame per file, strictly ascending in scan time.
        assert_eq!(output.volumes.len(), expected_frames, "one volume per file");
        let times: Vec<DateTime<Utc>> = output
            .volumes
            .iter()
            .map(|volume| volume.volume_time)
            .collect();
        eprintln!("[multi] frame times: {times:?}");
        for pair in times.windows(2) {
            assert!(
                pair[0] < pair[1],
                "scan times must strictly ascend: {pair:?}"
            );
        }
        // Every frame carries real echo (these are tornado-hour files).
        for volume in &output.volumes {
            assert!(
                volume.metadata.decoded_radial_count > 0,
                "frame {} has no radials",
                volume.volume_time
            );
        }

        // Per-file progress streamed in sorted order.
        let progress: Vec<String> = std::iter::from_fn(|| match rx.try_recv() {
            Ok(SyntheticRadarMessage::Progress(message)) => Some(message),
            _ => None,
        })
        .collect();
        let marker = format!("file 2/{expected_frames}");
        assert!(
            progress
                .iter()
                .any(|message| message.contains(&marker) && message.contains("building tilt")),
            "expected a '{marker} … building tilt …' progress line, got: {progress:?}"
        );

        // Feed the whole Vec to the real loop engine exactly like
        // `install_synthetic_radar_volumes` does, then advance the loop.
        let mut engine = LoopEngine::new(
            EngineId(1),
            EngineRole::Primary,
            FeedSource::LocalFiles {
                label: "wrf-synth multi-file test".to_owned(),
            },
        );
        engine.limits.grow_to_fit = true;
        let frames: Vec<DecodedLoad> = output
            .volumes
            .iter()
            .enumerate()
            .map(|(index, volume)| {
                let stamp = volume.volume_time.format("%Y%m%d_%H%M%S").to_string();
                DecodedLoad {
                    path: PathBuf::from(format!(
                        "wrf-synth://{}/{index:04}_{stamp}",
                        volume.site.id
                    )),
                    volume: Arc::clone(volume),
                    timings: LoadTimings::default(),
                    status: FrameStatus::Local,
                    source_label: format!("simulated WRF {stamp}"),
                }
            })
            .collect();
        let outcome = engine.install_batch(
            DecodedLoadBatch {
                frames,
                selected_index: 0,
            },
            &SelectionPolicy::SelectAnchor {
                blank_display_overrides_browsing: true,
            },
            None,
            |_anchor| false,
        );
        assert!(
            outcome.cross_site_clear.is_none(),
            "one shared site id: the cross-site guard must not clear"
        );
        assert_eq!(engine.history.len(), expected_frames, "all frames install");
        let installed: Vec<DateTime<Utc>> = engine
            .history
            .iter()
            .map(|entry| entry.identity.scan_time_utc)
            .collect();
        assert_eq!(installed, times, "history holds the frames in time order");

        // The loop advances through every frame and wraps back to 0.
        engine.cursor.playing = true;
        let mut sequence = Vec::new();
        for _ in 0..expected_frames {
            match engine.advance_loop(&SweepContext::PLAIN) {
                StepOutcome::Stepped { index } => sequence.push(index),
                StepOutcome::Stopped => panic!("a populated loop must never stop"),
            }
        }
        let expected_sequence: Vec<usize> = (1..expected_frames).chain([0]).collect();
        assert_eq!(sequence, expected_sequence, "loop advances then wraps");
        assert!(engine.cursor.playing);
        eprintln!(
            "[multi] {} frames installed in time order; loop stepped {sequence:?}",
            engine.history.len()
        );
    }

    #[test]
    fn parses_wrf_times_with_colon_or_underscore() {
        let expected = "2026-05-19T00:00:00+00:00";
        assert_eq!(
            parse_wrf_time("2026-05-19_00:00:00").unwrap().to_rfc3339(),
            expected
        );
        assert_eq!(
            parse_wrf_time(" 2026-05-19_00:00:00 ")
                .unwrap()
                .to_rfc3339(),
            expected
        );
        assert!(parse_wrf_time("not-a-time").is_none());
    }

    #[test]
    fn radial_velocity_projection_signs_are_physical() {
        // Beam pointing due east (az=90°) at 0° elevation: a pure eastward wind
        // (u>0, v=0) blows AWAY from the radar → positive Vr; a westward wind →
        // negative. Verifies the (sinAz·cosEl, cosAz·cosEl, sinEl) projection.
        let az_rad: f32 = 90f32.to_radians();
        let (sin_az, cos_az) = (az_rad.sin(), az_rad.cos());
        let (u, v, w) = (12.0f32, 0.0, 0.0);
        let vr = u * sin_az * 1.0 + v * cos_az * 1.0 + w * 0.0;
        assert!(
            (vr - 12.0).abs() < 1e-3,
            "east wind due-east beam Vr = {vr}"
        );

        // Straight-up beam (el=90°) sees only w.
        let el_rad: f32 = 90f32.to_radians();
        let vr_up = 0.0 * 0.0 + 0.0 * 0.0 + 3.5 * el_rad.sin();
        assert!((vr_up - 3.5).abs() < 1e-3, "vertical beam Vr = {vr_up}");
    }

    /// A tiny 2×2×2 uniform box model (40 dBZ everywhere, 10 m/s east wind,
    /// levels at ~100 m / ~8 km MSL) centred near (39, -95) — the smallest
    /// grid the whole sampling chain accepts.
    fn uniform_box_fields() -> WrfRadarFields {
        let nx = 2;
        let ny = 2;
        let nz = 2;
        let cells = nx * ny;
        // Grid centred near (39, -95) with ~0.2° spacing.
        let lat = vec![38.9f32, 38.9, 39.1, 39.1];
        let lon = vec![-95.1f32, -94.9, -95.1, -94.9];
        let height_msl = {
            let mut h = vec![0.0f32; nz * cells];
            for c in 0..cells {
                h[c] = 100.0; // level 0 ~100 m MSL
                h[cells + c] = 8000.0; // level 1 ~8 km MSL
            }
            h
        };
        let dbz = vec![40.0f32; nz * cells];
        let u = vec![10.0f32; nz * cells];
        let v = vec![0.0f32; nz * cells];
        let w = vec![0.0f32; nz * cells];
        let terrain_m = vec![0.0f32; cells];
        let lut = InverseLut::build_with_shape_domain_bounded(&lat, &lon, nx, ny).expect("lut");
        WrfRadarFields {
            nx,
            ny,
            nz,
            lat,
            lon,
            height_msl,
            dbz,
            u,
            v,
            w,
            terrain_m,
            property_scattering: None,
            raw_property_scene: None,
            polarimetric: None,
            dual_pol_status: None,
            tke_tenths_m2s2: None,
            ref_source: "test",
            dx_m: None,
            lut,
        }
    }

    #[test]
    fn temporal_column_sampling_blends_linear_z_and_wind_before_gate_physics() {
        let mut anchor = uniform_box_fields();
        anchor.dbz.fill(0.0);
        anchor.u.fill(10.0);
        let mut neighbor = uniform_box_fields();
        neighbor.dbz.fill(20.0);
        neighbor.u.fill(30.0);

        let midpoint = sample_column_temporal(
            &anchor,
            Some(&neighbor),
            0.5,
            anchor.cells(),
            39.0,
            -95.0,
            1_000.0,
            0.5,
            AtmosphereTimeMode::LinearAdjacent,
            ReflectivitySampling::LinearZ,
        )
        .expect("midpoint query")
        .expect("midpoint model column");
        assert!((z_to_dbz(midpoint.z_linear) - 17.032913).abs() < 1.0e-5);
        assert!((midpoint.u - 20.0).abs() < 1.0e-6);

        let exact_anchor = sample_column_temporal(
            &anchor,
            None,
            0.0,
            anchor.cells(),
            39.0,
            -95.0,
            1_000.0,
            0.5,
            AtmosphereTimeMode::LinearAdjacent,
            ReflectivitySampling::LinearZ,
        )
        .expect("anchor query")
        .expect("exact anchor column");
        assert_eq!(z_to_dbz(exact_anchor.z_linear), 0.0);
        assert_eq!(exact_anchor.u, 10.0);
    }

    fn attach_uniform_polar(
        fields: &mut WrfRadarFields,
        sample: crate::wrf_radar_physics::IntrinsicPolarSample,
    ) {
        let profile = crate::wrf_radar_physics::detect_scheme(
            Some(10),
            ["QRAIN", "QNRAIN", "QSNOW", "QNSNOW"],
        );
        let mut compact = CompactPolarFields::new(
            fields.dbz.len(),
            profile,
            vec!["QRAIN".to_string(), "QNRAIN".to_string()],
        );
        for index in 0..fields.dbz.len() {
            compact.store(index, sample);
        }
        fields.polarimetric = Some(compact);
    }

    #[test]
    fn compact_polar_fields_preserve_signed_kdp() {
        let profile = crate::wrf_radar_physics::detect_scheme(Some(10), ["QRAIN", "QNRAIN"]);
        let mut compact = CompactPolarFields::new(2, profile, vec!["QRAIN".to_string()]);
        let base = crate::wrf_radar_physics::IntrinsicPolarSample {
            zh: 10.0,
            zv: 10.0,
            covariance_magnitude: 10.0,
            rho_hv: 1.0,
            ..crate::wrf_radar_physics::IntrinsicPolarSample::default()
        };
        compact.store(
            0,
            crate::wrf_radar_physics::IntrinsicPolarSample {
                kdp_deg_km: -12.3,
                ..base
            },
        );
        compact.store(
            1,
            crate::wrf_radar_physics::IntrinsicPolarSample {
                kdp_deg_km: 12.3,
                ..base
            },
        );

        assert_eq!(compact.kdp, [-123, 123]);
        assert!((compact.contribution_at(0, 10.0).kdp_deg_km + 12.3).abs() < 1.0e-6);
        assert!((compact.contribution_at(1, 10.0).kdp_deg_km - 12.3).abs() < 1.0e-6);
    }

    #[test]
    fn compact_polar_precision_audit_counts_zeroing_and_saturation_without_changing_codes() {
        let profile = crate::wrf_radar_physics::detect_scheme(Some(10), ["QRAIN", "QNRAIN"]);
        let mut compact = CompactPolarFields::new(1, profile, vec!["QRAIN".to_string()]);
        let zh = 100.0;
        let zv = 10.0;
        let covariance = 20.0;
        let phase = 45.0f32.to_radians();
        compact.store(
            0,
            crate::wrf_radar_physics::IntrinsicPolarSample {
                zh,
                zv,
                cov_re: covariance * phase.cos(),
                cov_im: covariance * phase.sin(),
                covariance_magnitude: covariance,
                kdp_deg_km: 0.04,
                ah_db_km: 0.5,
                av_db_km: 0.1,
                fall_speed_mps: 30.0,
                fall_speed_variance_m2s2: 20.0f32.powi(2),
                zdr_db: 10.0,
                rho_hv: 0.8,
            },
        );

        assert_eq!(compact.zdr[0], i8::MAX);
        assert_eq!(compact.covariance_phase[0], i8::MAX);
        assert_eq!(compact.kdp[0], 0);
        assert_eq!(compact.ah[0], u8::MAX);
        assert_eq!(compact.adp[0], i8::MAX);
        assert_eq!(compact.fall_speed[0], u8::MAX);
        assert_eq!(compact.fall_speed_std[0], u8::MAX);
        assert!(compact.precision_audit.total_clamps() >= 6);
        assert!(compact.precision_audit.kdp_deg_km.quantized_to_zero >= 1);
        assert!(compact.precision_audit.zdr_db.max_abs_reconstruction_error > 3.0);
        assert!(
            compact
                .precision_audit
                .ah_db_km
                .max_abs_reconstruction_error
                > 0.2
        );
    }

    #[test]
    fn linear_z_interpolation_preserves_received_power() {
        let mut fields = uniform_box_fields();
        // West half 0 dBZ, east half 60 dBZ at both vertical levels. The box
        // centre therefore has equal contributors at the two powers.
        for level in 0..fields.nz {
            let base = level * fields.cells();
            fields.dbz[base] = 0.0;
            fields.dbz[base + 1] = 60.0;
            fields.dbz[base + 2] = 0.0;
            fields.dbz[base + 3] = 60.0;
        }
        let legacy = sample_column(
            &fields,
            fields.cells(),
            39.0,
            -95.0,
            1_000.0,
            0.5,
            ReflectivitySampling::LegacyDbz,
        )
        .expect("legacy query")
        .expect("legacy sample");
        let linear = sample_column(
            &fields,
            fields.cells(),
            39.0,
            -95.0,
            1_000.0,
            0.5,
            ReflectivitySampling::LinearZ,
        )
        .expect("linear-Z query")
        .expect("linear-Z sample");
        assert!((z_to_dbz(legacy.z_linear) - 30.0).abs() < 0.05);
        assert!((z_to_dbz(linear.z_linear) - 56.9897).abs() < 0.05);
    }

    #[test]
    fn quadrature_tiers_leave_a_uniform_scene_invariant() {
        let fields = uniform_box_fields();
        let mut config = SyntheticRadarConfig {
            ref_gate_texture: false,
            vel_gate_texture: false,
            spectrum_width: true,
            spectrum_width_floor_mps: 0.0,
            ..SyntheticRadarConfig::default()
        };
        let mut samples = Vec::new();
        for tier in [
            BeamIntegration::Center,
            BeamIntegration::Balanced,
            BeamIntegration::Reference,
        ] {
            config.beam_integration = tier;
            samples.push(
                sample_gate(
                    &fields,
                    None,
                    0.0,
                    fields.cells(),
                    39.0,
                    -95.0,
                    200.0,
                    90.0,
                    0.5,
                    2_000.0,
                    8,
                    250.0,
                    &config,
                    None,
                )
                .expect("sample uniform gate")
                .expect("uniform gate"),
            );
        }
        for sample in &samples {
            assert!((z_to_dbz(sample.z_linear) - 40.0).abs() < 0.02);
            assert!((sample.velocity_mps - 10.0).abs() < 0.02);
        }
        assert!(samples[0].spectrum_width_mps < 1.0e-4);
        assert!(samples[1].spectrum_width_mps >= 0.0);
        assert!(samples[2].spectrum_width_mps >= 0.0);
    }

    #[test]
    fn gate_quality_reports_full_and_missing_model_support() {
        let fields = uniform_box_fields();
        let config = SyntheticRadarConfig {
            beam_integration: BeamIntegration::Balanced,
            ref_gate_texture: false,
            ..SyntheticRadarConfig::default()
        };
        let full = sample_gate_with_quality(
            &fields,
            None,
            0.0,
            fields.cells(),
            39.0,
            -95.0,
            200.0,
            90.0,
            0.5,
            1_000.0,
            4,
            250.0,
            &config,
            None,
        )
        .unwrap();
        assert_eq!(full.quality.model_coverage_fraction, 1.0);
        assert_eq!(full.quality.terrain_unblocked_fraction, 1.0);
        assert_eq!(full.quality.meteorological_signal_fraction, 1.0);
        assert!(full.physical.is_some());

        let missing = sample_gate_with_quality(
            &fields,
            None,
            0.0,
            fields.cells(),
            39.0,
            -95.0,
            200.0,
            90.0,
            0.5,
            2_000_000.0,
            8_000,
            250.0,
            &config,
            None,
        )
        .unwrap();
        assert_eq!(missing.quality, GateQualityFractions::default());
        assert!(missing.physical.is_none());
    }

    #[test]
    fn synthetic_volume_emits_compact_quality_grids_and_coverage_mask_is_configurable() {
        let fields = uniform_box_fields();
        let base = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5],
            azimuth_count: 36,
            gate_spacing_m: 500.0,
            max_range_m: 20_000.0,
            beam_integration: BeamIntegration::Balanced,
            ref_floor_dbz: -20.0,
            ref_gate_texture: false,
            emit_quality_fields: true,
            minimum_model_coverage_fraction: 0.0,
            ..SyntheticRadarConfig::default()
        };
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let permissive = build_synthetic_volume(&fields, time, &base);
        for quality in QualityMoment::ALL {
            let grid = &permissive.cuts[0].moments[&quality.moment_type()];
            assert!(matches!(&grid.storage, MomentStorage::U8(_)));
            assert_eq!(grid.scale, 255.0);
        }

        let strict = build_synthetic_volume(
            &fields,
            time,
            &SyntheticRadarConfig {
                minimum_model_coverage_fraction: 1.0,
                ..base
            },
        );
        let finite_count = |volume: &RadarVolume| {
            let MomentStorage::F32(values) =
                &volume.cuts[0].moments[&MomentType::Reflectivity].storage
            else {
                panic!("synthetic reflectivity is f32");
            };
            values.iter().filter(|value| value.is_finite()).count()
        };
        assert!(
            finite_count(&strict) < finite_count(&permissive),
            "a strict full-support threshold must mask partially covered edge gates"
        );
        assert_eq!(
            strict.cuts[0].moments[&QualityMoment::ModelCoverage.moment_type()].radial_count(),
            strict.cuts[0].radials.len()
        );
    }

    #[test]
    fn ray_plan_resolves_sampling_time_and_status_before_rendering() {
        let rays = plan_synthetic_rays(1, 2, 4, ScanTiming::TimedVolume, 10.0, 40_000);
        assert_eq!(
            rays.iter()
                .map(|ray| (ray.azimuth_deg, ray.time_offset_ms))
                .collect::<Vec<_>>(),
            vec![
                (0.0, 40_000),
                (90.0, 49_000),
                (180.0, 58_000),
                (270.0, 67_000),
            ]
        );
        assert_eq!(
            rays.first().map(|ray| ray.radial_status),
            Some(radar_core::RadialStatus::StartElevation)
        );
        assert_eq!(
            rays.last().map(|ray| ray.radial_status),
            Some(radar_core::RadialStatus::EndVolume)
        );

        let frozen = plan_synthetic_rays(0, 1, 3, ScanTiming::InstantaneousTruth, 18.0, 99);
        assert!(frozen.iter().all(|ray| ray.time_offset_ms == 0));
    }

    #[test]
    fn timed_volume_stamps_monotonic_nonzero_ray_times() {
        let fields = uniform_box_fields();
        let config = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5, 1.5],
            azimuth_count: 12,
            gate_spacing_m: 500.0,
            max_range_m: 5_000.0,
            scan_timing: ScanTiming::TimedVolume,
            rotation_rate_deg_s: 12.0,
            transition_delay_s: 2.0,
            ref_gate_texture: false,
            ..SyntheticRadarConfig::default()
        };
        let volume = build_synthetic_volume(
            &fields,
            DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            &config,
        );
        let offsets: Vec<i32> = volume
            .cuts
            .iter()
            .flat_map(|cut| cut.radials.iter().map(|radial| radial.time_offset_ms))
            .collect();
        assert_eq!(offsets[0], 0);
        assert!(offsets.windows(2).all(|pair| pair[1] > pair[0]));
        assert!(offsets.last().copied().unwrap_or(0) > 50_000);
        assert_eq!(config.planned_scan_duration_ms(), 59_500);
        assert_eq!(
            config.planned_scan_duration_ms(),
            i64::from(*offsets.last().expect("last timed ray")),
            "temporal planner duration must be the exact latest sampled ray"
        );
    }

    #[test]
    fn dual_pol_path_emits_propagation_and_attenuation_moments() {
        let mut fields = uniform_box_fields();
        let zh = dbz_to_z(40.0);
        let zv = zh / 10.0f32.powf(0.1);
        let covariance = 0.97 * (zh * zv).sqrt();
        attach_uniform_polar(
            &mut fields,
            crate::wrf_radar_physics::IntrinsicPolarSample {
                zh,
                zv,
                cov_re: covariance,
                cov_im: 0.0,
                covariance_magnitude: covariance,
                kdp_deg_km: 1.0,
                ah_db_km: 0.01,
                av_db_km: 0.008,
                fall_speed_mps: 5.0,
                fall_speed_variance_m2s2: 1.0,
                zdr_db: 1.0,
                rho_hv: 0.97,
            },
        );
        let config = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5],
            azimuth_count: 4,
            gate_spacing_m: 1_000.0,
            max_range_m: 5_000.0,
            ref_floor_dbz: -20.0,
            ref_gate_texture: false,
            dual_pol: true,
            propagation: true,
            system_phidp_deg: 7.0,
            ..SyntheticRadarConfig::default()
        };
        let volume = build_synthetic_volume(
            &fields,
            DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            &config,
        );
        let cut = &volume.cuts[0];
        for moment in [
            MomentType::DifferentialReflectivity,
            MomentType::CorrelationCoefficient,
            MomentType::DifferentialPhase,
            MomentType::SpecificDifferentialPhase,
            MomentType::Unknown("AH".to_string()),
            MomentType::Unknown("PIA".to_string()),
            MomentType::Unknown("REFC".to_string()),
            MomentType::Unknown("ADP".to_string()),
            MomentType::Unknown("PIDA".to_string()),
            MomentType::Unknown("ZDRC".to_string()),
        ] {
            assert!(cut.moments.contains_key(&moment), "missing {moment}");
        }
        let phi = cut.moments[&MomentType::DifferentialPhase]
            .scaled_value(0, 2)
            .expect("PhiDP gate");
        let pia = cut.moments[&MomentType::Unknown("PIA".to_string())]
            .scaled_value(0, 2)
            .expect("PIA gate");
        let refc = cut.moments[&MomentType::Unknown("REFC".to_string())]
            .scaled_value(0, 2)
            .expect("REFC gate");
        let observed = cut.moments[&MomentType::Reflectivity]
            .scaled_value(0, 2)
            .expect("REF gate");
        assert!((phi - 11.0).abs() < 0.05, "PhiDP={phi}");
        assert!((pia - 0.04).abs() < 0.005, "PIA={pia}");
        assert!((refc - observed - pia).abs() < 0.005);
        let rho = cut.moments[&MomentType::CorrelationCoefficient]
            .scaled_value(0, 2)
            .expect("rho gate");
        assert!((0.0..=1.0).contains(&rho));
    }

    #[test]
    fn terminal_fall_speed_projects_toward_an_upward_beam() {
        let mut fields = uniform_box_fields();
        let zh = dbz_to_z(40.0);
        attach_uniform_polar(
            &mut fields,
            crate::wrf_radar_physics::IntrinsicPolarSample {
                zh,
                zv: zh,
                cov_re: zh,
                cov_im: 0.0,
                covariance_magnitude: zh,
                kdp_deg_km: 0.0,
                ah_db_km: 0.0,
                av_db_km: 0.0,
                fall_speed_mps: 5.0,
                fall_speed_variance_m2s2: 0.0,
                zdr_db: 0.0,
                rho_hv: 1.0,
            },
        );
        let mut config = SyntheticRadarConfig {
            ref_gate_texture: false,
            ..SyntheticRadarConfig::default()
        };
        let air = sample_gate(
            &fields,
            None,
            0.0,
            fields.cells(),
            39.0,
            -95.0,
            200.0,
            90.0,
            30.0,
            2_000.0,
            8,
            250.0,
            &config,
            None,
        )
        .expect("sample air-motion gate")
        .expect("air-motion gate");
        config.terminal_fall_speed = true;
        let scatterer = sample_gate(
            &fields,
            None,
            0.0,
            fields.cells(),
            39.0,
            -95.0,
            200.0,
            90.0,
            30.0,
            2_000.0,
            8,
            250.0,
            &config,
            None,
        )
        .expect("sample scatterer-motion gate")
        .expect("scatterer-motion gate");
        assert!((scatterer.velocity_mps - (air.velocity_mps - 2.5)).abs() < 0.08);
    }

    #[test]
    fn terrain_horizon_blocks_downstream_low_tilt() {
        let mut fields = uniform_box_fields();
        fields.terrain_m.fill(1_500.0);
        let horizon = TerrainHorizon::build(&fields, 39.0, -95.0, 200.0, 36, 20, 0, 500.0);
        let config = SyntheticRadarConfig {
            terrain_blockage: true,
            ref_gate_texture: false,
            ..SyntheticRadarConfig::default()
        };
        let blocked = sample_gate(
            &fields,
            None,
            0.0,
            fields.cells(),
            39.0,
            -95.0,
            200.0,
            90.0,
            0.5,
            2_000.0,
            4,
            500.0,
            &config,
            Some(&horizon),
        )
        .expect("sample blocked gate");
        assert!(
            blocked.is_none(),
            "a 1.5-km ridge must block the 0.5-degree beam"
        );
        assert!(
            sample_gate(
                &fields,
                None,
                0.0,
                fields.cells(),
                39.0,
                -95.0,
                200.0,
                90.0,
                0.5,
                2_000.0,
                4,
                500.0,
                &config,
                None,
            )
            .expect("sample visible gate")
            .is_some(),
            "same model gate is visible without blockage"
        );
    }

    /// A tiny synthetic 2×2×2 model verifies the whole sampling chain end to
    /// end without a wrfout: uniform 40 dBZ column, uniform 10 m/s east wind,
    /// radar at the box centre. Every in-domain, in-height gate must read
    /// 40 dBZ, and a due-east 0°-tilt gate near the ground must read ~+10 Vr.
    #[test]
    fn synthetic_box_model_samples_ref_and_velocity() {
        let fields = uniform_box_fields();

        let config = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5],
            azimuth_count: 360,
            gate_spacing_m: 250.0,
            max_range_m: 10_000.0,
            // Smooth field: this test asserts exact sampled values, so both
            // textures are off (the default enables reflectivity texture).
            ref_gate_texture: false,
            vel_gate_texture: false,
            ..SyntheticRadarConfig::default()
        };
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let volume = build_synthetic_volume(&fields, time, &config);

        assert_eq!(volume.cuts.len(), 1);
        let cut = &volume.cuts[0];
        let ref_grid = &cut.moments[&MomentType::Reflectivity];
        let vel_grid = &cut.moments[&MomentType::Velocity];

        // Interior gates read the uniform 40 dBZ.
        let mut finite_ref = 0;
        if let MomentStorage::F32(values) = &ref_grid.storage {
            for value in values {
                if value.is_finite() {
                    finite_ref += 1;
                    assert!((value - 40.0).abs() < 0.5, "ref {value}");
                }
            }
        }
        assert!(
            finite_ref > 100,
            "expected many finite REF gates, got {finite_ref}"
        );

        // Radial nearest az=90° (due east): near-ground gates blow away from
        // the radar at ~+10 m/s.
        let east_radial = (90 * 360 / 360) as usize; // az index for 90°
        let vel = vel_grid
            .scaled_value(east_radial, 4)
            .expect("east radial near gate");
        assert!((vel - 10.0).abs() < 1.5, "due-east Vr = {vel}");

        // West radial (az=270°) is the mirror image: toward the radar.
        let west_vel = vel_grid.scaled_value(270, 4).expect("west radial");
        assert!((west_vel + 10.0).abs() < 1.5, "due-west Vr = {west_vel}");
    }

    /// Round-trip a REAL synthetic volume (the box fixture, both tilts)
    /// through the CfRadial exporter and OUR OWN CfRadial decoder: format
    /// sniff, site, gate geometry, per-radial nyquist, and every REF/VEL
    /// bit (NaN patterns included) must survive the file.
    #[test]
    fn synthetic_volume_cfradial_round_trip_bit_exact() {
        let fields = uniform_box_fields();
        let config = box_model_config();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let volume = build_synthetic_volume(&fields, time, &config);

        let dir =
            std::env::temp_dir().join(format!("bowecho-wrf-radar-cfradial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(crate::radar_export::export_file_name(&volume));
        crate::radar_export::export_volume_cfradial(&volume, &path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            nexrad_io::sniff_supported_volume_format(&bytes),
            nexrad_io::SupportedVolumeFormat::CfRadial
        );
        let decoded = nexrad_io::decode_supported_volume_bytes(&bytes).unwrap();

        assert_eq!(decoded.volume_time, volume.volume_time);
        assert_eq!(decoded.site.id, volume.site.id);
        assert_eq!(decoded.site.latitude_deg, volume.site.latitude_deg);
        assert_eq!(decoded.site.longitude_deg, volume.site.longitude_deg);
        assert_eq!(decoded.site.elevation_m, volume.site.elevation_m);

        assert_eq!(decoded.cuts.len(), volume.cuts.len());
        for (decoded_cut, source_cut) in decoded.cuts.iter().zip(&volume.cuts) {
            assert_eq!(decoded_cut.elevation_deg, source_cut.elevation_deg);
            assert_eq!(decoded_cut.radials.len(), source_cut.radials.len());
            for (decoded_radial, source_radial) in
                decoded_cut.radials.iter().zip(&source_cut.radials)
            {
                assert_eq!(decoded_radial.azimuth_deg, source_radial.azimuth_deg);
                assert_eq!(decoded_radial.time_offset_ms, source_radial.time_offset_ms);
                assert_eq!(decoded_radial.gate_range, source_radial.gate_range);
                assert_eq!(
                    decoded_radial.nyquist_velocity_mps,
                    source_radial.nyquist_velocity_mps
                );
            }
        }

        assert_eq!(moment_bits(&decoded), moment_bits(&volume));
    }

    /// All F32 moment values of a volume as raw bits (NaN patterns
    /// included), for exact equality comparisons.
    fn moment_bits(volume: &RadarVolume) -> Vec<u32> {
        let mut bits = Vec::new();
        for cut in &volume.cuts {
            for moment in [MomentType::Reflectivity, MomentType::Velocity] {
                let MomentStorage::F32(values) = &cut.moments[&moment].storage else {
                    panic!("synthetic moments must be F32");
                };
                bits.extend(values.iter().map(|value| value.to_bits()));
            }
        }
        bits
    }

    /// The clean (both-textures-off) box fixture: the smooth trilinear
    /// baseline the texture tests perturb from. Explicit off because the
    /// shipped default now enables reflectivity texture.
    fn box_model_config() -> SyntheticRadarConfig {
        SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5, 3.1],
            azimuth_count: 360,
            gate_spacing_m: 250.0,
            max_range_m: 10_000.0,
            ref_gate_texture: false,
            vel_gate_texture: false,
            ..SyntheticRadarConfig::default()
        }
    }

    /// The reflectivity-operator choice resolves to the right source label for
    /// each branch. `read_reflectivity` needs a real WRF file (WrfFile only
    /// opens from a path), so the operator→source decision it makes is factored
    /// into `planned_ref_source` and asserted here for both operators, with and
    /// without a file-carried REFL_10CM.
    #[test]
    fn reflectivity_operator_selects_the_expected_source() {
        use ReflectivityOperator::{ClassicStoelinga, ModelNative};

        // Model native: prefers REFL_10CM when present, falls back to CALCDBZ.
        assert_eq!(planned_ref_source(ModelNative, true), REFL_10CM_SOURCE);
        assert_eq!(planned_ref_source(ModelNative, false), CALCDBZ_SOURCE);

        // Classic Stoelinga forces CALCDBZ EVEN when REFL_10CM is present, and
        // stamps a distinct label so a run documents the deliberate choice
        // rather than a fallback.
        assert_eq!(planned_ref_source(ClassicStoelinga, true), STOELINGA_SOURCE);
        assert_eq!(
            planned_ref_source(ClassicStoelinga, false),
            STOELINGA_SOURCE
        );

        // Only the model-native operator ever reads REFL_10CM.
        assert!(ModelNative.prefers_refl_10cm());
        assert!(!ClassicStoelinga.prefers_refl_10cm());

        // Default operator is model native (the historical behavior).
        assert_eq!(ReflectivityOperator::default(), ModelNative);
        assert_eq!(
            SyntheticRadarConfig::default().reflectivity_operator,
            ModelNative
        );

        // The three labels are distinct so the import note is unambiguous.
        assert_ne!(REFL_10CM_SOURCE, CALCDBZ_SOURCE);
        assert_ne!(CALCDBZ_SOURCE, STOELINGA_SOURCE);
    }

    /// The optional 0.1° low tilt is prepended below the standard ladder when
    /// opted in, and off by default it is bit-identical to the classic ladder.
    #[test]
    fn low_tilt_option_prepends_the_community_lowest_tilt() {
        let standard = elevation_ladder(false);
        assert_eq!(
            standard, DEFAULT_ELEVATIONS_DEG,
            "off = the classic ladder, unchanged"
        );

        let with_low = elevation_ladder(true);
        assert_eq!(
            with_low.len(),
            DEFAULT_ELEVATIONS_DEG.len() + 1,
            "one extra tilt"
        );
        assert_eq!(with_low[0], LOW_TILT_DEG, "0.1° comes first");
        assert!(
            with_low[0] < DEFAULT_ELEVATIONS_DEG[0],
            "the extra tilt is below the standard lowest tilt"
        );
        assert_eq!(
            &with_low[1..],
            DEFAULT_ELEVATIONS_DEG,
            "the standard ladder follows unchanged"
        );
    }

    #[test]
    fn physical_scan_plan_keeps_every_build_24_source_row() {
        for strategy in SyntheticScanStrategy::BUILD_24 {
            let config = SyntheticRadarConfig {
                scan_strategy: strategy,
                ..SyntheticRadarConfig::default()
            };
            let definition = strategy.definition().unwrap();
            let legs = config.physical_scan_legs();
            assert_eq!(legs.len(), definition.rows.len(), "{strategy:?}");
            for (index, (leg, row)) in legs.iter().zip(definition.rows).enumerate() {
                assert_eq!(leg.source_row_index, Some(index));
                assert_eq!(leg.source_row, Some(row));
                assert_eq!(leg.elevation_deg, f64::from(row.elevation_deg));
                assert_eq!(
                    leg.azimuth_rate_deg_per_second,
                    row.azimuth_rate_deg_per_second
                );
                assert_eq!(leg.source_period_seconds, row.source_period_seconds);
                assert_eq!(leg.transition_after_seconds, 0.0);
                assert_eq!(leg.moments, row.moments);
                assert_eq!(leg.waveform, Some(row.waveform));
            }
        }
    }

    #[test]
    fn physical_scan_plan_preserves_vcp_112_duplicate_split_and_mpda_cuts() {
        let config = SyntheticRadarConfig {
            scan_strategy: SyntheticScanStrategy::Build24Vcp112,
            ..SyntheticRadarConfig::default()
        };
        let legs = config.physical_scan_legs();
        assert_eq!(legs.len(), 20);
        assert_eq!(
            legs.iter()
                .take(3)
                .map(|leg| leg.elevation_deg)
                .collect::<Vec<_>>(),
            vec![0.5, 0.5, 0.5]
        );
        assert_eq!(legs[0].waveform, Some(Waveform::Sz2ContiguousSurveillance));
        assert_eq!(legs[1].waveform, Some(Waveform::Sz2ContiguousDoppler));
        assert_eq!(legs[2].waveform, Some(Waveform::Sz2ContiguousDoppler));
        assert_eq!(legs[0].moments, MomentCoverage::SURVEILLANCE);
        assert_eq!(legs[1].moments, MomentCoverage::DOPPLER);
        assert_eq!(legs[2].moments, MomentCoverage::DOPPLER);
    }

    #[test]
    fn custom_scan_plan_remains_the_legacy_all_moment_ladder() {
        let config = SyntheticRadarConfig::default();
        assert_eq!(config.scan_strategy, SyntheticScanStrategy::CustomLegacy);
        let legs = config.physical_scan_legs();
        assert_eq!(legs.len(), DEFAULT_ELEVATIONS_DEG.len());
        assert_eq!(
            legs.iter().map(|leg| leg.elevation_deg).collect::<Vec<_>>(),
            DEFAULT_ELEVATIONS_DEG
        );
        assert!(legs.iter().all(|leg| {
            leg.moments == MomentCoverage::ALL && leg.waveform.is_none() && leg.source_row.is_none()
        }));
    }

    #[test]
    fn named_vcp_identity_and_versioned_rows_move_the_fingerprint() {
        let custom = SyntheticRadarConfig::default();
        let vcp12 = SyntheticRadarConfig {
            scan_strategy: SyntheticScanStrategy::Build24Vcp12,
            // VCP 12 has the same unique elevation ladder as the legacy
            // default; identity/physical rows must still distinguish it.
            elevations_deg: DEFAULT_ELEVATIONS_DEG.to_vec(),
            ..custom.clone()
        };
        let vcp212 = SyntheticRadarConfig {
            scan_strategy: SyntheticScanStrategy::Build24Vcp212,
            elevations_deg: DEFAULT_ELEVATIONS_DEG.to_vec(),
            ..custom.clone()
        };
        assert_ne!(custom.data_fingerprint(), vcp12.data_fingerprint());
        assert_ne!(vcp12.data_fingerprint(), vcp212.data_fingerprint());
        assert_eq!(vcp12.data_fingerprint(), vcp12.clone().data_fingerprint());
    }

    /// The config fingerprint is the loop-engine dedupe discriminator: an
    /// UNCHANGED config must fingerprint identically (so a re-import reuses the
    /// stored volume) and EVERY data-affecting field must move it (so a changed
    /// setting rebuilds and replaces). `site_name` is presentation-only and must
    /// NOT move it.
    #[test]
    fn property_tmatrix_static_contract_is_exact_and_fail_closed() {
        assert!(
            SyntheticRadarConfig::default()
                .validate_science_contract()
                .is_ok()
        );

        let supported = SyntheticRadarConfig {
            dual_pol: true,
            polarimetric_kernel: PolarimetricKernel::PropertyTMatrixResearchV1,
            radar_frequency_mhz: PROPERTY_TMATRIX_RESEARCH_FREQUENCY_MHZ,
            reflectivity_sampling: ReflectivitySampling::LinearZ,
            beam_integration: BeamIntegration::Balanced,
            elevations_deg: elevation_ladder(true),
            ..SyntheticRadarConfig::default()
        };
        let supported_result = supported.validate_science_contract();
        assert!(
            supported_result.is_ok(),
            "the optional 0.1-degree cut and default one-degree beam fit the declared view axis: {supported_result:?}"
        );
        let raw_state = SyntheticRadarConfig {
            atmosphere_time_mode: AtmosphereTimeMode::RawStateLinear,
            scan_timing: ScanTiming::TimedVolume,
            ..supported.clone()
        };
        assert!(raw_state.validate_science_contract().is_ok());
        let raw_with_bulk = SyntheticRadarConfig {
            atmosphere_time_mode: AtmosphereTimeMode::RawStateLinear,
            scan_timing: ScanTiming::TimedVolume,
            ..SyntheticRadarConfig::default()
        };
        assert!(
            raw_with_bulk
                .validate_science_contract()
                .unwrap_err()
                .contains("only with the P3/ISHMAEL property T-matrix")
        );

        let wrong_frequency = SyntheticRadarConfig {
            radar_frequency_mhz: 2_900,
            ..supported.clone()
        };
        assert!(
            wrong_frequency
                .validate_science_contract()
                .unwrap_err()
                .contains("exactly 2800 MHz")
        );
        let legacy_log_sampling = SyntheticRadarConfig {
            reflectivity_sampling: ReflectivitySampling::LegacyDbz,
            ..supported.clone()
        };
        assert!(
            legacy_log_sampling
                .validate_science_contract()
                .unwrap_err()
                .contains("linear-Z")
        );
        let no_dual_pol = SyntheticRadarConfig {
            dual_pol: false,
            ..supported.clone()
        };
        assert!(
            no_dual_pol
                .validate_science_contract()
                .unwrap_err()
                .contains("requires S-band dual polarization")
        );
        let too_wide_for_low_cut = SyntheticRadarConfig {
            beam_width_deg: 1.5,
            ..supported
        };
        assert!(
            too_wide_for_low_cut
                .validate_science_contract()
                .unwrap_err()
                .contains("outside the exact")
        );
    }

    #[test]
    fn data_fingerprint_changes_with_every_data_field() {
        use ReflectivityOperator::ClassicStoelinga;

        let base = SyntheticRadarConfig {
            site_id: "WRF".to_string(),
            site_name: Some("Simulated WRF radar".to_string()),
            site_lat_deg: Some(35.0),
            site_lon_deg: Some(-97.0),
            antenna_msl_m: Some(400.0),
            elevations_deg: vec![0.5, 1.5, 2.4],
            azimuth_count: 720,
            gate_spacing_m: 250.0,
            max_range_m: 230_000.0,
            match_gate_to_grid: false,
            ref_floor_dbz: 0.0,
            nyquist_mps: 25.0,
            fold_velocity: false,
            reflectivity_operator: ReflectivityOperator::ModelNative,
            ref_gate_texture: true,
            vel_gate_texture: false,
            clutter_intensity: 0.0,
            ..SyntheticRadarConfig::default()
        };
        let fingerprint = base.data_fingerprint();

        // Stable across a clone (deterministic, no per-run seed).
        assert_eq!(fingerprint, base.clone().data_fingerprint());

        // Presentation-only fields must NOT change the fingerprint.
        let renamed = SyntheticRadarConfig {
            site_name: Some("A different label".to_string()),
            ..base.clone()
        };
        assert_eq!(
            renamed.data_fingerprint(),
            fingerprint,
            "site_name is a label — it must not move the data fingerprint"
        );

        let larger_memory_budget = SyntheticRadarConfig {
            temporal_memory_budget_mib: base.temporal_memory_budget_mib * 2,
            ..base.clone()
        };
        assert_eq!(
            larger_memory_budget.data_fingerprint(),
            fingerprint,
            "memory budget gates execution but does not alter a successful sample"
        );

        // Every data-affecting field must move the fingerprint. `differs`
        // clones the base, applies one edit, and reports whether it changed.
        let differs = |mutate: &dyn Fn(&mut SyntheticRadarConfig)| {
            let mut config = base.clone();
            mutate(&mut config);
            config.data_fingerprint() != fingerprint
        };
        assert!(differs(&|c| c.site_id = "KTLX".to_string()), "site_id");
        assert!(differs(&|c| c.site_lat_deg = Some(36.0)), "site_lat_deg");
        assert!(differs(&|c| c.site_lat_deg = None), "site_lat None");
        assert!(differs(&|c| c.site_lon_deg = Some(-96.0)), "site_lon_deg");
        assert!(differs(&|c| c.antenna_msl_m = Some(401.0)), "antenna_msl");
        assert!(differs(&|c| c.antenna_msl_m = None), "antenna_msl None");
        assert!(
            differs(&|c| c.elevations_deg = vec![0.1, 0.5]),
            "elevations"
        );
        assert!(differs(&|c| c.azimuth_count = 360), "azimuth_count");
        assert!(differs(&|c| c.gate_spacing_m = 500.0), "gate_spacing_m");
        assert!(differs(&|c| c.max_range_m = 460_000.0), "max_range_m");
        assert!(
            differs(&|c| c.match_gate_to_grid = true),
            "match_gate_to_grid — toggling grid-matching resizes gates and must rebuild"
        );
        assert!(differs(&|c| c.ref_floor_dbz = 5.0), "ref_floor_dbz");
        assert!(differs(&|c| c.nyquist_mps = 64.0), "nyquist_mps");
        assert!(
            differs(&|c| c.fold_velocity = true),
            "fold_velocity — toggling folding re-aliases VEL and re-stamps Nyquist"
        );
        assert!(differs(&|c| c.ref_gate_texture = false), "ref_gate_texture");
        assert!(differs(&|c| c.vel_gate_texture = true), "vel_gate_texture");
        assert!(
            differs(&|c| c.clutter_intensity = 0.5),
            "clutter_intensity — the slider must rebuild the volume"
        );
        assert!(
            differs(&|c| c.reflectivity_operator = ClassicStoelinga),
            "operator"
        );
        assert!(
            differs(&|c| c.simulation_mode = SimulationMode::Truth),
            "simulation_mode"
        );
        assert!(
            differs(&|c| c.reflectivity_sampling = ReflectivitySampling::LegacyDbz),
            "reflectivity_sampling"
        );
        assert!(
            differs(&|c| c.beam_integration = BeamIntegration::Balanced),
            "beam_integration"
        );
        assert!(differs(&|c| c.beam_width_deg = 1.2), "beam_width_deg");
        assert!(differs(&|c| c.pulse_width_us = 2.0), "pulse_width_us");
        assert!(
            differs(&|c| c.radar_frequency_mhz = 5_600),
            "radar_frequency_mhz"
        );
        assert!(
            differs(&|c| c.terminal_fall_speed = true),
            "terminal_fall_speed"
        );
        assert!(differs(&|c| c.terrain_blockage = true), "terrain_blockage");
        assert!(differs(&|c| c.spectrum_width = true), "spectrum_width");
        assert!(
            differs(&|c| c.spectrum_width_floor_mps = 0.8),
            "spectrum_width_floor_mps"
        );
        assert!(differs(&|c| c.dual_pol = true), "dual_pol");
        assert!(
            differs(&|c| c.polarimetric_kernel = PolarimetricKernel::PropertyTMatrixResearchV1),
            "polarimetric_kernel"
        );
        assert!(differs(&|c| c.propagation = true), "propagation");
        assert!(differs(&|c| c.system_phidp_deg = 11.0), "system_phidp_deg");
        assert!(differs(&|c| c.zdr_bias_db = 0.3), "zdr_bias_db");
        assert!(
            differs(&|c| c.scan_timing = ScanTiming::TimedVolume),
            "scan_timing"
        );
        assert!(
            differs(&|c| c.atmosphere_time_mode = AtmosphereTimeMode::LinearAdjacent),
            "atmosphere_time_mode"
        );
        assert!(
            differs(&|c| c.missing_neighbor_policy = MissingNeighborPolicy::DropFrame),
            "missing_neighbor_policy"
        );
        assert!(
            differs(&|c| c.rotation_rate_deg_s = 12.0),
            "rotation_rate_deg_s"
        );
        assert!(
            differs(&|c| c.transition_delay_s = 5.0),
            "transition_delay_s"
        );
        assert!(differs(&|c| c.prf_hz = 1_200.0), "prf_hz");
        assert!(
            differs(&|c| c.coupled_single_prf_estimator = true),
            "coupled_single_prf_estimator"
        );
        assert!(
            differs(&|c| c.estimator_dwell_ms = 75.0),
            "estimator_dwell_ms"
        );
        assert!(
            differs(&|c| c.estimator_pulse_count = Some(64)),
            "estimator_pulse_count"
        );
        assert!(
            differs(&|c| c.estimator_independent_sample_fraction = 0.75),
            "estimator_independent_sample_fraction"
        );
        assert!(
            differs(&|c| c.estimator_minimum_snr_db = 3.0),
            "estimator_minimum_snr_db"
        );
        assert!(
            differs(&|c| c.emit_stage_diagnostics = true),
            "emit_stage_diagnostics"
        );
        assert!(differs(&|c| c.instrument_noise = true), "instrument_noise");
        assert!(
            differs(&|c| c.sensitivity_dbz_at_1km = -38.0),
            "sensitivity_dbz_at_1km"
        );
        assert!(
            differs(&|c| c.emit_quality_fields = false),
            "emit_quality_fields"
        );
        assert!(
            differs(&|c| c.minimum_model_coverage_fraction = 0.75),
            "minimum_model_coverage_fraction"
        );
    }

    #[test]
    fn coupled_custom_instrument_resolves_one_physical_timing_contract() {
        let config = SyntheticRadarConfig {
            coupled_single_prf_estimator: true,
            radar_frequency_mhz: 2_800,
            pulse_width_us: 2.0,
            prf_hz: 1_200.0,
            estimator_dwell_ms: 40.0,
            estimator_pulse_count: None,
            estimator_independent_sample_fraction: 0.5,
            ..SyntheticRadarConfig::default()
        };
        config.validate_science_contract().unwrap();
        let coupled = resolve_coupled_instrument(&config).unwrap().unwrap();
        assert!((coupled.timing.prf_hz - 1_200.0).abs() < f64::EPSILON);
        assert!((coupled.timing.prt_s - 1.0 / 1_200.0).abs() < 1.0e-15);
        assert_eq!(coupled.sampling.transmitted_pulses, 48);
        assert!((coupled.sampling.independent_samples - 24.0).abs() < f64::EPSILON);
        assert_eq!(coupled.balanced_quadrature.len(), 9);
        assert_eq!(coupled.reference_quadrature.len(), 27);
        for points in [&coupled.balanced_quadrature, &coupled.reference_quadrature] {
            let weight_sum = points.iter().map(|point| point.weight).sum::<f64>();
            assert!((weight_sum - 1.0).abs() < 1.0e-12);
            assert!(points.iter().any(|point| point.range_offset_m < 0.0));
            assert!(points.iter().any(|point| point.range_offset_m > 0.0));
            assert!(
                points
                    .iter()
                    .all(|point| point.range_offset_m.abs() <= coupled.range_resolution_m)
            );
        }

        let different_gate_spacing = SyntheticRadarConfig {
            gate_spacing_m: 4_000.0,
            ..config
        };
        let other = resolve_coupled_instrument(&different_gate_spacing)
            .unwrap()
            .unwrap();
        assert_eq!(
            coupled
                .reference_quadrature
                .iter()
                .map(|point| point.range_offset_m.to_bits())
                .collect::<Vec<_>>(),
            other
                .reference_quadrature
                .iter()
                .map(|point| point.range_offset_m.to_bits())
                .collect::<Vec<_>>(),
            "matched-filter range offsets depend on pulse width, not gate spacing"
        );
    }

    #[test]
    fn coupled_estimator_rejects_named_vcp_prf_codes() {
        let config = SyntheticRadarConfig {
            coupled_single_prf_estimator: true,
            scan_strategy: SyntheticScanStrategy::Build24Vcp12,
            ..SyntheticRadarConfig::default()
        };
        let error = config.validate_science_contract().unwrap_err();
        assert!(error.contains("PRF codes are identifiers, not frequencies"));
        assert!(resolve_coupled_instrument(&config).is_err());
    }

    #[test]
    fn coupled_builder_stamps_timing_and_opt_in_stage_grids() {
        let fields = uniform_box_fields();
        let config = SyntheticRadarConfig {
            coupled_single_prf_estimator: true,
            emit_stage_diagnostics: true,
            elevations_deg: vec![0.5],
            azimuth_count: 4,
            gate_spacing_m: 500.0,
            max_range_m: 2_000.0,
            beam_integration: BeamIntegration::Balanced,
            spectrum_width: true,
            ref_gate_texture: false,
            vel_gate_texture: false,
            prf_hz: 1_000.0,
            estimator_dwell_ms: 50.0,
            estimator_pulse_count: Some(50),
            ..box_model_config()
        };
        let coupled = resolve_coupled_instrument(&config).unwrap().unwrap();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let volume = build_synthetic_volume(&fields, time, &config);
        assert!(
            stamped_nyquists(&volume)
                .iter()
                .all(|nyquist| { (*nyquist - coupled.stamped_nyquist_mps()).abs() < f32::EPSILON })
        );
        assert_eq!(
            volume.metadata.prt_s.unwrap().to_bits(),
            (coupled.timing.prt_s as f32).to_bits()
        );
        assert_eq!(
            volume.metadata.unambiguous_range_km.unwrap().to_bits(),
            ((coupled.timing.unambiguous_range_m / 1_000.0) as f32).to_bits()
        );
        let provenance = volume.metadata.forward_operator_config.as_deref().unwrap();
        assert!(provenance.contains("estimator=CustomSinglePrfV1"));
        assert!(provenance.contains("transmitted_pulses=50"));
        assert!(provenance.contains(IDEAL_STAGE_DEFINITION));
        assert!(provenance.contains(MEASURED_STAGE_DEFINITION));
        assert!(provenance.contains(PRESENTED_STAGE_DEFINITION));
        for name in [
            "IREF", "IVEL", "ISW", "IZDR", "IRHO", "IKDP", "MREF", "MVEL", "MSW", "MZDR", "MRHO",
            "MKDP",
        ] {
            let moment = MomentType::Unknown(name.to_string());
            let grid = volume.cuts[0]
                .moments
                .get(&moment)
                .unwrap_or_else(|| panic!("missing {name} stage diagnostic"));
            assert!(matches!(&grid.storage, MomentStorage::F32(_)));
        }
    }

    fn irregular_observed_replay_volume() -> RadarVolume {
        let mut site = RadarSite::new("KOBS");
        site.name = Some("Observed replay fixture".to_string());
        site.latitude_deg = Some(39.0);
        site.longitude_deg = Some(-95.0);
        site.elevation_m = Some(200.0);
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let mut volume = RadarVolume::new(site, time);
        volume.vcp = Some(VcpInfo { pattern: 212 });
        volume.metadata.archive_version = Some("AR2V0006".to_string());
        volume.metadata.prt_s = Some(0.001_25);
        volume.metadata.unambiguous_range_km = Some(119.9);
        let midnight_ms = ((time.timestamp() % 86_400) * 1_000) as i32;
        for split in 0..2usize {
            let mut cut = ElevationCut::new(0.5, Some((split + 1) as u8));
            for (ray_index, azimuth_deg) in [12.25, 97.75, 281.5].into_iter().enumerate() {
                cut.radials.push(Radial {
                    azimuth_deg,
                    elevation_deg: 0.48 + split as f32 * 0.04 + ray_index as f32 * 0.01,
                    time_offset_ms: midnight_ms + (split * 2_000 + ray_index * 175) as i32,
                    gate_range: GateRange {
                        first_gate_m: 125 + ray_index as i32 * 25,
                        gate_spacing_m: 250,
                        gate_count: 5 - usize::from(ray_index == 2),
                    },
                    nyquist_velocity_mps: Some(18.0 + ray_index as f32),
                    radial_status: Some(if ray_index == 0 {
                        radar_core::RadialStatus::StartElevation
                    } else if ray_index == 2 {
                        radar_core::RadialStatus::EndElevation
                    } else {
                        radar_core::RadialStatus::Intermediate
                    }),
                });
            }
            let ref_range = GateRange {
                first_gate_m: 125,
                gate_spacing_m: 250,
                gate_count: 4,
            };
            cut.moments.insert(
                MomentType::Reflectivity,
                f32_grid(
                    MomentType::Reflectivity,
                    ref_range,
                    if split == 0 {
                        vec![0, 2]
                    } else {
                        vec![0, 1, 2]
                    },
                    if split == 0 {
                        vec![20.0; 8]
                    } else {
                        vec![25.0; 12]
                    },
                ),
            );
            if split == 0 {
                cut.moments.insert(
                    MomentType::Velocity,
                    f32_grid(
                        MomentType::Velocity,
                        GateRange {
                            first_gate_m: 500,
                            gate_spacing_m: 500,
                            gate_count: 2,
                        },
                        vec![1, 2],
                        vec![3.0, 4.0, -3.0, -4.0],
                    ),
                );
            }
            volume.cuts.push(cut);
        }
        volume.metadata.decoded_radial_count = 6;
        volume
    }

    #[test]
    fn exact_observed_replay_preserves_irregular_geometry_and_builds_difference() {
        let fields = uniform_box_fields();
        let observed = Arc::new(irregular_observed_replay_volume());
        let observed_handle = Arc::clone(&observed);
        let config = SyntheticRadarConfig {
            ref_gate_texture: false,
            vel_gate_texture: false,
            emit_quality_fields: false,
            ..box_model_config()
        };
        let products = build_exact_replay_products(&fields, observed, &config).unwrap();
        assert!(Arc::ptr_eq(&products.observed, &observed_handle));
        assert!(products.unavailable_observed_moments.is_empty());
        let simulated = &products.simulated;
        assert_eq!(simulated.site, observed_handle.site);
        assert_eq!(simulated.volume_time, observed_handle.volume_time);
        assert_eq!(simulated.vcp, observed_handle.vcp);
        assert_eq!(simulated.metadata.prt_s, observed_handle.metadata.prt_s);
        assert_eq!(
            simulated.metadata.unambiguous_range_km,
            observed_handle.metadata.unambiguous_range_km
        );
        assert_eq!(simulated.cuts.len(), 2);
        for (observed_cut, simulated_cut) in observed_handle.cuts.iter().zip(&simulated.cuts) {
            assert_eq!(simulated_cut.elevation_deg, observed_cut.elevation_deg);
            assert_eq!(
                simulated_cut.elevation_number,
                observed_cut.elevation_number
            );
            assert_eq!(simulated_cut.radials.len(), observed_cut.radials.len());
            for (observed_ray, simulated_ray) in
                observed_cut.radials.iter().zip(&simulated_cut.radials)
            {
                assert_eq!(simulated_ray.azimuth_deg, observed_ray.azimuth_deg);
                assert_eq!(simulated_ray.elevation_deg, observed_ray.elevation_deg);
                assert_eq!(simulated_ray.gate_range, observed_ray.gate_range);
                assert_eq!(
                    simulated_ray.nyquist_velocity_mps,
                    observed_ray.nyquist_velocity_mps
                );
                assert_eq!(simulated_ray.radial_status, observed_ray.radial_status);
                assert_eq!(
                    app_ui::wrf_radar_validation::radial_acquisition_time_utc(
                        &observed_handle,
                        observed_ray
                    ),
                    app_ui::wrf_radar_validation::radial_acquisition_time_utc(
                        simulated,
                        simulated_ray
                    )
                );
            }
            for (moment, observed_grid) in &observed_cut.moments {
                let simulated_grid = &simulated_cut.moments[moment];
                assert_eq!(simulated_grid.gate_range, observed_grid.gate_range);
                assert_eq!(simulated_grid.radial_indices, observed_grid.radial_indices);
            }
            for quality in QualityMoment::ALL {
                assert!(simulated_cut.moments.contains_key(&quality.moment_type()));
            }
        }
        assert!(
            simulated
                .metadata
                .forward_operator_config
                .as_deref()
                .unwrap()
                .contains("vcp_reconstruction=false")
        );
        assert_eq!(products.difference.cuts.len(), observed_handle.cuts.len());
        assert!(
            products.difference.cuts[0]
                .moments
                .contains_key(&MomentType::Unknown("DIF_REF".to_string()))
        );
        assert!(
            products.difference.cuts[0]
                .moments
                .contains_key(&MomentType::Unknown("DIF_VEL".to_string()))
        );
    }

    #[test]
    fn exact_replay_geometry_moves_config_fingerprint() {
        let observed = irregular_observed_replay_volume();
        let first = Arc::new(ExactScanTemplate::from_volume(&observed).unwrap());
        let mut changed = observed.clone();
        changed.cuts[0].radials[1].azimuth_deg += 0.25;
        let second = Arc::new(ExactScanTemplate::from_volume(&changed).unwrap());
        let first_config = SyntheticRadarConfig {
            exact_replay_template: Some(first),
            ..SyntheticRadarConfig::default()
        };
        let second_config = SyntheticRadarConfig {
            exact_replay_template: Some(second),
            ..SyntheticRadarConfig::default()
        };
        assert_ne!(
            first_config.data_fingerprint(),
            second_config.data_fingerprint()
        );
    }

    #[test]
    fn exact_replay_reports_unavailable_observed_polar_moment() {
        let mut observed = irregular_observed_replay_volume();
        observed.cuts[0].moments.insert(
            MomentType::DifferentialReflectivity,
            f32_grid(
                MomentType::DifferentialReflectivity,
                GateRange {
                    first_gate_m: 125,
                    gate_spacing_m: 250,
                    gate_count: 4,
                },
                vec![0, 2],
                vec![1.0; 8],
            ),
        );
        let products = build_exact_replay_products(
            &uniform_box_fields(),
            Arc::new(observed),
            &SyntheticRadarConfig {
                ref_gate_texture: false,
                ..box_model_config()
            },
        )
        .unwrap();
        assert_eq!(products.unavailable_observed_moments.len(), 1);
        assert_eq!(products.unavailable_observed_moments[0].moment, "ZDR");
        assert!(
            !products.simulated.cuts[0]
                .moments
                .contains_key(&MomentType::DifferentialReflectivity)
        );
        assert!(
            !products.difference.cuts[0]
                .moments
                .contains_key(&MomentType::Unknown("DIF_ZDR".to_string()))
        );
    }

    /// The pure gate-spacing resolver, exercised without a file: matching off
    /// always uses the configured spacing; matching on uses the grid DX (clamped)
    /// when the file supplied a usable one, and falls back to the configured
    /// spacing when DX is missing, non-finite, or non-positive.
    #[test]
    fn effective_gate_spacing_resolves_match_and_fallback() {
        let base = SyntheticRadarConfig {
            gate_spacing_m: 250.0,
            ..SyntheticRadarConfig::default()
        };

        // Matching OFF: the configured spacing, regardless of any DX.
        assert_eq!(effective_gate_spacing(&base, None), 250.0);
        assert_eq!(effective_gate_spacing(&base, Some(3000.0)), 250.0);

        let matched = SyntheticRadarConfig {
            match_gate_to_grid: true,
            ..base.clone()
        };

        // Matching ON with a valid DX: the grid resolution (a 3 km grid → 3 km
        // gates, ~77 gates over 230 km instead of 920).
        assert_eq!(effective_gate_spacing(&matched, Some(3000.0)), 3000.0);
        assert_eq!(effective_gate_spacing(&matched, Some(250.0)), 250.0);

        // Clamped both ways so a garbage-but-positive DX can't blow up / collapse
        // the gate count.
        assert_eq!(
            effective_gate_spacing(&matched, Some(5.0)),
            GRID_GATE_MIN_M,
            "a sub-100 m DX clamps up to the floor"
        );
        assert_eq!(
            effective_gate_spacing(&matched, Some(50_000.0)),
            GRID_GATE_MAX_M,
            "a huge DX clamps down to the ceiling"
        );

        // Matching ON but DX missing / invalid: fall back to the configured value.
        assert_eq!(
            effective_gate_spacing(&matched, None),
            250.0,
            "no DX attribute → configured spacing"
        );
        assert_eq!(
            effective_gate_spacing(&matched, Some(0.0)),
            250.0,
            "DX == 0 is invalid → configured spacing"
        );
        assert_eq!(
            effective_gate_spacing(&matched, Some(-3000.0)),
            250.0,
            "negative DX is invalid → configured spacing"
        );
        assert_eq!(
            effective_gate_spacing(&matched, Some(f64::NAN)),
            250.0,
            "NaN DX is invalid → configured spacing"
        );

        // `matched_grid_dx` (drives the import note) agrees with the resolver.
        assert!(matched_grid_dx(&matched, Some(3000.0)));
        assert!(!matched_grid_dx(&matched, None));
        assert!(!matched_grid_dx(&matched, Some(0.0)));
        assert!(
            !matched_grid_dx(&base, Some(3000.0)),
            "off → never grid-matched"
        );
    }

    /// A build with grid-matching ON sizes gates to the file's DX (carried on the
    /// fields), and with DX absent falls back to the configured spacing — the
    /// resolver wired through the actual builder, not just in isolation.
    #[test]
    fn build_honours_match_gate_to_grid_dx() {
        let config = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5],
            azimuth_count: 90,
            gate_spacing_m: 250.0,
            max_range_m: 10_000.0,
            match_gate_to_grid: true,
            ref_gate_texture: false,
            vel_gate_texture: false,
            ..SyntheticRadarConfig::default()
        };
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();

        // DX = 1 km on the fields → 1 km gates (10 over 10 km), overriding the
        // configured 250 m.
        let mut fields = uniform_box_fields();
        fields.dx_m = Some(1000.0);
        let volume = build_synthetic_volume(&fields, time, &config);
        let grid = &volume.cuts[0].moments[&MomentType::Reflectivity];
        assert_eq!(grid.gate_range.gate_spacing_m, 1000);
        assert_eq!(grid.gate_range.gate_count, 10);

        // No DX on the fields → the configured 250 m (40 over 10 km).
        let mut no_dx = uniform_box_fields();
        no_dx.dx_m = None;
        let fallback = build_synthetic_volume(&no_dx, time, &config);
        let fb_grid = &fallback.cuts[0].moments[&MomentType::Reflectivity];
        assert_eq!(fb_grid.gate_range.gate_spacing_m, 250);
        assert_eq!(fb_grid.gate_range.gate_count, 40);
    }

    /// Collect every radial's stamped Nyquist across a built volume.
    fn stamped_nyquists(volume: &RadarVolume) -> Vec<f32> {
        volume
            .cuts
            .iter()
            .flat_map(|cut| cut.radials.iter())
            .filter_map(|radial| radial.nyquist_velocity_mps)
            .collect()
    }

    /// Collect every finite VELocity gate of a built volume (row-major).
    fn finite_velocities(volume: &RadarVolume) -> Vec<f32> {
        let mut out = Vec::new();
        for cut in &volume.cuts {
            let MomentStorage::F32(vels) = &cut.moments[&MomentType::Velocity].storage else {
                panic!("synthetic velocity must be F32");
            };
            out.extend(vels.iter().copied().filter(|v| v.is_finite()));
        }
        out
    }

    /// The fold aliases into the half-open `[-Vn, +Vn)` Level II co-interval:
    /// `|v| < Vn` is unchanged, `Vn+5` wraps to `-Vn+5`, the negative side
    /// mirrors, both boundaries `±Vn` land on `-Vn`, and every result is only the
    /// true value shifted by a whole multiple of `2·Vn`. NaN and a degenerate
    /// Nyquist pass through untouched.
    #[test]
    fn fold_velocity_math_matches_level_ii_convention() {
        let vn = 25.0f32;

        // Inside the co-interval: identity.
        assert_eq!(fold_velocity_mps(0.0, vn), 0.0);
        assert_eq!(fold_velocity_mps(5.0, vn), 5.0);
        assert_eq!(fold_velocity_mps(-7.0, vn), -7.0);
        assert_eq!(fold_velocity_mps(24.0, vn), 24.0);

        // One Nyquist past the top wraps to just inside the bottom, and mirror.
        assert_eq!(fold_velocity_mps(vn + 5.0, vn), -vn + 5.0); // 30 -> -20
        assert_eq!(fold_velocity_mps(-vn - 5.0, vn), vn - 5.0); // -30 -> 20

        // Whole co-intervals fold cleanly: 2·Vn -> 0, 3·Vn -> -Vn.
        assert_eq!(fold_velocity_mps(2.0 * vn, vn), 0.0);
        assert_eq!(fold_velocity_mps(3.0 * vn, vn), -vn);

        // Boundary convention: half-open [-Vn, +Vn) — BOTH +Vn and -Vn map to
        // -Vn (+Vn aliases in, -Vn is already the representable end).
        assert_eq!(fold_velocity_mps(vn, vn), -vn);
        assert_eq!(fold_velocity_mps(-vn, vn), -vn);

        // Sweep: every fold stays in the co-interval and differs from the true
        // value only by a whole multiple of 2·Vn (pure aliasing, no distortion).
        let mut v = -103.0f32;
        while v <= 103.0 {
            let r = fold_velocity_mps(v, vn);
            assert!(
                (-vn - 1e-4..vn + 1e-4).contains(&r),
                "fold({v}) = {r} escaped [-{vn}, {vn})"
            );
            let k = (v - r) / (2.0 * vn);
            assert!(
                (k - k.round()).abs() < 1e-3,
                "fold({v}) = {r} is not a whole-Nyquist alias of the truth (k = {k})"
            );
            // Already-folded values are fixed points (idempotent).
            assert!((fold_velocity_mps(r, vn) - r).abs() < 1e-4);
            v += 0.37;
        }

        // Missing/degenerate inputs pass through untouched.
        assert!(fold_velocity_mps(f32::NAN, vn).is_nan());
        assert_eq!(fold_velocity_mps(12.0, 0.0), 12.0, "Vn=0 is a no-op");
        assert_eq!(fold_velocity_mps(12.0, -5.0), 12.0, "Vn<0 is a no-op");
    }

    /// The stamped Nyquist is the truth downstream sees: the historical 320 with
    /// folding OFF (so the unfolder is a no-op), the folding Nyquist with folding
    /// ON. `nyquist_mps` is ignored when folding is off.
    #[test]
    fn stamped_nyquist_reports_320_off_and_the_fold_nyquist_on() {
        let off = SyntheticRadarConfig::default();
        assert!(!off.fold_velocity);
        assert_eq!(off.stamped_nyquist_mps(), UNFOLDED_NYQUIST_MPS);
        assert_eq!(UNFOLDED_NYQUIST_MPS, 320.0);

        let on = SyntheticRadarConfig {
            fold_velocity: true,
            nyquist_mps: 18.0,
            ..SyntheticRadarConfig::default()
        };
        assert_eq!(on.stamped_nyquist_mps(), 18.0);

        // Folding off: nyquist_mps is inert (still stamps the historical 320).
        let off_custom = SyntheticRadarConfig {
            fold_velocity: false,
            nyquist_mps: 18.0,
            ..SyntheticRadarConfig::default()
        };
        assert_eq!(off_custom.stamped_nyquist_mps(), UNFOLDED_NYQUIST_MPS);

        // The library default folding Nyquist is the documented 25 m/s.
        assert_eq!(off.nyquist_mps, DEFAULT_FOLD_NYQUIST_MPS);
        assert_eq!(DEFAULT_FOLD_NYQUIST_MPS, 25.0);
    }

    /// Folding OFF (the default) stamps 320 on every radial, never aliases the
    /// forward-modelled Vr (the box's peak ~10 m/s stays put), and is fully
    /// insensitive to `nyquist_mps` — two off-builds with different `nyquist_mps`
    /// are bit-identical. This is the "default false = today's behavior" contract.
    #[test]
    fn folding_off_stamps_320_and_leaves_velocity_unfolded() {
        let fields = uniform_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();

        let default_off = box_model_config(); // fold_velocity false, nyquist 25
        let other_nyquist = SyntheticRadarConfig {
            nyquist_mps: 999.0,
            ..box_model_config()
        };
        assert!(!default_off.fold_velocity && !other_nyquist.fold_velocity);

        let a = build_synthetic_volume(&fields, time, &default_off);
        let b = build_synthetic_volume(&fields, time, &other_nyquist);

        // Every radial stamped with the historical unfolded Nyquist.
        for nyq in stamped_nyquists(&a) {
            assert_eq!(
                nyq, UNFOLDED_NYQUIST_MPS,
                "off must stamp the historical 320"
            );
        }
        // No gate folded: the 10 m/s box wind stays inside a wide margin.
        for v in finite_velocities(&a) {
            assert!(v.abs() <= 12.0, "unfolded Vr {v} must not be aliased");
        }
        // `nyquist_mps` is inert with folding off: identical gates AND stamps.
        assert!(
            moment_bits(&a) == moment_bits(&b),
            "nyquist_mps must not touch the data when folding is off"
        );
        assert_eq!(stamped_nyquists(&a), stamped_nyquists(&b));
    }

    /// Folding ON stamps the folding Nyquist on every radial and aliases each
    /// gate exactly as [`fold_velocity_mps`] does — proven gate-by-gate against
    /// the unfolded build, with the box's ~10 m/s peak actually wrapping past an
    /// 8 m/s Nyquist.
    #[test]
    fn folding_on_aliases_velocity_and_stamps_the_nyquist() {
        let fields = uniform_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let vn = 8.0f32;

        let off = box_model_config();
        let on = SyntheticRadarConfig {
            fold_velocity: true,
            nyquist_mps: vn,
            ..box_model_config()
        };
        let off_vol = build_synthetic_volume(&fields, time, &off);
        let on_vol = build_synthetic_volume(&fields, time, &on);

        // Every radial now carries the folding Nyquist as the truth.
        for nyq in stamped_nyquists(&on_vol) {
            assert_eq!(nyq, vn, "folding on must stamp the folding Nyquist");
        }

        // Gate-by-gate: the folded field is exactly the fold of the true field,
        // and at least some gates genuinely wrapped (peak wind > Vn).
        let mut wrapped = 0usize;
        for (cut_off, cut_on) in off_vol.cuts.iter().zip(&on_vol.cuts) {
            let (MomentStorage::F32(a), MomentStorage::F32(b)) = (
                &cut_off.moments[&MomentType::Velocity].storage,
                &cut_on.moments[&MomentType::Velocity].storage,
            ) else {
                panic!("F32");
            };
            for (va, vb) in a.iter().zip(b) {
                assert_eq!(va.is_finite(), vb.is_finite());
                if !va.is_finite() {
                    continue;
                }
                let expect = fold_velocity_mps(*va, vn);
                assert!(
                    (vb - expect).abs() < 1e-4,
                    "gate {va} -> {vb}, expected {expect}"
                );
                assert!(
                    (-vn - 1e-4..vn + 1e-4).contains(vb),
                    "folded {vb} left co-interval"
                );
                if (vb - va).abs() > 1e-3 {
                    wrapped += 1;
                }
            }
        }
        assert!(
            wrapped > 0,
            "the ~10 m/s box wind must fold past an 8 m/s Nyquist"
        );
    }

    /// Folding is applied AFTER the clutter VEL replacement, and clutter gates are
    /// ~0 ± 0.5 m/s — well inside any sane Nyquist — so folding leaves every
    /// near-zero gate (clutter-dominated ground returns included) bit-identical.
    #[test]
    fn folding_leaves_near_zero_clutter_gates_untouched() {
        let fields = uniform_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let vn = 8.0f32;

        // Full ground clutter, folding off vs on at the same 8 m/s Nyquist.
        let off = clutter_box_config(1.0);
        let on = SyntheticRadarConfig {
            fold_velocity: true,
            nyquist_mps: vn,
            ..clutter_box_config(1.0)
        };
        let off_vol = build_synthetic_volume(&fields, time, &off);
        let on_vol = build_synthetic_volume(&fields, time, &on);

        let mut near_zero = 0usize;
        for (cut_off, cut_on) in off_vol.cuts.iter().zip(&on_vol.cuts) {
            let (MomentStorage::F32(a), MomentStorage::F32(b)) = (
                &cut_off.moments[&MomentType::Velocity].storage,
                &cut_on.moments[&MomentType::Velocity].storage,
            ) else {
                panic!("F32");
            };
            for (va, vb) in a.iter().zip(b) {
                if va.is_finite() && va.abs() <= 0.5 {
                    // The clutter ±0.5 m/s band: folding must be the identity here.
                    assert_eq!(
                        vb.to_bits(),
                        va.to_bits(),
                        "near-zero gate {va} must survive folding untouched"
                    );
                    near_zero += 1;
                }
            }
        }
        assert!(
            near_zero > 0,
            "expected near-zero (clutter-dominated) gates in the clutter box"
        );
    }

    /// The per-frame history path key is the dedupe discriminator: identical
    /// inputs give an identical key (unchanged re-import reuses), a different
    /// fingerprint gives a different key (changed config replaces), and the
    /// per-frame index/stamp/site keep frames distinct.
    #[test]
    fn synthetic_frame_path_keys_on_fingerprint_and_frame() {
        let fp = 0x1234_5678_9abc_def0u64;
        let a = synthetic_frame_path("WRF", fp, 0, "20250621_013000");
        // Deterministic: same inputs → identical key (so an unchanged re-import
        // hits the reuse path).
        assert_eq!(a, synthetic_frame_path("WRF", fp, 0, "20250621_013000"));
        // A different fingerprint → different key (the config-change trigger).
        assert_ne!(a, synthetic_frame_path("WRF", 1, 0, "20250621_013000"));
        // Per-frame uniqueness: index, stamp, and site all discriminate.
        assert_ne!(a, synthetic_frame_path("WRF", fp, 1, "20250621_013000"));
        assert_ne!(a, synthetic_frame_path("WRF", fp, 0, "20250621_014500"));
        assert_ne!(a, synthetic_frame_path("KTLX", fp, 0, "20250621_013000"));
        // Fixed-width hex keeps the key stable/greppable.
        assert!(a.to_string_lossy().contains("123456789abcdef0"));
    }

    /// End-to-end dedupe proof (the Bug-1 fix): re-importing the SAME forecast
    /// frame with a CHANGED config must REPLACE the stale volume in the loop
    /// engine, while an UNCHANGED re-import reuses the stored volume. This is
    /// the exact upsert the install path drives — same identity (site + scan
    /// time), keyed on the fingerprint path.
    #[test]
    fn changed_config_replaces_stale_synthetic_frame_but_unchanged_reuses() {
        use ui_core::loop_engine::{
            DecodedLoad, EngineId, EngineRole, FeedSource, FrameStatus, LoadTimings, LoopEngine,
            SelectionPolicy,
        };

        let fields = uniform_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let base = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5],
            azimuth_count: 360,
            gate_spacing_m: 250.0,
            max_range_m: 10_000.0,
            ..SyntheticRadarConfig::default()
        };
        // A genuinely different DATA config (operator swap; any data field
        // would trip the same fingerprint change).
        let changed = SyntheticRadarConfig {
            reflectivity_operator: ReflectivityOperator::ClassicStoelinga,
            ..base.clone()
        };
        assert_ne!(base.data_fingerprint(), changed.data_fingerprint());

        // Build one frame exactly as `install_synthetic_radar_volumes` does.
        let synth_frame = |config: &SyntheticRadarConfig| -> DecodedLoad {
            let volume = Arc::new(build_synthetic_volume(&fields, time, config));
            let stamp = volume.volume_time.format("%Y%m%d_%H%M%S").to_string();
            DecodedLoad {
                path: synthetic_frame_path(&volume.site.id, config.data_fingerprint(), 0, &stamp),
                volume,
                timings: LoadTimings::default(),
                status: FrameStatus::Local,
                source_label: "synthetic".to_string(),
            }
        };

        let mut engine = LoopEngine::new(
            EngineId(1),
            EngineRole::Primary,
            FeedSource::LocalFiles {
                label: "synth".to_owned(),
            },
        );
        // The upsert (replace vs reuse) runs before selection, so the policy is
        // immaterial here — KeepCursor keeps the test focused on the dedupe.
        let policy = SelectionPolicy::KeepCursor;

        // First import (base config).
        let first = synth_frame(&base);
        let first_arc = Arc::clone(&first.volume);
        let _ = engine.install_frame(first, &policy, None);
        assert_eq!(engine.history.len(), 1);
        assert!(Arc::ptr_eq(&engine.history[0].volume, &first_arc));

        // Re-import the SAME frame with a CHANGED config: same identity, NEW
        // path key → rule (c) replaces the stale volume.
        let changed_frame = synth_frame(&changed);
        let changed_arc = Arc::clone(&changed_frame.volume);
        assert_ne!(
            changed_frame.path, engine.history[0].path,
            "changed config must yield a new path key"
        );
        let _ = engine.install_frame(changed_frame, &policy, None);
        assert_eq!(engine.history.len(), 1, "same identity never duplicates");
        assert!(
            Arc::ptr_eq(&engine.history[0].volume, &changed_arc),
            "changed config must REPLACE the stale volume (the staleness fix)"
        );

        // Re-import with UNCHANGED config: same key → rule (b) reuse; the freshly
        // built (bit-identical) volume is discarded, the stored Arc is kept.
        let reimport = synth_frame(&changed);
        assert_eq!(
            reimport.path, engine.history[0].path,
            "unchanged config must yield the same path key"
        );
        let _ = engine.install_frame(reimport, &policy, None);
        assert_eq!(engine.history.len(), 1);
        assert!(
            Arc::ptr_eq(&engine.history[0].volume, &changed_arc),
            "unchanged config reuses the stored volume (the caching win)"
        );
    }

    /// The shipped texture DEFAULTS (owner verdict): reflectivity texture ON,
    /// velocity texture OFF. And when a flag is off, its perturbation path
    /// must not touch a single output value — a both-off build is bit-identical
    /// to a build without the feature.
    #[test]
    fn texture_defaults_ref_on_velocity_off_and_off_builds_are_bit_identical() {
        let defaults = SyntheticRadarConfig::default();
        assert!(
            defaults.ref_gate_texture,
            "reflectivity texture ships ON — the smooth field looks garbage without it"
        );
        assert!(
            !defaults.vel_gate_texture,
            "velocity texture stays opt-in — the clean Vr feeds dealias/GBVTD"
        );

        let fields = uniform_box_fields();
        // box_model_config() is the both-off baseline; an explicit both-off
        // build must be bit-identical to it.
        let config = box_model_config();
        assert!(!config.ref_gate_texture && !config.vel_gate_texture);
        let off = SyntheticRadarConfig {
            ref_gate_texture: false,
            vel_gate_texture: false,
            ..config.clone()
        };
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let a = build_synthetic_volume(&fields, time, &config);
        let b = build_synthetic_volume(&fields, time, &off);
        assert!(
            moment_bits(&a) == moment_bits(&b),
            "both textures off must be bit-identical to a textureless build"
        );
    }

    /// Texture ON: reproducible (hash-seeded, no RNG state — two builds are
    /// bit-identical), bounded (REF within ±2.5 dB of the smooth build, VEL
    /// within ±0.5 m/s), actually visible on REF, and gentle on velocity.
    #[test]
    fn gate_texture_is_deterministic_bounded_and_gentle_on_velocity() {
        let fields = uniform_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let off_config = box_model_config();
        let on_config = SyntheticRadarConfig {
            ref_gate_texture: true,
            vel_gate_texture: true,
            ..off_config.clone()
        };
        let off = build_synthetic_volume(&fields, time, &off_config);
        let on = build_synthetic_volume(&fields, time, &on_config);
        let on_again = build_synthetic_volume(&fields, time, &on_config);
        assert!(
            moment_bits(&on) == moment_bits(&on_again),
            "texture must be deterministic frame to frame"
        );

        let mut compared = 0usize;
        let mut ref_changed = 0usize;
        let mut max_ref_diff = 0.0f32;
        for (cut_off, cut_on) in off.cuts.iter().zip(&on.cuts) {
            for (moment, bound) in [
                (MomentType::Reflectivity, 2.51f32),
                (MomentType::Velocity, 0.51f32),
            ] {
                let MomentStorage::F32(a) = &cut_off.moments[&moment].storage else {
                    panic!("F32");
                };
                let MomentStorage::F32(b) = &cut_on.moments[&moment].storage else {
                    panic!("F32");
                };
                for (va, vb) in a.iter().zip(b) {
                    // Uniform 40 dBZ against a 0 dBZ floor: texture can
                    // never flip a gate across the floor here, so finite
                    // patterns must match exactly.
                    assert_eq!(va.is_finite(), vb.is_finite());
                    if !va.is_finite() {
                        continue;
                    }
                    let diff = (vb - va).abs();
                    assert!(diff <= bound, "{moment:?} perturbed by {diff}");
                    if moment == MomentType::Reflectivity {
                        compared += 1;
                        max_ref_diff = max_ref_diff.max(diff);
                        if diff > 0.05 {
                            ref_changed += 1;
                        }
                    }
                }
            }
        }
        assert!(compared > 10_000, "too few echo gates: {compared}");
        assert!(
            ref_changed * 2 > compared,
            "texture must actually move most REF gates ({ref_changed}/{compared})"
        );
        assert!(
            max_ref_diff > 1.5,
            "texture peak {max_ref_diff} dB is too tame to read as speckle"
        );
    }

    /// Texture is applied BEFORE the reflectivity floor, so echo edges go
    /// ragged like a marginal-SNR scope: with the floor parked just above
    /// the model's uniform 40 dBZ, the smooth build shows nothing while the
    /// textured build pokes gates through — all within the noise peak.
    #[test]
    fn gate_texture_pokes_ragged_gates_through_the_ref_floor() {
        let fields = uniform_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let off_config = SyntheticRadarConfig {
            ref_floor_dbz: 40.5,
            ..box_model_config()
        };
        let on_config = SyntheticRadarConfig {
            ref_gate_texture: true,
            ..off_config.clone()
        };
        let count_finite = |volume: &RadarVolume| {
            moment_bits(volume)
                .iter()
                .filter(|bits| f32::from_bits(**bits).is_finite())
                .count()
        };
        let off = build_synthetic_volume(&fields, time, &off_config);
        assert_eq!(
            count_finite(&off),
            0,
            "smooth 40 dBZ under a 40.5 floor must stay empty"
        );
        let on = build_synthetic_volume(&fields, time, &on_config);
        assert!(
            count_finite(&on) > 0,
            "texture must poke some gates through the floor"
        );
        for cut in &on.cuts {
            let MomentStorage::F32(values) = &cut.moments[&MomentType::Reflectivity].storage else {
                panic!("F32");
            };
            for value in values.iter().filter(|value| value.is_finite()) {
                assert!(
                    (40.5..=42.6).contains(value),
                    "ragged-edge gate {value} outside the floor..floor+peak band"
                );
            }
        }
    }

    /// The clutter scan config: the clean two-tilt box (both textures off) at a
    /// given clutter amount. Both tilts (0.5°, 3.1°) are below the clutter tilt
    /// limit, and every gate of the 10 km / 250 m circle is inside the 40 km cap.
    fn clutter_box_config(intensity: f32) -> SyntheticRadarConfig {
        SyntheticRadarConfig {
            clutter_intensity: intensity,
            ..box_model_config()
        }
    }

    /// The weak-echo box: the uniform box but 8 dBZ everywhere (still 10 m/s
    /// east wind), so fabricated clutter (up to ~35 dBZ) dominates and the
    /// velocity-replacement path is exercised. NaN-free counting relies on the
    /// echo being a single known value (8 dBZ) distinct from any clutter value.
    fn weak_echo_box_fields() -> WrfRadarFields {
        let mut fields = uniform_box_fields();
        fields.dbz = vec![8.0f32; fields.dbz.len()];
        fields
    }

    /// Every finite reflectivity value of a volume, tilt by tilt.
    fn ref_values(volume: &RadarVolume) -> Vec<f32> {
        let mut out = Vec::new();
        for cut in &volume.cuts {
            let MomentStorage::F32(values) = &cut.moments[&MomentType::Reflectivity].storage else {
                panic!("F32");
            };
            out.extend(values.iter().copied());
        }
        out
    }

    /// Clutter is deterministic (a rebuilt frame is bit-identical — no loop
    /// shimmer) and an amount of 0 injects NOTHING: every finite reflectivity is
    /// the pristine model echo, and the field differs from a cluttered build.
    #[test]
    fn clutter_is_deterministic_and_zero_amount_injects_nothing() {
        let fields = weak_echo_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();

        // Amount 0: the clutter path is skipped, so every finite REF is the pure
        // 8 dBZ echo — nothing is injected (bit-identical to the featureless path).
        let clean = build_synthetic_volume(&fields, time, &clutter_box_config(0.0));
        for value in ref_values(&clean).iter().filter(|value| value.is_finite()) {
            assert!(
                (value - 8.0).abs() < 1e-6,
                "amount-0 REF {value} must be the pristine model echo — no clutter injected"
            );
        }

        // Amount 1: two rebuilds of the SAME frame are bit-identical (no
        // shimmer), and clutter actually changed the field.
        let full = build_synthetic_volume(&fields, time, &clutter_box_config(1.0));
        let full_again = build_synthetic_volume(&fields, time, &clutter_box_config(1.0));
        assert_eq!(
            moment_bits(&full),
            moment_bits(&full_again),
            "same frame + config must rebuild bit-identically"
        );
        assert_ne!(
            moment_bits(&clean),
            moment_bits(&full),
            "clutter at full amount must change the field"
        );

        // A DIFFERENT forecast frame (different valid time) gets a DIFFERENT
        // clutter pattern — the seed folds in the frame time.
        let other_time = DateTime::<Utc>::from_timestamp(1_700_003_600, 0).unwrap();
        let other = build_synthetic_volume(&fields, other_time, &clutter_box_config(1.0));
        assert_ne!(
            moment_bits(&full),
            moment_bits(&other),
            "distinct forecast frames must get distinct clutter"
        );
    }

    /// The clutter amount scales the number of cluttered gates monotonically:
    /// none at 0, some at low amounts, and strictly more at full amount. A
    /// cluttered gate is a finite REF that exceeds the 8 dBZ model echo (clutter
    /// is applied only where it is stronger than the existing echo).
    #[test]
    fn clutter_amount_increases_the_number_of_cluttered_gates() {
        let fields = weak_echo_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let count = |intensity: f32| -> usize {
            let volume = build_synthetic_volume(&fields, time, &clutter_box_config(intensity));
            ref_values(&volume)
                .iter()
                .filter(|value| value.is_finite() && **value > 8.0 + 1e-3)
                .count()
        };
        let zero = count(0.0);
        let low = count(0.5);
        let mid = count(0.75);
        let high = count(1.0);
        assert_eq!(zero, 0, "amount 0 produces no clutter");
        assert!(low > 0, "amount 0.5 produces some clutter ({low})");
        assert!(
            mid >= low,
            "clutter is monotonic: 0.75 ≥ 0.5 ({mid} ≥ {low})"
        );
        assert!(
            high >= mid,
            "clutter is monotonic: 1.0 ≥ 0.75 ({high} ≥ {mid})"
        );
        assert!(
            high > low,
            "full amount must clutter strictly more gates than a lower amount ({high} > {low})"
        );
    }

    /// Storms are never overwritten: a uniform 40 dBZ field is stronger than any
    /// clutter value (clipped to ≤ 35 dBZ, so ≲ 37 dBZ after texture), so a
    /// full-amount build is bit-identical to a no-clutter build.
    #[test]
    fn clutter_never_overwrites_stronger_echo() {
        let fields = uniform_box_fields(); // 40 dBZ everywhere
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let clean = build_synthetic_volume(&fields, time, &clutter_box_config(0.0));
        let cluttered = build_synthetic_volume(&fields, time, &clutter_box_config(1.0));
        assert_eq!(
            moment_bits(&clean),
            moment_bits(&cluttered),
            "40 dBZ echo out-values any clutter — the field must be untouched"
        );
    }

    /// Clutter velocity: where clutter DOMINATES a gate that carried a
    /// wind-projected velocity, the velocity is replaced by a near-zero return
    /// (the ground is stationary); pure-echo gates keep their wind projection.
    /// In clear air (echo below a high floor) the clutter gate has REF but its
    /// velocity is left blank — the near-zero override is gated on velocity data
    /// existing, so it never fabricates a velocity where there was none.
    #[test]
    fn clutter_velocity_is_near_zero_where_wind_existed_and_blank_in_clear_air() {
        let fields = weak_echo_box_fields(); // 8 dBZ, 10 m/s east wind
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();

        // Echo present (floor 0): cluttered gates keep a (now near-zero)
        // velocity; the surviving 8 dBZ gates keep the wind projection.
        let volume = build_synthetic_volume(&fields, time, &clutter_box_config(1.0));
        let mut clutter_vel_gates = 0usize;
        let mut fast_wind_gates = 0usize;
        for cut in &volume.cuts {
            let MomentStorage::F32(refs) = &cut.moments[&MomentType::Reflectivity].storage else {
                panic!("F32");
            };
            let MomentStorage::F32(vels) = &cut.moments[&MomentType::Velocity].storage else {
                panic!("F32");
            };
            for (r, v) in refs.iter().zip(vels) {
                if !r.is_finite() {
                    continue;
                }
                if *r > 8.0 + 1e-3 {
                    assert!(v.is_finite(), "clutter gate over echo must keep a velocity");
                    assert!(
                        v.abs() <= 0.51,
                        "clutter-dominated Vr {v} must be ~0 (stationary ground)"
                    );
                    clutter_vel_gates += 1;
                } else if v.is_finite() && v.abs() > 1.0 {
                    fast_wind_gates += 1;
                }
            }
        }
        assert!(clutter_vel_gates > 0, "expected clutter-dominated gates");
        assert!(
            fast_wind_gates > 0,
            "pure-echo gates must keep the wind projection"
        );

        // Clear air (floor 30, so the 8 dBZ echo never passes): any finite REF
        // is clutter, and its velocity must be BLANK (no wind projection existed
        // to replace).
        let clear_cfg = SyntheticRadarConfig {
            ref_floor_dbz: 30.0,
            ..clutter_box_config(1.0)
        };
        let clear = build_synthetic_volume(&fields, time, &clear_cfg);
        let mut clear_clutter = 0usize;
        for cut in &clear.cuts {
            let MomentStorage::F32(refs) = &cut.moments[&MomentType::Reflectivity].storage else {
                panic!("F32");
            };
            let MomentStorage::F32(vels) = &cut.moments[&MomentType::Velocity].storage else {
                panic!("F32");
            };
            for (r, v) in refs.iter().zip(vels) {
                if r.is_finite() {
                    assert!(
                        !v.is_finite(),
                        "clear-air clutter gate must leave velocity blank (gated on vel existing)"
                    );
                    clear_clutter += 1;
                }
            }
        }
        assert!(clear_clutter > 0, "clutter must fill some clear-air gates");
    }

    /// A rotated curvilinear domain (nz = 2, uniform 40 dBZ, 10 m/s east
    /// wind): its true edge is NOT the lat/lon bbox, which is exactly where
    /// the pre-fix LUT leaked nearest-edge values (the smeared boundary
    /// ring). `bounded` picks the fixed vs the old LUT build.
    fn rotated_domain_fields(
        n: usize,
        spacing: f32,
        theta_deg: f32,
        bounded: bool,
    ) -> WrfRadarFields {
        let c = (n as f32 - 1.0) / 2.0;
        let (sin_t, cos_t) = theta_deg.to_radians().sin_cos();
        let cells = n * n;
        let nz = 2;
        let mut lat = Vec::with_capacity(cells);
        let mut lon = Vec::with_capacity(cells);
        for j in 0..n {
            for i in 0..n {
                let x = (i as f32 - c) * spacing;
                let y = (j as f32 - c) * spacing;
                lon.push(-95.0 + x * cos_t - y * sin_t);
                lat.push(39.0 + x * sin_t + y * cos_t);
            }
        }
        let mut height_msl = vec![100.0f32; nz * cells];
        height_msl[cells..].fill(8000.0);
        let lut = if bounded {
            InverseLut::build_with_shape_domain_bounded(&lat, &lon, n, n).expect("bounded lut")
        } else {
            InverseLut::build_with_shape(&lat, &lon, n, n).expect("plain lut")
        };
        WrfRadarFields {
            nx: n,
            ny: n,
            nz,
            lat,
            lon,
            height_msl,
            dbz: vec![40.0f32; nz * cells],
            u: vec![10.0f32; nz * cells],
            v: vec![0.0f32; nz * cells],
            w: vec![0.0f32; nz * cells],
            terrain_m: vec![0.0f32; cells],
            property_scattering: None,
            raw_property_scene: None,
            polarimetric: None,
            dual_pol_status: None,
            tke_tenths_m2s2: None,
            ref_source: "test",
            dx_m: None,
            lut,
        }
    }

    /// The domain-edge fix, end to end: on a rotated domain every finite
    /// gate must georeference to inside the true (rotated) domain edge —
    /// and the OLD, unbounded LUT demonstrably leaks a ring of finite gates
    /// past that edge on the same fixture, proving the assertion has teeth.
    #[test]
    fn synthetic_gates_stay_nan_outside_the_true_domain_edge() {
        let n = 41usize;
        let spacing = 0.02f32; // deg → half-width 0.4° (~44 km)
        let theta = 30.0f32;
        let config = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(300.0),
            elevations_deg: vec![0.5],
            azimuth_count: 360,
            gate_spacing_m: 250.0,
            max_range_m: 120_000.0,
            ..SyntheticRadarConfig::default()
        };
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();

        let half = (n as f32 - 1.0) / 2.0 * spacing;
        // Allowed slack past the edge: one LUT bin (a bin the boundary
        // merely clips legitimately resolves) plus in-bin position.
        let bin = spacing * theta.to_radians().cos() * 1.25;
        let tol = 2.0 * bin;
        let (sin_t, cos_t) = theta.to_radians().sin_cos();

        // Worst finite-gate excursion in the domain's own (rotated) frame.
        let worst_extent = |volume: &RadarVolume| -> (f32, usize) {
            let mut worst = 0.0f32;
            let mut finite = 0usize;
            for cut in &volume.cuts {
                let grid = &cut.moments[&MomentType::Reflectivity];
                let MomentStorage::F32(values) = &grid.storage else {
                    panic!("F32");
                };
                let gate_count = grid.gate_range.gate_count;
                let spacing_m = f64::from(grid.gate_range.gate_spacing_m);
                for (row, radial) in cut.radials.iter().enumerate() {
                    let az_rad = f64::from(radial.azimuth_deg).to_radians();
                    for gate in 0..gate_count {
                        if !values[row * gate_count + gate].is_finite() {
                            continue;
                        }
                        finite += 1;
                        let ground = beam_ground_range_m(
                            gate as f64 * spacing_m,
                            f64::from(radial.elevation_deg),
                        );
                        let (glat, glon) = aeqd_inverse_km(
                            39.0,
                            -95.0,
                            ground * az_rad.sin() / 1000.0,
                            ground * az_rad.cos() / 1000.0,
                        );
                        let (dlat, dlon) = ((glat - 39.0) as f32, (glon + 95.0) as f32);
                        let x = dlon * cos_t + dlat * sin_t;
                        let y = -dlon * sin_t + dlat * cos_t;
                        worst = worst.max(x.abs().max(y.abs()));
                    }
                }
            }
            (worst, finite)
        };

        let fixed = build_synthetic_volume(
            &rotated_domain_fields(n, spacing, theta, true),
            time,
            &config,
        );
        let (worst, finite) = worst_extent(&fixed);
        assert!(
            finite > 10_000,
            "beam must cover the domain: {finite} gates"
        );
        assert!(
            worst <= half + tol,
            "finite gate {worst}° from centre exceeds the true edge ({})",
            half + tol
        );

        let leaky = build_synthetic_volume(
            &rotated_domain_fields(n, spacing, theta, false),
            time,
            &config,
        );
        let (worst_leak, _) = worst_extent(&leaky);
        assert!(
            worst_leak > half + tol,
            "the unbounded LUT should leak past the edge (got {worst_leak}°) — \
             otherwise this test can't catch the bug"
        );
    }

    /// Real-data verification (project rule: prove on REAL data). Gated on
    /// `BOWECHO_WRF_RADAR_FIXTURE=<wrfout path>`; when set, builds a synthetic
    /// volume from the real file, renders the lowest tilt to a PNG for eyeball
    /// review, and asserts the reflectivity CO-LOCATES with the model's own
    /// column-max reflectivity (a georef proof) and lands in a physical dBZ
    /// band. Set `BOWECHO_WRF_RADAR_PNG=<dir>` to choose the PNG output dir.
    #[test]
    fn real_wrfout_builds_and_colocates_with_model_composite() {
        let Some(path) = std::env::var_os("BOWECHO_WRF_RADAR_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let file = WrfFile::open(&path).expect("open real wrfout");
        let config = SyntheticRadarConfig {
            azimuth_count: 720,
            ..SyntheticRadarConfig::default()
        };
        let fields = read_wrf_radar_fields(&file, 0, config.reflectivity_operator)
            .expect("read WRF radar fields");
        eprintln!(
            "reflectivity source: {}  grid {}x{}x{}",
            fields.ref_source, fields.nx, fields.ny, fields.nz
        );

        // Model column-max reflectivity (composite) + its argmax cell.
        let cells = fields.cells();
        let mut composite = vec![f32::NEG_INFINITY; cells];
        for k in 0..fields.nz {
            for (c, comp) in composite.iter_mut().enumerate() {
                let value = fields.dbz[k * cells + c];
                if value.is_finite() && value > *comp {
                    *comp = value;
                }
            }
        }
        let (argmax_cell, &model_max) = composite
            .iter()
            .enumerate()
            .filter(|(_, value)| value.is_finite())
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("finite composite max");
        let model_max_lat = fields.lat[argmax_cell];
        let model_max_lon = fields.lon[argmax_cell];
        eprintln!(
            "model composite max {model_max:.1} dBZ at lat {model_max_lat:.3} lon {model_max_lon:.3}"
        );
        assert!(
            (5.0..=90.0).contains(&model_max),
            "model composite max {model_max} dBZ is non-physical"
        );

        let time = file
            .times()
            .ok()
            .and_then(|times| times.first().and_then(|raw| parse_wrf_time(raw)))
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        let volume = build_synthetic_volume(&fields, time, &config);
        assert_eq!(volume.cuts.len(), config.elevations_deg.len());

        // The synthetic scan must carry real echo, in a physical band, and its
        // strongest gates must sit near the model composite maximum — the
        // georeferencing proof (beam geometry + AEQD placement + sampling).
        let site_lat = f64::from(volume.site.latitude_deg.unwrap());
        let site_lon = f64::from(volume.site.longitude_deg.unwrap());
        let mut finite_gates = 0usize;
        let mut synth_max = f32::NEG_INFINITY;
        let mut best_dist_km = f64::INFINITY;
        for cut in &volume.cuts {
            let grid = &cut.moments[&MomentType::Reflectivity];
            let MomentStorage::F32(values) = &grid.storage else {
                panic!("REF must be F32");
            };
            let gate_count = grid.gate_range.gate_count;
            let spacing = grid.gate_range.gate_spacing_m as f64;
            for (row, radial) in cut.radials.iter().enumerate() {
                let az_rad = f64::from(radial.azimuth_deg).to_radians();
                for gate in 0..gate_count {
                    let value = values[row * gate_count + gate];
                    if !value.is_finite() {
                        continue;
                    }
                    finite_gates += 1;
                    synth_max = synth_max.max(value);
                    // Only the strongest gates (within 6 dBZ of the model peak)
                    // are required to co-locate; find the closest to the model
                    // composite argmax.
                    if value >= model_max - 6.0 {
                        let ground = beam_ground_range_m(
                            gate as f64 * spacing,
                            f64::from(radial.elevation_deg),
                        );
                        let east_km = ground * az_rad.sin() / 1000.0;
                        let north_km = ground * az_rad.cos() / 1000.0;
                        let (glat, glon) = aeqd_inverse_km(site_lat, site_lon, east_km, north_km);
                        let dist = haversine_km(
                            glat,
                            glon,
                            f64::from(model_max_lat),
                            f64::from(model_max_lon),
                        );
                        best_dist_km = best_dist_km.min(dist);
                    }
                }
            }
        }
        eprintln!(
            "synthetic: {finite_gates} finite REF gates, max {synth_max:.1} dBZ, \
             nearest strong gate {best_dist_km:.2} km from model composite peak"
        );
        assert!(finite_gates > 1000, "too few echo gates: {finite_gates}");
        assert!(
            (5.0..=90.0).contains(&synth_max),
            "synthetic max {synth_max} dBZ non-physical"
        );
        assert!(
            synth_max >= model_max - 6.0,
            "synthetic peak {synth_max} far below model composite {model_max}"
        );
        // Strong echo within a few grid cells of the model peak proves the
        // geometry + georeferencing are right (WRF grid ~1 km here).
        assert!(
            best_dist_km <= 8.0,
            "strongest synthetic echo is {best_dist_km:.1} km from the model \
             composite peak — georeferencing is off"
        );

        // Render the lowest tilt to a PNG for the mandatory eyeball check.
        let out_dir = std::env::var_os("BOWECHO_WRF_RADAR_PNG")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&out_dir);
        let ref_png = out_dir.join("wrf_synth_ref.png");
        render2d::render_moment_png(
            &volume,
            0,
            MomentType::Reflectivity,
            &ref_png,
            render2d::RasterOptions::default(),
        )
        .expect("render synthetic REF PNG");
        let vel_png = out_dir.join("wrf_synth_vel.png");
        render2d::render_moment_png(
            &volume,
            0,
            MomentType::Velocity,
            &vel_png,
            render2d::RasterOptions::default(),
        )
        .expect("render synthetic VEL PNG");
        eprintln!("wrote {} and {}", ref_png.display(), vel_png.display());
    }

    fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        let r = 6371.0;
        let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
        let dphi = (lat2 - lat1).to_radians();
        let dlam = (lon2 - lon1).to_radians();
        let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlam / 2.0).sin().powi(2);
        2.0 * r * a.sqrt().asin()
    }

    /// Crop a square window centred on (cx, cy) out of a rendered PPI and
    /// save it (clamped to the image bounds).
    fn save_crop(image: &image::RgbaImage, cx: f32, cy: f32, window: u32, path: &Path) {
        let window = window.min(image.width()).min(image.height());
        let x0 = ((cx - window as f32 / 2.0).round().max(0.0) as u32).min(image.width() - window);
        let y0 = ((cy - window as f32 / 2.0).round().max(0.0) as u32).min(image.height() - window);
        image::imageops::crop_imm(image, x0, y0, window, window)
            .to_image()
            .save(path)
            .expect("save crop");
    }

    /// CLOSING PROOF for the gates-polish track (v0.29.3): ONE pass over a
    /// real Enderlin wrfout, run on the build node in release. Gated on
    /// `BOWECHO_GATES_PROBE_FIXTURE=<wrfout path>`; PNGs land in
    /// `BOWECHO_GATES_PROBE_PNG` (default: temp dir):
    ///  - `gates_default.png` (+`_zoom`) — lowest-tilt REF PPI, default
    ///    config: must look identical to the current shipped gates;
    ///  - `gates_speckle.png` (+`_zoom`) — the same PPI with the opt-in
    ///    reflectivity gate-texture speckle;
    ///  - `edge_before_zoom.png` / `edge_after_zoom.png` — the WRF domain
    ///    edge with the old bbox-dilated LUT (smeared off-domain ring) vs
    ///    the domain-bounded LUT (clean NaN cutoff). The "before" swaps the
    ///    unbounded LUT into the same fields — test-only, never a product
    ///    path. Also counts finite gates georeferencing off-domain: must be
    ///    zero after the fix, nonzero before (the ring, quantified).
    #[test]
    fn gates_polish_probe_renders_proof_pngs() {
        let Some(path) = std::env::var_os("BOWECHO_GATES_PROBE_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let out_dir = std::env::var_os("BOWECHO_GATES_PROBE_PNG")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&out_dir);

        let file = WrfFile::open(&path).expect("open real wrfout");
        // "gates_default" is the SMOOTH reference (both textures off) so the
        // speckle crop below is a clean before/after; the shipped default now
        // enables reflectivity texture, which is exactly the "speckle" look.
        let config = SyntheticRadarConfig {
            ref_gate_texture: false,
            vel_gate_texture: false,
            ..SyntheticRadarConfig::default()
        };
        let mut fields = read_wrf_radar_fields(&file, 0, config.reflectivity_operator)
            .expect("read WRF radar fields");
        let time = file
            .times()
            .ok()
            .and_then(|times| times.first().and_then(|raw| parse_wrf_time(raw)))
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());

        let raster = render2d::RasterOptions {
            width: 4096,
            height: 4096,
            range_fraction: 100,
        };
        let centre_px = (raster.width as f32 - 1.0) / 2.0;
        let to_px = |east_m: f64, north_m: f64, range_m: f64| -> (f32, f32) {
            (
                centre_px + (east_m / range_m) as f32 * centre_px,
                centre_px - (north_m / range_m) as f32 * centre_px,
            )
        };

        // (a) Default mode — the shipped look, must be visually identical
        // to the current gates.
        let default_volume = build_synthetic_volume(&fields, time, &config);
        let default_img =
            render2d::render_moment_image(&default_volume, 0, MomentType::Reflectivity, raster)
                .expect("render default PPI");
        default_img
            .save(out_dir.join("gates_default.png"))
            .expect("save default PPI");

        // Strongest low-tilt echo — the zoom anchor for the texture crops.
        let cut = &default_volume.cuts[0];
        let grid = &cut.moments[&MomentType::Reflectivity];
        let MomentStorage::F32(values) = &grid.storage else {
            panic!("REF must be F32");
        };
        let gate_count = grid.gate_range.gate_count;
        let spacing_m = f64::from(grid.gate_range.gate_spacing_m);
        let mut best = (f32::NEG_INFINITY, 0usize, 0usize);
        for (row, _) in cut.radials.iter().enumerate() {
            for gate in 0..gate_count {
                let value = values[row * gate_count + gate];
                if value.is_finite() && value > best.0 {
                    best = (value, row, gate);
                }
            }
        }
        let az_rad = f64::from(cut.radials[best.1].azimuth_deg).to_radians();
        let ground = beam_ground_range_m(best.2 as f64 * spacing_m, f64::from(cut.elevation_deg));
        let (echo_x, echo_y) = to_px(
            ground * az_rad.sin(),
            ground * az_rad.cos(),
            config.max_range_m,
        );
        save_crop(
            &default_img,
            echo_x,
            echo_y,
            768,
            &out_dir.join("gates_default_zoom.png"),
        );

        // (b) Speckle mode (reflectivity texture — the shipped default look).
        let speckle_config = SyntheticRadarConfig {
            ref_gate_texture: true,
            ..config.clone()
        };
        let speckle_volume = build_synthetic_volume(&fields, time, &speckle_config);
        let speckle_img =
            render2d::render_moment_image(&speckle_volume, 0, MomentType::Reflectivity, raster)
                .expect("render speckle PPI");
        speckle_img
            .save(out_dir.join("gates_speckle.png"))
            .expect("save speckle PPI");
        save_crop(
            &speckle_img,
            echo_x,
            echo_y,
            768,
            &out_dir.join("gates_speckle_zoom.png"),
        );

        // (c) Domain-edge zoom, before/after. Aim at the closest domain-side
        // midpoint and scan just past it so the edge fills the frame.
        let site_lat = f64::from(default_volume.site.latitude_deg.unwrap());
        let site_lon = f64::from(default_volume.site.longitude_deg.unwrap());
        let (nx, ny) = (fields.nx, fields.ny);
        let side_mids = [
            (ny - 1) * nx + nx / 2, // north
            (ny / 2) * nx + nx - 1, // east
            nx / 2,                 // south
            (ny / 2) * nx,          // west
        ];
        let (edge_east_km, edge_north_km) = side_mids
            .iter()
            .map(|&cell| {
                ui_core::geo::aeqd_forward_km(
                    site_lat,
                    site_lon,
                    f64::from(fields.lat[cell]),
                    f64::from(fields.lon[cell]),
                )
            })
            .min_by(|a, b| a.0.hypot(a.1).total_cmp(&b.0.hypot(b.1)))
            .expect("four side midpoints");
        let edge_dist_m = edge_east_km.hypot(edge_north_km) * 1000.0;
        let edge_config = SyntheticRadarConfig {
            elevations_deg: vec![config.elevations_deg[0]],
            max_range_m: (edge_dist_m * 1.25).max(60_000.0),
            ..config.clone()
        };

        // Off-domain finite-gate counter (the bounded LUT is the truth);
        // also returns the strongest leaked gate's (east, north) metres so
        // the edge zoom frames the actual smear, not empty boundary.
        let bounded_lut = InverseLut::build_with_shape_domain_bounded(
            &fields.lat,
            &fields.lon,
            fields.nx,
            fields.ny,
        )
        .expect("bounded LUT");
        // Leaked gate: finite in the volume but off the true domain (bounded
        // LUT says None). Returns (azimuth row, east m, north m, dBZ) per
        // leak, exact — azimuths rebuilt in f64 from the radial index just
        // like build_cut, so each gate re-maps to the very lat/lon the
        // sampler used.
        let off_domain_finite = |volume: &RadarVolume| -> Vec<(usize, f64, f64, f32)> {
            let cut = &volume.cuts[0];
            let grid = &cut.moments[&MomentType::Reflectivity];
            let MomentStorage::F32(values) = &grid.storage else {
                panic!("REF must be F32");
            };
            let gate_count = grid.gate_range.gate_count;
            let spacing_m = f64::from(grid.gate_range.gate_spacing_m);
            let naz = cut.radials.len();
            let mut leaks = Vec::new();
            for (row, radial) in cut.radials.iter().enumerate() {
                let az_rad = (row as f64 * 360.0 / naz as f64).to_radians();
                for gate in 0..gate_count {
                    let value = values[row * gate_count + gate];
                    if !value.is_finite() {
                        continue;
                    }
                    let ground = beam_ground_range_m(
                        gate as f64 * spacing_m,
                        f64::from(radial.elevation_deg),
                    );
                    let (east_m, north_m) = (ground * az_rad.sin(), ground * az_rad.cos());
                    let (glat, glon) =
                        aeqd_inverse_km(site_lat, site_lon, east_m / 1000.0, north_m / 1000.0);
                    if bounded_lut.lookup(glat as f32, glon as f32).is_none() {
                        leaks.push((row, east_m, north_m, value));
                    }
                }
            }
            leaks
        };

        let edge_after = build_synthetic_volume(&fields, time, &edge_config);
        let after_img =
            render2d::render_moment_image(&edge_after, 0, MomentType::Reflectivity, raster)
                .expect("render edge-after PPI");
        let after_leak = off_domain_finite(&edge_after).len();

        // "Before": the OLD bbox-dilated LUT swapped into the same fields —
        // test-only; the product path always builds domain-bounded.
        fields.lut = InverseLut::build_with_shape(&fields.lat, &fields.lon, fields.nx, fields.ny)
            .expect("unbounded LUT");
        let edge_before = build_synthetic_volume(&fields, time, &edge_config);
        let before_img =
            render2d::render_moment_image(&edge_before, 0, MomentType::Reflectivity, raster)
                .expect("render edge-before PPI");
        let leaks = off_domain_finite(&edge_before);
        let before_leak = leaks.len();

        // Zoom anchor: the DENSEST leak cluster (mode azimuth ±15 radials).
        // The ring touches the boundary at several separate arcs, so a
        // global centroid averages into the domain interior and frames
        // nothing; fall back to the nearest side midpoint when echo never
        // reaches the boundary at all.
        let leak_center = (!leaks.is_empty()).then(|| {
            let naz = edge_before.cuts[0].radials.len();
            let mut per_row = vec![0usize; naz];
            for &(row, ..) in &leaks {
                per_row[row] += 1;
            }
            let best_row = (0..naz).max_by_key(|&row| per_row[row]).unwrap_or(0);
            let near: Vec<_> = leaks
                .iter()
                .filter(|(row, ..)| row.abs_diff(best_row) <= 15)
                .collect();
            let n = near.len().max(1) as f64;
            (
                near.iter().map(|(_, east, ..)| east).sum::<f64>() / n,
                near.iter().map(|(_, _, north, _)| north).sum::<f64>() / n,
            )
        });
        let (leak_east_m, leak_north_m) =
            leak_center.unwrap_or((edge_east_km * 1000.0, edge_north_km * 1000.0));
        let (edge_x, edge_y) = to_px(leak_east_m, leak_north_m, edge_config.max_range_m);
        save_crop(
            &after_img,
            edge_x,
            edge_y,
            1024,
            &out_dir.join("edge_after_zoom.png"),
        );
        save_crop(
            &before_img,
            edge_x,
            edge_y,
            1024,
            &out_dir.join("edge_before_zoom.png"),
        );
        // Unambiguous ring picture: every leaked GATE painted magenta on the
        // fixed render, straight from the volume data. The leak here is
        // 0-5 dBZ fringe echo — below the REF colour table's first visible
        // level — so it is invisible in a plain before/after PNG pair and a
        // pixel diff only catches renderer smoothing; painting the gate
        // footprints shows exactly where the old LUT smeared past the edge.
        let mut ring_img = after_img.clone();
        let (width_px, height_px) = (ring_img.width() as i64, ring_img.height() as i64);
        for &(_, east_m, north_m, _) in &leaks {
            let (px, py) = to_px(east_m, north_m, edge_config.max_range_m);
            for dy in -2i64..=2 {
                for dx in -2i64..=2 {
                    let (x, y) = (px.round() as i64 + dx, py.round() as i64 + dy);
                    if (0..width_px).contains(&x) && (0..height_px).contains(&y) {
                        ring_img.put_pixel(x as u32, y as u32, image::Rgba([255, 0, 255, 255]));
                    }
                }
            }
        }
        save_crop(
            &ring_img,
            edge_x,
            edge_y,
            1024,
            &out_dir.join("edge_ring_highlight_zoom.png"),
        );

        // Interior integrity, on the REAL file: the fix may only REMOVE
        // off-domain gates — every gate finite in BOTH builds must carry
        // bit-identical values (verified: 0 differing gates on Enderlin
        // 02:15Z; the fix touches nothing inside the domain).
        {
            let MomentStorage::F32(vb) =
                &edge_before.cuts[0].moments[&MomentType::Reflectivity].storage
            else {
                panic!("F32");
            };
            let MomentStorage::F32(va) =
                &edge_after.cuts[0].moments[&MomentType::Reflectivity].storage
            else {
                panic!("F32");
            };
            let value_diffs = vb
                .iter()
                .zip(va)
                .filter(|(b, a)| b.is_finite() && a.is_finite() && b.to_bits() != a.to_bits())
                .count();
            assert_eq!(
                value_diffs, 0,
                "domain bound must not change any in-domain gate value"
            );
        }
        let (leak_min, leak_max) = leaks.iter().fold(
            (f32::INFINITY, f32::NEG_INFINITY),
            |(lo, hi), &(.., value)| (lo.min(value), hi.max(value)),
        );
        eprintln!(
            "[probe] peak echo {:.1} dBZ; domain edge {:.1} km out; \
             off-domain finite gates before={before_leak} after={after_leak} \
             (leak dBZ {leak_min:.1}..{leak_max:.1}); PNGs in {}",
            best.0,
            edge_dist_m / 1000.0,
            out_dir.display()
        );
        assert_eq!(
            after_leak, 0,
            "domain-bounded scan must not leak off-domain gates"
        );
        assert!(
            before_leak > 0,
            "unbounded LUT should show the ring this track fixes"
        );
    }

    /// Wall-time profile of the REAL synthetic-radar path (read fields + build
    /// volume) on a real wrfout. Gated on `BOWECHO_WRF_RADAR_FIXTURE`. Prints
    /// per-stage timing so we can find/verify the bottleneck. Run with:
    /// `cargo test -p app_ui --release profile_real_wrfout -- --nocapture`.
    #[test]
    fn profile_real_wrfout() {
        use std::time::Instant;
        let Some(path) = std::env::var_os("BOWECHO_WRF_RADAR_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let t0 = Instant::now();
        let file = WrfFile::open(&path).expect("open real wrfout");
        eprintln!(
            "[prof] open {:.2}s  dims {}x{}x{} nt={}",
            t0.elapsed().as_secs_f64(),
            file.nx,
            file.ny,
            file.nz,
            file.nt
        );
        let config = SyntheticRadarConfig::default();

        let tr = Instant::now();
        let fields =
            read_wrf_radar_fields(&file, 0, config.reflectivity_operator).expect("read fields");
        eprintln!(
            "[prof] read_wrf_radar_fields {:.2}s  refl_source={}",
            tr.elapsed().as_secs_f64(),
            fields.ref_source
        );

        let time = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let tb = Instant::now();
        let volume = build_synthetic_volume(&fields, time, &config);
        eprintln!(
            "[prof] build_synthetic_volume {:.2}s  cuts={} radials={}",
            tb.elapsed().as_secs_f64(),
            volume.cuts.len(),
            volume.metadata.decoded_radial_count
        );
        eprintln!("[prof] TOTAL {:.2}s", t0.elapsed().as_secs_f64());
    }

    /// The parallelized read must return BYTE-IDENTICAL fields to the original
    /// serial read (this is a speed change, not an accuracy change). Reads the
    /// four heavy fields both ways in one process and asserts every value
    /// matches bit-for-bit (NaN patterns included). Gated on the same fixture.
    #[test]
    fn parallel_read_matches_sequential_fields() {
        let Some(path) = std::env::var_os("BOWECHO_WRF_RADAR_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let file = WrfFile::open(&path).expect("open real wrfout");
        let nz = file.nz;
        let cells = file.nx * file.ny;

        // Original serial read logic (verbatim from before the parallelization).
        let seq = {
            let height = read_3d(&file, "height", 0, nz * cells).unwrap();
            let (dbz, _src) =
                read_reflectivity(&file, 0, nz * cells, ReflectivityOperator::ModelNative).unwrap();
            let (u, v) = match getvar(&file, "uvmet", Some(0), &ComputeOpts::default()) {
                Ok(uvmet) if uvmet.data.len() == 2 * nz * cells => {
                    let (ue, ve) = uvmet.data.split_at(nz * cells);
                    (to_f32(ue), to_f32(ve))
                }
                _ => {
                    let ua = read_3d(&file, "ua", 0, nz * cells).unwrap();
                    let va = read_3d(&file, "va", 0, nz * cells).unwrap();
                    (ua, va)
                }
            };
            let w = read_3d(&file, "wa", 0, nz * cells).unwrap();
            (height, dbz, u, v, w)
        };

        let par = read_wrf_radar_fields(&file, 0, ReflectivityOperator::ModelNative).unwrap();

        // Bit-identical comparison (compare raw bits so NaNs must match too).
        let same = |a: &[f32], b: &[f32]| -> bool {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
        };
        assert!(same(&seq.0, &par.height_msl), "height differs");
        assert!(same(&seq.1, &par.dbz), "dbz differs");
        assert!(same(&seq.2, &par.u), "u differs");
        assert!(same(&seq.3, &par.v), "v differs");
        assert!(same(&seq.4, &par.w), "w differs");
        eprintln!(
            "[equiv] parallel read == serial read: {} elems x 5 fields bit-identical",
            par.dbz.len()
        );
    }
}
