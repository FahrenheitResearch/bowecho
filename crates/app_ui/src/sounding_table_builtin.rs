//! Built-in registry and resolver for user-configurable sounding tables.
//!
//! Stable IDs live here rather than in the renderer. The persisted editor
//! stores those IDs; this adapter resolves them from the already-computed
//! SHARP profile/derived state and hands sharppyrs display-ready rows.

use eframe::egui::Color32;
use sharppyrs::{
    DiagnosticTableBoard, DiagnosticTablePanel, DiagnosticTablePanelKind, DiagnosticTableRow,
    DiagnosticTableSection, NativeDiagnosticPatch, NativeDiagnosticPatchBoard, Parcel,
};

use crate::formula_sounding::FormulaSoundingDiagnostic;
use crate::sounding_table_config::{
    SoundingDiagnosticOption, SoundingDiagnosticRef, SoundingTableConfig, SoundingTablePanelConfig,
    SoundingTablePanelId, SoundingTableSection as ConfigSection, SoundingTableSlot,
};

use super::SharppyAnalysis;

const YELLOW: Color32 = Color32::from_rgb(0xff, 0xff, 0x00);
const RED: Color32 = Color32::from_rgb(0xff, 0x00, 0x00);
const PINK: Color32 = Color32::from_rgb(0xff, 0x00, 0xff);
const CYAN: Color32 = Color32::from_rgb(0x00, 0xff, 0xff);
const GREEN: Color32 = Color32::from_rgb(0x00, 0xff, 0x00);
const ORANGE: Color32 = Color32::from_rgb(0xff, 0xa5, 0x00);
const AMBER: Color32 = Color32::from_rgb(0xc8, 0x91, 0x1f);
const BLUE: Color32 = Color32::from_rgb(0x33, 0x99, 0xff);

#[derive(Clone, Copy)]
struct BuiltInSpec {
    id: &'static str,
    label: &'static str,
    category: &'static str,
    unit: &'static str,
}

macro_rules! spec {
    ($id:literal, $label:literal, $category:literal, $unit:literal) => {
        BuiltInSpec {
            id: $id,
            label: $label,
            category: $category,
            unit: $unit,
        }
    };
}

