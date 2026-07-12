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

use chrono::{DateTime, NaiveDateTime, Utc};
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
use ui_core::geo::aeqd_inverse_km;

use app_ui::vcp_catalog::{
    BUILD_24_SOURCE, Build24Vcp, DopplerPrfValue, MomentCoverage, PhysicalScanRow, PulseLength,
    VcpDefinition, Waveform, build_24_definition,
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

/// Whether all rays represent one model instant or carry synthetic acquisition
/// times for a rotating volume scan. A timed scan still samples one WRF scene;
/// its provenance says so rather than implying temporal model interpolation.
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

/// Configuration for one synthetic scan.
#[derive(Clone, Debug)]
pub struct SyntheticRadarConfig {
    /// Named operating intent. This is also stamped in export provenance.
    pub simulation_mode: SimulationMode,
    /// Physical scan definition. The historical custom ladder remains the
    /// default; a Build 24 selection owns its rows, rates, periods, waveform,
    /// moment coverage and PRF-code provenance.
    pub scan_strategy: SyntheticScanStrategy,
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
    /// Integrate KDP/Ah/Av along each radial into PhiDP and attenuation.
    pub propagation: bool,
    /// Optional synthetic-system calibration offsets.
    pub system_phidp_deg: f32,
    pub zdr_bias_db: f32,
    /// Acquisition timing and nominal instrument cadence.
    pub scan_timing: ScanTiming,
    pub rotation_rate_deg_s: f32,
    pub transition_delay_s: f32,
    /// Pulse repetition frequency used for CfRadial metadata. Velocity folding
    /// remains controlled independently by `nyquist_mps`.
    pub prf_hz: f32,
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
}

/// Antenna height above model terrain when no explicit MSL altitude is given.
pub const DEFAULT_TOWER_M: f64 = 10.0;

impl Default for SyntheticRadarConfig {
    fn default() -> Self {
        Self {
            simulation_mode: SimulationMode::Presentation,
            scan_strategy: SyntheticScanStrategy::CustomLegacy,
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
            propagation: false,
            system_phidp_deg: 0.0,
            zdr_bias_db: 0.0,
            scan_timing: ScanTiming::InstantaneousTruth,
            rotation_rate_deg_s: 18.0,
            transition_delay_s: 3.5,
            prf_hz: 1_000.0,
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
                self.propagation = false;
                self.scan_timing = ScanTiming::InstantaneousTruth;
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
                self.propagation = true;
                self.scan_timing = ScanTiming::TimedVolume;
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
                self.propagation = false;
                self.scan_timing = ScanTiming::InstantaneousTruth;
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
        self.propagation.hash(&mut hasher);
        self.system_phidp_deg.to_bits().hash(&mut hasher);
        self.zdr_bias_db.to_bits().hash(&mut hasher);
        (self.scan_timing as u8).hash(&mut hasher);
        self.rotation_rate_deg_s.to_bits().hash(&mut hasher);
        self.transition_delay_s.to_bits().hash(&mut hasher);
        self.prf_hz.to_bits().hash(&mut hasher);
        self.instrument_noise.hash(&mut hasher);
        self.sensitivity_dbz_at_1km.to_bits().hash(&mut hasher);
        self.ref_gate_texture.hash(&mut hasher);
        self.vel_gate_texture.hash(&mut hasher);
        self.clutter_intensity.to_bits().hash(&mut hasher);
        hasher.finish()
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
    /// Compact scheme-aware polarimetric state. Eight one-byte planes retain
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
    kdp: Vec<u8>,
    ah: Vec<u8>,
    adp: Vec<i8>,
    fall_speed: Vec<u8>,
    fall_speed_std: Vec<u8>,
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
        self.kdp[index] = quantize_u8(sample.kdp_deg_km, COMPACT_KDP_STEP_DEG_KM);
        self.ah[index] = quantize_u8(sample.ah_db_km, COMPACT_ATTEN_STEP_DB_KM);
        self.adp[index] = quantize_i8(sample.ah_db_km - sample.av_db_km, COMPACT_ATTEN_STEP_DB_KM);
        self.fall_speed[index] = quantize_u8(sample.fall_speed_mps, COMPACT_FALL_STEP_MPS);
        self.fall_speed_std[index] = quantize_u8(
            sample.fall_speed_variance_m2s2.max(0.0).sqrt(),
            COMPACT_FALL_STD_STEP_MPS,
        );
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

impl WrfRadarFields {
    fn cells(&self) -> usize {
        self.nx * self.ny
    }

    /// Domain-centre grid cell (used for the default antenna position).
    fn center_cell(&self) -> usize {
        (self.ny / 2) * self.nx + (self.nx / 2)
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

    progress("reading model fields (reflectivity, winds, height)…");

    // Read the four heavy 3-D fields concurrently. Placeholders are overwritten
    // inside the scope; the scope join guarantees they are all set on exit.
    let mut height_res: Result<Vec<f32>, String> = Err("height not read".to_string());
    let mut refl_res: Result<(Vec<f32>, &'static str), String> = Err("refl not read".to_string());
    let mut winds_res: Result<(Vec<f32>, Vec<f32>), String> = Err("winds not read".to_string());
    let mut w_res: Result<Vec<f32>, String> = Err("wa not read".to_string());
    let mut terrain_m: Vec<f32> = Vec::new();
    std::thread::scope(|scope| {
        let th_height = scope.spawn(|| read_3d(file, "height", timeidx, nz * cells));
        let th_refl = scope.spawn(|| read_reflectivity(file, timeidx, nz * cells, operator));
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
    if dbz.iter().all(|value| !value.is_finite()) {
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
    timeidx: usize,
    config: &SyntheticRadarConfig,
    progress: &dyn Fn(&str),
) -> Result<WrfRadarFields, String> {
    let mut fields =
        read_wrf_radar_fields_reporting(file, timeidx, config.reflectivity_operator, progress)?;
    let expected = fields.nx * fields.ny * fields.nz;

    if config.dual_pol || config.terminal_fall_speed {
        progress("deriving scheme-aware hydrometeor scattering…");
        match build_compact_polar_fields(file, timeidx, expected, progress) {
            Ok((polar, bulk_zh_dbz)) => {
                fields.dual_pol_status = Some(format!(
                    "{} ({:?}{})",
                    polar.profile.name,
                    polar.profile.capability,
                    if polar.profile.assumption_heavy {
                        "; assumed PSD parameters"
                    } else {
                        ""
                    }
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
        .polarimetric
        .as_ref()
        .map(|polar| polar.present_fields.join(","))
        .unwrap_or_default();
    let named_vcp = config.scan_strategy.definition();
    if let Some(definition) = named_vcp {
        volume.vcp = Some(VcpInfo {
            pattern: definition.vcp.number(),
        });
    }
    let mut forward_operator_config = format!(
        "mode={:?}; reflectivity_sampling={:?}; beam_integration={:?}; \
         fall_speed={}; terrain_blockage={}; spectrum_width={}; dual_pol={}; \
         propagation={}; microphysics_fields={microphysics_inventory}",
        config.simulation_mode,
        config.reflectivity_sampling,
        config.beam_integration,
        config.terminal_fall_speed,
        config.terrain_blockage,
        config.spectrum_width,
        config.dual_pol,
        config.propagation,
    );
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
        prt_s: named_vcp
            .is_none()
            .then(|| {
                (config.prf_hz.is_finite() && config.prf_hz > 0.0).then_some(1.0 / config.prf_hz)
            })
            .flatten(),
        unambiguous_range_km: named_vcp
            .is_none()
            .then(|| {
                (config.prf_hz.is_finite() && config.prf_hz > 0.0)
                    .then_some(299_792.47 / (2.0 * config.prf_hz))
            })
            .flatten(),
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
        polarization: Some(if config.dual_pol && fields.polarimetric.is_some() {
            "simultaneous horizontal/vertical".to_string()
        } else {
            "horizontal".to_string()
        }),
        calibration: Some(format!(
            "ZDR bias={:.2} dB; system PhiDP={:.2} deg",
            config.zdr_bias_db, config.system_phidp_deg
        )),
        forward_operator: Some("BowEcho WRF polar-volume forward operator v2".to_string()),
        forward_operator_config: Some(forward_operator_config),
        source_model: Some("WRF".to_string()),
        microphysics_scheme: fields
            .polarimetric
            .as_ref()
            .map(|polar| format!("{} ({:?})", polar.profile.name, polar.profile.capability)),
        scattering_model: (config.dual_pol && fields.polarimetric.is_some()).then(|| {
            format!(
                "{} (scheme-aware bulk S-band Rayleigh; not T-matrix)",
                crate::wrf_radar_physics::bulk_sband_model_id()
            )
        }),
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
        );
        decoded_radials += cut.radials.len();
        volume.cuts.push(cut);
        if matches!(config.scan_timing, ScanTiming::TimedVolume) {
            let (sweep_ms, transition_ms) = if named_vcp.is_some() {
                (
                    1_000.0 * leg.source_period_seconds.max(0.0),
                    1_000.0 * leg.transition_after_seconds.max(0.0),
                )
            } else {
                // Preserve the historical custom-mode arithmetic order: the
                // f32 rounding of `360_000 / rate` is part of existing ray
                // timestamps and therefore of the bit-for-bit default contract.
                (
                    360_000.0 / config.rotation_rate_deg_s.max(0.1),
                    1_000.0 * config.transition_delay_s.max(0.0),
                )
            };
            cut_start_ms = (f64::from(cut_start_ms) + f64::from(sweep_ms + transition_ms))
                .round()
                .clamp(0.0, f64::from(i32::MAX)) as i32;
        }
    }
    volume.metadata.decoded_radial_count = decoded_radials;
    volume
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
                    let slant_m = gate as f64 * spacing_m;
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

#[allow(clippy::too_many_arguments)]
fn sample_gate(
    fields: &WrfRadarFields,
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
) -> Option<GatePhysicalSample> {
    let beam_sigma_deg = f64::from(config.beam_width_deg.max(0.05)) / (2.0 * (2.0_f64.ln()).sqrt());
    let points = quadrature_points(config.beam_integration);
    let mut valid_weight = 0.0f64;
    let mut signal_weight = 0.0f64;
    let mut sum_z = 0.0f64;
    let mut sum_z_vr = 0.0f64;
    let mut sum_z_vr2 = 0.0f64;
    let mut sum_z_subgrid_variance = 0.0f64;
    let mut polar_accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
    let mut any_polar = false;

    for point in points {
        let azimuth_deg = center_azimuth_deg + point.az_sigma * beam_sigma_deg;
        let elevation_deg = center_elevation_deg + point.el_sigma * beam_sigma_deg;
        let slant_m = (center_slant_m + point.range_gate * spacing_m).max(0.0);
        let beam_height_m = beam_height_above_radar_m(slant_m, elevation_deg);
        let z_msl = antenna_msl + beam_height_m;
        let ground_m = beam_ground_range_m(slant_m, elevation_deg);
        let azimuth = azimuth_deg.to_radians();
        let east_km = ground_m * azimuth.sin() / 1_000.0;
        let north_km = ground_m * azimuth.cos() / 1_000.0;
        let (lat, lon) = aeqd_inverse_km(site_lat, site_lon, east_km, north_km);
        let Some(sample) = sample_column(
            fields,
            cells,
            lat as f32,
            lon as f32,
            z_msl as f32,
            config.reflectivity_sampling,
        ) else {
            continue;
        };
        valid_weight += point.weight;

        let blocked = terrain_horizon.is_some_and(|horizon| {
            let horizon_deg = horizon.at(azimuth_deg, gate_index);
            horizon_deg.is_finite() && elevation_deg as f32 <= horizon_deg
        });
        if blocked {
            continue;
        }

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
            continue;
        }
        signal_weight += point.weight;
        sum_z += point.weight * z;
        sum_z_vr += point.weight * z * vr;
        sum_z_vr2 += point.weight * z * vr * vr;
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
        sum_z_subgrid_variance += point.weight * z * (terminal_variance + turbulent_variance);
        if let Some(polar) = sample.polar {
            any_polar = true;
            polar_accumulator.add(point.weight as f32, intrinsic_as_contribution(polar));
        }
    }

    if valid_weight <= 0.0 || signal_weight <= 0.0 || sum_z <= 0.0 {
        return None;
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
    Some(GatePhysicalSample {
        z_linear,
        velocity_mps: velocity as f32,
        spectrum_width_mps: (variance + floor * floor).sqrt() as f32,
        polar: polar.take(),
    })
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
}

impl CutMomentRow {
    fn blank(gates: usize) -> Self {
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
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_cut(
    fields: &WrfRadarFields,
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
) -> ElevationCut {
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

    // One row per radial, sampled in parallel. Each row is `gate_count` REF and
    // `gate_count` VEL f32 values (NaN = no data / below floor / off-domain).
    let rows: Vec<CutMomentRow> = (0..naz)
        .into_par_iter()
        .map(|iaz| {
            let az_deg = iaz as f64 * 360.0 / naz as f64;
            let mut row = CutMomentRow::blank(gate_count);
            let dr_km = (spacing / 1_000.0).max(0.0) as f32;
            let mut previous_kdp = 0.0f32;
            let mut previous_ah = 0.0f32;
            let mut previous_adp = 0.0f32;
            let mut phi_path = 0.0f32;
            let mut tau_h = 0.0f32;
            let mut tau_dp = 0.0f32;
            for gate in 0..gate_count {
                let slant_m = gate as f64 * spacing;
                // Doviak & Zrnić (1993) eq. 2.28b/c under the 4/3-earth model.
                let beam_height_m = beam_height_above_radar_m(slant_m, elevation_deg);
                let ground_m = beam_ground_range_m(slant_m, elevation_deg);
                let Some(sample) = sample_gate(
                    fields,
                    cells,
                    site_lat,
                    site_lon,
                    antenna_msl,
                    az_deg,
                    elevation_deg,
                    slant_m,
                    gate,
                    spacing,
                    config,
                    terrain_horizon,
                ) else {
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
            row
        })
        .collect();

    let mut cut = ElevationCut::new(elevation_deg as f32, u8::try_from(cut_index + 1).ok());
    let mut ref_values = Vec::with_capacity(naz * gate_count);
    let mut vel_values = Vec::with_capacity(naz * gate_count);
    let mut sw_values = Vec::with_capacity(naz * gate_count);
    let mut zdr_values = Vec::with_capacity(naz * gate_count);
    let mut rho_values = Vec::with_capacity(naz * gate_count);
    let mut phi_values = Vec::with_capacity(naz * gate_count);
    let mut kdp_values = Vec::with_capacity(naz * gate_count);
    let mut ah_values = Vec::with_capacity(naz * gate_count);
    let mut pia_values = Vec::with_capacity(naz * gate_count);
    let mut refc_values = Vec::with_capacity(naz * gate_count);
    let mut adp_values = Vec::with_capacity(naz * gate_count);
    let mut pida_values = Vec::with_capacity(naz * gate_count);
    let mut zdrc_values = Vec::with_capacity(naz * gate_count);
    for (iaz, row) in rows.into_iter().enumerate() {
        let az_deg = iaz as f32 * 360.0 / naz as f32;
        let time_offset_ms = match config.scan_timing {
            ScanTiming::InstantaneousTruth => 0,
            ScanTiming::TimedVolume => {
                let radial_ms = 1_000.0 * az_deg / scan_leg.azimuth_rate_deg_per_second.max(0.1);
                cut_start_ms.saturating_add(radial_ms.round() as i32)
            }
        };
        cut.radials.push(Radial {
            azimuth_deg: az_deg,
            elevation_deg: elevation_deg as f32,
            time_offset_ms,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: Some(config.stamped_nyquist_mps()),
            radial_status: Some(if iaz == 0 && cut_index == 0 {
                radar_core::RadialStatus::StartVolume
            } else if iaz == 0 {
                radar_core::RadialStatus::StartElevation
            } else if iaz + 1 == naz && cut_index + 1 == scan_leg_count {
                radar_core::RadialStatus::EndVolume
            } else if iaz + 1 == naz {
                radar_core::RadialStatus::EndElevation
            } else {
                radar_core::RadialStatus::Intermediate
            }),
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
    }

    let radial_indices: Vec<usize> = (0..naz).collect();
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
    if config.dual_pol && fields.polarimetric.is_some() && scan_leg.moments.has_reflectivity() {
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
    cut
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
    reflectivity_sampling: ReflectivitySampling,
) -> Option<ColumnSample> {
    let stencil = horizontal_stencil(fields, lat, lon)?;

    let mut wsum = 0.0f32;
    let mut reflectivity = 0.0f32;
    let mut u = 0.0f32;
    let mut v = 0.0f32;
    let mut w = 0.0f32;
    let mut tke = 0.0f32;
    let mut polar_accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
    for (col, weight) in stencil {
        if weight <= 0.0 {
            continue;
        }
        let Some((k, t)) = bracket_height(fields, cells, col, z_msl) else {
            continue;
        };
        let i0 = k * cells + col;
        let i1 = (k + 1) * cells + col;
        let Some(d) = (match reflectivity_sampling {
            ReflectivitySampling::LegacyDbz => lerp(fields.dbz[i0], fields.dbz[i1], t),
            ReflectivitySampling::LinearZ => {
                lerp(dbz_to_z(fields.dbz[i0]), dbz_to_z(fields.dbz[i1]), t)
            }
        }) else {
            continue;
        };
        let (Some(su), Some(sv), Some(sw)) = (
            lerp(fields.u[i0], fields.u[i1], t),
            lerp(fields.v[i0], fields.v[i1], t),
            lerp(fields.w[i0], fields.w[i1], t),
        ) else {
            continue;
        };
        if let Some(polar) = &fields.polarimetric {
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
        reflectivity += weight * d;
        u += weight * su;
        v += weight * sv;
        w += weight * sw;
    }
    if wsum <= 0.0 {
        return None;
    }
    let reflectivity = reflectivity / wsum;
    let z_linear = match reflectivity_sampling {
        ReflectivitySampling::LegacyDbz => dbz_to_z(reflectivity),
        ReflectivitySampling::LinearZ => reflectivity,
    };
    let polar = fields.polarimetric.as_ref().and_then(|_| {
        let sample = normalize_intrinsic(polar_accumulator.finalize(), wsum);
        (sample.zh > 0.0).then_some(sample)
    });
    Some(ColumnSample {
        z_linear: polar.map_or(z_linear, |sample| sample.zh),
        u: u / wsum,
        v: v / wsum,
        w: w / wsum,
        polar,
        tke_m2s2: tke / wsum,
    })
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
    /// [`SyntheticRadarConfig::data_fingerprint`] of the config that built these
    /// volumes. The install path folds it into each frame's history path key so
    /// a re-import with CHANGED settings deduplicates as a distinct build
    /// (replaces the stale volume) while an unchanged re-import reuses.
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

/// Spawn a worker that turns each forecast time of the given wrfout file(s)
/// into a simulated [`RadarVolume`]. Streams progress, then a `Done`.
pub fn spawn_synthetic_radar(
    paths: Vec<PathBuf>,
    config: SyntheticRadarConfig,
) -> SyntheticRadarTask {
    let label = if paths.len() == 1 {
        format!("Simulated radar from {}", display_name(&paths[0]))
    } else {
        format!("Simulated radar from {} WRF files", paths.len())
    };
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
    if paths.is_empty() {
        return Err("No WRF files selected".to_string());
    }
    let mut files: Vec<PathBuf> = paths
        .iter()
        .filter(|path| crate::wrf_process::is_supported_wrf_file(path))
        .cloned()
        .collect();
    // Multi-select / folder picks arrive in arbitrary order: sort by the WRF
    // valid time parsed from the filename so the loop plays in model time.
    files.sort_by_cached_key(|path| wrf_time_sort_key(path));
    if files.is_empty() {
        return Err("No supported WRF files selected".to_string());
    }

    let mut volumes = Vec::new();
    let mut notes = Vec::new();
    let mut fallback_index = 0u32;
    let file_total = files.len();
    for (file_index, path) in files.iter().enumerate() {
        let _ = tx.send(SyntheticRadarMessage::Progress(format!(
            "Opening WRF {}",
            display_name(path)
        )));
        let file = match WrfFile::open(path) {
            Ok(file) => file,
            Err(err) => {
                notes.push(format!("Open {} failed: {err}", display_name(path)));
                continue;
            }
        };
        let times = file.times().unwrap_or_default();
        let name = display_name(path);
        let nt = file.nt;
        for timeidx in 0..nt {
            // Stream fine-grained stage labels for this frame so the UI shows
            // steady progress instead of a multi-second (or, in a debug build,
            // multi-minute) freeze with no feedback. Multi-file loops lead
            // with "file 2/5: …" so the owner sees the loop building.
            let frame_prefix = match (file_total > 1, nt > 1) {
                (true, true) => format!(
                    "file {}/{file_total} ({name}, time {}/{nt}): ",
                    file_index + 1,
                    timeidx + 1
                ),
                (true, false) => format!("file {}/{file_total} ({name}): ", file_index + 1),
                (false, true) => format!("Simulating {name} (time {}/{nt}): ", timeidx + 1),
                (false, false) => format!("Simulating {name}: "),
            };
            let progress = |stage: &str| {
                let _ = tx.send(SyntheticRadarMessage::Progress(format!(
                    "{frame_prefix}{stage}"
                )));
            };
            progress("reading…");
            let fields =
                match read_wrf_radar_fields_for_config_reporting(&file, timeidx, config, &progress)
                {
                    Ok(fields) => fields,
                    Err(err) => {
                        notes.push(format!("{name} time {timeidx}: {err}"));
                        continue;
                    }
                };
            let valid_time = times
                .get(timeidx)
                .and_then(|raw| parse_wrf_time(raw))
                .unwrap_or_else(|| {
                    // No parsable Times entry — keep frames distinct so the
                    // loop engine does not collapse them into one identity.
                    let base = DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is valid")
                        + chrono::Duration::hours(i64::from(fallback_index));
                    fallback_index += 1;
                    base
                });
            let volume = build_synthetic_volume_reporting(&fields, valid_time, config, &progress);
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
                "{name} time {timeidx}: {} radials from {}{gate_note}{polar_note}",
                volume.metadata.decoded_radial_count, fields.ref_source,
            ));
            volumes.push(Arc::new(volume));
        }
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
    // The loop keys on the volume's own scan time: sort on it so frames play
    // in valid-time order even when a filename stamp and the file's internal
    // `Times` disagree (stable — equal times keep the filename order).
    volumes.sort_by_key(|volume| volume.volume_time);
    Ok(SyntheticRadarOutput {
        label: label.to_string(),
        volumes,
        notes,
        config_fingerprint: config.data_fingerprint(),
    })
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

/// Multi-file loop ordering key: the WRF valid time parsed from the filename
/// (`wrfout_d03_2025-06-21_01_30_00` → `"20250621013000"`), then the bare
/// filename as the tiebreak/fallback — so a shuffled multi-select or a folder
/// pick always builds (and reports "file k/n" progress) in model-time order.
/// Files without a parsable stamp sort together by name, ahead of nothing.
fn wrf_time_sort_key(path: &Path) -> (String, String) {
    let name = display_name(path);
    (
        crate::wrf_process::parse_wrf_timestamp(&name).unwrap_or_default(),
        name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_file_sort_key_orders_by_parsed_wrf_time() {
        // Shuffled pick order (what rfd multi-select hands back) must sort by
        // the model time embedded in the name, not the pick order. Paths are
        // built with join so the directory strips on every OS (a literal
        // `C:\run\...` is ONE component on Linux and broke the CI gate).
        let mut paths = [
            PathBuf::from("run").join("wrfout_d03_2025-06-21_02_15_00"),
            PathBuf::from("run").join("wrfout_d03_2025-06-21_01_30_00"),
            PathBuf::from("run").join("wrfout_d03_2025-06-21_02_30_00"),
            PathBuf::from("run").join("wrfout_d03_2025-06-21_02_00_00"),
            PathBuf::from("run").join("wrfout_d03_2025-06-21_01_45_00"),
        ];
        paths.sort_by_cached_key(|path| wrf_time_sort_key(path));
        let names: Vec<String> = paths.iter().map(|path| display_name(path)).collect();
        assert_eq!(
            names,
            vec![
                "wrfout_d03_2025-06-21_01_30_00",
                "wrfout_d03_2025-06-21_01_45_00",
                "wrfout_d03_2025-06-21_02_00_00",
                "wrfout_d03_2025-06-21_02_15_00",
                "wrfout_d03_2025-06-21_02_30_00",
            ]
        );

        // Colon-form stamps sort identically; unstamped names fall back to
        // filename order without panicking.
        let colon = wrf_time_sort_key(Path::new("wrfout_d02_2025-06-21_01:45:00"));
        let underscore = wrf_time_sort_key(Path::new("wrfout_d03_2025-06-21_01_45_00"));
        assert_eq!(colon.0, underscore.0);
        assert_eq!(colon.0, "20250621014500");
        let plain = wrf_time_sort_key(Path::new("some_model_output.nc"));
        assert!(plain.0.is_empty());
        assert_eq!(plain.1, "some_model_output.nc");
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
            polarimetric: None,
            dual_pol_status: None,
            tke_tenths_m2s2: None,
            ref_source: "test",
            dx_m: None,
            lut,
        }
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
            ReflectivitySampling::LegacyDbz,
        )
        .expect("legacy sample");
        let linear = sample_column(
            &fields,
            fields.cells(),
            39.0,
            -95.0,
            1_000.0,
            ReflectivitySampling::LinearZ,
        )
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
        .expect("air-motion gate");
        config.terminal_fall_speed = true;
        let scatterer = sample_gate(
            &fields,
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
        .expect("scatterer-motion gate");
        assert!((scatterer.velocity_mps - (air.velocity_mps - 2.5)).abs() < 0.08);
    }

    #[test]
    fn terrain_horizon_blocks_downstream_low_tilt() {
        let mut fields = uniform_box_fields();
        fields.terrain_m.fill(1_500.0);
        let horizon = TerrainHorizon::build(&fields, 39.0, -95.0, 200.0, 36, 20, 500.0);
        let config = SyntheticRadarConfig {
            terrain_blockage: true,
            ref_gate_texture: false,
            ..SyntheticRadarConfig::default()
        };
        let blocked = sample_gate(
            &fields,
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
        );
        assert!(
            blocked.is_none(),
            "a 1.5-km ridge must block the 0.5-degree beam"
        );
        assert!(
            sample_gate(
                &fields,
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
        assert!(differs(&|c| c.propagation = true), "propagation");
        assert!(differs(&|c| c.system_phidp_deg = 11.0), "system_phidp_deg");
        assert!(differs(&|c| c.zdr_bias_db = 0.3), "zdr_bias_db");
        assert!(
            differs(&|c| c.scan_timing = ScanTiming::TimedVolume),
            "scan_timing"
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
        assert!(differs(&|c| c.instrument_noise = true), "instrument_noise");
        assert!(
            differs(&|c| c.sensitivity_dbz_at_1km = -38.0),
            "sensitivity_dbz_at_1km"
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
