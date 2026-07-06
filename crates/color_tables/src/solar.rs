//! Solarpower07 WRF-Runner colormaps, ported into BowEcho for model/WRF
//! map layers.
//!
//! CREDIT: these palettes are ports of the colormaps authored by
//! **Solarpower07** for the WRF-Runner project (collaborator; credit pending
//! per project policy — handle only, no real name).
//! Source: <https://github.com/Solarpower07/WRF-Runner> `colormaps.py`
//! (branch `New-PC-Updates`), together with the level arrays applied in
//! `plot_helper.py`.
//!
//! Faithfulness notes:
//! * The RGB stops are copied verbatim from `colormaps.py`.
//! * The value levels come from the `add_filled_contours(..., levels=...)`
//!   calls in `plot_helper.py`. Solar's `_build_contourf_cmap_and_norm`
//!   samples each base colormap at interval midpoints normalized *linearly*
//!   over `[levels[0], levels[-1]]`, so the displayed colour of a value `v`
//!   is `base_cmap((v - levels[0]) / (levels[-1] - levels[0]))`. We therefore
//!   place each base-colormap anchor at its linear value position, which
//!   reproduces the same colour ramp.
//! * Solar authors most palettes in imperial/observational display units
//!   (dBZ, °F, kt, inches). WRF store fields arrive in native units
//!   (K, m/s, kg/m², …). The resolver [`solar_model_field_table`] is
//!   unit-aware: it converts Solar's anchor values into the field's units so
//!   the table lines up with the raw stored values.
//!
//! Where two adjacent Solar gradient segments meet with a colour
//! discontinuity, the "closing" anchor of the earlier segment is nudged a
//! hair below the boundary so the sharp transition survives linear
//! interpolation (the sub-degree gap is invisible on a weather map).

use crate::{ColorStop, ColorTable, Rgba8};

/// Temperature-family unit the stored field speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TempUnit {
    Fahrenheit,
    Celsius,
    Kelvin,
}

/// Wind-speed unit the stored field speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpeedUnit {
    Knots,
    MetersPerSecond,
    MilesPerHour,
}

/// Accumulated-precip unit the stored field speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DepthUnit {
    Inches,
    /// Millimetres, i.e. kg/m² of liquid water equivalent.
    Millimetres,
}

/// Parse a `#rrggbb` hex colour (opaque). Solar's palettes are all 6-digit.
fn hex(code: &str) -> Rgba8 {
    let bytes = code.trim_start_matches('#').as_bytes();
    let nibble = |b: u8| -> u8 {
        match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => 0,
        }
    };
    let channel = |i: usize| nibble(bytes[i]) * 16 + nibble(bytes[i + 1]);
    Rgba8::opaque(channel(0), channel(2), channel(4))
}

/// Build an interpolated table from `(value, "#rrggbb")` anchors, applying
/// `convert` to each anchor value first (identity for same-unit palettes).
fn interpolated_from_anchors(
    name: &str,
    anchors: &[(f32, &str)],
    convert: impl Fn(f32) -> f32,
) -> ColorTable {
    let stops = anchors
        .iter()
        .map(|&(value, code)| ColorStop {
            value: convert(value),
            color: hex(code),
            end_color: None,
        })
        .collect::<Vec<_>>();
    ColorTable::new(name, stops).expect("solar palette has >= 2 valid stops")
}

// ---------------------------------------------------------------------------
// Reflectivity — Solar's "PW Style" 27-colour dBZ `ListedColormap`
// (`REFLECTIVITY_COLORMAP`), applied over `np.arange(5, 73, 2.5)` in
// `plot_helper.py::plot_composite_reflectivity`: 28 boundaries, 27 discrete
// 2.5 dBZ bins from 5 to 72.5 dBZ.
// ---------------------------------------------------------------------------

const SOLAR_REFLECTIVITY_COLORS: [&str; 27] = [
    "#ffffff", "#f2f6fc", "#d9e3f4", "#b0c6e6", "#8aa7da", "#648bcb", "#396dc1", "#1350b4",
    "#0d4f5d", "#43736f", "#77987b", "#a8bf8b", "#fdf273", "#f2d45a", "#eeb247", "#e1932d",
    "#d97517", "#cd5403", "#cd0002", "#a10206", "#75030b", "#9e37ab", "#83259d", "#601490",
    "#818181", "#b3b3b3", "#e8e8e8",
];

