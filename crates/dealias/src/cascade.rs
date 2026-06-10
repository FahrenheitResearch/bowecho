//! Tilt-cascade dealiasing — volume-aware branch selection.
//!
//! The region engine resolves folds from boundary evidence alone, which
//! leaves each connected group's absolute branch under-determined; on sweeps
//! with widespread aliasing a same-sweep VAD reference is circular (wrapped
//! gates poison the fit). The cascade breaks the circularity vertically:
//! higher tilts carry higher Nyquist velocities and little aliasing, so the
//! volume is dealiased from the TOP tilt down. Each tilt's corrected field
//! yields a Browning & Wexler (1968) zeroth-harmonic fit per range band,
//! which serves as the external reference (in the spirit of UNRAVEL's
//! reference checks, Louf et al. 2020, doi:10.1175/JTECH-D-19-0020.1) for
//! branch selection and per-region verification on the next tilt below.

use crate::reference::fit_range_band_reference;
use crate::{Dealiased, RangeBandReference, Sweep, dealias_with_reference};

/// One velocity tilt of a radar volume, for [`dealias_cascade`].
#[derive(Debug, Clone, Copy)]
pub struct Tilt<'a> {
    pub sweep: Sweep<'a>,
    pub elevation_deg: f32,
}

/// Dealias the `target` tilt (an index into `tilts`) with the tilt-cascade
/// engine: every tilt above the target is dealiased first (top-down), each
/// feeding its wind-reference fit to the tilt below. Behaves exactly like
/// [`dealias`](crate::dealias) when no tilt sits above the target
/// (single-tilt volumes, topmost tilt).
///
/// Pass every velocity tilt of the volume in any order; tilts are sorted by
/// elevation internally, and revisits of an already-seen elevation (NEXRAD
/// SAILS/MRLE inserts) are skipped within 0.1° — except the target itself.
/// Returns `None` only if `target` is out of bounds.
pub fn dealias_cascade(tilts: &[Tilt], target: usize) -> Option<Dealiased> {
    tilts.get(target)?;

    // Tilt indices sorted by elevation DESCENDING (top first), de-duped.
    let mut order: Vec<usize> = (0..tilts.len()).collect();
    order.sort_by(|a, b| tilts[*b].elevation_deg.total_cmp(&tilts[*a].elevation_deg));
    let mut cascade: Vec<usize> = Vec::new();
    for index in order {
        let elevation = tilts[index].elevation_deg;
        let duplicate = cascade
            .iter()
            .any(|&c| (tilts[c].elevation_deg - elevation).abs() < 0.1);
        if !duplicate || index == target {
            cascade.push(index);
        }
    }

    let mut reference: Option<RangeBandReference> = None;
    for &index in &cascade {
        let tilt = &tilts[index];
        let dealiased = dealias_with_reference(&tilt.sweep, reference.as_ref());
        if index == target {
            return Some(dealiased);
        }
        reference = Some(fit_range_band_reference(
            &dealiased.velocity,
            tilt.sweep.gates,
            tilt.sweep.azimuths_deg,
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Velocity field of a uniform wind (radial component =
    /// speed·cos(az − dir)), wrapped into ±nyquist.
    fn wind_field(
        speed: f32,
        toward_deg: f32,
        nyquist: f32,
        rows: usize,
        gates: usize,
    ) -> Vec<f32> {
        let mut data = vec![f32::NAN; rows * gates];
        for row in 0..rows {
            let az = row as f32 * (360.0 / rows as f32);
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
        data
    }

    fn azimuths(rows: usize) -> Vec<f32> {
        (0..rows)
            .map(|row| row as f32 * (360.0 / rows as f32))
            .collect()
    }

    #[test]
    fn cascade_unfolds_widespread_aliasing_using_the_upper_tilt() {
        // 35 m/s wind. Top tilt: Nyquist 40 — no aliasing, clean reference.
        // Bottom tilt: Nyquist 20 — large sectors wrap (|v| up to 35), the
        // regime where single-sweep references are circular.
        let (rows, gates) = (360usize, 200usize);
        let az = azimuths(rows);
        let top_velocity = wind_field(35.0, 180.0, 40.0, rows, gates);
        let top_nyquist = vec![40.0f32; rows];
        let bottom_velocity = wind_field(35.0, 180.0, 20.0, rows, gates);
        let bottom_nyquist = vec![20.0f32; rows];
        let tilts = [
            Tilt {
                sweep: Sweep {
                    velocity: &bottom_velocity,
                    gates,
                    nyquist: &bottom_nyquist,
                    azimuths_deg: &az,
                },
                elevation_deg: 0.5,
            },
            Tilt {
                sweep: Sweep {
                    velocity: &top_velocity,
                    gates,
                    nyquist: &top_nyquist,
                    azimuths_deg: &az,
                },
                elevation_deg: 2.4,
            },
        ];

        let dealiased = dealias_cascade(&tilts, 0).expect("cascade");
        // Compare against truth everywhere.
        let mut worst = 0.0f32;
        for (row, &az_deg) in az.iter().enumerate() {
            let truth = 35.0 * ((az_deg - 180.0).to_radians()).cos();
            for gate in (0..gates).step_by(7) {
                let v = dealiased.velocity[row * gates + gate];
                if v.is_finite() {
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
        let (rows, gates) = (360usize, 120usize);
        let az = azimuths(rows);
        let velocity = wind_field(15.0, 90.0, 25.0, rows, gates);
        let nyquist = vec![25.0f32; rows];
        let tilts = [Tilt {
            sweep: Sweep {
                velocity: &velocity,
                gates,
                nyquist: &nyquist,
                azimuths_deg: &az,
            },
            elevation_deg: 0.5,
        }];
        let dealiased = dealias_cascade(&tilts, 0).expect("fallback");
        // 15 m/s wind under Nyquist 25: nothing wraps; output ≈ input.
        let v = dealiased.velocity[90 * gates + 50];
        assert!((v - 15.0).abs() < 1.0);
    }

    #[test]
    fn out_of_bounds_target_is_none() {
        assert!(dealias_cascade(&[], 0).is_none());
    }
}
