//! SPC-style DERIVED mesoanalysis products: analyze the surface first,
//! THEN recompute severe parameters from the corrected surface lifted
//! through the model profiles aloft — the ordering the SPC Mesoscale
//! Analysis uses (Bothwell, Hart & Thompson 2002; verified in
//! docs/mesoanalysis-spec.md: "analyze, then derive").
//!
//! v1 product: surface-based CAPE. The Bratseth-corrected T2m/Td2m
//! replace the model's surface level; sharprs' SHARPpy-faithful parcel
//! math (define_parcel + parcelx) lifts through temperature_iso /
//! dewpoint_iso / height_iso. Computed on a strided grid (~12 km — the
//! RAP-class resolution SPC products live at) and block-upsampled back
//! to the model grid so it rides the normal layer machinery.

use sharprs::params::cape;

pub struct OaCapeInputs {
    pub nx: usize,
    pub ny: usize,
    /// Surface pressure, Pa.
    pub psfc: Vec<f32>,
    /// Terrain height, m.
    pub orography: Vec<f32>,
    /// OA-corrected 2-m temperature / dewpoint, store units.
    pub t2m: Vec<f32>,
    pub td2m: Vec<f32>,
    /// Kelvin if true (store-native), already °C otherwise.
    pub kelvin: bool,
    /// Isobaric profiles, [level][y][x] slabs, levels descending in hPa.
    pub t_iso: Vec<f32>,
    pub td_iso: Vec<f32>,
    pub h_iso: Vec<f32>,
    pub levels_hpa: Vec<u16>,
}

/// The surface-driven thermodynamic suite — every product the surface
/// analysis can credibly correct, from ONE profile build per cell.
/// (Kinematic fields — SRH, shear — come from model winds aloft; the
/// surface analysis cannot correct them, so they are not offered here.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OaProduct {
    Sbcape,
    Sbcin,
    Mlcape,
    LclHeightM,
    ThetaE,
}

impl OaProduct {
    pub const ALL: [OaProduct; 5] = [
        OaProduct::Sbcape,
        OaProduct::Sbcin,
        OaProduct::Mlcape,
        OaProduct::LclHeightM,
        OaProduct::ThetaE,
    ];
    pub fn label(self) -> &'static str {
        match self {
            OaProduct::Sbcape => "SBCAPE",
            OaProduct::Sbcin => "SBCIN",
            OaProduct::Mlcape => "MLCAPE",
            OaProduct::LclHeightM => "LCL height",
            OaProduct::ThetaE => "Sfc theta-e",
        }
    }
    pub fn units(self) -> &'static str {
        match self {
            OaProduct::Sbcape | OaProduct::Sbcin | OaProduct::Mlcape => "J/kg",
            OaProduct::LclHeightM => "m",
            OaProduct::ThetaE => "K",
        }
    }
    /// Display range fallback when no operational style resolves.
    pub fn fallback_range(self, max_seen: f32) -> (f32, f32) {
        match self {
            OaProduct::Sbcape | OaProduct::Mlcape => (0.0, max_seen.clamp(1000.0, 6000.0)),
            OaProduct::Sbcin => (-400.0, 0.0),
            OaProduct::LclHeightM => (0.0, 4000.0),
            OaProduct::ThetaE => (290.0, 370.0),
        }
    }
    /// Style slug for operational_style_for_store_variable, when one
    /// exists in the render stack.
    pub fn style_slug(self) -> Option<&'static str> {
        match self {
            OaProduct::Sbcape | OaProduct::Mlcape => Some("cape"),
            _ => None,
        }
    }
}

/// Surface potential temperature-equivalent (theta-e), Bolton 1980 — the
/// standard operational approximation.
fn theta_e_k(p_hpa: f64, t_c: f64, td_c: f64) -> f64 {
    let t_k = t_c + 273.15;
    // Vapor pressure (Bolton Eq. 10) and mixing ratio.
    let e = 6.112 * (17.67 * td_c / (td_c + 243.5)).exp();
    let r = 0.622 * e / (p_hpa - e).max(1.0);
    // LCL temperature (Bolton Eq. 15).
    let t_lcl = 2840.0 / (3.5 * t_k.ln() - e.max(1e-3).ln() - 4.805) + 55.0;
    // Theta-e (Bolton Eq. 39).
    let theta_dl = t_k * (1000.0 / (p_hpa - e)).powf(0.2854) * (t_k / t_lcl).powf(0.28 * r);
    theta_dl * ((3036.0 / t_lcl - 1.78) * r * (1.0 + 0.448 * r)).exp()
}

/// Compute the chosen product on every `stride`-th cell, block-filled to
/// the full grid. NaN where inputs are missing.
pub fn product_grid(inputs: &OaCapeInputs, product: OaProduct, stride: usize) -> Vec<f32> {
    grid_impl(inputs, product, stride)
}

/// Back-compat: SBCAPE.
pub fn sbcape_grid(inputs: &OaCapeInputs, stride: usize) -> Vec<f32> {
    grid_impl(inputs, OaProduct::Sbcape, stride)
}

