//! Model-atmosphere radio refractivity and vertically varying radar-ray trace.
//!
//! The ordinary BowEcho beam geometry intentionally remains the familiar
//! standard-atmosphere 4/3-Earth solution.  This module is the explicit
//! research alternative: construct `N(z)` from model pressure, temperature and
//! humidity, then integrate the ray relative to the curved Earth.  It exposes
//! the actual refractivity gradient and ducting flag so anomalous propagation
//! is never silently presented as ordinary terrain blockage.

use serde::{Deserialize, Serialize};

use crate::EARTH_RADIUS_M;

/// Standard near-surface refractivity gradient represented by the 4/3-Earth
/// approximation, in N-units per kilometre.
pub const STANDARD_REFRACTIVITY_GRADIENT_N_PER_KM: f64 = -39.0;

/// Gradient at which downward ray curvature equals Earth's curvature.  More
/// negative values can form a surface/elevated duct.
pub const EARTH_DUCTING_GRADIENT_N_PER_KM: f64 = -1.0e9 / EARTH_RADIUS_M;

/// ITU-R P.453 radio refractivity from pressure, temperature and specific
/// humidity. Pressure is Pa, temperature K, and specific humidity kg/kg moist
/// air. Returns N-units (`n = 1 + N*1e-6`).
pub fn radio_refractivity_n_units(
    pressure_pa: f64,
    temperature_k: f64,
    specific_humidity_kgkg: f64,
) -> Option<f64> {
    if !pressure_pa.is_finite()
        || !temperature_k.is_finite()
        || !specific_humidity_kgkg.is_finite()
        || pressure_pa <= 0.0
        || temperature_k <= 0.0
        || !(0.0..1.0).contains(&specific_humidity_kgkg)
    {
        return None;
    }
    // q -> vapour partial pressure using epsilon = Rd/Rv.  Keep pressure and
    // vapour pressure in hPa for the P.453 coefficients.
    const EPSILON: f64 = 0.621_98;
    let pressure_hpa = pressure_pa * 0.01;
    let vapor_pressure_hpa = pressure_hpa * specific_humidity_kgkg
        / (EPSILON + (1.0 - EPSILON) * specific_humidity_kgkg);
    let dry = 77.6 * pressure_hpa / temperature_k;
    let wet = 3.732e5 * vapor_pressure_hpa / temperature_k.powi(2);
    let refractivity = dry + wet;
    refractivity.is_finite().then_some(refractivity)
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefractivityLevel {
    /// Height above the radar antenna, metres.
    pub height_m: f64,
    pub refractivity_n: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefractivityProfileError {
    TooFewLevels,
    NonFiniteLevel { index: usize },
    NonIncreasingHeight { index: usize },
}

impl std::fmt::Display for RefractivityProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooFewLevels => {
                formatter.write_str("refractivity profile needs at least two levels")
            }
            Self::NonFiniteLevel { index } => {
                write!(formatter, "refractivity level {index} is not finite")
            }
            Self::NonIncreasingHeight { index } => write!(
                formatter,
                "refractivity height at level {index} does not increase"
            ),
        }
    }
}

impl std::error::Error for RefractivityProfileError {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefractivityProfile {
    levels: Vec<RefractivityLevel>,
}

impl RefractivityProfile {
    pub fn new(levels: Vec<RefractivityLevel>) -> Result<Self, RefractivityProfileError> {
        if levels.len() < 2 {
            return Err(RefractivityProfileError::TooFewLevels);
        }
        for (index, level) in levels.iter().enumerate() {
            if !level.height_m.is_finite() || !level.refractivity_n.is_finite() {
                return Err(RefractivityProfileError::NonFiniteLevel { index });
            }
            if index > 0 && level.height_m <= levels[index - 1].height_m {
                return Err(RefractivityProfileError::NonIncreasingHeight { index });
            }
        }
        Ok(Self { levels })
    }

    pub fn levels(&self) -> &[RefractivityLevel] {
        &self.levels
    }

    /// Piecewise-linear N at height. End segments extend with their declared
    /// gradient so the ray does not acquire an artificial zero-gradient layer
    /// just above or below the sampled model column.
    pub fn refractivity_at(&self, height_m: f64) -> f64 {
        let segment = self.segment_index(height_m);
        let lower = self.levels[segment];
        let upper = self.levels[segment + 1];
        let alpha = (height_m - lower.height_m) / (upper.height_m - lower.height_m);
        lower.refractivity_n + alpha * (upper.refractivity_n - lower.refractivity_n)
    }

    pub fn gradient_n_per_km_at(&self, height_m: f64) -> f64 {
        let segment = self.segment_index(height_m);
        let lower = self.levels[segment];
        let upper = self.levels[segment + 1];
        1_000.0 * (upper.refractivity_n - lower.refractivity_n) / (upper.height_m - lower.height_m)
    }

