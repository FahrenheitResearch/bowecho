//! Pure instrument, estimator, selected-gate spectrum, explanation, and
//! algorithm-truth primitives for simulated radar.
//!
//! This module deliberately owns no WRF I/O, egui state, or `RadarVolume`
//! mutation. The forward operator can adopt each contract incrementally while
//! preserving one auditable boundary between ideal scattering, a measured
//! virtual instrument, and optional presentation effects.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::error::Error;
use std::f64::consts::{LN_10, PI, TAU};
use std::fmt;

/// Exact SI speed of light in vacuum.
pub const SPEED_OF_LIGHT_MPS: f64 = 299_792_458.0;

/// Coarse microwave-band identity derived from an exact transmit frequency.
/// The exact frequency remains on [`RadarInstrument`] and is always what the
/// physical calculations consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadarBand {
    S,
    C,
    X,
    Other,
}

impl RadarBand {
    pub fn from_frequency_hz(frequency_hz: f64) -> Result<Self, InstrumentError> {
        validate_positive_finite("radar frequency", frequency_hz)?;
        Ok(if (2.0e9..4.0e9).contains(&frequency_hz) {
            Self::S
        } else if (4.0e9..8.0e9).contains(&frequency_hz) {
            Self::C
        } else if (8.0e9..12.0e9).contains(&frequency_hz) {
            Self::X
        } else {
            Self::Other
        })
    }
}

/// Versionable physical instrument identity. No display label or band
/// rounding participates in wavelength/PRF calculations.
#[derive(Clone, Debug, PartialEq)]
pub struct RadarInstrument {
    pub name: String,
    pub band: RadarBand,
    pub frequency_hz: f64,
    pub pulse_width_s: f64,
}

impl RadarInstrument {
    pub fn new(
        name: impl Into<String>,
        frequency_hz: f64,
        pulse_width_s: f64,
    ) -> Result<Self, InstrumentError> {
        validate_positive_finite("radar frequency", frequency_hz)?;
        validate_positive_finite("pulse width", pulse_width_s)?;
        let name = name.into();
        if name.trim().is_empty() {
            return Err(InstrumentError::EmptyName);
        }
        Ok(Self {
            name,
            band: RadarBand::from_frequency_hz(frequency_hz)?,
            frequency_hz,
            pulse_width_s,
        })
    }

    pub fn wavelength_m(&self) -> f64 {
        SPEED_OF_LIGHT_MPS / self.frequency_hz
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum InstrumentError {
    EmptyName,
    InvalidValue { field: &'static str, value: f64 },
    NamedVcpPrfCodeUnresolved { vcp: u16, code: u8 },
}

impl fmt::Display for InstrumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => write!(f, "radar instrument name is empty"),
            Self::InvalidValue { field, value } => {
                write!(f, "{field} must be finite and positive, got {value}")
            }
            Self::NamedVcpPrfCodeUnresolved { vcp, code } => write!(
                f,
                "Build-qualified VCP {vcp} PRF code {code} is not a frequency; an authoritative code-to-Hz source is required"
            ),
        }
    }
}

impl Error for InstrumentError {}

fn validate_positive_finite(field: &'static str, value: f64) -> Result<(), InstrumentError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(InstrumentError::InvalidValue { field, value })
    }
}

/// A literal user/research PRF in hertz. This is intentionally a separate
/// type from a source-document PRF code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CustomSinglePrf {
    pub prf_hz: f64,
}

impl CustomSinglePrf {
    pub fn new(prf_hz: f64) -> Result<Self, InstrumentError> {
        validate_positive_finite("custom PRF", prf_hz)?;
        Ok(Self { prf_hz })
    }
}