/// Solar's "PW Style" composite-reflectivity palette (dBZ), a faithful
/// 27-colour stepped ladder over 5..72.5 dBZ; clear air (< 5 dBZ) transparent.
/// Credit: Solarpower07 / WRF-Runner `REFLECTIVITY_COLORMAP`.
pub fn solar_reflectivity_table() -> ColorTable {
    // Leading transparent band holds clear air (< 5 dBZ) — encoded as a real
    // stop (not a display threshold) so the table round-trips through .pal.
    let mut stops = vec![ColorStop {
        value: -30.0,
        color: Rgba8::TRANSPARENT,
        end_color: None,
    }];
    // Stepped: stop i sits at the lower edge of its 2.5 dBZ bin (5 + 2.5*i).
    stops.extend(
        SOLAR_REFLECTIVITY_COLORS
            .iter()
            .enumerate()
            .map(|(i, code)| ColorStop {
                value: 5.0 + 2.5 * i as f32,
                color: hex(code),
                end_color: None,
            }),
    );
    ColorTable::new_stepped("Solar PW Reflectivity", stops)
        .expect("solar reflectivity palette is valid")
}

// ---------------------------------------------------------------------------
// Temperature — Solar's `get_temp_colormap` with the `sfc_F` quantization
// `(20, 40, 32, 48, 20, 20)`, applied over -60..120 °F (`plot_helper.py`
// `contour_levels_F_sfc = (-60, 120, 1, 10)`). The quantizations map each
// gradient segment 1:1 onto its °F range, so anchors sit at exact °F values.
// ---------------------------------------------------------------------------

const SOLAR_TEMPERATURE_F_ANCHORS: [(f32, &str); 22] = [
    (-60.0, "#2b5d7e"),
    (-50.0, "#75a8b0"),
    (-40.0, "#B0E6DE"),
    (-30.0, "#a0b8d6"),
    (-20.0, "#968bc5"),
    (-10.0, "#8243b2"),
    (-0.5, "#7627A4"),
    (0.0, "#A040B2"),
    (16.0, "#f7f7ff"),
    (31.5, "#1D55B1"),
    (32.0, "#0F4454"),
    (41.6, "#88A080"),
    (51.2, "#F8EEA2"),
    (60.8, "#AA714D"),
    (70.4, "#5F0000"),
    (79.5, "#852C40"),
    (80.0, "#73372D"),
    (90.0, "#B69389"),
    (99.5, "#F2E6DC"),
    (100.0, "#E9DFD6"),
    (110.0, "#95918F"),
    (120.0, "#464646"),
];

fn solar_temperature_table(unit: TempUnit) -> ColorTable {
    interpolated_from_anchors("Solar Temperature", &SOLAR_TEMPERATURE_F_ANCHORS, |f| {
        fahrenheit_to(f, unit)
    })
}

// ---------------------------------------------------------------------------
// Dew point — Solar's `get_dew_point_colormap(80, 50)` (`sfc_F`), applied over
// -40..90 °F (`plot_helper.py::plot_dewpoint`, `contour_levels['sfc_F'] =
// (-40, 90, 1, 10)`). Dry block spans -40..40 °F; five moist blocks span
// 40..90 °F at 10 °F each.
// ---------------------------------------------------------------------------

const SOLAR_DEWPOINT_F_ANCHORS: [(f32, &str); 13] = [
    (-40.0, "#996f4f"),
    (0.0, "#4d4236"),
    (39.5, "#f2f2d8"),
    (40.0, "#e3f3e6"),
    (49.5, "#64c461"),
    (50.0, "#32ae32"),
    (59.5, "#084d06"),
    (60.0, "#66a3ad"),
    (69.5, "#12292a"),
    (70.0, "#66679d"),
    (79.5, "#2b1e63"),
    (80.0, "#714270"),
    (90.0, "#a27382"),
];

fn solar_dewpoint_table(unit: TempUnit) -> ColorTable {
    interpolated_from_anchors("Solar Dew Point", &SOLAR_DEWPOINT_F_ANCHORS, |f| {
        fahrenheit_to(f, unit)
    })
}

