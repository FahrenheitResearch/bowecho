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

/// The GBVTD fit on one radius ring: axisymmetric (wavenumber-0) tangential and
/// radial wind, plus the wavenumber-1 tangential asymmetry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingFit {
    pub radius_km: f32,
    /// Axisymmetric tangential wind (m/s); positive = cyclonic (Northern
    /// Hemisphere counter-clockwise).
    pub vt: f32,
    /// Axisymmetric radial wind (m/s); positive = outflow.
    pub vr: f32,
    /// Wavenumber-1 tangential-wind Fourier terms (m/s): the ring tangential
    /// wind is modeled as `VT(θ) = vt + vt1_cos·cos θ + vt1_sin·sin θ`, where θ
    /// is the GBVTD mathematical angle measured around the storm center from the
    /// radar→center axis (see `fit_ring`). These capture the storm's wavenumber-1
    /// asymmetry (e.g. a stronger eyewall on one side; Lee et al. 1999, §4).
    pub vt1_cos: f32,
    pub vt1_sin: f32,
    /// Wavenumber-1 tangential asymmetry amplitude, `hypot(vt1_cos, vt1_sin)`
    /// (m/s): 0 for a purely axisymmetric vortex.
    pub vt1_amp: f32,
    /// Phase of the wavenumber-1 tangential maximum, degrees in [-180, 180),
    /// `atan2(vt1_sin, vt1_cos)`, measured from the radar→center axis toward the
    /// GBVTD-positive (mathematically counter-clockwise) direction.
    pub vt1_phase_deg: f32,
    pub samples: usize,
    /// RMS Doppler residual of the axisymmetric (wavenumber-0) fit (m/s). The
    /// wavenumber-1 asymmetry is fit as a diagnostic on top of this and does not
    /// change `rms`, `vt`, or `vr`, so RMW / peak-wind gating is unchanged.
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
///
/// `wind_ms` is an EXTERNAL uniform environmental wind (Um east, Vm north, m/s)
/// — in practice the storm-motion vector from the tropical layer. A single
/// Doppler radar cannot separate this environmental flow from the vortex's own
/// tangential/radial wind: it aliases into the fit and biases the retrieved VT
/// *downward* (a data-driven fit for it collapses the vortex, so it must come
/// from outside). We remove it here by subtracting its beam-projected component
/// (`bx*Um + by*Vm`) from every Doppler sample before the VT/VR accumulation,
/// which de-aliases VT back up toward best-track intensity. Pass `(0.0, 0.0)`
/// for the unbiased/no-motion case (leaves behavior unchanged).
fn fit_ring(
    sampler: &Sampler<'_>,
    center_km: (f32, f32),
    radius_km: f32,
    n_azimuths: usize,
    wind_ms: (f64, f64),
) -> Option<RingFit> {
    let (mut saa, mut sac, mut scc, mut sad, mut scd) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    let mut samples = 0usize;
    // Per accepted sample: (a, c, d, a·cos θ, a·sin θ). The last two are the
    // wavenumber-1 tangential design columns (fit additively after the
    // axisymmetric solve, below); θ is the GBVTD mathematical angle.
    let mut resid_terms: Vec<(f64, f64, f64, f64, f64)> = Vec::with_capacity(n_azimuths);
    // GBVTD mathematical angle θ is measured around the storm center from the
    // radar→center axis: θ = β − φ₀, φ₀ = atan2(center_y, center_x). Referencing
    // θ to this axis makes the wavenumber-1 phase physically meaningful and keeps
    // the asymmetry basis (below) orthogonal to the axisymmetric VT0/VR0 basis in
    // the R≪R_center limit (Lee et al. 1999, §4).
    let phi0 = center_km.1.atan2(center_km.0);
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
        // Remove the beam-projected external environmental (storm-motion) wind
        // that single-Doppler GBVTD cannot separate from the vortex (see the
        // fn doc): the uniform flow (Um, Vm) contributes `Um*bx + Vm*by` to
        // this beam's Doppler velocity. Only ~half the motion projects on any
        // given beam, but subtracting it removes the dominant VT bias.
        let env = wind_ms.0 * f64::from(bx) + wind_ms.1 * f64::from(by);
        let d = f64::from(vd) - env;
        // Wavenumber-1 tangential-asymmetry basis at the GBVTD math angle
        // (beta - phi0), fit as a secondary least-squares below.
        let (st, ct) = (beta - phi0).sin_cos();
        let (ac, as_) = (a * f64::from(ct), a * f64::from(st));
        saa += a * a;
        sac += a * c;
        scc += c * c;
        sad += a * d;
        scd += c * d;
        resid_terms.push((a, c, d, ac, as_));
        samples += 1;
    }
    if samples < 8 {
        return None;
    }
    // Solve the 2x2 normal equations for (VT, VR).
    let det = saa * scc - sac * sac;
    // Conditioning / observability gates (Lee et al. 1999): the VT basis
    // vector a(β) vanishes as a ring approaches the radar, so near-radar or
    // narrow-arc rings make the VT column collapse and VT explode (dividing by
    // a near-zero det). `mean_a2` is the tangential observability; `rho` is the
    // 2x2 normal-matrix conditioning in [0,1] (~1 for a full orthogonal ring,
    // →0 near the radar or on a partial arc). This replaces the old, far too
    // loose `det.abs() < 1e-6` and kills the ~3500 kt off-eye artifact.
    let mean_a2 = saa / samples as f64;
    let rho = if saa > 0.0 && scc > 0.0 {
        det / (saa * scc)
    } else {
        0.0
    };
    if mean_a2 < MIN_TANGENTIAL_OBSERVABILITY || rho < MIN_RING_CONDITION {
        return None;
    }
    let vt = (sad * scc - scd * sac) / det;
    let vr = (saa * scd - sac * sad) / det;

    // Wavenumber-1 tangential asymmetry, fit ADDITIVELY on top of the
    // axisymmetric solution: model VT(θ) = vt + VTc·cos θ + VTs·sin θ. Because
    // the asymmetry columns a·cos θ and a·sin θ project onto different Doppler
    // azimuthal harmonics than the axisymmetric a (VT0) and c (VR0) columns —
    // wavenumber 0/2 vs wavenumber 1 in the R≪R_center limit — solving them
    // against the axisymmetric residual recovers the same VTc/VTs as a joint fit
    // while leaving `vt`, `vr`, and `rms` (hence all RMW/peak-wind gating)
    // byte-for-byte unchanged. This is the Lee et al. 1999 harmonic
    // decomposition carried to wavenumber 1. NOTE: VTs (the along-axis component)
    // aliases with any uniform environmental wind; storm-motion / mean-wind
    // removal in the axisymmetric solve de-aliases it.
    let (mut vt1_cos, mut vt1_sin) = (0.0f64, 0.0f64);
    let (mut m11, mut m12, mut m22, mut r1, mut r2) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    let mut sse = 0.0f64;
    for (a, c, d, ac, as_) in &resid_terms {
        let e = a * vt + c * vr - d; // axisymmetric residual (model − obs)
        sse += e * e;
        let target = -e; // obs − axisymmetric model
        m11 += ac * ac;
        m12 += ac * as_;
        m22 += as_ * as_;
        r1 += ac * target;
        r2 += as_ * target;
    }
    // Only report an asymmetry when the wavenumber-1 basis is observable and its
    // 2x2 normal matrix is well conditioned (a partial arc collapses it); else
    // leave the ring purely axisymmetric.
    let det2 = m11 * m22 - m12 * m12;
    if m11 > 0.0 && m22 > 0.0 && det2 / (m11 * m22) >= MIN_ASYMMETRY_CONDITION {
        vt1_cos = (r1 * m22 - r2 * m12) / det2;
        vt1_sin = (m11 * r2 - m12 * r1) / det2;
    }
    let (vt1_cos, vt1_sin) = (vt1_cos as f32, vt1_sin as f32);
    Some(RingFit {
        radius_km,
        vt: vt as f32,
        vr: vr as f32,
        vt1_cos,
        vt1_sin,
        vt1_amp: vt1_cos.hypot(vt1_sin),
        vt1_phase_deg: vt1_sin.atan2(vt1_cos).to_degrees(),
        samples,
        rms: (sse / samples as f64).sqrt() as f32,
    })
}