/// Input accepted at the fail-closed resolution boundary. A named-VCP code is
/// carried so callers can receive an explicit error; it is never cast to Hz.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PrfSpecification {
    CustomSinglePrf(CustomSinglePrf),
    NamedVcpCode { vcp: u16, code: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedSinglePrf {
    pub frequency_hz: f64,
    pub wavelength_m: f64,
    pub prf_hz: f64,
    pub prt_s: f64,
    pub nyquist_velocity_mps: f64,
    pub unambiguous_range_m: f64,
}

/// Couple frequency and a literal custom single PRF through the same physical
/// contract used by folding, range ambiguity, and estimator uncertainty.
pub fn resolve_prf(
    instrument: &RadarInstrument,
    specification: PrfSpecification,
) -> Result<ResolvedSinglePrf, InstrumentError> {
    let custom = match specification {
        PrfSpecification::CustomSinglePrf(custom) => custom,
        PrfSpecification::NamedVcpCode { vcp, code } => {
            return Err(InstrumentError::NamedVcpPrfCodeUnresolved { vcp, code });
        }
    };
    validate_positive_finite("radar frequency", instrument.frequency_hz)?;
    validate_positive_finite("custom PRF", custom.prf_hz)?;
    let wavelength_m = SPEED_OF_LIGHT_MPS / instrument.frequency_hz;
    Ok(ResolvedSinglePrf {
        frequency_hz: instrument.frequency_hz,
        wavelength_m,
        prf_hz: custom.prf_hz,
        prt_s: 1.0 / custom.prf_hz,
        nyquist_velocity_mps: wavelength_m * custom.prf_hz / 4.0,
        unambiguous_range_m: SPEED_OF_LIGHT_MPS / (2.0 * custom.prf_hz),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeQuadratureSample {
    pub offset_m: f64,
    pub weight: f64,
}

/// Normalized triangular matched-filter range response for a rectangular
/// transmitted pulse. The response support is `+/- c*tau/2`; quadrature
/// offsets depend only on pulse width, never gate spacing.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchedFilterRangeResponse {
    pub pulse_width_s: f64,
    pub range_resolution_m: f64,
    samples: Vec<RangeQuadratureSample>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PulseResponseError {
    InvalidPulseWidth(f64),
    InvalidSampleCount(usize),
}

impl fmt::Display for PulseResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPulseWidth(value) => {
                write!(f, "pulse width must be finite and positive, got {value}")
            }
            Self::InvalidSampleCount(value) => write!(
                f,
                "matched-filter quadrature needs an odd sample count in 3..=129, got {value}"
            ),
        }
    }
}

impl Error for PulseResponseError {}

impl MatchedFilterRangeResponse {
    pub fn new(pulse_width_s: f64, sample_count: usize) -> Result<Self, PulseResponseError> {
        if !pulse_width_s.is_finite() || pulse_width_s <= 0.0 {
            return Err(PulseResponseError::InvalidPulseWidth(pulse_width_s));
        }
        if !(3..=129).contains(&sample_count) || sample_count.is_multiple_of(2) {
            return Err(PulseResponseError::InvalidSampleCount(sample_count));
        }
        let range_resolution_m = SPEED_OF_LIGHT_MPS * pulse_width_s / 2.0;
        let cell_width = 2.0 * range_resolution_m / sample_count as f64;
        let mut samples = Vec::with_capacity(sample_count);
        let mut weight_sum = 0.0;
        for index in 0..sample_count {
            let offset_m = -range_resolution_m + (index as f64 + 0.5) * cell_width;
            let response = (1.0 - offset_m.abs() / range_resolution_m).max(0.0);
            weight_sum += response;
            samples.push(RangeQuadratureSample {
                offset_m,
                weight: response,
            });
        }
        for sample in &mut samples {
            sample.weight /= weight_sum;
        }
        Ok(Self {
            pulse_width_s,
            range_resolution_m,
            samples,
        })
    }

    pub fn samples(&self) -> &[RangeQuadratureSample] {
        &self.samples
    }

    /// Continuous normalized response density in inverse metres.
    pub fn weight_density_at(&self, offset_m: f64) -> f64 {
        if !offset_m.is_finite() || offset_m.abs() >= self.range_resolution_m {
            return 0.0;
        }
        (1.0 - offset_m.abs() / self.range_resolution_m) / self.range_resolution_m
    }
}

/// Common physical moment payload. `None` is the only missing/censored
/// representation; finite values remain in declared physical units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RadarMomentValues {
    pub reflectivity_dbz: Option<f64>,
    pub velocity_mps: Option<f64>,
    pub spectrum_width_mps: Option<f64>,
    pub zdr_db: Option<f64>,
    pub rho_hv: Option<f64>,
    pub kdp_deg_km: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IdealMoments {
    pub values: RadarMomentValues,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MomentUncertainty {
    pub reflectivity_sigma_db: f64,
    pub velocity_sigma_mps: f64,
    pub spectrum_width_sigma_mps: f64,
    pub zdr_sigma_db: f64,
    pub rho_hv_sigma: f64,
    pub kdp_sigma_deg_km: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MomentBias {
    pub reflectivity_db: f64,
    pub velocity_mps: f64,
    pub spectrum_width_mps: f64,
    pub zdr_db: f64,
    pub rho_hv: f64,
    pub kdp_deg_km: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MomentNoiseDraw {
    pub reflectivity_standard_normal: f64,
    pub velocity_standard_normal: f64,
    pub spectrum_width_standard_normal: f64,
    pub zdr_standard_normal: f64,
    pub rho_hv_standard_normal: f64,
    pub kdp_standard_normal: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResolvedEstimatorSampling {
    pub dwell_s: f64,
    pub transmitted_pulses: u32,
    pub independent_samples: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeasuredMoments {
    pub values: RadarMomentValues,
    pub sensitivity_dbz: Option<f64>,
    pub snr_db: Option<f64>,
    pub censored: bool,
    pub sampling: ResolvedEstimatorSampling,
    pub uncertainty: MomentUncertainty,
    pub bias: MomentBias,
    pub noise: MomentNoiseDraw,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PresentationAdjustment {
    pub reflectivity_db: f64,
    pub velocity_mps: f64,
    pub zdr_db: f64,
    pub clutter_replaced: bool,
    pub threshold_censored: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PresentedMoments {
    pub values: RadarMomentValues,
    pub adjustment: PresentationAdjustment,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MomentEstimatorConfig {
    pub dwell_s: f64,
    /// Explicit transmitted pulse count. `None` derives floor(dwell * PRF).
    pub pulse_count: Option<u32>,
    /// Accounts for temporal correlation between transmitted pulses.
    pub independent_sample_fraction: f64,
    pub sensitivity_dbz_at_1km: f64,
    pub minimum_snr_db: f64,
    pub zdr_system_bias_db: f64,
    /// Baseline used by the selected-gate KDP finite-difference estimator.
    pub kdp_baseline_km: f64,
}

impl Default for MomentEstimatorConfig {
    fn default() -> Self {
        Self {
            dwell_s: 0.05,
            pulse_count: None,
            independent_sample_fraction: 0.5,
            sensitivity_dbz_at_1km: -40.0,
            minimum_snr_db: 0.0,
            zdr_system_bias_db: 0.0,
            kdp_baseline_km: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoiseKey {
    pub seed: u64,
    pub frame: u32,
    pub cut: u16,
    pub ray: u32,
    pub gate: u32,
}

impl NoiseKey {
    fn stream_seed(self, stream: u64) -> u64 {
        let mut value = self.seed ^ stream.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        value ^= u64::from(self.frame).rotate_left(7);
        value ^= u64::from(self.cut).rotate_left(19);
        value ^= u64::from(self.ray).rotate_left(31);
        value ^= u64::from(self.gate).rotate_left(47);
        splitmix64(value)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EstimatorError {
    InvalidConfig { field: &'static str, value: f64 },
    InstrumentTimingMismatch,
    ZeroPulseCount,
    PulseCountOverflow,
}

impl fmt::Display for EstimatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig { field, value } => {
                write!(f, "estimator {field} is invalid: {value}")
            }
            Self::InstrumentTimingMismatch => write!(
                f,
                "resolved PRF timing does not match the exact radar instrument frequency"
            ),
            Self::ZeroPulseCount => write!(f, "estimator dwell/PRF resolves to zero pulses"),
            Self::PulseCountOverflow => write!(f, "estimator pulse count exceeds u32"),
        }
    }
}

impl Error for EstimatorError {}

pub fn resolve_estimator_sampling(
    timing: &ResolvedSinglePrf,
    config: &MomentEstimatorConfig,
) -> Result<ResolvedEstimatorSampling, EstimatorError> {
    if !timing.prf_hz.is_finite() || timing.prf_hz <= 0.0 {
        return Err(EstimatorError::InvalidConfig {
            field: "resolved_prf_hz",
            value: timing.prf_hz,
        });
    }
    if !config.dwell_s.is_finite() || config.dwell_s <= 0.0 {
        return Err(EstimatorError::InvalidConfig {
            field: "dwell_s",
            value: config.dwell_s,
        });
    }
    if !config.independent_sample_fraction.is_finite()
        || !(0.0..=1.0).contains(&config.independent_sample_fraction)
        || config.independent_sample_fraction == 0.0
    {
        return Err(EstimatorError::InvalidConfig {
            field: "independent_sample_fraction",
            value: config.independent_sample_fraction,
        });
    }
    let transmitted_pulses = if let Some(count) = config.pulse_count {
        if count == 0 {
            return Err(EstimatorError::ZeroPulseCount);
        }
        count
    } else {
        let count = (config.dwell_s * timing.prf_hz).floor();
        if !count.is_finite() || count > f64::from(u32::MAX) {
            return Err(EstimatorError::PulseCountOverflow);
        }
        let count = count as u32;
        if count == 0 {
            return Err(EstimatorError::ZeroPulseCount);
        }
        count
    };
    Ok(ResolvedEstimatorSampling {
        dwell_s: config.dwell_s,
        transmitted_pulses,
        independent_samples: (f64::from(transmitted_pulses) * config.independent_sample_fraction)
            .max(1.0),
    })
}

/// Apply a deterministic, SNR- and sample-count-dependent moment estimator.
/// The returned uncertainty and bias terms are the exact values used to
/// produce `values`, making selected-gate explanations reproducible.
pub fn estimate_measured_moments(
    ideal: IdealMoments,
    instrument: &RadarInstrument,
    timing: &ResolvedSinglePrf,
    config: &MomentEstimatorConfig,
    range_m: f64,
    noise_key: NoiseKey,
) -> Result<MeasuredMoments, EstimatorError> {
    if timing.frequency_hz.to_bits() != instrument.frequency_hz.to_bits() {
        return Err(EstimatorError::InstrumentTimingMismatch);
    }
    for (field, value, positive) in [
        ("range_m", range_m, false),
        (
            "sensitivity_dbz_at_1km",
            config.sensitivity_dbz_at_1km,
            false,
        ),
        ("minimum_snr_db", config.minimum_snr_db, false),
        ("kdp_baseline_km", config.kdp_baseline_km, true),
    ] {
        if !value.is_finite()
            || (positive && value <= 0.0)
            || (!positive && field == "range_m" && value < 0.0)
        {
            return Err(EstimatorError::InvalidConfig { field, value });
        }
    }
    let sampling = resolve_estimator_sampling(timing, config)?;
    let Some(reflectivity_dbz) = ideal
        .values
        .reflectivity_dbz
        .filter(|value| value.is_finite())
    else {
        return Ok(MeasuredMoments {
            sampling,
            censored: true,
            ..MeasuredMoments::default()
        });
    };
    let range_km = (range_m / 1_000.0).max(1.0);
    let sensitivity_dbz = config.sensitivity_dbz_at_1km + 20.0 * range_km.log10();
    let snr_db = reflectivity_dbz - sensitivity_dbz;
    if snr_db < config.minimum_snr_db {
        return Ok(MeasuredMoments {
            sensitivity_dbz: Some(sensitivity_dbz),
            snr_db: Some(snr_db),
            censored: true,
            sampling,
            ..MeasuredMoments::default()
        });
    }

    let snr_linear = 10.0f64.powf(snr_db / 10.0).max(1.0e-12);
    let n = sampling.independent_samples;
    let relative_power_sigma = (1.0 + 1.0 / snr_linear) / n.sqrt();
    let reflectivity_sigma_db = (10.0 / LN_10) * relative_power_sigma;
    let rho_truth = ideal.values.rho_hv.unwrap_or(1.0).clamp(0.05, 1.0);
    let phase_sigma_rad = ((1.0 + 1.0 / snr_linear) / (2.0 * n * rho_truth.powi(2))).sqrt();
    let velocity_sigma_mps = timing.wavelength_m * phase_sigma_rad / (4.0 * PI * timing.prt_s);
    let ideal_width = ideal.values.spectrum_width_mps.unwrap_or(0.0).max(0.0);
    let noisy_width = (ideal_width.powi(2) + velocity_sigma_mps.powi(2)).sqrt();
    let spectrum_width_sigma_mps = noisy_width / (2.0 * n).sqrt();
    let zdr_sigma_db = reflectivity_sigma_db * (2.0 - rho_truth).sqrt();
    let rho_hv_sigma =
        ((1.0 - rho_truth.powi(2)).max(0.0025) / (2.0 * n).sqrt()) * (1.0 + 1.0 / snr_linear);
    let kdp_sigma_deg_km =
        phase_sigma_rad.to_degrees() / (2.0_f64.sqrt() * 2.0 * config.kdp_baseline_km);
    let uncertainty = MomentUncertainty {
        reflectivity_sigma_db,
        velocity_sigma_mps,
        spectrum_width_sigma_mps,
        zdr_sigma_db,
        rho_hv_sigma,
        kdp_sigma_deg_km,
    };
    let relative_power_variance = relative_power_sigma.powi(2);
    let rho_bias = -rho_truth / (snr_linear + 1.0);
    let bias = MomentBias {
        reflectivity_db: -0.5 * (10.0 / LN_10) * relative_power_variance,
        velocity_mps: 0.0,
        spectrum_width_mps: noisy_width - ideal_width,
        zdr_db: config.zdr_system_bias_db,
        rho_hv: rho_bias,
        kdp_deg_km: 0.0,
    };
    let noise = MomentNoiseDraw {
        reflectivity_standard_normal: standard_normal(noise_key, 1),
        velocity_standard_normal: standard_normal(noise_key, 2),
        spectrum_width_standard_normal: standard_normal(noise_key, 3),
        zdr_standard_normal: standard_normal(noise_key, 4),
        rho_hv_standard_normal: standard_normal(noise_key, 5),
        kdp_standard_normal: standard_normal(noise_key, 6),
    };

    let velocity = ideal
        .values
        .velocity_mps
        .filter(|value| value.is_finite())
        .map(|value| {
            fold_velocity_f64(
                value + bias.velocity_mps + velocity_sigma_mps * noise.velocity_standard_normal,
                timing.nyquist_velocity_mps,
            )
        });
    let values = RadarMomentValues {
        reflectivity_dbz: Some(
            reflectivity_dbz
                + bias.reflectivity_db
                + reflectivity_sigma_db * noise.reflectivity_standard_normal,
        ),
        velocity_mps: velocity,
        spectrum_width_mps: ideal
            .values
            .spectrum_width_mps
            .filter(|value| value.is_finite())
            .map(|value| {
                (value
                    + bias.spectrum_width_mps
                    + spectrum_width_sigma_mps * noise.spectrum_width_standard_normal)
                    .max(0.0)
            }),
        zdr_db: ideal
            .values
            .zdr_db
            .filter(|value| value.is_finite())
            .map(|value| value + bias.zdr_db + zdr_sigma_db * noise.zdr_standard_normal),
        rho_hv: ideal
            .values
            .rho_hv
            .filter(|value| value.is_finite())
            .map(|value| {
                (value + bias.rho_hv + rho_hv_sigma * noise.rho_hv_standard_normal).clamp(0.0, 1.0)
            }),
        kdp_deg_km: ideal
            .values
            .kdp_deg_km
            .filter(|value| value.is_finite())
            .map(|value| value + bias.kdp_deg_km + kdp_sigma_deg_km * noise.kdp_standard_normal),
    };
    Ok(MeasuredMoments {
        values,
        sensitivity_dbz: Some(sensitivity_dbz),
        snr_db: Some(snr_db),
        censored: false,
        sampling,
        uncertainty,
        bias,
        noise,
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PresentationConfig {
    pub reflectivity_texture_sigma_db: f64,
    pub velocity_texture_sigma_mps: f64,
    pub zdr_display_bias_db: f64,
    pub reflectivity_display_floor_dbz: Option<f64>,
    pub clutter_reflectivity_dbz: Option<f64>,
    pub clutter_velocity_mps: f64,
}

impl Default for PresentationConfig {
    fn default() -> Self {
        Self {
            reflectivity_texture_sigma_db: 0.0,
            velocity_texture_sigma_mps: 0.0,
            zdr_display_bias_db: 0.0,
            reflectivity_display_floor_dbz: None,
            clutter_reflectivity_dbz: None,
            clutter_velocity_mps: 0.0,
        }
    }
}

pub fn present_measured_moments(
    measured: MeasuredMoments,
    config: &PresentationConfig,
    noise_key: NoiseKey,
) -> PresentedMoments {
    let mut values = measured.values;
    let reflectivity_adjustment =
        config.reflectivity_texture_sigma_db * standard_normal(noise_key, 101);
    let velocity_adjustment = config.velocity_texture_sigma_mps * standard_normal(noise_key, 102);
    if let Some(value) = &mut values.reflectivity_dbz {
        *value += reflectivity_adjustment;
    }
    if let Some(value) = &mut values.velocity_mps {
        *value += velocity_adjustment;
    }
    if let Some(value) = &mut values.zdr_db {
        *value += config.zdr_display_bias_db;
    }
    let mut clutter_replaced = false;
    if let Some(clutter_dbz) = config.clutter_reflectivity_dbz
        && values
            .reflectivity_dbz
            .is_none_or(|reflectivity| reflectivity < clutter_dbz)
    {
        values.reflectivity_dbz = Some(clutter_dbz);
        if values.velocity_mps.is_some() {
            values.velocity_mps = Some(config.clutter_velocity_mps);
        }
        clutter_replaced = true;
    }
    let threshold_censored = config.reflectivity_display_floor_dbz.is_some_and(|floor| {
        values
            .reflectivity_dbz
            .is_some_and(|reflectivity| reflectivity < floor)
    });
    if threshold_censored {
        values = RadarMomentValues::default();
    }
    PresentedMoments {
        values,
        adjustment: PresentationAdjustment {
            reflectivity_db: reflectivity_adjustment,
            velocity_mps: velocity_adjustment,
            zdr_db: config.zdr_display_bias_db,
            clutter_replaced,
            threshold_censored,
        },
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn uniform_open01(seed: u64) -> f64 {
    let mantissa = seed >> 11;
    (mantissa as f64 + 0.5) / ((1_u64 << 53) as f64)
}

fn standard_normal(key: NoiseKey, stream: u64) -> f64 {
    let u1 = uniform_open01(key.stream_seed(stream.wrapping_mul(2)));
    let u2 = uniform_open01(key.stream_seed(stream.wrapping_mul(2).wrapping_add(1)));
    (-2.0 * u1.ln()).sqrt() * (TAU * u2).cos()
}

pub fn fold_velocity_f64(value_mps: f64, nyquist_mps: f64) -> f64 {
    if !value_mps.is_finite() || !nyquist_mps.is_finite() || nyquist_mps <= 0.0 {
        return value_mps;
    }
    if (-nyquist_mps..nyquist_mps).contains(&value_mps) {
        return value_mps;
    }
    (value_mps + nyquist_mps).rem_euclid(2.0 * nyquist_mps) - nyquist_mps
}

/// One hydrometeor/scatterer mode contributing to a selected gate's Doppler
/// spectrum. Fall velocity is already projected onto the radar beam and is
/// added to the air-motion mean using the caller's sign convention.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeciesDopplerMode {
    pub name: String,
    pub power_linear: f64,
    pub air_velocity_mps: f64,
    pub fall_velocity_projection_mps: f64,
    pub intrinsic_width_mps: f64,
    pub beam_shear_width_mps: f64,
    pub turbulence_width_mps: f64,
}

impl SpeciesDopplerMode {
    pub fn mean_velocity_mps(&self) -> f64 {
        self.air_velocity_mps + self.fall_velocity_projection_mps
    }

    pub fn combined_width_mps(&self) -> f64 {
        (self.intrinsic_width_mps.powi(2)
            + self.beam_shear_width_mps.powi(2)
            + self.turbulence_width_mps.powi(2))
        .sqrt()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DopplerSpectrumConfig {
    pub true_min_velocity_mps: f64,
    pub true_max_velocity_mps: f64,
    pub true_bin_count: usize,
    pub output_bin_count: usize,
    pub nyquist_velocity_mps: Option<f64>,
    /// Mean exponential white-noise power in each output bin.
    pub white_noise_power_per_bin: f64,
    pub noise_key: NoiseKey,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpectrumMoments {
    pub total_power: f64,
    pub mean_velocity_mps: f64,
    pub spectrum_width_mps: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpeciesSpectrum {
    pub name: String,
    pub true_power: Vec<f64>,
    pub aliased_power: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DopplerSpectrum {
    pub true_velocity_centers_mps: Vec<f64>,
    pub output_velocity_centers_mps: Vec<f64>,
    pub species: Vec<SpeciesSpectrum>,
    pub true_signal_power: Vec<f64>,
    pub aliased_signal_power: Vec<f64>,
    pub white_noise_power: Vec<f64>,
    pub measured_power: Vec<f64>,
    pub noise_subtracted_power: Vec<f64>,
    pub true_moments: SpectrumMoments,
    pub aliased_signal_moments: SpectrumMoments,
    pub measured_moments: SpectrumMoments,
    pub noise_subtracted_moments: SpectrumMoments,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DopplerSpectrumError {
    InvalidBounds,
    InvalidBinCount(usize),
    InvalidNyquist(f64),
    InvalidNoisePower(f64),
    InvalidSpecies { name: String, field: &'static str },
    NoSignal,
}

impl fmt::Display for DopplerSpectrumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBounds => write!(f, "Doppler spectrum velocity bounds are invalid"),
            Self::InvalidBinCount(value) => {
                write!(
                    f,
                    "Doppler spectrum bin count must be in 8..=65536, got {value}"
                )
            }
            Self::InvalidNyquist(value) => {
                write!(
                    f,
                    "Doppler spectrum Nyquist must be finite and positive, got {value}"
                )
            }
            Self::InvalidNoisePower(value) => write!(
                f,
                "Doppler spectrum white-noise power must be finite and nonnegative, got {value}"
            ),
            Self::InvalidSpecies { name, field } => {
                write!(f, "Doppler species '{name}' has invalid {field}")
            }
            Self::NoSignal => write!(f, "Doppler spectrum has no positive species power"),
        }
    }
}

impl Error for DopplerSpectrumError {}

pub fn synthesize_selected_gate_spectrum(
    modes: &[SpeciesDopplerMode],
    config: DopplerSpectrumConfig,
) -> Result<DopplerSpectrum, DopplerSpectrumError> {
    if !config.true_min_velocity_mps.is_finite()
        || !config.true_max_velocity_mps.is_finite()
        || config.true_min_velocity_mps >= config.true_max_velocity_mps
    {
        return Err(DopplerSpectrumError::InvalidBounds);
    }
    for count in [config.true_bin_count, config.output_bin_count] {
        if !(8..=65_536).contains(&count) {
            return Err(DopplerSpectrumError::InvalidBinCount(count));
        }
    }
    if let Some(nyquist) = config.nyquist_velocity_mps
        && (!nyquist.is_finite() || nyquist <= 0.0)
    {
        return Err(DopplerSpectrumError::InvalidNyquist(nyquist));
    }
    if !config.white_noise_power_per_bin.is_finite() || config.white_noise_power_per_bin < 0.0 {
        return Err(DopplerSpectrumError::InvalidNoisePower(
            config.white_noise_power_per_bin,
        ));
    }
    for mode in modes {
        if mode.name.trim().is_empty() {
            return Err(DopplerSpectrumError::InvalidSpecies {
                name: mode.name.clone(),
                field: "name",
            });
        }
        for (field, value, nonnegative) in [
            ("power", mode.power_linear, true),
            ("air velocity", mode.air_velocity_mps, false),
            ("fall projection", mode.fall_velocity_projection_mps, false),
            ("intrinsic width", mode.intrinsic_width_mps, true),
            ("beam-shear width", mode.beam_shear_width_mps, true),
            ("turbulence width", mode.turbulence_width_mps, true),
        ] {
            if !value.is_finite() || (nonnegative && value < 0.0) {
                return Err(DopplerSpectrumError::InvalidSpecies {
                    name: mode.name.clone(),
                    field,
                });
            }
        }
    }
    if !modes.iter().any(|mode| mode.power_linear > 0.0) {
        return Err(DopplerSpectrumError::NoSignal);
    }

    let true_velocity_centers_mps = bin_centers(
        config.true_min_velocity_mps,
        config.true_max_velocity_mps,
        config.true_bin_count,
    );
    let (output_min, output_max) = config.nyquist_velocity_mps.map_or(
        (config.true_min_velocity_mps, config.true_max_velocity_mps),
        |nyquist| (-nyquist, nyquist),
    );
    let output_velocity_centers_mps = bin_centers(output_min, output_max, config.output_bin_count);
    let mut true_signal_power = vec![0.0; config.true_bin_count];
    let mut aliased_signal_power = vec![0.0; config.output_bin_count];
    let mut species = Vec::with_capacity(modes.len());
    for mode in modes {
        let true_power = gaussian_mode_power(&true_velocity_centers_mps, mode)?;
        for (total, value) in true_signal_power.iter_mut().zip(&true_power) {
            *total += value;
        }
        let aliased_power = remap_spectrum(
            &true_velocity_centers_mps,
            &true_power,
            output_min,
            output_max,
            config.output_bin_count,
            config.nyquist_velocity_mps,
        );
        for (total, value) in aliased_signal_power.iter_mut().zip(&aliased_power) {
            *total += value;
        }
        species.push(SpeciesSpectrum {
            name: mode.name.clone(),
            true_power,
            aliased_power,
        });
    }

    let white_noise_power = (0..config.output_bin_count)
        .map(|bin| {
            if config.white_noise_power_per_bin == 0.0 {
                0.0
            } else {
                let uniform = uniform_open01(
                    config
                        .noise_key
                        .stream_seed(10_000_u64.wrapping_add(bin as u64)),
                );
                -config.white_noise_power_per_bin * uniform.ln()
            }
        })
        .collect::<Vec<_>>();
    let measured_power = aliased_signal_power
        .iter()
        .zip(&white_noise_power)
        .map(|(signal, noise)| signal + noise)
        .collect::<Vec<_>>();
    let noise_subtracted_power = measured_power
        .iter()
        .map(|power| (power - config.white_noise_power_per_bin).max(0.0))
        .collect::<Vec<_>>();
    Ok(DopplerSpectrum {
        true_moments: moments_from_spectrum(&true_velocity_centers_mps, &true_signal_power),
        aliased_signal_moments: moments_from_spectrum(
            &output_velocity_centers_mps,
            &aliased_signal_power,
        ),
        measured_moments: moments_from_spectrum(&output_velocity_centers_mps, &measured_power),
        noise_subtracted_moments: moments_from_spectrum(
            &output_velocity_centers_mps,
            &noise_subtracted_power,
        ),
        true_velocity_centers_mps,
        output_velocity_centers_mps,
        species,
        true_signal_power,
        aliased_signal_power,
        white_noise_power,
        measured_power,
        noise_subtracted_power,
    })
}

fn bin_centers(minimum: f64, maximum: f64, count: usize) -> Vec<f64> {
    let width = (maximum - minimum) / count as f64;
    (0..count)
        .map(|index| minimum + (index as f64 + 0.5) * width)
        .collect()
}

fn gaussian_mode_power(
    centers: &[f64],
    mode: &SpeciesDopplerMode,
) -> Result<Vec<f64>, DopplerSpectrumError> {
    let mut power = vec![0.0; centers.len()];
    if mode.power_linear <= 0.0 {
        return Ok(power);
    }
    let mean = mode.mean_velocity_mps();
    let width = mode.combined_width_mps();
    if width <= 1.0e-9 {
        let index = centers
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                (**left - mean).abs().total_cmp(&(**right - mean).abs())
            })
            .map(|(index, _)| index)
            .ok_or_else(|| DopplerSpectrumError::InvalidSpecies {
                name: mode.name.clone(),
                field: "empty velocity grid",
            })?;
        power[index] = mode.power_linear;
        return Ok(power);
    }
    for (value, center) in power.iter_mut().zip(centers) {
        let standardized = (*center - mean) / width;
        *value = (-0.5 * standardized * standardized).exp();
    }
    let sum: f64 = power.iter().sum();
    if !sum.is_finite() || sum <= 0.0 {
        return Err(DopplerSpectrumError::InvalidSpecies {
            name: mode.name.clone(),
            field: "spectrum support",
        });
    }
    for value in &mut power {
        *value *= mode.power_linear / sum;
    }
    Ok(power)
}

fn remap_spectrum(
    source_centers: &[f64],
    source_power: &[f64],
    output_min: f64,
    output_max: f64,
    output_count: usize,
    nyquist_mps: Option<f64>,
) -> Vec<f64> {
    let mut output = vec![0.0; output_count];
    let width = (output_max - output_min) / output_count as f64;
    for (&velocity, &power) in source_centers.iter().zip(source_power) {
        let mapped = nyquist_mps.map_or(velocity, |nyquist| fold_velocity_f64(velocity, nyquist));
        let index = ((mapped - output_min) / width)
            .floor()
            .clamp(0.0, (output_count - 1) as f64) as usize;
        output[index] += power;
    }
    output
}

pub fn moments_from_spectrum(centers: &[f64], powers: &[f64]) -> SpectrumMoments {
    if centers.len() != powers.len() {
        return SpectrumMoments::default();
    }
    let total_power: f64 = powers
        .iter()
        .copied()
        .filter(|value| value.is_finite() && *value > 0.0)
        .sum();
    if total_power <= 0.0 {
        return SpectrumMoments::default();
    }
    let mean_velocity_mps = centers
        .iter()
        .zip(powers)
        .filter(|(center, power)| center.is_finite() && power.is_finite() && **power > 0.0)
        .map(|(center, power)| center * power)
        .sum::<f64>()
        / total_power;
    let variance = centers
        .iter()
        .zip(powers)
        .filter(|(center, power)| center.is_finite() && power.is_finite() && **power > 0.0)
        .map(|(center, power)| power * (*center - mean_velocity_mps).powi(2))
        .sum::<f64>()
        / total_power;
    SpectrumMoments {
        total_power,
        mean_velocity_mps,
        spectrum_width_mps: variance.max(0.0).sqrt(),
    }
}

// ---- Selected-gate explanation contracts ---------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GateIdentity {
    pub frame_index: usize,
    pub cut_index: usize,
    pub radial_index: usize,
    pub gate_index: usize,
    pub azimuth_deg: f64,
    pub elevation_deg: f64,
    pub slant_range_m: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GateTimeExplanation {
    pub ray_offset_ms: i64,
    pub anchor_unix_ms: i64,
    pub neighbor_unix_ms: Option<i64>,
    pub temporal_alpha: f64,
    pub held_anchor: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GateCoverageExplanation {
    pub model_coverage_fraction: f64,
    pub terrain_unblocked_fraction: f64,
    pub meteorological_signal_fraction: f64,
    pub unblocked_power_fraction: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HydrometeorGateContribution {
    pub name: String,
    pub zh_linear: f64,
    pub zv_linear: f64,
    pub kdp_deg_km: f64,
    pub ah_db_km: f64,
    pub fall_speed_mps: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GateVelocityExplanation {
    pub air_velocity_mps: f64,
    pub terminal_fall_correction_mps: f64,
    pub scatterer_velocity_mps: f64,
    pub pulse_volume_variance_m2s2: f64,
    pub terminal_variance_m2s2: f64,
    pub turbulence_variance_m2s2: f64,
    pub instrument_variance_m2s2: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GatePropagationExplanation {
    pub intrinsic_reflectivity_dbz: Option<f64>,
    pub observed_reflectivity_dbz: Option<f64>,
    pub intrinsic_zdr_db: Option<f64>,
    pub observed_zdr_db: Option<f64>,
    pub phi_dp_deg: Option<f64>,
    pub pia_db: Option<f64>,
    pub pida_db: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateExplanation {
    pub identity: GateIdentity,
    pub time: GateTimeExplanation,
    pub coverage: GateCoverageExplanation,
    pub hydrometeors: Vec<HydrometeorGateContribution>,
    pub velocity: GateVelocityExplanation,
    pub propagation: GatePropagationExplanation,
    pub instrument: RadarInstrument,
    pub timing: Option<ResolvedSinglePrf>,
    pub ideal: IdealMoments,
    pub measured: MeasuredMoments,
    pub presented: PresentedMoments,
    pub spectrum: Option<DopplerSpectrum>,
    pub provenance: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WhyThisGateUnavailable {
    NotSyntheticRadar,
    SourceSnapshotExpired,
    SourceFileUnavailable,
    StaleFrameWitness,
    UnsupportedSourceContract,
    WorkerFailed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum WhyThisGate {
    Available(Box<GateExplanation>),
    Unavailable(WhyThisGateUnavailable),
    Loading(GateIdentity),
}

// ---- Algorithm Truth Lab pure scorecards ---------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SummaryStats {
    pub count: usize,
    pub mean: Option<f64>,
    pub mean_absolute: Option<f64>,
    pub rmse: Option<f64>,
    pub p95_absolute: Option<f64>,
    pub maximum_absolute: Option<f64>,
}

impl SummaryStats {
    pub fn from_errors(errors: &[f64]) -> Self {
        let mut values = errors
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .collect::<Vec<_>>();
        if values.is_empty() {
            return Self::default();
        }
        let count = values.len();
        let mean = values.iter().sum::<f64>() / count as f64;
        let mean_absolute = values.iter().map(|value| value.abs()).sum::<f64>() / count as f64;
        let rmse = (values.iter().map(|value| value * value).sum::<f64>() / count as f64).sqrt();
        values.sort_by(|left, right| left.abs().total_cmp(&right.abs()));
        let p95_index = ((count - 1) as f64 * 0.95).ceil() as usize;
        let p95_absolute = values[p95_index].abs();
        let maximum_absolute = values
            .iter()
            .map(|value| value.abs())
            .fold(0.0_f64, f64::max);
        Self {
            count,
            mean: Some(mean),
            mean_absolute: Some(mean_absolute),
            rmse: Some(rmse),
            p95_absolute: Some(p95_absolute),
            maximum_absolute: Some(maximum_absolute),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VelocityTruthSample {
    pub true_velocity_mps: f64,
    pub folded_velocity_mps: Option<f64>,
    pub dealiased_velocity_mps: Option<f64>,
    pub nyquist_velocity_mps: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VelocityTruthScorecard {
    pub input_samples: usize,
    pub folded_samples: usize,
    pub dealiased_samples: usize,
    pub fold_consistency_errors: usize,
    pub recovered_folded_samples: usize,
    pub branch_errors: usize,
    pub false_unfolds: usize,
    pub folded_error: SummaryStats,
    pub dealiased_error: SummaryStats,
}

pub fn score_velocity_truth(
    samples: &[VelocityTruthSample],
    recovery_tolerance_mps: f64,
) -> VelocityTruthScorecard {
    let tolerance = recovery_tolerance_mps.max(0.0);
    let mut folded_errors = Vec::new();
    let mut dealiased_errors = Vec::new();
    let mut folded_samples = 0usize;
    let mut fold_consistency_errors = 0usize;
    let mut recovered_folded_samples = 0usize;
    let mut branch_errors = 0usize;
    let mut false_unfolds = 0usize;
    let mut dealiased_samples = 0usize;
    for sample in samples {
        if !sample.true_velocity_mps.is_finite() {
            continue;
        }
        let valid_nyquist = sample
            .nyquist_velocity_mps
            .filter(|value| value.is_finite() && *value > 0.0);
        let expected_folded =
            valid_nyquist.map(|nyquist| fold_velocity_f64(sample.true_velocity_mps, nyquist));
        let was_folded = expected_folded
            .is_some_and(|folded| (folded - sample.true_velocity_mps).abs() > tolerance);
        folded_samples += usize::from(was_folded);
        if let Some(stored_folded) = sample.folded_velocity_mps.filter(|value| value.is_finite()) {
            let error = stored_folded - sample.true_velocity_mps;
            folded_errors.push(error);
            if expected_folded.is_some_and(|expected| (stored_folded - expected).abs() > tolerance)
            {
                fold_consistency_errors += 1;
            }
        }
        let Some(dealiased) = sample
            .dealiased_velocity_mps
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        dealiased_samples += 1;
        let error = dealiased - sample.true_velocity_mps;
        dealiased_errors.push(error);
        if was_folded && error.abs() <= tolerance {
            recovered_folded_samples += 1;
        }
        if let Some(nyquist) = valid_nyquist {
            let branch = (error / (2.0 * nyquist)).round();
            let branch_residual = error - branch * 2.0 * nyquist;
            if branch != 0.0 && branch_residual.abs() <= tolerance {
                branch_errors += 1;
                if !was_folded {
                    false_unfolds += 1;
                }
            }
        }
    }
    VelocityTruthScorecard {
        input_samples: samples.len(),
        folded_samples,
        dealiased_samples,
        fold_consistency_errors,
        recovered_folded_samples,
        branch_errors,
        false_unfolds,
        folded_error: SummaryStats::from_errors(&folded_errors),
        dealiased_error: SummaryStats::from_errors(&dealiased_errors),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindVector {
    pub u_mps: f64,
    pub v_mps: f64,
}

impl WindVector {
    pub fn speed_mps(self) -> f64 {
        self.u_mps.hypot(self.v_mps)
    }

    fn mathematical_direction_deg(self) -> f64 {
        self.v_mps.atan2(self.u_mps).to_degrees().rem_euclid(360.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindTruthPair {
    pub truth: WindVector,
    pub retrieved: Option<WindVector>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindTruthScorecard {
    pub input_levels: usize,
    pub paired_levels: usize,
    pub u_error: SummaryStats,
    pub v_error: SummaryStats,
    pub vector_error: SummaryStats,
    pub speed_error: SummaryStats,
    pub direction_absolute_error: SummaryStats,
}

pub fn score_vector_winds(pairs: &[WindTruthPair]) -> WindTruthScorecard {
    let mut u_errors = Vec::new();
    let mut v_errors = Vec::new();
    let mut vector_errors = Vec::new();
    let mut speed_errors = Vec::new();
    let mut direction_errors = Vec::new();
    for pair in pairs {
        let Some(retrieved) = pair.retrieved else {
            continue;
        };
        if ![
            pair.truth.u_mps,
            pair.truth.v_mps,
            retrieved.u_mps,
            retrieved.v_mps,
        ]
        .into_iter()
        .all(f64::is_finite)
        {
            continue;
        }
        let du = retrieved.u_mps - pair.truth.u_mps;
        let dv = retrieved.v_mps - pair.truth.v_mps;
        u_errors.push(du);
        v_errors.push(dv);
        vector_errors.push(du.hypot(dv));
        speed_errors.push(retrieved.speed_mps() - pair.truth.speed_mps());
        if pair.truth.speed_mps() >= 0.5 && retrieved.speed_mps() >= 0.5 {
            direction_errors.push(circular_difference_deg(
                retrieved.mathematical_direction_deg(),
                pair.truth.mathematical_direction_deg(),
            ));
        }
    }
    WindTruthScorecard {
        input_levels: pairs.len(),
        paired_levels: u_errors.len(),
        u_error: SummaryStats::from_errors(&u_errors),
        v_error: SummaryStats::from_errors(&v_errors),
        vector_error: SummaryStats::from_errors(&vector_errors),
        speed_error: SummaryStats::from_errors(&speed_errors),
        direction_absolute_error: SummaryStats::from_errors(&direction_errors),
    }
}

fn circular_difference_deg(left: f64, right: f64) -> f64 {
    (left - right + 180.0).rem_euclid(360.0) - 180.0
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VortexDescriptor {
    pub center_east_km: f64,
    pub center_north_km: f64,
    pub radius_of_max_wind_km: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VortexTruthPair {
    pub truth: VortexDescriptor,
    pub retrieved: Option<VortexDescriptor>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VortexTruthScorecard {
    pub input_cases: usize,
    pub paired_cases: usize,
    pub center_distance_error_km: SummaryStats,
    pub radius_of_max_wind_error_km: SummaryStats,
}

pub fn score_vortex_truth(pairs: &[VortexTruthPair]) -> VortexTruthScorecard {
    let mut center_errors = Vec::new();
    let mut radius_errors = Vec::new();
    for pair in pairs {
        let Some(retrieved) = pair.retrieved else {
            continue;
        };
        let values = [
            pair.truth.center_east_km,
            pair.truth.center_north_km,
            pair.truth.radius_of_max_wind_km,
            retrieved.center_east_km,
            retrieved.center_north_km,
            retrieved.radius_of_max_wind_km,
        ];
        if !values.into_iter().all(f64::is_finite) {
            continue;
        }
        center_errors.push(
            (retrieved.center_east_km - pair.truth.center_east_km)
                .hypot(retrieved.center_north_km - pair.truth.center_north_km),
        );
        radius_errors.push(retrieved.radius_of_max_wind_km - pair.truth.radius_of_max_wind_km);
    }
    VortexTruthScorecard {
        input_cases: pairs.len(),
        paired_cases: center_errors.len(),
        center_distance_error_km: SummaryStats::from_errors(&center_errors),
        radius_of_max_wind_error_km: SummaryStats::from_errors(&radius_errors),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TruthObject {
    pub id: u64,
    pub east_km: f64,
    pub north_km: f64,
    pub area_km2: f64,
    pub maximum_dbz: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetrievedObject {
    pub id: u64,
    pub east_km: f64,
    pub north_km: f64,
    pub area_km2: f64,
    pub maximum_dbz: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObjectMatch {
    pub truth_id: u64,
    pub retrieved_id: u64,
    pub centroid_distance_km: f64,
    pub area_error_km2: f64,
    pub maximum_dbz_error: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObjectTruthScorecard {
    pub truth_objects: usize,
    pub retrieved_objects: usize,
    pub matches: Vec<ObjectMatch>,
    pub false_negatives: usize,
    pub false_positives: usize,
    pub precision: Option<f64>,
    pub recall: Option<f64>,
    pub centroid_distance_km: SummaryStats,
    pub area_error_km2: SummaryStats,
    pub maximum_dbz_error: SummaryStats,
}

/// Deterministic one-to-one nearest-pair matching inside a declared centroid
/// gate. The returned pair identities make the matching policy auditable.
pub fn score_object_matches(
    truth: &[TruthObject],
    retrieved: &[RetrievedObject],
    maximum_centroid_distance_km: f64,
) -> ObjectTruthScorecard {
    let gate = maximum_centroid_distance_km.max(0.0);
    let mut candidates = Vec::new();
    for (truth_index, truth_object) in truth.iter().enumerate() {
        for (retrieved_index, retrieved_object) in retrieved.iter().enumerate() {
            let distance = (truth_object.east_km - retrieved_object.east_km)
                .hypot(truth_object.north_km - retrieved_object.north_km);
            if distance.is_finite() && distance <= gate {
                candidates.push((
                    distance,
                    truth_object.id,
                    retrieved_object.id,
                    truth_index,
                    retrieved_index,
                ));
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    let mut used_truth = BTreeSet::new();
    let mut used_retrieved = BTreeSet::new();
    let mut matches = Vec::new();
    for (distance, _, _, truth_index, retrieved_index) in candidates {
        if used_truth.contains(&truth_index) || used_retrieved.contains(&retrieved_index) {
            continue;
        }
        used_truth.insert(truth_index);
        used_retrieved.insert(retrieved_index);
        let truth_object = truth[truth_index];
        let retrieved_object = retrieved[retrieved_index];
        matches.push(ObjectMatch {
            truth_id: truth_object.id,
            retrieved_id: retrieved_object.id,
            centroid_distance_km: distance,
            area_error_km2: retrieved_object.area_km2 - truth_object.area_km2,
            maximum_dbz_error: retrieved_object.maximum_dbz - truth_object.maximum_dbz,
        });
    }
    matches.sort_by_key(|item| (item.truth_id, item.retrieved_id));
    let matched = matches.len();
    let false_negatives = truth.len().saturating_sub(matched);
    let false_positives = retrieved.len().saturating_sub(matched);
    let centroid_errors = matches
        .iter()
        .map(|item| item.centroid_distance_km)
        .collect::<Vec<_>>();
    let area_errors = matches
        .iter()
        .map(|item| item.area_error_km2)
        .collect::<Vec<_>>();
    let reflectivity_errors = matches
        .iter()
        .map(|item| item.maximum_dbz_error)
        .collect::<Vec<_>>();
    ObjectTruthScorecard {
        truth_objects: truth.len(),
        retrieved_objects: retrieved.len(),
        matches,
        false_negatives,
        false_positives,
        precision: (!retrieved.is_empty()).then_some(matched as f64 / retrieved.len() as f64),
        recall: (!truth.is_empty()).then_some(matched as f64 / truth.len() as f64),
        centroid_distance_km: SummaryStats::from_errors(&centroid_errors),
        area_error_km2: SummaryStats::from_errors(&area_errors),
        maximum_dbz_error: SummaryStats::from_errors(&reflectivity_errors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: NoiseKey = NoiseKey {
        seed: 0x5eed_cafe_dead_beef,
        frame: 3,
        cut: 2,
        ray: 117,
        gate: 401,
    };

    fn s_band() -> RadarInstrument {
        RadarInstrument::new("research S band", 2.8e9, 1.0e-6).unwrap()
    }

    fn timing(prf_hz: f64) -> ResolvedSinglePrf {
        resolve_prf(
            &s_band(),
            PrfSpecification::CustomSinglePrf(CustomSinglePrf::new(prf_hz).unwrap()),
        )
        .unwrap()
    }

    fn ideal() -> IdealMoments {
        IdealMoments {
            values: RadarMomentValues {
                reflectivity_dbz: Some(35.0),
                velocity_mps: Some(42.0),
                spectrum_width_mps: Some(2.5),
                zdr_db: Some(1.4),
                rho_hv: Some(0.97),
                kdp_deg_km: Some(2.2),
            },
        }
    }

    fn assert_close(left: f64, right: f64, tolerance: f64) {
        assert!(
            (left - right).abs() <= tolerance,
            "{left} differs from {right} by more than {tolerance}"
        );
    }

    #[test]
    fn radar_band_is_a_label_while_exact_frequency_drives_wavelength() {
        let s = RadarInstrument::new("S", 2.8e9, 1.57e-6).unwrap();
        let c = RadarInstrument::new("C", 5.6e9, 1.0e-6).unwrap();
        let x = RadarInstrument::new("X", 9.4e9, 0.5e-6).unwrap();
        assert_eq!(s.band, RadarBand::S);
        assert_eq!(c.band, RadarBand::C);
        assert_eq!(x.band, RadarBand::X);
        assert_close(s.wavelength_m(), SPEED_OF_LIGHT_MPS / 2.8e9, 1.0e-15);
        assert_ne!(s.wavelength_m(), SPEED_OF_LIGHT_MPS / 3.0e9);
        assert!(RadarInstrument::new("", 2.8e9, 1.0e-6).is_err());
        assert!(RadarInstrument::new("bad", f64::NAN, 1.0e-6).is_err());
    }

    #[test]
    fn custom_single_prf_couples_prt_nyquist_and_unambiguous_range() {
        let instrument = s_band();
        let resolved = resolve_prf(
            &instrument,
            PrfSpecification::CustomSinglePrf(CustomSinglePrf::new(1_000.0).unwrap()),
        )
        .unwrap();
        assert_close(resolved.frequency_hz, 2.8e9, 0.0);
        assert_close(resolved.prt_s, 0.001, 1.0e-15);
        assert_close(
            resolved.nyquist_velocity_mps,
            instrument.wavelength_m() * 1_000.0 / 4.0,
            1.0e-12,
        );
        assert_close(
            resolved.unambiguous_range_m,
            SPEED_OF_LIGHT_MPS / 2_000.0,
            1.0e-9,
        );
    }

    #[test]
    fn named_vcp_prf_codes_fail_closed_instead_of_becoming_hertz() {
        let error = resolve_prf(
            &s_band(),
            PrfSpecification::NamedVcpCode { vcp: 212, code: 6 },
        )
        .unwrap_err();
        assert_eq!(
            error,
            InstrumentError::NamedVcpPrfCodeUnresolved { vcp: 212, code: 6 }
        );
        assert!(error.to_string().contains("not a frequency"));
    }

    #[test]
    fn matched_filter_response_is_pulse_derived_symmetric_and_normalized() {
        let response = MatchedFilterRangeResponse::new(1.0e-6, 9).unwrap();
        assert_close(
            response.range_resolution_m,
            SPEED_OF_LIGHT_MPS * 1.0e-6 / 2.0,
            1.0e-12,
        );
        assert_close(
            response.samples().iter().map(|sample| sample.weight).sum(),
            1.0,
            1.0e-15,
        );
        assert_close(
            response
                .samples()
                .iter()
                .map(|sample| sample.offset_m * sample.weight)
                .sum(),
            0.0,
            1.0e-12,
        );
        for pair in response
            .samples()
            .iter()
            .zip(response.samples().iter().rev())
        {
            assert_close(pair.0.offset_m, -pair.1.offset_m, 1.0e-12);
            assert_close(pair.0.weight, pair.1.weight, 1.0e-15);
        }
        assert!(response.weight_density_at(0.0) > response.weight_density_at(75.0));
        assert_eq!(response.weight_density_at(response.range_resolution_m), 0.0);
    }

    #[test]
    fn matched_filter_offsets_do_not_accept_or_depend_on_gate_spacing() {
        let short = MatchedFilterRangeResponse::new(1.0e-6, 7).unwrap();
        let long = MatchedFilterRangeResponse::new(2.0e-6, 7).unwrap();
        assert_close(
            long.range_resolution_m,
            2.0 * short.range_resolution_m,
            1.0e-12,
        );
        for (short_sample, long_sample) in short.samples().iter().zip(long.samples()) {
            assert_close(long_sample.offset_m, 2.0 * short_sample.offset_m, 1.0e-12);
            assert_close(long_sample.weight, short_sample.weight, 1.0e-15);
        }
        assert!(MatchedFilterRangeResponse::new(1.0e-6, 8).is_err());
        assert!(MatchedFilterRangeResponse::new(0.0, 9).is_err());
    }

    #[test]
    fn estimator_sampling_uses_dwell_prf_or_explicit_pulse_count() {
        let timing = timing(1_000.0);
        let derived = resolve_estimator_sampling(
            &timing,
            &MomentEstimatorConfig {
                dwell_s: 0.051,
                independent_sample_fraction: 0.5,
                ..MomentEstimatorConfig::default()
            },
        )
        .unwrap();
        assert_eq!(derived.transmitted_pulses, 51);
        assert_close(derived.independent_samples, 25.5, 0.0);
        let explicit = resolve_estimator_sampling(
            &timing,
            &MomentEstimatorConfig {
                dwell_s: 0.051,
                pulse_count: Some(64),
                independent_sample_fraction: 0.25,
                ..MomentEstimatorConfig::default()
            },
        )
        .unwrap();
        assert_eq!(explicit.transmitted_pulses, 64);
        assert_close(explicit.independent_samples, 16.0, 0.0);
        assert!(
            resolve_estimator_sampling(
                &timing,
                &MomentEstimatorConfig {
                    dwell_s: 0.0001,
                    ..MomentEstimatorConfig::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn measured_estimator_is_reproducible_and_folds_with_resolved_nyquist() {
        let instrument = s_band();
        let timing = timing(1_000.0);
        let config = MomentEstimatorConfig {
            pulse_count: Some(64),
            ..MomentEstimatorConfig::default()
        };
        let first =
            estimate_measured_moments(ideal(), &instrument, &timing, &config, 50_000.0, TEST_KEY)
                .unwrap();
        let second =
            estimate_measured_moments(ideal(), &instrument, &timing, &config, 50_000.0, TEST_KEY)
                .unwrap();
        assert_eq!(first, second);
        let velocity = first.values.velocity_mps.unwrap();
        assert!(velocity >= -timing.nyquist_velocity_mps);
        assert!(velocity < timing.nyquist_velocity_mps);
        assert!(!first.censored);
        assert!(first.uncertainty.reflectivity_sigma_db > 0.0);
        assert!(first.uncertainty.kdp_sigma_deg_km > 0.0);
    }

    #[test]
    fn more_independent_pulses_and_higher_snr_reduce_uncertainty() {
        let instrument = s_band();
        let timing = timing(1_000.0);
        let low_count = estimate_measured_moments(
            ideal(),
            &instrument,
            &timing,
            &MomentEstimatorConfig {
                pulse_count: Some(16),
                ..MomentEstimatorConfig::default()
            },
            100_000.0,
            TEST_KEY,
        )
        .unwrap();
        let high_count = estimate_measured_moments(
            ideal(),
            &instrument,
            &timing,
            &MomentEstimatorConfig {
                pulse_count: Some(128),
                ..MomentEstimatorConfig::default()
            },
            100_000.0,
            TEST_KEY,
        )
        .unwrap();
        assert!(
            high_count.uncertainty.reflectivity_sigma_db
                < low_count.uncertainty.reflectivity_sigma_db
        );
        assert!(
            high_count.uncertainty.velocity_sigma_mps < low_count.uncertainty.velocity_sigma_mps
        );

        let mut weak_ideal = ideal();
        weak_ideal.values.reflectivity_dbz = Some(5.0);
        let weak = estimate_measured_moments(
            weak_ideal,
            &instrument,
            &timing,
            &MomentEstimatorConfig {
                pulse_count: Some(64),
                minimum_snr_db: -100.0,
                ..MomentEstimatorConfig::default()
            },
            100_000.0,
            TEST_KEY,
        )
        .unwrap();
        assert!(
            weak.uncertainty.reflectivity_sigma_db > high_count.uncertainty.reflectivity_sigma_db
        );
        assert!(weak.bias.rho_hv < high_count.bias.rho_hv);
    }

    #[test]
    fn low_snr_is_censored_before_noise_can_resurrect_it() {
        let mut weak = ideal();
        weak.values.reflectivity_dbz = Some(-30.0);
        let measured = estimate_measured_moments(
            weak,
            &s_band(),
            &timing(1_000.0),
            &MomentEstimatorConfig::default(),
            100_000.0,
            TEST_KEY,
        )
        .unwrap();
        assert!(measured.censored);
        assert_eq!(measured.values, RadarMomentValues::default());
        assert!(measured.snr_db.unwrap() < 0.0);
    }

    #[test]
    fn timing_from_another_exact_frequency_is_rejected() {
        let c_band = RadarInstrument::new("C", 5.6e9, 1.0e-6).unwrap();
        let error = estimate_measured_moments(
            ideal(),
            &c_band,
            &timing(1_000.0),
            &MomentEstimatorConfig::default(),
            10_000.0,
            TEST_KEY,
        )
        .unwrap_err();
        assert_eq!(error, EstimatorError::InstrumentTimingMismatch);
    }

    #[test]
    fn presentation_changes_never_mutate_the_measured_stage() {
        let measured = estimate_measured_moments(
            ideal(),
            &s_band(),
            &timing(1_000.0),
            &MomentEstimatorConfig {
                pulse_count: Some(64),
                ..MomentEstimatorConfig::default()
            },
            50_000.0,
            TEST_KEY,
        )
        .unwrap();
        let measured_before = measured;
        let presented = present_measured_moments(
            measured,
            &PresentationConfig {
                reflectivity_texture_sigma_db: 2.0,
                velocity_texture_sigma_mps: 0.5,
                zdr_display_bias_db: 0.2,
                ..PresentationConfig::default()
            },
            TEST_KEY,
        );
        assert_eq!(measured, measured_before);
        assert_ne!(
            presented.values.reflectivity_dbz,
            measured.values.reflectivity_dbz
        );
        assert_ne!(presented.values.velocity_mps, measured.values.velocity_mps);
        assert_ne!(presented.values.zdr_db, measured.values.zdr_db);
    }

    #[test]
    fn presentation_clutter_and_threshold_are_explicit() {
        let measured = MeasuredMoments {
            values: RadarMomentValues {
                reflectivity_dbz: Some(5.0),
                velocity_mps: Some(12.0),
                ..RadarMomentValues::default()
            },
            ..MeasuredMoments::default()
        };
        let clutter = present_measured_moments(
            measured,
            &PresentationConfig {
                clutter_reflectivity_dbz: Some(20.0),
                clutter_velocity_mps: 0.25,
                ..PresentationConfig::default()
            },
            TEST_KEY,
        );
        assert_eq!(clutter.values.reflectivity_dbz, Some(20.0));
        assert_eq!(clutter.values.velocity_mps, Some(0.25));
        assert!(clutter.adjustment.clutter_replaced);
        let censored = present_measured_moments(
            measured,
            &PresentationConfig {
                reflectivity_display_floor_dbz: Some(10.0),
                ..PresentationConfig::default()
            },
            TEST_KEY,
        );
        assert!(censored.adjustment.threshold_censored);
        assert_eq!(censored.values, RadarMomentValues::default());
    }

    fn spectrum_config(nyquist: Option<f64>, noise: f64) -> DopplerSpectrumConfig {
        DopplerSpectrumConfig {
            true_min_velocity_mps: -80.0,
            true_max_velocity_mps: 80.0,
            true_bin_count: 2_048,
            output_bin_count: 512,
            nyquist_velocity_mps: nyquist,
            white_noise_power_per_bin: noise,
            noise_key: TEST_KEY,
        }
    }

    #[test]
    fn selected_gate_spectrum_preserves_species_power_and_combined_modes() {
        let modes = vec![
            SpeciesDopplerMode {
                name: "rain".to_owned(),
                power_linear: 100.0,
                air_velocity_mps: 10.0,
                fall_velocity_projection_mps: -3.0,
                intrinsic_width_mps: 0.5,
                beam_shear_width_mps: 1.0,
                turbulence_width_mps: 1.5,
            },
            SpeciesDopplerMode {
                name: "hail".to_owned(),
                power_linear: 50.0,
                air_velocity_mps: 10.0,
                fall_velocity_projection_mps: -8.0,
                intrinsic_width_mps: 0.3,
                beam_shear_width_mps: 0.6,
                turbulence_width_mps: 1.5,
            },
        ];
        let spectrum =
            synthesize_selected_gate_spectrum(&modes, spectrum_config(None, 0.0)).unwrap();
        assert_close(spectrum.true_moments.total_power, 150.0, 1.0e-10);
        assert_close(spectrum.species[0].true_power.iter().sum(), 100.0, 1.0e-10);
        assert_close(spectrum.species[1].true_power.iter().sum(), 50.0, 1.0e-10);
        assert!(spectrum.true_moments.mean_velocity_mps > 5.0);
        assert!(spectrum.true_moments.mean_velocity_mps < 7.0);
        assert!(spectrum.true_moments.spectrum_width_mps > 2.0);
    }

    #[test]
    fn selected_gate_spectrum_aliases_power_without_losing_it() {
        let modes = [SpeciesDopplerMode {
            name: "rain".to_owned(),
            power_linear: 10.0,
            air_velocity_mps: 35.0,
            fall_velocity_projection_mps: 0.0,
            intrinsic_width_mps: 0.1,
            beam_shear_width_mps: 0.0,
            turbulence_width_mps: 0.0,
        }];
        let spectrum =
            synthesize_selected_gate_spectrum(&modes, spectrum_config(Some(10.0), 0.0)).unwrap();
        assert_close(
            spectrum.aliased_signal_power.iter().sum(),
            spectrum.true_signal_power.iter().sum(),
            1.0e-10,
        );
        assert_close(
            spectrum.aliased_signal_moments.mean_velocity_mps,
            -5.0,
            0.08,
        );
    }

    #[test]
    fn selected_gate_white_noise_is_seeded_and_exposed() {
        let modes = [SpeciesDopplerMode {
            name: "snow".to_owned(),
            power_linear: 1.0,
            air_velocity_mps: 2.0,
            fall_velocity_projection_mps: -0.5,
            intrinsic_width_mps: 0.5,
            beam_shear_width_mps: 0.5,
            turbulence_width_mps: 0.5,
        }];
        let first =
            synthesize_selected_gate_spectrum(&modes, spectrum_config(Some(20.0), 0.01)).unwrap();
        let second =
            synthesize_selected_gate_spectrum(&modes, spectrum_config(Some(20.0), 0.01)).unwrap();
        assert_eq!(first, second);
        assert!(first.white_noise_power.iter().any(|value| *value > 0.0));
        assert!(first.measured_moments.total_power > first.aliased_signal_moments.total_power);
    }

    #[test]
    fn empty_or_invalid_selected_gate_spectra_fail_cleanly() {
        assert_eq!(
            synthesize_selected_gate_spectrum(&[], spectrum_config(None, 0.0)).unwrap_err(),
            DopplerSpectrumError::NoSignal
        );
        let invalid = SpeciesDopplerMode {
            name: "rain".to_owned(),
            power_linear: -1.0,
            air_velocity_mps: 0.0,
            fall_velocity_projection_mps: 0.0,
            intrinsic_width_mps: 0.0,
            beam_shear_width_mps: 0.0,
            turbulence_width_mps: 0.0,
        };
        assert!(synthesize_selected_gate_spectrum(&[invalid], spectrum_config(None, 0.0)).is_err());
        assert_eq!(
            moments_from_spectrum(&[0.0], &[1.0, 2.0]),
            SpectrumMoments::default()
        );
    }

    #[test]
    fn why_this_gate_contract_distinguishes_loading_available_and_unavailable() {
        let identity = GateIdentity {
            frame_index: 1,
            cut_index: 2,
            radial_index: 3,
            gate_index: 4,
            ..GateIdentity::default()
        };
        assert_eq!(
            WhyThisGate::Loading(identity),
            WhyThisGate::Loading(identity)
        );
        assert_eq!(
            WhyThisGate::Unavailable(WhyThisGateUnavailable::NotSyntheticRadar),
            WhyThisGate::Unavailable(WhyThisGateUnavailable::NotSyntheticRadar)
        );
    }

    #[test]
    fn summary_stats_are_deterministic_and_use_absolute_p95() {
        let stats = SummaryStats::from_errors(&[-3.0, -1.0, 0.0, 2.0, 10.0]);
        assert_eq!(stats.count, 5);
        assert_close(stats.mean.unwrap(), 1.6, 1.0e-12);
        assert_close(stats.mean_absolute.unwrap(), 3.2, 1.0e-12);
        assert_eq!(stats.p95_absolute, Some(10.0));
        assert_eq!(stats.maximum_absolute, Some(10.0));
        assert_eq!(SummaryStats::from_errors(&[f64::NAN]).count, 0);
    }

    #[test]
    fn velocity_truth_scorecard_counts_recovery_branch_and_false_unfold() {
        let nyquist = 10.0;
        let samples = [
            VelocityTruthSample {
                true_velocity_mps: 25.0,
                folded_velocity_mps: Some(5.0),
                dealiased_velocity_mps: Some(25.1),
                nyquist_velocity_mps: Some(nyquist),
            },
            VelocityTruthSample {
                true_velocity_mps: 5.0,
                folded_velocity_mps: Some(5.0),
                dealiased_velocity_mps: Some(25.0),
                nyquist_velocity_mps: Some(nyquist),
            },
            VelocityTruthSample {
                true_velocity_mps: -26.0,
                folded_velocity_mps: Some(-5.0),
                dealiased_velocity_mps: None,
                nyquist_velocity_mps: Some(nyquist),
            },
        ];
        let score = score_velocity_truth(&samples, 0.25);
        assert_eq!(score.folded_samples, 2);
        assert_eq!(score.recovered_folded_samples, 1);
        assert_eq!(score.false_unfolds, 1);
        assert_eq!(score.branch_errors, 1);
        assert_eq!(score.fold_consistency_errors, 1);
        assert_eq!(score.dealiased_samples, 2);
    }

    #[test]
    fn vector_wind_scorecard_handles_missing_and_circular_direction() {
        let score = score_vector_winds(&[
            WindTruthPair {
                truth: WindVector {
                    u_mps: 10.0,
                    v_mps: 0.0,
                },
                retrieved: Some(WindVector {
                    u_mps: 9.0,
                    v_mps: 1.0,
                }),
            },
            WindTruthPair {
                truth: WindVector {
                    u_mps: -10.0,
                    v_mps: -0.1,
                },
                retrieved: Some(WindVector {
                    u_mps: -10.0,
                    v_mps: 0.1,
                }),
            },
            WindTruthPair {
                truth: WindVector {
                    u_mps: 5.0,
                    v_mps: 5.0,
                },
                retrieved: None,
            },
        ]);
        assert_eq!(score.input_levels, 3);
        assert_eq!(score.paired_levels, 2);
        assert_eq!(score.u_error.count, 2);
        assert!(score.direction_absolute_error.maximum_absolute.unwrap() < 10.0);
    }

    #[test]
    fn vortex_scorecard_reports_center_distance_and_signed_rmw_error() {
        let score = score_vortex_truth(&[
            VortexTruthPair {
                truth: VortexDescriptor {
                    center_east_km: 0.0,
                    center_north_km: 0.0,
                    radius_of_max_wind_km: 20.0,
                },
                retrieved: Some(VortexDescriptor {
                    center_east_km: 3.0,
                    center_north_km: 4.0,
                    radius_of_max_wind_km: 18.0,
                }),
            },
            VortexTruthPair {
                truth: VortexDescriptor {
                    center_east_km: 1.0,
                    center_north_km: 1.0,
                    radius_of_max_wind_km: 10.0,
                },
                retrieved: None,
            },
        ]);
        assert_eq!(score.input_cases, 2);
        assert_eq!(score.paired_cases, 1);
        assert_eq!(score.center_distance_error_km.mean, Some(5.0));
        assert_eq!(score.radius_of_max_wind_error_km.mean, Some(-2.0));
    }

    #[test]
    fn object_scorecard_enforces_one_to_one_matches_and_counts_misses() {
        let truth = [
            TruthObject {
                id: 1,
                east_km: 0.0,
                north_km: 0.0,
                area_km2: 100.0,
                maximum_dbz: 60.0,
            },
            TruthObject {
                id: 2,
                east_km: 20.0,
                north_km: 0.0,
                area_km2: 50.0,
                maximum_dbz: 50.0,
            },
        ];
        let retrieved = [
            RetrievedObject {
                id: 10,
                east_km: 1.0,
                north_km: 0.0,
                area_km2: 110.0,
                maximum_dbz: 59.0,
            },
            RetrievedObject {
                id: 11,
                east_km: 2.0,
                north_km: 0.0,
                area_km2: 90.0,
                maximum_dbz: 58.0,
            },
            RetrievedObject {
                id: 12,
                east_km: 100.0,
                north_km: 100.0,
                area_km2: 10.0,
                maximum_dbz: 30.0,
            },
        ];
        let score = score_object_matches(&truth, &retrieved, 5.0);
        assert_eq!(score.matches.len(), 1);
        assert_eq!(score.matches[0].truth_id, 1);
        assert_eq!(score.matches[0].retrieved_id, 10);
        assert_eq!(score.false_negatives, 1);
        assert_eq!(score.false_positives, 2);
        assert_close(score.precision.unwrap(), 1.0 / 3.0, 1.0e-12);
        assert_close(score.recall.unwrap(), 0.5, 1.0e-12);
        assert_eq!(score.centroid_distance_km.mean, Some(1.0));
        assert_eq!(score.area_error_km2.mean, Some(10.0));
        assert_eq!(score.maximum_dbz_error.mean, Some(-1.0));
    }

    #[test]
    fn velocity_fold_uses_half_open_nyquist_interval() {
        assert_eq!(fold_velocity_f64(-10.0, 10.0), -10.0);
        assert_eq!(fold_velocity_f64(10.0, 10.0), -10.0);
        assert_eq!(fold_velocity_f64(30.0, 10.0), -10.0);
        assert_eq!(fold_velocity_f64(5.0, 10.0), 5.0);
        assert!(fold_velocity_f64(f64::NAN, 10.0).is_nan());
    }
}
