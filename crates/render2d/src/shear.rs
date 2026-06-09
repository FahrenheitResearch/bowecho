//! Azimuthal (rotational) shear via the Linear Least-Squares Derivative (LLSD)
//! method — the operational basis for the MRMS azimuthal-shear / rotation-track
//! products and a primary mesocyclone/tornado detection field.
//!
//! Smith & Elmore (2004), *The use of radial velocity derivatives to diagnose
//! rotation and divergence*, 11th Conf. Aviation, Range & Aerospace Meteorology;
//! Mahalik et al. (2019), *Estimterm… Azimuthal Shear* (MWR/WAF) for the modern
//! LLSD formulation. Azimuthal shear ≈ ∂Vr/∂x across the radial, estimated by a
//! least-squares fit of dealiased radial velocity over a small azimuth×range
//! window. Computed on the DEALIASED velocity field so range folds never
//! manufacture spurious shear.

use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType};

/// Half-width of the LLSD window in azimuth (radials) and range (gates).
const AZ_HALF: isize = 1; // ±1 radial (3 beams)
const RG_HALF: isize = 1; // ±1 gate
/// Output is stored in 10^-3 s^-1 (the conventional shear display unit). The
/// fit is along +azimuth (daz = azr - az0, clockwise), so positive azimuthal
/// shear = Vr increasing toward increasing azimuth (clockwise / right of the
/// down-range beam) — the cyclonic sense in the Northern Hemisphere.
const SHEAR_DISPLAY_SCALE: f32 = 1000.0;

/// Azimuthal shear (×10^-3 s^-1): ∂Vr across the radial. Mesocyclone/TVS
/// rotation detector. NaN = no data.
pub fn azimuthal_shear_grid(cut: &ElevationCut, velocity: &MomentGrid) -> MomentGrid {
    llsd_velocity_derivative(cut, velocity, Axis::Azimuthal)
}

/// Radial divergence (×10^-3 s^-1): ∂Vr along the radial. Positive = divergence
/// (e.g. downburst outflow / DCZ), negative = convergence (gust front / boundary)
/// — the defining derecho signature. Smith & Elmore (2004). NaN = no data.
pub fn radial_divergence_grid(cut: &ElevationCut, velocity: &MomentGrid) -> MomentGrid {
    llsd_velocity_derivative(cut, velocity, Axis::Radial)
}

#[derive(Clone, Copy, PartialEq)]
enum Axis {
    /// Cross-radial (azimuthal) derivative → rotational shear.
    Azimuthal,
    /// Along-radial (range) derivative → divergence/convergence.
    Radial,
}