/// A ring only counts toward the RMW / peak wind / center score if it is a
/// physically plausible RMW (not a tiny, under-sampled inner ring the center
/// search can game into a spurious peak) and well sampled around its
/// circumference. On real data a 6 km, 17-sample inner ring produced a bogus
/// 93 m/s peak; this rejects that class of artifact.
const MIN_CORE_RADIUS_KM: f32 = 10.0;
/// ~0.6 azimuthal coverage of the 72-sample ring: a partially-sampled far-range
/// ring loses basis orthogonality and inflates VT, so reject it.
const MIN_CORE_SAMPLES: usize = 43;
/// Tangential observability and normal-matrix conditioning floors for a ring
/// fit (see `fit_ring`). Tuned on real PGUA volumes, not synthetic-only.
const MIN_TANGENTIAL_OBSERVABILITY: f64 = 0.08;
const MIN_RING_CONDITION: f64 = 0.20;
/// Conditioning floor for the wavenumber-1 tangential 2x2 normal matrix (see
/// `fit_ring`). A full ring gives ~1 (its off-diagonal ~0); a partial arc drives
/// it toward 0. Below this, the asymmetry is not observable and is reported 0.
const MIN_ASYMMETRY_CONDITION: f64 = 0.05;
/// Physical + quality gates so a bad-center or far-range-folded ring cannot be
/// reported as a real RMW. VT_MAX ≈ 204 kt sits above any observed tangential
/// wind but well below the absurd near-radar blow-up; the RMS gates reject
/// rings the wavenumber-0 model barely explains (Lee et al. 1999).
const VT_MAX_PHYSICAL_MPS: f32 = 105.0;
/// Loose absolute RMS cap — only to reject truly garbage rings. On real data
/// the wavenumber-0 model leaves a substantial residual (the environmental
/// wind is not yet removed), so the eyewall itself can sit near ~20 m/s RMS;
/// the physical work is done by the `rms <= MAX_RMS_TO_VT * vt` ratio gate.
const MAX_CORE_RMS_MPS: f32 = 35.0;
const MAX_RMS_TO_VT: f32 = 0.5;
/// A single radar cannot GBVTD a storm sitting on top of it, and the analysis
/// ring must stay well inside the radar→center distance (Lee et al. 1999).
const MIN_CENTER_RANGE_KM: f32 = 25.0;
const MAX_RADIUS_FRACTION: f32 = 0.7;