/// Every scalar/text readout exposed by the three canonical table panels.
fn specs() -> &'static [BuiltInSpec] {
    &[
        spec!("parcel.sfc.cape", "SFC CAPE", "Parcels", "J/kg"),
        spec!("parcel.sfc.cinh", "SFC CINH", "Parcels", "J/kg"),
        spec!("parcel.sfc.lcl", "SFC LCL", "Parcels", "m AGL"),
        spec!("parcel.sfc.li", "SFC LI", "Parcels", "°C"),
        spec!("parcel.sfc.lfc", "SFC LFC", "Parcels", "m AGL"),
        spec!("parcel.sfc.el", "SFC EL", "Parcels", "m AGL"),
        spec!("parcel.sfc.mpl", "SFC MPL", "Parcels", "m AGL"),
        spec!("parcel.ml.cape", "ML CAPE", "Parcels", "J/kg"),
        spec!("parcel.ml.cinh", "ML CINH", "Parcels", "J/kg"),
        spec!("parcel.ml.lcl", "ML LCL", "Parcels", "m AGL"),
        spec!("parcel.ml.li", "ML LI", "Parcels", "°C"),
        spec!("parcel.ml.lfc", "ML LFC", "Parcels", "m AGL"),
        spec!("parcel.ml.el", "ML EL", "Parcels", "m AGL"),
        spec!("parcel.ml.mpl", "ML MPL", "Parcels", "m AGL"),
        spec!("parcel.fcst.cape", "FCST CAPE", "Parcels", "J/kg"),
        spec!("parcel.fcst.cinh", "FCST CINH", "Parcels", "J/kg"),
        spec!("parcel.fcst.lcl", "FCST LCL", "Parcels", "m AGL"),
        spec!("parcel.fcst.li", "FCST LI", "Parcels", "°C"),
        spec!("parcel.fcst.lfc", "FCST LFC", "Parcels", "m AGL"),
        spec!("parcel.fcst.el", "FCST EL", "Parcels", "m AGL"),
        spec!("parcel.fcst.mpl", "FCST MPL", "Parcels", "m AGL"),
        spec!("parcel.mu.cape", "MU CAPE", "Parcels", "J/kg"),
        spec!("parcel.mu.cinh", "MU CINH", "Parcels", "J/kg"),
        spec!("parcel.mu.lcl", "MU LCL", "Parcels", "m AGL"),
        spec!("parcel.mu.li", "MU LI", "Parcels", "°C"),
        spec!("parcel.mu.lfc", "MU LFC", "Parcels", "m AGL"),
        spec!("parcel.mu.el", "MU EL", "Parcels", "m AGL"),
        spec!("parcel.mu.mpl", "MU MPL", "Parcels", "m AGL"),
        spec!("thermo.pwat", "PWAT", "Thermodynamics", "in"),
        spec!("thermo.mean_mixr", "MeanW", "Thermodynamics", "g/kg"),
        spec!("thermo.low_rh", "LowRH", "Thermodynamics", "%"),
        spec!("thermo.mid_rh", "MidRH", "Thermodynamics", "%"),
        spec!("thermo.dcape", "DCAPE", "Thermodynamics", "J/kg"),
        spec!("thermo.downrush_temp", "DownT", "Thermodynamics", "°F"),
        spec!("thermo.k_index", "K", "Thermodynamics", ""),
        spec!("thermo.total_totals", "TT", "Thermodynamics", ""),
        spec!("thermo.convective_temp", "ConvT", "Thermodynamics", "°F"),
        spec!("thermo.max_temp", "MaxT", "Thermodynamics", "°F"),
        spec!("thermo.esp", "ESP", "Thermodynamics", ""),
        spec!("thermo.mmp", "MMP", "Thermodynamics", ""),
        spec!("thermo.wndg", "WNDG", "Thermodynamics", ""),
        spec!("thermo.tei", "TEI", "Thermodynamics", ""),
        spec!("thermo.cape_0_3km", "3CAPE", "Thermodynamics", "J/kg"),
        spec!("thermo.cape_0_6km", "6CAPE", "Thermodynamics", "J/kg"),
        spec!("thermo.mburst", "MBURST", "Thermodynamics", ""),
        spec!("thermo.sig_severe", "SigSvr", "Thermodynamics", "m³/s³"),
        spec!(
            "thermo.thetae_diff",
            "Theta-E Difference",
            "Thermodynamics",
            "°C"
        ),
        spec!("lapse.sfc_500m", "SFC-500m LR", "Lapse rates", "°C/km"),
        spec!("lapse.sfc_1km", "SFC-1km LR", "Lapse rates", "°C/km"),
        spec!("lapse.sfc_3km", "SFC-3km LR", "Lapse rates", "°C/km"),
        spec!("lapse.3_6km", "3-6km LR", "Lapse rates", "°C/km"),
        spec!("lapse.850_500", "850-500 LR", "Lapse rates", "°C/km"),
        spec!("lapse.700_500", "700-500 LR", "Lapse rates", "°C/km"),
        spec!("composite.scp_right", "Supercell Comp", "Composites", ""),
        spec!(
            "composite.scp_left",
            "Left Supercell Comp",
            "Composites",
            ""
        ),
        spec!("composite.stp_effective", "STP(cin)", "Composites", ""),
        spec!("composite.stp_fixed", "STP(fix)", "Composites", ""),
        spec!("composite.ship", "SHIP", "Composites", ""),
        spec!("composite.dcp", "Derecho Comp", "Composites", ""),
        spec!(
            "kin.sfc_500m.srh",
            "SFC-500m SRH",
            "Layer kinematics",
            "m²/s²"
        ),
        spec!(
            "kin.sfc_1km.srh",
            "SFC-1km SRH",
            "Layer kinematics",
            "m²/s²"
        ),
        spec!(
            "kin.sfc_3km.srh",
            "SFC-3km SRH",
            "Layer kinematics",
            "m²/s²"
        ),
        spec!(
            "kin.effective.srh",
            "Effective SRH",
            "Layer kinematics",
            "m²/s²"
        ),
        spec!(
            "kin.sfc_500m.shear",
            "SFC-500m Shear",
            "Layer kinematics",
            "kt"
        ),
        spec!(
            "kin.sfc_1km.shear",
            "SFC-1km Shear",
            "Layer kinematics",
            "kt"
        ),
        spec!(
            "kin.sfc_3km.shear",
            "SFC-3km Shear",
            "Layer kinematics",
            "kt"
        ),
        spec!(
            "kin.effective.shear",
            "Effective Inflow Shear",
            "Layer kinematics",
            "kt"
        ),
        spec!(
            "kin.sfc_6km.shear",
            "SFC-6km Shear",
            "Layer kinematics",
            "kt"
        ),
        spec!(
            "kin.sfc_8km.shear",
            "SFC-8km Shear",
            "Layer kinematics",
            "kt"
        ),
        spec!("kin.lcl_el.shear", "LCL-EL Shear", "Layer kinematics", "kt"),
        spec!(
            "kin.ebwd.shear",
            "Effective Bulk Shear",
            "Layer kinematics",
            "kt"
        ),
        spec!(
            "kin.sfc_500m.mean_wind",
            "SFC-500m Mean Wind",
            "Mean winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_1km.mean_wind",
            "SFC-1km Mean Wind",
            "Mean winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_3km.mean_wind",
            "SFC-3km Mean Wind",
            "Mean winds",
            "°/kt"
        ),
        spec!(
            "kin.effective.mean_wind",
            "Effective Mean Wind",
            "Mean winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_6km.mean_wind",
            "SFC-6km Mean Wind",
            "Mean winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_8km.mean_wind",
            "SFC-8km Mean Wind",
            "Mean winds",
            "°/kt"
        ),
        spec!(
            "kin.lcl_el.mean_wind",
            "LCL-EL Mean Wind",
            "Mean winds",
            "°/kt"
        ),
        spec!(
            "kin.ebwd.mean_wind",
            "Effective Bulk Mean Wind",
            "Mean winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_500m.srw",
            "SFC-500m SR Wind",
            "Storm-relative winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_1km.srw",
            "SFC-1km SR Wind",
            "Storm-relative winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_3km.srw",
            "SFC-3km SR Wind",
            "Storm-relative winds",
            "°/kt"
        ),
        spec!(
            "kin.effective.srw",
            "Effective SR Wind",
            "Storm-relative winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_6km.srw",
            "SFC-6km SR Wind",
            "Storm-relative winds",
            "°/kt"
        ),
        spec!(
            "kin.sfc_8km.srw",
            "SFC-8km SR Wind",
            "Storm-relative winds",
            "°/kt"
        ),
        spec!(
            "kin.lcl_el.srw",
            "LCL-EL SR Wind",
            "Storm-relative winds",
            "°/kt"
        ),
        spec!(
            "kin.ebwd.srw",
            "Effective Bulk SR Wind",
            "Storm-relative winds",
            "°/kt"
        ),
        spec!("kin.brn_shear", "BRN Shear", "Storm motion", "m²/s²"),
        spec!("kin.srw_4_6km", "4-6km SR Wind", "Storm motion", "°/kt"),
        spec!("kin.bunkers_right", "Bunkers Right", "Storm motion", "°/kt"),
        spec!("kin.bunkers_left", "Bunkers Left", "Storm motion", "°/kt"),
        spec!("kin.corfidi_down", "Corfidi Dshr", "Storm motion", "°/kt"),
        spec!("kin.corfidi_up", "Corfidi Ushr", "Storm motion", "°/kt"),
        spec!("kin.wind_1km", "1km Wind", "Storm motion", "°/kt"),
        spec!("kin.wind_6km", "6km Wind", "Storm motion", "°/kt"),
        spec!("kin.critical_angle", "Critical Angle", "Storm motion", "°"),
        spec!("severe.ehi_0_1km", "EHI 0-1km", "Severe indices", ""),
        spec!("severe.ehi_0_3km", "EHI 0-3km", "Severe indices", ""),
        spec!("severe.vgp", "VGP", "Severe indices", ""),
        spec!("severe.peskov", "Peskov Index", "Severe indices", ""),
        spec!("severe.mcs", "MCS Index", "Severe indices", ""),
        spec!("severe.sweat", "SWEAT", "Severe indices", ""),
        spec!("severe.moshe", "MOSHE", "Severe indices", ""),
        spec!("severe.lrghail", "LRGHAIL", "Severe indices", ""),
        spec!(
            "severe.hgz_cape",
            "HGZ CAPE",
            "Severe thermodynamics",
            "J/kg"
        ),
        spec!("severe.nstp", "NSTP", "Severe thermodynamics", ""),
        spec!("severe.ncape", "NCAPE", "Severe thermodynamics", "J/kg/m"),
        spec!("severe.ecape", "ECAPE", "Severe thermodynamics", "J/kg"),
        spec!("severe.lscp", "LSCP", "Severe thermodynamics", ""),
        spec!(
            "severe.wbz_height",
            "WBZ Height",
            "Severe thermodynamics",
            "m AGL"
        ),
    ]
}

