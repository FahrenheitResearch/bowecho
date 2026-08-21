//! Main compositor — assembles all panels into a final SHARPpy-style sounding image.
//!
//! The reference layout (2400×1800 px, 2× for crisp rendering) is:
//!
//! ```text
//! +--------------------------------------------------+---------------------+
//! | OMEGA |         SKEW-T              | BARBS |  HODOGRAPH            |
//! | (thin)|    (main diagram)           |(right)|  (top-right)          |
//! |       |                             |       |                       |
//! |       |                             |       +-----------------------+
//! |       |                             |       |                       |
//! |       |                             |       |     STORM SLINKY      |
//! |       |                             |       |                       |
//! +-------+-----------------------------+-------+-----------------------+
//! | PARAMETER TABLE (full width)                                        |
//! +---------------------------------------------------------------------+
//! ```
//!
//! The compositor function [`render_full_sounding`] creates the main canvas,
//! calls into the sub-module renderers, and assembles the final image.

use crate::params::cape::{self, DcapeResult, ParcelResult, ParcelType};
use crate::params::composites;
use crate::params::indices;
use crate::profile::Profile;
use crate::watch_type;
use crate::winds;

use super::canvas::Canvas;
use super::hodograph::{self, HodographData};
use super::panels::{self, SarsCategory, SarsData, SlinkyPoint, StpClimatology};
use super::param_table::{self, LapseRateRow, ParamTableData, ParcelRow, PixelBuf, ShearRow};

// =========================================================================
// Layout constants (pixels)
// =========================================================================

/// Total image width (2× for crisp rendering).
pub const IMG_W: u32 = 2400;
/// Total image height (2× for crisp rendering).
pub const IMG_H: u32 = 1800;

/// Title bar height at the very top.
const TITLE_H: u32 = 44;

/// Height of the upper panel region (skew-T + hodograph + insets).
const UPPER_H: u32 = 1120;

/// Parameter table height (bottom strip).
const TABLE_H: u32 = IMG_H - TITLE_H - UPPER_H; // 636

/// Width of the left region (skew-T with built-in omega + barbs).
/// The skew-T renderer handles omega and barbs internally.
const LEFT_W: u32 = 1680;

/// Width of the right column (hodograph + inset panels).
const RIGHT_W: u32 = IMG_W - LEFT_W; // 720

/// Height of the hodograph in the top-right.
const HODO_H: u32 = 600;

/// Height of the inset panels region below the hodograph.
const INSET_H: u32 = UPPER_H - HODO_H; // 520

// =========================================================================
// Colour palette
// =========================================================================

/// Knots to metres per second (SHARPpy's constant).
const KTS_TO_MS: f64 = 0.514_444;

const COL_BG: [u8; 4] = [0, 0, 0, 255];
const COL_TITLE_BG: [u8; 4] = [0, 0, 0, 255];
const COL_WHITE: [u8; 4] = [230, 230, 230, 255];
const COL_BORDER: [u8; 4] = [215, 215, 215, 255];

// =========================================================================
// ComputedParams — all pre-computed values needed by every panel
// =========================================================================

/// Holds every derived parameter needed to render the full sounding image.
///
/// Constructed once via [`compute_all_params`] and then borrowed immutably by
/// each panel renderer.
#[derive(Debug, Clone)]
pub struct ComputedParams {
    // -- Parcel results --
    pub sfcpcl: ParcelResult,
    pub mlpcl: ParcelResult,
    pub mupcl: ParcelResult,
    pub sfc_ecape: EcapeParcelParams,
    pub ml_ecape: EcapeParcelParams,
    pub mu_ecape: EcapeParcelParams,

    // -- DCAPE --
    pub dcape: DcapeResult,

    // -- Storm motion (Bunkers) --
    pub rstu: f64,
    pub rstv: f64,
    pub lstu: f64,
    pub lstv: f64,

    // -- Corfidi vectors --
    pub corfidi_up_u: f64,
    pub corfidi_up_v: f64,
    pub corfidi_dn_u: f64,
    pub corfidi_dn_v: f64,

    // -- Helicity --
    /// 0-1 km SRH (total, positive, negative) m^2/s^2.
    pub srh01: (f64, f64, f64),
    /// 0-3 km SRH.
    pub srh03: (f64, f64, f64),

    // -- Shear --
    pub shr01: (f64, f64),
    pub shr03: (f64, f64),
    pub shr06: (f64, f64),
    pub shr08: (f64, f64),

    // -- Mean wind --
    pub mean_wind_06: (f64, f64),

    // -- Effective inflow layer --
    pub eff_inflow: (f64, f64),
    /// Cyclonic effective SRH magnitude (m2/s2): right-mover in the Northern
    /// Hemisphere, sign-normalized left-mover in the Southern Hemisphere.
    pub effective_srh: Option<f64>,
    /// Bulk shear across the effective inflow layer itself (base -> top), m/s.
    /// SHARPpy labels this "Eff Inflow" in the shear table.
    pub eff_shear: Option<f64>,
    /// Effective bulk wind difference (EBWD), m/s: inflow base -> half the
    /// depth from that base to the MU parcel EL. SHARPpy labels this
    /// "Eff Shear"; it is the shear term SCP, STP(CIN) and VTP consume.
    pub effective_bwd: Option<f64>,

    // -- Stability indices --
    pub k_index: Option<f64>,
    pub t_totals: Option<f64>,
    pub v_totals: Option<f64>,
    pub c_totals: Option<f64>,
    pub precip_water: Option<f64>,
    pub conv_t: Option<f64>,
    pub max_temp: Option<f64>,

    // -- Lapse rates --
    pub lr03: Option<f64>,
    pub lr36: Option<f64>,
    pub lr75: Option<f64>,
    pub lr85: Option<f64>,

    // -- Composite parameters --
    pub stp_fixed: Option<f64>,
    pub stp_cin: Option<f64>,
    pub scp: Option<f64>,
    pub ship: Option<f64>,
    pub tehi: Option<f64>,
    pub tts: Option<f64>,
    pub vtp_mod: Option<f64>,

    // -- EHI --
    pub ehi01: Option<f64>,
    pub ehi03: Option<f64>,

    // -- Temperature levels --
    pub frz_lvl: Option<f64>,
    pub wb_zero: Option<f64>,

    // -- Theta-E --
    pub tei: Option<f64>,

    // -- Watch type --
    pub watch_type: watch_type::WatchType,

    // -- Critical angle --
    pub critical_angle: f64,

    // -- Mean mixing ratio / RH --
    pub mean_mixr: Option<f64>,
    pub mean_rh_low: Option<f64>,
    pub mean_rh_mid: Option<f64>,

    // -- Wind at key levels --
    pub wind_1km: (f64, f64), // dir, spd
    pub wind_6km: (f64, f64),
}

/// Verified ECAPE parcel diagnostics supplied by rustwx-sounding.
#[derive(Debug, Clone, Copy)]
pub struct EcapeParcelParams {
    /// Entraining CAPE (J/kg).
    pub ecape: f64,
    /// Normalized CAPE from the ECAPE parcel path.
    pub ncape: f64,
    /// Undiluted parcel CAPE (J/kg), computed through ecape-rs with zero entrainment.
    pub cape: f64,
    /// Undiluted parcel CIN (J/kg).
    pub cinh: f64,
    /// Undiluted 0-3 km positive buoyancy (J/kg).
    pub cape_3km: f64,
    /// Undiluted 0-6 km positive buoyancy (J/kg).
    pub cape_6km: f64,
    /// LFC height (m AGL).
    pub lfc_m: f64,
    /// EL height (m AGL).
    pub el_m: f64,
}