fn ring_is_core_candidate(ring: &RingFit) -> bool {
    ring.radius_km >= MIN_CORE_RADIUS_KM
        && ring.samples >= MIN_CORE_SAMPLES
        && ring.vt > 0.0
        && ring.vt <= VT_MAX_PHYSICAL_MPS
        && ring.rms <= MAX_CORE_RMS_MPS
        && ring.rms <= MAX_RMS_TO_VT * ring.vt
}

/// Retrieve the axisymmetric circulation about a KNOWN center, over the given
/// radii (km). `n_azimuths` ring samples (72 = every 5°) is a good default.
/// `wind_ms` is the external environmental (storm-motion) wind removed from
/// each Doppler sample; pass `(0.0, 0.0)` when none is available (see
/// [`fit_ring`]).
pub fn retrieve_axisymmetric(
    field: &PolarVelocityField,
    center_km: (f32, f32),
    radii_km: &[f32],
    n_azimuths: usize,
    wind_ms: (f64, f64),
) -> TcCirculation {
    let sampler = field.sampler();
    let rings: Vec<RingFit> = radii_km
        .iter()
        .filter_map(|&r| fit_ring(&sampler, center_km, r, n_azimuths, wind_ms))
        .collect();
    let r_center = (center_km.0 * center_km.0 + center_km.1 * center_km.1).sqrt();
    let max_radius = MAX_RADIUS_FRACTION * r_center;
    let (rmw_km, vt_max) = rings
        .iter()
        .filter(|ring| ring_is_core_candidate(ring) && ring.radius_km <= max_radius)
        .max_by(|a, b| a.vt.total_cmp(&b.vt))
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
/// `wind_ms` is the external environmental (storm-motion) wind (Um east, Vm
/// north, m/s) removed from every Doppler sample before fitting; pass
/// `(0.0, 0.0)` when no storm motion is available (see [`fit_ring`]).
pub fn find_center_and_retrieve(
    field: &PolarVelocityField,
    guess_km: (f32, f32),
    search_km: f32,
    step_km: f32,
    radii_km: &[f32],
    wind_ms: (f64, f64),
) -> Option<TcCirculation> {
    let sampler = field.sampler();
    let step = step_km.max(0.5);
    let steps = (search_km / step).round() as i32;
    let mut best: Option<TcCirculation> = None;
    let mut best_score = f32::NEG_INFINITY;
    for iy in -steps..=steps {
        for ix in -steps..=steps {
            let center = (guess_km.0 + ix as f32 * step, guess_km.1 + iy as f32 * step);
            // A single radar cannot GBVTD a storm sitting on top of it, and the
            // analysis rings must stay well inside the radar→center distance.
            let r_center = (center.0 * center.0 + center.1 * center.1).sqrt();
            if r_center < MIN_CENTER_RANGE_KM {
                continue;
            }
            let max_radius = MAX_RADIUS_FRACTION * r_center;
            let eligible = radii_km.iter().filter(|&&r| r <= max_radius).count();
            let mut score = f32::NEG_INFINITY;
            let mut rings = Vec::with_capacity(eligible);
            for &r in radii_km {
                if r > max_radius {
                    continue;
                }
                if let Some(fit) = fit_ring(&sampler, center, r, 72, wind_ms) {
                    if ring_is_core_candidate(&fit) {
                        score = score.max(fit.vt);
                    }
                    rings.push(fit);
                }
            }
            // Require both adequate coverage and a plausible, well-sampled core
            // ring; otherwise this center is not a real circulation.
            if eligible < 3 || rings.len() < eligible / 2 || score <= 0.0 {
                continue;
            }
            if score > best_score {
                best_score = score;
                let best_ring = rings
                    .iter()
                    .filter(|ring| ring_is_core_candidate(ring))
                    .max_by(|a, b| a.vt.total_cmp(&b.vt))
                    .expect("score>0 implies a core candidate exists");
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

    /// Like [`synthetic_vortex`] but with an imposed wavenumber-1 tangential
    /// asymmetry: the tangential wind is scaled by `1 + (asym_amp/vt) · cos(θ −
    /// phase)`, θ = GBVTD math angle about the center from the radar→center axis.
    /// This is the field the retrieval must decompose back into VT0 and the
    /// wavenumber-1 (VTc, VTs) terms.
    fn synthetic_vortex_asym(
        center_km: (f32, f32),
        rmw_km: f32,
        vt_max: f32,
        asym_amp: f32,
        asym_phase_deg: f32,
    ) -> PolarVelocityField {
        let n_radials = 720;
        let gate_count = 1000;
        let first_gate_m = 0.0f32;
        let gate_spacing_m = 250.0f32;
        let phi0 = center_km.1.atan2(center_km.0);
        let phase = asym_phase_deg.to_radians();
        let azimuths_deg: Vec<f32> = (0..n_radials).map(|i| i as f32 * 0.5).collect();
        let mut values = vec![f32::NAN; n_radials * gate_count];
        for (row, &az) in azimuths_deg.iter().enumerate() {
            let (sa, ca) = az.to_radians().sin_cos();
            let (bx, by) = (sa, ca);
            for gate in 0..gate_count {
                let range_km = (first_gate_m + gate as f32 * gate_spacing_m) / 1000.0;
                let (gx, gy) = (range_km * sa, range_km * ca);
                let (dx, dy) = (gx - center_km.0, gy - center_km.1);
                let r = (dx * dx + dy * dy).sqrt();
                if r < 0.5 {
                    continue;
                }
                let beta = dy.atan2(dx);
                let (sb, cb) = beta.sin_cos();
                // Axisymmetric magnitude plus wavenumber-1 asymmetry at this
                // GBVTD angle θ = beta − phi0.
                let vt = rankine_vt(r, rmw_km, vt_max) + asym_amp * (beta - phi0 - phase).cos();
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

        // No environmental wind in the synthetic vortex.
        let circ = retrieve_axisymmetric(&field, center, &radii, 144, (0.0, 0.0));

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
        let circ = find_center_and_retrieve(&field, guess, 12.0, 2.0, &radii, (0.0, 0.0))
            .expect("center found");

        assert!(
            (circ.center_km.0 - center.0).abs() <= 3.0
                && (circ.center_km.1 - center.1).abs() <= 3.0,
            "recovered center {:?} vs {:?}",
            circ.center_km,
            center
        );
        assert!(circ.vt_max.unwrap() > 44.0, "vt_max = {:?}", circ.vt_max);
    }

    #[test]
    fn recovers_imposed_wavenumber1_asymmetry() {
        let center = (0.0, 130.0); // 130 km north of the radar
        let (rmw, vtmax) = (25.0f32, 50.0f32);
        let (amp, phase_deg) = (15.0f32, 40.0f32);
        let field = synthetic_vortex_asym(center, rmw, vtmax, amp, phase_deg);
        let radii: Vec<f32> = (15..=60).step_by(5).map(|r| r as f32).collect();

        let circ = retrieve_axisymmetric(&field, center, &radii, 180, (0.0, 0.0));

        // The axisymmetric peak is still recovered — the wavenumber-1 term does
        // not leak into VT0 (it lives in different Doppler harmonics).
        assert!(
            (circ.vt_max.unwrap() - vtmax).abs() < 3.0,
            "vt_max = {:?} (asymmetry leaked into VT0)",
            circ.vt_max
        );

        // Every well-fit ring recovers the imposed asymmetry amplitude & phase.
        let mut checked = 0;
        for ring in &circ.rings {
            if !ring_is_core_candidate(ring) {
                continue;
            }
            assert!(
                (ring.vt1_amp - amp).abs() < 3.0,
                "r={} km: VT1 amp {} vs imposed {}",
                ring.radius_km,
                ring.vt1_amp,
                amp
            );
            let dphase = angular_distance_deg(ring.vt1_phase_deg, phase_deg);
            assert!(
                dphase < 15.0,
                "r={} km: VT1 phase {} vs imposed {} (Δ {:.1}°)",
                ring.radius_km,
                ring.vt1_phase_deg,
                phase_deg,
                dphase
            );
            checked += 1;
        }
        assert!(checked >= 3, "only {checked} rings checked");

        // Control: a purely axisymmetric vortex of the same core retrieves a
        // near-zero wavenumber-1 amplitude (no spurious asymmetry).
        let sym = synthetic_vortex_asym(center, rmw, vtmax, 0.0, 0.0);
        let sym_circ = retrieve_axisymmetric(&sym, center, &radii, 180, (0.0, 0.0));
        for ring in &sym_circ.rings {
            assert!(
                ring.vt1_amp < 3.0,
                "spurious asymmetry at r={} km: amp {}",
                ring.radius_km,
                ring.vt1_amp
            );
        }
    }

    /// Real-data check: dealias a genuine hurricane volume and retrieve its
    /// circulation. Env-gated on a Level-II path (e.g. the KLIX Hurricane Ida
    /// landfall volume). Prints the retrieval so it can be sanity-checked
    /// against the known storm; never verify GBVTD on synthetic data alone.
    #[test]
    fn gbvtd_on_real_hurricane_volume() {
        let Some(path) = std::env::var_os("BOWECHO_GBVTD_VOLUME") else {
            return;
        };
        let volume =
            nexrad_io::decode_volume_from_path(std::path::Path::new(&path)).expect("decode volume");
        let (cut_index, cut) = volume
            .cuts
            .iter()
            .enumerate()
            .filter(|(_, c)| c.moments.contains_key(&radar_core::MomentType::Velocity))
            .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
            .expect("a velocity cut");
        let velocity = cut.moments.get(&radar_core::MomentType::Velocity).unwrap();
        let dealiased = crate::dealias_velocity_grid(cut, velocity);
        let field = PolarVelocityField::from_dealiased_velocity(cut, &dealiased);

        let guess_x: f32 = std::env::var("BOWECHO_GBVTD_GUESS_X")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(-36.0);
        let guess_y: f32 = std::env::var("BOWECHO_GBVTD_GUESS_Y")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(-137.0);
        // External storm-motion (environmental) wind, m/s, Um east / Vm north.
        // Defaults to none; set BOWECHO_GBVTD_UM / _VM to de-alias VT with a
        // best-track motion vector (e.g. BAVI moving WNW: Um=-6, Vm=3).
        let um: f64 = std::env::var("BOWECHO_GBVTD_UM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let vm: f64 = std::env::var("BOWECHO_GBVTD_VM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let radii: Vec<f32> = (6..=100).step_by(4).map(|r| r as f32).collect();
        let circ =
            find_center_and_retrieve(&field, (guess_x, guess_y), 40.0, 3.0, &radii, (um, vm))
                .expect("a circulation");

        eprintln!(
            "GBVTD real: cut {cut_index} ({:.2} deg) wind=({um:.1},{vm:.1}) m/s center=({:.1},{:.1}) km RMW={:?} VT_max={:?} m/s",
            cut.elevation_deg, circ.center_km.0, circ.center_km.1, circ.rmw_km, circ.vt_max
        );
        for ring in &circ.rings {
            eprintln!(
                "  r={:>4.0} km  VT={:>6.1}  VR={:>6.1}  VT1={:>5.1} @ {:>6.0}°  n={:>3}  rms={:.1}",
                ring.radius_km,
                ring.vt,
                ring.vr,
                ring.vt1_amp,
                ring.vt1_phase_deg,
                ring.samples,
                ring.rms
            );
        }
        // Report the wavenumber-1 asymmetry at the RMW (the eyewall) — the
        // headline number for storm asymmetry / motion-relative structure.
        if let Some(rmw) = circ.rmw_km
            && let Some(ring) = circ.rings.iter().min_by(|a, b| {
                (a.radius_km - rmw)
                    .abs()
                    .total_cmp(&(b.radius_km - rmw).abs())
            })
        {
            eprintln!(
                "GBVTD asymmetry at RMW ({:.0} km): wavenumber-1 VT amp = {:.1} m/s ({:.1}% of VT0={:.1}), phase {:.0}°",
                ring.radius_km,
                ring.vt1_amp,
                100.0 * ring.vt1_amp / ring.vt.max(0.1),
                ring.vt,
                ring.vt1_phase_deg,
            );
        }
    }

    /// Real-data audit (env-gated): decode every volume in a directory and
    /// tally moment presence. Explains the PGUA loop bugs — velocity-less
    /// frames force the sticky reflectivity fallback, and empty / no-moment
    /// frames are dropped from the loop history. Point BOWECHO_PGUA_DIR at the
    /// on-disk cache (e.g. %LOCALAPPDATA%\BowEcho\cache\level2\PGUA).
    #[test]
    fn pgua_frame_moment_audit() {
        let Some(dir) = std::env::var_os("BOWECHO_PGUA_DIR") else {
            return;
        };
        use radar_core::MomentType;
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .collect();
        entries.sort();

        // Replica of ui_core::loop_engine::policy::estimated_volume_bytes,
        // to test the 8 GiB primary byte-budget hypothesis for the 200->93
        // frame drop without adding a ui_core dependency to render2d.
        fn est_volume_bytes(volume: &radar_core::RadarVolume) -> usize {
            let mut bytes = std::mem::size_of::<radar_core::RadarVolume>();
            for cut in &volume.cuts {
                bytes += std::mem::size_of_val(cut);
                bytes += cut.radials.len() * std::mem::size_of::<radar_core::Radial>();
                for grid in cut.moments.values() {
                    bytes += std::mem::size_of_val(grid);
                    bytes += grid.radial_indices.len() * std::mem::size_of::<usize>();
                    bytes += grid.storage.len() * usize::from(grid.storage.word_size_bits() / 8);
                }
            }
            bytes
        }

        let (mut total, mut empty_cuts, mut with_vel, mut without_vel, mut no_disp) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut decode_fail = 0usize;
        let mut times = std::collections::BTreeSet::new();
        let mut velless: Vec<String> = Vec::new();
        let mut sizes: Vec<(i64, usize)> = Vec::new();

        for path in &entries {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let volume = match nexrad_io::decode_volume_from_path(path) {
                Ok(v) => v,
                Err(_) => {
                    decode_fail += 1;
                    continue;
                }
            };
            total += 1;
            let ts = volume.volume_time.timestamp();
            times.insert(ts);
            sizes.push((ts, est_volume_bytes(&volume)));
            let n_ref = volume
                .cuts
                .iter()
                .filter(|c| c.moments.contains_key(&MomentType::Reflectivity))
                .count();
            let n_vel = volume
                .cuts
                .iter()
                .filter(|c| c.moments.contains_key(&MomentType::Velocity))
                .count();
            if volume.cuts.is_empty() {
                empty_cuts += 1;
            }
            if n_vel > 0 {
                with_vel += 1;
            } else {
                without_vel += 1;
                if velless.len() < 50 {
                    velless.push(format!(
                        "{name}  cuts={} ref={} vel={}",
                        volume.cuts.len(),
                        n_ref,
                        n_vel
                    ));
                }
            }
            if n_ref == 0 && n_vel == 0 {
                no_disp += 1;
            }
        }

        eprintln!("=== PGUA MOMENT AUDIT ({} files) ===", entries.len());
        eprintln!("decoded ok:         {total}");
        eprintln!("decode failures:    {decode_fail}");
        eprintln!("unique scan times:  {}", times.len());
        eprintln!("empty cuts:         {empty_cuts}");
        eprintln!("WITH velocity:      {with_vel}");
        eprintln!("WITHOUT velocity:   {without_vel}");
        eprintln!("no displayable:     {no_disp}");
        eprintln!("--- velocity-less frames (up to 50) ---");
        for line in &velless {
            eprintln!("  {line}");
        }

        // Byte-budget test: newest-first, how many fit in the 8 GiB primary
        // budget (the trim drops oldest first, keeps newest).
        sizes.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        const GIB8: usize = 8 * 1024 * 1024 * 1024;
        let total_bytes: usize = sizes.iter().map(|(_, b)| *b).sum();
        let avg_mb = if total > 0 {
            (total_bytes / total) as f64 / (1024.0 * 1024.0)
        } else {
            0.0
        };
        let mut acc = 0usize;
        let mut fit = 0usize;
        for (_, b) in &sizes {
            if acc + b > GIB8 {
                break;
            }
            acc += b;
            fit += 1;
        }
        eprintln!("--- byte-budget (8 GiB primary) ---");
        eprintln!("avg decoded/frame:  {avg_mb:.1} MB");
        eprintln!("total decoded:      {:.1} GB", total_bytes as f64 / 1e9);
        eprintln!("newest frames that fit in 8 GiB: {fit}");
    }
}