// ---------------------------------------------------------------------------
// Wind speed — Solar's `WINDS_COLORMAP_BASE` (`get_winds_colormap`), applied
// over the surface range 10..70 kt (`plot_helper.py` wind `ranges['sfc']`).
// 13 colours evenly spaced every 5 kt; calm (< 10 kt) held white.
// ---------------------------------------------------------------------------

const SOLAR_WIND_KT_ANCHORS: [(f32, &str); 14] = [
    (0.0, "#ffffff"),
    (10.0, "#ffffff"),
    (15.0, "#87cefa"),
    (20.0, "#6a5acd"),
    (25.0, "#e696dc"),
    (30.0, "#c85abe"),
    (35.0, "#a01496"),
    (40.0, "#c80028"),
    (45.0, "#dc283c"),
    (50.0, "#f05050"),
    (55.0, "#faf064"),
    (60.0, "#dcbe46"),
    (65.0, "#be8c28"),
    (70.0, "#a05a0a"),
];

fn solar_wind_speed_table(unit: SpeedUnit) -> ColorTable {
    interpolated_from_anchors("Solar Wind Speed", &SOLAR_WIND_KT_ANCHORS, |kt| {
        knots_to(kt, unit)
    })
}

// ---------------------------------------------------------------------------
// Precip — Solar's `PRECIP_COLORMAP_IN` (`create_custom_cmap`, 1500 colours),
// applied over 0..15 in (`plot_helper.py::plot_precip`, `unit == 'in'`). Block
// value edges follow the quantizations [1, 9, 40, 50, 100, 200, 1100].
// ---------------------------------------------------------------------------

const SOLAR_PRECIP_IN_ANCHORS: [(f32, &str); 16] = [
    (0.0, "#ffffff"),
    (0.01, "#dcdcdc"),
    (0.04, "#bebebe"),
    (0.07, "#9e9e9e"),
    (0.099, "#818181"),
    (0.1, "#b8f0c1"),
    (0.499, "#156471"),
    (0.5, "#164fba"),
    (0.999, "#d8edf5"),
    (1.0, "#cfbddd"),
    (1.999, "#a134b1"),
    (2.0, "#a43c32"),
    (3.999, "#dd9c98"),
    (4.0, "#f6f0a3"),
    (9.5, "#7e4b26"),
    (15.0, "#542f17"),
];

fn solar_precip_table(unit: DepthUnit) -> ColorTable {
    let table =
        interpolated_from_anchors("Solar Precipitation", &SOLAR_PRECIP_IN_ANCHORS, |inches| {
            inches_to(inches, unit)
        });
    // Below 0.01 in (0.254 mm) is effectively dry — hold it transparent so the
    // basemap shows through rather than washing white.
    table.with_display_threshold(Some(inches_to(0.01, unit)), false)
}

// ---------------------------------------------------------------------------
// Relative humidity — Solar's `RH_COLORMAP` (`create_custom_cmap`, quant
// [40, 50, 10]), applied over 0..100 % (`plot_helper.py::plot_rh`).
// ---------------------------------------------------------------------------

const SOLAR_RH_ANCHORS: [(f32, &str); 9] = [
    (0.0, "#a5734d"),
    (10.0, "#382f28"),
    (20.0, "#6e6559"),
    (30.0, "#a59b8e"),
    (39.5, "#ddd1c3"),
    (40.0, "#c8d7c0"),
    (89.5, "#004a2f"),
    (90.0, "#004123"),
    (100.0, "#28588c"),
];

/// Solar's relative-humidity palette (%). Credit: Solarpower07 / WRF-Runner
/// `RH_COLORMAP`.
pub fn solar_relative_humidity_table() -> ColorTable {
    interpolated_from_anchors("Solar Relative Humidity", &SOLAR_RH_ANCHORS, |v| v)
}

// ---------------------------------------------------------------------------
// CAPE — Solar's `CAPE_COLORMAP` = `get_composite_colormap([10,10,10,10,10,10,
// 20])` (7 active blocks, 80 colours), applied over 0..8000 J/kg
// (`plot_helper.py::plot_cape`, `np.arange(0, 8001, 100)`).
// ---------------------------------------------------------------------------

