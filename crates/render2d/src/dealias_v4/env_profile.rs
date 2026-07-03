//! Environmental wind profile — the external absolute branch reference.
//!
//! The one thing no self-derived reference (same sweep, upper tilt, previous
//! volume) can supply in deep widespread aliasing is the ABSOLUTE Nyquist
//! branch (`docs/dealias-fold-branch-analysis.md`).  A model wind profile at
//! the radar site is external and therefore cannot be poisoned by the sweep's
//! own wrapped gates.  Precedents: the legacy WSR-88D VDA's environmental
//! wind constraints (Eilts & Smith 1990, *JTECH* 7, 118–128) and 4DD's
//! sounding initialization (James & Houze 2001, *JTECH* 18, 1674–1683).
//!
//! INVARIANTS
//! - render2d never fetches anything: the profile is a caller-supplied value
//!   object (the app wires HRRR/RAP later; tests and the bench use fixtures).
//! - The profile is a **branch separator, not a truth field**: branch spacing
//!   2N ≥ ~23 m/s dwarfs HRRR/RAP low-level RMS vector error (~3–5 m/s), and
//!   the energy term built from it is capped and low-weight so storm-scale
//!   deviations (RIJ, mesocyclone, eyewall) cannot buy a wrong branch.
//! - Vertical air motion is deliberately ignored in the projection.
//! - A stale profile is worse than none in evolving synoptic flow: profiles
//!   further than [`ENV_PROFILE_MAX_AGE`] from the volume time are ignored
//!   (the caller sees the same graceful degradation as passing `None`).

use chrono::{DateTime, Utc};
use radar_core::beam_height_above_radar_m;

/// Ignore profiles more than this far (either direction) from the volume
/// time.  3 h spans two RAP cycles — beyond that the environment itself has
/// usually evolved past usefulness as a branch separator.
pub const ENV_PROFILE_MAX_AGE: chrono::Duration = chrono::Duration::hours(3);

/// One level of the environmental wind profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvWindLevel {
    /// Height above radar level in meters (NOT MSL — the caller subtracts
    /// station elevation once, so the engine never needs site metadata).
    pub height_m_arl: f32,
    /// Eastward wind component (m/s).
    pub u_mps: f32,
    /// Northward wind component (m/s).
    pub v_mps: f32,
}

/// Environmental wind profile at the radar site, supplied by the caller.
///
/// `levels` must be strictly increasing in height; profiles that are empty,
/// unsorted, or non-finite are rejected by [`EnvironmentalWindProfile::usable_for`]
/// and the engine degrades to v3-class evidence (temporal + vertical +
/// harmonic) exactly as if `None` had been passed.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentalWindProfile {
    pub levels: Vec<EnvWindLevel>,
    /// Model valid time of the analysis the profile was extracted from.
    pub valid_time: DateTime<Utc>,
}

impl EnvironmentalWindProfile {
    /// True when the profile is well-formed and fresh enough for a volume
    /// scanned at `volume_time` (§4a staleness guard).
    pub fn usable_for(&self, volume_time: DateTime<Utc>) -> bool {
        if self.levels.is_empty() {
            return false;
        }
        let mut previous = f32::NEG_INFINITY;
        for level in &self.levels {
            if !level.height_m_arl.is_finite()
                || !level.u_mps.is_finite()
                || !level.v_mps.is_finite()
                || level.height_m_arl <= previous
            {
                return false;
            }
            previous = level.height_m_arl;
        }
        let age = volume_time.signed_duration_since(self.valid_time);
        age.abs() <= ENV_PROFILE_MAX_AGE
    }

    /// Interpolate (u, v) at `height_m_arl`.  Linear between levels, constant
    /// beyond the profile ends (a radar beam below the lowest model level is
    /// best served by that lowest level, not by extrapolation).
    pub fn wind_at(&self, height_m_arl: f32) -> Option<(f32, f32)> {
        let first = self.levels.first()?;
        let last = self.levels.last()?;
        if height_m_arl <= first.height_m_arl {
            return Some((first.u_mps, first.v_mps));
        }
        if height_m_arl >= last.height_m_arl {
            return Some((last.u_mps, last.v_mps));
        }
        // partition_point: first level strictly above the query height.
        let above = self
            .levels
            .partition_point(|level| level.height_m_arl <= height_m_arl);
        let hi = self.levels[above];
        let lo = self.levels[above - 1];
        let span = hi.height_m_arl - lo.height_m_arl;
        let t = if span > 0.0 {
            (height_m_arl - lo.height_m_arl) / span
        } else {
            0.0
        };
        Some((
            lo.u_mps + t * (hi.u_mps - lo.u_mps),
            lo.v_mps + t * (hi.v_mps - lo.v_mps),
        ))
    }
}

/// Predicted radial velocity field for one cut, on its velocity grid's
/// lattice.  Used by the eval harness for the reference-RMS metric; the
/// engine itself projects internally.
pub fn project_environmental_winds(
    profile: &EnvironmentalWindProfile,
    cut: &radar_core::ElevationCut,
    grid: &radar_core::MomentGrid,
) -> Vec<f32> {
    let azimuths = crate::radial_azimuths(cut, grid);
    project_profile_to_tilt(
        profile,
        cut.elevation_deg,
        &azimuths,
        grid.gate_range.first_gate_m,
        grid.gate_range.gate_spacing_m,
        grid.gate_range.gate_count,
    )
}

