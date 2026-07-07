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
// Temperature at pressure levels + the °C-native surface variant — Solar's
// `get_temp_colormap` with the per-level quantizations from `plot_helper.py`
// (`cmap_segments_data` / `contour_levels_C` in `plot_temperature`). Same six
// gradient blocks as the sfc_F table above (3, 5, 3, 6, 3, 3 colours); a
// quantization of 0 means that block is absent at that level.
//
// At every level the total quantization equals the °C span of the contour
// range, so one quantization step is exactly 1 °C: each gradient block spans
// the interval between its cumulative-quantization boundaries and its colours
// sit evenly across that interval, giving anchors at exact °C values. As in
// the sfc_F port, a block's closing anchor is nudged 0.5 °C below the
// boundary wherever the next block opens with a different colour.
// Credit: Solarpower07 / WRF-Runner `get_temp_colormap` + `plot_temperature`.
// ---------------------------------------------------------------------------

// 250 mb — quantizations (0, 15, 10, 20, 5, 0) over -70..-20 °C.
// Blocks: -70..-55 (5 colours, step 3.75), -55..-45 (3, step 5),
// -45..-25 (6, step 4), -25..-20 (3, step 2.5).
const SOLAR_TEMPERATURE_C_250_ANCHORS: [(f32, &str); 17] = [
    (-70.0, "#B0E6DE"),
    (-66.25, "#a0b8d6"),
    (-62.5, "#968bc5"),
    (-58.75, "#8243b2"),
    (-55.5, "#7627A4"),
    (-55.0, "#A040B2"),
    (-50.0, "#f7f7ff"),
    (-45.5, "#1D55B1"),
    (-45.0, "#0F4454"),
    (-41.0, "#88A080"),
    (-37.0, "#F8EEA2"),
    (-33.0, "#AA714D"),
    (-29.0, "#5F0000"),
    (-25.5, "#852C40"),
    (-25.0, "#73372D"),
    (-22.5, "#B69389"),
    (-20.0, "#F2E6DC"),
];

// 500 mb — quantizations (0, 20, 15, 20, 5, 0) over -50..10 °C.
// Blocks: -50..-30 (5 colours, step 5), -30..-15 (3, step 7.5),
// -15..5 (6, step 4), 5..10 (3, step 2.5).
const SOLAR_TEMPERATURE_C_500_ANCHORS: [(f32, &str); 17] = [
    (-50.0, "#B0E6DE"),
    (-45.0, "#a0b8d6"),
    (-40.0, "#968bc5"),
    (-35.0, "#8243b2"),
    (-30.5, "#7627A4"),
    (-30.0, "#A040B2"),
    (-22.5, "#f7f7ff"),
    (-15.5, "#1D55B1"),
    (-15.0, "#0F4454"),
    (-11.0, "#88A080"),
    (-7.0, "#F8EEA2"),
    (-3.0, "#AA714D"),
    (1.0, "#5F0000"),
    (4.5, "#852C40"),
    (5.0, "#73372D"),
    (7.5, "#B69389"),
    (10.0, "#F2E6DC"),
];

// 700 mb — quantizations (0, 20, 15, 25, 10, 0) over -40..30 °C.
// Blocks: -40..-20 (5 colours, step 5), -20..-5 (3, step 7.5),
// -5..20 (6, step 5), 20..30 (3, step 5).
const SOLAR_TEMPERATURE_C_700_ANCHORS: [(f32, &str); 17] = [
    (-40.0, "#B0E6DE"),
    (-35.0, "#a0b8d6"),
    (-30.0, "#968bc5"),
    (-25.0, "#8243b2"),
    (-20.5, "#7627A4"),
    (-20.0, "#A040B2"),
    (-12.5, "#f7f7ff"),
    (-5.5, "#1D55B1"),
    (-5.0, "#0F4454"),
    (0.0, "#88A080"),
    (5.0, "#F8EEA2"),
    (10.0, "#AA714D"),
    (15.0, "#5F0000"),
    (19.5, "#852C40"),
    (20.0, "#73372D"),
    (25.0, "#B69389"),
    (30.0, "#F2E6DC"),
];