pub(super) fn catalog(
    formula: Option<&FormulaSoundingDiagnostic>,
) -> Vec<SoundingDiagnosticOption> {
    let mut catalog = specs()
        .iter()
        .map(|spec| {
            let description = if spec.id.ends_with(".mpl") {
                "Maximum Parcel Level: height AGL where the selected parcel's remaining positive-buoyancy energy is exhausted above its equilibrium level."
            } else {
                "Native SHARP sounding diagnostic; recalculated from the displayed profile."
            };
            SoundingDiagnosticOption::built_in(
                spec.id,
                spec.label,
                spec.category,
                (!spec.unit.is_empty()).then_some(spec.unit),
                description,
            )
        })
        .collect::<Vec<_>>();
    if let Some(formula) = formula {
        catalog.push(SoundingDiagnosticOption::formula(
            formula.id.clone(),
            formula.label.clone(),
            (!formula.units.is_empty()).then_some(formula.units.clone()),
            formula.unavailable_reason.clone().unwrap_or_else(|| {
                "Last completed Formula Lab field sampled at this exact sounding.".to_owned()
            }),
        ));
    }
    catalog
}

fn slots(ids: &[&str]) -> Vec<SoundingTableSlot> {
    ids.iter()
        .map(|id| SoundingTableSlot::new(SoundingDiagnosticRef::built_in(*id)))
        .collect()
}

fn section(id: &str, title: &str, ids: &[&str]) -> ConfigSection {
    ConfigSection {
        id: id.to_owned(),
        title: title.to_owned(),
        slots: slots(ids),
    }
}

/// A complete editable copy of the canonical readout inventory. The canonical
/// board itself remains `SoundingTableConfig::default()` and is never replaced
/// until the user explicitly selects Customize.
pub(super) fn default_config() -> SoundingTableConfig {
    SoundingTableConfig::custom_template(vec![
        SoundingTablePanelConfig {
            panel: SoundingTablePanelId::Convective,
            title: "Parcels & thermo".to_owned(),
            override_active: false,
            sections: vec![
                section(
                    "parcels",
                    "Parcels",
                    &[
                        "parcel.sfc.cape",
                        "parcel.sfc.cinh",
                        "parcel.sfc.lcl",
                        "parcel.sfc.li",
                        "parcel.sfc.lfc",
                        "parcel.sfc.el",
                        "parcel.sfc.mpl",
                        "parcel.ml.cape",
                        "parcel.ml.cinh",
                        "parcel.ml.lcl",
                        "parcel.ml.li",
                        "parcel.ml.lfc",
                        "parcel.ml.el",
                        "parcel.ml.mpl",
                        "parcel.fcst.cape",
                        "parcel.fcst.cinh",
                        "parcel.fcst.lcl",
                        "parcel.fcst.li",
                        "parcel.fcst.lfc",
                        "parcel.fcst.el",
                        "parcel.fcst.mpl",
                        "parcel.mu.cape",
                        "parcel.mu.cinh",
                        "parcel.mu.lcl",
                        "parcel.mu.li",
                        "parcel.mu.lfc",
                        "parcel.mu.el",
                        "parcel.mu.mpl",
                    ],
                ),
                section(
                    "thermodynamics",
                    "Thermodynamics",
                    &[
                        "thermo.pwat",
                        "thermo.mean_mixr",
                        "thermo.low_rh",
                        "thermo.mid_rh",
                        "thermo.dcape",
                        "thermo.downrush_temp",
                        "thermo.k_index",
                        "thermo.total_totals",
                        "thermo.convective_temp",
                        "thermo.max_temp",
                        "thermo.esp",
                        "thermo.mmp",
                        "thermo.wndg",
                        "thermo.tei",
                        "thermo.cape_0_3km",
                        "thermo.cape_0_6km",
                        "thermo.mburst",
                        "thermo.sig_severe",
                    ],
                ),
                section(
                    "lapse-composites",
                    "Lapse rates & composites",
                    &[
                        "lapse.sfc_500m",
                        "lapse.sfc_1km",
                        "lapse.sfc_3km",
                        "lapse.850_500",
                        "lapse.700_500",
                        "composite.scp_right",
                        "composite.stp_effective",
                        "composite.stp_fixed",
                        "composite.ship",
                        "composite.dcp",
                    ],
                ),
            ],
        },
        SoundingTablePanelConfig {
            panel: SoundingTablePanelId::Kinematics,
            title: "Kinematics".to_owned(),
            override_active: false,
            sections: vec![
                section(
                    "helicity",
                    "Storm-relative helicity",
                    &[
                        "kin.sfc_500m.srh",
                        "kin.sfc_1km.srh",
                        "kin.sfc_3km.srh",
                        "kin.effective.srh",
                    ],
                ),
                section(
                    "shear",
                    "Bulk shear",
                    &[
                        "kin.sfc_500m.shear",
                        "kin.sfc_1km.shear",
                        "kin.sfc_3km.shear",
                        "kin.effective.shear",
                        "kin.sfc_6km.shear",
                        "kin.sfc_8km.shear",
                        "kin.lcl_el.shear",
                        "kin.ebwd.shear",
                    ],
                ),
                section(
                    "mean-wind",
                    "Mean wind",
                    &[
                        "kin.sfc_500m.mean_wind",
                        "kin.sfc_1km.mean_wind",
                        "kin.sfc_3km.mean_wind",
                        "kin.effective.mean_wind",
                        "kin.sfc_6km.mean_wind",
                        "kin.sfc_8km.mean_wind",
                        "kin.lcl_el.mean_wind",
                        "kin.ebwd.mean_wind",
                    ],
                ),
                section(
                    "storm-relative",
                    "Storm-relative wind",
                    &[
                        "kin.sfc_500m.srw",
                        "kin.sfc_1km.srw",
                        "kin.sfc_3km.srw",
                        "kin.effective.srw",
                        "kin.sfc_6km.srw",
                        "kin.sfc_8km.srw",
                        "kin.lcl_el.srw",
                        "kin.ebwd.srw",
                    ],
                ),
                section(
                    "motion",
                    "Storm motion",
                    &[
                        "kin.brn_shear",
                        "kin.srw_4_6km",
                        "kin.bunkers_right",
                        "kin.bunkers_left",
                        "kin.corfidi_down",
                        "kin.corfidi_up",
                        "kin.wind_1km",
                        "kin.wind_6km",
                    ],
                ),
            ],
        },
        SoundingTablePanelConfig {
            panel: SoundingTablePanelId::Severe,
            title: "Severe indices".to_owned(),
            override_active: false,
            sections: vec![
                section(
                    "severe",
                    "Severe indices",
                    &[
                        "severe.ehi_0_1km",
                        "severe.ehi_0_3km",
                        "severe.vgp",
                        "severe.peskov",
                        "severe.mcs",
                        "severe.sweat",
                        "severe.moshe",
                        "severe.lrghail",
                    ],
                ),
                section(
                    "severe-thermo",
                    "Thermodynamic indices",
                    &[
                        "severe.hgz_cape",
                        "severe.nstp",
                        "severe.ncape",
                        "severe.ecape",
                        "severe.lscp",
                        "severe.wbz_height",
                    ],
                ),
            ],
        },
    ])
}

