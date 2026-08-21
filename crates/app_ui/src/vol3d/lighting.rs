//! Positive-energy spherical-harmonic lighting for the 3D radar explorer.
//!
//! The environment term is represented as
//!
//! `E(n) = exp(sum_i c_i Y_i(n))`
//!
//! rather than a linear SH series. Fitting the logarithm keeps the reconstructed
//! irradiance strictly positive and avoids the negative lobes that otherwise
//! have to be clamped after truncating a spherical-harmonic expansion.

use std::collections::hash_map::DefaultHasher;
use std::f32::consts::PI;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

pub const SH_L3_COEFFICIENTS: usize = 16;
pub const LIGHT_VOLUME_N: u32 = 96;
pub const LIGHT_VOLUME_NZ: u32 = 24;
pub const LIGHT_WORKGROUP_SIZE: u32 = 4;
pub const MAX_SHADOW_STEPS: u32 = 48;
pub const LIGHTING_ENCODE_MAX: f32 = 2.0;

const FIT_SAMPLES: usize = 256;
const FIT_EPSILON: f32 = 1.0e-5;
const SH_Y00: f64 = 0.282_094_791_773_878_14;
const REGULARIZATION: f64 = 2.0e-5;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Vol3dLightingPreset {
    /// Exactly uniform environment irradiance. Useful for comparison and for
    /// confirming that the lighting cache does not alter palette hue.
    Flat,
    /// Broad sky/fill balance intended for routine radar interpretation.
    Operational,
    /// Stronger sky-ground separation for presentation and storm-structure
    /// inspection.
    Sculpted,
}

impl Vol3dLightingPreset {
    pub const ALL: [Self; 3] = [Self::Flat, Self::Operational, Self::Sculpted];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Flat => "Flat",
            Self::Operational => "Operational",
            Self::Sculpted => "Sculpted",
        }
    }

    pub fn log_sh_coefficients(self) -> [f32; SH_L3_COEFFICIENTS] {
        static OPERATIONAL: OnceLock<[f32; SH_L3_COEFFICIENTS]> = OnceLock::new();
        static SCULPTED: OnceLock<[f32; SH_L3_COEFFICIENTS]> = OnceLock::new();

        match self {
            Self::Flat => [0.0; SH_L3_COEFFICIENTS],
            Self::Operational => *OPERATIONAL.get_or_init(|| {
                fit_log_sh_l3(FIT_SAMPLES, |direction| {
                    positive_environment_target(Self::Operational, direction)
                })
            }),
            Self::Sculpted => *SCULPTED.get_or_init(|| {
                fit_log_sh_l3(FIT_SAMPLES, |direction| {
                    positive_environment_target(Self::Sculpted, direction)
                })
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vol3dLightingSettings {
    pub enabled: bool,
    pub preset: Vol3dLightingPreset,
    /// Meteorological azimuth: clockwise from north, in degrees.
    pub light_azimuth_deg: f32,
    /// Elevation above the horizon, in degrees.
    pub light_elevation_deg: f32,
    pub ambient_strength: f32,
    pub key_strength: f32,
    pub rim_strength: f32,
    /// Blend from unshadowed key light (0) to pseudo-optical self-shadowing (1).
    pub shadow_strength: f32,
    /// Pseudo-extinction multiplier applied to thresholded radar occupancy.
    pub shadow_density: f32,
    pub shadow_steps: u32,
}

impl Default for Vol3dLightingSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            preset: Vol3dLightingPreset::Operational,
            // Closely preserves the old hard-coded light direction while
            // exposing it as an explicit, physically understandable control.
            light_azimuth_deg: 140.0,
            light_elevation_deg: 51.0,
            ambient_strength: 0.62,
            key_strength: 0.82,
            rim_strength: 0.14,
            shadow_strength: 0.72,
            shadow_density: 3.4,
            shadow_steps: 24,
        }
    }
}

impl Vol3dLightingSettings {
    /// Unit vector from a sample toward the key light in BowEcho world axes:
    /// +X east, +Y north, +Z up.
    pub fn light_direction(self) -> [f32; 3] {
        let azimuth = self.light_azimuth_deg.rem_euclid(360.0).to_radians();
        let elevation = self.light_elevation_deg.clamp(-89.0, 89.0).to_radians();
        let horizontal = elevation.cos();
        [
            azimuth.sin() * horizontal,
            azimuth.cos() * horizontal,
            elevation.sin(),
        ]
    }

    pub fn log_sh_coefficients(self) -> [f32; SH_L3_COEFFICIENTS] {
        self.preset.log_sh_coefficients()
    }

    /// Hash only values that affect the compute-cached light volume. The
    /// fragment-only shading blend and rim are intentionally absent, so those
    /// controls remain instant and never rebuild the 3D cache.
    pub fn cache_revision(
        self,
        threshold: f32,
        threshold_high: f32,
        threshold_mode: f32,
        velocity_mode: f32,
        ref_gate: f32,
        zspan: f32,
    ) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.preset.hash(&mut hasher);
        hash_f32(&mut hasher, self.light_azimuth_deg);
        hash_f32(&mut hasher, self.light_elevation_deg);
        hash_f32(&mut hasher, self.ambient_strength);
        hash_f32(&mut hasher, self.key_strength);
        hash_f32(&mut hasher, self.shadow_strength);
        hash_f32(&mut hasher, self.shadow_density);
        self.shadow_steps
            .clamp(1, MAX_SHADOW_STEPS)
            .hash(&mut hasher);
        hash_f32(&mut hasher, threshold);
        hash_f32(&mut hasher, threshold_high);
        hash_f32(&mut hasher, threshold_mode);
        hash_f32(&mut hasher, velocity_mode);
        hash_f32(&mut hasher, ref_gate);
        hash_f32(&mut hasher, zspan);
        hasher.finish()
    }
}

