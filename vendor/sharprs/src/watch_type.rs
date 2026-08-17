//! SPC watch-type classifier — fuzzy-logic system for determining whether
//! atmospheric conditions favour a particular watch or hazard type.
//!
//! Rust port of `sharppy/sharptab/watch_type.py`.
//! Original Python by Greg Blumberg (CIMMS) and Kelton Halbert (OU SoM),
//! with tornado/severe thresholds contributed by Rich Thompson (NOAA SPC).
//!
//! The classifier examines a bundle of pre-computed convective parameters
//! (STP, SCP, SRH, SHIP, etc.) and applies layered threshold logic to
//! produce an ordered list of applicable watch categories.

// ---------------------------------------------------------------------------
// Watch types
// ---------------------------------------------------------------------------

/// Possible watch / hazard categories, ranked roughly by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchType {
    /// Particularly Dangerous Situation — Tornado Watch.
    PdsTornado,
    /// Tornado Watch.
    Tornado,
    /// Marginal Tornado Watch.
    MarginalTornado,
    /// Severe Thunderstorm Watch.
    Severe,
    /// Marginal Severe Thunderstorm Watch.
    MarginalSevere,
    /// Flash Flood Watch.
    FlashFlood,
    /// Blizzard Watch.
    Blizzard,
    /// Excessive Heat Watch.
    ExcessiveHeat,
    /// No significant watch type indicated.
    None,
}

impl WatchType {
    /// Short label matching the original SHARPpy string codes.
    pub fn label(self) -> &'static str {
        match self {
            Self::PdsTornado => "PDS TOR",
            Self::Tornado => "TOR",
            Self::MarginalTornado => "MRGL TOR",
            Self::Severe => "SVR",
            Self::MarginalSevere => "MRGL SVR",
            Self::FlashFlood => "FLASH FLOOD",
            Self::Blizzard => "BLIZZARD",
            Self::ExcessiveHeat => "EXCESSIVE HEAT",
            Self::None => "NONE",
        }
    }
}

// ---------------------------------------------------------------------------
// Heat index (needed for excessive-heat logic)
// ---------------------------------------------------------------------------

/// Compute the NWS heat index (°F) from temperature (°F) and relative
/// humidity (%).  Uses the Steadman (1979) regression with NWS adjustments.
pub fn heat_index(temp_f: f64, rh: f64) -> f64 {
    if temp_f < 40.0 {
        return temp_f;
    }

    let hi_simple = 0.5 * (temp_f + 61.0 + (temp_f - 68.0) * 1.2 + rh * 0.094);
    let avg = (hi_simple + temp_f) / 2.0;
    if avg < 80.0 {
        return hi_simple;
    }

    let t = temp_f;
    let mut hi = -42.379 + 2.04901523 * t + 10.14333127 * rh
        - 0.22475541 * t * rh
        - 6.83783e-3 * t * t
        - 5.481717e-2 * rh * rh
        + 1.22874e-3 * rh * t * t
        + 8.5282e-4 * t * rh * rh
        - 1.99e-6 * rh * rh * t * t;

    if rh < 13.0 && t > 80.0 && t < 112.0 {
        let adj = ((13.0 - rh) / 4.0) * ((17.0 - (t - 95.0).abs()) / 17.0).sqrt();
        hi -= adj;
    } else if rh > 85.0 && t > 80.0 && t < 87.0 {
        let adj = ((rh - 85.0) / 10.0) * ((87.0 - t) / 5.0);
        hi += adj;
    }

    hi
}

/// Wind chill (°F) from surface temperature (°F) and wind speed (mph).
///
/// Uses the NWS formula: <https://www.weather.gov/safety/cold-wind-chill-chart>
pub fn wind_chill(temp_f: f64, wspd_mph: f64) -> f64 {
    35.74 + 0.6215 * temp_f - 35.75 * wspd_mph.powf(0.16) + 0.4275 * temp_f * wspd_mph.powf(0.16)
}

// ---------------------------------------------------------------------------
// Convective parameter bundle
// ---------------------------------------------------------------------------