impl EcapeParcelParams {
    pub const fn missing() -> Self {
        Self {
            ecape: f64::NAN,
            ncape: f64::NAN,
            cape: f64::NAN,
            cinh: f64::NAN,
            cape_3km: f64::NAN,
            cape_6km: f64::NAN,
            lfc_m: f64::NAN,
            el_m: f64::NAN,
        }
    }
}

impl Default for EcapeParcelParams {
    fn default() -> Self {
        Self::missing()
    }
}

#[inline]
fn southern_hemisphere(profile: &Profile) -> bool {
    profile.station.latitude.is_finite() && profile.station.latitude < 0.0
}

/// Select the cyclonic Bunkers mover for the station's hemisphere while
/// retaining both raw right/left vectors in [`ComputedParams`] for display.
#[inline]
fn cyclonic_storm_motion(
    profile: &Profile,
    rstu: f64,
    rstv: f64,
    lstu: f64,
    lstv: f64,
) -> (f64, f64) {
    if southern_hemisphere(profile) {
        (lstu, lstv)
    } else {
        (rstu, rstv)
    }
}

/// Express helicity in a cyclonic-positive coordinate system. A mirrored
/// Southern Hemisphere hodograph has negative raw total/segments, so swap and
/// negate the signed buckets as well as the total.
#[inline]
fn cyclonic_helicity(
    profile: &Profile,
    (total, positive, negative): (f64, f64, f64),
) -> (f64, f64, f64) {
    if southern_hemisphere(profile) {
        (-total, -negative, -positive)
    } else {
        (total, positive, negative)
    }
}

