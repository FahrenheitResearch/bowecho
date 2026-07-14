//! Friendly names + color-family hints for raw WRF store fields.
//!
//! BowEcho's WRF imports write two kinds of 2-D store variables: canonical
//! fields with readable names (`temperature_2m`, `sbcape`, …) and RAW
//! passthrough fields named `wrf_{lowercased wrfout name}` — the light import
//! stores every `[Time, south_north, west_east]` variable that way (~119 on a
//! standard wrfout), and the heavy path prefixes every wrf-core diagnostic it
//! has no canonical slug for. Those raw names (`wrf_swupt`, `wrf_swdnbc`, …)
//! are WRF Registry mnemonics most users don't recognize, and none of them
//! resolved a default palette.
//!
//! This module is the single source of truth mapping each raw store name to:
//! * a short DISPLAY label (units stay appended by the picker, so labels
//!   carry none) — applied at display time in the Model Data dock, so
//!   existing stores benefit without re-import;
//! * a one-line description for hover/tooltip surfaces;
//! * a color-family hint the Solar resolver
//!   ([`crate::solar_model_field_table`]) turns into an EXISTING ramp.
//!
//! Every label contains at least one character outside `[a-z0-9_]` (store
//! names are sanitized slugs), so a label can never collide with a real store
//! variable name — the display-time rename is safely invertible. Tests below
//! enforce that, plus key/label uniqueness.

/// Color-family hint for one raw WRF field. Semantic families resolve to the
/// unit-aware Solarpower07 tables; the parameterized families reuse existing
/// BowEcho ramps rescaled over a physically sensible range (no new palettes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WrfColorFamily {
    /// Solar temperature (unit-aware K/°C/°F).
    Temperature,
    /// Solar dew point (unit-aware).
    Dewpoint,
    /// Solar relative humidity (0–100 %).
    RelativeHumidity,
    /// Solar wind speed (unit-aware kt / m/s / mph).
    WindSpeed,
    /// Solar accumulated-precip depth (unit-aware mm/in).
    PrecipDepth,
    /// Solar "PW Style" reflectivity (dBZ).
    Reflectivity,
    /// Solar CAPE (J/kg).
    Cape,
    /// Solar composite severe ramp over 0..600 (helicity/SRH convention).
    Helicity,
    /// Analyst MEHS hail-size ladder (mm).
    HailSize,
    /// Analyst Probability ramp over 0..100 (percent-valued fields).
    Percent,
    /// Analyst Probability ramp rescaled over 0..1 (fraction-valued fields).
    Fraction,
    /// Solar composite severe ramp over 0..`vmax` (dimensionless composites).
    Composite { vmax: f32 },
    /// Analyst Generic sequential ramp rescaled over `lo..hi`.
    Sequential { lo: f32, hi: f32 },
    /// Balance (CVD-safe) diverging ramp rescaled to ±`max_abs` — signed
    /// fluxes and vector components, neutral at zero.
    Diverging { max_abs: f32 },
    /// No existing ramp fits (categories, buckets, run-length accumulations):
    /// keep the caller's fallback (range-normalized generic).
    Unassigned,
}

/// One raw WRF store field: display label, hover description, family hint.
#[derive(Clone, Copy, Debug)]
pub struct WrfFieldInfo {
    /// Short display label. Units are appended by the picker — labels carry
    /// none. Always contains a non-slug character (see module docs).
    pub label: &'static str,
    /// One-line description for hover/tooltip surfaces.
    pub description: &'static str,
    /// Default-palette family hint.
    pub family: WrfColorFamily,
}

const fn info(
    label: &'static str,
    description: &'static str,
    family: WrfColorFamily,
) -> WrfFieldInfo {
    WrfFieldInfo {
        label,
        description,
        family,
    }
}

use WrfColorFamily as F;

