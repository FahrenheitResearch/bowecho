//! Tilt-cascade dealiasing — the second dealias engine.
//!
//! Grid adapter over [`bowecho_dealias::dealias_cascade`]: the region-based
//! unfolder resolves folds from boundary evidence alone, which leaves each
//! connected group's absolute BRANCH under-determined, and on sweeps with
//! widespread aliasing a same-sweep VAD reference is circular (wrapped gates
//! poison the fit — see docs/dealias-fold-branch-analysis.md). The cascade
//! breaks the circularity vertically: higher tilts carry higher Nyquist
//! velocities and little aliasing, so the volume is dealiased from the TOP
//! tilt down, each tilt's Browning & Wexler 1968 zeroth-harmonic fit serving
//! as the EXTERNAL reference (in the spirit of UNRAVEL's reference checks,
//! Louf et al. 2020) for branch selection and per-region verification on the
//! next tilt below.

use radar_core::{ElevationCut, MomentGrid, MomentType, RadarVolume};

use crate::{RangeBandReference, encode_dealiased_grid, velocity_sweep_buffers};

/// Fit the per-range-band zeroth harmonic v(az) = a·cos(az) + b·sin(az) on a
/// (dealiased) velocity grid. Two passes: fit, then refit excluding outliers.
pub fn fit_range_band_reference(cut: &ElevationCut, grid: &MomentGrid) -> RangeBandReference {
    let (velocity, _nyquist, azimuths) = velocity_sweep_buffers(cut, grid);
    bowecho_dealias::fit_range_band_reference(&velocity, grid.gate_range.gate_count, &azimuths)
}

/// Dealias one velocity tilt using the tilt-cascade engine: every velocity
/// tilt ABOVE the target is dealiased first (top-down), each feeding its
/// reference fit to the tilt below. Falls back to the plain region engine
/// when the volume has no higher velocity tilt (single-tilt volumes,
/// topmost tilt). SAILS revisits at the same elevation are de-duplicated by
/// the engine (first per 0.1° bucket).
pub fn dealias_velocity_grid_cascade(volume: &RadarVolume, cut_index: usize) -> Option<MomentGrid> {
    let target_grid = volume
        .cuts
        .get(cut_index)?
        .moments
        .get(&MomentType::Velocity)?;

    // (velocities, per-row Nyquist, azimuths, gate count) per velocity tilt.
    type SweepBuffers = (Vec<f32>, Vec<f32>, Vec<f32>, usize);
    let mut cut_indices: Vec<usize> = Vec::new();
    let mut buffers: Vec<SweepBuffers> = Vec::new();
    for (index, cut) in volume.cuts.iter().enumerate() {
        let Some(grid) = cut.moments.get(&MomentType::Velocity) else {
            continue;
        };
        let (velocity, nyquist, azimuths) = velocity_sweep_buffers(cut, grid);
        cut_indices.push(index);
        buffers.push((velocity, nyquist, azimuths, grid.gate_range.gate_count));
    }
    let target = cut_indices.iter().position(|&index| index == cut_index)?;
    let tilts: Vec<bowecho_dealias::Tilt> = cut_indices
        .iter()
        .zip(&buffers)
        .map(
            |(&index, (velocity, nyquist, azimuths, gates))| bowecho_dealias::Tilt {
                sweep: bowecho_dealias::Sweep {
                    velocity,
                    gates: *gates,
                    nyquist,
                    azimuths_deg: azimuths,
                },
                elevation_deg: volume.cuts[index].elevation_deg,
            },
        )
        .collect();

    let dealiased = bowecho_dealias::dealias_cascade(&tilts, target)?;
    Some(encode_dealiased_grid(target_grid, &dealiased.velocity))
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
