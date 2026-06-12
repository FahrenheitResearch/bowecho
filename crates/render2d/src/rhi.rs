//! Native RHI (range-height indicator) sweep rendering.
//!
//! An RHI sweep holds the antenna at a fixed azimuth and sweeps in elevation,
//! so each radial is one beam in the vertical plane through the radar. The
//! display is a range-height panel: x = ground range from the radar, y =
//! height above the radar. This module resamples one RHI cut onto that
//! Cartesian panel by inverting the 4/3-Earth beam-propagation model
//! (Doviak & Zrnić 1993, *Doppler Radar and Weather Observations*, 2nd ed.,
//! eq. 2.28b/c — the same model the volume cross-sections use) and sampling
//! the nearest beam/gate, which is how operational RHI displays (soloii,
//! lrose HawkEye) draw native RHIs.
//!
//! This is distinct from the *reconstructed* RHI in [`crate::volumetric`]
//! (which interpolates between PPI tilts along an arbitrary ground path):
//! a native RHI has dense elevation sampling in a single plane, so nearest-
//! beam lookup preserves the fine vertical structure mobile crews scan RHIs
//! for (vault/BWER edges, descending reflectivity cores, TVS columns).

use radar_core::{EFFECTIVE_EARTH_RADIUS_M, ElevationCut, MomentGrid};
use rayon::prelude::*;

use crate::volumetric::CrossSection;

/// Maximum gap between a requested elevation angle and the nearest recorded
/// beam before a pixel is left empty (degrees). Mobile X-band beamwidths run
/// 0.45° (RaXPol) to ~0.93° (DOW); 1.0° keeps the fan contiguous between
/// beams without smearing data into the unscanned wedge above the top beam.
const MAX_BEAM_GAP_DEG: f32 = 1.0;

/// `true` when the cut's radials look like an elevation sweep at a fixed
/// azimuth — the geometric signature of an RHI. Used as a fallback when the
/// source format did not declare a scan mode.
pub fn cut_looks_like_rhi(cut: &ElevationCut) -> bool {
    if cut.radials.len() < 8 {
        return false;
    }
    let mut elev_min = f32::INFINITY;
    let mut elev_max = f32::NEG_INFINITY;
    // Circular spread of azimuths via resultant-vector length.
    let (mut sin_sum, mut cos_sum) = (0.0f64, 0.0f64);
    for radial in &cut.radials {
        elev_min = elev_min.min(radial.elevation_deg);
        elev_max = elev_max.max(radial.elevation_deg);
        let az = f64::from(radial.azimuth_deg).to_radians();
        sin_sum += az.sin();
        cos_sum += az.cos();
    }
    let resultant = (sin_sum.hypot(cos_sum) / cut.radials.len() as f64).clamp(0.0, 1.0);
    // Mean angular deviation ~ sqrt(2(1-R)) rad; require < ~3° azimuth spread
    // and > 10° of elevation sweep.
    let azimuth_spread_deg = (2.0 * (1.0 - resultant)).sqrt().to_degrees();
    elev_max - elev_min > 10.0 && azimuth_spread_deg < 3.0
}

/// Circular-mean azimuth of the cut's radials (degrees, [0, 360)). For an
/// RHI this is the fixed pointing azimuth of the whole sweep.
pub fn rhi_fixed_azimuth_deg(cut: &ElevationCut) -> f32 {
    let (mut sin_sum, mut cos_sum) = (0.0f64, 0.0f64);
    for radial in &cut.radials {
        let az = f64::from(radial.azimuth_deg).to_radians();
        sin_sum += az.sin();
        cos_sum += az.cos();
    }
    (sin_sum.atan2(cos_sum).to_degrees().rem_euclid(360.0)) as f32
}

