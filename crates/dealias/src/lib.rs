//! Region-based Doppler velocity dealiasing (unfolding) for weather radar.
//!
//! This is the dealiasing engine shipped in [BowEcho](https://github.com/FahrenheitResearch/bowecho),
//! extracted as a standalone, zero-dependency crate. It operates on plain
//! `f32` slices — no radar-format types — so it can sit behind any Level II
//! decoder (NEXRAD, ODIM, IRIS, …) or synthetic data source.
//!
//! # Why region-based?
//!
//! Doppler radars measure radial velocity modulo the Nyquist co-interval: a
//! true velocity outside ±v_N is *aliased* (folded) back into the interval by
//! a multiple of 2·v_N. The classic gate-by-gate radial-walk correction
//! propagates a single bad fold decision down an entire ray, producing the
//! radial "spokes" that plague high-shear convection (derechos, mesocyclones).
//! The region-based approach instead decides whole coherent regions at once
//! and lets genuine discontinuities remain at region boundaries, so an error
//! cannot run down a radial. See Jing & Wiener (1993, *JTECH* 10, 798–808,
//! doi:10.1175/1520-0426(1993)010<0798:TDDODV>2.0.CO;2), Py-ART's
//! `dealias_region_based` (Helmus & Collis 2016, *J. Open Res. Softw.* 4(1),
//! e25, doi:10.5334/jors.119), and Feldmann et al. (2020, *R2D2*, *JTECH* 37,
//! doi:10.1175/JTECH-D-20-0054.1).
//!
//! # Algorithm
//!
//! [`dealias`] runs these steps on one sweep (PPI):
//!
//! 1. **Label regions** — flood-fill connected regions whose neighbouring
//!    gates differ by less than half a Nyquist, so no fold occurs *within* a
//!    region (union-find over the 4-connected polar grid, including the
//!    azimuthal wrap of a full 360° sweep).
//! 2. **Vote on boundaries** — build a region-adjacency graph whose edges
//!    carry the integer Nyquist fold between the two regions: the consensus
//!    of `round((v_a − v_b) / 2·v_N)` over all shared boundary gate-pairs.
//! 3. **Resolve folds** — merge regions strongest-boundary-first through a
//!    union-find that tracks a per-node integer fold offset (a weighted /
//!    "potential" DSU), so all relative fold relations stay consistent.
//! 4. **Anchor** — shift each connected group so its largest region has fold
//!    zero (the dominant region is assumed unaliased).
//! 5. **Apply & despeckle** — add `2·v_N·fold` per gate, then snap isolated
//!    single-gate outliers onto the Nyquist multiple nearest their local
//!    median (dual-PRF/processor speckle; Holleman & Beekhuis 2003, *JTECH*
//!    20, 443–453; Altube et al. 2017, *JTECH* 34, 1529–1543,
//!    doi:10.1175/JTECH-D-16-0065.1).
//!
//! Boundary votes lock folds only *relative* to each other; each connected
//! group's absolute branch is under-determined from one sweep alone. When
//! independent evidence is available, pass it as a [`RangeBandReference`]
//! (a Browning & Wexler 1968 zeroth-harmonic wind fit per range band) to
//! [`dealias_with_reference`], which uses it for group branch selection and
//! per-region verification in the spirit of UNRAVEL's reference checks
//! (Louf et al. 2020, *JTECH* 37(5), 741–758, doi:10.1175/JTECH-D-19-0020.1).
//! [`dealias_cascade`] derives that reference from the radar volume itself:
//! higher tilts carry higher Nyquist velocities and little aliasing, so the
//! volume is dealiased from the top tilt down, each tilt's wind fit feeding
//! the tilt below. A reference can also come from entirely external data
//! (e.g. NWP model winds at the radar site) by constructing a
//! [`RangeBandReference`] directly — its fields are public for exactly that
//! purpose.
//!
//! # Example
//!
//! ```
//! use bowecho_dealias::{Sweep, dealias};
//!
//! // 1 radial × 5 gates, Nyquist 10 m/s: the last two gates are folded.
//! let velocity = [0.0_f32, 5.0, 9.0, -9.0, -7.0];
//! let sweep = Sweep {
//!     velocity: &velocity,
//!     gates: 5,
//!     nyquist: &[10.0],
//!     azimuths_deg: &[0.0],
//! };
//! let result = dealias(&sweep);
//! assert_eq!(result.velocity, vec![0.0, 5.0, 9.0, 11.0, 13.0]);
//! assert_eq!(result.folds, vec![0, 0, 0, 1, 1]);
//! ```
//!
//! # Data model
//!
//! All grids are row-major `rows × gates` with one row per radial. `NaN`
//! marks gates with no data (below threshold, range-folded, censored); they
//! never join a region and stay `NaN` in the output. Velocities are in m/s,
//! positive away from the radar.