/// All the pre-computed convective / sounding-derived parameters needed by
/// the watch-type classifier.
///
/// Field names mirror the SHARPpy `ConvectiveProfile` attributes.  For a
/// southern-hemisphere profile the caller should negate the helicity and
/// STP fields before passing them in (the Python code does this inline).
#[derive(Debug, Clone)]
pub struct WatchParams {
    // --- Tornado parameters ---
    /// Effective-layer Significant Tornado Parameter (with CIN constraint).
    pub stp_eff: f64,
    /// Fixed-layer STP.
    pub stp_fixed: f64,
    /// 4–6 km storm-relative wind speed (kts).
    pub srw_4_6km: f64,
    /// Effective-layer storm-relative helicity (m² s⁻²).
    pub esrh: f64,
    /// 0–1 km storm-relative helicity (m² s⁻²).
    pub srh1km: f64,
    /// 0–8 km bulk shear magnitude (kts).
    pub sfc_8km_shear: f64,
    /// 0–1 km lapse rate (°C km⁻¹).
    pub lr1: f64,

    // --- Parcel parameters ---
    /// Surface-based parcel LCL height (m AGL).
    pub sfcpcl_lclhght: f64,
    /// Mixed-layer parcel LCL height (m AGL).
    pub mlpcl_lclhght: f64,
    /// Mixed-layer parcel CINH (J kg⁻¹, negative = inhibition).
    pub mlpcl_bminus: f64,
    /// Most-unstable parcel CINH (J kg⁻¹).
    pub mupcl_bminus: f64,
    /// Effective-layer bottom pressure (mb, 0 = surface-based).
    pub ebotm: f64,

    // --- Severe / supercell parameters ---
    /// Supercell Composite Parameter.
    pub scp: f64,
    /// Significant Hail Parameter.
    pub ship: f64,
    /// Significant severe parameter (kt·J/kg product).
    pub sig_severe: f64,
    /// Mesoscale convective potential (MMP).
    pub mmp: f64,
    /// Wind damage parameter (WNDG).
    pub wndg: f64,
    /// DCAPE (J kg⁻¹).
    pub dcape: f64,

    // --- Moisture ---
    /// Precipitable water (inches).
    pub pwat: f64,
    /// Precipitable-water climatology flag (0=below normal … 3=well above).
    pub pwv_flag: u8,
    /// Low-level (0–3 km) mean RH (%).
    pub low_rh: f64,
    /// Mid-level (3–6 km or similar) mean RH (%).
    pub mid_rh: f64,
    /// Upshear (cloud-layer) mean wind speed (kts).
    pub upshear_wspd: f64,

    // --- Surface observations ---
    /// Surface temperature (°C).
    pub sfc_tmpc: f64,
    /// Surface dewpoint (°C).
    pub sfc_dwpc: f64,
    /// Surface pressure (mb).
    pub sfc_pres: f64,
    /// Surface wind speed (kts).
    pub sfc_wspd_kts: f64,

    // --- Precipitation type (from init_phase) ---
    /// Best-guess precip type string (e.g. "Snow", "Rain", …).
    pub precip_type: String,
}

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