// 850 mb — quantizations (0, 20, 20, 30, 10, 10) over -40..50 °C.
// Blocks: -40..-20 (5 colours, step 5), -20..0 (3, step 10), 0..30 (6, step
// 6), 30..40 (3, step 5), 40..50 (3, step 5). The only level where the last
// (grey/haze) gradient block is present.
const SOLAR_TEMPERATURE_C_850_ANCHORS: [(f32, &str); 20] = [
    (-40.0, "#B0E6DE"),
    (-35.0, "#a0b8d6"),
    (-30.0, "#968bc5"),
    (-25.0, "#8243b2"),
    (-20.5, "#7627A4"),
    (-20.0, "#A040B2"),
    (-10.0, "#f7f7ff"),
    (-0.5, "#1D55B1"),
    (0.0, "#0F4454"),
    (6.0, "#88A080"),
    (12.0, "#F8EEA2"),
    (18.0, "#AA714D"),
    (24.0, "#5F0000"),
    (29.5, "#852C40"),
    (30.0, "#73372D"),
    (35.0, "#B69389"),
    (39.5, "#F2E6DC"),
    (40.0, "#E9DFD6"),
    (45.0, "#95918F"),
    (50.0, "#464646"),
];

// WRF-Runner's °C-native surface variant (`sfc_C`) uses the exact same
// quantizations (0, 20, 20, 30, 10, 10) and -40..50 °C contour range as the
// 850 mb level, so it shares the anchor array.
const SOLAR_TEMPERATURE_SFC_C_ANCHORS: [(f32, &str); 20] = SOLAR_TEMPERATURE_C_850_ANCHORS;

/// Solar's per-pressure-level temperature palette (°C-anchored), converted to
/// the field's unit. WRF-Runner defines exactly 250 / 500 / 700 / 850 mb.
/// Credit: Solarpower07 / WRF-Runner `get_temp_colormap` + `plot_temperature`.
fn solar_temperature_level_table(level_mb: u16, unit: TempUnit) -> Option<ColorTable> {
    let (name, anchors): (&str, &[(f32, &str)]) = match level_mb {
        250 => ("Solar Temperature 250mb", &SOLAR_TEMPERATURE_C_250_ANCHORS),
        500 => ("Solar Temperature 500mb", &SOLAR_TEMPERATURE_C_500_ANCHORS),
        700 => ("Solar Temperature 700mb", &SOLAR_TEMPERATURE_C_700_ANCHORS),
        850 => ("Solar Temperature 850mb", &SOLAR_TEMPERATURE_C_850_ANCHORS),
        _ => return None,
    };
    Some(interpolated_from_anchors(name, anchors, |c| {
        celsius_to(c, unit)
    }))
}