/// Compute all derived sounding parameters from a profile.
///
/// This is the single entry point that calls into every analysis module.
/// The resulting [`ComputedParams`] is passed to each panel renderer.
pub fn compute_all_params(profile: &Profile) -> ComputedParams {
    // -- Build cape::Profile from the main Profile --
    let cape_prof = cape::Profile::new(
        profile.pres.clone(),
        profile.hght.clone(),
        profile.tmpc.clone(),
        profile.dwpc.clone(),
        profile.sfc,
    );

    // -- Parcel computations --
    let sfc_lpl = cape::define_parcel(&cape_prof, ParcelType::Surface);
    let sfcpcl = cape::parcelx(&cape_prof, &sfc_lpl, None, None);

    let ml_lpl = cape::define_parcel(&cape_prof, ParcelType::MixedLayer { depth_hpa: 100.0 });
    let mlpcl = cape::parcelx(&cape_prof, &ml_lpl, None, None);

    let mu_lpl = cape::define_parcel(&cape_prof, ParcelType::MostUnstable { depth_hpa: 300.0 });
    let mupcl = cape::parcelx(&cape_prof, &mu_lpl, None, None);

    let dcape_result = cape::dcape(&cape_prof);

    // -- Effective inflow layer --
    let eff_inflow = cape::effective_inflow_layer(&cape_prof, 100.0, -250.0, Some(&mupcl));
    let has_effective_inflow = eff_inflow.0.is_finite() && eff_inflow.1.is_finite();

    // -- Storm motion --
    // SHARPpy's ConvectiveProfile takes the parcel-based Bunkers motion
    // whenever an effective inflow layer exists, and only falls back to the
    // fixed SFC-6 km method when it does not.
    let (rstu, rstv, lstu, lstv) = if has_effective_inflow {
        winds::bunkers_storm_motion(profile, &mupcl, eff_inflow.0)
    } else {
        winds::non_parcel_bunkers_motion(profile).unwrap_or((
            f64::NAN,
            f64::NAN,
            f64::NAN,
            f64::NAN,
        ))
    };
    let (storm_u, storm_v) = cyclonic_storm_motion(profile, rstu, rstv, lstu, lstv);

    // -- Corfidi vectors --
    let (corfidi_up_u, corfidi_up_v, corfidi_dn_u, corfidi_dn_v) =
        winds::corfidi_mcs_motion(profile).unwrap_or((f64::NAN, f64::NAN, f64::NAN, f64::NAN));

    // -- Helicity --
    let srh01 = cyclonic_helicity(
        profile,
        winds::helicity(profile, 0.0, 1000.0, storm_u, storm_v, -1.0, true).unwrap_or((
            f64::NAN,
            f64::NAN,
            f64::NAN,
        )),
    );
    let srh03 = cyclonic_helicity(
        profile,
        winds::helicity(profile, 0.0, 3000.0, storm_u, storm_v, -1.0, true).unwrap_or((
            f64::NAN,
            f64::NAN,
            f64::NAN,
        )),
    );

    // -- Shear --
    let p_sfc = profile.sfc_pressure();
    let p1km = profile.pres_at_height(profile.to_msl(1000.0));
    let p3km = profile.pres_at_height(profile.to_msl(3000.0));
    let p6km = profile.pres_at_height(profile.to_msl(6000.0));
    let p8km = profile.pres_at_height(profile.to_msl(8000.0));

    let shr01 = winds::wind_shear(profile, p_sfc, p1km).unwrap_or((f64::NAN, f64::NAN));
    let shr03 = winds::wind_shear(profile, p_sfc, p3km).unwrap_or((f64::NAN, f64::NAN));
    let shr06 = winds::wind_shear(profile, p_sfc, p6km).unwrap_or((f64::NAN, f64::NAN));
    let shr08 = winds::wind_shear(profile, p_sfc, p8km).unwrap_or((f64::NAN, f64::NAN));

    let eff_bot_h = profile.to_agl(profile.interp_hght(eff_inflow.0));
    let eff_top_h = profile.to_agl(profile.interp_hght(eff_inflow.1));
    // A single-level inflow layer is legitimate (`helicity` returns zero for a
    // zero-thickness layer); demanding a strictly positive depth turned those
    // cells into holes on the composite grid.
    let effective_srh = if eff_bot_h.is_finite() && eff_top_h.is_finite() {
        winds::helicity(profile, eff_bot_h, eff_top_h, storm_u, storm_v, -1.0, true)
            .ok()
            .map(|value| cyclonic_helicity(profile, value).0)
    } else {
        None
    };
    // Two distinct effective-layer shears, kept apart the way SHARPpy keeps
    // them apart:
    //   * `eff_shear` spans the inflow layer itself (base -> top) and is what
    //     the shear table's "Eff Inflow" row reports;
    //   * `effective_bwd` is the effective bulk wind difference (EBWD), which
    //     spans the base up to half the depth from the base to the MU parcel's
    //     equilibrium level.  EBWD — not the inflow-layer shear — is the term
    //     SCP, STP(CIN) and VTP multiply through, and their 10 / 12.5 m/s
    //     cutoffs zero the whole composite when it is understated.
    let (eff_shear, effective_bwd) = if has_effective_inflow {
        let layer = winds::wind_shear(profile, eff_inflow.0, eff_inflow.1)
            .ok()
            .map(|(u, v)| (u * u + v * v).sqrt() * KTS_TO_MS);
        let ebwd = if eff_bot_h.is_finite() && mupcl.elhght.is_finite() {
            let depth = (mupcl.elhght - eff_bot_h) / 2.0;
            let elh = profile.pres_at_height(profile.to_msl(eff_bot_h + depth));
            winds::wind_shear(profile, eff_inflow.0, elh)
                .ok()
                .map(|(u, v)| (u * u + v * v).sqrt() * KTS_TO_MS)
        } else {
            None
        };
        (layer, ebwd)
    } else {
        (None, None)
    };

    // -- Mean wind --
    let mean_wind_06 =
        winds::mean_wind(profile, p_sfc, p6km, -1.0, 0.0, 0.0).unwrap_or((f64::NAN, f64::NAN));

    // -- Stability indices --
    let k_index = indices::k_index(profile);
    let t_totals = indices::t_totals(profile);
    let v_totals = indices::v_totals(profile);
    let c_totals = indices::c_totals(profile);
    let precip_water = indices::precip_water(profile, None, None);
    let conv_t = indices::conv_t(profile);
    let max_temp = indices::max_temp(profile, None);

    // -- Lapse rates --
    let lr03 = indices::lapse_rate(profile, 0.0, 3000.0, false);
    let lr36 = indices::lapse_rate(profile, 3000.0, 6000.0, false);
    let lr75 = indices::lapse_rate(profile, 700.0, 500.0, true);
    let lr85 = indices::lapse_rate(profile, 850.0, 500.0, true);

    // -- Temperature levels --
    let frz_lvl = indices::temp_lvl(profile, 0.0, false).map(|p| {
        let h = profile.interp_hght(p);
        if h.is_finite() {
            h - profile.sfc_height()
        } else {
            f64::NAN
        }
    });
    let wb_zero = indices::wet_bulb_zero(profile);

    // -- Composite parameters --
    let shr06_mag = composites::shr_sfc_to_6km(shr06.0, shr06.1);
    let stp_fixed = shr06_mag
        .and_then(|bwd6| composites::stp_fixed(sfcpcl.bplus, sfcpcl.lclhght, srh01.0, bwd6));
    // SHARPpy defines effective-layer severe composites as zero when no
    // effective inflow layer exists. Keep the diagnostic inputs missing, but
    // do not turn the user-facing SCP/STP/VTP maps into NaN holes. If a layer
    // does exist and one of its calculations fails, retain None honestly.
    let stp_cin = if has_effective_inflow {
        effective_bwd.and_then(|ebwd| {
            composites::stp_cin(
                mlpcl.bplus,
                effective_srh.unwrap_or(f64::NAN),
                ebwd,
                mlpcl.lclhght,
                mlpcl.bminus,
            )
        })
    } else {
        Some(0.0)
    };
    let scp = if has_effective_inflow {
        effective_bwd
            .and_then(|ebwd| composites::scp(mupcl.bplus, effective_srh.unwrap_or(f64::NAN), ebwd))
    } else {
        Some(0.0)
    };
    let ship = {
        let mu_mr = if mupcl.pres.is_finite() && mupcl.dwpc.is_finite() {
            crate::profile::mixratio(mupcl.pres, mupcl.dwpc)
        } else {
            f64::NAN
        };
        let t500 = profile.interp_tmpc(500.0);
        let frz_agl = frz_lvl.unwrap_or(f64::NAN);
        match (lr75, shr06_mag) {
            (Some(lr), Some(shr)) => composites::ship(mupcl.bplus, mu_mr, lr, t500, shr, frz_agl),
            _ => None,
        }
    };

    // -- EHI --
    let ehi01 = composites::ehi(sfcpcl.bplus, srh01.0);
    let ehi03 = composites::ehi(sfcpcl.bplus, srh03.0);
    let tehi = shr06_mag.and_then(|shear06| {
        composites::tehi(
            srh01.0,
            mlpcl.bplus,
            mlpcl.b3km,
            shear06,
            mlpcl.lclhght,
            mlpcl.bminus,
            sfcpcl.bminus,
        )
    });
    let tts = shr06_mag.and_then(|shear06| {
        composites::tts(
            srh01.0,
            mlpcl.b3km,
            mlpcl.bplus,
            shear06,
            mlpcl.lclhght,
            mlpcl.bminus,
            sfcpcl.bminus,
        )
    });
    let vtp_mod = if has_effective_inflow {
        match (effective_srh, effective_bwd, lr75) {
            (Some(esrh), Some(ebwd), Some(lr75)) => composites::vtp_mod(
                mlpcl.bplus,
                esrh,
                ebwd,
                mlpcl.lclhght,
                mlpcl.bminus,
                mlpcl.b3km,
                lr75,
            ),
            _ => None,
        }
    } else {
        Some(0.0)
    };

    // -- TEI --
    let tei = indices::thetae_diff(profile);

    // -- Watch type --
    let shr08_mag = (shr08.0 * shr08.0 + shr08.1 * shr08.1).sqrt();
    let watch_params = watch_type::WatchParams {
        stp_eff: stp_cin.unwrap_or(0.0),
        stp_fixed: stp_fixed.unwrap_or(0.0),
        srw_4_6km: f64::NAN,
        esrh: srh01.0,
        srh1km: srh01.0,
        sfc_8km_shear: shr08_mag,
        lr1: lr03.unwrap_or(0.0),
        sfcpcl_lclhght: sfcpcl.lclhght,
        mlpcl_lclhght: mlpcl.lclhght,
        mlpcl_bminus: mlpcl.bminus,
        mupcl_bminus: mupcl.bminus,
        ebotm: eff_inflow.0,
        scp: scp.unwrap_or(0.0),
        ship: ship.unwrap_or(0.0),
        sig_severe: 0.0,
        mmp: 0.0,
        wndg: 0.0,
        dcape: dcape_result.dcape,
        pwat: precip_water.unwrap_or(0.0),
        pwv_flag: 0,
        low_rh: 0.0,
        mid_rh: 0.0,
        upshear_wspd: 0.0,
        sfc_tmpc: profile.tmpc[profile.sfc],
        sfc_dwpc: profile.dwpc[profile.sfc],
        sfc_pres: p_sfc,
        sfc_wspd_kts: profile.wspd[profile.sfc],
        precip_type: String::new(),
    };
    let watch = watch_type::best_watch(&watch_params);

    // -- Critical angle --
    let critical_angle = winds::critical_angle(profile, storm_u, storm_v).unwrap_or(f64::NAN);

    // -- Mean mixing ratio / RH --
    let mean_mixr = indices::mean_mixratio(profile, None, None);
    let mean_rh_low = indices::mean_relh(profile, None, None);
    let p700 = 700.0_f64;
    let p500 = 500.0_f64;
    let mean_rh_mid = indices::mean_relh(profile, Some(p700), Some(p500));

    // -- Wind at key levels --
    let wind_1km = if p1km.is_finite() {
        profile.interp_vec(p1km)
    } else {
        (f64::NAN, f64::NAN)
    };
    let wind_6km = if p6km.is_finite() {
        profile.interp_vec(p6km)
    } else {
        (f64::NAN, f64::NAN)
    };

    ComputedParams {
        sfcpcl,
        mlpcl,
        mupcl,
        sfc_ecape: EcapeParcelParams::missing(),
        ml_ecape: EcapeParcelParams::missing(),
        mu_ecape: EcapeParcelParams::missing(),
        dcape: dcape_result,
        rstu,
        rstv,
        lstu,
        lstv,
        corfidi_up_u,
        corfidi_up_v,
        corfidi_dn_u,
        corfidi_dn_v,
        srh01,
        srh03,
        shr01,
        shr03,
        shr06,
        shr08,
        mean_wind_06,
        eff_inflow,
        effective_srh,
        eff_shear,
        effective_bwd,
        k_index,
        t_totals,
        v_totals,
        c_totals,
        precip_water,
        conv_t,
        max_temp,
        lr03,
        lr36,
        lr75,
        lr85,
        stp_fixed,
        stp_cin,
        scp,
        ship,
        tehi,
        tts,
        vtp_mod,
        ehi01,
        ehi03,
        frz_lvl,
        wb_zero,
        tei,
        watch_type: watch,
        critical_angle,
        mean_mixr,
        mean_rh_low,
        mean_rh_mid,
        wind_1km,
        wind_6km,
    }
}