struct Resolved {
    value: String,
    unit: String,
    color: Option<Color32>,
}

fn finite(value: f64) -> Option<f64> {
    (value.is_finite() && value > -9_990.0).then_some(value)
}

fn i0(value: f64) -> String {
    finite(value).map_or_else(|| "--".to_owned(), |value| format!("{value:.0}"))
}

fn f1(value: f64) -> String {
    finite(value).map_or_else(|| "--".to_owned(), |value| format!("{value:.1}"))
}

fn f2(value: f64) -> String {
    finite(value).map_or_else(|| "--".to_owned(), |value| format!("{value:.2}"))
}

fn magnitude(value: (f64, f64)) -> f64 {
    if value.0.is_finite() && value.1.is_finite() {
        value.0.hypot(value.1)
    } else {
        f64::NAN
    }
}

fn vector(value: (f64, f64)) -> String {
    if !value.0.is_finite() || !value.1.is_finite() {
        return "--".to_owned();
    }
    format!(
        "{:03}/{:02}",
        (value.0.round() as i64).rem_euclid(360),
        value.1.round() as i64
    )
}

fn components(value: (f64, f64)) -> String {
    if !value.0.is_finite() || !value.1.is_finite() {
        return "--".to_owned();
    }
    let direction = (270.0 - value.1.atan2(value.0).to_degrees()).rem_euclid(360.0);
    format!(
        "{:03}/{:02}",
        direction.round() as i64,
        value.0.hypot(value.1).round() as i64
    )
}

fn gradient(value: f64, yellow: f64, red: f64, pink: f64, higher: bool) -> Option<Color32> {
    let value = finite(value)?;
    if value == 0.0 {
        None
    } else if higher {
        if value >= pink {
            Some(PINK)
        } else if value >= red {
            Some(RED)
        } else if value >= yellow {
            Some(YELLOW)
        } else {
            None
        }
    } else if value <= pink {
        Some(PINK)
    } else if value <= red {
        Some(RED)
    } else if value <= yellow {
        Some(YELLOW)
    } else {
        None
    }
}

fn low_severe(value: f64) -> Option<Color32> {
    finite(value)
        .filter(|value| (0.0..1.0).contains(value))
        .map(|_| AMBER)
}

fn lapse_color(value: f64) -> Option<Color32> {
    let value = finite(value)?;
    if value == 0.0 {
        None
    } else if value <= 6.0 {
        Some(GREEN)
    } else if value <= 7.0 {
        Some(YELLOW)
    } else if value <= 8.0 {
        Some(ORANGE)
    } else if value <= 9.0 {
        Some(RED)
    } else {
        Some(PINK)
    }
}

fn cape3_color(value: f64) -> Option<Color32> {
    let value = finite(value)?;
    if value > 125.0 {
        Some(PINK)
    } else if value > 100.0 {
        Some(RED)
    } else if value > 75.0 {
        Some(ORANGE)
    } else if value > 50.0 {
        Some(YELLOW)
    } else if value > 25.0 {
        Some(GREEN)
    } else {
        None
    }
}

fn parcel<'a>(analysis: &'a SharppyAnalysis, name: &str) -> Option<&'a Parcel> {
    match name {
        "sfc" => Some(&analysis.prof.sfcpcl),
        "ml" => Some(&analysis.prof.mlpcl),
        "fcst" => Some(&analysis.prof.fcstpcl),
        "mu" => Some(&analysis.prof.mupcl),
        _ => None,
    }
}

fn resolve_parcel(analysis: &SharppyAnalysis, id: &str) -> Option<Resolved> {
    let mut parts = id.split('.');
    (parts.next()? == "parcel").then_some(())?;
    let parcel = parcel(analysis, parts.next()?)?;
    let field = parts.next()?;
    let active = finite(parcel.bplus).is_some_and(|cape| cape > 0.0);
    let (value, unit, color) = match field {
        "cape" => (
            i0(parcel.bplus),
            "J/kg",
            active
                .then(|| gradient(parcel.bplus, 1000.0, 2500.0, 4000.0, true))
                .flatten(),
        ),
        "cinh" => {
            let color = active
                .then(|| finite(parcel.bminus))
                .flatten()
                .map(|value| {
                    if value >= -50.0 {
                        GREEN
                    } else if value >= -100.0 {
                        ORANGE
                    } else {
                        RED
                    }
                });
            (i0(parcel.bminus), "J/kg", color)
        }
        "lcl" => (i0(parcel.lclhght), "m AGL", None),
        "li" => (
            i0(parcel.li5),
            "°C",
            active
                .then(|| gradient(parcel.li5, -4.0, -7.0, -10.0, false))
                .flatten(),
        ),
        "lfc" => (i0(parcel.lfchght), "m AGL", None),
        "el" => (i0(parcel.elhght), "m AGL", None),
        "mpl" => (i0(parcel.mplhght), "m AGL", None),
        _ => return None,
    };
    Some(Resolved {
        value,
        unit: unit.to_owned(),
        color,
    })
}