/// The catalog: raw store name (exact, lowercase) → field info. Keys are the
/// `wrf_*` slugs BOTH import paths write (`local_import`'s sanitized raw
/// names and `wrf_process::derived_name`'s prefixed diagnostics use the same
/// scheme). Canonical store names (`temperature_2m`, `sbcape`, …) are
/// deliberately absent — they are already readable and already styled.
#[rustfmt::skip]
static WRF_FIELD_CATALOG: &[(&str, WrfFieldInfo)] = &[
    // ── Radiation budget — instantaneous (W m-2) ────────────────────────────
    ("wrf_swdown",  info("Shortwave down surface (SWDOWN)", "Downward solar flux reaching the ground", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_swnorm",  info("Shortwave down slope-normal", "Downward solar flux normal to the terrain slope", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_gsw",     info("Shortwave net, surface", "Net solar flux absorbed at the ground", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_glw",     info("Longwave down surface (GLW)", "Downward thermal (infrared) flux at the ground", F::Sequential { lo: 100.0, hi: 500.0 })),
    ("wrf_olr",     info("Longwave up TOA (OLR)", "Outgoing longwave radiation at the top of the atmosphere — cold cloud tops read low", F::Sequential { lo: 80.0, hi: 340.0 })),
    ("wrf_swupt",   info("Shortwave up TOA — reflected solar", "Solar flux reflected back out the top of the atmosphere", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_swuptc",  info("Shortwave up TOA (clear-sky)", "Reflected solar at the top of the atmosphere assuming no clouds", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_swdnt",   info("Shortwave down TOA — incoming solar", "Incoming solar flux at the top of the atmosphere", F::Sequential { lo: 0.0, hi: 1400.0 })),
    ("wrf_swdntc",  info("Shortwave down TOA (clear-sky)", "Incoming solar at the top of the atmosphere, clear-sky", F::Sequential { lo: 0.0, hi: 1400.0 })),
    ("wrf_swupb",   info("Shortwave up surface — reflected", "Solar flux reflected by the ground (albedo × downward SW)", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_swupbc",  info("Shortwave up surface (clear-sky)", "Reflected solar at the ground assuming no clouds", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_swdnb",   info("Shortwave down surface", "Downward solar flux at the ground (all-sky)", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_swdnbc",  info("Shortwave down surface (clear-sky)", "Downward solar flux at the ground assuming no clouds", F::Sequential { lo: 0.0, hi: 1100.0 })),
    ("wrf_lwupt",   info("Longwave up TOA", "Thermal flux leaving the top of the atmosphere (all-sky OLR)", F::Sequential { lo: 80.0, hi: 340.0 })),
    ("wrf_lwuptc",  info("Longwave up TOA (clear-sky)", "Thermal flux leaving the top of the atmosphere assuming no clouds", F::Sequential { lo: 80.0, hi: 340.0 })),
    ("wrf_lwdnt",   info("Longwave down TOA", "Downward thermal flux at the top of the atmosphere (near zero)", F::Unassigned)),
    ("wrf_lwdntc",  info("Longwave down TOA (clear-sky)", "Downward thermal flux at the top of the atmosphere, clear-sky", F::Unassigned)),
    ("wrf_lwupb",   info("Longwave up surface", "Thermal flux emitted upward by the ground (~σT⁴)", F::Sequential { lo: 200.0, hi: 650.0 })),
    ("wrf_lwupbc",  info("Longwave up surface (clear-sky)", "Thermal flux emitted upward by the ground, clear-sky", F::Sequential { lo: 200.0, hi: 650.0 })),
    ("wrf_lwdnb",   info("Longwave down surface", "Downward thermal flux at the ground (all-sky, ≈ GLW)", F::Sequential { lo: 100.0, hi: 500.0 })),
    ("wrf_lwdnbc",  info("Longwave down surface (clear-sky)", "Downward thermal flux at the ground assuming no clouds", F::Sequential { lo: 100.0, hi: 500.0 })),
    // ── Radiation budget — accumulated (J m-2; range grows with sim time) ──
    ("wrf_acswupt",  info("Shortwave up TOA (accum.)", "Time-integrated reflected solar at the top of the atmosphere", F::Unassigned)),
    ("wrf_acswuptc", info("Shortwave up TOA clear-sky (accum.)", "Time-integrated clear-sky reflected solar at TOA", F::Unassigned)),
    ("wrf_acswdnt",  info("Shortwave down TOA (accum.)", "Time-integrated incoming solar at the top of the atmosphere", F::Unassigned)),
    ("wrf_acswdntc", info("Shortwave down TOA clear-sky (accum.)", "Time-integrated clear-sky incoming solar at TOA", F::Unassigned)),
    ("wrf_acswupb",  info("Shortwave up surface (accum.)", "Time-integrated solar flux reflected by the ground", F::Unassigned)),
    ("wrf_acswupbc", info("Shortwave up surface clear-sky (accum.)", "Time-integrated clear-sky reflected solar at the ground", F::Unassigned)),
    ("wrf_acswdnb",  info("Shortwave down surface (accum.)", "Time-integrated downward solar flux at the ground", F::Unassigned)),
    ("wrf_acswdnbc", info("Shortwave down surface clear-sky (accum.)", "Time-integrated clear-sky downward solar at the ground", F::Unassigned)),
    ("wrf_aclwupt",  info("Longwave up TOA (accum.)", "Time-integrated outgoing thermal flux at TOA", F::Unassigned)),
    ("wrf_aclwuptc", info("Longwave up TOA clear-sky (accum.)", "Time-integrated clear-sky outgoing thermal flux at TOA", F::Unassigned)),
    ("wrf_aclwdnt",  info("Longwave down TOA (accum.)", "Time-integrated downward thermal flux at TOA", F::Unassigned)),
    ("wrf_aclwdntc", info("Longwave down TOA clear-sky (accum.)", "Time-integrated clear-sky downward thermal flux at TOA", F::Unassigned)),
    ("wrf_aclwupb",  info("Longwave up surface (accum.)", "Time-integrated thermal flux emitted by the ground", F::Unassigned)),
    ("wrf_aclwupbc", info("Longwave up surface clear-sky (accum.)", "Time-integrated clear-sky thermal flux emitted by the ground", F::Unassigned)),
    ("wrf_aclwdnb",  info("Longwave down surface (accum.)", "Time-integrated downward thermal flux at the ground", F::Unassigned)),
    ("wrf_aclwdnbc", info("Longwave down surface clear-sky (accum.)", "Time-integrated clear-sky downward thermal at the ground", F::Unassigned)),
    // ── Surface radiative properties ────────────────────────────────────────
    ("wrf_albedo",  info("Albedo", "Surface shortwave reflectance (0–1)", F::Fraction)),
    ("wrf_albbck",  info("Background albedo", "Snow-free background surface albedo", F::Fraction)),
    ("wrf_snoalb",  info("Max snow albedo", "Maximum albedo the surface takes when snow-covered", F::Fraction)),
    ("wrf_emiss",   info("Surface emissivity", "Longwave emissivity of the surface", F::Fraction)),
    // ── Surface energy & moisture fluxes ────────────────────────────────────
    ("wrf_hfx",     info("Sensible heat flux (HFX)", "Upward sensible heat flux at the surface — positive heats the air", F::Diverging { max_abs: 700.0 })),
    ("wrf_lh",      info("Latent heat flux (LH)", "Upward latent-heat (evaporation) flux at the surface", F::Diverging { max_abs: 700.0 })),
    ("wrf_grdflx",  info("Ground heat flux (GRDFLX)", "Heat conducted into the soil (WRF sign: negative into the ground)", F::Diverging { max_abs: 300.0 })),
    ("wrf_qfx",     info("Surface moisture flux (QFX)", "Upward water-vapor mass flux at the surface", F::Diverging { max_abs: 4.0e-4 })),
    ("wrf_achfx",   info("Sensible heat flux (accum.)", "Time-integrated surface sensible heat flux", F::Unassigned)),
    ("wrf_aclhf",   info("Latent heat flux (accum.)", "Time-integrated surface latent heat flux", F::Unassigned)),
    ("wrf_noahres", info("Noah LSM energy residual", "Surface energy-balance residual of the Noah land model", F::Diverging { max_abs: 50.0 })),
    ("wrf_flhc",    info("Sfc exchange coeff — heat", "Surface exchange coefficient for heat", F::Unassigned)),
    ("wrf_flqc",    info("Sfc exchange coeff — moisture", "Surface exchange coefficient for moisture", F::Unassigned)),
    ("wrf_canwat",  info("Canopy water", "Liquid water intercepted on the vegetation canopy", F::PrecipDepth)),
    // ── Surface meteorology ─────────────────────────────────────────────────
    ("wrf_t2",      info("2 m temperature (T2)", "Diagnosed air temperature 2 m above ground", F::Temperature)),
    ("wrf_th2",     info("2 m potential temperature", "Potential temperature 2 m above ground", F::Temperature)),
    ("wrf_td2",     info("2 m dew point (TD2)", "Diagnosed dew point 2 m above ground", F::Dewpoint)),
    ("wrf_q2",      info("2 m mixing ratio (Q2)", "Water-vapor mixing ratio 2 m above ground", F::Sequential { lo: 0.0, hi: 0.025 })),
    ("wrf_psfc",    info("Surface pressure (PSFC)", "Full pressure at the surface", F::Unassigned)),
    ("wrf_u10",     info("10 m U wind (U10)", "Grid-relative west–east wind component at 10 m", F::Diverging { max_abs: 30.0 })),
    ("wrf_v10",     info("10 m V wind (V10)", "Grid-relative south–north wind component at 10 m", F::Diverging { max_abs: 30.0 })),
    ("wrf_tsk",     info("Skin temperature (TSK)", "Radiative surface (skin) temperature", F::Temperature)),
    ("wrf_sst",     info("Sea-surface temperature", "Sea-surface temperature", F::Temperature)),
    ("wrf_sstsk",   info("Sea-surface skin temperature", "Diurnally varying sea-surface skin temperature", F::Temperature)),
    ("wrf_tmn",     info("Deep-soil temperature", "Lower-boundary (deep-layer) soil temperature", F::Temperature)),
    // ── Precipitation accumulations ─────────────────────────────────────────
    ("wrf_rainc",      info("Rain — cumulus scheme (accum.)", "Run-total convective (cumulus-scheme) rainfall", F::PrecipDepth)),
    ("wrf_rainnc",     info("Precip — grid-scale (accum.)", "Run-total resolved (microphysics) precipitation", F::PrecipDepth)),
    ("wrf_rainsh",     info("Rain — shallow cumulus (accum.)", "Run-total shallow-convective rainfall", F::PrecipDepth)),
    ("wrf_snownc",     info("Snow + ice (accum. LWE)", "Run-total grid-scale snow and ice, liquid-water equivalent", F::PrecipDepth)),
    ("wrf_graupelnc",  info("Graupel (accum. LWE)", "Run-total grid-scale graupel, liquid-water equivalent", F::PrecipDepth)),
    ("wrf_hailnc",     info("Hail (accum. LWE)", "Run-total grid-scale hail, liquid-water equivalent", F::PrecipDepth)),
    ("wrf_prec_acc_c", info("Rain — cumulus (bucket)", "Convective precip over the output bucket interval", F::PrecipDepth)),
    ("wrf_prec_acc_nc", info("Precip — grid-scale (bucket)", "Grid-scale precip over the output bucket interval", F::PrecipDepth)),
    ("wrf_snow_acc_nc", info("Snow (bucket LWE)", "Grid-scale snowfall over the output bucket interval", F::PrecipDepth)),
    ("wrf_i_rainc",    info("Rain bucket count — cumulus", "Bucket-tip counter for convective rain (RAINC = bucket × count + remainder)", F::Unassigned)),
    ("wrf_i_rainnc",   info("Rain bucket count — grid-scale", "Bucket-tip counter for grid-scale precip", F::Unassigned)),
    // ── Snow & ice state ────────────────────────────────────────────────────
    ("wrf_snow",    info("Snow water equivalent", "Snowpack water content (liquid-water equivalent)", F::PrecipDepth)),
    ("wrf_snowh",   info("Snow depth", "Physical snowpack depth", F::Sequential { lo: 0.0, hi: 2.0 })),
    ("wrf_snowc",   info("Snow cover flag", "1 where the ground is snow-covered", F::Fraction)),
    ("wrf_acsnow",  info("Snowfall (accum. LWE)", "Run-total snowfall, liquid-water equivalent", F::PrecipDepth)),
    ("wrf_acsnom",  info("Snow melt (accum.)", "Run-total melted snow", F::PrecipDepth)),
    ("wrf_sr",      info("Frozen precip fraction", "Fraction of precipitation falling frozen", F::Fraction)),
    ("wrf_seaice",  info("Sea-ice fraction", "Sea-ice coverage fraction", F::Fraction)),
    ("wrf_xicem",   info("Sea-ice fraction (prev. step)", "Sea-ice coverage from the previous timestep", F::Fraction)),
    // ── Boundary layer & surface layer ──────────────────────────────────────
    ("wrf_pblh",    info("PBL height", "Boundary-layer depth above ground", F::Sequential { lo: 0.0, hi: 3000.0 })),
    ("wrf_ust",     info("Friction velocity u★", "Surface-layer momentum flux scale", F::Sequential { lo: 0.0, hi: 1.5 })),
    ("wrf_znt",     info("Roughness length z₀", "Aerodynamic roughness length of the surface", F::Sequential { lo: 0.0, hi: 2.0 })),
    ("wrf_mol",     info("Surface-layer T★", "Monin–Obukhov temperature scale", F::Diverging { max_abs: 2.0 })),
    ("wrf_rmol",    info("Inverse Obukhov length", "1/L — surface-layer stability (positive = stable)", F::Diverging { max_abs: 0.1 })),
    ("wrf_regime",  info("Surface-layer regime", "Stability regime category (1–4)", F::Unassigned)),
    ("wrf_hgt",     info("Terrain height (HGT)", "Model terrain elevation", F::Sequential { lo: 0.0, hi: 3500.0 })),
    ("wrf_wspd10max", info("Max 10 m wind speed", "Peak 10 m wind speed since the last output time", F::WindSpeed)),
    // ── Land use, vegetation, soil & water bodies ───────────────────────────
    ("wrf_landmask", info("Land mask", "1 = land, 0 = water", F::Fraction)),
    ("wrf_lakemask", info("Lake mask", "1 = lake, 0 elsewhere", F::Fraction)),
    ("wrf_xland",   info("Land–water mask", "1 = land, 2 = water", F::Unassigned)),
    ("wrf_lu_index", info("Land-use category", "Dominant land-use class index", F::Unassigned)),
    ("wrf_isltyp",  info("Soil type category", "Dominant soil texture class index", F::Unassigned)),
    ("wrf_ivgtyp",  info("Vegetation category", "Dominant vegetation class index", F::Unassigned)),
    ("wrf_vegfra",  info("Vegetation fraction", "Green-vegetation coverage (0–100)", F::Percent)),
    ("wrf_shdmax",  info("Max annual veg fraction", "Annual maximum green-vegetation coverage", F::Percent)),
    ("wrf_shdmin",  info("Min annual veg fraction", "Annual minimum green-vegetation coverage", F::Percent)),
    ("wrf_lai",     info("Leaf area index", "One-sided leaf area per ground area", F::Sequential { lo: 0.0, hi: 7.0 })),
    ("wrf_smstav",  info("Soil moisture availability", "Relative soil moisture availability (0–1)", F::Fraction)),
    ("wrf_smstot",  info("Total soil moisture", "Column-total soil moisture", F::Unassigned)),
    ("wrf_sfroff",  info("Surface runoff (accum.)", "Run-total surface runoff", F::PrecipDepth)),
    ("wrf_udroff",  info("Subsurface runoff (accum.)", "Run-total underground runoff", F::PrecipDepth)),
    ("wrf_mu",      info("Dry-air mass perturbation (MU)", "Perturbation dry-air mass in the column", F::Diverging { max_abs: 5000.0 })),
    ("wrf_mub",     info("Dry-air mass base state (MUB)", "Base-state dry-air mass in the column", F::Unassigned)),
    ("wrf_var",     info("Orographic variance", "Subgrid terrain height variance", F::Unassigned)),
    ("wrf_var_sso", info("Subgrid orography variance", "Terrain variance for the gravity-wave drag scheme", F::Unassigned)),
    ("wrf_lake_depth", info("Lake depth", "Prescribed lake depth", F::Sequential { lo: 0.0, hi: 60.0 })),
    ("wrf_uoce",    info("Ocean current U", "West–east ocean surface current", F::Diverging { max_abs: 2.0 })),
    ("wrf_voce",    info("Ocean current V", "South–north ocean surface current", F::Diverging { max_abs: 2.0 })),
    // ── Convective diagnostic maxima (output_diagnostics wrfouts) ───────────
    ("wrf_refd_max",    info("Max reflectivity (derived)", "Column-max simulated reflectivity since the last output time", F::Reflectivity)),
    ("wrf_up_heli_max", info("Max updraft helicity 2–5 km", "Peak 2–5 km updraft helicity since the last output — supercell tracks", F::Helicity)),
    ("wrf_up_heli_min", info("Min updraft helicity (anticyclonic)", "Most-negative updraft helicity since the last output", F::Diverging { max_abs: 400.0 })),
    ("wrf_w_up_max",    info("Max updraft speed", "Peak column updraft speed since the last output", F::Sequential { lo: 0.0, hi: 40.0 })),
    ("wrf_w_dn_max",    info("Max downdraft speed", "Peak column downdraft speed since the last output", F::Sequential { lo: 0.0, hi: 25.0 })),
    ("wrf_grpl_max",    info("Max column graupel", "Peak column-integrated graupel since the last output", F::Sequential { lo: 0.0, hi: 50.0 })),
    ("wrf_hail_maxk1",  info("Max hail size — level 1", "NSSL microphysics max hail diameter at the lowest model level", F::HailSize)),
    ("wrf_hail_max2d",  info("Max hail size — column", "NSSL microphysics max hail diameter in the column", F::HailSize)),
    // ── Heavy-path wrf-core diagnostics (wrf_-prefixed derived fields) ──────
    ("wrf_tv2m",        info("2 m virtual temperature", "Virtual temperature 2 m above ground", F::Temperature)),
    ("wrf_wspd10",      info("10 m wind speed", "Wind speed 10 m above ground", F::WindSpeed)),
    ("wrf_wdir10",      info("10 m wind direction", "Wind direction 10 m above ground (degrees)", F::Unassigned)),
    ("wrf_uvmet10_u",   info("10 m U wind (earth-relative)", "Earth-relative west–east wind at 10 m", F::Diverging { max_abs: 30.0 })),
    ("wrf_uvmet10_v",   info("10 m V wind (earth-relative)", "Earth-relative south–north wind at 10 m", F::Diverging { max_abs: 30.0 })),
    ("wrf_srh",         info("Storm-relative helicity", "Storm-relative helicity", F::Helicity)),
    ("wrf_bunkers_rm_u", info("Bunkers right-mover U", "Bunkers right-moving supercell motion, U component", F::Diverging { max_abs: 30.0 })),
    ("wrf_bunkers_rm_v", info("Bunkers right-mover V", "Bunkers right-moving supercell motion, V component", F::Diverging { max_abs: 30.0 })),
    ("wrf_bunkers_lm_u", info("Bunkers left-mover U", "Bunkers left-moving supercell motion, U component", F::Diverging { max_abs: 30.0 })),
    ("wrf_bunkers_lm_v", info("Bunkers left-mover V", "Bunkers left-moving supercell motion, V component", F::Diverging { max_abs: 30.0 })),
    ("wrf_ctt",         info("Cloud-top temperature", "Temperature at cloud top — cold tops mark deep convection", F::Temperature)),
    ("wrf_cloudfrac_low",  info("Cloud fraction — low", "Low-level cloud cover", F::Percent)),
    ("wrf_cloudfrac_mid",  info("Cloud fraction — mid", "Mid-level cloud cover", F::Percent)),
    ("wrf_cloudfrac_high", info("Cloud fraction — high", "High-level cloud cover", F::Percent)),
    ("wrf_sb3cape",     info("SBCAPE 0–3 km", "Surface-based CAPE below 3 km", F::Cape)),
    ("wrf_ml3cape",     info("MLCAPE 0–3 km", "Mixed-layer CAPE below 3 km", F::Cape)),
    ("wrf_mu3cape",     info("MUCAPE 0–3 km", "Most-unstable CAPE below 3 km", F::Cape)),
    ("wrf_sb6cape",     info("SBCAPE 0–6 km", "Surface-based CAPE below 6 km", F::Cape)),
    ("wrf_ml6cape",     info("MLCAPE 0–6 km", "Mixed-layer CAPE below 6 km", F::Cape)),
    ("wrf_mu6cape",     info("MUCAPE 0–6 km", "Most-unstable CAPE below 6 km", F::Cape)),
    ("wrf_cape",        info("CAPE (generic)", "Convective available potential energy", F::Cape)),
    ("wrf_cin",         info("CIN (generic)", "Convective inhibition", F::Unassigned)),
    ("wrf_ncape",       info("Analytic NCAPE", "NCAPE paired with the standard analytic ECAPE calculation", F::Sequential { lo: 0.0, hi: 0.5 })),
    ("wrf_effective_cape", info("Effective-layer CAPE", "CAPE of the effective inflow layer", F::Cape)),
    ("wrf_ecape",       info("Analytic ECAPE", "Standard Peters-style analytic ECAPE", F::Cape)),
    ("wrf_ecape_cape",  info("Entraining parcel CAPE", "CAPE integrated along the explicit entraining parcel path", F::Cape)),
    ("wrf_ecape_cin",   info("Entraining parcel CIN", "CIN integrated along the explicit entraining parcel path", F::Unassigned)),
    ("wrf_effective_srh", info("Effective-layer SRH", "Storm-relative helicity of the effective inflow layer", F::Helicity)),
    ("wrf_ebwd",        info("Effective bulk wind difference", "Bulk shear across the effective inflow layer", F::WindSpeed)),
    ("wrf_bulk_shear",  info("Bulk shear (generic)", "Bulk wind difference magnitude", F::WindSpeed)),
    ("wrf_critical_angle", info("Critical angle", "Angle between storm-relative inflow and the 0–500 m shear vector", F::Unassigned)),
    ("wrf_lapse_rate",  info("Lapse rate (generic)", "Temperature lapse rate", F::Sequential { lo: 4.0, hi: 10.0 })),
    ("wrf_lapse_rate_700_500", info("Lapse rate 700–500 mb", "Mid-level temperature lapse rate — steep is destabilizing", F::Sequential { lo: 4.0, hi: 10.0 })),
    ("wrf_lapse_rate_0_3km",   info("Lapse rate 0–3 km", "Low-level temperature lapse rate", F::Sequential { lo: 4.0, hi: 10.0 })),
    ("wrf_freezing_level", info("Freezing-level height", "Height of the 0 °C level", F::Sequential { lo: 0.0, hi: 6000.0 })),
    ("wrf_wet_bulb_0",  info("Wet-bulb zero height", "Height of the wet-bulb 0 °C level — hail melting depth", F::Sequential { lo: 0.0, hi: 6000.0 })),
    ("wrf_k_index",     info("K index", "Airmass thunderstorm potential index", F::Sequential { lo: 15.0, hi: 45.0 })),
    ("wrf_total_totals", info("Total totals index", "Severe-weather airmass index", F::Sequential { lo: 40.0, hi: 65.0 })),
    ("wrf_mean_mixr",   info("Mean mixing ratio (low levels)", "Mean low-level water-vapor mixing ratio", F::Unassigned)),
    ("wrf_low_rh",      info("RH — low levels", "Mean relative humidity of the lower troposphere", F::RelativeHumidity)),
    ("wrf_mid_rh",      info("RH — mid levels", "Mean relative humidity of the mid troposphere", F::RelativeHumidity)),
    ("wrf_dgz_rh",      info("RH — dendritic growth zone", "Relative humidity in the −12…−18 °C snow-growth layer", F::RelativeHumidity)),
    ("wrf_lcl_temp",    info("LCL temperature", "Temperature at the lifting condensation level", F::Temperature)),
    ("wrf_convective_temp", info("Convective temperature", "Surface temperature needed for free convection", F::Temperature)),
    ("wrf_max_temp",    info("Forecast max temperature", "Diagnosed afternoon maximum temperature", F::Temperature)),
    ("wrf_fosberg",     info("Fosberg fire-weather index", "Fire-weather index from temperature, RH and wind", F::Sequential { lo: 0.0, hi: 100.0 })),
    ("wrf_haines",      info("Haines index", "Lower-atmosphere fire-growth index (2–6)", F::Sequential { lo: 2.0, hi: 6.0 })),
    ("wrf_hdw",         info("Hot-dry-windy index", "Fire-weather index from heat, dryness and wind", F::Sequential { lo: 0.0, hi: 500.0 })),
    ("wrf_ship",        info("Significant hail parameter", "Composite parameter for ≥2 in hail environments", F::Composite { vmax: 4.0 })),
    ("wrf_bri",         info("Bulk Richardson number", "Buoyancy-to-shear ratio — supercells favored 10–50", F::Sequential { lo: 0.0, hi: 100.0 })),
    ("wrf_dcp",         info("Derecho composite parameter", "Composite parameter for derecho-producing MCS environments", F::Composite { vmax: 4.0 })),
    ("wrf_wndg",        info("Wind damage parameter (WNDG)", "Composite parameter for convective wind damage potential", F::Composite { vmax: 2.0 })),
    ("wrf_esp",         info("Enhanced stretching potential", "Composite parameter for low-level stretching (landspout) potential", F::Composite { vmax: 2.0 })),
    ("wrf_mmp",         info("MCS maintenance probability", "Probability an existing MCS persists (0–1)", F::Fraction)),
    ("wrf_effective_inflow_base", info("Effective inflow base", "Height of the effective inflow layer base", F::Sequential { lo: 0.0, hi: 4000.0 })),
    ("wrf_effective_inflow_top",  info("Effective inflow top", "Height of the effective inflow layer top", F::Sequential { lo: 0.0, hi: 6000.0 })),
];

/// The full catalog, for iteration (tests, pickers).
pub fn wrf_field_catalog() -> &'static [(&'static str, WrfFieldInfo)] {
    WRF_FIELD_CATALOG
}

/// Info for a raw WRF store variable name (case-insensitive exact match), or
/// `None` for canonical/unknown names — the caller keeps its existing
/// behavior, so genuinely unknown fields pass through unchanged.
pub fn wrf_field_info(store_var: &str) -> Option<&'static WrfFieldInfo> {
    // Store names are already lowercase slugs; avoid allocating for them.
    let lowered;
    let name = if store_var.bytes().any(|b| b.is_ascii_uppercase()) {
        lowered = store_var.to_ascii_lowercase();
        lowered.as_str()
    } else {
        store_var
    };
    WRF_FIELD_CATALOG
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, info)| info)
}

/// Display label for a raw WRF store variable, or `None` when the name is
/// canonical/unknown (display it as-is).
pub fn wrf_display_label(store_var: &str) -> Option<&'static str> {
    wrf_field_info(store_var).map(|info| info.label)
}

/// Inverse of [`wrf_display_label`]: the store variable a display label names
/// (exact match). Labels always contain a non-slug character, so a real store
/// name can never round-trip through here by accident.
pub fn wrf_store_name_for_label(label: &str) -> Option<&'static str> {
    WRF_FIELD_CATALOG
        .iter()
        .find(|(_, info)| info.label == label)
        .map(|(key, _)| *key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_store_slug_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
    }

    #[test]
    fn catalog_keys_are_unique_lowercase_wrf_slugs() {
        let mut seen = std::collections::HashSet::new();
        for (key, _) in wrf_field_catalog() {
            assert!(
                key.starts_with("wrf_") && key.chars().all(is_store_slug_char),
                "{key}: catalog keys must be lowercase wrf_-prefixed store slugs"
            );
            assert!(seen.insert(*key), "{key}: duplicate catalog key");
        }
    }

    /// Collision guard both ways: no two raw names share a label (the
    /// display-time rename must stay invertible), and no label is itself a
    /// valid store slug (so a label can never shadow a real store variable).
    #[test]
    fn labels_are_unique_and_never_valid_store_names() {
        let mut seen = std::collections::HashSet::new();
        for (key, info) in wrf_field_catalog() {
            assert!(
                seen.insert(info.label),
                "{key}: label {:?} maps from two raw names",
                info.label
            );
            assert!(
                info.label.chars().any(|c| !is_store_slug_char(c)),
                "{key}: label {:?} is a valid store slug — could shadow a real variable",
                info.label
            );
            assert!(
                !info.description.is_empty(),
                "{key}: description must not be empty"
            );
        }
    }

    #[test]
    fn label_lookup_round_trips_the_whole_catalog() {
        for (key, info) in wrf_field_catalog() {
            assert_eq!(wrf_display_label(key), Some(info.label), "{key}");
            assert_eq!(wrf_store_name_for_label(info.label), Some(*key), "{key}");
        }
    }

    #[test]
    fn lookup_covers_a_sample_from_every_family_and_is_case_insensitive() {
        // (var, expected family) — one probe per family variant.
        let probes: &[(&str, WrfColorFamily)] = &[
            ("wrf_t2", F::Temperature),
            ("wrf_td2", F::Dewpoint),
            ("wrf_low_rh", F::RelativeHumidity),
            ("wrf_wspd10", F::WindSpeed),
            ("wrf_rainnc", F::PrecipDepth),
            ("wrf_refd_max", F::Reflectivity),
            ("wrf_sb3cape", F::Cape),
            ("wrf_up_heli_max", F::Helicity),
            ("wrf_hail_maxk1", F::HailSize),
            ("wrf_vegfra", F::Percent),
            ("wrf_albedo", F::Fraction),
            ("wrf_ship", F::Composite { vmax: 4.0 }),
            (
                "wrf_swupt",
                F::Sequential {
                    lo: 0.0,
                    hi: 1100.0,
                },
            ),
            ("wrf_hfx", F::Diverging { max_abs: 700.0 }),
            ("wrf_psfc", F::Unassigned),
        ];
        for (var, family) in probes {
            let info = wrf_field_info(var).unwrap_or_else(|| panic!("{var} missing"));
            assert_eq!(info.family, *family, "{var}");
            // The light import lowercases, but defend the API anyway.
            let upper = var.to_ascii_uppercase();
            assert_eq!(
                wrf_field_info(&upper).map(|info| info.label),
                Some(info.label),
                "{var}: uppercase lookup"
            );
        }
    }

    #[test]
    fn ecape_fields_name_the_quantity_the_wrf_bridge_stores() {
        let analytic = wrf_field_info("wrf_ecape").expect("wrf_ecape catalog entry");
        assert_eq!(analytic.label, "Analytic ECAPE");
        assert!(analytic.description.contains("analytic ECAPE"));

        let path_cape = wrf_field_info("wrf_ecape_cape").expect("wrf_ecape_cape catalog entry");
        assert_eq!(path_cape.label, "Entraining parcel CAPE");
        assert!(
            path_cape
                .description
                .contains("explicit entraining parcel path")
        );

        let path_cin = wrf_field_info("wrf_ecape_cin").expect("wrf_ecape_cin catalog entry");
        assert_eq!(path_cin.label, "Entraining parcel CIN");
        assert!(
            path_cin
                .description
                .contains("explicit entraining parcel path")
        );
    }

    /// Genuinely unknown names — raw passthroughs the catalog does not cover
    /// and every canonical store name — must return `None`, keeping their
    /// existing display and palette behavior byte-for-byte.
    #[test]
    fn unknown_and_canonical_names_pass_through() {
        for var in [
            "wrf_some_experimental_field",
            "wrf_",
            "temperature_2m",
            "sbcape",
            "composite_reflectivity",
            "apcp",
            "srh_0_1km",
            "",
        ] {
            assert_eq!(wrf_field_info(var).map(|i| i.label), None, "{var}");
            assert_eq!(wrf_display_label(var), None, "{var}");
        }
        assert_eq!(wrf_store_name_for_label("temperature_2m"), None);
        assert_eq!(wrf_store_name_for_label("wrf_swupt"), None);
    }
}