fn hash_f32(hasher: &mut DefaultHasher, value: f32) {
    value.to_bits().hash(hasher);
}

/// Mix a settings revision with the generation counter of the texture that was
/// actually uploaded. This is deliberately based on the upload, not the UI's
/// requested volume key: a worker may finish after the requested key changed.
pub fn combine_cache_revision(settings_revision: u64, volume_generation: u64) -> u64 {
    settings_revision
        ^ volume_generation
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .rotate_left(23)
}

/// Real, orthonormal spherical-harmonic basis through degree 3. Ordering is
/// shared verbatim with `light_volume.wgsl`:
///
/// `Y00; Y1-1,Y10,Y11; Y2-2..Y22; Y3-3..Y33`.
pub fn real_sh_l3([x, y, z]: [f32; 3]) -> [f32; SH_L3_COEFFICIENTS] {
    [
        0.282_094_8,
        0.488_602_52 * y,
        0.488_602_52 * z,
        0.488_602_52 * x,
        1.092_548_5 * x * y,
        1.092_548_5 * y * z,
        0.315_391_57 * (3.0 * z * z - 1.0),
        1.092_548_5 * x * z,
        0.546_274_24 * (x * x - y * y),
        0.590_043_6 * y * (3.0 * x * x - y * y),
        2.890_611_4 * x * y * z,
        0.457_045_8 * y * (5.0 * z * z - 1.0),
        0.373_176_34 * z * (5.0 * z * z - 3.0),
        0.457_045_8 * x * (5.0 * z * z - 1.0),
        1.445_305_7 * z * (x * x - y * y),
        0.590_043_6 * x * (x * x - 3.0 * y * y),
    ]
}

pub fn evaluate_log_sh_l3(coefficients: &[f32; SH_L3_COEFFICIENTS], direction: [f32; 3]) -> f32 {
    real_sh_l3(direction)
        .iter()
        .zip(coefficients)
        .map(|(basis, coefficient)| *basis * *coefficient)
        .sum()
}

#[cfg(test)]
pub fn evaluate_positive_sh_l3(
    coefficients: &[f32; SH_L3_COEFFICIENTS],
    direction: [f32; 3],
) -> f32 {
    evaluate_log_sh_l3(coefficients, direction)
        .clamp(-12.0, 6.0)
        .exp()
}