fn resolve_builtin(analysis: &SharppyAnalysis, id: &str) -> Option<Resolved> {
    if id.starts_with("parcel.") {
        return resolve_parcel(analysis, id);
    }
    let d = &analysis.derived;
    let normal = |value: String, unit: &str| Resolved {
        value,
        unit: unit.to_owned(),
        color: None,
    };
    let toned = |value: String, unit: &str, color| Resolved {
        value,
        unit: unit.to_owned(),
        color,
    };
    Some(match id {
        "thermo.pwat" => normal(f2(d.pwat), "in"),
        "thermo.mean_mixr" => normal(f2(d.mean_mixr), "g/kg"),
        "thermo.low_rh" => normal(i0(d.low_rh), "%"),
        "thermo.mid_rh" => normal(i0(d.mid_rh), "%"),
        "thermo.dcape" => normal(i0(d.dcape), "J/kg"),
        "thermo.downrush_temp" => normal(i0(d.drush_f), "°F"),
        "thermo.k_index" => normal(i0(d.k_idx), ""),
        "thermo.total_totals" => normal(i0(d.totals_totals), ""),
        "thermo.convective_temp" => normal(i0(d.conv_t_f), "°F"),
        "thermo.max_temp" => normal(i0(d.max_t_f), "°F"),
        "thermo.esp" => normal(f1(d.esp), ""),
        "thermo.mmp" => normal(f2(d.mmp), ""),
        "thermo.wndg" => normal(f1(d.wndg), ""),
        "thermo.tei" => normal(i0(d.tei), ""),
        "thermo.cape_0_3km" => toned(i0(d.cape_0_3km), "J/kg", cape3_color(d.cape_0_3km)),
        "thermo.cape_0_6km" => toned(i0(d.cape_0_6km), "J/kg", cape3_color(d.cape_0_6km)),
        "thermo.mburst" => normal(i0(d.mburst), ""),
        "thermo.sig_severe" => normal(i0(d.sig_severe), "m³/s³"),
        "thermo.thetae_diff" => normal(f1(d.thetae_diff), "°C"),
        "lapse.sfc_500m" => toned(
            f1(d.lapserate_sfc_500m),
            "°C/km",
            lapse_color(d.lapserate_sfc_500m),
        ),
        "lapse.sfc_1km" => toned(
            f1(d.lapserate_sfc_1km),
            "°C/km",
            lapse_color(d.lapserate_sfc_1km),
        ),
        "lapse.sfc_3km" => toned(f1(d.lapserate_3km), "°C/km", lapse_color(d.lapserate_3km)),
        "lapse.3_6km" => toned(
            f1(d.lapserate_3_6km),
            "°C/km",
            lapse_color(d.lapserate_3_6km),
        ),
        "lapse.850_500" => toned(
            f1(d.lapserate_850_500),
            "°C/km",
            lapse_color(d.lapserate_850_500),
        ),
        "lapse.700_500" => toned(
            f1(d.lapserate_700_500),
            "°C/km",
            lapse_color(d.lapserate_700_500),
        ),
        "composite.scp_right" => toned(
            f1(d.right_scp),
            "",
            if finite(d.right_scp).is_some_and(|value| value < 0.0) {
                Some(CYAN)
            } else {
                low_severe(d.right_scp).or_else(|| gradient(d.right_scp, 0.5, 2.0, 5.0, true))
            },
        ),
        "composite.scp_left" => toned(f1(d.left_scp), "", Some(CYAN)),
        "composite.stp_effective" => toned(
            f1(d.stp_cin),
            "",
            low_severe(d.stp_cin).or_else(|| gradient(d.stp_cin, 0.5, 2.0, 5.0, true)),
        ),
        "composite.stp_fixed" => toned(
            f1(d.stp_fixed),
            "",
            low_severe(d.stp_fixed).or_else(|| gradient(d.stp_fixed, 1.0, 2.0, 5.0, true)),
        ),
        "composite.ship" => toned(
            f1(d.ship),
            "",
            low_severe(d.ship).or_else(|| gradient(d.ship, 1.0, 2.0, 3.0, true)),
        ),
        "composite.dcp" => toned(
            f1(d.dcp),
            "",
            low_severe(d.dcp).or_else(|| gradient(d.dcp, 1.0, 4.0, 6.0, true)),
        ),
        "kin.sfc_500m.srh" => normal(i0(d.srh500), "m²/s²"),
        "kin.sfc_1km.srh" => normal(i0(d.srh1km), "m²/s²"),
        "kin.sfc_3km.srh" => normal(i0(d.srh3km), "m²/s²"),
        "kin.effective.srh" => normal(i0(d.right_esrh), "m²/s²"),
        "kin.sfc_500m.shear" => normal(i0(magnitude(d.sfc_500m_shear)), "kt"),
        "kin.sfc_1km.shear" => normal(i0(magnitude(d.sfc_1km_shear)), "kt"),
        "kin.sfc_3km.shear" => normal(i0(magnitude(d.sfc_3km_shear)), "kt"),
        "kin.effective.shear" => normal(i0(magnitude(d.eff_shear)), "kt"),
        "kin.sfc_6km.shear" => normal(i0(magnitude(d.sfc_6km_shear)), "kt"),
        "kin.sfc_8km.shear" => normal(i0(magnitude(d.sfc_8km_shear)), "kt"),
        "kin.lcl_el.shear" => normal(i0(magnitude(d.lcl_el_shear)), "kt"),
        "kin.ebwd.shear" => normal(i0(magnitude(d.ebwd)), "kt"),
        "kin.sfc_500m.mean_wind" => normal(components(d.mean_wind_sfc_500m), "°/kt"),
        "kin.sfc_1km.mean_wind" => normal(vector(d.mean_1km), "°/kt"),
        "kin.sfc_3km.mean_wind" => normal(vector(d.mean_3km), "°/kt"),
        "kin.effective.mean_wind" => normal(components(d.mean_eff), "°/kt"),
        "kin.sfc_6km.mean_wind" => normal(vector(d.mean_6km), "°/kt"),
        "kin.sfc_8km.mean_wind" => normal(vector(d.mean_8km), "°/kt"),
        "kin.lcl_el.mean_wind" => normal(vector(d.mean_lcl_el), "°/kt"),
        "kin.ebwd.mean_wind" => normal(components(d.mean_ebw), "°/kt"),
        "kin.sfc_500m.srw" => normal(components(d.srw_sfc_500m), "°/kt"),
        "kin.sfc_1km.srw" => normal(vector(d.srw_1km), "°/kt"),
        "kin.sfc_3km.srw" => normal(vector(d.srw_3km), "°/kt"),
        "kin.effective.srw" => normal(components(d.srw_eff), "°/kt"),
        "kin.sfc_6km.srw" => normal(vector(d.srw_6km), "°/kt"),
        "kin.sfc_8km.srw" => normal(vector(d.srw_8km), "°/kt"),
        "kin.lcl_el.srw" => normal(vector(d.srw_lcl_el), "°/kt"),
        "kin.ebwd.srw" => normal(components(d.srw_ebw), "°/kt"),
        "kin.brn_shear" => normal(i0(d.brnshear), "m²/s²"),
        "kin.srw_4_6km" => normal(vector(d.srw_4_5km), "°/kt"),
        "kin.bunkers_right" => normal(
            components((analysis.prof.srwind.0, analysis.prof.srwind.1)),
            "°/kt",
        ),
        "kin.bunkers_left" => toned(
            components((analysis.prof.srwind.2, analysis.prof.srwind.3)),
            "°/kt",
            Some(RED),
        ),
        "kin.corfidi_down" => normal(components(d.corfidi_dn), "°/kt"),
        "kin.corfidi_up" => normal(components(d.corfidi_up), "°/kt"),
        "kin.wind_1km" => normal(vector(d.wind1km), "°/kt"),
        "kin.wind_6km" => normal(vector(d.wind6km), "°/kt"),
        "kin.critical_angle" => normal(i0(d.right_critical_angle), "°"),
        "severe.ehi_0_1km" => toned(
            f1(d.ehi_0_1km),
            "",
            gradient(d.ehi_0_1km, 1.0, 2.0, 3.0, true),
        ),
        "severe.ehi_0_3km" => toned(
            f1(d.ehi_0_3km),
            "",
            gradient(d.ehi_0_3km, 1.0, 2.0, 3.0, true),
        ),
        "severe.vgp" => normal(f2(d.vgp), ""),
        "severe.peskov" => toned(f1(d.peskov), "", gradient(d.peskov, 1.0, 4.0, 7.0, true)),
        "severe.mcs" => toned(
            f1(d.mcs_index),
            "",
            gradient(d.mcs_index, 1.0, 2.0, 3.0, true),
        ),
        "severe.sweat" => {
            let color = finite(d.sweat).and_then(|value| {
                if value == 0.0 {
                    None
                } else if value < 250.0 {
                    Some(BLUE)
                } else if value < 350.0 {
                    Some(Color32::WHITE)
                } else if value < 500.0 {
                    Some(YELLOW)
                } else if value < 650.0 {
                    Some(RED)
                } else {
                    Some(PINK)
                }
            });
            toned(i0(d.sweat), "", color)
        }
        "severe.moshe" => toned(
            f1(d.modified_sherbe),
            "",
            gradient(d.modified_sherbe, 1.0, 2.0, 3.0, true),
        ),
        "severe.lrghail" => toned(f1(d.lrghail), "", gradient(d.lrghail, 4.0, 7.0, 10.0, true)),
        "severe.hgz_cape" => toned(
            i0(d.hgz_cape),
            "J/kg",
            gradient(d.hgz_cape, 1000.0, 2500.0, 4000.0, true),
        ),
        "severe.nstp" => toned(f1(d.nstp), "", gradient(d.nstp, 1.0, 2.0, 4.0, true)),
        "severe.ncape" => toned(
            f2(d.ncape),
            "J/kg/m",
            gradient(d.ncape, 0.1, 0.2, 0.3, true),
        ),
        "severe.ecape" => toned(
            i0(d.ecape),
            "J/kg",
            gradient(d.ecape, 1000.0, 2500.0, 4000.0, true),
        ),
        "severe.lscp" => toned(
            f1(finite(d.lscp).unwrap_or(d.left_scp)),
            "",
            gradient(
                finite(d.lscp).unwrap_or(d.left_scp),
                -1.0,
                -4.0,
                -8.0,
                false,
            ),
        ),
        "severe.wbz_height" => normal(i0(d.wbz_height), "m AGL"),
        _ => return None,
    })
}