const SOLAR_CAPE_ANCHORS: [(f32, &str); 14] = [
    (0.0, "#ffffff"),
    (999.0, "#696969"),
    (1000.0, "#37536a"),
    (1999.0, "#a7c8ce"),
    (2000.0, "#e9dd96"),
    (2999.0, "#e16f02"),
    (3000.0, "#dc4110"),
    (3999.0, "#8b0950"),
    (4000.0, "#73088a"),
    (4999.0, "#da99e7"),
    (5000.0, "#e9bec3"),
    (5999.0, "#b2445a"),
    (6000.0, "#893d48"),
    (8000.0, "#bc9195"),
];

/// Solar's CAPE palette (J/kg), the "composite" severe ramp over 0..8000.
/// Credit: Solarpower07 / WRF-Runner `CAPE_COLORMAP` / `get_composite_colormap`.
pub fn solar_cape_table() -> ColorTable {
    let table = interpolated_from_anchors("Solar CAPE", &SOLAR_CAPE_ANCHORS, |v| v);
    // Trivial CAPE (< 100 J/kg) reads as dry — transparent, not white.
    table.with_display_threshold(Some(100.0), false)
}

/// Solar's "composite" severe ramp stretched over an arbitrary positive range
/// — used for helicity / SRH / updraft-helicity fields, which share the same
/// `get_composite_colormap` base in WRF-Runner. Credit: Solarpower07 /
/// WRF-Runner `get_composite_colormap`.
fn solar_composite_severe_table(name: &str, vmax: f32) -> ColorTable {
    // Same 7-block composite base as CAPE, rescaled 0..vmax.
    let anchors: [(f32, &str); 8] = [
        (0.0, "#ffffff"),
        (0.14, "#696969"),
        (0.29, "#a7c8ce"),
        (0.43, "#e16f02"),
        (0.57, "#8b0950"),
        (0.71, "#da99e7"),
        (0.86, "#b2445a"),
        (1.0, "#19191a"),
    ];
    let table = interpolated_from_anchors(name, &anchors, |t| t * vmax);
    table.with_display_threshold(Some(0.02 * vmax), false)
}

// ---------------------------------------------------------------------------
// Relative vorticity — Solar's `RVORT_COLORMAP` (`get_relvort_colormap`),
// applied over -40..60 (×1e-5 s⁻¹) (`plot_helper.py::plot_relvort`,
// `np.arange(-40, 60, 1)`).
// ---------------------------------------------------------------------------

const SOLAR_RVORT_ANCHORS: [(f32, &str); 21] = [
    (-40.0, "#323232"),
    (-35.0, "#4d4d4d"),
    (-30.0, "#707070"),
    (-25.0, "#8A8A8A"),
    (-20.0, "#a1a1a1"),
    (-15.0, "#c0c0c0"),
    (-10.0, "#d6d6d6"),
    (-5.0, "#e5e5e5"),
    (0.0, "#ffffff"),
    (5.0, "#fdd244"),
    (10.0, "#fea000"),
    (15.0, "#f16702"),
    (20.0, "#da2422"),
    (25.0, "#ab029b"),
    (30.0, "#78008f"),
    (35.0, "#44008b"),
    (40.0, "#000160"),
    (45.0, "#244488"),
    (50.0, "#4f85b2"),
    (55.0, "#73cadb"),
    (60.0, "#91fffd"),
];

/// Solar's relative-vorticity palette (×1e-5 s⁻¹). Credit: Solarpower07 /
/// WRF-Runner `RVORT_COLORMAP`.
pub fn solar_vorticity_table() -> ColorTable {
    interpolated_from_anchors("Solar Relative Vorticity", &SOLAR_RVORT_ANCHORS, |v| v)
}

// --- unit conversions ------------------------------------------------------

fn fahrenheit_to(f: f32, unit: TempUnit) -> f32 {
    match unit {
        TempUnit::Fahrenheit => f,
        TempUnit::Celsius => (f - 32.0) * 5.0 / 9.0,
        TempUnit::Kelvin => (f - 32.0) * 5.0 / 9.0 + 273.15,
    }
}