/// Weighted least-squares fit in log space. Fibonacci samples are equal-area,
/// so every sample receives equal weight. A degree-weighted Tikhonov term
/// suppresses unstable high-order coefficients. Finally, the constant term is
/// corrected in linear space so the fitted and source mean energies agree.
#[allow(clippy::needless_range_loop)]
pub fn fit_log_sh_l3(
    sample_count: usize,
    mut positive_target: impl FnMut([f32; 3]) -> f32,
) -> [f32; SH_L3_COEFFICIENTS] {
    let sample_count = sample_count.max(SH_L3_COEFFICIENTS * 4);
    let samples = (0..sample_count)
        .map(|index| {
            let direction = fibonacci_direction(index, sample_count);
            let target = positive_target(direction).max(FIT_EPSILON);
            (direction, target)
        })
        .collect::<Vec<_>>();

    let mut normal = [[0.0_f64; SH_L3_COEFFICIENTS]; SH_L3_COEFFICIENTS];
    let mut rhs = [0.0_f64; SH_L3_COEFFICIENTS];

    for (direction, target) in &samples {
        let basis = real_sh_l3(*direction).map(f64::from);
        let log_target = f64::from(*target).ln();
        for row in 0..SH_L3_COEFFICIENTS {
            rhs[row] += basis[row] * log_target;
            for column in 0..SH_L3_COEFFICIENTS {
                normal[row][column] += basis[row] * basis[column];
            }
        }
    }

    for (index, row) in normal.iter_mut().enumerate() {
        let degree = degree_for_index(index) as f64;
        let laplacian = degree * (degree + 1.0);
        row[index] += REGULARIZATION * laplacian * laplacian + 1.0e-12;
    }

    let solved = cholesky_solve(normal, rhs).unwrap_or_default();
    let mut coefficients = solved.map(|value| value as f32);

    let source_mean = samples
        .iter()
        .map(|(_, target)| f64::from(*target))
        .sum::<f64>()
        / sample_count as f64;
    let fitted_mean = samples
        .iter()
        .map(|(direction, _)| {
            f64::from(evaluate_log_sh_l3(&coefficients, *direction))
                .clamp(-30.0, 20.0)
                .exp()
        })
        .sum::<f64>()
        / sample_count as f64;
    if source_mean.is_finite() && fitted_mean.is_finite() && fitted_mean > 0.0 {
        coefficients[0] += ((source_mean / fitted_mean).ln() / SH_Y00) as f32;
    }

    coefficients
}

fn positive_environment_target(preset: Vol3dLightingPreset, direction: [f32; 3]) -> f32 {
    let [x, y, z] = direction;
    let up = z.max(0.0);
    let down = (-z).max(0.0);
    let horizon = (1.0 - z * z).max(0.0).sqrt();
    let fill_direction = normalize([-0.55, 0.30, 0.78]);
    let directional_fill = dot([x, y, z], fill_direction).max(0.0);

    let irradiance = match preset {
        Vol3dLightingPreset::Flat => 1.0,
        Vol3dLightingPreset::Operational => {
            0.56 + 0.43 * up + 0.08 * horizon + 0.035 * directional_fill.powi(2) - 0.04 * down
        }
        Vol3dLightingPreset::Sculpted => {
            0.34 + 0.62 * up.powf(0.65) + 0.12 * horizon + 0.10 * directional_fill.powi(3)
                - 0.055 * down
        }
    };
    irradiance.max(FIT_EPSILON)
}

fn fibonacci_direction(index: usize, count: usize) -> [f32; 3] {
    let z = 1.0 - 2.0 * (index as f32 + 0.5) / count as f32;
    let radius = (1.0 - z * z).max(0.0).sqrt();
    let golden_angle = PI * (3.0 - 5.0_f32.sqrt());
    let azimuth = golden_angle * index as f32;
    [radius * azimuth.cos(), radius * azimuth.sin(), z]
}

fn degree_for_index(index: usize) -> usize {
    match index {
        0 => 0,
        1..=3 => 1,
        4..=8 => 2,
        _ => 3,
    }
}

