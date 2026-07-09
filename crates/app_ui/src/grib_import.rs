//! Local GRIB Edition 1 import — the GDEX "past" datasets (ERA-20C et al.).
//!
//! One GRIB1 file carries ONE parameter across MANY timesteps (the owner's
//! ERA-20C surface stream is 2,928 three-hourly analyses spanning a year in a
//! single 450 MB file), so this importer inverts `local_import`'s
//! one-file-one-hour shape: it indexes every message's byte range and header
//! once, then decodes lazily — one 320x160 plane at a time — writing each
//! timestep as its own forecast-hour slot via
//! `rw_store::write_hour_from_fields_with_derived` and dropping the plane
//! before the next. Peak decoded state is one timestep's fields, never the
//! whole year.
//!
//! Decoding is grib-core's (the pinned rusty-weather vendor crate): PDS/GDS
//! parse, IBM-float reference values, 24-bit simple packing, and true
//! Gaussian latitudes (Legendre roots) all come from
//! `grib_core::grib1::Grib1File`. What lives HERE is the app seam grib-core
//! does not provide:
//! - a streaming message INDEX (grib-core's `from_bytes` eagerly clones every
//!   section of every message — 2x file size in RAM for a 450 MB file);
//! - ECMWF parameter table 128 names/units (grib-core ships WMO table 2 only
//!   and ignores `table_version`);
//! - the store-write plan: canonical `FieldSelector`s for the params whose
//!   units match what the WRF import precedent stores, derived slugs for the
//!   rest, and hour keys derived from each message's valid time;
//! - global-grid longitude normalization (columns rotated so longitudes run
//!   -180..180 monotonic — the map layer's inverse LUT does not wrap, so a
//!   raw 0..360 grid would blank the western hemisphere).
//!
//! Hour keys are HOURS SINCE THE FIRST TIMESTEP (0, 3, 6, ... 8781 for a
//! 3-hourly year), computed from decoded reference times rather than assumed
//! spacing, so gappy or differently-stepped files stay correct. The run name
//! ("era20c_fsr_2004010100") is deliberately NOT `YYYYMMDD_HHz`-shaped:
//! `model_data::model_run_time_utc` would otherwise pull a year-long 2004
//! reanalysis into the wall-clock timeline. Reanalysis runs are reached
//! through the run browser tree. The store model slug is `wrf` — the same
//! slug both existing import paths stamp (including GDEX climate wrfouts) —
//! because the Solar-fallback styling, label translation, and native-plot
//! paths all key on it; a new slug would render every field styleless.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use grib_core::grib1::{Grib1File, Grib1Message, GridType};
use rustwx_core::{CanonicalField, FieldSelector, GridShape, LatLonGrid, SelectedField2D};
use rw_store::{DerivedFieldInput, write_hour_from_fields_with_derived};

use crate::local_import::LocalImportSummary;

/// Standard gravity (m/s^2) — converts ECMWF geopotential (m^2/s^2) to the
/// geopotential height (gpm) every other height field in the store speaks.
const STANDARD_GRAVITY: f64 = 9.80665;

/// Extension gate: GRIB1 containers only. `.grb2`/`.grib2` deliberately stay
/// unsupported here — MRMS/HRRR GRIB2 arrive through their own feeds, and a
/// GRIB2 message inside a `.grb` file gets a clear edition error at index
/// time instead of silently decoding garbage.
pub fn is_grib1_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("grb" | "grib")
    )
}

// ---------------------------------------------------------------------------
// Message index
// ---------------------------------------------------------------------------

/// One GRIB1 message's byte range plus the PDS/GDS header facts the import
/// plan needs. Built by [`index_grib1_file`] from ~120 header bytes per
/// message — values stay packed on disk until the write loop asks.
#[derive(Debug, Clone)]
pub(crate) struct IndexedMessage {
    pub offset: u64,
    pub total_len: u32,
    pub table_version: u8,
    pub center: u8,
    pub parameter: u8,
    pub level_type: u8,
    pub level_value: u16,
    /// Valid time (reference time + forecast offset), unix seconds.
    pub valid_unix: i64,
    pub ni: u16,
    pub nj: u16,
}

fn read_exact_at(file: &mut File, offset: u64, buf: &mut [u8]) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|err| format!("seek to {offset}: {err}"))?;
    file.read_exact(buf)
        .map_err(|err| format!("read {} bytes at {offset}: {err}", buf.len()))
}