/// Highest beam height (m, above the radar) reached anywhere in the sweep —
/// the natural top of the RHI panel.
pub fn rhi_coverage_top_m(cut: &ElevationCut, grid: &MomentGrid) -> f32 {
    let max_slant_m = f64::from(grid.gate_range.first_gate_m)
        + f64::from(grid.gate_range.gate_spacing_m) * grid.gate_range.gate_count as f64;
    cut.radials
        .iter()
        .map(|radial| {
            radar_core::beam_height_above_radar_m(max_slant_m, f64::from(radial.elevation_deg))
                as f32
        })
        .fold(0.0f32, f32::max)
}

/// Furthest ground range (m) reached by any gate in the sweep.
pub fn rhi_coverage_range_m(cut: &ElevationCut, grid: &MomentGrid) -> f32 {
    let max_slant_m = f64::from(grid.gate_range.first_gate_m)
        + f64::from(grid.gate_range.gate_spacing_m) * grid.gate_range.gate_count as f64;
    cut.radials
        .iter()
        .map(|radial| {
            radar_core::beam_ground_range_m(max_slant_m, f64::from(radial.elevation_deg)) as f32
        })
        .fold(0.0f32, f32::max)
}

/// Invert the 4/3-Earth beam model: ground range `s` and height-above-radar
/// `z` (both m) to (slant range m, elevation deg).
///
/// Geometry: the radar sits at radius `aₑ` from the effective Earth center
/// and the target at radius `aₑ + z`, separated by central angle `φ = s/aₑ`.
/// Law of cosines gives the slant range; the elevation angle is the angle of
/// the radar→target vector above the radar's local horizontal. This is the
/// exact inverse of Doviak & Zrnić (1993) eq. 2.28b/c as implemented by
/// [`radar_core::beam_height_above_radar_m`] / [`radar_core::beam_ground_range_m`].
fn invert_beam_geometry(ground_range_m: f64, height_m: f64) -> (f64, f64) {
    let ae = EFFECTIVE_EARTH_RADIUS_M;
    let target_radius = ae + height_m;
    let phi = ground_range_m / ae;
    let (sin_phi, cos_phi) = phi.sin_cos();
    let slant =
        (ae * ae + target_radius * target_radius - 2.0 * ae * target_radius * cos_phi).sqrt();
    let elevation_rad = (target_radius * cos_phi - ae).atan2(target_radius * sin_phi);
    (slant, elevation_rad.to_degrees())
}

/// Resample one native RHI sweep onto a range-height panel.
///
/// `values[y * width + x]` holds the moment value at ground range
/// `x/(width-1) * max_range_m` and height `top_m * (1 - y/(height-1))`
/// above the radar (NaN = no data) — the same layout as
/// [`CrossSection`], so panels can share rendering code. Each pixel is
/// inverse-mapped to (slant range, elevation) and filled from the nearest
/// recorded beam within [`MAX_BEAM_GAP_DEG`] and the nearest gate.
pub fn rhi_section(
    cut: &ElevationCut,
    grid: &MomentGrid,
    width: usize,
    height: usize,
    top_m: f32,
    max_range_m: f32,
) -> Option<CrossSection> {
    if width < 2 || height < 2 || top_m <= 0.0 || max_range_m <= 0.0 {
        return None;
    }
    if grid.gate_range.gate_spacing_m <= 0 || grid.gate_range.gate_count == 0 {
        return None;
    }
    // (elevation, grid row) sorted by elevation for nearest-beam lookup.
    let mut beams: Vec<(f32, usize)> = grid
        .radial_indices
        .iter()
        .enumerate()
        .filter_map(|(row, radial_index)| {
            let radial = cut.radials.get(*radial_index)?;
            radial
                .elevation_deg
                .is_finite()
                .then_some((radial.elevation_deg, row))
        })
        .collect();
    if beams.is_empty() {
        return None;
    }
    beams.sort_by(|a, b| a.0.total_cmp(&b.0));

    let first_gate_m = f64::from(grid.gate_range.first_gate_m);
    let spacing_m = f64::from(grid.gate_range.gate_spacing_m);
    let gate_count = grid.gate_range.gate_count;

    let rows: Vec<Vec<f32>> = (0..height)
        .into_par_iter()
        .map(|y| {
            let z = f64::from(top_m) * (1.0 - y as f64 / (height - 1) as f64);
            let mut row = vec![f32::NAN; width];
            for (x, cell) in row.iter_mut().enumerate() {
                let s = f64::from(max_range_m) * x as f64 / (width - 1) as f64;
                let (slant_m, elevation_deg) = invert_beam_geometry(s, z);
                let gate = (slant_m - first_gate_m) / spacing_m;
                if gate < -0.5 || gate >= gate_count as f64 - 0.5 {
                    continue;
                }
                let gate = gate.round().max(0.0) as usize;
                let Some((beam_elev, beam_row)) = nearest_beam(&beams, elevation_deg as f32) else {
                    continue;
                };
                if (beam_elev - elevation_deg as f32).abs() > MAX_BEAM_GAP_DEG {
                    continue;
                }
                if let Some(value) = grid.scaled_value(beam_row, gate) {
                    *cell = value;
                }
            }
            row
        })
        .collect();

    let mut values = vec![f32::NAN; width * height];
    for (y, row) in rows.into_iter().enumerate() {
        values[y * width..(y + 1) * width].copy_from_slice(&row);
    }
    Some(CrossSection {
        width,
        height,
        top_m,
        length_m: max_range_m,
        values,
    })
}