fn knots_to(kt: f32, unit: SpeedUnit) -> f32 {
    match unit {
        SpeedUnit::Knots => kt,
        SpeedUnit::MetersPerSecond => kt * 0.514_444,
        SpeedUnit::MilesPerHour => kt * 1.150_779,
    }
}

fn inches_to(inches: f32, unit: DepthUnit) -> f32 {
    match unit {
        DepthUnit::Inches => inches,
        DepthUnit::Millimetres => inches * 25.4,
    }
}

// --- field -> table resolver ----------------------------------------------

/// Classify a temperature-family field's units into a [`TempUnit`], defaulting
/// to Kelvin (WRF store native) when the string is unrecognized.
fn temp_unit_of(units: &str) -> TempUnit {
    let u = units.trim().to_ascii_lowercase();
    if u.contains('f') || u.contains("fahren") {
        TempUnit::Fahrenheit
    } else if u == "c" || u.contains("celsius") || u.contains("degc") || u == "°c" {
        TempUnit::Celsius
    } else {
        TempUnit::Kelvin
    }
}

fn speed_unit_of(units: &str) -> SpeedUnit {
    let u = units.trim().to_ascii_lowercase();
    if u.contains("kt") || u.contains("kts") || u.contains("knot") {
        SpeedUnit::Knots
    } else if u.contains("mph") || u.contains("mile") {
        SpeedUnit::MilesPerHour
    } else {
        SpeedUnit::MetersPerSecond
    }
}

fn depth_unit_of(units: &str) -> DepthUnit {
    let u = units.trim().to_ascii_lowercase();
    if u.contains("in") {
        DepthUnit::Inches
    } else {
        // kg/m^2, mm, and unknowns all treat as mm (kg/m² == mm water).
        DepthUnit::Millimetres
    }
}

