//! Rotation detection: cluster LLSD azimuthal-shear maxima into meso / TVS
//! candidate sites, in the spirit of the WSR-88D MDA/TDA thresholds
//! (Stumpf et al. 1998 WAF 13(2) mesocyclone detection; Mitchell et al. 1998
//! WAF 13(2) tornado detection; LLSD shear per Smith & Elmore 2004 and
//! Mahalik et al. 2019). Operates on a single tilt's dealiased velocity.

use radar_core::{ElevationCut, MomentGrid};

use crate::dealias_velocity_grid;
use crate::shear::azimuthal_shear_grid;

/// Azimuthal shear magnitude (s⁻¹) that flags possible mesocyclonic rotation.
pub const MESO_SHEAR_THRESHOLD: f32 = 0.006;
/// Stronger-rotation threshold suggesting a tornadic vortex signature.
pub const TVS_SHEAR_THRESHOLD: f32 = 0.020;
/// Shear above this is physically implausible rotation — clutter/noise.
const MAX_PLAUSIBLE_SHEAR: f32 = 0.15;
/// Couplet strength requirements (max-min dealiased velocity in/near the
/// cluster), the discriminator the WSR-88D MDA leans on.
const MESO_DELTA_V_MPS: f32 = 22.0;
const TVS_DELTA_V_MPS: f32 = 40.0;
/// Minimum clustered gates for a site (rejects single-gate noise).
const MIN_CLUSTER_GATES: usize = 6;
/// Compact-circulation cap: real mesos span at most a few km — huge flagged
/// regions are noise fields or convergence lines, not vortices.
const MAX_CLUSTER_GATES: usize = 600;
/// Range window: inside ~15 km the cross-beam distance is so small that
/// noise produces enormous shear; beyond ~150 km the beam is too wide/high.
const MIN_RANGE_M: f64 = 15_000.0;
const MAX_RANGE_M: f64 = 150_000.0;
/// Cap on reported sites (strongest first).
const MAX_SITES: usize = 12;

/// A detected rotation site on one tilt.
#[derive(Clone, Copy, Debug)]
pub struct RotationSite {
    /// Beam-centre azimuth of the shear peak (degrees clockwise from north).
    pub azimuth_deg: f32,
    /// Ground range of the shear peak from the radar (m).
    pub ground_range_m: f64,
    /// Peak azimuthal shear (s⁻¹); positive = cyclonic.
    pub peak_shear_s: f32,
    /// Number of gates in the cluster (size proxy).
    pub gate_count: usize,
    /// Peak exceeds the TVS-like threshold.
    pub tvs: bool,
}

