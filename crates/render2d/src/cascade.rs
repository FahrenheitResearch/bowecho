//! Tilt-cascade dealiasing — the second dealias engine.
//!
//! The region-based unfolder resolves folds from boundary evidence alone,
//! which leaves each connected group's absolute BRANCH under-determined; on
//! sweeps with widespread aliasing a same-sweep VAD reference is circular
//! (wrapped gates poison the fit — see docs/dealias-fold-branch-analysis.md).
//!
//! The cascade breaks the circularity vertically: higher tilts carry higher
//! Nyquist velocities and little aliasing, so the volume is dealiased from
//! the TOP tilt down. Each tilt's corrected field yields a Browning & Wexler
//! 1968 zeroth-harmonic fit per range band, which serves as the EXTERNAL
//! reference (in the spirit of UNRAVEL's reference checks, Louf et al. 2020)
//! for branch selection and per-region verification on the next tilt below.

use radar_core::{ElevationCut, MomentGrid, MomentType, RadarVolume};

use crate::{RangeBandReference, dealias_velocity_grid_with_reference};

/// Gates per range band for the reference fit.
pub(crate) const REFERENCE_BAND_GATES: usize = 16;
const FIT_MIN_SAMPLES: u32 = 48;
const FIT_MIN_SECTORS: u32 = 5; // of 12 × 30° azimuth sectors
/// Outlier trim for the second fit pass (m/s).
const FIT_TRIM_MPS: f32 = 12.0;

/// Fit the per-range-band zeroth harmonic v(az) = a·cos(az) + b·sin(az) on a
/// (dealiased) velocity grid. Two passes: fit, then refit excluding outliers.
pub fn fit_range_band_reference(cut: &ElevationCut, grid: &MomentGrid) -> RangeBandReference {
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    let bands = gates.div_ceil(REFERENCE_BAND_GATES).max(1);
    let azimuth = |row: usize| -> Option<f32> {
        grid.radial_indices
            .get(row)
            .and_then(|&i| cut.radials.get(i))
            .map(|r| r.azimuth_deg)
    };

    let mut fits: Vec<Option<(f32, f32)>> = vec![None; bands];
    for pass in 0..2 {
        let mut acc = vec![[0.0f64; 6]; bands]; // cc, cs, ss, cv, sv, n
        let mut sectors = vec![0u16; bands];
        for row in 0..rows {
            let Some(az_deg) = azimuth(row) else {
                continue;
            };
            let az = (az_deg as f64).to_radians();
            let (sin_az, cos_az) = (az.sin(), az.cos());
            let sector_bit = 1u16 << ((az_deg.rem_euclid(360.0) / 30.0) as u32 % 12);
            for gate in 0..gates {
                let Some(v) = grid.scaled_value(row, gate).filter(|v| v.is_finite()) else {
                    continue;
                };
                let band = gate / REFERENCE_BAND_GATES;
                if pass == 1
                    && let Some((a, b)) = fits[band]
                {
                    let predicted = a * cos_az as f32 + b * sin_az as f32;
                    if (v - predicted).abs() > FIT_TRIM_MPS {
                        continue;
                    }
                }
                let entry = &mut acc[band];
                entry[0] += cos_az * cos_az;
                entry[1] += cos_az * sin_az;
                entry[2] += sin_az * sin_az;
                entry[3] += cos_az * v as f64;
                entry[4] += sin_az * v as f64;
                entry[5] += 1.0;
                sectors[band] |= sector_bit;
            }
        }
        for band in 0..bands {
            let entry = &acc[band];
            if (entry[5] as u32) < FIT_MIN_SAMPLES || sectors[band].count_ones() < FIT_MIN_SECTORS {
                fits[band] = None;
                continue;
            }
            let det = entry[0] * entry[2] - entry[1] * entry[1];
            if det.abs() < 1e-6 {
                fits[band] = None;
                continue;
            }
            let a = (entry[3] * entry[2] - entry[4] * entry[1]) / det;
            let b = (entry[4] * entry[0] - entry[3] * entry[1]) / det;
            fits[band] = Some((a as f32, b as f32));
        }
    }
    RangeBandReference {
        band_gates: REFERENCE_BAND_GATES,
        fits,
    }
}