mod cascade;
mod despeckle;
mod reference;
mod region;

pub use cascade::{Tilt, dealias_cascade};
pub use despeckle::despeckle;
pub use reference::{REFERENCE_BAND_GATES, RangeBandReference, fit_range_band_reference};

/// One radar sweep (PPI) in polar coordinates, row-major `rows × gates`.
///
/// `rows` is implied by `nyquist.len()` (== `azimuths_deg.len()`); the
/// functions taking a `Sweep` panic if `velocity.len() != rows * gates` or
/// the per-radial slices disagree, since that is always a caller bug.
#[derive(Debug, Clone, Copy)]
pub struct Sweep<'a> {
    /// Observed radial velocities (m/s), `NaN` = no data / range folded.
    pub velocity: &'a [f32],
    /// Number of range gates per radial.
    pub gates: usize,
    /// Per-radial Nyquist velocity (m/s). Non-finite or non-positive entries
    /// fall back to the median of the valid entries; if none are valid the
    /// sweep passes through unchanged.
    pub nyquist: &'a [f32],
    /// Per-radial beam azimuth (degrees). Used to detect whether the sweep
    /// closes a full 360° circle (so the last radial neighbours the first)
    /// and to evaluate a [`RangeBandReference`].
    pub azimuths_deg: &'a [f32],
}

impl Sweep<'_> {
    /// Number of radials (rows) in the sweep.
    pub fn rows(&self) -> usize {
        self.nyquist.len()
    }

    fn assert_valid(&self) {
        assert_eq!(
            self.azimuths_deg.len(),
            self.nyquist.len(),
            "azimuths_deg and nyquist must both have one entry per radial",
        );
        let total = self
            .rows()
            .checked_mul(self.gates)
            .expect("rows * gates overflows usize");
        assert_eq!(
            self.velocity.len(),
            total,
            "velocity must be row-major rows * gates",
        );
    }
}

/// The output of [`dealias`] / [`dealias_with_reference`].
///
/// Invariant: wherever the input was finite and the radial's Nyquist is
/// known, `velocity[i] == observed[i] + 2.0 * nyquist[row] * folds[i] as f32`
/// (despeckling included — it only ever adds whole fold multiples). Gates
/// with unknown Nyquist pass through unchanged with fold 0; `NaN` gates stay
/// `NaN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Dealiased {
    /// Corrected (unfolded) radial velocities (m/s), same layout as the input.
    pub velocity: Vec<f32>,
    /// Integer number of Nyquist co-intervals (2·v_N) added to each gate.
    pub folds: Vec<i32>,
}

/// Dealias one sweep with the region-based engine (steps 1–5 above).
///
/// Deterministic: identical input always yields the identical output.
pub fn dealias(sweep: &Sweep) -> Dealiased {
    dealias_with_reference(sweep, None)
}