/// Binary search the elevation-sorted beam list for the closest beam.
fn nearest_beam(beams: &[(f32, usize)], elevation_deg: f32) -> Option<(f32, usize)> {
    let index = beams.partition_point(|(elev, _)| *elev < elevation_deg);
    let after = beams.get(index);
    let before = index.checked_sub(1).and_then(|i| beams.get(i));
    match (before, after) {
        (Some(b), Some(a)) => {
            if (elevation_deg - b.0).abs() <= (a.0 - elevation_deg).abs() {
                Some(*b)
            } else {
                Some(*a)
            }
        }
        (Some(b), None) => Some(*b),
        (None, Some(a)) => Some(*a),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentStorage, MomentType, Radial};

    /// Synthetic RHI: beams every 0.5° from 0.5° to 30° at azimuth 271°,
    /// each beam filled with its own elevation index so samples are
    /// attributable to a specific beam.
    fn rhi_cut(gates: usize, spacing_m: i32) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: spacing_m,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(271.0, None);
        let mut storage = Vec::new();
        let mut radial_indices = Vec::new();
        for k in 0..60usize {
            let elevation = 0.5 + k as f32 * 0.5;
            cut.radials.push(Radial {
                azimuth_deg: 271.0,
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            storage.extend(std::iter::repeat_n(k as f32, gates));
            radial_indices.push(k);
        }
        let grid = MomentGrid {
            moment: MomentType::Reflectivity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices,
            storage: MomentStorage::F32(storage),
        };
        (cut, grid)
    }

    #[test]
    fn beam_geometry_inversion_round_trips() {
        for &(slant, elev) in &[
            (5_000.0f64, 0.5f64),
            (30_000.0, 4.0),
            (60_000.0, 12.0),
            (20_000.0, 45.0),
            (10_000.0, 80.0),
        ] {
            let z = radar_core::beam_height_above_radar_m(slant, elev);
            let s = radar_core::beam_ground_range_m(slant, elev);
            let (slant_back, elev_back) = invert_beam_geometry(s, z);
            assert!(
                (slant_back - slant).abs() < 0.5,
                "slant {slant} -> {slant_back}"
            );
            assert!(
                (elev_back - elev).abs() < 0.01,
                "elev {elev} -> {elev_back}"
            );
        }
    }

    #[test]
    fn rhi_section_samples_the_matching_beam() {
        let (cut, grid) = rhi_cut(400, 150); // 60 km of gates
        let top_m = 12_000.0f32;
        let max_range_m = 50_000.0f32;
        let (w, h) = (200usize, 100usize);
        let section = rhi_section(&cut, &grid, w, h, top_m, max_range_m).expect("section");

        // Pick a beam (k = 20 → 10.5° elevation) and a slant range, project
        // to panel coordinates, and verify the sampled value is that beam's.
        let elev = 10.5f64;
        let slant = 30_000.0f64;
        let z = radar_core::beam_height_above_radar_m(slant, elev) as f32;
        let s = radar_core::beam_ground_range_m(slant, elev) as f32;
        let x = (s / max_range_m * (w - 1) as f32).round() as usize;
        let y = ((1.0 - z / top_m) * (h - 1) as f32).round() as usize;
        let sampled = section.values[y * w + x];
        assert!(
            (sampled - 20.0).abs() <= 1.0,
            "expected beam ~20 at ({x},{y}), got {sampled}"
        );
    }

    #[test]
    fn rhi_section_is_empty_above_the_top_beam() {
        let (cut, grid) = rhi_cut(400, 150);
        let section = rhi_section(&cut, &grid, 200, 100, 12_000.0, 50_000.0).expect("section");
        // 10 km up at 45 km out needs ~12.5° of elevation — covered. But at
        // 8 km out, 10 km up needs ~51°, far above the 30° top beam: empty.
        let x = (8_000.0f32 / 50_000.0 * 199.0).round() as usize;
        let y = ((1.0 - 10_000.0f32 / 12_000.0) * 99.0).round() as usize;
        assert!(section.values[y * 200 + x].is_nan());
    }

    #[test]
    fn rhi_section_is_empty_beyond_gate_coverage() {
        let (cut, grid) = rhi_cut(100, 150); // only 15 km of gates
        let section = rhi_section(&cut, &grid, 200, 100, 12_000.0, 50_000.0).expect("section");
        // 40 km out is far past the last gate on every beam.
        let x = (40_000.0f32 / 50_000.0 * 199.0).round() as usize;
        let y = 99; // near the surface
        assert!(section.values[y * 200 + x].is_nan());
    }

    #[test]
    fn rhi_heuristic_accepts_elevation_sweeps_and_rejects_ppi() {
        let (rhi, _) = rhi_cut(64, 250);
        assert!(cut_looks_like_rhi(&rhi));
        assert!((rhi_fixed_azimuth_deg(&rhi) - 271.0).abs() < 1e-3);

        // A PPI cut: fixed elevation, azimuth sweep.
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 250,
            gate_count: 16,
        };
        let mut ppi = ElevationCut::new(0.5, None);
        for k in 0..360 {
            ppi.radials.push(Radial {
                azimuth_deg: k as f32,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        assert!(!cut_looks_like_rhi(&ppi));
    }

    #[test]
    fn rhi_coverage_extents_track_the_sweep() {
        let (cut, grid) = rhi_cut(400, 150); // 60 km, up to 30°
        let top = rhi_coverage_top_m(&cut, &grid);
        // 60 km at 30° elevation is ~30.2 km high.
        assert!((29_000.0..32_500.0).contains(&top), "top was {top}");
        let range = rhi_coverage_range_m(&cut, &grid);
        // Lowest beam carries gates nearly the full 60 km downrange.
        assert!((58_000.0..60_500.0).contains(&range), "range was {range}");
    }

    #[test]
    fn azimuth_circular_mean_handles_north_wrap() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 250,
            gate_count: 4,
        };
        let mut cut = ElevationCut::new(359.0, None);
        for (az, elev) in [(359.0f32, 1.0f32), (1.0, 2.0), (0.0, 3.0)] {
            cut.radials.push(Radial {
                azimuth_deg: az,
                elevation_deg: elev,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        let mean = rhi_fixed_azimuth_deg(&cut);
        assert!(!(0.5..=359.5).contains(&mean), "mean was {mean}");
    }
}