// =========================================================================
// Data bridge helpers — convert ComputedParams into render-module structs
// =========================================================================

/// Build [`HodographData`] from a profile (delegates to hodograph module).
fn build_hodo_data(profile: &Profile) -> Option<HodographData> {
    hodograph::hodograph_data_from_profile(profile).ok()
}

/// Build [`ParamTableData`] from ComputedParams.
fn build_param_table(profile: &Profile, p: &ComputedParams) -> ParamTableData {
    let nan_or = |v: f64| if v.is_finite() { v } else { f64::NAN };
    let opt_or = |v: Option<f64>| v.unwrap_or(f64::NAN);
    let mag = |u: f64, v: f64| (u * u + v * v).sqrt();

    let p_sfc = profile.sfc_pressure();
    let p500m = profile.pres_at_height(profile.to_msl(500.0));
    let p1km = profile.pres_at_height(profile.to_msl(1000.0));
    let p2km = profile.pres_at_height(profile.to_msl(2000.0));
    let p3km = profile.pres_at_height(profile.to_msl(3000.0));
    let p3500m = profile.pres_at_height(profile.to_msl(3500.0));
    let p6km = profile.pres_at_height(profile.to_msl(6000.0));
    let p12km = profile.pres_at_height(profile.to_msl(12000.0));
    let (storm_u, storm_v) = cyclonic_storm_motion(profile, p.rstu, p.rstv, p.lstu, p.lstv);

    let shear_mag = |pbot: f64, ptop: f64| -> f64 {
        winds::wind_shear(profile, pbot, ptop)
            .map(|(u, v)| mag(u, v))
            .unwrap_or(f64::NAN)
    };
    let mean_wind_mag = |pbot: f64, ptop: f64| -> f64 {
        winds::mean_wind(profile, pbot, ptop, -1.0, 0.0, 0.0)
            .map(|(u, v)| mag(u, v))
            .unwrap_or(f64::NAN)
    };
    let sr_wind = |pbot: f64, ptop: f64| -> (f64, f64, f64) {
        if let Ok((su, sv)) = winds::sr_wind(profile, pbot, ptop, storm_u, storm_v, -1.0) {
            let (dir, spd) = crate::profile::comp2vec(su, sv);
            (dir, spd, spd)
        } else {
            (f64::NAN, f64::NAN, f64::NAN)
        }
    };
    let srh_layer = |bottom_agl: f64, top_agl: f64| -> f64 {
        winds::helicity(profile, bottom_agl, top_agl, storm_u, storm_v, -1.0, true)
            .map(|value| cyclonic_helicity(profile, value).0)
            .unwrap_or(f64::NAN)
    };
    let ehi_for_srh = |srh: f64| composites::ehi(p.sfcpcl.bplus, srh).unwrap_or(f64::NAN);
    let shear_row =
        |label: &str, pbot: f64, ptop: f64, bottom_agl: f64, top_agl: f64| -> ShearRow {
            let srh = srh_layer(bottom_agl, top_agl);
            let (srw_dir, srw_spd, srw) = sr_wind(pbot, ptop);
            ShearRow {
                label: label.into(),
                ehi: ehi_for_srh(srh),
                srh: nan_or(srh),
                shear: shear_mag(pbot, ptop),
                mn_wind: mean_wind_mag(pbot, ptop),
                srw_dir,
                srw_spd,
                srw,
            }
        };
    let finite_or = |value: f64, fallback: f64| {
        if value.is_finite() {
            value
        } else {
            fallback
        }
    };

    let parcels = vec![
        ParcelRow {
            label: "SFC".into(),
            ecape: nan_or(p.sfc_ecape.ecape),
            ncape: nan_or(p.sfc_ecape.ncape),
            cape: finite_or(p.sfcpcl.bplus, nan_or(p.sfc_ecape.cape)),
            cape_3km: finite_or(p.sfcpcl.b3km, nan_or(p.sfc_ecape.cape_3km)),
            cape_6km: finite_or(p.sfcpcl.b6km, nan_or(p.sfc_ecape.cape_6km)),
            cinh: finite_or(p.sfcpcl.bminus, nan_or(p.sfc_ecape.cinh)),
            lcl_m: nan_or(p.sfcpcl.lclhght),
            li: nan_or(p.sfcpcl.li5),
            lfc_m: nan_or(p.sfcpcl.lfchght),
            el_m: nan_or(p.sfcpcl.elhght),
        },
        ParcelRow {
            label: "ML".into(),
            ecape: nan_or(p.ml_ecape.ecape),
            ncape: nan_or(p.ml_ecape.ncape),
            cape: finite_or(p.mlpcl.bplus, nan_or(p.ml_ecape.cape)),
            cape_3km: finite_or(p.mlpcl.b3km, nan_or(p.ml_ecape.cape_3km)),
            cape_6km: finite_or(p.mlpcl.b6km, nan_or(p.ml_ecape.cape_6km)),
            cinh: finite_or(p.mlpcl.bminus, nan_or(p.ml_ecape.cinh)),
            lcl_m: nan_or(p.mlpcl.lclhght),
            li: nan_or(p.mlpcl.li5),
            lfc_m: nan_or(p.mlpcl.lfchght),
            el_m: nan_or(p.mlpcl.elhght),
        },
        ParcelRow {
            label: "MU".into(),
            ecape: nan_or(p.mu_ecape.ecape),
            ncape: nan_or(p.mu_ecape.ncape),
            cape: finite_or(p.mupcl.bplus, nan_or(p.mu_ecape.cape)),
            cape_3km: finite_or(p.mupcl.b3km, nan_or(p.mu_ecape.cape_3km)),
            cape_6km: finite_or(p.mupcl.b6km, nan_or(p.mu_ecape.cape_6km)),
            cinh: finite_or(p.mupcl.bminus, nan_or(p.mu_ecape.cinh)),
            lcl_m: nan_or(p.mupcl.lclhght),
            li: nan_or(p.mupcl.li5),
            lfc_m: nan_or(p.mupcl.lfchght),
            el_m: nan_or(p.mupcl.elhght),
        },
    ];

    let eff_bot_h = profile.to_agl(profile.interp_hght(p.eff_inflow.0));
    let eff_top_h = profile.to_agl(profile.interp_hght(p.eff_inflow.1));
    // The "Eff Inflow" row reports the shear across the inflow layer itself,
    // so it takes `eff_shear` (m/s -> kt) rather than the EBWD that
    // `effective_bwd` now carries.
    let mut eff_inflow_row = shear_row(
        "Eff Inflow",
        p.eff_inflow.0,
        p.eff_inflow.1,
        eff_bot_h,
        eff_top_h,
    );
    eff_inflow_row.shear = p
        .eff_shear
        .map_or(f64::NAN, |shear_ms| shear_ms / KTS_TO_MS);
    let shear_layers = vec![
        shear_row("SFC-500m", p_sfc, p500m, 0.0, 500.0),
        shear_row("SFC-1km", p_sfc, p1km, 0.0, 1000.0),
        eff_inflow_row,
        shear_row("SFC-3km", p_sfc, p3km, 0.0, 3000.0),
        shear_row("1km-3km", p1km, p3km, 1000.0, 3000.0),
        shear_row("3km-6km", p3km, p6km, 3000.0, 6000.0),
        shear_row("SFC-6km", p_sfc, p6km, 0.0, 6000.0),
        shear_row("SFC-2km", p_sfc, p2km, 0.0, 2000.0),
    ];
    let lr03_table = p
        .lr03
        .unwrap_or_else(|| lapse_rate_agl(profile, 0.0, 3000.0));
    let lr36_table = p
        .lr36
        .unwrap_or_else(|| lapse_rate_agl(profile, 3000.0, 6000.0));
    let sfc_lcl_lr = if p.sfcpcl.lclhght.is_finite() && p.sfcpcl.lclhght > 1.0 {
        let value = lapse_rate_agl(profile, 0.0, p.sfcpcl.lclhght);
        if value.is_finite() {
            value
        } else {
            let lcl_env_tmpc = profile.interp_tmpc(p.sfcpcl.lclpres);
            (profile.tmpc[profile.sfc] - lcl_env_tmpc) / p.sfcpcl.lclhght * 1000.0
        }
    } else {
        f64::NAN
    };

    let lapse_rates = vec![
        LapseRateRow {
            label: "SFC-LCL".into(),
            value: sfc_lcl_lr,
        },
        LapseRateRow {
            label: "950-850".into(),
            value: indices::lapse_rate(profile, 950.0, 850.0, true).unwrap_or(f64::NAN),
        },
        LapseRateRow {
            label: "SFC-3km".into(),
            value: nan_or(lr03_table),
        },
        LapseRateRow {
            label: "3-6km".into(),
            value: nan_or(lr36_table),
        },
        LapseRateRow {
            label: "700-500".into(),
            value: opt_or(p.lr75),
        },
        LapseRateRow {
            label: "850-500".into(),
            value: opt_or(p.lr85),
        },
    ];

    let (rm_dir, rm_spd) = crate::profile::comp2vec(p.rstu, p.rstv);
    let (lm_dir, lm_spd) = crate::profile::comp2vec(p.lstu, p.lstv);
    let (cu_dir, cu_spd) = crate::profile::comp2vec(p.corfidi_up_u, p.corfidi_up_v);
    let (cd_dir, cd_spd) = crate::profile::comp2vec(p.corfidi_dn_u, p.corfidi_dn_v);
    let (_, lcl_temp_c) = cape::lcl(p_sfc, profile.tmpc[profile.sfc], profile.dwpc[profile.sfc]);
    let (dgz_bot, dgz_top) = indices::dgz(profile);
    let dgz_rh = indices::mean_relh(profile, Some(dgz_bot), Some(dgz_top)).unwrap_or(f64::NAN);
    let mean_wind_1_35_ms = mean_wind_mag(p1km, p3500m) * 0.514_444;
    let wndg = composites::wndg(p.mlpcl.bplus, lr03_table, mean_wind_1_35_ms, p.mlpcl.bminus)
        .unwrap_or(f64::NAN);
    let shr06_mag = mag(p.shr06.0, p.shr06.1);
    let mean06_mag = mag(p.mean_wind_06.0, p.mean_wind_06.1);
    let dcp =
        composites::dcp(p.dcape.dcape, p.mupcl.bplus, shr06_mag, mean06_mag).unwrap_or(f64::NAN);
    let esp = composites::esp(p.mlpcl.b3km, lr03_table, p.mlpcl.bplus).unwrap_or(f64::NAN);
    let lr38 = indices::lapse_rate(profile, 3000.0, 8000.0, false).unwrap_or(f64::NAN);
    let mean_wind_3_12_ms = mean_wind_mag(p3km, p12km) * 0.514_444;
    let mmp = {
        let max_bulk_shear = max_bulk_shear_0_1_to_6_10_mps(profile);
        if max_bulk_shear.is_finite() && lr38.is_finite() && mean_wind_3_12_ms.is_finite() {
            indices::coniglio(p.mupcl.bplus, max_bulk_shear, lr38, mean_wind_3_12_ms)
        } else {
            f64::NAN
        }
    };
    let down_t = p.dcape.ttrace.last().copied().unwrap_or(f64::NAN);
    let sfc_rh = profile.relh.get(profile.sfc).copied().unwrap_or(f64::NAN);

    ParamTableData {
        parcels,
        shear_layers,
        pw: opt_or(p.precip_water),
        mean_w: opt_or(p.mean_mixr),
        sfc_rh: nan_or(sfc_rh),
        low_rh: opt_or(p.mean_rh_low),
        mid_rh: opt_or(p.mean_rh_mid),
        dgz_rh: nan_or(dgz_rh),
        freezing_level_m: opt_or(p.frz_lvl),
        wb_zero_m: opt_or(p.wb_zero),
        mu_mpl_m: nan_or(p.mupcl.mplhght),
        thetae_diff_3km: opt_or(p.tei),
        lcl_temp_c: nan_or(lcl_temp_c),
        dcape: nan_or(p.dcape.dcape),
        dwn_t: nan_or(down_t),
        k_index: opt_or(p.k_index),
        t_totals: opt_or(p.t_totals),
        tei: opt_or(p.tei),
        tehi: opt_or(p.tehi),
        tts: opt_or(p.tts),
        vtp_mod: opt_or(p.vtp_mod),
        conv_t: opt_or(p.conv_t),
        max_t: opt_or(p.max_temp),
        mmp: nan_or(mmp),
        sig_svr: f64::NAN,
        esp: nan_or(esp),
        wndg: nan_or(wndg),
        dcp: nan_or(dcp),
        lhp: f64::NAN,
        cape_3km: nan_or(p.sfcpcl.b3km),
        lapse_rates,
        bunkers_right: param_table::StormMotion {
            label: "RM".into(),
            direction: rm_dir,
            speed: rm_spd,
        },
        bunkers_left: param_table::StormMotion {
            label: "LM".into(),
            direction: lm_dir,
            speed: lm_spd,
        },
        corfidi_down: param_table::StormMotion {
            label: "DN".into(),
            direction: cd_dir,
            speed: cd_spd,
        },
        corfidi_up: param_table::StormMotion {
            label: "UP".into(),
            direction: cu_dir,
            speed: cu_spd,
        },
        stp_cin: opt_or(p.stp_cin),
        stp_fix: opt_or(p.stp_fixed),
        ship: opt_or(p.ship),
        scp: opt_or(p.scp),
        brn_shear: f64::NAN,
        wind_1km_dir: p.wind_1km.0,
        wind_1km_spd: p.wind_1km.1,
        wind_6km_dir: p.wind_6km.0,
        wind_6km_spd: p.wind_6km.1,
    }
}