/// [`dealias`] with an optional external wind reference: the resolver uses it
/// for connected-group branch selection and per-region verification
/// (UNRAVEL-style checks, Louf et al. 2020). With `None` the behaviour is
/// identical to the plain region engine.
pub fn dealias_with_reference(sweep: &Sweep, reference: Option<&RangeBandReference>) -> Dealiased {
    sweep.assert_valid();
    let rows = sweep.rows();
    let gates = sweep.gates;
    let nyq = resolve_nyquist(sweep.nyquist);

    let mut folds = region::solve_folds(
        sweep.velocity,
        &nyq,
        rows,
        gates,
        sweep.azimuths_deg,
        reference,
    );

    let mut velocity = vec![f32::NAN; sweep.velocity.len()];
    for (row, &n) in nyq.iter().enumerate().take(rows) {
        for gate in 0..gates {
            let idx = row * gates + gate;
            let v = sweep.velocity[idx];
            if !v.is_finite() {
                continue;
            }
            velocity[idx] = if n.is_finite() {
                v + 2.0 * n * folds[idx] as f32
            } else {
                v
            };
        }
    }

    despeckle::despeckle_tracking_folds(&mut velocity, gates, &nyq, Some(&mut folds));

    Dealiased { velocity, folds }
}

/// Compute the per-gate integer fold counts only (steps 1–4, no apply, no
/// despeckle). Useful when you keep your own storage and want to apply
/// `2·v_N·fold` yourself, or want fold counts as a QC field.
pub fn compute_folds(sweep: &Sweep, reference: Option<&RangeBandReference>) -> Vec<i32> {
    sweep.assert_valid();
    let nyq = resolve_nyquist(sweep.nyquist);
    region::solve_folds(
        sweep.velocity,
        &nyq,
        sweep.rows(),
        sweep.gates,
        sweep.azimuths_deg,
        reference,
    )
}