/// Determine the set of applicable watch / hazard types for a given
/// convective environment.
///
/// The returned vector is ordered by severity (most severe first) and always
/// ends with [`WatchType::None`], matching the SHARPpy convention.
///
/// # Logic overview
///
/// 1. **PDS TOR** — extreme STP (≥ 3), SRH (≥ 200), 0–8 km shear (> 45 kt),
///    low LCL, steep low-level lapse rates, minimal CIN, surface-based.
/// 2. **TOR** — high STP (≥ 3 eff or ≥ 4 fixed) *or* moderate STP (≥ 1)
///    combined with strong shear, high RH, or favourable CIN.
/// 3. **MRGL TOR** — STP ≥ 1 with weaker support, or STP ≥ 0.5 with SRH ≥ 150.
/// 4. **SVR** — high SCP (≥ 4) or STP (≥ 1), or SCP ≥ 2 plus hail/DCAPE,
///    or sig-severe × MMP product.
/// 5. **MRGL SVR** — moderate SCP/SHIP/WNDG with relaxed CIN.
/// 6. **FLASH FLOOD** — above-normal PWAT and slow upshear flow.
/// 7. **BLIZZARD** — surface wind > 35 mph, T ≤ 0 °C, snow precip type.
/// 8. **EXCESSIVE HEAT** — heat index > 105 °F.
pub fn possible_watch(p: &WatchParams) -> Vec<WatchType> {
    let mut types: Vec<WatchType> = Vec::new();

    // -----------------------------------------------------------------------
    // Tornado logic
    // -----------------------------------------------------------------------
    if p.stp_eff >= 3.0
        && p.stp_fixed >= 3.0
        && p.srh1km >= 200.0
        && p.esrh >= 200.0
        && p.srw_4_6km >= 15.0
        && p.sfc_8km_shear > 45.0
        && p.sfcpcl_lclhght < 1000.0
        && p.mlpcl_lclhght < 1200.0
        && p.lr1 >= 5.0
        && p.mlpcl_bminus > -50.0
        && p.ebotm == 0.0
    {
        types.push(WatchType::PdsTornado);
    } else if (p.stp_eff >= 3.0 || p.stp_fixed >= 4.0) && p.mlpcl_bminus > -125.0 && p.ebotm == 0.0
    {
        types.push(WatchType::Tornado);
    } else if (p.stp_eff >= 1.0 || p.stp_fixed >= 1.0)
        && (p.srw_4_6km >= 15.0 || p.sfc_8km_shear >= 40.0)
        && p.mlpcl_bminus > -50.0
        && p.ebotm == 0.0
    {
        types.push(WatchType::Tornado);
    } else if (p.stp_eff >= 1.0 || p.stp_fixed >= 1.0)
        && ((p.low_rh + p.mid_rh) / 2.0 >= 60.0)
        && p.lr1 >= 5.0
        && p.mlpcl_bminus > -50.0
        && p.ebotm == 0.0
    {
        types.push(WatchType::Tornado);
    } else if (p.stp_eff >= 1.0 || p.stp_fixed >= 1.0) && p.mlpcl_bminus > -150.0 && p.ebotm == 0.0
    {
        types.push(WatchType::MarginalTornado);
    } else if ((p.stp_eff >= 0.5 && p.esrh >= 150.0) || (p.stp_fixed >= 0.5 && p.srh1km >= 150.0))
        && p.mlpcl_bminus > -50.0
        && p.ebotm == 0.0
    {
        // NOTE: The original Python has an operator-precedence subtlety here
        // (the `and`/`or` grouping).  We follow the *intended* logic: either
        // of the two STP+SRH combos, combined with the CIN/ebotp checks.
        types.push(WatchType::MarginalTornado);
    }

    // -----------------------------------------------------------------------
    // Severe thunderstorm logic
    // -----------------------------------------------------------------------
    if (p.stp_fixed >= 1.0 || p.scp >= 4.0 || p.stp_eff >= 1.0) && p.mupcl_bminus >= -50.0 {
        types.push(WatchType::Severe);
    } else if p.scp >= 2.0 && (p.ship >= 1.0 || p.dcape >= 750.0) && p.mupcl_bminus >= -50.0 {
        types.push(WatchType::Severe);
    } else if p.sig_severe >= 30000.0 && p.mmp >= 0.6 && p.mupcl_bminus >= -50.0 {
        types.push(WatchType::Severe);
    } else if p.mupcl_bminus >= -75.0 && (p.wndg >= 0.5 || p.ship >= 0.5 || p.scp >= 0.5) {
        types.push(WatchType::MarginalSevere);
    }

    // -----------------------------------------------------------------------
    // Flash flood
    // -----------------------------------------------------------------------
    if p.pwv_flag >= 2 && p.upshear_wspd < 25.0 {
        types.push(WatchType::FlashFlood);
    }

    // -----------------------------------------------------------------------
    // Blizzard
    // -----------------------------------------------------------------------
    let sfc_wspd_mph = p.sfc_wspd_kts * 1.15078;
    if sfc_wspd_mph > 35.0 && p.sfc_tmpc <= 0.0 && p.precip_type.contains("Snow") {
        types.push(WatchType::Blizzard);
    }

    // -----------------------------------------------------------------------
    // Excessive heat
    // -----------------------------------------------------------------------
    let sfc_tmpf = p.sfc_tmpc * 9.0 / 5.0 + 32.0;
    let rh = {
        let es = 6.112 * ((17.67 * p.sfc_tmpc) / (p.sfc_tmpc + 243.5)).exp();
        let e = 6.112 * ((17.67 * p.sfc_dwpc) / (p.sfc_dwpc + 243.5)).exp();
        (e / es * 100.0).clamp(0.0, 100.0)
    };
    let hi = heat_index(sfc_tmpf, rh);
    if hi > 105.0 {
        types.push(WatchType::ExcessiveHeat);
    }

    // Always append NONE as the final entry
    types.push(WatchType::None);

    types
}