/// Dealias one velocity tilt using the tilt-cascade engine: every velocity
/// tilt ABOVE the target is dealiased first (top-down), each feeding its
/// reference fit to the tilt below. Falls back to the plain region engine
/// when the volume has no higher velocity tilt (single-tilt volumes,
/// topmost tilt).
pub fn dealias_velocity_grid_cascade(volume: &RadarVolume, cut_index: usize) -> Option<MomentGrid> {
    let target_cut = volume.cuts.get(cut_index)?;
    target_cut.moments.get(&MomentType::Velocity)?;

    // Velocity tilts sorted by elevation DESCENDING (top first). De-dupe
    // SAILS revisits at the same elevation: keep the first per 0.1° bucket.
    let mut order: Vec<usize> = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Velocity))
        .map(|(i, _)| i)
        .collect();
    order.sort_by(|a, b| {
        volume.cuts[*b]
            .elevation_deg
            .total_cmp(&volume.cuts[*a].elevation_deg)
    });
    let mut cascade: Vec<usize> = Vec::new();
    for index in order {
        let elevation = volume.cuts[index].elevation_deg;
        let duplicate = cascade
            .iter()
            .any(|&c| (volume.cuts[c].elevation_deg - elevation).abs() < 0.1);
        if !duplicate || index == cut_index {
            cascade.push(index);
        }
    }

    let mut reference: Option<RangeBandReference> = None;
    for &index in &cascade {
        let cut = &volume.cuts[index];
        let Some(grid) = cut.moments.get(&MomentType::Velocity) else {
            continue;
        };
        let dealiased = dealias_velocity_grid_with_reference(cut, grid, reference.as_ref());
        if index == cut_index {
            return Some(dealiased);
        }
        reference = Some(fit_range_band_reference(cut, &dealiased));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentStorage, Radial};

    /// Build a velocity tilt whose TRUE field is a uniform wind (radial
    /// component = speed·cos(az − dir)), wrapped into ±nyquist.
    fn tilt_with_wind(
        elevation: f32,
        speed: f32,
        toward_deg: f32,
        nyquist: f32,
        rows: usize,
        gates: usize,
    ) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elevation, None);
        let mut data = vec![f32::NAN; rows * gates];
        for row in 0..rows {
            let az = row as f32 * (360.0 / rows as f32);
            cut.radials.push(Radial {
                azimuth_deg: az,
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(nyquist),
                radial_status: None,
            });
            // Radial velocity of a uniform wind blowing TOWARD `toward_deg`:
            // positive (outbound) looking down-wind.
            let true_v = speed * ((az - toward_deg).to_radians()).cos();
            // Wrap into the Nyquist interval.
            let mut wrapped = true_v;
            while wrapped > nyquist {
                wrapped -= 2.0 * nyquist;
            }
            while wrapped < -nyquist {
                wrapped += 2.0 * nyquist;
            }
            for gate in 0..gates {
                data[row * gates + gate] = wrapped;
            }
        }
        cut.moments.insert(
            MomentType::Velocity,
            MomentGrid {
                moment: MomentType::Velocity,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: (0..rows).collect(),
                storage: MomentStorage::F32(data),
            },
        );
        cut
    }

    #[test]
    fn cascade_unfolds_widespread_aliasing_using_the_upper_tilt() {
        // 35 m/s wind. Top tilt: Nyquist 40 — no aliasing, clean reference.
        // Bottom tilt: Nyquist 20 — large sectors wrap (|v| up to 35), the
        // regime where single-sweep references are circular.
        let top = tilt_with_wind(2.4, 35.0, 180.0, 40.0, 360, 200);
        let bottom = tilt_with_wind(0.5, 35.0, 180.0, 20.0, 360, 200);
        let mut volume = RadarVolume::new(
            radar_core::RadarSite::new("TEST"),
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        );
        volume.cuts = vec![bottom, top];

        let dealiased = dealias_velocity_grid_cascade(&volume, 0).expect("cascade");
        // Compare against truth everywhere.
        let mut worst = 0.0f32;
        for row in 0..360 {
            let az = row as f32;
            let truth = 35.0 * ((az - 180.0).to_radians()).cos();
            for gate in (0..200).step_by(7) {
                if let Some(v) = dealiased.scaled_value(row, gate).filter(|v| v.is_finite()) {
                    worst = worst.max((v - truth).abs());
                }
            }
        }
        assert!(
            worst < 2.0,
            "cascade should recover the true field everywhere; worst error {worst} m/s"
        );
    }

    #[test]
    fn single_tilt_volume_falls_back_to_the_region_engine() {
        let only = tilt_with_wind(0.5, 15.0, 90.0, 25.0, 360, 120);
        let mut volume = RadarVolume::new(
            radar_core::RadarSite::new("TEST"),
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        );
        volume.cuts = vec![only];
        let dealiased = dealias_velocity_grid_cascade(&volume, 0).expect("fallback");
        // 15 m/s wind under Nyquist 25: nothing wraps; output ≈ input.
        let v = dealiased.scaled_value(90, 50).expect("value");
        assert!((v - 15.0 * 0.0_f32.to_radians().cos()).abs() < 1.0);
    }
}
