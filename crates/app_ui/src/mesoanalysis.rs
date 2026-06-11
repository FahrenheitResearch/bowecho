//! Surface mesoanalysis: Bratseth (1986) successive corrections blending
//! surface observations with the HRRR background — the scheme that
//! converges to the Optimal Interpolation solution without forming or
//! inverting any matrix (Tellus 38A, 439–447).
//!
//! Implementation follows docs/mesoanalysis-spec.md (research-verified,
//! with the adversarial-pass corrections applied):
//! - Operates on INCREMENTS d_i = ob − background(station), never raw
//!   values; the analysis relaxes exactly to the background away from
//!   data (hard correlation cutoff at 3.5R).
//! - ADAS concrete weights (Lazarus et al. 2002, Eqs. 5–10): grid pulled
//!   by ρ/mᵢ, station estimates by (ρ + ε²I)/mᵢ, mᵢ = ε²ᵢ + Σⱼ ρⱼᵢ — the
//!   normalization that makes clusters de-weight themselves and the
//!   fixed point equal OI (Bratseth Eqs. 7–11; Gershgorin convergence).
//! - Bratseth's Eq. 8 optimization: iterate ONLY in station space,
//!   accumulating residuals; ONE gridding scatter at the end.
//! - QC per Tyndall & Horel (2013): innovation gross check
//!   |d| ≤ max(ε_m·σ_local(40 km), floor) with ε_m = 10, floors 3 K (T)
//!   / 4 K (Td); per-network ε² from their Table (CWOP-class inflated).
//! - Config anchors: R = 80 km (background-error scale, Tyndall, Horel &
//!   De Pondeca 2010 — NOT shrunk for dense mesonets; density is mᵢ's
//!   job), 10 iterations w/ tolerance exit (skill saturates ~10, Myrick
//!   et al. 2005 Fig. 4).
//!
//! v1 limitation (flagged, v1.1 work): the vertical elevation term
//! exp(−dz²/Rz²) and the intervening-terrain term (Myrick Eqs. 3–4)
//! need the orography grid plumbed; until then this is the horizontal
//! analysis — correct in the plains, conservative QC limits mountain
//! damage.

use crate::obs::SurfaceOb;
use rw_ui::FieldData;
use std::sync::Arc;

/// Horizontal background-error correlation scale, km (Tyndall 2010).
const R_KM: f64 = 80.0;
/// Hard-zero cutoff (Tyndall: 300 km for R = 80) — compact support and
/// exact background relaxation outside data.
const CUTOFF_KM: f64 = 3.5 * R_KM;
const MAX_ITER: usize = 12;
/// Stop when the largest station residual falls below this fraction of
/// the background error.
const TOL_FRAC: f64 = 0.05;
/// Innovation gross check multiplier (Tyndall Eq. 5).
const EPS_M: f64 = 10.0;
/// Vertical decorrelation scale, m (Tyndall, Horel & De Pondeca 2010).
const RZ_M: f64 = 200.0;
/// Intervening-terrain blockage scale, m (Myrick, Horel & Lazarus 2005,
/// Eqs. 3-4: rho *= exp(-a^2/RB^2), a = max(0, z_ridge - max(z_i, z_j))).
const RB_M: f64 = 2000.0;
// Wind components skip the vertical term (RTMA keeps wind anisotropy
// very weak); flagged per VarConfig::terrain_aware below.

/// Per-variable analysis configuration (spec §7 table).
#[derive(Clone, Copy)]
pub struct VarConfig {
    /// Background error stddev (HRRR 1-h).
    pub sigma_b: f64,
    /// ε² = (σ_o/σ_b)² for METAR-class and mesonet-class networks.
    pub eps2_metar: f64,
    pub eps2_mesonet: f64,
    /// Gross-check floor (same units as the variable).
    pub qc_floor: f64,
    /// Apply elevation/terrain decorrelation (off for wind, per RTMA).
    pub terrain_aware: bool,
}