#[allow(clippy::needless_range_loop)]
fn cholesky_solve(
    matrix: [[f64; SH_L3_COEFFICIENTS]; SH_L3_COEFFICIENTS],
    rhs: [f64; SH_L3_COEFFICIENTS],
) -> Option<[f64; SH_L3_COEFFICIENTS]> {
    let mut lower = [[0.0_f64; SH_L3_COEFFICIENTS]; SH_L3_COEFFICIENTS];
    for row in 0..SH_L3_COEFFICIENTS {
        for column in 0..=row {
            let mut value = matrix[row][column];
            for inner in 0..column {
                value -= lower[row][inner] * lower[column][inner];
            }
            if row == column {
                if !value.is_finite() || value <= 1.0e-14 {
                    return None;
                }
                lower[row][column] = value.sqrt();
            } else {
                lower[row][column] = value / lower[column][column];
            }
        }
    }

    let mut forward = [0.0_f64; SH_L3_COEFFICIENTS];
    for row in 0..SH_L3_COEFFICIENTS {
        let mut value = rhs[row];
        for column in 0..row {
            value -= lower[row][column] * forward[column];
        }
        forward[row] = value / lower[row][row];
    }

    let mut solution = [0.0_f64; SH_L3_COEFFICIENTS];
    for row in (0..SH_L3_COEFFICIENTS).rev() {
        let mut value = forward[row];
        for column in row + 1..SH_L3_COEFFICIENTS {
            value -= lower[column][row] * solution[column];
        }
        solution[row] = value / lower[row][row];
    }
    Some(solution)
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn normalize(vector: [f32; 3]) -> [f32; 3] {
    let length = dot(vector, vector).sqrt().max(1.0e-8);
    [vector[0] / length, vector[1] / length, vector[2] / length]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_positive_field_is_recovered() {
        let coefficients = fit_log_sh_l3(128, |_| 0.73);
        for index in 0..1024 {
            let direction = fibonacci_direction(index, 1024);
            let reconstructed = evaluate_positive_sh_l3(&coefficients, direction);
            assert!((reconstructed - 0.73).abs() < 2.0e-4);
        }
    }

    #[test]
    fn fitted_presets_stay_positive_and_finite() {
        for preset in Vol3dLightingPreset::ALL {
            let coefficients = preset.log_sh_coefficients();
            for index in 0..4096 {
                let direction = fibonacci_direction(index, 4096);
                let value = evaluate_positive_sh_l3(&coefficients, direction);
                assert!(value.is_finite() && value > 0.0, "{preset:?}: {value}");
            }
        }
    }

    #[test]
    fn broad_preset_fit_error_is_small() {
        for (preset, maximum_relative_rmse) in [
            (Vol3dLightingPreset::Operational, 0.04),
            (Vol3dLightingPreset::Sculpted, 0.10),
        ] {
            let coefficients = preset.log_sh_coefficients();
            let mut squared_relative_error = 0.0;
            let count = 4096;
            for index in 0..count {
                let direction = fibonacci_direction(index, count);
                let expected = positive_environment_target(preset, direction);
                let actual = evaluate_positive_sh_l3(&coefficients, direction);
                squared_relative_error += ((actual - expected) / expected).powi(2);
            }
            let relative_rmse = (squared_relative_error / count as f32).sqrt();
            assert!(
                relative_rmse < maximum_relative_rmse,
                "{preset:?} relative RMSE {relative_rmse}"
            );
        }
    }

    #[test]
    fn meteorological_azimuth_maps_to_world_axes() {
        let mut settings = Vol3dLightingSettings {
            light_elevation_deg: 0.0,
            light_azimuth_deg: 0.0,
            ..Default::default()
        };
        let north = settings.light_direction();
        assert!(north[0].abs() < 1.0e-6 && (north[1] - 1.0).abs() < 1.0e-6);

        settings.light_azimuth_deg = 90.0;
        let east = settings.light_direction();
        assert!((east[0] - 1.0).abs() < 1.0e-6 && east[1].abs() < 1.0e-6);
    }

    #[test]
    fn compute_cache_revision_changes_only_for_compute_inputs() {
        let settings = Vol3dLightingSettings::default();
        let base = settings.cache_revision(0.3, 0.8, 0.0, 0.0, 0.2, 0.6);
        let mut rim_only = settings;
        rim_only.rim_strength += 0.1;
        assert_eq!(base, rim_only.cache_revision(0.3, 0.8, 0.0, 0.0, 0.2, 0.6));

        let mut changed = settings;
        changed.shadow_density += 0.1;
        assert_ne!(base, changed.cache_revision(0.3, 0.8, 0.0, 0.0, 0.2, 0.6));
    }

    #[test]
    fn uploaded_volume_generation_invalidates_the_cache() {
        let settings_revision = 0x1234_5678_9abc_def0;
        let first = combine_cache_revision(settings_revision, 7);
        let repeated = combine_cache_revision(settings_revision, 7);
        let next_upload = combine_cache_revision(settings_revision, 8);
        assert_eq!(first, repeated);
        assert_ne!(first, next_upload);
    }
}