/// Convenience: return just the highest-priority (most severe) watch type.
pub fn best_watch(p: &WatchParams) -> WatchType {
    let types = possible_watch(p);
    // The first element is the most severe; the last is always NONE.
    types.into_iter().next().unwrap_or(WatchType::None)
}

// ---------------------------------------------------------------------------
// Precipitation-type support (init_phase / best_guess helpers)
// ---------------------------------------------------------------------------

/// Initial precipitation phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrecipPhase {
    /// Warm — rain.
    Rain,
    /// Near-freezing — freezing rain or ZR/snow mix.
    FreezingOrMix,
    /// Cold — snow.
    Snow,
    /// No precipitation source found.
    None,
}

impl PrecipPhase {
    /// Numeric code matching SHARPpy: 0=Rain, 1=FreezingOrMix, 3=Snow, -1=None.
    pub fn code(self) -> i8 {
        match self {
            Self::Rain => 0,
            Self::FreezingOrMix => 1,
            Self::Snow => 3,
            Self::None => -1,
        }
    }
}

/// Classify initial precipitation phase from the temperature at the
/// precipitation source level.
///
/// This is a standalone version of the temperature-threshold logic from
/// `init_phase()`.  Identifying the source level itself requires the full
/// profile, but once the source-level temperature is known this function
/// applies the same breakpoints.
pub fn classify_precip_phase(source_temp_c: f64) -> (PrecipPhase, &'static str) {
    if source_temp_c > 0.0 {
        (PrecipPhase::Rain, "Rain")
    } else if source_temp_c > -5.0 {
        (PrecipPhase::FreezingOrMix, "Freezing Rain")
    } else if source_temp_c > -9.0 {
        (PrecipPhase::FreezingOrMix, "ZR/S Mix")
    } else {
        (PrecipPhase::Snow, "Snow")
    }
}

