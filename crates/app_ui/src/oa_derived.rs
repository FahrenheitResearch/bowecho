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
    /// Winds (m/s) — empty when the caller only needs thermo products.
    pub u10: Vec<f32>,
    pub v10: Vec<f32>,
    pub u_iso: Vec<f32>,
    pub v_iso: Vec<f32>,
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
    /// exists in the render stack (rustwx-products `DerivedRecipe` slugs).
    pub fn style_slug(self) -> Option<&'static str> {
        match self {
            OaProduct::Sbcape => Some("sbcape"),
            OaProduct::Mlcape => Some("mlcape"),
            OaProduct::Sbcin => Some("sbcin"),
            OaProduct::LclHeightM => Some("sblcl"),
            OaProduct::ThetaE => Some("theta_e_2m_10m_winds"),
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

/// One computed composite field, ready to become a map layer. Values are
/// kept on the strided analysis lattice (`sx` x `sy`) — ~36x smaller than
/// the model grid at stride 6 — and block-expanded on demand.
pub struct CompositeField {
    pub name: &'static str,
    pub units: &'static str,
    /// SPC Mesoscale Analysis page section this product lives under
    /// (https://www.spc.noaa.gov/exper/mesoanalysis/) — drives the picker
    /// menu grouping.
    pub group: &'static str,
    /// rustwx-products `DerivedRecipe` slug whose production color scale
    /// this field borrows, when one exists.
    pub style_slug: Option<&'static str>,
    /// Strided-lattice values, row-major `sy` rows of `sx`.
    pub values: Vec<f32>,
    pub range: (f32, f32),
    nx: usize,
    ny: usize,
    stride: usize,
}

impl CompositeField {
    /// Block-fill the strided lattice back to the full model grid (each
    /// lattice sample covers its stride x stride block), NaN elsewhere.
    pub fn expand_full(&self) -> Vec<f32> {
        let sx = self.nx.div_ceil(self.stride);
        let mut out = vec![f32::NAN; self.nx * self.ny];
        for y in 0..self.ny {
            let row = (y / self.stride) * sx;
            for x in 0..self.nx {
                out[y * self.nx + x] = self.values[row + x / self.stride];
            }
        }
        out
    }
}

/// SPC mesoanalysis menu sections, in page order. The picker menu groups
/// fields by these.
pub const GROUPS: [&str; 9] = [
    "Surface",
    "Thermodynamics",
    "Wind Shear",
    "Composite Indices",
    "Heavy Rain",
    "Winter",
    "Fire Weather",
    "Classic",
    "Beta",
];

/// One product in the cached suite: identity, display metadata, and the
/// SPC section it belongs to. The values are extracted by `spec_value`
/// (match on `name` — exercised exhaustively by the suite test).
struct CompositeSpec {
    name: &'static str,
    units: &'static str,
    group: &'static str,
    slug: Option<&'static str>,
    range: (f32, f32),
}

const fn spec(
    name: &'static str,
    units: &'static str,
    group: &'static str,
    slug: Option<&'static str>,
    range: (f32, f32),
) -> CompositeSpec {
    CompositeSpec {
        name,
        units,
        group,
        slug,
        range,
    }
}

/// The full cached suite. The first 14 entries preserve the legacy order
/// (index 0 = SCP stays the auto-shown field); everything after is the
/// phase-2 catalog expansion (docs/spc-catalog.md). Display ranges follow
/// the SPC mesoanalysis chart ranges.
const SPECS: &[CompositeSpec] = &[
    // -- legacy 14 (order preserved) --
    spec(
        "SCP",
        "",
        "Composite Indices",
        Some("scp_mu_0_3km_0_6km_proxy"),
        (0.0, 20.0),
    ),
    spec(
        "STP (CIN)",
        "",
        "Composite Indices",
        Some("stp_fixed"),
        (0.0, 8.0),
    ),
    spec(
        "STP (fixed)",
        "",
        "Composite Indices",
        Some("stp_fixed"),
        (0.0, 8.0),
    ),
    spec("SHIP", "", "Composite Indices", None, (0.0, 4.0)),
    spec(
        "EHI 0-1km",
        "",
        "Composite Indices",
        Some("ehi_0_1km"),
        (0.0, 8.0),
    ),
    spec(
        "EHI 0-3km",
        "",
        "Composite Indices",
        Some("ehi_0_3km"),
        (0.0, 8.0),
    ),
    spec(
        "Eff SRH",
        "m²/s²",
        "Wind Shear",
        Some("srh_0_3km"),
        (0.0, 600.0),
    ),
    spec(
        "Eff bulk shear",
        "kt",
        "Wind Shear",
        Some("bulk_shear_0_6km"),
        (0.0, 80.0),
    ),
    spec(
        "SRH 0-1km",
        "m²/s²",
        "Wind Shear",
        Some("srh_0_1km"),
        (0.0, 500.0),
    ),
    spec(
        "SRH 0-3km",
        "m²/s²",
        "Wind Shear",
        Some("srh_0_3km"),
        (0.0, 700.0),
    ),
    spec(
        "MUCAPE",
        "J/kg",
        "Thermodynamics",
        Some("mucape"),
        (0.0, 6000.0),
    ),
    spec("PWAT", "in", "Heavy Rain", None, (0.0, 2.5)),
    spec("K-index", "", "Classic", None, (15.0, 45.0)),
    spec("Freezing lvl", "m", "Winter", None, (0.0, 6000.0)),
    // -- Surface --
    spec(
        "Sfc theta-e",
        "K",
        "Surface",
        Some("theta_e_2m_10m_winds"),
        (290.0, 370.0),
    ),
    // -- Thermodynamics --
    spec(
        "SBCAPE",
        "J/kg",
        "Thermodynamics",
        Some("sbcape"),
        (0.0, 6000.0),
    ),
    spec(
        "SBCIN",
        "J/kg",
        "Thermodynamics",
        Some("sbcin"),
        (-400.0, 0.0),
    ),
    spec(
        "MLCAPE",
        "J/kg",
        "Thermodynamics",
        Some("mlcape"),
        (0.0, 6000.0),
    ),
    spec(
        "MLCIN",
        "J/kg",
        "Thermodynamics",
        Some("mlcin"),
        (-400.0, 0.0),
    ),
    spec(
        "MUCIN",
        "J/kg",
        "Thermodynamics",
        Some("mucin"),
        (-400.0, 0.0),
    ),
    spec("MLCAPE 0-3km", "J/kg", "Thermodynamics", None, (0.0, 300.0)),
    spec(
        "DCAPE",
        "J/kg",
        "Thermodynamics",
        Some("dcape"),
        (0.0, 2000.0),
    ),
    spec("NCAPE", "m/s²", "Thermodynamics", None, (0.0, 0.5)),
    spec(
        "SB lifted index",
        "°C",
        "Thermodynamics",
        Some("lifted_index"),
        (-12.0, 8.0),
    ),
    spec(
        "Lapse rate 0-3km",
        "°C/km",
        "Thermodynamics",
        Some("lapse_rate_0_3km"),
        (3.0, 11.0),
    ),
    spec(
        "Lapse rate 3-6km",
        "°C/km",
        "Thermodynamics",
        None,
        (3.0, 10.0),
    ),
    spec(
        "Lapse rate 700-500",
        "°C/km",
        "Thermodynamics",
        Some("lapse_rate_700_500"),
        (4.5, 9.5),
    ),
    spec(
        "Lapse rate 850-500",
        "°C/km",
        "Thermodynamics",
        None,
        (4.5, 9.5),
    ),
    spec(
        "SB LCL hgt",
        "m",
        "Thermodynamics",
        Some("sblcl"),
        (0.0, 4000.0),
    ),
    spec("ML LCL hgt", "m", "Thermodynamics", None, (0.0, 4000.0)),
    spec("ML LFC hgt", "m", "Thermodynamics", None, (0.0, 6000.0)),
    spec("MU EL hgt", "m", "Thermodynamics", None, (0.0, 16000.0)),
    spec("LCL-LFC mean RH", "%", "Thermodynamics", None, (0.0, 100.0)),
    // -- Wind Shear --
    spec(
        "Shear 0-1km",
        "kt",
        "Wind Shear",
        Some("bulk_shear_0_1km"),
        (0.0, 50.0),
    ),
    spec("Shear 0-3km", "kt", "Wind Shear", None, (0.0, 60.0)),
    spec(
        "Shear 0-6km",
        "kt",
        "Wind Shear",
        Some("bulk_shear_0_6km"),
        (0.0, 80.0),
    ),
    spec("Shear 0-8km", "kt", "Wind Shear", None, (0.0, 100.0)),
    spec("BRN shear", "m²/s²", "Wind Shear", None, (0.0, 120.0)),
    spec(
        "SRH 0-500m",
        "m²/s²",
        "Wind Shear",
        Some("srh_0_1km"),
        (0.0, 300.0),
    ),
    spec("Eff inflow base", "m", "Wind Shear", None, (0.0, 3000.0)),
    spec(
        "Eff inflow shear",
        "kt",
        "Wind Shear",
        Some("bulk_shear_0_6km"),
        (0.0, 80.0),
    ),
    spec("Mean wind 0-6km", "kt", "Wind Shear", None, (0.0, 80.0)),
    spec("Bunkers RM speed", "kt", "Wind Shear", None, (0.0, 60.0)),
    spec("Bunkers LM speed", "kt", "Wind Shear", None, (0.0, 60.0)),
    // -- Composite Indices --
    spec("VTP", "", "Composite Indices", None, (0.0, 8.0)),
    spec(
        "Derecho composite",
        "",
        "Composite Indices",
        None,
        (0.0, 4.0),
    ),
    spec(
        "Craven-Brooks SigSvr",
        "m³/s³",
        "Composite Indices",
        None,
        (0.0, 60000.0),
    ),
    spec("BRN", "", "Composite Indices", None, (0.0, 100.0)),
    spec("MCS maintenance", "", "Composite Indices", None, (0.0, 1.0)),
    spec(
        "Enhanced stretching",
        "",
        "Composite Indices",
        None,
        (0.0, 5.0),
    ),
    spec("WNDG", "", "Composite Indices", None, (0.0, 3.0)),
    spec(
        "Critical angle",
        "°",
        "Composite Indices",
        None,
        (0.0, 180.0),
    ),
    // -- Heavy Rain --
    spec("Mean mixing ratio", "g/kg", "Heavy Rain", None, (0.0, 20.0)),
    spec("Mean RH (low)", "%", "Heavy Rain", None, (0.0, 100.0)),
    spec("Mean RH (mid)", "%", "Heavy Rain", None, (0.0, 100.0)),
    spec("Theta-e index (TEI)", "K", "Heavy Rain", None, (0.0, 40.0)),
    spec("Corfidi upshear", "kt", "Heavy Rain", None, (0.0, 60.0)),
    // -- Winter --
    spec("Wet-bulb zero hgt", "m", "Winter", None, (0.0, 5000.0)),
    // -- Fire Weather --
    spec("Fosberg index", "", "Fire Weather", None, (0.0, 100.0)),
    // -- Classic --
    spec("Total Totals", "", "Classic", None, (30.0, 60.0)),
    // -- Beta --
    spec("SHERBS3", "", "Beta", None, (0.0, 3.0)),
    spec("SHERBE", "", "Beta", None, (0.0, 3.0)),
    spec("MOSHE", "", "Beta", None, (0.0, 3.0)),
    spec("Tornadic EHI 0-1km", "", "Beta", None, (0.0, 5.0)),
    spec("Tornadic tilt/stretch", "", "Beta", None, (0.0, 5.0)),
];

const KT_TO_MS: f64 = 0.514_444;

/// Per-cell quantities NOT in `ComputedParams`: the cheap category-(c)
/// derivations from docs/spc-catalog.md, computed from the same profile
/// while it is in hand. Citations on each computation site.
struct CellExtras {
    theta_e_sfc: f64,
    srh_500m: f64,
    eff_base_agl: f64,
    brn_shear: f64,
    brn: f64,
    ncape: f64,
    lcl_lfc_rh: f64,
    mean_wind_06_kt: f64,
    dcp: f64,
    sig_severe: f64,
    sherbs3: f64,
    sherbe: f64,
    moshe: f64,
    esp: f64,
    wndg: f64,
    mmp: f64,
    fosberg: f64,
}

fn mag(u: f64, v: f64) -> f64 {
    (u * u + v * v).sqrt()
}

#[allow(clippy::too_many_lines)]
fn cell_extras(
    profile: &sharprs::profile::Profile,
    p: &sharprs::render::compositor::ComputedParams,
    psfc_hpa: f64,
    t_sfc: f64,
    td_sfc: f64,
    sfc_wspd_kt: f64,
) -> CellExtras {
    use sharprs::params::{composites, indices};
    use sharprs::winds;
    let nanf = f64::NAN;
    let opt = |o: Option<f64>| o.unwrap_or(nanf);
    let p_sfc = profile.sfc_pressure();
    let p_at = |agl_m: f64| profile.pres_at_height(profile.to_msl(agl_m));

    // Sfc-500 m SRH with the Bunkers right-mover motion — the near-ground
    // SRH layer of Coffer et al. 2019 (WAF 34, 1417-1435).
    let srh_500m = winds::helicity(profile, 0.0, 500.0, p.rstu, p.rstv, -1.0, false)
        .map(|h| h.0)
        .unwrap_or(nanf);

    // Effective inflow base height AGL (Thompson, Mead & Edwards 2007,
    // WAF 22, 102-115): the stored bound is a pressure.
    let eff_base_agl = if p.eff_inflow.0.is_finite() {
        profile.to_agl(profile.interp_hght(p.eff_inflow.0))
    } else {
        nanf
    };

    // BRN shear = 1/2 |V(0-6 km mean) - V(0-500 m mean)|^2 in m^2/s^2,
    // and BRN = MLCAPE / BRN shear (Weisman & Klemp 1982, MWR 110,
    // 504-520; matches SHARPpy params.bulk_rich layer choices).
    let p6km = p_at(6000.0);
    let p500m = p_at(500.0);
    let mw = |pbot: f64, ptop: f64| -> Option<(f64, f64)> {
        (pbot.is_finite() && ptop.is_finite())
            .then(|| winds::mean_wind(profile, pbot, ptop, -1.0, 0.0, 0.0).ok())
            .flatten()
    };
    let brn_shear = match (mw(p_sfc, p6km), mw(p_sfc, p500m)) {
        (Some((u6, v6)), Some((u5, v5))) => {
            let dv = mag(u6 - u5, v6 - v5) * KT_TO_MS;
            0.5 * dv * dv
        }
        _ => nanf,
    };
    let brn = if brn_shear.is_finite() && brn_shear > 1e-6 && p.mlpcl.bplus.is_finite() {
        p.mlpcl.bplus / brn_shear
    } else {
        nanf
    };

    // Normalized CAPE: MUCAPE over the LFC->EL depth (Blanchard 1998,
    // WAF 13, 870-877). Units m/s^2.
    let depth = p.mupcl.elhght - p.mupcl.lfchght;
    let ncape = if p.mupcl.bplus.is_finite() && depth.is_finite() && depth > 1.0 {
        p.mupcl.bplus / depth
    } else {
        nanf
    };

    // LCL-LFC mean RH of the 100-mb mixed-layer parcel (the SPC
    // "LCL-LFC Mean RH" product).
    let lcl_lfc_rh = if p.mlpcl.lclpres.is_finite() && p.mlpcl.lfcpres.is_finite() {
        opt(indices::mean_relh(
            profile,
            Some(p.mlpcl.lclpres),
            Some(p.mlpcl.lfcpres),
        ))
    } else {
        nanf
    };

    let shr01_ms = mag(p.shr01.0, p.shr01.1) * KT_TO_MS;
    let shr03_ms = mag(p.shr03.0, p.shr03.1) * KT_TO_MS;
    let shr06_kt = mag(p.shr06.0, p.shr06.1);
    let mean_wind_06_kt = mag(p.mean_wind_06.0, p.mean_wind_06.1);

    // Derecho Composite Parameter (Evans & Doswell 2001, WAF 16,
    // 329-342): shear + mean wind terms in knots per SPC help_dcp.html.
    let dcp = opt(composites::dcp(
        p.dcape.dcape,
        p.mupcl.bplus,
        shr06_kt,
        mean_wind_06_kt,
    ));

    // Craven & Brooks 2004 (Natl. Wea. Digest 28, 13-24) significant
    // severe: MLCAPE x 0-6 km shear (m/s).
    let sig_severe = opt(composites::sig_severe(p.mlpcl.bplus, shr06_kt * KT_TO_MS));

    // SHERB family for high-shear/low-CAPE environments (Sherburn &
    // Parker 2014, WAF 29, 854-877): fixed 0-3 km variant, effective
    // variant, and the modified-SHERBE (MOSHE) low-level-shear extension.
    let lr03 = opt(p.lr03);
    let lr75 = opt(p.lr75);
    let sherbs3 = opt(composites::sherb(shr03_ms, lr03, lr75, false));
    let sherbe = if p.effective_bwd.is_some() {
        opt(composites::sherb(opt(p.effective_bwd), lr03, lr75, true))
    } else {
        nanf
    };
    let moshe = if p.effective_bwd.is_some() {
        opt(composites::moshe(
            opt(p.effective_bwd),
            lr03,
            lr75,
            shr01_ms,
        ))
    } else {
        nanf
    };

    // Enhanced Stretching Potential (J. Davies; SPC help_esp.html):
    // low-level buoyancy x steep low-level lapse rates.
    let esp = opt(composites::esp(p.mlpcl.b3km, lr03, p.mlpcl.bplus));

    // WNDG (SPC wind-damage parameter, help_wndg.html): needs the
    // 1-3.5 km AGL mean wind in m/s.
    let p1km = p_at(1000.0);
    let p3p5km = p_at(3500.0);
    let wndg = match mw(p1km, p3p5km) {
        Some((u, v)) => opt(composites::wndg(
            p.mlpcl.bplus,
            lr03,
            mag(u, v) * KT_TO_MS,
            p.mlpcl.bminus,
        )),
        None => nanf,
    };

    // MCS Maintenance Probability (Coniglio, Stensrud & Wicker 2006;
    // SHARPpy port): max bulk shear between any 0-1 km and any 6-10 km
    // level, the 3-8 km lapse rate, and the 3-12 km mean wind (m/s).
    let mmp = {
        let mut low: Vec<usize> = Vec::new();
        let mut high: Vec<usize> = Vec::new();
        for i in profile.sfc..profile.pres.len() {
            let agl = profile.to_agl(profile.hght[i]);
            if !agl.is_finite() || !profile.u[i].is_finite() || !profile.v[i].is_finite() {
                continue;
            }
            if agl <= 1000.0 {
                low.push(i);
            } else if (6000.0..=10000.0).contains(&agl) {
                high.push(i);
            }
        }
        let mut max_shear = nanf;
        for &a in &low {
            for &b in &high {
                let s = mag(profile.u[b] - profile.u[a], profile.v[b] - profile.v[a]) * KT_TO_MS;
                if !max_shear.is_finite() || s > max_shear {
                    max_shear = s;
                }
            }
        }
        let lr38 = indices::lapse_rate(profile, 3000.0, 8000.0, false);
        let mw312 = mw(p_at(3000.0), p_at(12000.0));
        match (lr38, mw312) {
            (Some(lr), Some((u, v))) if max_shear.is_finite() => opt(composites::mmp(
                p.mupcl.bplus,
                max_shear,
                lr,
                mag(u, v) * KT_TO_MS,
            )),
            _ => nanf,
        }
    };

    // Fosberg Fire Weather Index from the OA-corrected surface
    // (Fosberg 1978, AMS Conf. Sierra Nevada Meteorology).
    let fosberg = sharprs::fire::fosberg(t_sfc, td_sfc, sfc_wspd_kt);

    CellExtras {
        theta_e_sfc: theta_e_k(psfc_hpa, t_sfc, td_sfc),
        srh_500m,
        eff_base_agl,
        brn_shear,
        brn,
        ncape,
        lcl_lfc_rh,
        mean_wind_06_kt,
        dcp,
        sig_severe,
        sherbs3,
        sherbe,
        moshe,
        esp,
        wndg,
        mmp,
        fosberg,
    }
}

/// Extract one suite value by spec name. Every `SPECS` name has an arm;
/// the suite unit test exercises all of them (a typo panics there, not in
/// production — unknown names yield NaN here).
fn spec_value(name: &str, p: &sharprs::render::compositor::ComputedParams, ex: &CellExtras) -> f32 {
    let f = |o: Option<f64>| o.map(|v| v as f32).unwrap_or(f32::NAN);
    match name {
        // legacy 14
        "SCP" => f(p.scp),
        "STP (CIN)" => f(p.stp_cin),
        "STP (fixed)" => f(p.stp_fixed),
        "SHIP" => f(p.ship),
        "EHI 0-1km" => f(p.ehi01),
        "EHI 0-3km" => f(p.ehi03),
        "Eff SRH" => f(p.effective_srh),
        // Both effective-layer shears are stored in m/s; display kt. The
        // names follow the sounding table's vocabulary, where "Effective
        // Bulk Shear" is the effective bulk wind difference (inflow base ->
        // half the depth to the MU EL) and "Effective Inflow Shear" is the
        // shear across the inflow layer itself.
        "Eff bulk shear" => f(p.effective_bwd.map(|v| v / KT_TO_MS)),
        "Eff inflow shear" => f(p.eff_shear.map(|v| v / KT_TO_MS)),
        "SRH 0-1km" => p.srh01.0 as f32,
        "SRH 0-3km" => p.srh03.0 as f32,
        "MUCAPE" => p.mupcl.bplus as f32,
        "PWAT" => f(p.precip_water),
        "K-index" => f(p.k_index),
        "Freezing lvl" => f(p.frz_lvl),
        // Surface
        "Sfc theta-e" => ex.theta_e_sfc as f32,
        // Thermodynamics
        "SBCAPE" => p.sfcpcl.bplus as f32,
        "SBCIN" => p.sfcpcl.bminus as f32,
        "MLCAPE" => p.mlpcl.bplus as f32,
        "MLCIN" => p.mlpcl.bminus as f32,
        "MUCIN" => p.mupcl.bminus as f32,
        "MLCAPE 0-3km" => p.mlpcl.b3km as f32,
        "DCAPE" => p.dcape.dcape as f32,
        "NCAPE" => ex.ncape as f32,
        "SB lifted index" => p.sfcpcl.li5 as f32,
        "Lapse rate 0-3km" => f(p.lr03),
        "Lapse rate 3-6km" => f(p.lr36),
        "Lapse rate 700-500" => f(p.lr75),
        "Lapse rate 850-500" => f(p.lr85),
        "SB LCL hgt" => p.sfcpcl.lclhght as f32,
        "ML LCL hgt" => p.mlpcl.lclhght as f32,
        "ML LFC hgt" => p.mlpcl.lfchght as f32,
        "MU EL hgt" => p.mupcl.elhght as f32,
        "LCL-LFC mean RH" => ex.lcl_lfc_rh as f32,
        // Wind Shear
        "Shear 0-1km" => mag(p.shr01.0, p.shr01.1) as f32,
        "Shear 0-3km" => mag(p.shr03.0, p.shr03.1) as f32,
        "Shear 0-6km" => mag(p.shr06.0, p.shr06.1) as f32,
        "Shear 0-8km" => mag(p.shr08.0, p.shr08.1) as f32,
        "BRN shear" => ex.brn_shear as f32,
        "SRH 0-500m" => ex.srh_500m as f32,
        "Eff inflow base" => ex.eff_base_agl as f32,
        "Mean wind 0-6km" => ex.mean_wind_06_kt as f32,
        "Bunkers RM speed" => mag(p.rstu, p.rstv) as f32,
        "Bunkers LM speed" => mag(p.lstu, p.lstv) as f32,
        // Composite Indices
        "VTP" => f(p.vtp_mod),
        "Derecho composite" => ex.dcp as f32,
        "Craven-Brooks SigSvr" => ex.sig_severe as f32,
        "BRN" => ex.brn as f32,
        "MCS maintenance" => ex.mmp as f32,
        "Enhanced stretching" => ex.esp as f32,
        "WNDG" => ex.wndg as f32,
        "Critical angle" => p.critical_angle as f32,
        // Heavy Rain
        "Mean mixing ratio" => f(p.mean_mixr),
        "Mean RH (low)" => f(p.mean_rh_low),
        "Mean RH (mid)" => f(p.mean_rh_mid),
        "Theta-e index (TEI)" => f(p.tei),
        "Corfidi upshear" => mag(p.corfidi_up_u, p.corfidi_up_v) as f32,
        // Winter
        "Wet-bulb zero hgt" => f(p.wb_zero),
        // Fire Weather
        "Fosberg index" => ex.fosberg as f32,
        // Classic
        "Total Totals" => f(p.t_totals),
        // Beta
        "SHERBS3" => ex.sherbs3 as f32,
        "SHERBE" => ex.sherbe as f32,
        "MOSHE" => ex.moshe as f32,
        "Tornadic EHI 0-1km" => f(p.tehi),
        "Tornadic tilt/stretch" => f(p.tts),
        _ => f32::NAN,
    }
}

/// THE SPC-mesoanalysis pass: per strided cell, build the full sharprs
/// profile (OA-corrected surface + model column, winds included), run
/// `compute_all_params` — SHARPpy's entire parameter suite in one call —
/// plus the cheap category-(c) extras, then expose every catalog field
/// (docs/spc-catalog.md) on the strided lattice. Compute once, display
/// many.
pub fn composite_pass(
    inputs: &OaCapeInputs,
    stride: usize,
    progress: &std::sync::atomic::AtomicUsize,
) -> Vec<CompositeField> {
    use rayon::prelude::*;
    let (nx, ny) = (inputs.nx, inputs.ny);
    let stride = stride.max(1);
    let plane = nx * ny;
    let (sx, sy) = (nx.div_ceil(stride), ny.div_ceil(stride));
    let to_c = |v: f32| -> f64 {
        if inputs.kelvin {
            f64::from(v) - 273.15
        } else {
            f64::from(v)
        }
    };
    let cells: Vec<(usize, usize)> = (0..ny)
        .step_by(stride)
        .flat_map(|y| (0..nx).step_by(stride).map(move |x| (y, x)))
        .collect();
    type CellResult = ((usize, usize), Option<Vec<f32>>);
    let results: Vec<CellResult> = cells
        .par_iter()
        .map(|&(y, x)| {
            progress.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let i = y * nx + x;
            let vals = (|| -> Option<Vec<f32>> {
                let psfc_hpa = f64::from(*inputs.psfc.get(i)?) / 100.0;
                let t_sfc = to_c(*inputs.t2m.get(i)?);
                let td_sfc = to_c(*inputs.td2m.get(i)?).min(t_sfc);
                if !psfc_hpa.is_finite() || !t_sfc.is_finite() {
                    return None;
                }
                let z_sfc = f64::from(inputs.orography.get(i).copied().unwrap_or(0.0));
                let (u_s, v_s) = (
                    f64::from(*inputs.u10.get(i)?),
                    f64::from(*inputs.v10.get(i)?),
                );
                let uv_to_dirspd = |u: f64, v: f64| -> (f64, f64) {
                    let spd = (u * u + v * v).sqrt() * 1.943_84; // m/s -> kt
                    let dir = (270.0 - v.atan2(u).to_degrees()).rem_euclid(360.0);
                    (dir, spd)
                };
                let (d0, s0) = uv_to_dirspd(u_s, v_s);
                let mut pres = vec![psfc_hpa];
                let mut hght = vec![z_sfc];
                let mut tmpc = vec![t_sfc];
                let mut dwpc = vec![td_sfc];
                let mut wdir = vec![d0];
                let mut wspd = vec![s0];
                for (li, &level) in inputs.levels_hpa.iter().enumerate() {
                    let p = f64::from(level);
                    if p >= psfc_hpa - 1.0 {
                        continue;
                    }
                    let idx = li * plane + i;
                    let (t, td, h, u, v) = (
                        *inputs.t_iso.get(idx)?,
                        *inputs.td_iso.get(idx)?,
                        *inputs.h_iso.get(idx)?,
                        *inputs.u_iso.get(idx)?,
                        *inputs.v_iso.get(idx)?,
                    );
                    if !t.is_finite() || !h.is_finite() {
                        continue;
                    }
                    let (d, s) = uv_to_dirspd(f64::from(u), f64::from(v));
                    pres.push(p);
                    hght.push(f64::from(h));
                    tmpc.push(to_c(t));
                    dwpc.push(to_c(td).min(to_c(t)));
                    wdir.push(d);
                    wspd.push(s);
                }
                if pres.len() < 10 {
                    return None;
                }
                let n = pres.len();
                let station = sharprs::profile::StationInfo {
                    station_id: "OA".to_owned(),
                    latitude: 0.0,
                    longitude: 0.0,
                    elevation: z_sfc,
                    ..Default::default()
                };
                let profile = sharprs::profile::Profile::new(
                    &pres,
                    &hght,
                    &tmpc,
                    &dwpc,
                    &wdir,
                    &wspd,
                    &vec![0.0; n],
                    station,
                )
                .ok()?;
                let p = sharprs::render::compositor::compute_all_params(&profile);
                let ex = cell_extras(&profile, &p, psfc_hpa, t_sfc, td_sfc, s0);
                Some(
                    SPECS
                        .iter()
                        .map(|spec| spec_value(spec.name, &p, &ex))
                        .collect(),
                )
            })();
            ((y, x), vals)
        })
        .collect();
    let mut out: Vec<CompositeField> = SPECS
        .iter()
        .map(|spec| CompositeField {
            name: spec.name,
            units: spec.units,
            group: spec.group,
            style_slug: spec.slug,
            values: vec![f32::NAN; sx * sy],
            range: spec.range,
            nx,
            ny,
            stride,
        })
        .collect();
    for ((y, x), vals) in results {
        let Some(vals) = vals else { continue };
        let cell = (y / stride) * sx + (x / stride);
        for (field, value) in out.iter_mut().zip(&vals) {
            field.values[cell] = *value;
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
            u10: Vec::new(),
            v10: Vec::new(),
            u_iso: Vec::new(),
            v_iso: Vec::new(),
        };
        let juicy = product_grid(&base, OaProduct::Sbcape, 1);
        assert!(
            juicy[0] > 500.0,
            "moist surface must have CAPE, got {}",
            juicy[0]
        );
        let mut dry = base;
        dry.td2m = vec![263.15; 16]; // -10 °C dewpoint
        let none = product_grid(&dry, OaProduct::Sbcape, 1);
        assert!(
            none[0] < 100.0,
            "dry surface must be near-zero, got {}",
            none[0]
        );
    }

    /// Bolton 1980 worked value: 1000 hPa, T=25 °C, Td=20 °C gives
    /// theta-e ~341.6 K (hand computation via Eqs. 10/15/39).
    #[test]
    fn theta_e_matches_hand_computed_bolton() {
        let v = theta_e_k(1000.0, 25.0, 20.0);
        assert!((v - 341.6).abs() < 1.0, "theta-e {v} K vs hand 341.6 K");
    }

    /// expand_full block-fills each lattice sample over its stride block,
    /// hand-checked on a 5x3 grid at stride 2 (lattice 3x2).
    #[test]
    fn expand_full_block_fills() {
        let field = CompositeField {
            name: "T",
            units: "",
            group: "Surface",
            style_slug: None,
            values: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
            range: (0.0, 1.0),
            nx: 5,
            ny: 3,
            stride: 2,
        };
        let full = field.expand_full();
        #[rustfmt::skip]
        let expect = vec![
            1.0, 1.0, 2.0, 2.0, 3.0,
            1.0, 1.0, 2.0, 2.0, 3.0,
            4.0, 4.0, 5.0, 5.0, 6.0,
        ];
        assert_eq!(full, expect);
    }

    /// A capey, sheared synthetic column for suite tests: warm moist
    /// surface under the dry-aloft standard atmosphere, with a wind
    /// profile strengthening from 9 m/s at the surface to ~40 m/s aloft.
    fn suite_inputs() -> OaCapeInputs {
        let (nx, ny) = (2, 2);
        let plane = nx * ny;
        let levels: Vec<u16> = vec![900, 800, 700, 600, 500, 400, 300, 250, 200];
        let nl = levels.len();
        let mut t_iso = vec![0.0f32; nl * plane];
        let mut td_iso = vec![0.0f32; nl * plane];
        let mut h_iso = vec![0.0f32; nl * plane];
        let mut u_iso = vec![0.0f32; nl * plane];
        let mut v_iso = vec![0.0f32; nl * plane];
        for (li, &p) in levels.iter().enumerate() {
            let z = 44_330.0 * (1.0 - (f64::from(p) / 1013.25).powf(0.1903));
            let t_k = 288.15 - 0.0065 * z;
            for i in 0..plane {
                t_iso[li * plane + i] = t_k as f32;
                td_iso[li * plane + i] = (t_k - 25.0) as f32;
                h_iso[li * plane + i] = z as f32;
                u_iso[li * plane + i] = 2.0 + 3.5 * li as f32;
                v_iso[li * plane + i] = 8.0 + 1.5 * li as f32;
            }
        }
        OaCapeInputs {
            nx,
            ny,
            psfc: vec![100_000.0; plane],
            orography: vec![100.0; plane],
            t2m: vec![303.15; plane],  // 30 °C
            td2m: vec![297.15; plane], // 24 °C
            kelvin: true,
            t_iso,
            td_iso,
            h_iso,
            levels_hpa: levels,
            u10: vec![3.0; plane],
            v10: vec![8.0; plane],
            u_iso,
            v_iso,
        }
    }

    fn suite_value(fields: &[CompositeField], name: &str) -> f32 {
        fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("missing field {name}"))
            .values[0]
    }

    /// The full composite pass: every spec present (names unique, groups
    /// valid, legacy index 0 = SCP preserved), headline fields physical,
    /// and the new category-(c) derivations verified against their
    /// published formulas using the suite's own ingredient fields —
    /// hand-computed cross-checks that pin the extraction wiring.
    #[test]
    fn composite_pass_full_suite() {
        let inputs = suite_inputs();
        let progress = std::sync::atomic::AtomicUsize::new(0);
        let fields = composite_pass(&inputs, 1, &progress);
        assert_eq!(fields.len(), SPECS.len());
        assert_eq!(fields[0].name, "SCP", "legacy auto-show index 0");
        let mut names: Vec<&str> = fields.iter().map(|f| f.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), fields.len(), "duplicate spec names");
        for field in &fields {
            assert!(
                GROUPS.contains(&field.group),
                "{} has unknown group {}",
                field.name,
                field.group
            );
        }
        let v = |name: &str| f64::from(suite_value(&fields, name));

        // Physical sanity on the headline ingredients.
        assert!(v("SBCAPE") > 500.0, "SBCAPE {}", v("SBCAPE"));
        assert!(v("MLCAPE") > 100.0, "MLCAPE {}", v("MLCAPE"));
        assert!(v("Shear 0-6km") > 10.0, "shr06 {}", v("Shear 0-6km"));
        assert!(v("Mean wind 0-6km") > 10.0);
        assert!(v("BRN shear") > 0.0);
        assert!((320.0..380.0).contains(&v("Sfc theta-e")));
        assert!((0.0..=100.0).contains(&v("Fosberg index")));
        assert!((10.0..60.0).contains(&v("Total Totals")));
        assert!((0.0..4000.0).contains(&v("SB LCL hgt")));
        assert!(v("Lapse rate 700-500").is_finite());
        assert!(v("SRH 0-1km").is_finite());
        assert!(v("SRH 0-500m").is_finite());
        assert!(v("Bunkers RM speed") > 0.0);

        // EHI = CAPE * SRH / 160000 (Hart & Korotky 1991).
        let ehi = v("SBCAPE") * v("SRH 0-1km") / 160_000.0;
        assert!(
            (v("EHI 0-1km") - ehi).abs() <= 0.02 * ehi.abs().max(0.1),
            "EHI {} vs hand {ehi}",
            v("EHI 0-1km")
        );
        // Craven & Brooks 2004: MLCAPE x 0-6 km shear (m/s).
        let sig = v("MLCAPE") * v("Shear 0-6km") * KT_TO_MS;
        assert!(
            (v("Craven-Brooks SigSvr") - sig).abs() <= 0.02 * sig,
            "SigSvr {} vs hand {sig}",
            v("Craven-Brooks SigSvr")
        );
        // Evans & Doswell 2001 DCP (knot-normalized terms).
        let dcp = (v("DCAPE") / 980.0)
            * (v("MUCAPE") / 2000.0)
            * (v("Shear 0-6km") / 20.0)
            * (v("Mean wind 0-6km") / 16.0);
        assert!(
            (v("Derecho composite") - dcp).abs() <= 0.02 * dcp.abs().max(0.01),
            "DCP {} vs hand {dcp}",
            v("Derecho composite")
        );
        // Sherburn & Parker 2014 SHERBS3.
        let sherb = (v("Shear 0-3km") * KT_TO_MS / 26.0)
            * (v("Lapse rate 0-3km") / 5.2)
            * (v("Lapse rate 700-500") / 5.6);
        assert!(
            (v("SHERBS3") - sherb).abs() <= 0.02 * sherb.abs().max(0.01),
            "SHERBS3 {} vs hand {sherb}",
            v("SHERBS3")
        );
        // Weisman & Klemp 1982: BRN = MLCAPE / BRN shear.
        let brn = v("MLCAPE") / v("BRN shear");
        assert!(
            (v("BRN") - brn).abs() <= 0.02 * brn,
            "BRN {} vs hand {brn}",
            v("BRN")
        );
        // The lattice expands to the full grid with the same value
        // everywhere (uniform inputs).
        let full = fields[0].expand_full();
        assert_eq!(full.len(), inputs.nx * inputs.ny);
    }

    /// The HRRR point sounding sharppyrs ships as its golden fixture
    /// (36.68°N 95.66°W, 2026-06-25 06z F018) — a real model column with a
    /// deep effective inflow layer, copied verbatim from that crate's
    /// `testdata/hrrr_example.rs` so both implementations analyze byte-for-byte
    /// identical input.
    fn hrrr_kbvo_sounding() -> sharppyrs::SoundingData {
        sharppyrs::SoundingData {
            pres: vec![
                985.8, 975.0, 950.0, 925.0, 900.0, 875.0, 850.0, 825.0, 800.0, 775.0, 750.0, 725.0,
                700.0, 675.0, 650.0, 625.0, 600.0, 575.0, 550.0, 525.0, 500.0, 475.0, 450.0, 425.0,
                400.0, 375.0, 350.0, 325.0, 300.0, 275.0, 250.0, 225.0, 200.0, 175.0, 150.0, 125.0,
                100.0, 75.0, 50.0,
            ],
            hght: vec![
                213.51339721679688,
                310.502197265625,
                540.55712890625,
                775.928466796875,
                1016.1875610351562,
                1261.655517578125,
                1512.6800537109375,
                1769.573486328125,
                2032.5953369140625,
                2302.38818359375,
                2579.57421875,
                2864.239501953125,
                3156.391845703125,
                3457.74072265625,
                3767.03076171875,
                4086.14111328125,
                4415.43603515625,
                4756.08935546875,
                5110.24853515625,
                5476.9296875,
                5860.0302734375,
                6256.8681640625,
                6671.98388671875,
                7106.837890625,
                7562.14111328125,
                8040.9287109375,
                8546.40234375,
                9081.7255859375,
                9651.3251953125,
                10260.05078125,
                10913.732421875,
                11621.001953125,
                12392.541015625,
                13242.4150390625,
                14188.244140625,
                15284.6171875,
                16634.470703125,
                18388.3046875,
                20905.138671875,
            ],
            tmpc: vec![
                27.166253662109398,
                26.73785400390625,
                26.321624755859375,
                24.69580078125,
                22.928131103515625,
                21.05621337890625,
                19.1776123046875,
                17.550933837890625,
                15.977386474609375,
                14.38116455078125,
                12.66644287109375,
                10.86541748046875,
                9.091461181640625,
                7.057037353515625,
                4.884246826171875,
                2.462188720703125,
                0.486236572265625,
                -1.251922607421875,
                -3.009368896484375,
                -4.9248046875,
                -7.23626708984375,
                -9.766204833984375,
                -12.420257568359375,
                -15.093414306640625,
                -17.978759765625,
                -21.136749267578125,
                -24.5872802734375,
                -28.24627685546875,
                -31.993133544921875,
                -36.24822998046875,
                -41.21965026855469,
                -46.25970458984375,
                -52.4495849609375,
                -59.24462890625,
                -66.33905029296875,
                -67.38008117675781,
                -65.44467163085938,
                -64.0802001953125,
                -58.01991271972656,
            ],
            dwpc: vec![
                23.465356445312523,
                22.610382080078125,
                21.294158935546875,
                20.248138427734375,
                19.10125732421875,
                18.41229248046875,
                17.66229248046875,
                16.340362548828125,
                14.72479248046875,
                13.157073974609375,
                11.799896240234375,
                10.030914306640625,
                7.831939697265625,
                5.2469482421875,
                2.956085205078125,
                1.66229248046875,
                0.03729248046875,
                -1.96270751953125,
                -4.71270751953125,
                -8.33770751953125,
                -11.71270751953125,
                -15.33770751953125,
                -18.58770751953125,
                -23.40020751953125,
                -28.90020751953125,
                -31.58770751953125,
                -29.21270751953125,
                -31.58770751953125,
                -35.08770751953125,
                -39.77520751953125,
                -45.40020751953125,
                -50.15020751953125,
                -63.58770751953125,
                -74.33770751953125,
                -78.83770751953125,
                -79.71270751953125,
                -81.03324890136719,
                -81.02520751953125,
                -81.02520751953125,
            ],
            wdir: vec![
                104.60573810089315,
                138.4558563232422,
                164.28582763671875,
                171.779541015625,
                177.2738800048828,
                183.77174377441406,
                190.67617797851562,
                198.77003479003906,
                208.36819458007812,
                219.11795043945312,
                228.00775146484375,
                234.8870849609375,
                240.7722625732422,
                240.48626708984375,
                242.9276123046875,
                250.9413604736328,
                259.2303771972656,
                266.01019287109375,
                270.7851257324219,
                275.0746765136719,
                277.2320556640625,
                278.1169738769531,
                276.0415954589844,
                272.3275451660156,
                268.81414794921875,
                267.2091369628906,
                268.7153625488281,
                271.86041259765625,
                278.75,
                292.636962890625,
                311.5157470703125,
                328.4579772949219,
                337.0023193359375,
                329.3992614746094,
                314.789306640625,
                296.55194091796875,
                293.8990173339844,
                287.2645568847656,
                97.38957214355469,
            ],
            wspd: vec![
                4.906037628303065,
                10.943281173706055,
                21.63814353942871,
                27.339815139770508,
                29.368289947509766,
                29.07607650756836,
                28.314294815063477,
                27.803192138671875,
                27.638057708740234,
                27.571807861328125,
                26.856725692749023,
                25.70720100402832,
                23.986652374267578,
                21.34491539001465,
                19.908233642578125,
                21.591846466064453,
                25.936294555664062,
                32.476280212402344,
                36.65385818481445,
                39.96938705444336,
                41.354278564453125,
                42.6301155090332,
                44.219573974609375,
                46.13201904296875,
                47.8463134765625,
                48.702327728271484,
                48.98480987548828,
                48.8314094543457,
                47.35453414916992,
                45.13577651977539,
                45.340389251708984,
                41.565120697021484,
                36.321983337402344,
                39.186214447021484,
                52.992671966552734,
                44.226234436035156,
                29.563827514648438,
                4.533064365386963,
                9.24278450012207,
            ],
            omeg: Some(vec![
                0.0,
                -0.4323129653930664,
                -1.3790178298950195,
                -1.8084402084350586,
                -1.9566125869750977,
                -1.9744071960449219,
                -1.9750938415527344,
                -1.9902076721191406,
                -2.0170440673828125,
                -2.006511688232422,
                -1.904571533203125,
                -1.6951675415039062,
                -1.4294052124023438,
                -0.9965896606445312,
                -0.5760574340820312,
                -0.11753463745117188,
                0.24896240234375,
                0.5164413452148438,
                0.6187515258789062,
                0.6002998352050781,
                0.5252838134765625,
                0.4488372802734375,
                0.43321990966796875,
                0.5521163940429688,
                0.7316207885742188,
                0.9387054443359375,
                0.98828125,
                0.6303482055664062,
                -0.00026702880859375,
                -0.5542945861816406,
                -1.0112495422363281,
                -1.2430343627929688,
                -0.985260009765625,
                -0.4290752410888672,
                -0.2476654052734375,
                -0.255340576171875,
                -0.15076017379760742,
                0.1661156415939331,
                0.23698043823242188,
            ]),
            latitude: Some(36.675168663242175),
            longitude: Some(-95.65655745938363),
            missing: None,
        }
    }

    /// BowEcho analyses every sounding twice: the sounding window runs
    /// sharppyrs, the OA mesoanalysis map runs sharprs' compositor. Both are
    /// SHARPpy ports of the *same* algorithms over the *same* column, so the
    /// two must agree. This pins the two inputs the composite parameters are
    /// most sensitive to — the Bunkers storm motion, which sets every
    /// storm-relative quantity, and the effective bulk wind difference, a bare
    /// multiplicative term in SCP/STP(CIN)/VTP whose 10 and 12.5 m/s cutoffs
    /// zero the entire composite when EBWD comes out too small.
    ///
    /// sharppyrs is the oracle: its values are the ones the sounding window
    /// already displays and the ones its own golden fixture records.
    #[test]
    fn compositor_matches_sharppyrs_storm_motion_and_ebwd() {
        let data = hrrr_kbvo_sounding();

        // -- oracle: the sounding window's analysis --------------------------
        let oracle = sharppyrs::Profile::new(data.clone()).expect("sharppyrs profile builds");
        let dv = sharppyrs::DerivedParams::compute(&oracle);
        assert!(
            oracle.ebottom.is_finite() && oracle.etop.is_finite(),
            "fixture must have an effective inflow layer to compare against"
        );
        let oracle_ebwd_kt = dv.ebwd.0.hypot(dv.ebwd.1);
        let oracle_eff_shear_kt = dv.eff_shear.0.hypot(dv.eff_shear.1);
        assert!(
            (oracle_ebwd_kt - oracle_eff_shear_kt).abs() > 5.0,
            "fixture must separate EBWD ({oracle_ebwd_kt:.2} kt) from the \
             inflow-layer shear ({oracle_eff_shear_kt:.2} kt), else the test \
             cannot tell the two apart"
        );

        // -- subject: the OA map layer's analysis ----------------------------
        let station = sharprs::profile::StationInfo {
            station_id: "OA".to_owned(),
            latitude: data.latitude.unwrap_or(f64::NAN),
            longitude: data.longitude.unwrap_or(f64::NAN),
            elevation: f64::NAN,
            datetime: String::new(),
        };
        let profile = sharprs::profile::Profile::new(
            &data.pres,
            &data.hght,
            &data.tmpc,
            &data.dwpc,
            &data.wdir,
            &data.wspd,
            data.omeg.as_deref().unwrap_or(&[]),
            station,
        )
        .expect("sharprs profile builds");
        let p = sharprs::render::compositor::compute_all_params(&profile);

        // Measured agreement on this column is exact to every printed digit;
        // 0.1% leaves headroom for the two crates' slightly different
        // thermodynamic normalizations reaching the MU parcel EL, and the
        // defects this guards against are 46-49% errors.
        const TOL: f64 = 0.001;
        let mut report: Vec<String> = Vec::new();
        let mut bad: Vec<String> = Vec::new();
        let mut check = |what: &str, map: f64, sounding: f64, tol: f64| {
            let rel = (map - sounding).abs() / sounding.abs().max(1.0);
            let row = format!(
                "{what}: map {map:.4}, sounding {sounding:.4}, off by {:.2}% (tol {:.1}%)",
                rel * 100.0,
                tol * 100.0
            );
            // A non-finite ratio means one side produced NaN, which must fail
            // rather than silently compare equal.
            if !rel.is_finite() || rel > tol {
                bad.push(row.clone());
            }
            report.push(row);
        };
        check("Bunkers RM u (kt)", p.rstu, oracle.srwind.0, TOL);
        check("Bunkers RM v (kt)", p.rstv, oracle.srwind.1, TOL);
        check("Bunkers LM u (kt)", p.lstu, oracle.srwind.2, TOL);
        check("Bunkers LM v (kt)", p.lstv, oracle.srwind.3, TOL);
        let map_ebwd_kt = p.effective_bwd.expect("map computes an EBWD") / KT_TO_MS;
        check("EBWD (kt)", map_ebwd_kt, oracle_ebwd_kt, TOL);
        // The inflow-layer shear the fix split out must survive as its own
        // quantity and keep matching the sounding window's "Eff Inflow" row.
        let map_eff_shear_kt = p.eff_shear.expect("map keeps the inflow-layer shear") / KT_TO_MS;
        check(
            "Eff Inflow shear (kt)",
            map_eff_shear_kt,
            oracle_eff_shear_kt,
            TOL,
        );
        check(
            "Eff SRH (m2/s2)",
            p.effective_srh.unwrap_or(f64::NAN),
            oracle.right_esrh,
            TOL,
        );

        assert!(
            bad.is_empty(),
            "sharprs compositor disagrees with the sharppyrs sounding window:\n  {}\n\
             full comparison:\n  {}",
            bad.join("\n  "),
            report.join("\n  ")
        );

        // Why the conflation was fatal rather than merely biasing: `vtp_mod`'s
        // EBWD term is `0.0` for EBWD <= 20 m/s and multiplies straight
        // through. Feed it the inflow-layer shear the map used to pass as
        // EBWD and it returns an exact zero on this supercell column.
        let vtp_from_inflow_shear = sharprs::params::composites::vtp_mod(
            p.mlpcl.bplus,
            p.effective_srh.expect("effective SRH"),
            p.eff_shear.expect("inflow-layer shear"),
            p.mlpcl.lclhght,
            p.mlpcl.bminus,
            p.mlpcl.b3km,
            p.lr75.expect("700-500 lapse rate"),
        );
        assert_eq!(
            vtp_from_inflow_shear,
            Some(0.0),
            "the pre-fix EBWD substitute must reproduce the zeroed VTP"
        );

        // The user-visible symptom, pinned directly: on a supercell column the
        // shear-gated composites must not paint zero.
        for (name, value) in [("SCP", p.scp), ("STP (CIN)", p.stp_cin), ("VTP", p.vtp_mod)] {
            assert!(
                value.is_some_and(|v| v > 0.0),
                "{name} must be positive on this supercell column, got {value:?}"
            );
        }
    }
}