/// Solar's °C-native surface temperature palette (`sfc_C`). Credit:
/// Solarpower07 / WRF-Runner `get_temp_colormap` + `plot_temperature`.
fn solar_temperature_sfc_c_table(unit: TempUnit) -> ColorTable {
    interpolated_from_anchors(
        "Solar Temperature Surface °C",
        &SOLAR_TEMPERATURE_SFC_C_ANCHORS,
        |c| celsius_to(c, unit),
    )
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

fn celsius_to(c: f32, unit: TempUnit) -> f32 {
    match unit {
        TempUnit::Fahrenheit => c * 9.0 / 5.0 + 32.0,
        TempUnit::Celsius => c,
        TempUnit::Kelvin => c + 273.15,
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

/// Parse a pressure level (mb) out of a stored variable name
/// (`temperature_850`, `t_850mb`, `temp850`, `temperature_500hpa`, …).
///
/// Guard rails: only WRF-Runner's temperature levels 250/500/700/850 count,
/// the digit run must be exactly the level (so the "2" in `temperature_2m` or
/// a stray `8500` never match), and the run must be followed by nothing, a
/// separator, or an explicit `mb`/`hpa` suffix (so `..._850m` — metres — does
/// not read as a pressure level). Expects an already-lowercased name.
fn parse_pressure_level_mb(name: &str) -> Option<u16> {
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let level = match &name[start..i] {
            "250" => 250_u16,
            "500" => 500,
            "700" => 700,
            "850" => 850,
            _ => continue,
        };
        let rest = &name[i..];
        let mb_ish = rest.is_empty()
            || rest.starts_with("mb")
            || rest.starts_with("hpa")
            || !rest.starts_with(|c: char| c.is_ascii_alphanumeric());
        if mb_ish {
            return Some(level);
        }
    }
    None
}

/// True when the (already-lowercased) var name reads as a temperature field.
/// Bare `t*` / `temp*` short forms only count when a pressure level was
/// parsed, so stray `t`-prefixed non-thermal fields can't hijack the palette.
fn is_temperature_var(name: &str, level_mb: Option<u16>) -> bool {
    if name.contains("temperature")
        || name == "t2"
        || name == "t2m"
        || name.contains("tsk")
        || name.contains("sst")
        || name.contains("skin_temp")
        || name.contains("sea_surface")
    {
        return true;
    }
    if level_mb.is_none() {
        return false;
    }
    // Short forms with an explicit level: `temp850`, `temp_850mb`, `t_850mb`,
    // `t850`.
    if let Some(rest) = name.strip_prefix("temp").or_else(|| name.strip_prefix('t')) {
        let rest = rest.strip_prefix('_').unwrap_or(rest);
        return rest.starts_with(|c: char| c.is_ascii_digit());
    }
    false
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
/// Raw `wrf_*`-prefixed passthrough fields (the WRF Registry names the
/// imports store verbatim, e.g. `wrf_swupt`, `wrf_hfx`) resolve FIRST through
/// the [`crate::wrf_fields`] catalog's family hints — Solar tables where a
/// family exists, existing BowEcho ramps rescaled over a physical range
/// otherwise — so the standard diagnostic set never falls to a meaningless
/// normalized default. Catalog entries with no assigned family fall through
/// to the name heuristics below unchanged.
///
/// CREDIT: every table returned here is a port of a Solarpower07 WRF-Runner
/// palette (see module docs).
pub fn solar_model_field_table(var: &str, units: &str) -> Option<ColorTable> {
    let name = var.to_ascii_lowercase();
    let unit = units.trim().to_ascii_lowercase();

    // Exact-match raw-WRF catalog first: these names are WRF Registry
    // mnemonics the substring heuristics below can't classify (and could
    // misclassify — e.g. nothing in `wrf_snownc` says "precip").
    if let Some(info) = crate::wrf_fields::wrf_field_info(&name)
        && let Some(table) = wrf_family_table(info.family, units)
    {
        return Some(table);
    }

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

    // Temperature (pressure level / 2 m / skin / sea-surface). Guard on a
    // temperature unit so a stray "temp" substring in a non-thermal field
    // can't hijack the palette.
    let temp_unitish = matches!(
        unit.as_str(),
        "k" | "kelvin" | "c" | "degc" | "°c" | "f" | "degf" | "°f" | "fahrenheit" | "celsius"
    );
    if temp_unitish {
        let level_mb = parse_pressure_level_mb(&name);
        if is_temperature_var(&name, level_mb) {
            let temp_unit = temp_unit_of(units);
            // Pressure-level fields (250/500/700/850 mb) get Solar's per-level
            // °C-anchored palettes; `parse_pressure_level_mb` only ever yields
            // those four levels, so the lookup below always succeeds.
            if let Some(level_mb) = level_mb {
                return solar_temperature_level_table(level_mb, temp_unit);
            }
            // Surface (no level in the name):
            // * °C-styled fields use Solar's native `sfc_C` palette.
            // * °F and Kelvin fields keep the F-anchored surface table
            //   (converted) — deliberately unchanged so Kelvin-stored WRF
            //   fields preserve the shipped v0.29.3 default look.
            return Some(match temp_unit {
                TempUnit::Celsius => solar_temperature_sfc_c_table(temp_unit),
                TempUnit::Fahrenheit | TempUnit::Kelvin => solar_temperature_table(temp_unit),
            });
        }
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

/// Resolve a [`crate::wrf_fields`] family hint to a concrete table, keyed to
/// the field's stored units where the family is unit-aware. `Unassigned`
/// yields `None` so the caller's name heuristics / generic fallback still
/// apply. Every ramp returned here already exists in BowEcho — the
/// parameterized families only RESCALE existing ramps (colors verbatim), they
/// never introduce a new palette.
fn wrf_family_table(family: crate::wrf_fields::WrfColorFamily, units: &str) -> Option<ColorTable> {
    use crate::wrf_fields::WrfColorFamily as F;
    match family {
        // Same unit-aware surface behavior as the temperature heuristic
        // below: °C-styled fields take Solar's native sfc_C palette, °F and
        // Kelvin keep the F-anchored surface table.
        F::Temperature => Some(match temp_unit_of(units) {
            TempUnit::Celsius => solar_temperature_sfc_c_table(TempUnit::Celsius),
            unit @ (TempUnit::Fahrenheit | TempUnit::Kelvin) => solar_temperature_table(unit),
        }),
        F::Dewpoint => Some(solar_dewpoint_table(temp_unit_of(units))),
        F::RelativeHumidity => Some(solar_relative_humidity_table()),
        F::WindSpeed => Some(solar_wind_speed_table(speed_unit_of(units))),
        F::PrecipDepth => Some(solar_precip_table(depth_unit_of(units))),
        F::Reflectivity => Some(solar_reflectivity_table()),
        F::Cape => Some(solar_cape_table()),
        F::Helicity => Some(solar_composite_severe_table("Solar Helicity", 600.0)),
        F::HailSize => Some(crate::builtin_hail_size_table()),
        F::Percent => Some(crate::builtin_probability_table()),
        F::Fraction => Some(rescaled_table(
            &crate::builtin_probability_table(),
            "Analyst Probability (fraction)",
            0.0,
            1.0,
        )),
        F::Composite { vmax } => Some(solar_composite_severe_table("Solar Composite", vmax)),
        F::Sequential { lo, hi } => Some(rescaled_table(
            &crate::builtin_generic_table(),
            "Analyst Generic (scaled)",
            lo,
            hi,
        )),
        F::Diverging { max_abs } => Some(rescaled_table(
            &crate::balance_velocity_table(),
            "Balance Diverging (scaled)",
            -max_abs,
            max_abs,
        )),
        F::Unassigned => None,
    }
}

/// Linearly remap an existing table's stops onto `lo..hi`, keeping every
/// color verbatim — palette reuse, not palette invention. The base tables
/// used here are interpolated ramps with ≥ 2 distinct stop values, so the
/// span division is well-formed by construction.
fn rescaled_table(base: &ColorTable, name: &str, lo: f32, hi: f32) -> ColorTable {
    let stops = base.stops();
    let first = stops.first().map_or(0.0, |stop| stop.value);
    let last = stops.last().map_or(1.0, |stop| stop.value);
    let span = last - first;
    let rescaled = stops
        .iter()
        .map(|stop| ColorStop {
            value: lo + (stop.value - first) / span * (hi - lo),
            ..*stop
        })
        .collect();
    ColorTable::new(name, rescaled).expect("rescaling a valid table keeps it valid")
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
    fn level_parsing_resolves_level_tables() {
        // Each supported spelling routes to the 850 mb °C-anchored table.
        let expect_850 = solar_temperature_level_table(850, TempUnit::Kelvin).expect("850 table");
        for var in ["temperature_850", "t_850mb", "temp850", "TEMPERATURE_850MB"] {
            let table = solar_model_field_table(var, "K").expect("850 temp table");
            assert_eq!(table.sample(273.15), expect_850.sample(273.15), "{var}");
        }
        // The other levels each hit their own table: the same physical value
        // shades differently because every level stretches the gradient
        // blocks over a different °C range.
        let resolved = |mb: u16| {
            solar_model_field_table(&format!("temperature_{mb}"), "degC")
                .expect("level table")
                .sample(-30.0)
        };
        for mb in [250_u16, 500, 700] {
            let direct = solar_temperature_level_table(mb, TempUnit::Celsius).expect("table");
            assert_eq!(resolved(mb), direct.sample(-30.0), "{mb} mb");
        }
        assert_ne!(resolved(250), resolved(700));
    }

    #[test]
    fn level_parsing_guards_against_non_levels() {
        // Height-ish / oversized digit runs never read as pressure levels.
        assert_eq!(parse_pressure_level_mb("temperature_2m"), None);
        assert_eq!(parse_pressure_level_mb("temperature_8500"), None);
        assert_eq!(parse_pressure_level_mb("temperature_850m"), None); // metres
        assert_eq!(parse_pressure_level_mb("temperature_850"), Some(850));
        assert_eq!(parse_pressure_level_mb("t_850mb"), Some(850));
        assert_eq!(parse_pressure_level_mb("temp250"), Some(250));
        assert_eq!(parse_pressure_level_mb("temperature_500hpa"), Some(500));
        // 2 m temperature keeps the surface behaviour even though it has a
        // digit in its name.
        let sfc = solar_model_field_table("temperature_2m", "K").expect("sfc temp");
        let expected = solar_temperature_table(TempUnit::Kelvin);
        assert_eq!(sfc.sample(300.0), expected.sample(300.0));
    }

    #[test]
    fn level_anchor_tables_span_exact_ranges() {
        // First/last anchors sit exactly at plot_helper.py's contour range
        // ends (`contour_levels_C`).
        let cases = [
            (&SOLAR_TEMPERATURE_C_250_ANCHORS[..], -70.0, -20.0),
            (&SOLAR_TEMPERATURE_C_500_ANCHORS[..], -50.0, 10.0),
            (&SOLAR_TEMPERATURE_C_700_ANCHORS[..], -40.0, 30.0),
            (&SOLAR_TEMPERATURE_C_850_ANCHORS[..], -40.0, 50.0),
            (&SOLAR_TEMPERATURE_SFC_C_ANCHORS[..], -40.0, 50.0),
        ];
        for (anchors, lo, hi) in cases {
            assert_eq!(anchors.first().expect("anchors").0, lo);
            assert_eq!(anchors.last().expect("anchors").0, hi);
        }
    }

    #[test]
    fn degc_surface_resolves_sfc_c_table() {
        // A °C-styled surface field gets Solar's native sfc_C palette, under
        // every recognized °C unit spelling…
        let native = solar_temperature_sfc_c_table(TempUnit::Celsius);
        for units in ["C", "degC", "°C", "celsius"] {
            let table = solar_model_field_table("temperature_2m", units).expect("sfc temp");
            for v in [-40.0, -5.0, 20.0, 45.0, 50.0] {
                assert_eq!(table.sample(v), native.sample(v), "{units} @ {v}");
            }
        }
        // …which really is a different stretch from the F-anchored surface
        // table converted to °C (45 °C is an exact sfc_C anchor, but sits
        // mid-gradient at 113 °F in the F-anchored table).
        let f_in_c = solar_temperature_table(TempUnit::Celsius);
        assert_ne!(native.sample(45.0), f_in_c.sample(45.0));
    }

    #[test]
    fn kelvin_and_fahrenheit_surface_behavior_unchanged() {
        // Kelvin-stored WRF surface fields keep the shipped v0.29.3
        // F-anchored look (converted to K), NOT the new sfc_C palette.
        let kelvin = solar_model_field_table("temperature_2m", "K").expect("sfc temp");
        let expected_k = solar_temperature_table(TempUnit::Kelvin);
        for v in [233.15, 273.15, 300.0, 322.0] {
            assert_eq!(kelvin.sample(v), expected_k.sample(v), "K @ {v}");
        }
        // °F-styled fields likewise keep the F-anchored table.
        let degf = solar_model_field_table("t2m", "degF").expect("sfc temp");
        let expected_f = solar_temperature_table(TempUnit::Fahrenheit);
        assert_eq!(degf.sample(72.0), expected_f.sample(72.0));
    }

    /// Raw radiation-budget fields (WRF Registry names) resolve the EXISTING
    /// Analyst Generic ramp rescaled over their physical W/m² range — colors
    /// verbatim from the base ramp, ends pinned to the base ramp's ends.
    #[test]
    fn raw_wrf_radiation_fields_resolve_the_scaled_sequential_ramp() {
        let generic = crate::builtin_generic_table();
        for (var, lo, hi) in [
            ("wrf_swupt", 0.0, 1100.0),
            ("wrf_swuptc", 0.0, 1100.0),
            ("wrf_swdnt", 0.0, 1400.0),
            ("wrf_swdntc", 0.0, 1400.0),
            ("wrf_swupb", 0.0, 1100.0),
            ("wrf_swupbc", 0.0, 1100.0),
            ("wrf_swdnb", 0.0, 1100.0),
            ("wrf_swdnbc", 0.0, 1100.0),
            ("wrf_swdown", 0.0, 1100.0),
            ("wrf_lwupt", 80.0, 340.0),
            ("wrf_lwuptc", 80.0, 340.0),
            ("wrf_lwupb", 200.0, 650.0),
            ("wrf_lwdnb", 100.0, 500.0),
            ("wrf_lwdnbc", 100.0, 500.0),
            ("wrf_glw", 100.0, 500.0),
            ("wrf_olr", 80.0, 340.0),
        ] {
            let table = solar_model_field_table(var, "W m-2")
                .unwrap_or_else(|| panic!("{var} must resolve a radiation ramp"));
            assert_eq!(table.sample(lo), generic.sample(0.0), "{var} low end");
            assert_eq!(table.sample(hi), generic.sample(100.0), "{var} high end");
            assert_ne!(
                table.sample(lo),
                table.sample(hi),
                "{var}: ramp must actually vary"
            );
        }
    }

    /// Signed surface-energy fluxes take the EXISTING Balance diverging ramp
    /// rescaled symmetrically: zero flux sits exactly on the neutral center,
    /// the extremes on the base ramp's extremes.
    #[test]
    fn raw_wrf_flux_fields_resolve_the_scaled_diverging_ramp() {
        let balance = crate::balance_velocity_table();
        for (var, units, max_abs) in [
            ("wrf_hfx", "W m-2", 700.0_f32),
            ("wrf_lh", "W m-2", 700.0),
            ("wrf_grdflx", "W m-2", 300.0),
            ("wrf_qfx", "kg m-2 s-1", 4.0e-4),
            ("wrf_u10", "m s-1", 30.0),
            ("wrf_v10", "m s-1", 30.0),
        ] {
            let table = solar_model_field_table(var, units)
                .unwrap_or_else(|| panic!("{var} must resolve a diverging ramp"));
            assert_eq!(
                table.sample(0.0),
                balance.sample(0.0),
                "{var}: zero must sit on the neutral center"
            );
            assert_eq!(
                table.sample(max_abs),
                balance.sample(70.0),
                "{var} positive end"
            );
            assert_eq!(
                table.sample(-max_abs),
                balance.sample(-70.0),
                "{var} negative end"
            );
        }
    }

    /// Raw surface-met / precip / hail catalog entries land on the same
    /// existing tables their canonical siblings use.
    #[test]
    fn raw_wrf_met_fields_resolve_their_solar_family_tables() {
        // T2 (K) shades exactly like canonical temperature_2m.
        let t2 = solar_model_field_table("wrf_t2", "K").expect("t2");
        let canonical = solar_model_field_table("temperature_2m", "K").expect("canonical");
        for value in [233.15_f32, 273.15, 300.0] {
            assert_eq!(t2.sample(value), canonical.sample(value), "{value} K");
        }
        assert!(solar_model_field_table("wrf_tsk", "K").is_some());
        assert!(solar_model_field_table("wrf_tmn", "K").is_some());

        // TD2 takes the dew point table (not the temperature branch).
        let td2 = solar_model_field_table("wrf_td2", "K").expect("td2");
        assert_eq!(
            td2.sample(280.0),
            solar_dewpoint_table(TempUnit::Kelvin).sample(280.0)
        );

        // Frozen accumulations (mm) share the Solar precip ramp even though
        // no substring heuristic can classify them.
        let precip = solar_precip_table(DepthUnit::Millimetres);
        for var in ["wrf_snownc", "wrf_graupelnc", "wrf_hailnc", "wrf_snow"] {
            let table = solar_model_field_table(var, "mm")
                .unwrap_or_else(|| panic!("{var} must resolve the precip ramp"));
            assert_eq!(table.sample(20.0), precip.sample(20.0), "{var}");
        }

        // 10 m speed diagnostics use the Solar wind ramp, m/s-aware.
        let wspd = solar_model_field_table("wrf_wspd10", "m s-1").expect("wspd10");
        assert_eq!(
            wspd.sample(20.0),
            solar_wind_speed_table(SpeedUnit::MetersPerSecond).sample(20.0)
        );

        // NSSL hail maxima take the existing Analyst MEHS ladder.
        let hail = solar_model_field_table("wrf_hail_maxk1", "mm").expect("hail");
        assert_eq!(
            hail.sample(30.0),
            crate::builtin_hail_size_table().sample(30.0)
        );

        // Reflectivity / CAPE / helicity diagnostics keep their Solar ramps.
        assert!(solar_model_field_table("wrf_refd_max", "dBZ").is_some());
        assert!(solar_model_field_table("wrf_sb3cape", "J kg-1").is_some());
        assert!(solar_model_field_table("wrf_effective_srh", "m2 s-2").is_some());
    }

    /// Percent-valued fields use the probability ramp directly; fraction-
    /// valued ones use it rescaled to 0..1, so 0.75 shades exactly like 75 %.
    #[test]
    fn raw_wrf_fraction_and_percent_fields_use_the_probability_ramp() {
        let percent = crate::builtin_probability_table();
        let cloud = solar_model_field_table("wrf_cloudfrac_low", "%").expect("cloudfrac");
        assert_eq!(cloud.sample(75.0), percent.sample(75.0));
        let veg = solar_model_field_table("wrf_vegfra", "").expect("vegfra");
        assert_eq!(veg.sample(75.0), percent.sample(75.0));
        let albedo = solar_model_field_table("wrf_albedo", "").expect("albedo");
        assert_eq!(albedo.sample(0.75), percent.sample(75.0));
        let snowc = solar_model_field_table("wrf_snowc", "").expect("snowc");
        assert_eq!(snowc.sample(1.0), percent.sample(100.0));
    }

    /// Catalog entries with no assigned family — and genuinely unknown raw
    /// names — keep today's behavior: no table, so the caller's generic
    /// normalized fallback still applies.
    #[test]
    fn unassigned_and_unknown_wrf_names_keep_the_generic_fallback() {
        for (var, units) in [
            ("wrf_psfc", "Pa"),
            ("wrf_lu_index", ""),
            ("wrf_xland", ""),
            ("wrf_lwdnt", "W m-2"),
            ("wrf_acswupt", "J m-2"),
            ("wrf_wdir10", "degrees"),
            ("wrf_cin", "J kg-1"),
            ("wrf_mystery_field", "widgets"),
            ("wrf_some_experimental_field", ""),
        ] {
            assert!(
                solar_model_field_table(var, units).is_none(),
                "{var} must keep the generic fallback"
            );
        }
    }

    #[test]
    fn every_solar_table_builds() {
        // Guards the anchor arrays (>= 2 stops, finite values) at test time.
        let _ = solar_reflectivity_table();
        let _ = solar_temperature_table(TempUnit::Kelvin);
        let _ = solar_temperature_level_table(250, TempUnit::Kelvin).expect("250");
        let _ = solar_temperature_level_table(500, TempUnit::Celsius).expect("500");
        let _ = solar_temperature_level_table(700, TempUnit::Fahrenheit).expect("700");
        let _ = solar_temperature_level_table(850, TempUnit::Kelvin).expect("850");
        let _ = solar_temperature_sfc_c_table(TempUnit::Celsius);
        let _ = solar_dewpoint_table(TempUnit::Celsius);
        let _ = solar_wind_speed_table(SpeedUnit::MetersPerSecond);
        let _ = solar_precip_table(DepthUnit::Millimetres);
        let _ = solar_relative_humidity_table();
        let _ = solar_cape_table();
        let _ = solar_composite_severe_table("Solar Helicity", 600.0);
        let _ = solar_vorticity_table();
    }
}