fn max_bulk_shear_0_1_to_6_10_mps(profile: &Profile) -> f64 {
    let low = wind_indices_in_layer(profile, 0.0, 1000.0);
    let high = wind_indices_in_layer(profile, 6000.0, 10_000.0);
    let mut max_shear = f64::NAN;
    for &i in &low {
        for &j in &high {
            let du = profile.u[j] - profile.u[i];
            let dv = profile.v[j] - profile.v[i];
            if du.is_finite() && dv.is_finite() {
                let shear = (du * du + dv * dv).sqrt() * 0.514_444;
                if !max_shear.is_finite() || shear > max_shear {
                    max_shear = shear;
                }
            }
        }
    }
    max_shear
}

fn lapse_rate_agl(profile: &Profile, lower_agl: f64, upper_agl: f64) -> f64 {
    let lower_msl = profile.to_msl(lower_agl);
    let upper_msl = profile.to_msl(upper_agl);
    let t_lower = interp_profile_field_by_height(profile, lower_msl, &profile.tmpc);
    let t_upper = interp_profile_field_by_height(profile, upper_msl, &profile.tmpc);
    let dz = upper_msl - lower_msl;
    if t_lower.is_finite() && t_upper.is_finite() && dz.abs() > 1.0 {
        (t_upper - t_lower) / dz * -1000.0
    } else {
        f64::NAN
    }
}