/// T2m (Kelvin-equivalent °C units) — spec §7 row 1.
pub const T2M: VarConfig = VarConfig {
    sigma_b: 1.7,
    eps2_metar: 0.35,
    eps2_mesonet: 0.5,
    qc_floor: 3.0,
    terrain_aware: true,
};
/// Td2m — spec §7 row 2 (derived* anchors).
pub const TD2M: VarConfig = VarConfig {
    sigma_b: 2.0,
    eps2_metar: 0.6,
    eps2_mesonet: 1.5,
    qc_floor: 4.0,
    terrain_aware: true,
};
/// 10-m wind components, analyzed separately/univariately (spec §7 row 3;
/// sigma_o mesonet ratio 2.0 per Tyndall — RAWS wind). No vertical term.
pub const WIND10: VarConfig = VarConfig {
    sigma_b: 1.8,
    eps2_metar: 1.0,
    eps2_mesonet: 2.0,
    qc_floor: 7.5,
    terrain_aware: false,
};

/// One quality-controlled, background-matched observation ready for the
/// solver.
struct AnalysisOb {
    col: f64,
    row: f64,
    /// Innovation: ob − background at the station.
    d: f64,
    eps2: f64,
    /// Station elevation m MSL (reported, else model terrain at cell).
    elev: f64,
}

/// Result: the analyzed increment on the background grid + diagnostics.
pub struct Analysis {
    /// Increment per grid cell (row-major, background grid).
    pub increment: Vec<f32>,
    pub obs_used: usize,
    pub obs_rejected: usize,
    pub iterations: usize,
    /// RMS innovation before / station residual after — the fit.
    pub rms_before: f64,
    pub rms_after: f64,
}

/// Which observation value feeds the analysis for a store variable.
pub fn ob_value_for(var: &str, ob: &SurfaceOb) -> Option<f64> {
    let component = |east: bool| -> Option<f64> {
        let (dir, spd) = (ob.wind_dir_deg?, ob.wind_speed_kt?);
        let speed_ms = f64::from(spd) * 0.514_444;
        let rad = f64::from(dir).to_radians();
        Some(if east {
            -speed_ms * rad.sin()
        } else {
            -speed_ms * rad.cos()
        })
    };
    match var {
        "temperature_2m" => ob.temp_c.map(f64::from),
        "dewpoint_2m" => ob.dewpoint_c.map(f64::from),
        "u_10m" => component(true),
        "v_10m" => component(false),
        _ => None,
    }
}

pub fn config_for(var: &str) -> Option<VarConfig> {
    match var {
        "temperature_2m" => Some(T2M),
        "dewpoint_2m" => Some(TD2M),
        "u_10m" | "v_10m" => Some(WIND10),
        _ => None,
    }
}

/// Convert the °C observation into the background field's unit space,
/// branching on the field's declared units (mirrors rw-ui's reader) so
/// a K-vs-°C store difference can never poison the innovations.
fn ob_to_field_units(units: &str, value_c: f64) -> f64 {
    match units {
        "K" => value_c + 273.15,
        _ => value_c,
    }
}

/// Bilinear background value at fractional grid coords.
fn bilinear(field: &FieldData, col: f64, row: f64) -> Option<f64> {
    let (nx, ny) = (field.nx, field.ny);
    if col < 0.0 || row < 0.0 || col > (nx - 1) as f64 || row > (ny - 1) as f64 {
        return None;
    }
    let (c0, r0) = (col.floor() as usize, row.floor() as usize);
    let (c1, r1) = ((c0 + 1).min(nx - 1), (r0 + 1).min(ny - 1));
    let (fc, fr) = (col - c0 as f64, row - r0 as f64);
    let v = |r: usize, c: usize| -> Option<f64> {
        let value = *field.values.get(r * nx + c)?;
        value.is_finite().then_some(f64::from(value))
    };
    let (v00, v01, v10, v11) = (v(r0, c0)?, v(r0, c1)?, v(r1, c0)?, v(r1, c1)?);
    Some(
        v00 * (1.0 - fc) * (1.0 - fr)
            + v01 * fc * (1.0 - fr)
            + v10 * (1.0 - fc) * fr
            + v11 * fc * fr,
    )
}