/// Shared LLSD core: fit v = a + b·x over a small azimuth×range window, where x
/// is cross-radial arc distance (Azimuthal) or along-radial distance (Radial).
/// b is the velocity derivative (s^-1), output scaled to ×10^-3 s^-1.
fn llsd_velocity_derivative(cut: &ElevationCut, velocity: &MomentGrid, axis: Axis) -> MomentGrid {
    let dealiased = crate::dealias_velocity_grid(cut, velocity);
    let rows = dealiased.radial_count();
    let gates = dealiased.gate_range.gate_count;
    let gr = &dealiased.gate_range;
    let spacing = gr.gate_spacing_m as f32;

    let az_deg: Vec<f32> = (0..rows)
        .map(|r| {
            dealiased
                .radial_indices
                .get(r)
                .and_then(|ri| cut.radials.get(*ri))
                .map(|radial| radial.azimuth_deg.rem_euclid(360.0))
                .unwrap_or(f32::NAN)
        })
        .collect();

    let mut out = vec![f32::NAN; rows.saturating_mul(gates)];
    for row in 0..rows {
        let az0 = az_deg[row];
        if !az0.is_finite() {
            continue;
        }
        for gate in 0..gates {
            let r_m = gr.first_gate_m as f32 + gate as f32 * spacing;
            if r_m <= 0.0 {
                continue;
            }
            let (mut sx, mut sv, mut sxx, mut sxv, mut n) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0u32);
            for dr in -AZ_HALF..=AZ_HALF {
                let Some(rr) = row.checked_add_signed(dr) else {
                    continue;
                };
                if rr >= rows {
                    continue;
                }
                let azr = az_deg[rr];
                if !azr.is_finite() {
                    continue;
                }
                // signed azimuth delta (radians, wrapped to [-pi, pi])
                let mut daz = (azr - az0).to_radians();
                while daz > std::f32::consts::PI {
                    daz -= std::f32::consts::TAU;
                }
                while daz < -std::f32::consts::PI {
                    daz += std::f32::consts::TAU;
                }
                for dg in -RG_HALF..=RG_HALF {
                    let Some(gg) = gate.checked_add_signed(dg) else {
                        continue;
                    };
                    if gg >= gates {
                        continue;
                    }
                    let Some(v) = dealiased.scaled_value(rr, gg) else {
                        continue;
                    };
                    if !v.is_finite() {
                        continue;
                    }
                    let x = match axis {
                        Axis::Azimuthal => r_m * daz, // cross-radial arc (m)
                        Axis::Radial => (gg as f32 - gate as f32) * spacing, // along-radial (m)
                    } as f64;
                    let v = v as f64;
                    sx += x;
                    sv += v;
                    sxx += x * x;
                    sxv += x * v;
                    n += 1;
                }
            }
            if n < 4 {
                continue;
            }
            let nf = n as f64;
            let denom = nf * sxx - sx * sx;
            if denom.abs() < 1e-6 {
                continue;
            }
            let slope = (nf * sxv - sx * sv) / denom; // s^-1
            out[row * gates + gate] = (slope as f32) * SHEAR_DISPLAY_SCALE;
        }
    }

    MomentGrid {
        moment: MomentType::Velocity,
        gate_range: gr.clone(),
        scale: 1.0,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        radial_indices: dealiased.radial_indices.clone(),
        storage: MomentStorage::F32(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, Radial};

    /// A pure rotational couplet (Vr increasing linearly across azimuth) must
    /// yield a constant positive azimuthal shear of the expected magnitude.
    #[test]
    fn detects_linear_rotational_shear() {
        let rows = 8usize;
        let gates = 20usize;
        let spacing = 250i32;
        let gate_range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: spacing,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(0.5, None);
        let az_step = 1.0f32; // degrees per radial
        for r in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: r as f32 * az_step,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(60.0),
                radial_status: None,
            });
        }
        // Vr varies with azimuth only: Vr = K * (arc distance). Pick K so shear
        // is a clean value. arc = r_m * d_az_rad; set Vr(row,gate) = SHEAR * r_m * az_rad.
        let shear_true = 0.01f32; // s^-1
        let mut vals = vec![0.0f32; rows * gates];
        for r in 0..rows {
            let az_rad = (r as f32 * az_step).to_radians();
            for g in 0..gates {
                let r_m = 250.0 + g as f32 * spacing as f32;
                vals[r * gates + g] = shear_true * r_m * az_rad;
            }
        }
        let grid = MomentGrid {
            moment: MomentType::Velocity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..rows).collect(),
            storage: MomentStorage::F32(vals),
        };

        let shear = azimuthal_shear_grid(&cut, &grid);
        // interior gate/row should read ~ shear_true * 1000 (×10^-3 s^-1).
        let v = shear.scaled_value(4, 10).expect("shear value");
        assert!(
            (v - shear_true * 1000.0).abs() < 1.0,
            "expected ~{} got {v}",
            shear_true * 1000.0
        );
    }

    #[test]
    fn shear_handles_degraded_velocity_without_panicking() {
        let make = |gates: usize, fill: f32| {
            let gate_range = GateRange {
                first_gate_m: 250,
                gate_spacing_m: 250,
                gate_count: gates,
            };
            let mut cut = ElevationCut::new(0.5, None);
            for r in 0..4 {
                cut.radials.push(Radial {
                    azimuth_deg: r as f32,
                    elevation_deg: 0.5,
                    time_offset_ms: 0,
                    gate_range: gate_range.clone(),
                    nyquist_velocity_mps: Some(30.0),
                    radial_status: None,
                });
            }
            let grid = MomentGrid {
                moment: MomentType::Velocity,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: (0..4).collect(),
                storage: MomentStorage::F32(vec![fill; 4 * gates]),
            };
            (cut, grid)
        };

        // All-NaN velocity → all-NaN shear/divergence, no panic.
        let (cut, grid) = make(12, f32::NAN);
        let shear = azimuthal_shear_grid(&cut, &grid);
        let div = radial_divergence_grid(&cut, &grid);
        for r in 0..shear.radial_count() {
            for g in 0..shear.gate_range.gate_count {
                // no-data F32 cells read back as None or NaN, never finite.
                assert!(shear.scaled_value(r, g).is_none_or(|v| v.is_nan()));
                assert!(div.scaled_value(r, g).is_none_or(|v| v.is_nan()));
            }
        }

        // Zero-gate grid → empty output, no panic.
        let (cut, grid) = make(0, 5.0);
        assert_eq!(azimuthal_shear_grid(&cut, &grid).gate_range.gate_count, 0);
        assert_eq!(radial_divergence_grid(&cut, &grid).gate_range.gate_count, 0);
    }

    /// Vr increasing linearly with range yields a constant positive divergence
    /// of the expected magnitude.
    #[test]
    fn detects_linear_radial_divergence() {
        let rows = 6usize;
        let gates = 24usize;
        let spacing = 250i32;
        let gate_range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: spacing,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(0.5, None);
        for r in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: r as f32,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(80.0),
                radial_status: None,
            });
        }
        let div_true = 0.008f32; // s^-1
        let mut vals = vec![0.0f32; rows * gates];
        for r in 0..rows {
            for g in 0..gates {
                let r_m = 250.0 + g as f32 * spacing as f32;
                vals[r * gates + g] = div_true * r_m; // Vr ∝ range
            }
        }
        let grid = MomentGrid {
            moment: MomentType::Velocity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..rows).collect(),
            storage: MomentStorage::F32(vals),
        };
        let div = radial_divergence_grid(&cut, &grid);
        let v = div.scaled_value(3, 12).expect("divergence value");
        assert!(
            (v - div_true * 1000.0).abs() < 1.0,
            "expected ~{} got {v}",
            div_true * 1000.0
        );
    }
}