fn interp_profile_field_by_height(profile: &Profile, target_msl: f64, field: &[f64]) -> f64 {
    if !target_msl.is_finite() || field.len() != profile.hght.len() {
        return f64::NAN;
    }
    for i in 0..profile.hght.len().saturating_sub(1) {
        let h0 = profile.hght[i];
        let h1 = profile.hght[i + 1];
        let v0 = field[i];
        let v1 = field[i + 1];
        if !h0.is_finite() || !h1.is_finite() || !v0.is_finite() || !v1.is_finite() {
            continue;
        }
        if (target_msl >= h0 && target_msl <= h1) || (target_msl <= h0 && target_msl >= h1) {
            let dh = h1 - h0;
            if dh.abs() < 1.0e-6 {
                return v0;
            }
            let frac = (target_msl - h0) / dh;
            return v0 + frac * (v1 - v0);
        }
    }
    f64::NAN
}

fn wind_indices_in_layer(profile: &Profile, bottom_agl: f64, top_agl: f64) -> Vec<usize> {
    profile
        .hght
        .iter()
        .enumerate()
        .filter_map(|(index, &height_msl)| {
            let agl = profile.to_agl(height_msl);
            if agl >= bottom_agl
                && agl <= top_agl
                && profile.u[index].is_finite()
                && profile.v[index].is_finite()
            {
                Some(index)
            } else {
                None
            }
        })
        .collect()
}

/// Build SARS data (placeholder — no analog database yet).
fn build_sars_data(_p: &ComputedParams) -> SarsData {
    SarsData {
        supercell: SarsCategory {
            loose_matches: 0,
            quality_matches: 0,
            quality_lines: Vec::new(),
        },
        sgfnt_hail: SarsCategory {
            loose_matches: 0,
            quality_matches: 0,
            quality_lines: Vec::new(),
        },
    }
}

/// Build STP climatology box-plot data (Thompson et al. climatological values).
fn build_stp_climo() -> StpClimatology {
    StpClimatology::standard()
}

/// Build storm slinky points from parcel trace and storm motion.
fn build_slinky(profile: &Profile, p: &ComputedParams) -> Vec<SlinkyPoint> {
    let (storm_u, storm_v) = cyclonic_storm_motion(profile, p.rstu, p.rstv, p.lstu, p.lstv);
    if !storm_u.is_finite() || !storm_v.is_finite() {
        return Vec::new();
    }
    let pcl = &p.sfcpcl;
    if pcl.ptrace.is_empty() {
        return Vec::new();
    }

    let mut points = Vec::new();
    let sfc_h = profile.sfc_height();
    for i in 0..pcl.ptrace.len() {
        let pres = pcl.ptrace[i];
        if !pres.is_finite() || pres <= 0.0 {
            continue;
        }
        let h_msl = profile.interp_hght(pres);
        if !h_msl.is_finite() {
            continue;
        }
        let h_agl = h_msl - sfc_h;
        if h_agl < 0.0 {
            continue;
        }
        // Storm-relative wind at this level
        let (u, v) = profile.interp_wind(pres);
        if !u.is_finite() || !v.is_finite() {
            continue;
        }
        points.push(SlinkyPoint {
            height_m: h_agl,
            sr_u: u - storm_u,
            sr_v: v - storm_v,
        });
    }
    points
}

// =========================================================================
// Canvas helpers — blit and PNG encoding
// =========================================================================

/// Blit a source canvas onto a destination canvas at offset (dx, dy).
fn blit_canvas(dst: &mut Canvas, src: &Canvas, dx: i32, dy: i32) {
    for sy in 0..src.h as i32 {
        for sx in 0..src.w as i32 {
            let si = ((sy as u32) * src.w + (sx as u32)) as usize * 4;
            if si + 3 >= src.pixels.len() {
                continue;
            }
            let tx = dx + sx;
            let ty = dy + sy;
            if tx < 0 || ty < 0 || tx >= dst.w as i32 || ty >= dst.h as i32 {
                continue;
            }
            let di = ((ty as u32) * dst.w + (tx as u32)) as usize * 4;
            // Alpha blend
            let sa = src.pixels[si + 3] as f32 / 255.0;
            let inv = 1.0 - sa;
            dst.pixels[di] = (src.pixels[si] as f32 * sa + dst.pixels[di] as f32 * inv) as u8;
            dst.pixels[di + 1] =
                (src.pixels[si + 1] as f32 * sa + dst.pixels[di + 1] as f32 * inv) as u8;
            dst.pixels[di + 2] =
                (src.pixels[si + 2] as f32 * sa + dst.pixels[di + 2] as f32 * inv) as u8;
            dst.pixels[di + 3] = 255;
        }
    }
}

/// Blit a PixelBuf (from param_table) onto a Canvas at offset (dx, dy).
fn blit_pixelbuf(dst: &mut Canvas, src: &PixelBuf, dx: i32, dy: i32) {
    for sy in 0..src.height as i32 {
        for sx in 0..src.width as i32 {
            let si = (sy as usize * src.width + sx as usize) * 4;
            if si + 3 >= src.data.len() {
                continue;
            }
            let tx = dx + sx;
            let ty = dy + sy;
            if tx < 0 || ty < 0 || tx >= dst.w as i32 || ty >= dst.h as i32 {
                continue;
            }
            let di = ((ty as u32) * dst.w + (tx as u32)) as usize * 4;
            let sa = src.data[si + 3] as f32 / 255.0;
            let inv = 1.0 - sa;
            dst.pixels[di] = (src.data[si] as f32 * sa + dst.pixels[di] as f32 * inv) as u8;
            dst.pixels[di + 1] =
                (src.data[si + 1] as f32 * sa + dst.pixels[di + 1] as f32 * inv) as u8;
            dst.pixels[di + 2] =
                (src.data[si + 2] as f32 * sa + dst.pixels[di + 2] as f32 * inv) as u8;
            dst.pixels[di + 3] = 255;
        }
    }
}