    fn segment_index(&self, height_m: f64) -> usize {
        if height_m <= self.levels[0].height_m {
            return 0;
        }
        let upper = self
            .levels
            .partition_point(|level| level.height_m <= height_m);
        upper.saturating_sub(1).min(self.levels.len() - 2)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropagationRegime {
    Subrefractive,
    NearStandard,
    Superrefractive,
    Ducting,
}

/// Classify a local N-gradient without claiming a complete duct from one
/// sample. `Ducting` is the geometric threshold where downward refraction is
/// at least Earth's curvature; the full trace reports whether it encountered
/// such a layer.
pub fn propagation_regime(gradient_n_per_km: f64) -> PropagationRegime {
    if gradient_n_per_km <= EARTH_DUCTING_GRADIENT_N_PER_KM {
        PropagationRegime::Ducting
    } else if gradient_n_per_km < -79.0 {
        PropagationRegime::Superrefractive
    } else if gradient_n_per_km <= -20.0 {
        PropagationRegime::NearStandard
    } else {
        PropagationRegime::Subrefractive
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefractedBeamPoint {
    pub slant_range_m: f64,
    pub ground_range_m: f64,
    /// Height above the radar antenna's local Earth surface.
    pub height_above_radar_m: f64,
    /// Ray elevation relative to the local horizontal.
    pub elevation_deg: f64,
    pub refractivity_n: f64,
    pub gradient_n_per_km: f64,
    pub regime: PropagationRegime,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefractedBeamTrace {
    pub points: Vec<RefractedBeamPoint>,
    pub encountered_ducting_layer: bool,
    pub minimum_gradient_n_per_km: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RefractedBeamError {
    InvalidElevation(f64),
    InvalidRange(f64),
    InvalidStep(f64),
}

impl std::fmt::Display for RefractedBeamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidElevation(value) => write!(formatter, "invalid beam elevation {value}"),
            Self::InvalidRange(value) => write!(formatter, "invalid beam range {value} m"),
            Self::InvalidStep(value) => write!(formatter, "invalid ray-trace step {value} m"),
        }
    }
}

impl std::error::Error for RefractedBeamError {}

#[derive(Clone, Copy)]
struct RayState {
    ground_m: f64,
    height_m: f64,
    elevation_rad: f64,
}

/// Integrate a vertically varying refracted ray with fourth-order Runge-Kutta.
/// `step_m` is bounded by the caller; 100--500 m is a useful reference range.
pub fn trace_refracted_beam(
    profile: &RefractivityProfile,
    initial_elevation_deg: f64,
    maximum_slant_range_m: f64,
    step_m: f64,
) -> Result<RefractedBeamTrace, RefractedBeamError> {
    if !initial_elevation_deg.is_finite() || !(-10.0..=90.0).contains(&initial_elevation_deg) {
        return Err(RefractedBeamError::InvalidElevation(initial_elevation_deg));
    }
    if !maximum_slant_range_m.is_finite() || maximum_slant_range_m < 0.0 {
        return Err(RefractedBeamError::InvalidRange(maximum_slant_range_m));
    }
    if !step_m.is_finite() || step_m <= 0.0 || step_m > 10_000.0 {
        return Err(RefractedBeamError::InvalidStep(step_m));
    }

    let mut state = RayState {
        ground_m: 0.0,
        height_m: 0.0,
        elevation_rad: initial_elevation_deg.to_radians(),
    };
    let mut slant_m = 0.0;
    let mut points = Vec::with_capacity((maximum_slant_range_m / step_m).ceil() as usize + 1);
    let mut minimum_gradient = f64::INFINITY;
    let mut encountered_ducting = false;
    loop {
        let gradient = profile.gradient_n_per_km_at(state.height_m);
        minimum_gradient = minimum_gradient.min(gradient);
        let regime = propagation_regime(gradient);
        encountered_ducting |= regime == PropagationRegime::Ducting;
        points.push(RefractedBeamPoint {
            slant_range_m: slant_m,
            ground_range_m: state.ground_m,
            height_above_radar_m: state.height_m,
            elevation_deg: state.elevation_rad.to_degrees(),
            refractivity_n: profile.refractivity_at(state.height_m),
            gradient_n_per_km: gradient,
            regime,
        });
        if slant_m >= maximum_slant_range_m {
            break;
        }
        let ds = step_m.min(maximum_slant_range_m - slant_m);
        state = rk4_step(profile, state, ds);
        slant_m += ds;
    }
    Ok(RefractedBeamTrace {
        points,
        encountered_ducting_layer: encountered_ducting,
        minimum_gradient_n_per_km: minimum_gradient,
    })
}

fn rk4_step(profile: &RefractivityProfile, state: RayState, ds: f64) -> RayState {
    let k1 = derivatives(profile, state);
    let k2 = derivatives(profile, add_scaled(state, k1, ds * 0.5));
    let k3 = derivatives(profile, add_scaled(state, k2, ds * 0.5));
    let k4 = derivatives(profile, add_scaled(state, k3, ds));
    RayState {
        ground_m: state.ground_m
            + ds * (k1.ground_m + 2.0 * k2.ground_m + 2.0 * k3.ground_m + k4.ground_m) / 6.0,
        height_m: state.height_m
            + ds * (k1.height_m + 2.0 * k2.height_m + 2.0 * k3.height_m + k4.height_m) / 6.0,
        elevation_rad: state.elevation_rad
            + ds * (k1.elevation_rad
                + 2.0 * k2.elevation_rad
                + 2.0 * k3.elevation_rad
                + k4.elevation_rad)
                / 6.0,
    }
}

fn add_scaled(state: RayState, derivative: RayState, scale: f64) -> RayState {
    RayState {
        ground_m: state.ground_m + derivative.ground_m * scale,
        height_m: state.height_m + derivative.height_m * scale,
        elevation_rad: state.elevation_rad + derivative.elevation_rad * scale,
    }
}

fn derivatives(profile: &RefractivityProfile, state: RayState) -> RayState {
    let refractivity = profile.refractivity_at(state.height_m);
    let refractive_index = 1.0 + refractivity * 1.0e-6;
    let gradient_n_per_m = profile.gradient_n_per_km_at(state.height_m) / 1_000.0;
    let radius = (EARTH_RADIUS_M + state.height_m).max(EARTH_RADIUS_M * 0.5);
    let cosine = state.elevation_rad.cos();
    RayState {
        // Ground arc on the reference Earth corresponding to displacement
        // along the local tangent of the ray.
        ground_m: cosine * EARTH_RADIUS_M / radius,
        height_m: state.elevation_rad.sin(),
        // The local horizontal rotates downward with Earth curvature (+1/R
        // in relative elevation), while a negative dn/dz bends the ray down.
        elevation_rad: cosine * (1.0 / radius + 1.0e-6 * gradient_n_per_m / refractive_index),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beam_height_above_radar_m;

    fn constant_gradient(gradient_n_per_km: f64) -> RefractivityProfile {
        RefractivityProfile::new(vec![
            RefractivityLevel {
                height_m: 0.0,
                refractivity_n: 320.0,
            },
            RefractivityLevel {
                height_m: 10_000.0,
                refractivity_n: 320.0 + 10.0 * gradient_n_per_km,
            },
        ])
        .unwrap()
    }

    #[test]
    fn itu_refractivity_is_meteorologically_plausible() {
        let dry = radio_refractivity_n_units(101_325.0, 288.15, 0.0).unwrap();
        let moist = radio_refractivity_n_units(101_325.0, 288.15, 0.010).unwrap();
        assert!((dry - 273.0).abs() < 2.0, "dry N was {dry}");
        assert!(
            moist > dry + 40.0,
            "moist N {moist} did not exceed dry {dry}"
        );
        assert!(radio_refractivity_n_units(-1.0, 288.0, 0.01).is_none());
    }

    #[test]
    fn standard_gradient_reproduces_four_thirds_earth_height() {
        let profile = constant_gradient(STANDARD_REFRACTIVITY_GRADIENT_N_PER_KM);
        let trace = trace_refracted_beam(&profile, 0.5, 100_000.0, 250.0).unwrap();
        let traced = trace.points.last().unwrap().height_above_radar_m;
        let analytic = beam_height_above_radar_m(100_000.0, 0.5);
        assert!(
            (traced - analytic).abs() < 15.0,
            "refracted {traced} m versus 4/3-Earth {analytic} m"
        );
    }

    #[test]
    fn superrefraction_lowers_and_subrefraction_raises_beam() {
        let standard = trace_refracted_beam(&constant_gradient(-39.0), 0.5, 150_000.0, 250.0)
            .unwrap()
            .points
            .last()
            .unwrap()
            .height_above_radar_m;
        let super_height = trace_refracted_beam(&constant_gradient(-120.0), 0.5, 150_000.0, 250.0)
            .unwrap()
            .points
            .last()
            .unwrap()
            .height_above_radar_m;
        let sub_height = trace_refracted_beam(&constant_gradient(0.0), 0.5, 150_000.0, 250.0)
            .unwrap()
            .points
            .last()
            .unwrap()
            .height_above_radar_m;
        assert!(super_height < standard);
        assert!(sub_height > standard);
    }

    #[test]
    fn ducting_threshold_is_reported() {
        let profile = constant_gradient(-180.0);
        let trace = trace_refracted_beam(&profile, 0.2, 50_000.0, 100.0).unwrap();
        assert!(trace.encountered_ducting_layer);
        assert_eq!(
            propagation_regime(EARTH_DUCTING_GRADIENT_N_PER_KM - 1.0),
            PropagationRegime::Ducting
        );
    }

    #[test]
    fn profile_interpolation_and_extension_keep_segment_gradient() {
        let profile = RefractivityProfile::new(vec![
            RefractivityLevel {
                height_m: 0.0,
                refractivity_n: 320.0,
            },
            RefractivityLevel {
                height_m: 1_000.0,
                refractivity_n: 280.0,
            },
            RefractivityLevel {
                height_m: 2_000.0,
                refractivity_n: 260.0,
            },
        ])
        .unwrap();
        assert_eq!(profile.refractivity_at(500.0), 300.0);
        assert_eq!(profile.gradient_n_per_km_at(500.0), -40.0);
        assert_eq!(profile.gradient_n_per_km_at(3_000.0), -20.0);
        assert_eq!(profile.refractivity_at(3_000.0), 240.0);
    }
}