fn formula_value(value: f64) -> String {
    if !value.is_finite() {
        "--".to_owned()
    } else if value != 0.0 && value.abs() < 0.001 {
        format!("{value:.2e}")
    } else if value.abs() >= 1_000.0 {
        format!("{value:.0}")
    } else if value.abs() >= 100.0 {
        format!("{value:.1}")
    } else if value.abs() >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.3}")
    }
}

fn panel_kind(id: SoundingTablePanelId) -> DiagnosticTablePanelKind {
    match id {
        SoundingTablePanelId::Convective => DiagnosticTablePanelKind::Convective,
        SoundingTablePanelId::Kinematics => DiagnosticTablePanelKind::Kinematics,
        SoundingTablePanelId::Severe => DiagnosticTablePanelKind::Severe,
    }
}

fn preferred_columns(panel: SoundingTablePanelId, section: &str, rows: usize) -> usize {
    match (panel, section) {
        (SoundingTablePanelId::Convective, "parcels") => 4,
        (SoundingTablePanelId::Convective, "thermodynamics") => 3,
        (SoundingTablePanelId::Convective, "lapse-composites") => 2,
        (SoundingTablePanelId::Kinematics, _) | (SoundingTablePanelId::Severe, _) => 2,
        _ if rows >= 18 => 3,
        _ if rows >= 8 => 2,
        _ => 1,
    }
}

/// The two independent renderer hooks produced by a customized board.
///
/// Non-structural edits stay in `native_patches`, preserving sharppyrs' exact
/// hand-tuned table geometry. Only edits that cannot be expressed against the
/// canonical positions (renames and row/section count changes) enter
/// `generic` and replace that one panel with the free-form renderer.
pub(super) struct BuiltTableOverrides {
    pub(super) generic: DiagnosticTableBoard,
    pub(super) native_patches: NativeDiagnosticPatchBoard,
}

fn resolved_row(
    slot: &SoundingTableSlot,
    catalog: &[SoundingDiagnosticOption],
    analysis: &SharppyAnalysis,
    formula: Option<&FormulaSoundingDiagnostic>,
) -> DiagnosticTableRow {
    let (label, resolved) = match &slot.diagnostic {
        SoundingDiagnosticRef::Blank => (
            String::new(),
            Resolved {
                value: String::new(),
                unit: String::new(),
                color: None,
            },
        ),
        SoundingDiagnosticRef::BuiltIn { id } => {
            let label = catalog
                .iter()
                .find(|option| option.diagnostic == slot.diagnostic)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| format!("Unavailable · {id}"));
            let resolved = resolve_builtin(analysis, id).unwrap_or_else(|| Resolved {
                value: "--".to_owned(),
                unit: String::new(),
                color: None,
            });
            (label, resolved)
        }
        SoundingDiagnosticRef::Formula { id } => {
            let matching = formula.filter(|formula| formula.id == *id);
            let label = matching
                .map(|formula| formula.label.clone())
                .unwrap_or_else(|| format!("Unavailable · {id}"));
            let resolved = Resolved {
                value: matching
                    .and_then(|formula| formula.value)
                    .map_or_else(|| "--".to_owned(), formula_value),
                unit: matching
                    .map(|formula| formula.units.clone())
                    .unwrap_or_default(),
                color: None,
            };
            (label, resolved)
        }
    };
    let label = slot.label_override.clone().unwrap_or(label);
    let mut row = DiagnosticTableRow::new(label, resolved.value).unit(resolved.unit);
    if let Some(color) = resolved.color {
        row = row.color(color);
    }
    row
}