/// Blit raw RGBA pixel data (from skewt::render_skewt) onto a Canvas.
fn blit_raw_rgba(dst: &mut Canvas, src: &[u8], src_w: u32, src_h: u32, dx: i32, dy: i32) {
    for sy in 0..src_h as i32 {
        for sx in 0..src_w as i32 {
            let si = ((sy as u32) * src_w + (sx as u32)) as usize * 4;
            if si + 3 >= src.len() {
                continue;
            }
            let tx = dx + sx;
            let ty = dy + sy;
            if tx < 0 || ty < 0 || tx >= dst.w as i32 || ty >= dst.h as i32 {
                continue;
            }
            let di = ((ty as u32) * dst.w + (tx as u32)) as usize * 4;
            let sa = src[si + 3] as f32 / 255.0;
            let inv = 1.0 - sa;
            dst.pixels[di] = (src[si] as f32 * sa + dst.pixels[di] as f32 * inv) as u8;
            dst.pixels[di + 1] = (src[si + 1] as f32 * sa + dst.pixels[di + 1] as f32 * inv) as u8;
            dst.pixels[di + 2] = (src[si + 2] as f32 * sa + dst.pixels[di + 2] as f32 * inv) as u8;
            dst.pixels[di + 3] = 255;
        }
    }
}

/// Encode a Canvas to PNG bytes.
fn canvas_to_png(c: &Canvas) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut buf);
        use image::ImageEncoder;
        encoder
            .write_image(&c.pixels, c.w, c.h, image::ExtendedColorType::Rgba8)
            .expect("PNG encode");
    }
    buf
}

// =========================================================================
// Main compositor entry point
// =========================================================================