fn read_u24(bytes: &[u8]) -> u32 {
    ((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | (bytes[2] as u32)
}

/// Scan forward from `pos` for the next "GRIB" magic, tolerating padding or
/// index blocks between messages. Chunked with a 3-byte overlap so magic
/// spanning a chunk boundary still matches.
fn scan_forward_for_magic(file: &mut File, pos: u64, file_len: u64) -> Result<Option<u64>, String> {
    const CHUNK: usize = 64 * 1024;
    let mut start = pos;
    let mut buf = vec![0u8; CHUNK];
    while start + 4 <= file_len {
        let want = usize::try_from((file_len - start).min(CHUNK as u64)).unwrap_or(CHUNK);
        let chunk = &mut buf[..want];
        read_exact_at(file, start, chunk)?;
        if let Some(found) = chunk.windows(4).position(|window| window == b"GRIB") {
            return Ok(Some(start + found as u64));
        }
        if start + want as u64 >= file_len {
            break;
        }
        // Re-read the last 3 bytes with the next chunk.
        start += (want - 3) as u64;
    }
    Ok(None)
}

/// Forecast offset in seconds from PDS time unit / P1 / P2 / time range
/// indicator. `None` for calendar units (months, years, ...) whose length is
/// not fixed — the caller skips such messages with a note rather than
/// guessing. Analyses (the ERA-20C case) are `tri 0, P1 0` and return 0.
fn forecast_offset_seconds(time_unit: u8, p1: u8, p2: u8, tri: u8) -> Option<i64> {
    let unit_seconds: i64 = match time_unit {
        0 => 60,
        1 => 3_600,
        2 => 86_400,
        10 => 3 * 3_600,
        11 => 6 * 3_600,
        12 => 12 * 3_600,
        13 => 900,
        14 => 1_800,
        254 => 1,
        // 3..=7: months / years / decades / normals / centuries.
        _ => return None,
    };
    let periods: i64 = match tri {
        // Two-octet P1 (used when a forecast period exceeds 255 units).
        10 => ((p1 as i64) << 8) | (p2 as i64),
        // Period products (averages / accumulations / differences) are valid
        // at the END of the (P1, P2) window.
        2..=5 => p2 as i64,
        _ => p1 as i64,
    };
    Some(periods * unit_seconds)
}

/// Index every GRIB1 message in `path`: byte ranges from each message's own
/// length word (NO uniform-record-size assumption), plus the PDS/GDS facts
/// the field plan needs. Each message's trailing `7777` is verified so a
/// truncated download fails here, loudly, instead of mid-import.
pub(crate) fn index_grib1_file(path: &Path) -> Result<Vec<IndexedMessage>, String> {
    let name = display_name(path);
    let mut file = File::open(path).map_err(|err| format!("{name}: open: {err}"))?;
    let file_len = file
        .metadata()
        .map_err(|err| format!("{name}: metadata: {err}"))?
        .len();

    let mut out = Vec::new();
    let mut pos = 0u64;
    let mut header = [0u8; 8];
    while pos + 8 <= file_len {
        read_exact_at(&mut file, pos, &mut header).map_err(|err| format!("{name}: {err}"))?;
        if &header[0..4] != b"GRIB" {
            match scan_forward_for_magic(&mut file, pos, file_len)
                .map_err(|err| format!("{name}: {err}"))?
            {
                Some(next) => {
                    pos = next;
                    continue;
                }
                None => break,
            }
        }
        let total_len = read_u24(&header[4..7]);
        let edition = header[7];
        if edition != 1 {
            return Err(format!(
                "{name}: GRIB edition {edition} message at byte {pos} — this importer handles \
                 GRIB1 only (GRIB2 products arrive through their own feeds)"
            ));
        }
        if total_len < 40 || pos + total_len as u64 > file_len {
            return Err(format!(
                "{name}: message at byte {pos} claims {total_len} bytes but the file holds \
                 {file_len} — truncated download?"
            ));
        }

        // PDS: everything the plan needs sits in the first 28 bytes.
        let mut pds = [0u8; 28];
        read_exact_at(&mut file, pos + 8, &mut pds).map_err(|err| format!("{name}: {err}"))?;
        let pds_len = read_u24(&pds[0..3]);
        if pds_len < 28 {
            return Err(format!(
                "{name}: message at byte {pos}: PDS of {pds_len} bytes (< 28) not supported"
            ));
        }
        let gds_present = pds[7] & 0x80 != 0;
        if !gds_present {
            return Err(format!(
                "{name}: message at byte {pos} has no Grid Description Section — cannot place \
                 its values on a grid"
            ));
        }
        let table_version = pds[3];
        let center = pds[4];
        let parameter = pds[8];
        let level_type = pds[9];
        let level_value = ((pds[10] as u16) << 8) | pds[11] as u16;
        let year_of_century = pds[12] as i32;
        let century = pds[24] as i32;
        let year = if century == 0 {
            1900 + year_of_century
        } else {
            (century - 1) * 100 + year_of_century
        };
        let reference = chrono::NaiveDate::from_ymd_opt(year, pds[13] as u32, pds[14] as u32)
            .and_then(|date| date.and_hms_opt(pds[15] as u32, pds[16] as u32, 0))
            .ok_or_else(|| {
                format!(
                    "{name}: message at byte {pos}: bad reference time {year}-{}-{} {}:{}",
                    pds[13], pds[14], pds[15], pds[16]
                )
            })?;
        let offset_seconds = forecast_offset_seconds(pds[17], pds[18], pds[19], pds[20])
            .ok_or_else(|| {
                format!(
                    "{name}: message at byte {pos}: calendar forecast time unit {} (months/\
                     years) not supported",
                    pds[17]
                )
            })?;
        let valid_unix = reference.and_utc().timestamp() + offset_seconds;

        // GDS: only Ni/Nj here (grid geometry decodes through grib-core at
        // write time). Fixed offsets 6..10 hold Ni/Nj for the rectilinear
        // grid types this importer accepts.
        let mut gds = [0u8; 10];
        read_exact_at(&mut file, pos + 8 + pds_len as u64, &mut gds)
            .map_err(|err| format!("{name}: {err}"))?;
        let ni = ((gds[6] as u16) << 8) | gds[7] as u16;
        let nj = ((gds[8] as u16) << 8) | gds[9] as u16;
        if ni == 0xFFFF {
            return Err(format!(
                "{name}: message at byte {pos} uses a quasi-regular (reduced) grid — download \
                 the regular-grid product (e.g. regn80sc) instead"
            ));
        }

        // End section: `7777` exactly where the length word says.
        let mut end = [0u8; 4];
        read_exact_at(&mut file, pos + total_len as u64 - 4, &mut end)
            .map_err(|err| format!("{name}: {err}"))?;
        if &end != b"7777" {
            return Err(format!(
                "{name}: message at byte {pos} does not end in '7777' — corrupt or truncated"
            ));
        }

        out.push(IndexedMessage {
            offset: pos,
            total_len,
            table_version,
            center,
            parameter,
            level_type,
            level_value,
            valid_unix,
            ni,
            nj,
        });
        pos += total_len as u64;
    }

    if out.is_empty() {
        return Err(format!("{name}: no GRIB1 messages found"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ECMWF parameter table 128
// ---------------------------------------------------------------------------

/// One ECMWF table-128 parameter: GDEX short name, store slug, human label,
/// units as stored (ERA native units — no conversion beyond geopotential).
pub(crate) struct EraParam {
    pub short: &'static str,
    pub slug: &'static str,
    pub label: &'static str,
    pub units: &'static str,
}

/// The ECMWF table-128 parameters ERA-20C (and the other GDEX "past"
/// reanalysis streams) actually publish. grib-core's tables.rs is WMO table 2
/// only and ignores `table_version`, so this map is the app seam. Slugs are
/// deliberately readable (they surface verbatim in the field picker) and
/// chosen so `color_tables::solar_model_field_table`'s substring heuristics
/// land where a palette exists (temperature/dewpoint/cape/vort/precip/...).
pub(crate) fn era128_param(parameter: u8) -> Option<EraParam> {
    let entry = |short, slug, label, units| {
        Some(EraParam {
            short,
            slug,
            label,
            units,
        })
    };
    match parameter {
        31 => entry("ci", "sea_ice_cover", "Sea-ice cover", "(0-1)"),
        34 => entry("sst", "sea_surface_temperature", "Sea surface temperature", "K"),
        59 => entry("cape", "cape", "Convective available potential energy", "J/kg"),
        129 => entry("z", "geopotential", "Geopotential", "m2/s2"),
        130 => entry("t", "temperature", "Temperature", "K"),
        131 => entry("u", "u_wind", "U component of wind", "m/s"),
        132 => entry("v", "v_wind", "V component of wind", "m/s"),
        133 => entry("q", "specific_humidity", "Specific humidity", "kg/kg"),
        134 => entry("sp", "surface_pressure", "Surface pressure", "Pa"),
        135 => entry("w", "omega", "Vertical velocity (pressure)", "Pa/s"),
        136 => entry("tcw", "total_column_water", "Total column water", "kg/m2"),
        137 => entry(
            "tcwv",
            "total_column_water_vapour",
            "Total column water vapour",
            "kg/m2",
        ),
        138 => entry("vo", "relative_vorticity", "Relative vorticity", "1/s"),
        141 => entry("sd", "snow_depth", "Snow depth (water equivalent)", "m"),
        142 => entry(
            "lsp",
            "large_scale_precipitation",
            "Large-scale precipitation",
            "m",
        ),
        143 => entry("cp", "convective_precipitation", "Convective precipitation", "m"),
        144 => entry("sf", "snowfall", "Snowfall (water equivalent)", "m"),
        151 => entry("msl", "mslp", "Mean sea level pressure", "Pa"),
        155 => entry("d", "divergence", "Divergence", "1/s"),
        156 => entry("gh", "height", "Geopotential height", "gpm"),
        157 => entry("r", "relative_humidity", "Relative humidity", "%"),
        159 => entry("blh", "boundary_layer_height", "Boundary layer height", "m"),
        164 => entry("tcc", "total_cloud_cover", "Total cloud cover", "(0-1)"),
        165 => entry("10u", "u_10m", "10 m U wind component", "m/s"),
        166 => entry("10v", "v_10m", "10 m V wind component", "m/s"),
        167 => entry("2t", "temperature_2m", "2 m temperature", "K"),
        168 => entry("2d", "dewpoint_2m", "2 m dewpoint temperature", "K"),
        172 => entry("lsm", "land_sea_mask", "Land-sea mask", "(0-1)"),
        173 => entry("sr", "surface_roughness", "Surface roughness", "m"),
        182 => entry("e", "evaporation", "Evaporation (water equivalent)", "m"),
        186 => entry("lcc", "low_cloud_cover", "Low cloud cover", "(0-1)"),
        187 => entry("mcc", "medium_cloud_cover", "Medium cloud cover", "(0-1)"),
        188 => entry("hcc", "high_cloud_cover", "High cloud cover", "(0-1)"),
        201 => entry("mx2t", "temperature_2m_max", "Maximum 2 m temperature", "K"),
        202 => entry("mn2t", "temperature_2m_min", "Minimum 2 m temperature", "K"),
        205 => entry("ro", "runoff", "Runoff", "m"),
        228 => entry("tp", "total_precipitation", "Total precipitation", "m"),
        235 => entry("skt", "skin_temperature", "Skin temperature", "K"),
        238 => entry("tsn", "snow_temperature", "Temperature of snow layer", "K"),
        243 => entry("fal", "forecast_albedo", "Forecast albedo", "(0-1)"),
        244 => entry(
            "fsr",
            "forecast_surface_roughness",
            "Forecast surface roughness",
            "m",
        ),
        245 => entry(
            "flsr",
            "log_surface_roughness_heat",
            "Forecast log of surface roughness for heat",
            "~",
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Field plan
// ---------------------------------------------------------------------------

/// How one message lands in the store: a canonical 2D field (real
/// `FieldSelector`, so production styles resolve for the names the HRRR
/// recipe set knows) or a derived slug (the honest `{"derived": slug}`
/// marker for everything without a units-safe canonical mapping).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlannedField {
    Canonical {
        name: String,
        selector: FieldSelector,
        units: String,
        /// Multiplier applied to decoded values (1.0 = passthrough;
        /// 1/9.80665 turns geopotential into height).
        scale: f64,
    },
    Derived {
        name: String,
        units: String,
        scale: f64,
    },
}

impl PlannedField {
    pub(crate) fn name(&self) -> &str {
        match self {
            PlannedField::Canonical { name, .. } | PlannedField::Derived { name, .. } => name,
        }
    }

    fn scale(&self) -> f64 {
        match self {
            PlannedField::Canonical { scale, .. } | PlannedField::Derived { scale, .. } => *scale,
        }
    }
}

/// Level suffix for store names, following the iso naming contract
/// (`temperature_850` — `color_tables::iso_levels`): isobaric levels append
/// the bare hPa value, heights append metres, everything else stays explicit
/// about its GRIB level type rather than pretending to be a surface field.
fn level_suffix(level_type: u8, level_value: u16) -> String {
    match level_type {
        1 => String::new(),
        100 => format!("_{level_value}"),
        105 if level_value == 0 => String::new(),
        105 => format!("_{level_value}m"),
        109 => format!("_hyb{level_value}"),
        111 | 112 => format!("_soil{level_value}"),
        _ if level_value == 0 => format!("_lt{level_type}"),
        _ => format!("_lt{level_type}_{level_value}"),
    }
}

/// Map one indexed message to its store field. Canonical selectors are
/// assigned ONLY where the ERA native units match what the WRF import
/// precedent stores for that selector (K, m/s, Pa, gpm, %) — a canonical
/// selector with off-units would let a production style apply its unit
/// arithmetic to the wrong quantity. Geopotential (129) is divided by g and
/// stored as height in gpm, matching every other height field.
pub(crate) fn plan_field(msg: &IndexedMessage) -> PlannedField {
    let level = msg.level_value;
    if msg.table_version == 128 {
        // Surface-ish level types ECMWF uses for its named screen-level
        // params (1 = surface, 105 = fixed height above ground).
        let sfc = matches!(msg.level_type, 1 | 105);
        match (msg.parameter, msg.level_type) {
            (129, 100) => {
                return PlannedField::Canonical {
                    name: format!("height_{level}"),
                    selector: FieldSelector::isobaric(CanonicalField::GeopotentialHeight, level),
                    units: "gpm".to_string(),
                    scale: 1.0 / STANDARD_GRAVITY,
                };
            }
            (129, _) if sfc => {
                return PlannedField::Canonical {
                    name: "orography".to_string(),
                    selector: FieldSelector::surface(CanonicalField::GeopotentialHeight),
                    units: "gpm".to_string(),
                    scale: 1.0 / STANDARD_GRAVITY,
                };
            }
            (156, 100) => {
                return PlannedField::Canonical {
                    name: format!("height_{level}"),
                    selector: FieldSelector::isobaric(CanonicalField::GeopotentialHeight, level),
                    units: "gpm".to_string(),
                    scale: 1.0,
                };
            }
            (130, 100) => {
                return PlannedField::Canonical {
                    name: format!("temperature_{level}"),
                    selector: FieldSelector::isobaric(CanonicalField::Temperature, level),
                    units: "K".to_string(),
                    scale: 1.0,
                };
            }
            (131, 100) => {
                return PlannedField::Canonical {
                    name: format!("u_{level}"),
                    selector: FieldSelector::isobaric(CanonicalField::UWind, level),
                    units: "m/s".to_string(),
                    scale: 1.0,
                };
            }
            (132, 100) => {
                return PlannedField::Canonical {
                    name: format!("v_{level}"),
                    selector: FieldSelector::isobaric(CanonicalField::VWind, level),
                    units: "m/s".to_string(),
                    scale: 1.0,
                };
            }
            (157, 100) => {
                return PlannedField::Canonical {
                    name: format!("relative_humidity_{level}"),
                    selector: FieldSelector::isobaric(CanonicalField::RelativeHumidity, level),
                    units: "%".to_string(),
                    scale: 1.0,
                };
            }
            (134, _) if sfc => {
                return PlannedField::Canonical {
                    name: "surface_pressure".to_string(),
                    selector: FieldSelector::surface(CanonicalField::Pressure),
                    units: "Pa".to_string(),
                    scale: 1.0,
                };
            }
            (151, _) => {
                return PlannedField::Canonical {
                    name: "mslp".to_string(),
                    selector: FieldSelector::mean_sea_level(
                        CanonicalField::PressureReducedToMeanSeaLevel,
                    ),
                    units: "Pa".to_string(),
                    scale: 1.0,
                };
            }
            (165, _) if sfc => {
                return PlannedField::Canonical {
                    name: "u_10m".to_string(),
                    selector: FieldSelector::height_agl(CanonicalField::UWind, 10),
                    units: "m/s".to_string(),
                    scale: 1.0,
                };
            }
            (166, _) if sfc => {
                return PlannedField::Canonical {
                    name: "v_10m".to_string(),
                    selector: FieldSelector::height_agl(CanonicalField::VWind, 10),
                    units: "m/s".to_string(),
                    scale: 1.0,
                };
            }
            (167, _) if sfc => {
                return PlannedField::Canonical {
                    name: "temperature_2m".to_string(),
                    selector: FieldSelector::height_agl(CanonicalField::Temperature, 2),
                    units: "K".to_string(),
                    scale: 1.0,
                };
            }
            (168, _) if sfc => {
                return PlannedField::Canonical {
                    name: "dewpoint_2m".to_string(),
                    selector: FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
                    units: "K".to_string(),
                    scale: 1.0,
                };
            }
            _ => {}
        }
        if let Some(param) = era128_param(msg.parameter) {
            return PlannedField::Derived {
                name: format!(
                    "{}{}",
                    param.slug,
                    level_suffix(msg.level_type, msg.level_value)
                ),
                units: param.units.to_string(),
                scale: 1.0,
            };
        }
    } else if let Some(abbrev) = grib_core::grib1::parameter_abbrev(msg.parameter) {
        // Non-ECMWF tables: WMO table 2 via grib-core.
        let slug: String = abbrev
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();
        return PlannedField::Derived {
            name: format!("{slug}{}", level_suffix(msg.level_type, msg.level_value)),
            units: grib_core::grib1::parameter_units(msg.parameter)
                .unwrap_or("")
                .to_string(),
            scale: 1.0,
        };
    }
    PlannedField::Derived {
        name: format!(
            "p{}_t{}{}",
            msg.parameter,
            msg.table_version,
            level_suffix(msg.level_type, msg.level_value)
        ),
        units: String::new(),
        scale: 1.0,
    }
}

// ---------------------------------------------------------------------------
// Grid plan
// ---------------------------------------------------------------------------

/// The run grid plus the column rotation every decoded plane must apply.
/// Global rectilinear grids (span >= 350 degrees) rotate so longitudes run
/// -180..180 monotonic; regional grids pass through untouched.
pub(crate) struct GridPlan {
    pub nx: usize,
    pub ny: usize,
    /// Source column index that becomes output column 0.
    pub rotate: usize,
    pub grid: LatLonGrid,
}

fn normalize_lon_180(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

/// Build the grid plan from a fully parsed first message. Rectilinear grids
/// only (lat/lon and Gaussian); grib-core computes true Gaussian latitudes
/// (Legendre roots), so no linear approximation is involved.
pub(crate) fn build_grid_plan(msg: &Grib1Message) -> Result<GridPlan, String> {
    let gds = msg
        .gds
        .as_ref()
        .ok_or_else(|| "message has no Grid Description Section".to_string())?;
    let (ni, nj, scanning_mode) = match &gds.grid_type {
        GridType::LatLon {
            ni,
            nj,
            scanning_mode,
            ..
        }
        | GridType::Gaussian {
            ni,
            nj,
            scanning_mode,
            ..
        } => (*ni as usize, *nj as usize, *scanning_mode),
        other => {
            return Err(format!(
                "unsupported GRIB1 grid type {other:?} — this importer handles regular \
                 lat/lon and Gaussian grids"
            ));
        }
    };
    if scanning_mode & 0x20 != 0 {
        return Err(
            "j-consecutive (column-major) scanning not supported — ERA-20C regular grids \
             are row-major"
                .to_string(),
        );
    }
    let coords = msg
        .latlons()
        .map_err(|err| format!("grid coordinates: {err}"))?;
    if coords.len() != ni * nj {
        return Err(format!(
            "grid coordinate count {} does not match Ni x Nj = {}",
            coords.len(),
            ni * nj
        ));
    }

    // Rectilinear: longitudes from the first row, latitudes from the first
    // column (grib-core emits coordinates in data order).
    let row_lons: Vec<f64> = coords[..ni].iter().map(|c| c.lon).collect();
    let col_lats: Vec<f64> = (0..nj).map(|j| coords[j * ni].lat).collect();

    let (min_lon, max_lon) = row_lons
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &lon| {
            (lo.min(lon), hi.max(lon))
        });
    let rotate = if max_lon - min_lon >= 350.0 {
        // Global grid: rotate so output longitudes ascend from -180. The
        // rotation point is the first column at or past the antimeridian.
        row_lons
            .iter()
            .position(|&lon| lon >= 180.0)
            .unwrap_or(0)
    } else {
        0
    };

    let mut lat_deg = Vec::with_capacity(ni * nj);
    let mut lon_deg = Vec::with_capacity(ni * nj);
    let out_lons: Vec<f64> = (0..ni)
        .map(|i| normalize_lon_180(row_lons[(rotate + i) % ni]))
        .collect();
    for &lat in &col_lats {
        for &lon in &out_lons {
            lat_deg.push(lat as f32);
            lon_deg.push(lon as f32);
        }
    }

    let shape = GridShape::new(ni, nj).map_err(|err| format!("grid shape: {err}"))?;
    let grid = LatLonGrid::new(shape, lat_deg, lon_deg).map_err(|err| format!("grid: {err}"))?;
    Ok(GridPlan {
        nx: ni,
        ny: nj,
        rotate,
        grid,
    })
}

/// Apply the plan's column rotation and unit scale to one decoded plane.
fn rotate_and_scale(values: &[f64], plan: &GridPlan, scale: f64) -> Vec<f32> {
    let (nx, ny, rotate) = (plan.nx, plan.ny, plan.rotate);
    let mut out = Vec::with_capacity(values.len());
    for j in 0..ny {
        let row = &values[j * nx..(j + 1) * nx];
        for i in 0..nx {
            out.push((row[(rotate + i) % nx] * scale) as f32);
        }
    }
    out
}

/// Read one message's byte range and decode it through grib-core.
fn parse_message_at(
    file: &mut File,
    msg: &IndexedMessage,
    file_label: &str,
) -> Result<Grib1Message, String> {
    let mut bytes = vec![0u8; msg.total_len as usize];
    read_exact_at(file, msg.offset, &mut bytes).map_err(|err| format!("{file_label}: {err}"))?;
    let parsed = Grib1File::from_bytes(&bytes)
        .map_err(|err| format!("{file_label}: message at byte {}: {err}", msg.offset))?;
    parsed
        .messages
        .into_iter()
        .next()
        .ok_or_else(|| format!("{file_label}: message at byte {}: empty parse", msg.offset))
}

/// Decode one message's values through grib-core (24-bit simple packing, IBM
/// reference value, binary/decimal scaling) and shape them for the store.
fn decode_values(
    file: &mut File,
    msg: &IndexedMessage,
    plan: &GridPlan,
    scale: f64,
    file_label: &str,
) -> Result<Vec<f32>, String> {
    let parsed = parse_message_at(file, msg, file_label)?;
    let values = parsed
        .values()
        .map_err(|err| format!("{file_label}: message at byte {}: {err}", msg.offset))?;
    if values.len() != plan.nx * plan.ny {
        return Err(format!(
            "{file_label}: message at byte {}: {} values for a {}x{} grid",
            msg.offset,
            values.len(),
            plan.nx,
            plan.ny
        ));
    }
    Ok(rotate_and_scale(&values, plan, scale))
}

// ---------------------------------------------------------------------------
// Import driver
// ---------------------------------------------------------------------------

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("grib file")
        .to_string()
}

fn sanitize_run_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// Run name: `{dataset}_{short}_{startYYYYMMDDHH}` — readable in the run
/// browser and deliberately NOT `YYYYMMDD_HHz`-shaped (see module docs).
fn run_name(paths: &[PathBuf], first: &IndexedMessage, first_valid_unix: i64) -> String {
    let dataset = if first.center == 98 && first.table_version == 128 {
        "era20c"
    } else {
        "grib1"
    };
    let short = if paths.len() > 1 {
        format!("{}files", paths.len())
    } else {
        era_short_from_filename(&paths[0]).unwrap_or_else(|| {
            era128_param(first.parameter)
                .map(|param| param.short.to_string())
                .unwrap_or_else(|| format!("p{}", first.parameter))
        })
    };
    let stamp = chrono::DateTime::from_timestamp(first_valid_unix, 0)
        .map(|time| time.format("%Y%m%d%H").to_string())
        .unwrap_or_else(|| "unknown".to_string());
    sanitize_run_component(&format!("{dataset}_{short}_{stamp}"))
}

/// The GDEX filename grammar
/// (`e20c.oper.an.sfc.3hr.{table}_{param}_{short}.regn80sc.{start}_{end}.grb`)
/// carries the dataset short name — use it when it parses, fall back to the
/// parameter table otherwise.
fn era_short_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('.').collect();
    let ids = parts.iter().find(|part| {
        let segments: Vec<&str> = part.split('_').collect();
        segments.len() == 3
            && segments[0].chars().all(|ch| ch.is_ascii_digit())
            && segments[1].chars().all(|ch| ch.is_ascii_digit())
            && !segments[2].is_empty()
    })?;
    Some(ids.split('_').nth(2)?.to_string())
}

fn writer_build() -> &'static str {
    concat!("bowecho-grib1-local-import-", env!("CARGO_PKG_VERSION"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn format_utc(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|time| time.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_else(|| format!("unix {unix}"))
}

/// Import one or more GRIB1 files as a single store run. Every distinct
/// valid time becomes one forecast-hour slot (hours since the first
/// timestep); multiple files merge by valid time, so a matching set of
/// single-parameter ERA-20C downloads lands as one multi-variable run.
///
/// Runs on `local_import::spawn_import_paths`' worker thread, which has
/// already dropped itself to below-normal priority.
pub(crate) fn import_grib1_files(
    paths: &[PathBuf],
    store_root: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<LocalImportSummary, String> {
    if paths.is_empty() {
        return Err("no GRIB1 files selected".to_string());
    }

    // ---- Index every file (headers only — values stay on disk). ----
    let mut indexed: Vec<(usize, IndexedMessage)> = Vec::new();
    let mut per_file: Vec<usize> = Vec::new();
    for (file_idx, path) in paths.iter().enumerate() {
        progress(format!(
            "GRIB1 {}: indexing messages ({}/{})",
            display_name(path),
            file_idx + 1,
            paths.len()
        ));
        let messages = index_grib1_file(path)?;
        per_file.push(messages.len());
        indexed.extend(messages.into_iter().map(|msg| (file_idx, msg)));
    }

    // ---- Reference grid from the first message of the first file. ----
    let first_label = display_name(&paths[0]);
    let mut first_file = File::open(&paths[0])
        .map_err(|err| format!("{first_label}: open: {err}"))?;
    let first_msg = indexed[0].1.clone();
    let plan = build_grid_plan(&parse_message_at(&mut first_file, &first_msg, &first_label)?)
        .map_err(|err| format!("{first_label}: {err}"))?;
    drop(first_file);

    let mut notes: Vec<String> = Vec::new();
    let (ref_ni, ref_nj) = (first_msg.ni, first_msg.nj);
    let before = indexed.len();
    indexed.retain(|(_, msg)| msg.ni == ref_ni && msg.nj == ref_nj);
    if indexed.len() != before {
        notes.push(format!(
            "{} message(s) on a different grid than the first ({ref_ni}x{ref_nj}) skipped",
            before - indexed.len()
        ));
    }

    // ---- Hour keys: hours since the first timestep. ----
    let first_valid = indexed
        .iter()
        .map(|(_, msg)| msg.valid_unix)
        .min()
        .ok_or_else(|| "no importable messages".to_string())?;
    let mut hours: BTreeMap<u16, Vec<(usize, IndexedMessage)>> = BTreeMap::new();
    let mut skipped_subhour = 0usize;
    let mut skipped_range = 0usize;
    for (file_idx, msg) in indexed {
        let offset_seconds = msg.valid_unix - first_valid;
        if offset_seconds % 3_600 != 0 {
            skipped_subhour += 1;
            continue;
        }
        match u16::try_from(offset_seconds / 3_600) {
            Ok(hour) => hours.entry(hour).or_default().push((file_idx, msg)),
            Err(_) => skipped_range += 1,
        }
    }
    if skipped_subhour > 0 {
        notes.push(format!(
            "{skipped_subhour} message(s) at sub-hourly offsets skipped (hour slots are whole \
             hours)"
        ));
    }
    if skipped_range > 0 {
        notes.push(format!(
            "{skipped_range} message(s) more than {} hours after the first timestep skipped \
             (store hour keys are u16)",
            u16::MAX
        ));
    }
    if hours.is_empty() {
        return Err("no importable timesteps".to_string());
    }

    let run = run_name(paths, &first_msg, first_valid);
    let model = "wrf".to_string();
    let total_hours = hours.len();
    let last_valid = first_valid
        + i64::from(*hours.keys().next_back().unwrap_or(&0)) * 3_600;

    // ---- Decode-write-drop, one timestep at a time. ----
    let mut files: Vec<File> = Vec::with_capacity(paths.len());
    for path in paths {
        files.push(
            File::open(path).map_err(|err| format!("{}: open: {err}", display_name(path)))?,
        );
    }
    let mut all_vars: Vec<String> = Vec::new();
    let mut hours_written = 0usize;
    let mut duplicate_fields = 0usize;
    for (step, (&hour, group)) in hours.iter().enumerate() {
        let mut canonical: Vec<(String, SelectedField2D)> = Vec::new();
        let mut derived: Vec<(String, String, Vec<f32>)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (file_idx, msg) in group {
            let field = plan_field(msg);
            if !seen.insert(field.name().to_string()) {
                duplicate_fields += 1;
                continue;
            }
            let label = display_name(&paths[*file_idx]);
            let values = decode_values(&mut files[*file_idx], msg, &plan, field.scale(), &label)?;
            match field {
                PlannedField::Canonical {
                    name,
                    selector,
                    units,
                    ..
                } => {
                    let selected =
                        SelectedField2D::new(selector, units, plan.grid.clone(), values)
                            .map_err(|err| format!("{label}: field {name}: {err}"))?;
                    canonical.push((name, selected));
                }
                PlannedField::Derived { name, units, .. } => {
                    derived.push((name, units, values));
                }
            }
        }

        // Wind speed companions where both components landed (same derived-
        // at-ingest convention as the WRF import's `wind_speed_10m`).
        let mut speeds: Vec<(String, SelectedField2D)> = Vec::new();
        for (name, u_field) in &canonical {
            let Some(rest) = name.strip_prefix("u_") else {
                continue;
            };
            let Some((_, v_field)) = canonical
                .iter()
                .find(|(v_name, _)| v_name == &format!("v_{rest}"))
            else {
                continue;
            };
            let speed_name = format!("wind_speed_{rest}");
            if seen.contains(&speed_name) {
                continue;
            }
            let selector = if rest == "10m" {
                FieldSelector::height_agl(CanonicalField::WindSpeed, 10)
            } else if let Ok(level) = rest.parse::<u16>() {
                FieldSelector::isobaric(CanonicalField::WindSpeed, level)
            } else {
                continue;
            };
            let values: Vec<f32> = u_field
                .values
                .iter()
                .zip(&v_field.values)
                .map(|(u, v)| u.mul_add(*u, v * v).sqrt())
                .collect();
            if let Ok(selected) =
                SelectedField2D::new(selector, "m/s", plan.grid.clone(), values)
            {
                seen.insert(speed_name.clone());
                speeds.push((speed_name, selected));
            }
        }
        canonical.extend(speeds);

        // The store writer requires a canonical field to carry the hour
        // grid. When a file yields only derived params (the fsr case),
        // promote the first one with a GeopotentialHeight surface selector:
        // `operational_style_for_store_variable` explicitly returns None for
        // that field (production never color-fills heights), so the selector
        // carries the grid and nothing else.
        if canonical.is_empty() {
            let (name, units, values) = derived.remove(0);
            let selected = SelectedField2D::new(
                FieldSelector::surface(CanonicalField::GeopotentialHeight),
                units,
                plan.grid.clone(),
                values,
            )
            .map_err(|err| format!("field {name}: {err}"))?;
            canonical.insert(0, (name, selected));
        }

        progress(format!(
            "GRIB1 {run}: timestep {}/{total_hours} (f{hour:03}, {}) — {} field(s)",
            step + 1,
            format_utc(first_valid + i64::from(hour) * 3_600),
            canonical.len() + derived.len(),
        ));

        let refs: Vec<(&str, &SelectedField2D)> = canonical
            .iter()
            .map(|(name, field)| (name.as_str(), field))
            .collect();
        let raw_refs: Vec<DerivedFieldInput<'_>> = derived
            .iter()
            .map(|(name, units, values)| DerivedFieldInput {
                name,
                units,
                values,
            })
            .collect();
        let written = write_hour_from_fields_with_derived(
            store_root,
            &model,
            &run,
            hour,
            &refs,
            &raw_refs,
            &[],
            writer_build(),
            now_unix(),
        )
        .map_err(|err| format!("store write f{hour:03}: {err}"))?;
        all_vars.extend(written.vars);
        hours_written += 1;
    }

    if duplicate_fields > 0 {
        notes.push(format!(
            "{duplicate_fields} duplicate field/timestep message(s) skipped (first occurrence \
             wins)"
        ));
    }
    notes.push(format!(
        "{hours_written} timestep(s), {} to {}",
        format_utc(first_valid),
        format_utc(last_valid)
    ));

    all_vars.sort();
    all_vars.dedup();
    Ok(LocalImportSummary {
        store_root: store_root.to_path_buf(),
        model,
        run,
        files_seen: paths.len(),
        hours_written,
        variables: all_vars,
        notes,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// First message (bytes 0..153,708) of the owner's real ERA-20C file
    /// `e20c.oper.an.sfc.3hr.128_244_fsr.regn80sc.2004010100_2004123121.grb`
    /// (ECMWF, table 128, param 244 fsr, N80 Gaussian 320x160, 24-bit simple
    /// packing) — vendored whole so the regression runs the exact bytes the
    /// import path sees.
    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/e20c_fsr_2004010100_msg0.grb")
    }

    fn fixture_bytes() -> Vec<u8> {
        std::fs::read(fixture_path()).expect("read vendored ERA-20C fixture")
    }

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bowecho-grib1-{name}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        std::fs::write(&path, bytes).expect("write temp grib");
        path
    }

    #[test]
    fn fixture_index_reads_pds_and_gds_facts() {
        let msgs = index_grib1_file(&fixture_path()).expect("index fixture");
        assert_eq!(msgs.len(), 1);
        let msg = &msgs[0];
        assert_eq!(msg.offset, 0);
        assert_eq!(msg.total_len, 153_708);
        assert_eq!(msg.table_version, 128);
        assert_eq!(msg.center, 98);
        assert_eq!(msg.parameter, 244);
        assert_eq!(msg.level_type, 1);
        assert_eq!(msg.level_value, 0);
        assert_eq!((msg.ni, msg.nj), (320, 160));
        // Reference time 2004-01-01 00:00Z, analysis (tri 0, P1 0).
        let expected = chrono::NaiveDate::from_ymd_opt(2004, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(msg.valid_unix, expected);
    }

    #[test]
    fn index_walks_concatenated_messages_and_padding() {
        let bytes = fixture_bytes();
        let mut doubled = bytes.clone();
        // Inter-message padding: the indexer must scan forward to the next
        // magic rather than assume back-to-back records.
        doubled.extend_from_slice(&[0u8; 16]);
        doubled.extend_from_slice(&bytes);
        let path = temp_file("concat", &doubled);
        let msgs = index_grib1_file(&path).expect("index concatenated");
        std::fs::remove_file(&path).ok();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].offset, 0);
        assert_eq!(msgs[1].offset, 153_708 + 16);
        assert_eq!(msgs[1].total_len, 153_708);
    }

    #[test]
    fn index_rejects_grib2_editions() {
        let mut bytes = fixture_bytes();
        bytes[7] = 2;
        let path = temp_file("edition2", &bytes);
        let err = index_grib1_file(&path).expect_err("edition 2 must be rejected");
        std::fs::remove_file(&path).ok();
        assert!(err.contains("edition 2"), "unexpected error: {err}");
    }

    #[test]
    fn fixture_unpack_matches_hand_decoded_values() {
        // Hand-decoded from the fixture's hex (see grib1-import-notes.md):
        // BDS binary scale E = -23 (0x8017 sign-magnitude), reference
        // R = IBM 0x3D195DE7 = 1_662_439 * 2^-36, 24 bits/value, decimal
        // scale 0. First three packed integers: 8166, 8166, 8165.
        let reference = 1_662_439.0 * (2.0_f64).powi(-36);
        let v0 = reference + 8_166.0 * (2.0_f64).powi(-23);
        let v2 = reference + 8_165.0 * (2.0_f64).powi(-23);

        let msgs = index_grib1_file(&fixture_path()).expect("index fixture");
        let mut file = File::open(fixture_path()).expect("open fixture");
        let parsed = parse_message_at(&mut file, &msgs[0], "fixture").expect("parse");
        let plan = build_grid_plan(&parsed).expect("grid plan");
        let values = decode_values(&mut file, &msgs[0], &plan, 1.0, "fixture").expect("decode");

        assert_eq!(values.len(), 320 * 160);
        // The global grid rotates by 160 columns (lon 180 -> output column
        // 0), so source column 0 lands at output column 160.
        assert_eq!(plan.rotate, 160);
        assert!(
            (f64::from(values[160]) - v0).abs() < 1e-9,
            "values[160] = {}, hand-decoded {v0}",
            values[160]
        );
        assert!(
            (f64::from(values[162]) - v2).abs() < 1e-9,
            "values[162] = {}, hand-decoded {v2}",
            values[162]
        );
        // Physical plausibility across the whole plane: surface roughness is
        // non-negative, meters, small over ocean/ice and < 10 m everywhere.
        let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
        for &value in &values {
            assert!(value.is_finite());
            min = min.min(value);
            max = max.max(value);
        }
        assert!(min >= 0.0, "roughness must be non-negative, got {min}");
        assert!(max < 10.0, "roughness above 10 m is implausible, got {max}");
        assert!(max > 0.1, "land roughness should exceed 0.1 m, got {max}");
    }

    #[test]
    fn fixture_grid_rotates_to_monotonic_signed_longitudes() {
        let msgs = index_grib1_file(&fixture_path()).expect("index fixture");
        let mut file = File::open(fixture_path()).expect("open fixture");
        let parsed = parse_message_at(&mut file, &msgs[0], "fixture").expect("parse");
        let plan = build_grid_plan(&parsed).expect("grid plan");

        assert_eq!((plan.nx, plan.ny), (320, 160));
        // First Gaussian latitude for N80 is 89.1416 (Legendre root), which
        // the GDS encodes as 89.142 millidegrees-truncated.
        let lat0 = f64::from(plan.grid.lat_deg[0]);
        assert!((lat0 - 89.1416).abs() < 0.01, "lat0 = {lat0}");
        let lat_last = f64::from(plan.grid.lat_deg[(160 - 1) * 320]);
        assert!((lat_last + 89.1416).abs() < 0.01, "lat_last = {lat_last}");
        // Longitudes: -180 .. 178.875 step 1.125, strictly ascending — the
        // map layer's inverse LUT does not wrap 0..360 grids.
        let lons: Vec<f32> = plan.grid.lon_deg[..320].to_vec();
        assert!((f64::from(lons[0]) + 180.0).abs() < 1e-6, "lon0 = {}", lons[0]);
        assert!(
            (f64::from(lons[319]) - 178.875).abs() < 1e-3,
            "lon_last = {}",
            lons[319]
        );
        assert!(
            lons.windows(2).all(|pair| pair[1] > pair[0]),
            "rotated longitudes must ascend monotonically"
        );
        // Latitude constant along a row.
        assert_eq!(plan.grid.lat_deg[0], plan.grid.lat_deg[319]);
    }

    #[test]
    fn era128_param_labels_cover_the_task_set() {
        let fsr = era128_param(244).expect("fsr");
        assert_eq!(fsr.short, "fsr");
        assert_eq!(fsr.slug, "forecast_surface_roughness");
        assert_eq!(fsr.label, "Forecast surface roughness");
        assert_eq!(fsr.units, "m");
        for (param, short) in [
            (129u8, "z"),
            (130, "t"),
            (131, "u"),
            (132, "v"),
            (133, "q"),
            (134, "sp"),
            (151, "msl"),
            (165, "10u"),
            (166, "10v"),
            (167, "2t"),
            (168, "2d"),
            (228, "tp"),
            (59, "cape"),
        ] {
            assert_eq!(era128_param(param).expect("param").short, short);
        }
        assert!(era128_param(0).is_none());
    }

    fn indexed(parameter: u8, level_type: u8, level_value: u16) -> IndexedMessage {
        IndexedMessage {
            offset: 0,
            total_len: 0,
            table_version: 128,
            center: 98,
            parameter,
            level_type,
            level_value,
            valid_unix: 0,
            ni: 320,
            nj: 160,
        }
    }

    #[test]
    fn field_plan_maps_canonical_and_derived_params() {
        // 2 m temperature: canonical with the WRF-import store name.
        match plan_field(&indexed(167, 1, 0)) {
            PlannedField::Canonical { name, units, .. } => {
                assert_eq!(name, "temperature_2m");
                assert_eq!(units, "K");
            }
            other => panic!("2t must be canonical, got {other:?}"),
        }
        // 850 hPa temperature: iso naming contract slug (temperature_850).
        match plan_field(&indexed(130, 100, 850)) {
            PlannedField::Canonical { name, .. } => assert_eq!(name, "temperature_850"),
            other => panic!("t850 must be canonical, got {other:?}"),
        }
        // Geopotential at 500 hPa: stored as height (gpm), scaled by 1/g.
        match plan_field(&indexed(129, 100, 500)) {
            PlannedField::Canonical {
                name, units, scale, ..
            } => {
                assert_eq!(name, "height_500");
                assert_eq!(units, "gpm");
                assert!((scale - 1.0 / STANDARD_GRAVITY).abs() < 1e-12);
            }
            other => panic!("z500 must be canonical height, got {other:?}"),
        }
        // fsr: no canonical mapping — derived slug with ERA units.
        match plan_field(&indexed(244, 1, 0)) {
            PlannedField::Derived { name, units, .. } => {
                assert_eq!(name, "forecast_surface_roughness");
                assert_eq!(units, "m");
            }
            other => panic!("fsr must be derived, got {other:?}"),
        }
        // Specific humidity on a level keeps the level suffix.
        match plan_field(&indexed(133, 100, 700)) {
            PlannedField::Derived { name, .. } => assert_eq!(name, "specific_humidity_700"),
            other => panic!("q700 must be derived, got {other:?}"),
        }
        // Unknown parameter in an unknown table: self-describing fallback.
        let mut unknown = indexed(250, 1, 0);
        unknown.table_version = 200;
        match plan_field(&unknown) {
            PlannedField::Derived { name, .. } => assert_eq!(name, "p250_t200"),
            other => panic!("unknown param must be derived, got {other:?}"),
        }
    }

    #[test]
    fn forecast_offsets_follow_time_unit_and_range_indicator() {
        // Analysis: tri 0, P1 0 (the ERA-20C case).
        assert_eq!(forecast_offset_seconds(1, 0, 0, 0), Some(0));
        // 3-hour forecast in hour units.
        assert_eq!(forecast_offset_seconds(1, 3, 0, 0), Some(3 * 3_600));
        // Day units.
        assert_eq!(forecast_offset_seconds(2, 2, 0, 0), Some(2 * 86_400));
        // Accumulation valid at the end of (P1, P2).
        assert_eq!(forecast_offset_seconds(1, 0, 6, 4), Some(6 * 3_600));
        // Two-octet P1 (tri 10).
        assert_eq!(forecast_offset_seconds(1, 1, 4, 10), Some(260 * 3_600));
        // Calendar units have no fixed length.
        assert_eq!(forecast_offset_seconds(3, 1, 0, 0), None);
    }

    #[test]
    fn fixture_imports_to_store_and_reads_back() {
        let store_root = std::env::temp_dir().join(format!(
            "bowecho-grib1-store-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let mut lines = Vec::new();
        let summary = import_grib1_files(
            &[fixture_path()],
            &store_root,
            &mut |line| lines.push(line),
        )
        .expect("import fixture");

        assert_eq!(summary.model, "wrf");
        assert_eq!(summary.run, "era20c_fsr_2004010100");
        assert_eq!(summary.hours_written, 1);
        assert_eq!(summary.variables, vec!["forecast_surface_roughness"]);
        assert!(!lines.is_empty());

        let hour_path = store_root
            .join(&summary.model)
            .join(&summary.run)
            .join("f000.rws");
        let reader = rw_store::reader::HourReader::open(&hour_path).expect("open written hour");
        let var = reader
            .variable("forecast_surface_roughness")
            .expect("written variable");
        assert_eq!(var.units, "m");
        let values = reader
            .read_full_2d("forecast_surface_roughness")
            .expect("read plane back");
        assert_eq!(values.len(), 320 * 160);
        // Same hand-decoded first value as the unpack test, at its rotated
        // column, surviving the store round-trip (f32 store codec).
        let v0 = 1_662_439.0 * (2.0_f64).powi(-36) + 8_166.0 * (2.0_f64).powi(-23);
        assert!(
            (f64::from(values[160]) - v0).abs() < 1e-6,
            "store round-trip values[160] = {}, expected {v0}",
            values[160]
        );

        std::fs::remove_dir_all(&store_root).ok();
    }

    /// Full-file proof against the owner's real 450 MB ERA-20C download
    /// (env-gated like `RW_LOCAL_IMPORT_FIXTURE`): index all 2,928 messages,
    /// verify the 3-hourly axis spans the year monotonically, and decode the
    /// first/middle/last planes through the real path.
    #[test]
    fn optional_era20c_full_file_indexes_and_decodes() {
        let Ok(fixture) = std::env::var("RW_ERA20C_GRIB_FIXTURE") else {
            eprintln!("skipping ERA-20C full-file test; set RW_ERA20C_GRIB_FIXTURE");
            return;
        };
        let path = PathBuf::from(&fixture);
        let started = std::time::Instant::now();
        let msgs = index_grib1_file(&path).expect("index full file");
        let index_elapsed = started.elapsed();
        assert_eq!(msgs.len(), 2_928, "expected exactly 2,928 messages");

        // Monotonic 3-hourly valid times across the whole year.
        for pair in msgs.windows(2) {
            assert_eq!(
                pair[1].valid_unix - pair[0].valid_unix,
                3 * 3_600,
                "3-hourly step broken between offsets {} and {}",
                pair[0].offset,
                pair[1].offset
            );
        }
        let span_hours = (msgs.last().unwrap().valid_unix - msgs[0].valid_unix) / 3_600;
        assert_eq!(span_hours, 8_781, "year of 3-hourly steps spans 8,781 h");

        let mut file = File::open(&path).expect("open full file");
        let plan = build_grid_plan(
            &parse_message_at(&mut file, &msgs[0], "full").expect("parse first"),
        )
        .expect("grid plan");
        let decode_started = std::time::Instant::now();
        for msg in [&msgs[0], &msgs[msgs.len() / 2], &msgs[msgs.len() - 1]] {
            let values = decode_values(&mut file, msg, &plan, 1.0, "full").expect("decode");
            assert_eq!(values.len(), 320 * 160);
            assert!(values.iter().all(|value| value.is_finite() && *value >= 0.0));
        }
        eprintln!(
            "ERA-20C full file: indexed {} messages in {:.2?}, decoded 3 planes in {:.2?}",
            msgs.len(),
            index_elapsed,
            decode_started.elapsed()
        );
    }
}
