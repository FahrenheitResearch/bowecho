//! External wind references for absolute fold-branch selection.
//!
//! Boundary votes inside one sweep only constrain folds *relative* to each
//! other; the absolute branch of each connected group needs independent
//! evidence. The reference here is the zeroth azimuthal harmonic of the VAD
//! family (Browning & Wexler 1968, *J. Appl. Meteor.* 7, 105–113,
//! doi:10.1175/1520-0450(1968)007<0105:TDOKPO>2.0.CO;2): per band of range
//! gates, the horizontally uniform wind whose radial projection is
//! v̂(az) = a·cos(az) + b·sin(az). Using a reference for verification follows
//! UNRAVEL (Louf et al. 2020, *JTECH* 37(5), 741–758,
//! doi:10.1175/JTECH-D-19-0020.1).
//!
//! Fit it from an already-trustworthy field with
//! [`fit_range_band_reference`] (e.g. the dealiased tilt above, as
//! [`dealias_cascade`](crate::dealias_cascade) does), or build one directly
//! from external data such as NWP model winds: for a wind blowing *toward*
//! azimuth φ at speed s, the coefficients are `a = s·cos(φ)`, `b = s·sin(φ)`.

/// Gates per range band used by [`fit_range_band_reference`].
pub const REFERENCE_BAND_GATES: usize = 16;
const FIT_MIN_SAMPLES: u32 = 48;
const FIT_MIN_SECTORS: u32 = 5; // of 12 × 30° azimuth sectors
/// Outlier trim for the second fit pass (m/s).
const FIT_TRIM_MPS: f32 = 12.0;

/// Per-range-band zeroth-harmonic wind reference (Browning & Wexler 1968):
/// v̂(az) = a·cos(az) + b·sin(az), one optional `(a, b)` fit per band of
/// `band_gates` gates. Fields are public so a reference can be seeded from
/// external data (model winds, soundings) instead of a fit.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeBandReference {
    /// Number of range gates per band; gate `g` uses `fits[g / band_gates]`.
    pub band_gates: usize,
    /// Per-band `(a, b)` coefficients; `None` where no trustworthy fit exists.
    pub fits: Vec<Option<(f32, f32)>>,
}

impl RangeBandReference {
    /// Predicted radial velocity (m/s) at the given azimuth and gate, or
    /// `None` where the reference has no coverage.
    pub fn evaluate(&self, azimuth_deg: f32, gate: usize) -> Option<f32> {
        let az = azimuth_deg.to_radians();
        self.eval_trig(az.sin(), az.cos(), gate)
    }

    #[inline]
    pub(crate) fn eval_trig(&self, sin_az: f32, cos_az: f32, gate: usize) -> Option<f32> {
        let (a, b) = (*self.fits.get(gate / self.band_gates.max(1))?)?;
        Some(a * cos_az + b * sin_az)
    }
}

/// Fit the per-range-band zeroth harmonic v(az) = a·cos(az) + b·sin(az) on an
/// already-dealiased velocity field (row-major `azimuths_deg.len() × gates`,
/// `NaN` = no data). Two passes: fit, then refit excluding outliers. Bands
/// with too few samples or too little azimuthal coverage stay `None`.
pub fn fit_range_band_reference(
    velocity: &[f32],
    gates: usize,
    azimuths_deg: &[f32],
) -> RangeBandReference {
    let rows = azimuths_deg.len();
    let bands = gates.div_ceil(REFERENCE_BAND_GATES).max(1);

    let mut fits: Vec<Option<(f32, f32)>> = vec![None; bands];
    for pass in 0..2 {
        let mut acc = vec![[0.0f64; 6]; bands]; // cc, cs, ss, cv, sv, n
        let mut sectors = vec![0u16; bands];
        for (row, &az_deg) in azimuths_deg.iter().enumerate().take(rows) {
            if !az_deg.is_finite() {
                continue;
            }
            let az = (az_deg as f64).to_radians();
            let (sin_az, cos_az) = (az.sin(), az.cos());
            let sector_bit = 1u16 << ((az_deg.rem_euclid(360.0) / 30.0) as u32 % 12);
            for gate in 0..gates {
                let Some(v) = velocity.get(row * gates + gate).filter(|v| v.is_finite()) else {
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
                entry[3] += cos_az * *v as f64;
                entry[4] += sin_az * *v as f64;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_uniform_wind() {
        // 25 m/s wind toward 90° (due east): v(az) = 25·sin(az), so a ≈ 0,
        // b ≈ 25 in v̂(az) = a·cos(az) + b·sin(az).
        let rows = 360usize;
        let gates = 32usize;
        let azimuths: Vec<f32> = (0..rows).map(|r| r as f32).collect();
        let velocity: Vec<f32> = (0..rows)
            .flat_map(|row| {
                let v = 25.0 * (azimuths[row].to_radians()).sin();
                std::iter::repeat_n(v, gates)
            })
            .collect();
        let reference = fit_range_band_reference(&velocity, gates, &azimuths);
        let (a, b) = reference.fits[0].expect("band 0 fit");
        assert!(a.abs() < 0.1, "a = {a}");
        assert!((b - 25.0).abs() < 0.1, "b = {b}");
        let predicted = reference.evaluate(90.0, 0).expect("coverage");
        assert!((predicted - 25.0).abs() < 0.1);
    }

    #[test]
    fn sparse_bands_have_no_fit() {
        let azimuths: Vec<f32> = (0..10).map(|r| r as f32).collect();
        let velocity = vec![5.0f32; 10 * 4];
        let reference = fit_range_band_reference(&velocity, 4, &azimuths);
        // Only 10° of azimuth coverage: fails the sector gate.
        assert!(reference.fits.iter().all(Option::is_none));
    }
}
