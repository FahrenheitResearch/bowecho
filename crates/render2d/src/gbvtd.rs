//! GBVTD single-Doppler tropical-cyclone circulation retrieval.
//!
//! Recovers a tropical cyclone's axisymmetric tangential (VT) and radial (VR)
//! wind versus radius, its center, and its radius of maximum wind (RMW) from
//! ONE ground-based radar's dealiased radial-velocity field. A single Doppler
//! radar measures only the along-beam wind component, so the primary vortex is
//! reconstructed by exploiting the geometry of a full azimuthal ring about the
//! storm center.
//!
//! Method: Lee, Jou, Chang & Deng 1999, "Tropical Cyclone Kinematic Structure
//! Retrieved from Single-Doppler Radar Observations. Part I: Interpretation of
//! Doppler Velocity Patterns and the GBVTD Technique." *Mon. Wea. Rev.* 127,
//! 2419-2439. Center finding follows the GBVTD-simplex idea (Lee & Marks 2000,
//! *Mon. Wea. Rev.* 128, 1925-1936): the true center maximizes the retrieved
//! axisymmetric tangential wind, because an off-center analysis aliases the
//! symmetric circulation into apparent asymmetry.
//!
//! This module implements the axisymmetric (wavenumber-0) core: on each radius
//! ring centered on the storm, the observed Doppler velocity is projected onto
//! the local beam direction and least-squares fit for the ring-constant VT and
//! VR. Wavenumber-1+ asymmetry retrieval is a documented follow-up.

use radar_core::{ElevationCut, MomentGrid};

/// A dealiased radial-velocity PPI in polar form, radar at the origin.
/// `x` is east, `y` is north; azimuth is degrees from north, clockwise.
#[derive(Clone, Debug)]
pub struct PolarVelocityField {
    /// One azimuth (deg from north, clockwise) per radial.
    pub azimuths_deg: Vec<f32>,
    pub first_gate_m: f32,
    pub gate_spacing_m: f32,
    pub gate_count: usize,
    /// Dealiased radial velocity in m/s, row-major `[radial * gate_count +
    /// gate]`; NaN marks missing/thresholded gates.
    pub values: Vec<f32>,
}

/// The axisymmetric fit on one radius ring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingFit {
    pub radius_km: f32,
    /// Axisymmetric tangential wind (m/s); positive = cyclonic (Northern
    /// Hemisphere counter-clockwise).
    pub vt: f32,
    /// Axisymmetric radial wind (m/s); positive = outflow.
    pub vr: f32,
    pub samples: usize,
    /// RMS Doppler residual of the fit (m/s).
    pub rms: f32,
}

/// A retrieved TC primary circulation.
#[derive(Clone, Debug, PartialEq)]
pub struct TcCirculation {
    /// Storm center in radar-relative km (x east, y north).
    pub center_km: (f32, f32),
    pub rings: Vec<RingFit>,
    /// Radius of maximum (axisymmetric tangential) wind, km.
    pub rmw_km: Option<f32>,
    /// Peak axisymmetric tangential wind, m/s.
    pub vt_max: Option<f32>,
}

impl PolarVelocityField {
    /// Build the polar field from a dealiased velocity moment grid.
    pub fn from_dealiased_velocity(cut: &ElevationCut, grid: &MomentGrid) -> Self {
        let rows = grid.radial_count();
        let gates = grid.gate_range.gate_count;
        let azimuths_deg = crate::radial_azimuths(cut, grid);
        let mut values = vec![f32::NAN; rows.saturating_mul(gates)];
        let mut row_buf = vec![f32::NAN; gates];
        for row in 0..rows {
            crate::copy_scaled_velocity_row(grid, row, &mut row_buf);
            values[row * gates..(row + 1) * gates].copy_from_slice(&row_buf);
        }
        Self {
            azimuths_deg,
            first_gate_m: grid.gate_range.first_gate_m as f32,
            gate_spacing_m: (grid.gate_range.gate_spacing_m.max(1)) as f32,
            gate_count: gates,
            values,
        }
    }