fn native_shape_matches(
    panel: &SoundingTablePanelConfig,
    canonical: &SoundingTablePanelConfig,
) -> bool {
    panel.title == canonical.title
        && panel.sections.len() == canonical.sections.len()
        && panel
            .sections
            .iter()
            .zip(&canonical.sections)
            .all(|(section, canonical)| {
                section.id == canonical.id
                    && section.title == canonical.title
                    && section.slots.len() == canonical.slots.len()
            })
}

fn generic_panel(
    panel_config: &SoundingTablePanelConfig,
    catalog: &[SoundingDiagnosticOption],
    analysis: &SharppyAnalysis,
    formula: Option<&FormulaSoundingDiagnostic>,
) -> DiagnosticTablePanel {
    let mut sections = Vec::new();
    for section_config in &panel_config.sections {
        let rows = section_config
            .slots
            .iter()
            .map(|slot| resolved_row(slot, catalog, analysis, formula))
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            let columns = preferred_columns(panel_config.panel, &section_config.id, rows.len());
            sections.push(
                DiagnosticTableSection::new(section_config.title.clone(), rows).columns(columns),
            );
        }
    }
    DiagnosticTablePanel::new(
        panel_kind(panel_config.panel),
        panel_config.title.clone(),
        sections,
    )
}