/// Best-guess surface precipitation type given the initial precipitation
/// phase, source-level metadata, warm/cold layer areas, and surface
/// temperature.
///
/// Matches the logic in `best_guess_precip()` from `watch_type.py`.
///
/// # Arguments
///
/// * `phase` – initial precipitation phase.
/// * `init_temp_c` – temperature at the precipitation source level (°C).
/// * `init_lvl_agl_m` – height AGL of the precipitation source (m).
/// * `tpos` – positive (warm-layer) area in the temperature profile (J kg⁻¹).
/// * `tneg` – negative (cold-layer) area in the temperature profile (J kg⁻¹,
///   typically ≤ 0).
/// * `sfc_tmpc` – surface temperature (°C).
pub fn best_guess_precip(
    phase: PrecipPhase,
    init_temp_c: f64,
    init_lvl_agl_m: f64,
    tpos: f64,
    tneg: f64,
    sfc_tmpc: f64,
) -> &'static str {
    match phase {
        // No precip
        PrecipPhase::None => "None.",

        // Always too warm → Rain
        PrecipPhase::Rain if tneg >= 0.0 && sfc_tmpc > 0.0 => "Rain.",

        // ZR too warm at surface → Rain
        PrecipPhase::FreezingOrMix if tpos <= 0.0 && sfc_tmpc > 0.0 => "Rain.",

        // Non-snow init, always too cold → Sleet/FZ variants
        PrecipPhase::FreezingOrMix if tpos <= 0.0 && sfc_tmpc <= 0.0 => {
            if init_lvl_agl_m >= 3000.0 {
                if init_temp_c <= -4.0 {
                    "Sleet and Snow."
                } else {
                    "Sleet."
                }
            } else {
                "Freezing Rain/Drizzle."
            }
        }

        // Always too cold → Snow
        PrecipPhase::Snow if tpos <= 0.0 && sfc_tmpc <= 0.0 => "Snow.",

        // Snow aloft but warm at surface
        PrecipPhase::Snow if tpos <= 0.0 && sfc_tmpc > 0.0 => {
            if sfc_tmpc > 4.0 {
                "Rain."
            } else {
                "Snow."
            }
        }

        // Warm layer present
        _ if tpos > 0.0 => {
            let x1 = tpos;
            let y1 = -tneg;
            let y2 = 0.62 * x1 + 60.0;
            if y1 > y2 {
                "Sleet."
            } else if sfc_tmpc <= 0.0 {
                "Freezing Rain."
            } else {
                "Rain."
            }
        }

        _ => "Unknown.",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a default WatchParams with values that should produce no watches.
    fn quiet_params() -> WatchParams {
        WatchParams {
            stp_eff: 0.0,
            stp_fixed: 0.0,
            srw_4_6km: 5.0,
            esrh: 50.0,
            srh1km: 50.0,
            sfc_8km_shear: 15.0,
            lr1: 4.0,
            sfcpcl_lclhght: 2000.0,
            mlpcl_lclhght: 2200.0,
            mlpcl_bminus: -10.0,
            mupcl_bminus: -10.0,
            ebotm: 0.0,
            scp: 0.2,
            ship: 0.1,
            sig_severe: 5000.0,
            mmp: 0.1,
            wndg: 0.1,
            dcape: 200.0,
            pwat: 0.8,
            pwv_flag: 0,
            low_rh: 50.0,
            mid_rh: 40.0,
            upshear_wspd: 30.0,
            sfc_tmpc: 20.0,
            sfc_dwpc: 10.0,
            sfc_pres: 1013.0,
            sfc_wspd_kts: 5.0,
            precip_type: String::new(),
        }
    }

    // --- Heat index ---

    #[test]
    fn heat_index_below_threshold() {
        assert_eq!(heat_index(30.0, 50.0), 30.0);
    }

    #[test]
    fn heat_index_simple_regime() {
        let hi = heat_index(75.0, 50.0);
        assert!(hi > 70.0 && hi < 80.0, "got {hi}");
    }

    #[test]
    fn heat_index_full_regression() {
        let hi = heat_index(100.0, 60.0);
        // Should be well above 100
        assert!(hi > 110.0, "got {hi}");
    }

    #[test]
    fn heat_index_low_rh_adjustment() {
        // RH < 13, T in 80–112 → adjustment subtracts
        let hi = heat_index(95.0, 10.0);
        assert!(hi > 80.0, "got {hi}");
    }

    #[test]
    fn heat_index_high_rh_adjustment() {
        // RH > 85, T in 80–87 → adjustment adds
        let hi = heat_index(83.0, 90.0);
        assert!(hi > 80.0, "got {hi}");
    }

    // --- Wind chill ---

    #[test]
    fn wind_chill_basic() {
        let wc = wind_chill(0.0, 15.0);
        assert!(wc < 0.0, "got {wc}");
    }

    // --- Watch types: quiet day ---

    #[test]
    fn quiet_day_only_none() {
        let p = quiet_params();
        let types = possible_watch(&p);
        assert_eq!(types, vec![WatchType::None]);
    }

    // --- PDS TOR ---

    #[test]
    fn pds_tornado() {
        let mut p = quiet_params();
        p.stp_eff = 5.0;
        p.stp_fixed = 5.0;
        p.srh1km = 300.0;
        p.esrh = 300.0;
        p.srw_4_6km = 20.0;
        p.sfc_8km_shear = 50.0;
        p.sfcpcl_lclhght = 800.0;
        p.mlpcl_lclhght = 900.0;
        p.lr1 = 6.0;
        p.mlpcl_bminus = -20.0;
        p.mupcl_bminus = -20.0;
        p.ebotm = 0.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::PdsTornado));
    }

    // --- TOR ---

    #[test]
    fn tornado_high_stp() {
        let mut p = quiet_params();
        p.stp_eff = 4.0;
        p.stp_fixed = 2.0;
        p.mlpcl_bminus = -50.0;
        p.ebotm = 0.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::Tornado));
    }

    #[test]
    fn tornado_stp_plus_shear() {
        let mut p = quiet_params();
        p.stp_eff = 1.5;
        p.srw_4_6km = 20.0;
        p.mlpcl_bminus = -30.0;
        p.ebotm = 0.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::Tornado));
    }

    #[test]
    fn tornado_stp_plus_rh() {
        let mut p = quiet_params();
        p.stp_eff = 1.5;
        p.low_rh = 70.0;
        p.mid_rh = 65.0;
        p.lr1 = 6.0;
        p.mlpcl_bminus = -30.0;
        p.ebotm = 0.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::Tornado));
    }

    // --- MRGL TOR ---

    #[test]
    fn marginal_tornado() {
        let mut p = quiet_params();
        p.stp_eff = 1.5;
        p.mlpcl_bminus = -100.0;
        p.srw_4_6km = 5.0;
        p.sfc_8km_shear = 20.0;
        p.low_rh = 30.0;
        p.mid_rh = 30.0;
        p.ebotm = 0.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::MarginalTornado));
    }

    #[test]
    fn marginal_tornado_stp_half_esrh() {
        let mut p = quiet_params();
        p.stp_eff = 0.6;
        p.esrh = 160.0;
        p.mlpcl_bminus = -30.0;
        p.ebotm = 0.0;
        // Make sure we don't hit an earlier TOR branch
        p.stp_fixed = 0.3;
        p.srw_4_6km = 5.0;
        p.sfc_8km_shear = 20.0;
        p.low_rh = 30.0;
        p.mid_rh = 30.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::MarginalTornado));
    }

    // --- SVR ---

    #[test]
    fn severe_high_scp() {
        let mut p = quiet_params();
        p.scp = 5.0;
        p.mupcl_bminus = -30.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::Severe));
    }

    #[test]
    fn severe_scp_ship() {
        let mut p = quiet_params();
        p.scp = 3.0;
        p.ship = 1.5;
        p.mupcl_bminus = -30.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::Severe));
    }

    #[test]
    fn severe_sig_severe_mmp() {
        let mut p = quiet_params();
        p.sig_severe = 40000.0;
        p.mmp = 0.8;
        p.mupcl_bminus = -30.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::Severe));
    }

    // --- MRGL SVR ---

    #[test]
    fn marginal_severe() {
        let mut p = quiet_params();
        p.wndg = 0.6;
        p.mupcl_bminus = -60.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::MarginalSevere));
    }

    // --- FLASH FLOOD ---

    #[test]
    fn flash_flood() {
        let mut p = quiet_params();
        p.pwv_flag = 3;
        p.upshear_wspd = 10.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::FlashFlood));
    }

    #[test]
    fn no_flash_flood_fast_upshear() {
        let mut p = quiet_params();
        p.pwv_flag = 3;
        p.upshear_wspd = 30.0;
        let types = possible_watch(&p);
        assert!(!types.contains(&WatchType::FlashFlood));
    }

    // --- BLIZZARD ---

    #[test]
    fn blizzard() {
        let mut p = quiet_params();
        p.sfc_wspd_kts = 35.0; // ~40 mph
        p.sfc_tmpc = -5.0;
        p.precip_type = "Snow.".to_string();
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::Blizzard));
    }

    // --- EXCESSIVE HEAT ---

    #[test]
    fn excessive_heat() {
        let mut p = quiet_params();
        p.sfc_tmpc = 38.0;
        p.sfc_dwpc = 26.0;
        let types = possible_watch(&p);
        assert!(types.contains(&WatchType::ExcessiveHeat));
    }

    // --- best_watch ---

    #[test]
    fn best_watch_returns_most_severe() {
        let mut p = quiet_params();
        p.stp_eff = 5.0;
        p.stp_fixed = 5.0;
        p.srh1km = 300.0;
        p.esrh = 300.0;
        p.srw_4_6km = 20.0;
        p.sfc_8km_shear = 50.0;
        p.sfcpcl_lclhght = 800.0;
        p.mlpcl_lclhght = 900.0;
        p.lr1 = 6.0;
        p.mlpcl_bminus = -20.0;
        p.mupcl_bminus = -20.0;
        p.ebotm = 0.0;
        assert_eq!(best_watch(&p), WatchType::PdsTornado);
    }

    // --- Precip phase ---

    #[test]
    fn precip_phase_warm() {
        let (phase, label) = classify_precip_phase(5.0);
        assert_eq!(phase, PrecipPhase::Rain);
        assert_eq!(label, "Rain");
    }

    #[test]
    fn precip_phase_cold() {
        let (phase, label) = classify_precip_phase(-15.0);
        assert_eq!(phase, PrecipPhase::Snow);
        assert_eq!(label, "Snow");
    }

    #[test]
    fn precip_phase_zr() {
        let (phase, _) = classify_precip_phase(-3.0);
        assert_eq!(phase, PrecipPhase::FreezingOrMix);
    }

    // --- Best guess precip ---

    #[test]
    fn best_guess_rain() {
        let result = best_guess_precip(PrecipPhase::Rain, 5.0, 2000.0, 0.0, 0.0, 10.0);
        assert_eq!(result, "Rain.");
    }

    #[test]
    fn best_guess_snow() {
        let result = best_guess_precip(PrecipPhase::Snow, -15.0, 3000.0, 0.0, 0.0, -5.0);
        assert_eq!(result, "Snow.");
    }

    #[test]
    fn best_guess_freezing_rain_drizzle() {
        let result = best_guess_precip(PrecipPhase::FreezingOrMix, -3.0, 1500.0, 0.0, 0.0, -2.0);
        assert_eq!(result, "Freezing Rain/Drizzle.");
    }

    #[test]
    fn best_guess_sleet_high_source() {
        let result = best_guess_precip(PrecipPhase::FreezingOrMix, -6.0, 4000.0, 0.0, 0.0, -2.0);
        assert_eq!(result, "Sleet and Snow.");
    }

    #[test]
    fn best_guess_warm_layer_sleet() {
        // tpos > 0, large negative area → sleet
        let result = best_guess_precip(PrecipPhase::Rain, 2.0, 2000.0, 100.0, -200.0, 1.0);
        assert_eq!(result, "Sleet.");
    }

    #[test]
    fn best_guess_warm_layer_freezing_rain() {
        // tpos > 0, small negative area, sfc ≤ 0 → freezing rain
        let result = best_guess_precip(PrecipPhase::Rain, 2.0, 2000.0, 100.0, -50.0, -1.0);
        assert_eq!(result, "Freezing Rain.");
    }

    #[test]
    fn best_guess_no_precip() {
        let result = best_guess_precip(PrecipPhase::None, 0.0, 0.0, 0.0, 0.0, 0.0);
        assert_eq!(result, "None.");
    }

    // --- Labels ---

    #[test]
    fn watch_type_labels() {
        assert_eq!(WatchType::PdsTornado.label(), "PDS TOR");
        assert_eq!(WatchType::Tornado.label(), "TOR");
        assert_eq!(WatchType::MarginalTornado.label(), "MRGL TOR");
        assert_eq!(WatchType::Severe.label(), "SVR");
        assert_eq!(WatchType::MarginalSevere.label(), "MRGL SVR");
        assert_eq!(WatchType::FlashFlood.label(), "FLASH FLOOD");
        assert_eq!(WatchType::Blizzard.label(), "BLIZZARD");
        assert_eq!(WatchType::ExcessiveHeat.label(), "EXCESSIVE HEAT");
        assert_eq!(WatchType::None.label(), "NONE");
    }
}