/// Resolve a model / WRF field to a Solarpower07 WRF-Runner colormap keyed to
/// the field's own units, or `None` when the field has no Solar counterpart
/// (the caller then keeps its existing production style or generic ramp).
///
/// Matching is by the stored variable name plus a unit sanity check, so it
/// works for both raw WRF store names (`temperature_2m`, `wind_speed_10m`,
/// `composite_reflectivity`, `apcp`, …) and styled fields whose values were
/// already converted to display units.
///
/// CREDIT: every table returned here is a port of a Solarpower07 WRF-Runner
/// palette (see module docs).
pub fn solar_model_field_table(var: &str, units: &str) -> Option<ColorTable> {
    let name = var.to_ascii_lowercase();
    let unit = units.trim().to_ascii_lowercase();

    // Reflectivity (dBZ). Solar's flagship "PW Style".
    if unit == "dbz"
        || name.contains("reflectivity")
        || name.contains("dbz")
        || name == "maxdbz"
        || name.contains("cref")
    {
        return Some(solar_reflectivity_table());
    }

    // Dew point (check before the broader temperature match).
    if name.contains("dewpoint") || name.contains("dew_point") || name.contains("dewpt") {
        return Some(solar_dewpoint_table(temp_unit_of(units)));
    }

    // Temperature (2 m / skin / sea-surface). Guard on a temperature unit so a
    // stray "temp" substring in a non-thermal field can't hijack the palette.
    let temp_unitish = matches!(
        unit.as_str(),
        "k" | "kelvin" | "c" | "degc" | "°c" | "f" | "degf" | "°f" | "fahrenheit" | "celsius"
    );
    if temp_unitish
        && (name.contains("temperature")
            || name == "t2"
            || name == "t2m"
            || name.contains("tsk")
            || name.contains("sst")
            || name.contains("skin_temp")
            || name.contains("sea_surface"))
    {
        return Some(solar_temperature_table(temp_unit_of(units)));
    }

    // Relative humidity (%).
    if name.contains("relative_humidity") || name == "rh" || name.contains("rh2m") {
        return Some(solar_relative_humidity_table());
    }

    // Wind speed / gust magnitude.
    if name.contains("wind_speed")
        || name.contains("windspeed")
        || name.contains("wspd")
        || name.contains("gust")
        || name == "wspd10max"
    {
        return Some(solar_wind_speed_table(speed_unit_of(units)));
    }

    // Accumulated precip / precipitable water (kg/m² == mm, or inches).
    if name.contains("precip")
        || name == "apcp"
        || name.contains("rain")
        || name == "pwat"
        || name.contains("pwat")
        || name.contains("qpf")
    {
        return Some(solar_precip_table(depth_unit_of(units)));
    }

    // CAPE (J/kg).
    if name.contains("cape") {
        return Some(solar_cape_table());
    }

    // Helicity / SRH / updraft helicity — the composite severe ramp.
    if name.contains("helicity")
        || name.contains("srh")
        || name.contains("uhel")
        || name.contains("up_heli")
        || name.contains("updraft_helicity")
    {
        return Some(solar_composite_severe_table("Solar Helicity", 600.0));
    }

    // Relative vorticity.
    if name.contains("vort") {
        return Some(solar_vorticity_table());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parses_six_digit_colours() {
        assert_eq!(hex("#ffffff"), Rgba8::opaque(255, 255, 255));
        assert_eq!(hex("#000000"), Rgba8::opaque(0, 0, 0));
        assert_eq!(hex("#1350b4"), Rgba8::opaque(0x13, 0x50, 0xb4));
    }

    #[test]
    fn reflectivity_is_pw_style_and_masks_clear_air() {
        let table = solar_reflectivity_table();
        // Below 5 dBZ transparent (clear-air mask).
        assert_eq!(table.sample(0.0), Rgba8::TRANSPARENT);
        // First bin [5, 7.5) is Solar's white; a strong core is opaque.
        assert_eq!(table.sample(6.0), hex("#ffffff"));
        assert!(table.sample(70.0).a > 0);
    }

    #[test]
    fn temperature_resolver_is_unit_aware() {
        // Kelvin store field: freezing (273.15 K == 32 °F) lands on the 32 °F
        // anchor colour, not on a °F-keyed miss.
        let kelvin = solar_model_field_table("temperature_2m", "K").expect("temp table");
        let at_freezing_k = kelvin.sample(273.15);
        let fahrenheit = solar_temperature_table(TempUnit::Fahrenheit);
        assert_eq!(at_freezing_k, fahrenheit.sample(32.0));
        // A degF-styled field maps freezing at 32.
        let degf = solar_model_field_table("temperature_2m", "degF").expect("temp table");
        assert_eq!(degf.sample(32.0), fahrenheit.sample(32.0));
    }

    #[test]
    fn resolver_maps_the_core_wrf_fields() {
        assert!(solar_model_field_table("composite_reflectivity", "dBZ").is_some());
        assert!(solar_model_field_table("dewpoint_2m", "K").is_some());
        assert!(solar_model_field_table("wind_speed_10m", "m/s").is_some());
        assert!(solar_model_field_table("apcp", "kg/m^2").is_some());
        assert!(solar_model_field_table("relative_humidity_2m", "%").is_some());
        assert!(solar_model_field_table("sbcape", "J/kg").is_some());
        assert!(solar_model_field_table("updraft_helicity_2to5km", "m2/s2").is_some());
        // No Solar counterpart -> None (keep generic/production).
        assert!(solar_model_field_table("orography", "m").is_none());
        assert!(solar_model_field_table("surface_pressure", "Pa").is_none());
    }

    #[test]
    fn dewpoint_wins_over_temperature_match() {
        // "dewpoint_2m" must not be captured by the temperature branch.
        let dp = solar_model_field_table("dewpoint_2m", "K").expect("dewpoint");
        let expected = solar_dewpoint_table(TempUnit::Kelvin);
        assert_eq!(dp.sample(280.0), expected.sample(280.0));
    }

    #[test]
    fn every_solar_table_builds() {
        // Guards the anchor arrays (>= 2 stops, finite values) at test time.
        let _ = solar_reflectivity_table();
        let _ = solar_temperature_table(TempUnit::Kelvin);
        let _ = solar_dewpoint_table(TempUnit::Celsius);
        let _ = solar_wind_speed_table(SpeedUnit::MetersPerSecond);
        let _ = solar_precip_table(DepthUnit::Millimetres);
        let _ = solar_relative_humidity_table();
        let _ = solar_cape_table();
        let _ = solar_composite_severe_table("Solar Helicity", 600.0);
        let _ = solar_vorticity_table();
    }
}