/// Render a complete SHARPpy-style sounding analysis image.
///
/// Returns the PNG file as raw bytes.  The image is 2400x1800 pixels (2×
/// scale for crisp rendering) with a dark background, matching the classic
/// SHARPpy display.
///
/// # Arguments
///
/// * `profile` — the sounding profile to render
/// * `params` — pre-computed parameters (from [`compute_all_params`])
pub fn render_full_sounding(profile: &Profile, params: &ComputedParams) -> Vec<u8> {
    let mut img = Canvas::new(IMG_W, IMG_H, COL_BG);

    // ===================================================================
    // 1. Title bar
    // ===================================================================
    img.fill_rect(0, 0, IMG_W as i32, TITLE_H as i32, COL_TITLE_BG);
    let station = &profile.station.station_id;
    let datetime = &profile.station.datetime;
    let title = if station.is_empty() && datetime.is_empty() {
        "rustwx Sounding Analysis".to_string()
    } else {
        format!("rustwx Sounding Analysis - {} {}", station, datetime)
    };
    img.draw_text_centered(&title, IMG_W as i32 / 2, 12, COL_WHITE);

    // ===================================================================
    // 2. Skew-T diagram (left region, includes omega + barbs internally)
    // ===================================================================
    let skewt_rgba = super::skewt::render_skewt_only(profile, LEFT_W, UPPER_H);
    blit_raw_rgba(&mut img, &skewt_rgba, LEFT_W, UPPER_H, 0, TITLE_H as i32);

    // ===================================================================
    // 3. Hodograph (top-right)
    // ===================================================================
    if let Some(hodo_data) = build_hodo_data(profile) {
        let hodo_canvas = hodograph::render_hodograph(&hodo_data, RIGHT_W, HODO_H);
        blit_canvas(&mut img, &hodo_canvas, LEFT_W as i32, TITLE_H as i32);
    }

    // ===================================================================
    // 4. Right-side diagnostic panels (below hodograph)
    // ===================================================================
    {
        let sars = build_sars_data(params);
        let climo = build_stp_climo();
        let current_stp = params.stp_cin.or(params.stp_fixed).unwrap_or(0.0);
        let slinky_pts = build_slinky(profile, params);

        let panels_x = LEFT_W as i32;
        let panels_y = TITLE_H as i32 + HODO_H as i32;
        let panels_w = RIGHT_W as i32;
        let panels_h = INSET_H as i32;

        panels::draw_all_panels(
            &mut img,
            &sars,
            &climo,
            current_stp,
            None, // STP probabilities not yet computed
            &slinky_pts,
            None, // slinky tilt degrees not yet computed
            params.watch_type,
            &[], // temp advection levels not yet computed
            panels_x,
            panels_y,
            panels_w,
            panels_h,
        );
    }

    // ===================================================================
    // 5. Parameter table (bottom, full width)
    // ===================================================================
    {
        let table_data = build_param_table(profile, params);
        let table_buf = param_table::render_sized(&table_data, IMG_W as usize, TABLE_H as usize);
        blit_pixelbuf(&mut img, &table_buf, 0, (TITLE_H + UPPER_H) as i32);
    }

    // ===================================================================
    // 6. Panel border lines
    // ===================================================================
    // Horizontal separator between upper panels and table
    img.draw_line(
        0,
        (TITLE_H + UPPER_H) as i32,
        IMG_W as i32 - 1,
        (TITLE_H + UPPER_H) as i32,
        COL_BORDER,
    );
    // Vertical separator between skew-T and right panels
    img.draw_line(
        LEFT_W as i32,
        TITLE_H as i32,
        LEFT_W as i32,
        (TITLE_H + UPPER_H) as i32 - 1,
        COL_BORDER,
    );
    // Horizontal separator between hodograph and inset panels
    img.draw_line(
        LEFT_W as i32,
        (TITLE_H + HODO_H) as i32,
        IMG_W as i32 - 1,
        (TITLE_H + HODO_H) as i32,
        COL_BORDER,
    );

    canvas_to_png(&img)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::StationInfo;

    fn test_profile() -> Profile {
        let pres = [1000.0, 925.0, 850.0, 700.0, 500.0, 300.0, 200.0];
        let hght = [100.0, 800.0, 1500.0, 3100.0, 5600.0, 9200.0, 12000.0];
        let tmpc = [30.0, 24.0, 18.0, 4.0, -15.0, -40.0, -55.0];
        let dwpc = [22.0, 18.0, 12.0, -4.0, -30.0, -50.0, -65.0];
        let wdir = [180.0, 200.0, 220.0, 250.0, 270.0, 280.0, 280.0];
        let wspd = [10.0, 15.0, 20.0, 30.0, 50.0, 60.0, 70.0];
        Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &wdir,
            &wspd,
            &[],
            StationInfo {
                station_id: "TST".into(),
                datetime: "250402/0000".into(),
                latitude: 35.0,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn stable_profile_without_effective_inflow() -> Profile {
        let pres = [1000.0, 925.0, 850.0, 700.0, 500.0, 300.0, 200.0];
        let hght = [100.0, 800.0, 1500.0, 3100.0, 5600.0, 9200.0, 12000.0];
        let tmpc = [10.0, 12.0, 10.0, 3.0, -12.0, -38.0, -52.0];
        let dwpc = [-20.0, -24.0, -28.0, -35.0, -45.0, -58.0, -68.0];
        let wdir = [180.0, 195.0, 210.0, 235.0, 260.0, 275.0, 280.0];
        let wspd = [10.0, 15.0, 20.0, 30.0, 45.0, 60.0, 70.0];
        Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &wdir,
            &wspd,
            &[],
            StationInfo {
                station_id: "STABLE".into(),
                latitude: 35.0,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn hemisphere_test_profile() -> Profile {
        // Extend the ordinary renderer fixture far enough above the parcel EL
        // that effective bulk wind difference is available for the severe
        // composites as well as for the mover/helicity checks.
        let pres = [
            1000.0, 925.0, 850.0, 700.0, 500.0, 300.0, 200.0, 150.0, 100.0,
        ];
        let hght = [
            100.0, 800.0, 1500.0, 3100.0, 5600.0, 9200.0, 12000.0, 14000.0, 16500.0,
        ];
        let tmpc = [30.0, 24.0, 18.0, 4.0, -15.0, -40.0, -55.0, -61.0, -67.0];
        let dwpc = [22.0, 18.0, 12.0, -4.0, -30.0, -50.0, -65.0, -71.0, -77.0];
        let wdir = [
            180.0, 200.0, 220.0, 250.0, 270.0, 280.0, 280.0, 285.0, 290.0,
        ];
        let wspd = [10.0, 15.0, 20.0, 30.0, 50.0, 60.0, 70.0, 75.0, 80.0];
        Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &wdir,
            &wspd,
            &[],
            StationInfo {
                station_id: "TST-NH".into(),
                datetime: "250402/0000".into(),
                latitude: 35.0,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn mirrored_southern_profile(north: &Profile) -> Profile {
        let mirrored_u: Vec<f64> = north.u.iter().map(|u| -*u).collect();
        Profile::from_uv(
            &north.pres,
            &north.hght,
            &north.tmpc,
            &north.dwpc,
            &mirrored_u,
            &north.v,
            &north.omeg,
            StationInfo {
                station_id: "TST-SH".into(),
                latitude: -35.0,
                ..north.station.clone()
            },
        )
        .unwrap()
    }

    fn assert_close(actual: f64, expected: f64, label: &str) {
        let tolerance = 1e-8 * expected.abs().max(1.0);
        assert!(
            (actual - expected).abs() <= tolerance,
            "{label}: actual {actual}, expected {expected}, tolerance {tolerance}"
        );
    }

    fn assert_same_value(actual: f64, expected: f64) {
        if actual.is_nan() && expected.is_nan() {
            return;
        }
        assert_eq!(actual, expected);
    }

    fn table_value(value: f64) -> f64 {
        if value.is_finite() {
            value
        } else {
            f64::NAN
        }
    }

    #[test]
    fn compute_params_runs() {
        let prof = test_profile();
        let params = compute_all_params(&prof);
        // CAPE should be a finite number (or NaN for stable profiles).
        assert!(params.sfcpcl.bplus.is_finite() || params.sfcpcl.bplus.is_nan());
        // Storm motion should have been computed.
        assert!(params.rstu.is_finite() || params.rstu.is_nan());
    }

    #[test]
    fn no_effective_layer_keeps_inputs_missing_but_composites_zero() {
        let params = compute_all_params(&stable_profile_without_effective_inflow());
        assert!(!params.eff_inflow.0.is_finite());
        assert!(!params.eff_inflow.1.is_finite());
        assert_eq!(params.effective_srh, None);
        assert_eq!(params.eff_shear, None);
        assert_eq!(params.effective_bwd, None);
        assert_eq!(params.scp, Some(0.0));
        assert_eq!(params.stp_cin, Some(0.0));
        assert_eq!(params.vtp_mod, Some(0.0));
    }

    #[test]
    fn mirrored_southern_profile_uses_left_mover_with_cyclonic_positive_helicity() {
        let north_profile = hemisphere_test_profile();
        let south_profile = mirrored_southern_profile(&north_profile);
        let north = compute_all_params(&north_profile);
        let south = compute_all_params(&south_profile);

        // Reflection across the north-south axis swaps the physical right and
        // left movers. The Southern Hemisphere analysis must select that
        // reflected left mover, not continue using the right mover.
        assert_close(south.lstu, -north.rstu, "active mover u");
        assert_close(south.lstv, north.rstv, "active mover v");
        assert_close(south.srh01.0, north.srh01.0, "0-1 km cyclonic SRH");
        assert_close(south.srh03.0, north.srh03.0, "0-3 km cyclonic SRH");

        let north_esrh = north.effective_srh.expect("north effective SRH");
        let south_esrh = south.effective_srh.expect("south effective SRH");
        assert_close(south_esrh, north_esrh, "effective cyclonic SRH");
        assert_close(
            south.scp.expect("south SCP"),
            north.scp.expect("north SCP"),
            "SCP",
        );
        assert_close(
            south.stp_cin.expect("south STP(CIN)"),
            north.stp_cin.expect("north STP(CIN)"),
            "STP(CIN)",
        );
        assert_close(
            south.stp_fixed.expect("south fixed STP"),
            north.stp_fixed.expect("north fixed STP"),
            "fixed STP",
        );
        assert_close(
            south.vtp_mod.expect("south VTP"),
            north.vtp_mod.expect("north VTP"),
            "VTP",
        );
    }

    #[test]
    fn param_table_prefers_native_parcel_values_over_ecape_bridge() {
        let prof = test_profile();
        let mut params = compute_all_params(&prof);
        params.sfc_ecape = EcapeParcelParams {
            ecape: 1111.0,
            ncape: 0.42,
            cape: 9999.0,
            cinh: -9999.0,
            cape_3km: 9999.0,
            cape_6km: 9999.0,
            lfc_m: 0.0,
            el_m: 9999.0,
        };

        let table = build_param_table(&prof, &params);
        let sfc = table
            .parcels
            .iter()
            .find(|row| row.label == "SFC")
            .expect("SFC row");

        assert_eq!(sfc.ecape, 1111.0);
        assert_eq!(sfc.ncape, 0.42);
        assert_same_value(sfc.cape, table_value(params.sfcpcl.bplus));
        assert_same_value(sfc.cape_3km, table_value(params.sfcpcl.b3km));
        assert_same_value(sfc.cape_6km, table_value(params.sfcpcl.b6km));
        assert_same_value(sfc.cinh, table_value(params.sfcpcl.bminus));
        assert_same_value(sfc.lfc_m, table_value(params.sfcpcl.lfchght));
        assert_same_value(sfc.el_m, table_value(params.sfcpcl.elhght));
    }

    #[test]
    fn render_produces_valid_png() {
        let prof = test_profile();
        let params = compute_all_params(&prof);
        let png = render_full_sounding(&prof, &params);
        // PNG magic bytes: 0x89 P N G
        assert!(png.len() > 1000, "PNG too small: {} bytes", png.len());
        assert_eq!(&png[..4], &[0x89, 0x50, 0x4E, 0x47], "Not a valid PNG");
    }
}