/// Predicted radial velocities for one tilt: `values[row * gates + gate]`,
/// positive away from the radar (NEXRAD convention).
///
/// Beam height comes from the 4/3-effective-earth-radius model
/// (Doviak & Zrnić 1993, *Doppler Radar and Weather Observations*, 2nd ed.,
/// eq. 2.28b — [`radar_core::beam_height_above_radar_m`]), (u, v) is
/// interpolated in height, and the projection is
/// v̂ᵣ = (u·sin az + v·cos az)·cos φₑ  (elevation φₑ; vertical air motion
/// ignored — see module doc).
pub(crate) fn project_profile_to_tilt(
    profile: &EnvironmentalWindProfile,
    elevation_deg: f32,
    azimuths_deg: &[f32],
    first_gate_m: i32,
    gate_spacing_m: i32,
    gates: usize,
) -> Vec<f32> {
    let rows = azimuths_deg.len();
    let mut values = vec![f32::NAN; rows.saturating_mul(gates)];
    if rows == 0 || gates == 0 {
        return values;
    }
    let cos_elev = (elevation_deg as f64).to_radians().cos() as f32;

    // Per-gate wind: height varies with range only, so interpolate once per
    // gate, not once per (row, gate).
    let mut gate_wind = vec![(f32::NAN, f32::NAN); gates];
    for (gate, slot) in gate_wind.iter_mut().enumerate() {
        let slant_range_m = first_gate_m as f64 + gate as f64 * gate_spacing_m as f64;
        let height = beam_height_above_radar_m(slant_range_m.max(0.0), elevation_deg as f64);
        if let Some((u, v)) = profile.wind_at(height as f32) {
            *slot = (u, v);
        }
    }

    for (row, azimuth_deg) in azimuths_deg.iter().enumerate() {
        if !azimuth_deg.is_finite() {
            continue;
        }
        let (sin_az, cos_az) = (*azimuth_deg as f64).to_radians().sin_cos();
        let (sin_az, cos_az) = (sin_az as f32, cos_az as f32);
        let out = &mut values[row * gates..(row + 1) * gates];
        for (gate, slot) in out.iter_mut().enumerate() {
            let (u, v) = gate_wind[gate];
            if u.is_finite() && v.is_finite() {
                *slot = (u * sin_az + v * cos_az) * cos_elev;
            }
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(levels: Vec<EnvWindLevel>) -> EnvironmentalWindProfile {
        EnvironmentalWindProfile {
            levels,
            valid_time: DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    fn level(height: f32, u: f32, v: f32) -> EnvWindLevel {
        EnvWindLevel {
            height_m_arl: height,
            u_mps: u,
            v_mps: v,
        }
    }

    #[test]
    fn uniform_wind_projects_to_cosine_of_azimuth_minus_direction() {
        // 20 m/s wind blowing TOWARD 90° (from the west): u = 20, v = 0.
        // Radial velocity at azimuth az is 20·cos(az − 90°) at 0° elevation.
        let p = profile(vec![level(0.0, 20.0, 0.0), level(10_000.0, 20.0, 0.0)]);
        let azimuths: Vec<f32> = (0..360).map(|az| az as f32).collect();
        let projected = project_profile_to_tilt(&p, 0.0, &azimuths, 1000, 250, 4);
        for (row, azimuth) in azimuths.iter().enumerate() {
            let expected = 20.0 * (azimuth - 90.0).to_radians().cos();
            let got = projected[row * 4 + 2];
            assert!(
                (got - expected).abs() < 0.05,
                "az {azimuth}: got {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn interpolates_between_levels_and_clamps_at_profile_ends() {
        let p = profile(vec![level(1000.0, 10.0, 0.0), level(3000.0, 30.0, 0.0)]);
        assert_eq!(p.wind_at(2000.0), Some((20.0, 0.0)));
        assert_eq!(p.wind_at(0.0), Some((10.0, 0.0)), "below: lowest level");
        assert_eq!(p.wind_at(9000.0), Some((30.0, 0.0)), "above: highest level");
    }

    #[test]
    fn beam_height_reaches_upper_levels_at_long_range() {
        // Doviak & Zrnić (1993) 4/3-earth model: 0.5° beam at 200 km sits
        // near 4.1 km ARL, so the projection must sample the upper wind.
        let p = profile(vec![level(0.0, 0.0, 10.0), level(4000.0, 0.0, 40.0)]);
        let azimuths = vec![0.0f32]; // v̂ = v at azimuth 0, elevation ~0.
        let gate_spacing = 250;
        let gates = 801; // gate 800 → 1000 + 800·250 = 201 km slant range
        let projected = project_profile_to_tilt(&p, 0.5, &azimuths, 1000, gate_spacing, gates);
        let near = projected[4]; // 2 km range: beam essentially at 0 m ARL
        let far = projected[800];
        assert!((near - 10.0).abs() < 1.0, "near gate got {near}");
        assert!(far > 38.0, "far gate should sample the 4 km wind: {far}");
    }

    #[test]
    fn staleness_and_malformed_profiles_are_rejected() {
        let now = DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::days(365);
        let mut p = profile(vec![level(0.0, 1.0, 1.0), level(1000.0, 2.0, 2.0)]);
        p.valid_time = now - chrono::Duration::hours(2);
        assert!(p.usable_for(now), "2 h old profile is fresh");
        p.valid_time = now - chrono::Duration::hours(4);
        assert!(!p.usable_for(now), "4 h old profile is stale");
        p.valid_time = now + chrono::Duration::hours(4);
        assert!(!p.usable_for(now), "far-future profile is stale too");

        p.valid_time = now;
        p.levels = vec![];
        assert!(!p.usable_for(now), "empty profile");
        p.levels = vec![level(1000.0, 1.0, 1.0), level(500.0, 1.0, 1.0)];
        assert!(!p.usable_for(now), "non-increasing heights");
        p.levels = vec![level(0.0, f32::NAN, 1.0)];
        assert!(!p.usable_for(now), "non-finite component");
    }
}