/// Local background stddev within ~40 km of (col,row) — the gross check's
/// "keep good obs near drylines" term (Tyndall Eq. 5). Subsampled.
fn local_stddev(field: &FieldData, col: f64, row: f64, cells_40km: usize) -> f64 {
    let (nx, ny) = (field.nx, field.ny);
    let (c, r) = (col.round() as isize, row.round() as isize);
    let radius = cells_40km as isize;
    let step = (radius / 6).max(1);
    let mut sum = 0.0f64;
    let mut sum2 = 0.0f64;
    let mut count = 0usize;
    let mut rr = r - radius;
    while rr <= r + radius {
        let mut cc = c - radius;
        while cc <= c + radius {
            if rr >= 0 && cc >= 0 && (rr as usize) < ny && (cc as usize) < nx {
                let value = field.values[rr as usize * nx + cc as usize];
                if value.is_finite() {
                    sum += f64::from(value);
                    sum2 += f64::from(value) * f64::from(value);
                    count += 1;
                }
            }
            cc += step;
        }
        rr += step;
    }
    if count < 4 {
        return 0.0;
    }
    let mean = sum / count as f64;
    (sum2 / count as f64 - mean * mean).max(0.0).sqrt()
}

/// Run the Bratseth analysis for one variable.
///
/// `grid_cell_km` = the background's grid spacing (HRRR: 3.0).
/// `locate` maps an ob's lat/lon to fractional (col,row) on the grid, or
/// None when outside.
pub fn analyze(
    var: &str,
    field: &Arc<FieldData>,
    obs: &[SurfaceOb],
    grid_cell_km: f64,
    orography: Option<&[f32]>,
    locate: impl Fn(&SurfaceOb) -> Option<(f64, f64)>,
) -> Option<Analysis> {
    let config = config_for(var)?;
    let cells_40km = (40.0 / grid_cell_km).round() as usize;
    let r_cells = R_KM / grid_cell_km;
    let cutoff_cells = CUTOFF_KM / grid_cell_km;

    // ---- innovations + QC ----
    let mut accepted: Vec<AnalysisOb> = Vec::new();
    let mut rejected = 0usize;
    for ob in obs {
        let Some(value_c) = ob_value_for(var, ob) else {
            continue;
        };
        let Some((col, row)) = locate(ob) else {
            continue;
        };
        let Some(background) = bilinear(field, col, row) else {
            continue;
        };
        let d = ob_to_field_units(&field.units, value_c) - background;
        // Innovation gross check (Tyndall Eq. 5).
        let sigma_local = local_stddev(field, col, row, cells_40km);
        if d.abs() > (EPS_M * sigma_local).max(config.qc_floor) {
            rejected += 1;
            continue;
        }
        let eps2 = if ob.network == "METAR" {
            config.eps2_metar
        } else {
            config.eps2_mesonet
        };
        let terrain_at = |c: f64, r: f64| -> f64 {
            orography
                .and_then(|oro| oro.get(r.round() as usize * field.nx + c.round() as usize))
                .copied()
                .map(f64::from)
                .unwrap_or(0.0)
        };
        let elev = ob
            .elevation_m
            .map(f64::from)
            .unwrap_or_else(|| terrain_at(col, row));
        accepted.push(AnalysisOb {
            col,
            row,
            d,
            eps2,
            elev,
        });
    }
    if accepted.is_empty() {
        return None;
    }

    // ---- station-space structures ----
    let n = accepted.len();
    // ρ'_ij with compact support (dense-in-cutoff sparse rows).
    let mut rho_rows: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut row_sum = vec![0.0f64; n];
    // Decimated terrain for the intervening-terrain term: max-pool 8x —
    // ITT only cares about km-scale barriers (Myrick et al. 2005).
    let terrain = config
        .terrain_aware
        .then(|| {
            orography.map(|oro| {
                let factor = 8usize;
                let tnx = field.nx.div_ceil(factor);
                let tny = field.ny.div_ceil(factor);
                let mut pooled = vec![f32::MIN; tnx * tny];
                for r in 0..field.ny {
                    for c in 0..field.nx {
                        let v = oro[r * field.nx + c];
                        let cell = (r / factor) * tnx + c / factor;
                        if v > pooled[cell] {
                            pooled[cell] = v;
                        }
                    }
                }
                (pooled, tnx, factor)
            })
        })
        .flatten();
    let ridge_between = |a: &AnalysisOb, b: &AnalysisOb| -> f64 {
        let Some((pooled, tnx, factor)) = &terrain else {
            return f64::MIN;
        };
        let steps = ((a.col - b.col).abs().max((a.row - b.row).abs()) / *factor as f64)
            .ceil()
            .max(1.0) as usize;
        let mut z_max = f64::MIN;
        for k in 0..=steps {
            let t = k as f64 / steps as f64;
            let c = (a.col + (b.col - a.col) * t) / *factor as f64;
            let r = (a.row + (b.row - a.row) * t) / *factor as f64;
            if let Some(z) = pooled.get(r as usize * tnx + c as usize) {
                z_max = z_max.max(f64::from(*z));
            }
        }
        z_max
    };
    for i in 0..n {
        for j in 0..n {
            let dx = accepted[i].col - accepted[j].col;
            let dy = accepted[i].row - accepted[j].row;
            let r2 = dx * dx + dy * dy;
            if r2 > cutoff_cells * cutoff_cells {
                continue;
            }
            let mut rho = (-r2 / (r_cells * r_cells)).exp();
            if config.terrain_aware {
                // Vertical decorrelation (Lazarus Eq. 10 family).
                let dz = accepted[i].elev - accepted[j].elev;
                rho *= (-(dz * dz) / (RZ_M * RZ_M)).exp();
                // Intervening-terrain blockage (Myrick Eqs. 3-4):
                // symmetric, so C stays symmetric.
                if i != j {
                    let blockage = (ridge_between(&accepted[i], &accepted[j])
                        - accepted[i].elev.max(accepted[j].elev))
                    .max(0.0);
                    if blockage > 0.0 {
                        rho *= (-(blockage * blockage) / (RB_M * RB_M)).exp();
                    }
                }
            }
            rho_rows[i].push((j, rho));
            row_sum[i] += rho;
        }
    }
    let m: Vec<f64> = (0..n).map(|i| accepted[i].eps2 + row_sum[i]).collect();

    // ---- Bratseth iteration in station space (Eq. 8 accumulation) ----
    let d: Vec<f64> = accepted.iter().map(|ob| ob.d).collect();
    let mut f = vec![0.0f64; n]; // station estimates
    let mut s = vec![0.0f64; n]; // accumulated residuals (grid weights apply at the end)
    let rms_before = (d.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
    let mut iterations = 0;
    for _ in 0..MAX_ITER {
        iterations += 1;
        let residual: Vec<f64> = (0..n).map(|i| d[i] - f[i]).collect();
        let mut max_residual = 0.0f64;
        for i in 0..n {
            s[i] += residual[i];
            max_residual = max_residual.max(residual[i].abs());
        }
        // f_j += Σ_i a_ji · residual_i  with a_ji = (ρ_ji + ε²_i δ_ji)/m_i
        let mut delta_f = vec![0.0f64; n];
        for (i, row) in rho_rows.iter().enumerate() {
            let scaled = residual[i] / m[i];
            for &(j, rho) in row {
                delta_f[j] += rho * scaled;
            }
            delta_f[i] += accepted[i].eps2 * scaled;
        }
        for j in 0..n {
            f[j] += delta_f[j];
        }
        if max_residual < TOL_FRAC * config.sigma_b {
            break;
        }
    }
    let rms_after = ((0..n).map(|i| (d[i] - f[i]) * (d[i] - f[i])).sum::<f64>() / n as f64).sqrt();

    // ---- single gridding scatter: δ_x = Σ_i (ρ_xi / m_i) · s_i ----
    let (nx, ny) = (field.nx, field.ny);
    let mut increment = vec![0.0f32; nx * ny];
    let cut = cutoff_cells;
    for (i, ob) in accepted.iter().enumerate() {
        let weight_base = s[i] / m[i];
        if weight_base == 0.0 {
            continue;
        }
        let c_lo = ((ob.col - cut).floor().max(0.0)) as usize;
        let c_hi = ((ob.col + cut).ceil() as usize).min(nx - 1);
        let r_lo = ((ob.row - cut).floor().max(0.0)) as usize;
        let r_hi = ((ob.row + cut).ceil() as usize).min(ny - 1);
        for r in r_lo..=r_hi {
            let dy = r as f64 - ob.row;
            for c in c_lo..=c_hi {
                let dx = c as f64 - ob.col;
                let r2 = dx * dx + dy * dy;
                if r2 > cut * cut {
                    continue;
                }
                let mut rho = (-r2 / (r_cells * r_cells)).exp();
                if config.terrain_aware
                    && let Some(oro) = orography
                {
                    let dz = ob.elev - f64::from(oro[r * nx + c]);
                    rho *= (-(dz * dz) / (RZ_M * RZ_M)).exp();
                }
                increment[r * nx + c] += (rho * weight_base) as f32;
            }
        }
    }

    Some(Analysis {
        increment,
        obs_used: n,
        obs_rejected: rejected,
        iterations,
        rms_before,
        rms_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rw_ui::{FieldData, FieldKey, HourKey};

    fn flat_field(nx: usize, ny: usize, value: f32) -> Arc<FieldData> {
        Arc::new(FieldData {
            key: FieldKey {
                hour: HourKey {
                    model: "hrrr".into(),
                    run: "20990101_00z".into(),
                    hour: 0,
                },
                var: "temperature_2m".into(),
            },
            units: "K".into(),
            nx,
            ny,
            values: vec![value; nx * ny],
            range: Some((value, value)),
            grid: None,
            lat_descending: false,
            style: None,
        })
    }

    fn ob_at(lat: f32, lon: f32, temp_c: f32, network: &str) -> SurfaceOb {
        SurfaceOb {
            elevation_m: None,
            network: network.into(),
            station_id: "TEST".into(),
            time_utc: None,
            lat,
            lon,
            temp_c: Some(temp_c),
            dewpoint_c: None,
            wind_dir_deg: None,
            wind_speed_kt: None,
            wind_gust_kt: None,
            altim_in_hg: None,
            completeness: 1,
        }
    }

    /// Spec validation item 1: a single ob makes a Gaussian blob of
    /// amplitude d/(1+ε²) at the ob, e-folding R, EXACTLY zero far away.
    #[test]
    fn single_ob_blob_matches_oi_amplitude() {
        let field = flat_field(200, 200, 273.15); // 0 °C everywhere
        // One METAR reading +2 °C at grid center (locate maps it there).
        let obs = vec![ob_at(40.0, -95.0, 2.0, "METAR")];
        let analysis = analyze("temperature_2m", &field, &obs, 3.0, None, |_| {
            Some((100.0, 100.0))
        })
        .expect("analysis");
        assert_eq!(analysis.obs_used, 1);
        let d = 2.0f64;
        let expected = d / (1.0 + T2M.eps2_metar);
        let at_ob = analysis.increment[100 * 200 + 100] as f64;
        assert!(
            (at_ob - expected).abs() < 0.02,
            "amplitude {at_ob} vs OI {expected}"
        );
        // e-folding at R (80 km = 26.7 cells): ratio ≈ e^-1.
        let at_r = analysis.increment[100 * 200 + 100 + 27] as f64;
        assert!(
            (at_r / at_ob - (-1.0f64).exp()).abs() < 0.05,
            "e-folding ratio {}",
            at_r / at_ob
        );
        // Beyond the 3.5R cutoff: exactly zero.
        let far = analysis.increment[100 * 200 + 100 + 95] as f64;
        assert_eq!(far, 0.0, "must relax exactly to background");
    }

    /// Cluster de-weighting: two co-located obs must NOT double the pull
    /// (m_i grows with density — the OI behavior Barnes lacks).
    #[test]
    fn clustered_obs_deweight() {
        let field = flat_field(200, 200, 273.15);
        let obs = vec![
            ob_at(40.0, -95.0, 2.0, "METAR"),
            ob_at(40.0, -95.0, 2.0, "METAR"),
        ];
        let analysis = analyze("temperature_2m", &field, &obs, 3.0, None, |_| {
            Some((100.0, 100.0))
        })
        .expect("analysis");
        let single_expected = 2.0 / (1.0 + T2M.eps2_metar);
        let at_ob = analysis.increment[100 * 200 + 100] as f64;
        // Two identical obs: amplitude rises only via the reduced effective
        // error (d/(1+ε²/2)), never toward 2·d.
        let two_ob_oi = 2.0 / (1.0 + T2M.eps2_metar / 2.0);
        assert!(
            at_ob > single_expected - 0.02 && at_ob < two_ob_oi + 0.05,
            "{at_ob} outside [{single_expected}, {two_ob_oi}]"
        );
    }

    /// QC: an absurd innovation is rejected by the gross check.
    #[test]
    fn gross_check_rejects() {
        let field = flat_field(200, 200, 273.15);
        let obs = vec![ob_at(40.0, -95.0, 40.0, "METAR")]; // +40 °C vs flat 0 °C bg
        let analysis = analyze("temperature_2m", &field, &obs, 3.0, None, |_| {
            Some((100.0, 100.0))
        });
        assert!(analysis.is_none(), "lone absurd ob -> no analysis");
    }
}