    fn sampler(&self) -> Sampler<'_> {
        let mut sorted: Vec<(f32, usize)> = self
            .azimuths_deg
            .iter()
            .enumerate()
            .filter(|(_, az)| az.is_finite())
            .map(|(i, az)| (az.rem_euclid(360.0), i))
            .collect();
        sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
        Sampler {
            field: self,
            sorted,
        }
    }
}

/// Azimuth-indexed nearest-gate sampler over a [`PolarVelocityField`].
struct Sampler<'a> {
    field: &'a PolarVelocityField,
    /// (azimuth 0..360, radial index), ascending by azimuth.
    sorted: Vec<(f32, usize)>,
}

impl Sampler<'_> {
    /// Dealiased radial velocity nearest to radar-relative (x, y) km, if a
    /// valid gate exists there.
    fn sample(&self, x_km: f32, y_km: f32) -> Option<f32> {
        if self.sorted.is_empty() {
            return None;
        }
        let range_m = (x_km * x_km + y_km * y_km).sqrt() * 1000.0;
        let gate_f = ((range_m - self.field.first_gate_m) / self.field.gate_spacing_m).round();
        if !gate_f.is_finite() || gate_f < 0.0 || gate_f >= self.field.gate_count as f32 {
            return None;
        }
        let gate = gate_f as usize;
        let az = x_km.atan2(y_km).to_degrees().rem_euclid(360.0);
        let radial = self.nearest_radial(az)?;
        let value = self.field.values[radial * self.field.gate_count + gate];
        value.is_finite().then_some(value)
    }

    fn nearest_radial(&self, az: f32) -> Option<usize> {
        // Binary search the insertion point, then compare the two neighbours
        // (with wraparound) by circular angular distance.
        let pos = self.sorted.partition_point(|(a, _)| *a < az);
        let n = self.sorted.len();
        let lo = &self.sorted[(pos + n - 1) % n];
        let hi = &self.sorted[pos % n];
        let dl = angular_distance_deg(lo.0, az);
        let dh = angular_distance_deg(hi.0, az);
        Some(if dl <= dh { lo.1 } else { hi.1 })
    }
}

fn angular_distance_deg(a: f32, b: f32) -> f32 {
    ((a - b + 180.0).rem_euclid(360.0) - 180.0).abs()
}

/// Least-squares fit the ring-constant (VT, VR) on one radius ring about
/// `center_km`, sampling `n_azimuths` evenly around it. Returns None if too
/// few valid samples or the geometry is degenerate.
fn fit_ring(
    sampler: &Sampler<'_>,
    center_km: (f32, f32),
    radius_km: f32,
    n_azimuths: usize,
) -> Option<RingFit> {
    let (mut saa, mut sac, mut scc, mut sad, mut scd) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    let mut samples = 0usize;
    let mut resid_terms: Vec<(f64, f64, f64)> = Vec::with_capacity(n_azimuths);
    for k in 0..n_azimuths {
        let beta = std::f32::consts::TAU * k as f32 / n_azimuths as f32;
        let (sb, cb) = beta.sin_cos();
        let px = center_km.0 + radius_km * cb;
        let py = center_km.1 + radius_km * sb;
        let rho = (px * px + py * py).sqrt();
        if rho < 1e-3 {
            continue; // ring point sits on the radar
        }
        let Some(vd) = sampler.sample(px, py) else {
            continue;
        };
        // Beam unit vector (radar->point); coefficients of VT and VR.
        let (bx, by) = (px / rho, py / rho);
        let a = f64::from((-sb) * bx + cb * by); // VT coefficient
        let c = f64::from(cb * bx + sb * by); // VR coefficient
        let d = f64::from(vd);
        saa += a * a;
        sac += a * c;
        scc += c * c;
        sad += a * d;
        scd += c * d;
        resid_terms.push((a, c, d));
        samples += 1;
    }
    if samples < 8 {
        return None;
    }
    // Solve the 2x2 normal equations for (VT, VR).
    let det = saa * scc - sac * sac;
    if det.abs() < 1e-6 {
        return None; // degenerate (e.g. radar effectively at infinity)
    }
    let vt = (sad * scc - scd * sac) / det;
    let vr = (saa * scd - sac * sad) / det;
    let mut sse = 0.0f64;
    for (a, c, d) in &resid_terms {
        let e = a * vt + c * vr - d;
        sse += e * e;
    }
    Some(RingFit {
        radius_km,
        vt: vt as f32,
        vr: vr as f32,
        samples,
        rms: (sse / samples as f64).sqrt() as f32,
    })
}