fn grid_impl(inputs: &OaCapeInputs, product: OaProduct, stride: usize) -> Vec<f32> {
    let (nx, ny) = (inputs.nx, inputs.ny);
    let stride = stride.max(1);
    let plane = nx * ny;
    let to_c = |v: f32| -> f64 {
        if inputs.kelvin {
            f64::from(v) - 273.15
        } else {
            f64::from(v)
        }
    };
    let mut out = vec![f32::NAN; plane];
    let cells: Vec<(usize, usize)> = (0..ny)
        .step_by(stride)
        .flat_map(|y| (0..nx).step_by(stride).map(move |x| (y, x)))
        .collect();
    use rayon::prelude::*;
    let computed: Vec<((usize, usize), f32)> = cells
        .par_iter()
        .map(|&(y, x)| {
            let i = y * nx + x;
            let cape_val = (|| -> Option<f32> {
                let psfc_hpa = f64::from(*inputs.psfc.get(i)?) / 100.0;
                let t_sfc = to_c(*inputs.t2m.get(i)?);
                let mut td_sfc = to_c(*inputs.td2m.get(i)?);
                if !psfc_hpa.is_finite() || !t_sfc.is_finite() || !td_sfc.is_finite() {
                    return None;
                }
                td_sfc = td_sfc.min(t_sfc);
                let z_sfc = f64::from(*inputs.orography.get(i).unwrap_or(&0.0));
                let mut pres = vec![psfc_hpa];
                let mut hght = vec![z_sfc];
                let mut tmpc = vec![t_sfc];
                let mut dwpc = vec![td_sfc];
                for (li, &level) in inputs.levels_hpa.iter().enumerate() {
                    let p = f64::from(level);
                    // Above-ground levels only (descending pressure order).
                    if p >= psfc_hpa - 1.0 {
                        continue;
                    }
                    let idx = li * plane + i;
                    let (t, td, h) = (
                        *inputs.t_iso.get(idx)?,
                        *inputs.td_iso.get(idx)?,
                        *inputs.h_iso.get(idx)?,
                    );
                    if !t.is_finite() || !td.is_finite() || !h.is_finite() {
                        continue;
                    }
                    pres.push(p);
                    hght.push(f64::from(h));
                    tmpc.push(to_c(t));
                    dwpc.push(to_c(td).min(to_c(t)));
                }
                if product == OaProduct::ThetaE {
                    let v = theta_e_k(psfc_hpa, t_sfc, td_sfc);
                    return v.is_finite().then_some(v as f32);
                }
                if pres.len() < 8 {
                    return None;
                }
                let prof = cape::Profile::new(pres, hght, tmpc, dwpc, 0);
                let ptype = match product {
                    OaProduct::Mlcape => cape::ParcelType::MixedLayer { depth_hpa: 100.0 },
                    _ => cape::ParcelType::Surface,
                };
                let lpl = cape::define_parcel(&prof, ptype);
                let pcl = cape::parcelx(&prof, &lpl, None, None);
                let v = match product {
                    OaProduct::Sbcape | OaProduct::Mlcape => pcl.bplus,
                    OaProduct::Sbcin => pcl.bminus,
                    OaProduct::LclHeightM => pcl.lclhght,
                    OaProduct::ThetaE => unreachable!(),
                };
                let ok = v.is_finite()
                    && match product {
                        OaProduct::Sbcape | OaProduct::Mlcape => v >= 0.0,
                        _ => true,
                    };
                ok.then_some(v as f32)
            })();
            ((y, x), cape_val.unwrap_or(f32::NAN))
        })
        .collect();
    // Block-fill each strided sample over its stride x stride block.
    for ((y, x), value) in computed {
        for dy in 0..stride {
            for dx in 0..stride {
                let (yy, xx) = (y + dy, x + dx);
                if yy < ny && xx < nx {
                    out[yy * nx + xx] = value;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A warm, moist surface under a cold aloft profile must yield CAPE;
    /// a bone-dry surface must yield ~0.
    #[test]
    fn cape_responds_to_surface_moisture() {
        let (nx, ny) = (4, 4);
        let plane = nx * ny;
        let levels: Vec<u16> = vec![900, 800, 700, 600, 500, 400, 300, 250, 200];
        let nl = levels.len();
        // Standard-ish atmosphere aloft: T drops 6.5 K/km, z from pressure.
        let mut t_iso = vec![0.0f32; nl * plane];
        let mut td_iso = vec![0.0f32; nl * plane];
        let mut h_iso = vec![0.0f32; nl * plane];
        for (li, &p) in levels.iter().enumerate() {
            let z = 44_330.0 * (1.0 - (f64::from(p) / 1013.25).powf(0.1903));
            let t_k = 288.15 - 0.0065 * z;
            for i in 0..plane {
                t_iso[li * plane + i] = t_k as f32;
                td_iso[li * plane + i] = (t_k - 25.0) as f32; // dry aloft
                h_iso[li * plane + i] = z as f32;
            }
        }
        let base = OaCapeInputs {
            nx,
            ny,
            psfc: vec![100_000.0; plane],
            orography: vec![100.0; plane],
            t2m: vec![303.15; plane],  // 30 °C
            td2m: vec![297.15; plane], // 24 °C — juicy
            kelvin: true,
            t_iso,
            td_iso,
            h_iso,
            levels_hpa: levels,
        };
        let juicy = sbcape_grid(&base, 1);
        assert!(
            juicy[0] > 500.0,
            "moist surface must have CAPE, got {}",
            juicy[0]
        );
        let mut dry = base;
        dry.td2m = vec![263.15; 16]; // -10 °C dewpoint
        let none = sbcape_grid(&dry, 1);
        assert!(
            none[0] < 100.0,
            "dry surface must be near-zero, got {}",
            none[0]
        );
    }
}