/// Replace non-finite / non-positive per-radial Nyquist entries with the
/// median of the valid ones (the whole-sweep consensus), `NaN` if none exist.
pub(crate) fn resolve_nyquist(nyquist: &[f32]) -> Vec<f32> {
    let mut valid: Vec<f32> = nyquist
        .iter()
        .copied()
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect();
    let fallback = if valid.is_empty() {
        f32::NAN
    } else {
        valid.sort_by(f32::total_cmp);
        valid[valid.len() / 2]
    };
    nyquist
        .iter()
        .map(|value| {
            if value.is_finite() && *value > 0.0 {
                *value
            } else {
                fallback
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap a true velocity into the ±nyquist interval.
    fn alias(v: f32, nyq: f32) -> f32 {
        let mut a = v;
        while a > nyq {
            a -= 2.0 * nyq;
        }
        while a < -nyq {
            a += 2.0 * nyq;
        }
        a
    }

    fn uniform_sweep(rows_data: &[Vec<f32>], nyq: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let rows = rows_data.len();
        let velocity: Vec<f32> = rows_data.iter().flatten().copied().collect();
        let nyquist = vec![nyq; rows];
        let azimuths: Vec<f32> = (0..rows).map(|row| row as f32).collect();
        (velocity, nyquist, azimuths)
    }

    #[test]
    fn unfolds_radial_continuity_on_a_single_ray() {
        let velocity = [0.0_f32, 5.0, 9.0, -9.0, -7.0];
        let sweep = Sweep {
            velocity: &velocity,
            gates: 5,
            nyquist: &[10.0],
            azimuths_deg: &[0.0],
        };
        let result = dealias(&sweep);
        assert_eq!(result.velocity, vec![0.0, 5.0, 9.0, 11.0, 13.0]);
        assert_eq!(result.folds, vec![0, 0, 0, 1, 1]);
    }

    #[test]
    fn recovers_smooth_folded_ramp() {
        // A smooth radial velocity ramp from -34 to +34 m/s with Nyquist 20 is
        // aliased into [-20, 20]; the region-based unfolder must recover the
        // smooth field (up to a global 2·Nyquist constant from anchoring).
        let nyq = 20.0f32;
        let gates = 24usize;
        let rows = 12usize;
        let truth: Vec<f32> = (0..gates)
            .map(|g| -34.0 + 68.0 * g as f32 / (gates as f32 - 1.0))
            .collect();
        let observed_row: Vec<f32> = truth.iter().map(|v| alias(*v, nyq)).collect();
        // The raw row genuinely folds (large gate-to-gate jumps present).
        let raw_jumps = observed_row
            .windows(2)
            .filter(|w| (w[0] - w[1]).abs() > nyq)
            .count();
        assert!(raw_jumps >= 1, "test fixture must actually alias");

        let rows_data: Vec<Vec<f32>> = (0..rows).map(|_| observed_row.clone()).collect();
        let (velocity, nyquist, azimuths) = uniform_sweep(&rows_data, nyq);
        let result = dealias(&Sweep {
            velocity: &velocity,
            gates,
            nyquist: &nyquist,
            azimuths_deg: &azimuths,
        });
        let recovered = &result.velocity[(rows / 2) * gates..(rows / 2 + 1) * gates];

        // 1) the unfolded field is smooth: no gate-to-gate jump exceeds Nyquist.
        for w in recovered.windows(2) {
            assert!(
                (w[0] - w[1]).abs() <= nyq,
                "residual fold in dealiased ramp: {w:?}"
            );
        }
        // 2) it matches the truth up to a single constant multiple of 2·Nyquist.
        let offset = recovered[0] - truth[0];
        let folds = (offset / (2.0 * nyq)).round();
        assert!(
            (offset - folds * 2.0 * nyq).abs() < 1.0,
            "offset not a fold multiple: {offset}"
        );
        for (r, t) in recovered.iter().zip(truth.iter()) {
            assert!(
                (r - (t + folds * 2.0 * nyq)).abs() < 1.0,
                "recovered {r} != truth {t} (+{folds} folds)"
            );
        }
    }

    #[test]
    fn does_not_propagate_errors_down_a_radial() {
        // The classic spoke failure: coherent sub-Nyquist data must come back
        // essentially unchanged (no fold anywhere).
        let nyq = 20.0f32;
        let coherent = vec![2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0];
        let rows_data: Vec<Vec<f32>> = (0..16).map(|_| coherent.clone()).collect();
        let (velocity, nyquist, azimuths) = uniform_sweep(&rows_data, nyq);
        let result = dealias(&Sweep {
            velocity: &velocity,
            gates: coherent.len(),
            nyquist: &nyquist,
            azimuths_deg: &azimuths,
        });
        for g in 0..coherent.len() {
            assert_eq!(
                result.velocity[5 * coherent.len() + g],
                coherent[g],
                "coherent gate {g} should be untouched"
            );
        }
    }

    #[test]
    fn is_deterministic_across_runs() {
        // Same input must always produce the same unfolded field: edge
        // resolution order and tied fold votes must not depend on HashMap
        // iteration order (which differs per HashMap instance).
        let nyq = 20.0f32;
        let rows = 24usize;
        let gates = 40usize;
        let mut seed = 0x2468_ace1_u32;
        let mut lcg = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as f32 / 65_536.0
        };
        let rows_data: Vec<Vec<f32>> = (0..rows)
            .map(|row| {
                (0..gates)
                    .map(|gate| {
                        let patch = (row / 3) * 7 + gate / 5;
                        let base = match patch % 4 {
                            0 => -38.0,
                            1 => -2.0,
                            2 => 18.5,
                            _ => 39.0,
                        };
                        alias(base + (lcg() - 0.5) * 6.0, nyq)
                    })
                    .collect()
            })
            .collect();
        let (velocity, nyquist, azimuths) = uniform_sweep(&rows_data, nyq);
        let sweep = Sweep {
            velocity: &velocity,
            gates,
            nyquist: &nyquist,
            azimuths_deg: &azimuths,
        };

        let reference = dealias(&sweep);
        for run in 0..16 {
            assert_eq!(
                dealias(&sweep),
                reference,
                "dealias output changed between identical runs (run {run})"
            );
        }
    }

    #[test]
    fn unfolds_geometrically_supported_fold() {
        // A folded 2-gate segment surrounded on three sides by data that
        // consistently implies one fold is unfolded (a radial-walk scheme
        // wrongly "suppresses" this as a spike and leaves the alias in place).
        let quiet = vec![0.0, 3.0, 5.0, 7.0, 8.0];
        let folded = vec![0.0, 5.0, 9.0, -9.0, -7.0];
        let rows_data = vec![quiet.clone(), quiet.clone(), folded, quiet.clone(), quiet];
        let (velocity, nyquist, azimuths) = uniform_sweep(&rows_data, 10.0);
        let result = dealias(&Sweep {
            velocity: &velocity,
            gates: 5,
            nyquist: &nyquist,
            azimuths_deg: &azimuths,
        });
        assert_eq!(result.velocity[2 * 5 + 3], 11.0);
        assert_eq!(result.velocity[2 * 5 + 4], 13.0);
    }

    #[test]
    fn preserves_supported_adjacent_folds() {
        let quiet = vec![0.0, 3.0, 5.0, 7.0, 8.0];
        let folded = vec![0.0, 5.0, 9.0, -9.0, -7.0];
        let rows_data = vec![quiet.clone(), folded.clone(), folded.clone(), folded, quiet];
        let (velocity, nyquist, azimuths) = uniform_sweep(&rows_data, 10.0);
        let result = dealias(&Sweep {
            velocity: &velocity,
            gates: 5,
            nyquist: &nyquist,
            azimuths_deg: &azimuths,
        });
        assert_eq!(result.velocity[2 * 5 + 3], 11.0);
        assert_eq!(result.velocity[2 * 5 + 4], 13.0);
    }

    #[test]
    fn output_satisfies_the_fold_invariant() {
        let nyq = 15.0f32;
        let rows = 20usize;
        let gates = 30usize;
        let mut seed = 0x1357_9bdf_u32;
        let mut lcg = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as f32 / 65_536.0
        };
        let velocity: Vec<f32> = (0..rows * gates)
            .map(|idx| {
                if idx % 17 == 0 {
                    f32::NAN
                } else {
                    alias((lcg() - 0.5) * 70.0, nyq)
                }
            })
            .collect();
        let nyquist = vec![nyq; rows];
        let azimuths: Vec<f32> = (0..rows).map(|row| row as f32).collect();
        let result = dealias(&Sweep {
            velocity: &velocity,
            gates,
            nyquist: &nyquist,
            azimuths_deg: &azimuths,
        });
        for (idx, &observed) in velocity.iter().enumerate() {
            if observed.is_finite() {
                let expected = observed + 2.0 * nyq * result.folds[idx] as f32;
                assert!(
                    (result.velocity[idx] - expected).abs() < 1e-3,
                    "invariant broken at {idx}: {} vs {expected}",
                    result.velocity[idx]
                );
            } else {
                assert!(result.velocity[idx].is_nan());
                assert_eq!(result.folds[idx], 0);
            }
        }
    }

    #[test]
    fn unknown_nyquist_rows_fall_back_to_the_sweep_median() {
        let velocity = [0.0_f32, 5.0, 9.0, -9.0, -7.0, 0.0, 5.0, 9.0, -9.0, -7.0];
        let sweep = Sweep {
            velocity: &velocity,
            gates: 5,
            nyquist: &[f32::NAN, 10.0],
            azimuths_deg: &[0.0, 1.0],
        };
        let result = dealias(&sweep);
        assert_eq!(&result.velocity[..5], &[0.0, 5.0, 9.0, 11.0, 13.0]);
    }

    #[test]
    fn all_unknown_nyquist_passes_through() {
        let velocity = [0.0_f32, 5.0, 9.0, -9.0, -7.0];
        let sweep = Sweep {
            velocity: &velocity,
            gates: 5,
            nyquist: &[f32::NAN],
            azimuths_deg: &[0.0],
        };
        let result = dealias(&sweep);
        assert_eq!(result.velocity, velocity);
    }
}