/// Retrieve the axisymmetric circulation about a KNOWN center, over the given
/// radii (km). `n_azimuths` ring samples (72 = every 5°) is a good default.
pub fn retrieve_axisymmetric(
    field: &PolarVelocityField,
    center_km: (f32, f32),
    radii_km: &[f32],
    n_azimuths: usize,
) -> TcCirculation {
    let sampler = field.sampler();
    let rings: Vec<RingFit> = radii_km
        .iter()
        .filter_map(|&r| fit_ring(&sampler, center_km, r, n_azimuths))
        .collect();
    let (rmw_km, vt_max) = rings
        .iter()
        .max_by(|a, b| a.vt.total_cmp(&b.vt))
        .filter(|best| best.vt > 0.0)
        .map(|best| (Some(best.radius_km), Some(best.vt)))
        .unwrap_or((None, None));
    TcCirculation {
        center_km,
        rings,
        rmw_km,
        vt_max,
    }
}

/// GBVTD-simplex-style center finding: search a square grid about `guess_km`
/// (± `search_km`, step `step_km`) for the center that maximizes the peak
/// axisymmetric tangential wind, then retrieve about it. Returns None if no
/// candidate yields a usable circulation.
pub fn find_center_and_retrieve(
    field: &PolarVelocityField,
    guess_km: (f32, f32),
    search_km: f32,
    step_km: f32,
    radii_km: &[f32],
) -> Option<TcCirculation> {
    let sampler = field.sampler();
    let step = step_km.max(0.5);
    let steps = (search_km / step).round() as i32;
    let mut best: Option<TcCirculation> = None;
    let mut best_score = f32::NEG_INFINITY;
    for iy in -steps..=steps {
        for ix in -steps..=steps {
            let center = (guess_km.0 + ix as f32 * step, guess_km.1 + iy as f32 * step);
            let mut vt_max = f32::NEG_INFINITY;
            let mut rings = Vec::with_capacity(radii_km.len());
            for &r in radii_km {
                if let Some(fit) = fit_ring(&sampler, center, r, 72) {
                    vt_max = vt_max.max(fit.vt);
                    rings.push(fit);
                }
            }
            if rings.len() < radii_km.len() / 2 || vt_max <= 0.0 {
                continue;
            }
            if vt_max > best_score {
                best_score = vt_max;
                let best_ring = rings
                    .iter()
                    .max_by(|a, b| a.vt.total_cmp(&b.vt))
                    .expect("non-empty");
                best = Some(TcCirculation {
                    center_km: center,
                    rmw_km: Some(best_ring.radius_km),
                    vt_max: Some(best_ring.vt),
                    rings,
                });
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Modified-Rankine tangential wind: solid-body inside the RMW, decaying
    /// outside. VR = 0 (pure vortex).
    fn rankine_vt(r_km: f32, rmw_km: f32, vt_max: f32) -> f32 {
        if r_km <= rmw_km {
            vt_max * r_km / rmw_km
        } else {
            vt_max * (rmw_km / r_km)
        }
    }

    /// Synthesize the single-Doppler field a radar would see of a Rankine
    /// vortex centered at `center_km` (radar at origin).
    fn synthetic_vortex(center_km: (f32, f32), rmw_km: f32, vt_max: f32) -> PolarVelocityField {
        let n_radials = 720; // 0.5° spacing
        let gate_count = 1000; // 250 km at 250 m spacing
        let first_gate_m = 0.0f32;
        let gate_spacing_m = 250.0f32;
        let azimuths_deg: Vec<f32> = (0..n_radials).map(|i| i as f32 * 0.5).collect();
        let mut values = vec![f32::NAN; n_radials * gate_count];
        for (row, &az) in azimuths_deg.iter().enumerate() {
            let (sa, ca) = az.to_radians().sin_cos(); // az from north, clockwise
            let (bx, by) = (sa, ca); // beam unit (radar->gate)
            for gate in 0..gate_count {
                let range_km = (first_gate_m + gate as f32 * gate_spacing_m) / 1000.0;
                let (gx, gy) = (range_km * sa, range_km * ca);
                let (dx, dy) = (gx - center_km.0, gy - center_km.1);
                let r = (dx * dx + dy * dy).sqrt();
                if r < 0.5 {
                    continue; // eye
                }
                let beta = dy.atan2(dx);
                let (sb, cb) = beta.sin_cos();
                let vt = rankine_vt(r, rmw_km, vt_max);
                // Cyclonic tangential wind, no radial component.
                let (wx, wy) = (vt * -sb, vt * cb);
                values[row * gate_count + gate] = wx * bx + wy * by;
            }
        }
        PolarVelocityField {
            azimuths_deg,
            first_gate_m,
            gate_spacing_m,
            gate_count,
            values,
        }
    }

    #[test]
    fn retrieves_rankine_profile_at_true_center() {
        let center = (0.0, 120.0); // 120 km north of the radar
        let (rmw, vtmax) = (25.0f32, 55.0f32);
        let field = synthetic_vortex(center, rmw, vtmax);
        let radii: Vec<f32> = (10..=80).step_by(5).map(|r| r as f32).collect();

        let circ = retrieve_axisymmetric(&field, center, &radii, 144);

        // Peak tangential wind and RMW are recovered.
        assert!(
            (circ.vt_max.unwrap() - vtmax).abs() < 3.0,
            "vt_max = {:?}",
            circ.vt_max
        );
        assert!(
            (circ.rmw_km.unwrap() - rmw).abs() <= 5.0,
            "rmw = {:?}",
            circ.rmw_km
        );
        // VR is ~0 everywhere (pure vortex).
        for ring in &circ.rings {
            assert!(
                ring.vr.abs() < 4.0,
                "spurious VR at {} km: {}",
                ring.radius_km,
                ring.vr
            );
        }
        // The profile matches Rankine at a few radii.
        for ring in &circ.rings {
            let expected = rankine_vt(ring.radius_km, rmw, vtmax);
            assert!(
                (ring.vt - expected).abs() < 4.0,
                "VT({}) = {} vs {}",
                ring.radius_km,
                ring.vt,
                expected
            );
        }
    }

    #[test]
    fn simplex_recovers_the_storm_center() {
        let center = (-30.0, 110.0);
        let field = synthetic_vortex(center, 20.0, 50.0);
        let radii: Vec<f32> = (10..=60).step_by(5).map(|r| r as f32).collect();

        // Start the search 8 km off in both axes.
        let guess = (center.0 + 8.0, center.1 - 8.0);
        let circ =
            find_center_and_retrieve(&field, guess, 12.0, 2.0, &radii).expect("center found");

        assert!(
            (circ.center_km.0 - center.0).abs() <= 3.0
                && (circ.center_km.1 - center.1).abs() <= 3.0,
            "recovered center {:?} vs {:?}",
            circ.center_km,
            center
        );
        assert!(circ.vt_max.unwrap() > 44.0, "vt_max = {:?}", circ.vt_max);
    }
}