pub(super) fn build_board(
    config: &SoundingTableConfig,
    analysis: &SharppyAnalysis,
    formula: Option<&FormulaSoundingDiagnostic>,
) -> Option<BuiltTableOverrides> {
    if !config.is_custom() {
        return None;
    }
    let catalog = catalog(formula);
    let defaults = default_config();
    let mut panels = Vec::new();
    let mut patches = Vec::new();
    for panel_config in &config.panels {
        // Opening the editor seeds an editable copy of every canonical panel,
        // but that must not change the rendered board by itself. A panel only
        // replaces sharppyrs' exact native renderer after the user edits that
        // individual panel and activates its override.
        if !config.panel_override_active(panel_config.panel) {
            continue;
        }
        let Some(canonical) = defaults.panel(panel_config.panel) else {
            panels.push(generic_panel(panel_config, &catalog, analysis, formula));
            continue;
        };
        if !native_shape_matches(panel_config, canonical) {
            panels.push(generic_panel(panel_config, &catalog, analysis, formula));
            continue;
        }

        // Preserve native placement and formatting by diffing each configured
        // row against its canonical position. Reordering equal-size rows is a
        // set of replacements at those positions, not a structural change.
        for (section, canonical_section) in panel_config.sections.iter().zip(&canonical.sections) {
            for (slot, canonical_slot) in section.slots.iter().zip(&canonical_section.slots) {
                if slot == canonical_slot {
                    continue;
                }
                let SoundingDiagnosticRef::BuiltIn { id: canonical_id } =
                    &canonical_slot.diagnostic
                else {
                    unreachable!("default_config native positions must be built-in diagnostics")
                };
                let panel = panel_kind(panel_config.panel);
                match slot.diagnostic {
                    SoundingDiagnosticRef::Blank => {
                        patches.push(NativeDiagnosticPatch::blank(panel, canonical_id.clone()));
                    }
                    _ => patches.push(NativeDiagnosticPatch::replace(
                        panel,
                        canonical_id.clone(),
                        resolved_row(slot, &catalog, analysis, formula),
                    )),
                }
            }
        }
    }
    Some(BuiltTableOverrides {
        generic: DiagnosticTableBoard { panels },
        native_patches: NativeDiagnosticPatchBoard { patches },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_analysis() -> SharppyAnalysis {
        let prof = sharppyrs::Profile::new(sharppyrs::SoundingData {
            pres: vec![
                1000.0, 925.0, 850.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0,
            ],
            hght: vec![
                110.0, 780.0, 1500.0, 3100.0, 5800.0, 7500.0, 9600.0, 10900.0, 12300.0, 14100.0,
            ],
            tmpc: vec![
                27.0, 22.0, 17.5, 8.0, -8.5, -20.0, -36.0, -46.0, -55.0, -60.0,
            ],
            dwpc: vec![
                22.0, 19.0, 15.0, 4.0, -15.0, -30.0, -48.0, -58.0, -68.0, -75.0,
            ],
            wdir: vec![
                170.0, 180.0, 190.0, 205.0, 220.0, 230.0, 240.0, 245.0, 250.0, 255.0,
            ],
            wspd: vec![10.0, 15.0, 20.0, 28.0, 38.0, 45.0, 52.0, 58.0, 62.0, 65.0],
            latitude: Some(35.0),
            longitude: Some(-97.0),
            ..Default::default()
        })
        .expect("sample profile");
        let derived = sharppyrs::DerivedParams::compute(&prof);
        SharppyAnalysis {
            prof,
            derived,
            title: "sample".to_owned(),
            obs_adjusted_model: false,
        }
    }

    #[test]
    fn registry_ids_are_unique_and_default_slots_all_resolve_to_catalog() {
        let mut ids = specs().iter().map(|spec| spec.id).collect::<Vec<_>>();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
        assert!(count >= 102, "complete canonical inventory, got {count}");

        let catalog = catalog(None);
        let defaults = default_config();
        let analysis = sample_analysis();
        for panel in defaults.panels {
            for section in panel.sections {
                for slot in section.slots {
                    assert!(
                        catalog
                            .iter()
                            .any(|option| option.diagnostic == slot.diagnostic),
                        "missing {:?}",
                        slot.diagnostic
                    );
                    let SoundingDiagnosticRef::BuiltIn { id } = &slot.diagnostic else {
                        panic!("default templates contain built-ins only")
                    };
                    assert!(
                        resolve_builtin(&analysis, id).is_some(),
                        "default ID has no evaluator: {id}"
                    );
                }
            }
        }
    }

    #[test]
    fn mu_mpl_is_selectable_and_uses_the_existing_parcel_height() {
        let option = catalog(None)
            .into_iter()
            .find(|option| option.diagnostic == SoundingDiagnosticRef::built_in("parcel.mu.mpl"))
            .expect("MU MPL appears in the custom-table picker");
        assert_eq!(option.label, "MU MPL");
        assert_eq!(option.unit.as_deref(), Some("m AGL"));
        assert!(option.description.contains("Maximum Parcel Level"));

        let mut analysis = sample_analysis();
        analysis.prof.mupcl.mplhght = 12_345.6;
        let resolved = resolve_builtin(&analysis, "parcel.mu.mpl").expect("MPL resolver");
        assert_eq!(resolved.value, "12346");
        assert_eq!(resolved.unit, "m AGL");

        analysis.prof.mupcl.mplhght = f64::NAN;
        assert_eq!(
            resolve_builtin(&analysis, "parcel.mu.mpl")
                .expect("non-finite MPL remains a known diagnostic")
                .value,
            "--"
        );
    }

    #[test]
    fn formula_formatter_is_compact_across_scales() {
        assert_eq!(formula_value(2_515.4), "2515");
        assert_eq!(formula_value(12.345), "12.35");
        assert_eq!(formula_value(0.000_012), "1.20e-5");
        assert_eq!(formula_value(f64::NAN), "--");
    }

    #[test]
    fn default_templates_exactly_match_the_native_slot_inventory() {
        let defaults = default_config();
        for panel_id in SoundingTablePanelId::ALL {
            let configured = defaults
                .panel(panel_id)
                .expect("default panel")
                .sections
                .iter()
                .flat_map(|section| &section.slots)
                .map(|slot| match &slot.diagnostic {
                    SoundingDiagnosticRef::BuiltIn { id } => id.as_str(),
                    _ => panic!("native default positions must be built-ins"),
                })
                .collect::<Vec<_>>();
            assert_eq!(
                configured,
                sharppyrs::native_diagnostic_slot_ids(panel_kind(panel_id)),
                "BowEcho defaults must address the exact native positions"
            );
        }
    }

    #[test]
    fn customize_or_unchanged_active_panel_emits_no_override() {
        let analysis = sample_analysis();
        let mut config = default_config();

        let board = build_board(&config, &analysis, None).expect("custom editor template");
        assert!(
            board.generic.panels.is_empty() && board.native_patches.patches.is_empty(),
            "opening Customize tables must leave every native panel untouched"
        );

        config
            .panel_mut(SoundingTablePanelId::Kinematics)
            .expect("kinematics template")
            .override_active = true;
        let board = build_board(&config, &analysis, None).expect("active panel override");
        assert!(
            board.generic.panels.is_empty() && board.native_patches.patches.is_empty(),
            "an active but unchanged panel is still exactly native"
        );
    }

    #[test]
    fn one_lapse_rate_change_is_one_native_patch_and_no_generic_panel() {
        let analysis = sample_analysis();
        let mut config = default_config();
        let panel = config
            .panel_mut(SoundingTablePanelId::Convective)
            .expect("convective template");
        panel.override_active = true;
        panel
            .sections
            .iter_mut()
            .find(|section| section.id == "lapse-composites")
            .expect("lapse section")
            .slots[0]
            .diagnostic = SoundingDiagnosticRef::built_in("lapse.3_6km");

        let board = build_board(&config, &analysis, None).expect("native patch board");
        assert!(board.generic.panels.is_empty());
        assert_eq!(board.native_patches.patches.len(), 1);
        let patch = &board.native_patches.patches[0];
        assert_eq!(patch.panel, DiagnosticTablePanelKind::Convective);
        assert_eq!(patch.slot_id, "lapse.sfc_500m");
        let sharppyrs::NativeDiagnosticSlotPatch::Replace(row) = &patch.value else {
            panic!("changed lapse rate must replace its canonical native cell")
        };
        assert_eq!(row.label, "3-6km LR");
    }

    #[test]
    fn same_count_reorder_is_positional_native_patches() {
        let analysis = sample_analysis();
        let mut config = default_config();
        let panel = config
            .panel_mut(SoundingTablePanelId::Convective)
            .expect("convective template");
        panel.override_active = true;
        let slots = &mut panel
            .sections
            .iter_mut()
            .find(|section| section.id == "lapse-composites")
            .expect("lapse section")
            .slots;
        slots.swap(0, 1);

        let board = build_board(&config, &analysis, None).expect("native patch board");
        assert!(board.generic.panels.is_empty());
        assert_eq!(board.native_patches.patches.len(), 2);
        assert_eq!(board.native_patches.patches[0].slot_id, "lapse.sfc_500m");
        assert_eq!(board.native_patches.patches[1].slot_id, "lapse.sfc_1km");
    }

    #[test]
    fn removing_a_row_is_structural_and_uses_one_generic_panel() {
        let analysis = sample_analysis();
        let mut config = default_config();
        let panel = config
            .panel_mut(SoundingTablePanelId::Convective)
            .expect("convective template");
        panel.override_active = true;
        panel
            .sections
            .iter_mut()
            .find(|section| section.id == "lapse-composites")
            .expect("lapse section")
            .slots
            .pop();

        let board = build_board(&config, &analysis, None).expect("generic board");
        assert_eq!(board.generic.panels.len(), 1);
        assert_eq!(
            board.generic.panels[0].kind,
            DiagnosticTablePanelKind::Convective
        );
        assert!(board.native_patches.patches.is_empty());
    }

    #[test]
    fn formula_replacement_uses_the_current_resolved_value() {
        let analysis = sample_analysis();
        let formula = FormulaSoundingDiagnostic {
            id: "formula_lab:test".to_owned(),
            label: "Custom hail signal".to_owned(),
            units: "widgets".to_owned(),
            source_hour: rw_ui::HourKey {
                model: "wrf".to_owned(),
                run: "test".to_owned(),
                hour: 0,
                exact_time: None,
            },
            value: Some(12.345),
            unavailable_reason: None,
        };
        let mut config = default_config();
        let panel = config
            .panel_mut(SoundingTablePanelId::Convective)
            .expect("convective template");
        panel.override_active = true;
        panel.sections[1].slots[0].diagnostic = SoundingDiagnosticRef::formula(formula.id.clone());

        let board = build_board(&config, &analysis, Some(&formula)).expect("native patch board");
        assert!(board.generic.panels.is_empty());
        assert_eq!(board.native_patches.patches.len(), 1);
        let patch = &board.native_patches.patches[0];
        assert_eq!(patch.slot_id, "thermo.pwat");
        let sharppyrs::NativeDiagnosticSlotPatch::Replace(row) = &patch.value else {
            panic!("formula must replace its selected native cell")
        };
        assert_eq!(row.label, "Custom hail signal");
        assert_eq!(row.value, "12.35");
        assert_eq!(row.unit, "widgets");
    }
}