/// Detect rotation sites on a tilt: threshold |azimuthal shear|, flood-fill
/// clusters on the polar grid, keep significant ones, strongest first.
pub fn detect_rotation_sites(cut: &ElevationCut, velocity: &MomentGrid) -> Vec<RotationSite> {
    let shear = azimuthal_shear_grid(cut, velocity);
    let rows = shear.radial_count();
    let gates = shear.gate_range.gate_count;
    if rows == 0 || gates == 0 {
        return Vec::new();
    }
    let spacing_m = shear.gate_range.gate_spacing_m.max(1) as f64;
    let first_gate_m = shear.gate_range.first_gate_m as f64;

    // Display grid is scaled ×1000; work in s⁻¹.
    let shear_at = |row: usize, gate: usize| -> Option<f32> {
        shear
            .scaled_value(row, gate)
            .filter(|v| v.is_finite())
            .map(|v| v / 1000.0)
    };

    let mut flagged = vec![false; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            let range = first_gate_m + gate as f64 * spacing_m;
            if range > MAX_RANGE_M {
                break;
            }
            if range < MIN_RANGE_M {
                continue;
            }
            if let Some(s) = shear_at(row, gate)
                && s.abs() >= MESO_SHEAR_THRESHOLD
                && s.abs() <= MAX_PLAUSIBLE_SHEAR
            {
                flagged[row * gates + gate] = true;
            }
        }
    }

    // Dealiased velocity for the couplet (delta-V) check.
    let dealiased = dealias_velocity_grid(cut, velocity);
    let vel_at = |row: usize, gate: usize| -> Option<f32> {
        dealiased.scaled_value(row, gate).filter(|v| v.is_finite())
    };

    // Flood-fill clusters (4-connected, azimuth wraps).
    let mut visited = vec![false; rows * gates];
    let mut sites = Vec::new();
    let mut stack = Vec::new();
    for seed in 0..rows * gates {
        if !flagged[seed] || visited[seed] {
            continue;
        }
        stack.clear();
        stack.push(seed);
        visited[seed] = true;
        let mut count = 0usize;
        let mut peak = 0.0f32;
        let mut peak_cell = seed;
        while let Some(cell) = stack.pop() {
            count += 1;
            let (row, gate) = (cell / gates, cell % gates);
            if let Some(s) = shear_at(row, gate)
                && s.abs() > peak.abs()
            {
                peak = s;
                peak_cell = cell;
            }
            let mut push = |r: usize, g: usize| {
                let idx = r * gates + g;
                if flagged[idx] && !visited[idx] {
                    visited[idx] = true;
                    stack.push(idx);
                }
            };
            push((row + 1) % rows, gate);
            push((row + rows - 1) % rows, gate);
            if gate + 1 < gates {
                push(row, gate + 1);
            }
            if gate > 0 {
                push(row, gate - 1);
            }
        }
        if !(MIN_CLUSTER_GATES..=MAX_CLUSTER_GATES).contains(&count) {
            continue;
        }
        let (peak_row, peak_gate) = (peak_cell / gates, peak_cell % gates);
        // Couplet strength: max-min dealiased velocity in an azimuthal
        // neighbourhood across the peak (the MDA delta-V discriminator).
        let mut v_min = f32::INFINITY;
        let mut v_max = f32::NEG_INFINITY;
        for dr in -4i64..=4 {
            let r = ((peak_row as i64 + dr).rem_euclid(rows as i64)) as usize;
            for dg in -2i64..=2 {
                let g = peak_gate as i64 + dg;
                if g < 0 || g >= gates as i64 {
                    continue;
                }
                if let Some(v) = vel_at(r, g as usize) {
                    v_min = v_min.min(v);
                    v_max = v_max.max(v);
                }
            }
        }
        let delta_v = if v_max >= v_min { v_max - v_min } else { 0.0 };
        if delta_v < MESO_DELTA_V_MPS {
            continue;
        }
        let Some(radial_index) = shear.radial_indices.get(peak_row).copied() else {
            continue;
        };
        let Some(radial) = cut.radials.get(radial_index) else {
            continue;
        };
        sites.push(RotationSite {
            azimuth_deg: radial.azimuth_deg,
            ground_range_m: first_gate_m + peak_gate as f64 * spacing_m,
            peak_shear_s: peak,
            gate_count: count,
            tvs: peak.abs() >= TVS_SHEAR_THRESHOLD && delta_v >= TVS_DELTA_V_MPS,
        });
    }
    sites.sort_by(|a, b| b.peak_shear_s.abs().total_cmp(&a.peak_shear_s.abs()));
    sites.truncate(MAX_SITES);
    sites
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentStorage, MomentType, Radial};

    /// A synthetic cyclonic couplet: inbound on one side, outbound on the
    /// other, across a few radials at a known az/range.
    #[test]
    fn detects_a_synthetic_couplet() {
        let gates = 200usize;
        let rows = 360usize;
        let gate_range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(0.5, None);
        for r in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: r as f32,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(60.0),
                radial_status: None,
            });
        }
        let mut data = vec![0.0f32; rows * gates];
        // Couplet centred at az 90°, gates 80..90 (~20-22 km): ±25 m/s across
        // four radials -> azimuthal shear well above the meso threshold.
        for (row, sign) in [(88usize, -1.0f32), (89, -1.0), (91, 1.0), (92, 1.0)] {
            for gate in 78..94 {
                data[row * gates + gate] = sign * 25.0;
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
            storage: MomentStorage::F32(data),
        };
        let sites = detect_rotation_sites(&cut, &grid);
        assert!(!sites.is_empty(), "couplet not detected");
        let best = &sites[0];
        assert!(
            (best.azimuth_deg - 90.0).abs() <= 3.0,
            "azimuth {} not near 90",
            best.azimuth_deg
        );
        assert!(
            (best.ground_range_m - 21_000.0).abs() < 4_000.0,
            "range {} not near 21 km",
            best.ground_range_m
        );
        assert!(best.peak_shear_s > 0.0, "couplet should read cyclonic");
    }

    #[test]
    fn quiet_field_detects_nothing() {
        let gates = 100usize;
        let gate_range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(0.5, None);
        for r in 0..120 {
            cut.radials.push(Radial {
                azimuth_deg: r as f32 * 3.0,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(60.0),
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
            radial_indices: (0..120).collect(),
            storage: MomentStorage::F32(vec![5.0; 120 * gates]),
        };
        assert!(detect_rotation_sites(&cut, &grid).is_empty());
    }
}
