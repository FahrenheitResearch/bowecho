//! Native "synthetic radar from WRF" — turn an ingested WRF forecast hour
//! into a [`radar_core::RadarVolume`] of SIMULATED reflectivity (and radial
//! velocity) sampled onto a polar range/azimuth/elevation grid, so the model
//! output renders and LOOPS through the existing radar viewer (colormaps,
//! cross-sections, GBVTD, loop engine) with no new file format.
//!
//! The scan is virtual: a synthetic NEXRAD-like antenna is placed over the WRF
//! domain and, for every (elevation, azimuth, range) gate, the beam centre is
//! traced through the 4/3-earth model to a model-space (lat, lon, MSL height),
//! where the model's 3-D reflectivity and earth-relative winds are
//! trilinearly sampled. The result is stored as true `f32` dBZ / m·s⁻¹
//! (`MomentStorage::F32`, scale 1, offset 0) so the render/dealias/GBVTD F32
//! paths and the standard REF/VEL colour tables consume it unchanged.
//!
//! Physics / algorithm references:
//! - Beam geometry (height + ground range under the 4/3-earth effective-radius
//!   refraction model): Doviak & Zrnić (1993), *Doppler Radar and Weather
//!   Observations* (2nd ed.), eq. 2.28b/c — via
//!   [`radar_core::beam_height_above_radar_m`] /
//!   [`radar_core::beam_ground_range_m`].
//! - Simulated reflectivity Z from hydrometeor mixing ratios: Stoelinga (2005),
//!   "Simulated equivalent reflectivity factor as currently formulated in the
//!   WRF model" (WRF microphysics tech note); Thompson et al. (2008), *Mon.
//!   Wea. Rev.* 136, 5095–5115 (variable-intercept option) — computed inside
//!   `wrf-core`'s `dbz` (`CALCDBZ`) diagnostic, or read directly from the
//!   model's own `REFL_10CM` field when present.
//! - Radial velocity as the projection of the 3-D wind onto the beam unit
//!   vector: Sun & Crook (1997), *J. Atmos. Sci.* 54, 1642–1661 (radar radial
//!   velocity in a variational/forward-operator context).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use chrono::{DateTime, NaiveDateTime, Utc};
use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, RadarSite, RadarVolume, Radial,
    ScanMode, VolumeMetadata, beam_ground_range_m, beam_height_above_radar_m,
};
use rayon::prelude::*;
use wrf_core::{ComputeOpts, WrfFile, getvar};

use crate::model_layer::{
    InverseLut, neighboring_cell_starts, solve_bilinear_coords, unwrap_lon_near,
};
use ui_core::geo::aeqd_inverse_km;

/// Default WSR-88D-like elevation ladder (deg). Covers the low tilts that
/// dominate a plan-view display plus enough high tilts for cross-sections.
pub const DEFAULT_ELEVATIONS_DEG: &[f64] = &[
    0.5, 0.9, 1.3, 1.8, 2.4, 3.1, 4.0, 5.1, 6.4, 8.0, 10.0, 12.5, 15.6, 19.5,
];

/// Configuration for one synthetic scan.
#[derive(Clone, Debug)]
pub struct SyntheticRadarConfig {
    /// Site id stamped on the volume (drives the loop-engine cross-site guard —
    /// every hour of one run must share it).
    pub site_id: String,
    pub site_name: Option<String>,
    /// Antenna position. `None` places it at the WRF domain centre.
    pub site_lat_deg: Option<f64>,
    pub site_lon_deg: Option<f64>,
    /// Antenna altitude MSL (m). `None` uses the model terrain at the site plus
    /// [`DEFAULT_TOWER_M`].
    pub antenna_msl_m: Option<f64>,
    pub elevations_deg: Vec<f64>,
    /// Azimuth samples per sweep, clockwise from north (e.g. 360 → 1.0°, 720 →
    /// 0.5°).
    pub azimuth_count: usize,
    pub gate_spacing_m: f64,
    pub max_range_m: f64,
    /// Reflectivity floor (dBZ): gates below this — and their velocity — are
    /// left NaN so clear air renders transparent, like a real scope.
    pub ref_floor_dbz: f32,
    /// Nyquist velocity stamped on each radial. Deliberately large so the
    /// native, forward-modelled Vr is treated as already unfolded (TRUE
    /// velocity) by downstream dealias/readout code.
    pub nyquist_mps: f32,
    /// Prefer the model's own `REFL_10CM` 3-D reflectivity when the file
    /// carries it; otherwise fall back to the computed `dbz` (`CALCDBZ`).
    pub prefer_refl_10cm: bool,
}

/// Antenna height above model terrain when no explicit MSL altitude is given.
pub const DEFAULT_TOWER_M: f64 = 10.0;

impl Default for SyntheticRadarConfig {
    fn default() -> Self {
        Self {
            site_id: "WRF".to_string(),
            site_name: Some("Simulated WRF radar".to_string()),
            site_lat_deg: None,
            site_lon_deg: None,
            antenna_msl_m: None,
            elevations_deg: DEFAULT_ELEVATIONS_DEG.to_vec(),
            azimuth_count: 720,
            gate_spacing_m: 250.0,
            max_range_m: 230_000.0,
            ref_floor_dbz: 0.0,
            nyquist_mps: 320.0,
            prefer_refl_10cm: true,
        }
    }
}

/// The 3-D model fields one synthetic scan samples, read once per forecast
/// time and flattened to `f32` on the WRF unstaggered grid.
///
/// All 3-D arrays are row-major `[nz, ny, nx]` (index `k * ny*nx + j*nx + i`);
/// lat/lon are `[ny, nx]`. `dbz` is dBZ, winds are earth-relative m·s⁻¹,
/// `height_msl` is metres MSL.
pub struct WrfRadarFields {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub lat: Vec<f32>,
    pub lon: Vec<f32>,
    pub height_msl: Vec<f32>,
    pub dbz: Vec<f32>,
    pub u: Vec<f32>,
    pub v: Vec<f32>,
    pub w: Vec<f32>,
    pub terrain_m: Vec<f32>,
    /// Which reflectivity source populated `dbz` ("REFL_10CM" or "dbz/CALCDBZ").
    pub ref_source: &'static str,
    lut: InverseLut,
}

impl WrfRadarFields {
    fn cells(&self) -> usize {
        self.nx * self.ny
    }

    /// Domain-centre grid cell (used for the default antenna position).
    fn center_cell(&self) -> usize {
        (self.ny / 2) * self.nx + (self.nx / 2)
    }
}

/// Read the WRF 3-D reflectivity + earth-relative winds + height for one time.
///
/// Reflectivity: `REFL_10CM` (the model's own Thompson 10-cm reflectivity)
/// when present and `prefer_refl_10cm`, else `wrf-core`'s `dbz` (`CALCDBZ`,
/// Stoelinga 2005) — the same diagnostic BowEcho's composite reflectivity uses,
/// so a synthetic scan co-locates with the model's own composite. Raw wrfout
/// carries the hydrometeor mixing ratios `dbz` needs; a post-processed /
/// climate wrfout may not — this returns an `Err` (empty/absent reflectivity)
/// so the caller can warn rather than emit an all-NaN scan.
pub fn read_wrf_radar_fields(
    file: &WrfFile,
    timeidx: usize,
    prefer_refl_10cm: bool,
) -> Result<WrfRadarFields, String> {
    read_wrf_radar_fields_reporting(file, timeidx, prefer_refl_10cm, &|_| {})
}

/// Read one time's fields, streaming stage labels through `progress` so the UI
/// can show "Reading …" instead of freezing.
///
/// PERF: the four heavy 3-D fields (height, reflectivity, earth-relative winds,
/// vertical velocity) are the whole cost of a synthetic scan — on a 250 m
/// 800×800×79 wrfout the NetCDF decompress dominates (~7 s serial), while the
/// polar sampling that follows is <0.1 s. Each is an independent variable, so
/// they are read/decompressed on separate threads with `std::thread::scope`.
/// The pure-Rust HDF5 reader guards only the file handle with a mutex and
/// decompresses (the expensive part) without it, so the inflates overlap:
/// wall time drops to the single longest field (~2.5–3 s here, ~2.5× faster).
/// Each thread calls the exact same `getvar`/`read_var` entry points as before,
/// so the sampled output is byte-for-byte unchanged — this is a speed change,
/// not an accuracy change.
pub fn read_wrf_radar_fields_reporting(
    file: &WrfFile,
    timeidx: usize,
    prefer_refl_10cm: bool,
    progress: &dyn Fn(&str),
) -> Result<WrfRadarFields, String> {
    let nx = file.nx;
    let ny = file.ny;
    let nz = file.nz;
    let cells = nx * ny;
    if cells == 0 || nz == 0 {
        return Err("WRF grid has zero cells".to_string());
    }

    let lat = file
        .xlat(timeidx)
        .map_err(|err| format!("read XLAT: {err}"))?;
    let lon = file
        .xlong(timeidx)
        .map_err(|err| format!("read XLONG: {err}"))?;
    if lat.len() != cells || lon.len() != cells {
        return Err(format!(
            "WRF lat/lon size mismatch: expected {cells}, got lat {} lon {}",
            lat.len(),
            lon.len()
        ));
    }

    progress("reading model fields (reflectivity, winds, height)…");

    // Read the four heavy 3-D fields concurrently. Placeholders are overwritten
    // inside the scope; the scope join guarantees they are all set on exit.
    let mut height_res: Result<Vec<f32>, String> = Err("height not read".to_string());
    let mut refl_res: Result<(Vec<f32>, &'static str), String> = Err("refl not read".to_string());
    let mut winds_res: Result<(Vec<f32>, Vec<f32>), String> = Err("winds not read".to_string());
    let mut w_res: Result<Vec<f32>, String> = Err("wa not read".to_string());
    let mut terrain_m: Vec<f32> = Vec::new();
    std::thread::scope(|scope| {
        let th_height = scope.spawn(|| read_3d(file, "height", timeidx, nz * cells));
        let th_refl =
            scope.spawn(|| read_reflectivity(file, timeidx, nz * cells, prefer_refl_10cm));
        let th_winds = scope.spawn(|| read_earth_relative_winds(file, timeidx, nz * cells));
        let th_w = scope.spawn(|| read_3d(file, "wa", timeidx, nz * cells));
        let th_terrain = scope.spawn(|| read_terrain_m(file, timeidx, cells));

        height_res = join_read(th_height, "height");
        refl_res = join_read(th_refl, "reflectivity");
        winds_res = join_read(th_winds, "winds");
        w_res = join_read(th_w, "wa");
        terrain_m = th_terrain.join().unwrap_or_else(|_| vec![0.0; cells]);
    });

    let height = height_res?;
    let (dbz, ref_source) = refl_res?;
    if dbz.iter().all(|value| !value.is_finite()) {
        return Err(format!(
            "WRF reflectivity ({ref_source}) is entirely missing — is this a \
             post-processed/climate wrfout without hydrometeor mixing ratios?"
        ));
    }
    let (u, v) = winds_res?;
    let w = w_res?;

    progress("building geolocation index…");
    let lat_f32 = to_f32(&lat);
    let lon_f32 = to_f32(&lon);
    let lut = InverseLut::build_with_shape(&lat_f32, &lon_f32, nx, ny)
        .ok_or_else(|| "failed to build WRF inverse geolocation LUT".to_string())?;

    Ok(WrfRadarFields {
        nx,
        ny,
        nz,
        lat: lat_f32,
        lon: lon_f32,
        height_msl: height,
        dbz,
        u,
        v,
        w,
        terrain_m,
        ref_source,
        lut,
    })
}

/// Earth-relative winds. `uvmet` returns `[u_earth.., v_earth..]`
/// (2 * nz * cells); fall back to grid-relative `ua`/`va` if unavailable.
/// Extracted verbatim from the original inline logic so the values match.
fn read_earth_relative_winds(
    file: &WrfFile,
    timeidx: usize,
    expected: usize,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    match getvar(file, "uvmet", Some(timeidx), &ComputeOpts::default()) {
        Ok(uvmet) if uvmet.data.len() == 2 * expected => {
            let (ue, ve) = uvmet.data.split_at(expected);
            Ok((to_f32(ue), to_f32(ve)))
        }
        _ => {
            let ua = read_3d(file, "ua", timeidx, expected)?;
            let va = read_3d(file, "va", timeidx, expected)?;
            Ok((ua, va))
        }
    }
}

fn read_terrain_m(file: &WrfFile, timeidx: usize, cells: usize) -> Vec<f32> {
    file.terrain(timeidx)
        .map(|ter| ter.iter().map(|value| *value as f32).collect::<Vec<_>>())
        .unwrap_or_else(|_| vec![0.0; cells])
}

/// Join a scoped read thread, turning a thread panic into a readable error.
fn join_read<T>(
    handle: std::thread::ScopedJoinHandle<'_, Result<T, String>>,
    what: &str,
) -> Result<T, String> {
    match handle.join() {
        Ok(inner) => inner,
        Err(_) => Err(format!("WRF {what} read thread panicked")),
    }
}

fn read_reflectivity(
    file: &WrfFile,
    timeidx: usize,
    expected: usize,
    prefer_refl_10cm: bool,
) -> Result<(Vec<f32>, &'static str), String> {
    if prefer_refl_10cm
        && file.has_var("REFL_10CM")
        && let Ok(raw) = file.read_var("REFL_10CM", timeidx)
        && raw.len() == expected
    {
        return Ok((to_f32(&raw), "REFL_10CM"));
    }
    // wrf-core `dbz` = CALCDBZ (Stoelinga 2005), the same source BowEcho's
    // composite reflectivity uses. Constant intercepts / no bright-band
    // correction (ComputeOpts default) to match that composite exactly.
    let dbz = read_3d(file, "dbz", timeidx, expected)
        .map_err(|err| format!("no REFL_10CM and computed dbz failed: {err}"))?;
    Ok((dbz, "dbz/CALCDBZ"))
}

fn read_3d(
    file: &WrfFile,
    name: &str,
    timeidx: usize,
    expected: usize,
) -> Result<Vec<f32>, String> {
    let out = getvar(file, name, Some(timeidx), &ComputeOpts::default())
        .map_err(|err| format!("read WRF {name}: {err}"))?;
    if out.data.len() != expected {
        return Err(format!(
            "WRF {name} has {} values, expected {expected}",
            out.data.len()
        ));
    }
    Ok(to_f32(&out.data))
}

fn to_f32(values: &[f64]) -> Vec<f32> {
    values.iter().map(|value| *value as f32).collect()
}

/// Build one synthetic [`RadarVolume`] from pre-read [`WrfRadarFields`].
///
/// `valid_time` is the volume's scan time (the WRF forecast valid time), which
/// keys the frame in a loop.
pub fn build_synthetic_volume(
    fields: &WrfRadarFields,
    valid_time: DateTime<Utc>,
    config: &SyntheticRadarConfig,
) -> RadarVolume {
    build_synthetic_volume_reporting(fields, valid_time, config, &|_| {})
}

/// As [`build_synthetic_volume`], but streams a per-tilt progress label so the
/// UI shows "building tilt k/n…" instead of freezing while the polar volume is
/// traced. The per-tilt work itself is unchanged.
pub fn build_synthetic_volume_reporting(
    fields: &WrfRadarFields,
    valid_time: DateTime<Utc>,
    config: &SyntheticRadarConfig,
    progress: &dyn Fn(&str),
) -> RadarVolume {
    let cells = fields.cells();
    let center = fields.center_cell();
    let site_lat = config
        .site_lat_deg
        .unwrap_or_else(|| fields.lat[center] as f64);
    let site_lon = config
        .site_lon_deg
        .unwrap_or_else(|| fields.lon[center] as f64);
    let antenna_msl = config
        .antenna_msl_m
        .unwrap_or_else(|| fields.terrain_m[center] as f64 + DEFAULT_TOWER_M);

    let naz = config.azimuth_count.max(1);
    let spacing = config.gate_spacing_m.max(1.0);
    let gate_count = ((config.max_range_m / spacing).floor() as usize).max(1);
    let gate_range = GateRange {
        first_gate_m: 0,
        gate_spacing_m: spacing.round() as i32,
        gate_count,
    };

    let mut site = RadarSite::new(config.site_id.clone());
    site.name = config.site_name.clone();
    site.latitude_deg = Some(site_lat as f32);
    site.longitude_deg = Some(site_lon as f32);
    site.elevation_m = Some(antenna_msl as f32);

    let mut volume = RadarVolume::new(site, valid_time);
    volume.metadata = VolumeMetadata {
        source_path: None,
        archive_version: Some("simulated-wrf".to_string()),
        compression: None,
        message_count: 0,
        decoded_radial_count: 0,
        skipped_message_count: 0,
        scan_mode: Some(ScanMode::Ppi),
        radar_frequency_mhz: None,
    };

    let mut decoded_radials = 0usize;
    let tilt_total = config.elevations_deg.len();
    for (cut_index, &elevation_deg) in config.elevations_deg.iter().enumerate() {
        progress(&format!(
            "building tilt {}/{tilt_total} ({elevation_deg:.1}°)…",
            cut_index + 1
        ));
        let cut = build_cut(
            fields,
            cells,
            site_lat,
            site_lon,
            antenna_msl,
            elevation_deg,
            cut_index,
            naz,
            spacing,
            gate_range.clone(),
            config,
        );
        decoded_radials += cut.radials.len();
        volume.cuts.push(cut);
    }
    volume.metadata.decoded_radial_count = decoded_radials;
    volume
}

#[allow(clippy::too_many_arguments)]
fn build_cut(
    fields: &WrfRadarFields,
    cells: usize,
    site_lat: f64,
    site_lon: f64,
    antenna_msl: f64,
    elevation_deg: f64,
    cut_index: usize,
    naz: usize,
    spacing: f64,
    gate_range: GateRange,
    config: &SyntheticRadarConfig,
) -> ElevationCut {
    let gate_count = gate_range.gate_count;
    let el_rad = elevation_deg.to_radians();
    let sin_el = el_rad.sin();
    let cos_el = el_rad.cos();
    let floor = config.ref_floor_dbz;

    // One row per radial, sampled in parallel. Each row is `gate_count` REF and
    // `gate_count` VEL f32 values (NaN = no data / below floor / off-domain).
    let rows: Vec<(Vec<f32>, Vec<f32>)> = (0..naz)
        .into_par_iter()
        .map(|iaz| {
            let az_deg = iaz as f64 * 360.0 / naz as f64;
            let az_rad = az_deg.to_radians();
            let sin_az = az_rad.sin();
            let cos_az = az_rad.cos();

            let mut ref_row = vec![f32::NAN; gate_count];
            let mut vel_row = vec![f32::NAN; gate_count];
            for gate in 0..gate_count {
                let slant_m = gate as f64 * spacing;
                // Doviak & Zrnić (1993) eq. 2.28b/c under the 4/3-earth model.
                let z_msl = antenna_msl + beam_height_above_radar_m(slant_m, elevation_deg);
                let ground_m = beam_ground_range_m(slant_m, elevation_deg);
                let east_km = ground_m * sin_az / 1000.0;
                let north_km = ground_m * cos_az / 1000.0;
                let (lat, lon) = aeqd_inverse_km(site_lat, site_lon, east_km, north_km);

                let Some(sample) =
                    sample_column(fields, cells, lat as f32, lon as f32, z_msl as f32)
                else {
                    continue;
                };
                if !sample.dbz.is_finite() || sample.dbz < floor {
                    continue;
                }
                ref_row[gate] = sample.dbz;
                // Radial velocity: wind projected onto the beam unit vector
                // (east, north, up) = (sinAz·cosEl, cosAz·cosEl, sinEl), with
                // azimuth clockwise from north. Positive = away from the radar
                // (NEXRAD convention). Sun & Crook (1997).
                let vr = sample.u * (sin_az as f32) * (cos_el as f32)
                    + sample.v * (cos_az as f32) * (cos_el as f32)
                    + sample.w * (sin_el as f32);
                if vr.is_finite() {
                    vel_row[gate] = vr;
                }
            }
            (ref_row, vel_row)
        })
        .collect();

    let mut cut = ElevationCut::new(elevation_deg as f32, u8::try_from(cut_index + 1).ok());
    let mut ref_values = Vec::with_capacity(naz * gate_count);
    let mut vel_values = Vec::with_capacity(naz * gate_count);
    for (iaz, (ref_row, vel_row)) in rows.into_iter().enumerate() {
        let az_deg = iaz as f32 * 360.0 / naz as f32;
        cut.radials.push(Radial {
            azimuth_deg: az_deg,
            elevation_deg: elevation_deg as f32,
            time_offset_ms: 0,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: Some(config.nyquist_mps),
            radial_status: None,
        });
        ref_values.extend(ref_row);
        vel_values.extend(vel_row);
    }

    let radial_indices: Vec<usize> = (0..naz).collect();
    cut.moments.insert(
        MomentType::Reflectivity,
        f32_grid(
            MomentType::Reflectivity,
            gate_range.clone(),
            radial_indices.clone(),
            ref_values,
        ),
    );
    cut.moments.insert(
        MomentType::Velocity,
        f32_grid(MomentType::Velocity, gate_range, radial_indices, vel_values),
    );
    cut
}

fn f32_grid(
    moment: MomentType,
    gate_range: GateRange,
    radial_indices: Vec<usize>,
    values: Vec<f32>,
) -> MomentGrid {
    // True physical units: dBZ / m·s⁻¹ stored directly (scale 1, offset 0), so
    // the render/dealias/GBVTD F32 paths and the standard colour tables read
    // them without a raw→scaled conversion.
    MomentGrid {
        moment,
        gate_range,
        scale: 1.0,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        radial_indices,
        storage: MomentStorage::F32(values),
    }
}

/// One trilinearly-sampled model column value at a gate.
struct ColumnSample {
    dbz: f32,
    u: f32,
    v: f32,
    w: f32,
}

/// Sample the 3-D model fields at (lat, lon, MSL height) by horizontal 2×2
/// bilinear weights (over the curvilinear WRF grid) combined with a
/// per-corner vertical bracket in MSL height — i.e. trilinear. Returns `None`
/// off the domain or when the height sits below terrain / above the model top
/// at every contributing corner.
fn sample_column(
    fields: &WrfRadarFields,
    cells: usize,
    lat: f32,
    lon: f32,
    z_msl: f32,
) -> Option<ColumnSample> {
    let stencil = horizontal_stencil(fields, lat, lon)?;

    let mut wsum = 0.0f32;
    let mut dbz = 0.0f32;
    let mut u = 0.0f32;
    let mut v = 0.0f32;
    let mut w = 0.0f32;
    for (col, weight) in stencil {
        if weight <= 0.0 {
            continue;
        }
        let Some((k, t)) = bracket_height(fields, cells, col, z_msl) else {
            continue;
        };
        let i0 = k * cells + col;
        let i1 = (k + 1) * cells + col;
        let Some(d) = lerp(fields.dbz[i0], fields.dbz[i1], t) else {
            continue;
        };
        let (Some(su), Some(sv), Some(sw)) = (
            lerp(fields.u[i0], fields.u[i1], t),
            lerp(fields.v[i0], fields.v[i1], t),
            lerp(fields.w[i0], fields.w[i1], t),
        ) else {
            continue;
        };
        wsum += weight;
        dbz += weight * d;
        u += weight * su;
        v += weight * sv;
        w += weight * sw;
    }
    if wsum <= 0.0 {
        return None;
    }
    Some(ColumnSample {
        dbz: dbz / wsum,
        u: u / wsum,
        v: v / wsum,
        w: w / wsum,
    })
}

/// Up to four `(column index, horizontal weight)` pairs for the WRF cell
/// containing (lat, lon), via the inverse LUT + a 2×2 bilinear solve. Falls
/// back to nearest-neighbour when the point is not cleanly inside a cell.
fn horizontal_stencil(fields: &WrfRadarFields, lat: f32, lon: f32) -> Option<[(usize, f32); 4]> {
    let nx = fields.nx;
    let ny = fields.ny;
    let nearest = fields.lut.lookup(lat, lon)?;
    if nx < 2 || ny < 2 {
        return Some([
            (nearest, 1.0),
            (nearest, 0.0),
            (nearest, 0.0),
            (nearest, 0.0),
        ]);
    }
    let row = nearest / nx;
    let col = nearest % nx;
    let target_lon = f64::from(lon);
    let target_lat = f64::from(lat);
    for y0 in neighboring_cell_starts(row, ny).into_iter().flatten() {
        for x0 in neighboring_cell_starts(col, nx).into_iter().flatten() {
            let i00 = y0 * nx + x0;
            let i10 = i00 + 1;
            let i01 = i00 + nx;
            let i11 = i01 + 1;
            let corners = [
                (
                    unwrap_lon_near(f64::from(fields.lon[i00]), target_lon),
                    f64::from(fields.lat[i00]),
                ),
                (
                    unwrap_lon_near(f64::from(fields.lon[i10]), target_lon),
                    f64::from(fields.lat[i10]),
                ),
                (
                    unwrap_lon_near(f64::from(fields.lon[i01]), target_lon),
                    f64::from(fields.lat[i01]),
                ),
                (
                    unwrap_lon_near(f64::from(fields.lon[i11]), target_lon),
                    f64::from(fields.lat[i11]),
                ),
            ];
            let Some((uu, vv)) = solve_bilinear_coords(corners, target_lon, target_lat) else {
                continue;
            };
            if !((-0.02..=1.02).contains(&uu) && (-0.02..=1.02).contains(&vv)) {
                continue;
            }
            let uu = uu.clamp(0.0, 1.0) as f32;
            let vv = vv.clamp(0.0, 1.0) as f32;
            return Some([
                (i00, (1.0 - uu) * (1.0 - vv)),
                (i10, uu * (1.0 - vv)),
                (i01, (1.0 - uu) * vv),
                (i11, uu * vv),
            ]);
        }
    }
    Some([
        (nearest, 1.0),
        (nearest, 0.0),
        (nearest, 0.0),
        (nearest, 0.0),
    ])
}

/// Bracket a target MSL height in a WRF column (height increases with model
/// level index k). Returns the lower level and the linear weight, or `None`
/// when the target is below the lowest level (below terrain) or above the top.
fn bracket_height(
    fields: &WrfRadarFields,
    cells: usize,
    col: usize,
    z: f32,
) -> Option<(usize, f32)> {
    let nz = fields.nz;
    let h0 = fields.height_msl[col];
    let htop = fields.height_msl[(nz - 1) * cells + col];
    if !h0.is_finite() || !htop.is_finite() || z < h0 || z > htop {
        return None;
    }
    for k in 0..nz - 1 {
        let hk = fields.height_msl[k * cells + col];
        let hk1 = fields.height_msl[(k + 1) * cells + col];
        if !hk.is_finite() || !hk1.is_finite() || hk1 <= hk {
            continue;
        }
        if z >= hk && z <= hk1 {
            return Some((k, (z - hk) / (hk1 - hk)));
        }
    }
    None
}

fn lerp(a: f32, b: f32, t: f32) -> Option<f32> {
    (a.is_finite() && b.is_finite()).then_some(a + t * (b - a))
}

/// Parse a WRF `Times` string ("YYYY-MM-DD_HH:MM:SS") to a UTC scan time.
fn parse_wrf_time(raw: &str) -> Option<DateTime<Utc>> {
    let cleaned = raw.trim().replace('_', " ");
    NaiveDateTime::parse_from_str(&cleaned, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

// ── Background job: WRF file(s) → Vec<Arc<RadarVolume>> ───────────────────────

#[derive(Debug)]
pub enum SyntheticRadarMessage {
    Progress(String),
    Done(Result<SyntheticRadarOutput, String>),
}

/// Result of a finished synthetic-radar job: one volume per WRF forecast time,
/// ready to feed the loop engine as a looping sequence.
pub struct SyntheticRadarOutput {
    pub label: String,
    pub volumes: Vec<Arc<RadarVolume>>,
    pub notes: Vec<String>,
}

impl std::fmt::Debug for SyntheticRadarOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntheticRadarOutput")
            .field("label", &self.label)
            .field("volumes", &self.volumes.len())
            .field("notes", &self.notes)
            .finish()
    }
}

pub struct SyntheticRadarTask {
    pub label: String,
    pub rx: Receiver<SyntheticRadarMessage>,
}

/// Spawn a worker that turns each forecast time of the given wrfout file(s)
/// into a simulated [`RadarVolume`]. Streams progress, then a `Done`.
pub fn spawn_synthetic_radar(
    paths: Vec<PathBuf>,
    config: SyntheticRadarConfig,
) -> SyntheticRadarTask {
    let label = if paths.len() == 1 {
        format!("Simulated radar from {}", display_name(&paths[0]))
    } else {
        format!("Simulated radar from {} WRF files", paths.len())
    };
    let (tx, rx) = channel();
    let label_for_thread = label.clone();
    std::thread::Builder::new()
        .name("rw-ui-wrf-synth-radar".to_string())
        .spawn(move || {
            let result = build_synthetic_from_paths(&paths, &config, &label_for_thread, &tx);
            let _ = tx.send(SyntheticRadarMessage::Done(result));
        })
        .expect("spawn WRF synthetic-radar worker");
    SyntheticRadarTask { label, rx }
}

fn build_synthetic_from_paths(
    paths: &[PathBuf],
    config: &SyntheticRadarConfig,
    label: &str,
    tx: &Sender<SyntheticRadarMessage>,
) -> Result<SyntheticRadarOutput, String> {
    if paths.is_empty() {
        return Err("No WRF files selected".to_string());
    }
    let mut files: Vec<PathBuf> = paths
        .iter()
        .filter(|path| crate::wrf_process::is_supported_wrf_file(path))
        .cloned()
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.is_empty() {
        return Err("No supported WRF files selected".to_string());
    }

    let mut volumes = Vec::new();
    let mut notes = Vec::new();
    let mut fallback_index = 0u32;
    for path in &files {
        let _ = tx.send(SyntheticRadarMessage::Progress(format!(
            "Opening WRF {}",
            display_name(path)
        )));
        let file = match WrfFile::open(path) {
            Ok(file) => file,
            Err(err) => {
                notes.push(format!("Open {} failed: {err}", display_name(path)));
                continue;
            }
        };
        let times = file.times().unwrap_or_default();
        let name = display_name(path);
        let nt = file.nt;
        for timeidx in 0..nt {
            // Stream fine-grained stage labels for this frame so the UI shows
            // steady progress instead of a multi-second (or, in a debug build,
            // multi-minute) freeze with no feedback.
            let frame_prefix = if nt > 1 {
                format!("Simulating {name} (time {}/{nt}): ", timeidx + 1)
            } else {
                format!("Simulating {name}: ")
            };
            let progress = |stage: &str| {
                let _ = tx.send(SyntheticRadarMessage::Progress(format!(
                    "{frame_prefix}{stage}"
                )));
            };
            progress("reading…");
            let fields = match read_wrf_radar_fields_reporting(
                &file,
                timeidx,
                config.prefer_refl_10cm,
                &progress,
            ) {
                Ok(fields) => fields,
                Err(err) => {
                    notes.push(format!("{name} time {timeidx}: {err}"));
                    continue;
                }
            };
            let valid_time = times
                .get(timeidx)
                .and_then(|raw| parse_wrf_time(raw))
                .unwrap_or_else(|| {
                    // No parsable Times entry — keep frames distinct so the
                    // loop engine does not collapse them into one identity.
                    let base = DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is valid")
                        + chrono::Duration::hours(i64::from(fallback_index));
                    fallback_index += 1;
                    base
                });
            let volume = build_synthetic_volume_reporting(&fields, valid_time, config, &progress);
            notes.push(format!(
                "{name} time {timeidx}: {} radials from {}",
                volume.metadata.decoded_radial_count,
                fields.ref_source
            ));
            volumes.push(Arc::new(volume));
        }
    }

    if volumes.is_empty() {
        return Err(if notes.is_empty() {
            "WRF produced no simulated radar volumes".to_string()
        } else {
            format!(
                "WRF produced no simulated radar volumes: {}",
                notes.join("; ")
            )
        });
    }
    Ok(SyntheticRadarOutput {
        label: label.to_string(),
        volumes,
        notes,
    })
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrf_times_with_colon_or_underscore() {
        let expected = "2026-05-19T00:00:00+00:00";
        assert_eq!(
            parse_wrf_time("2026-05-19_00:00:00").unwrap().to_rfc3339(),
            expected
        );
        assert_eq!(
            parse_wrf_time(" 2026-05-19_00:00:00 ")
                .unwrap()
                .to_rfc3339(),
            expected
        );
        assert!(parse_wrf_time("not-a-time").is_none());
    }

    #[test]
    fn radial_velocity_projection_signs_are_physical() {
        // Beam pointing due east (az=90°) at 0° elevation: a pure eastward wind
        // (u>0, v=0) blows AWAY from the radar → positive Vr; a westward wind →
        // negative. Verifies the (sinAz·cosEl, cosAz·cosEl, sinEl) projection.
        let az_rad: f32 = 90f32.to_radians();
        let (sin_az, cos_az) = (az_rad.sin(), az_rad.cos());
        let (u, v, w) = (12.0f32, 0.0, 0.0);
        let vr = u * sin_az * 1.0 + v * cos_az * 1.0 + w * 0.0;
        assert!(
            (vr - 12.0).abs() < 1e-3,
            "east wind due-east beam Vr = {vr}"
        );

        // Straight-up beam (el=90°) sees only w.
        let el_rad: f32 = 90f32.to_radians();
        let vr_up = 0.0 * 0.0 + 0.0 * 0.0 + 3.5 * el_rad.sin();
        assert!((vr_up - 3.5).abs() < 1e-3, "vertical beam Vr = {vr_up}");
    }

    /// A tiny synthetic 2×2×2 model verifies the whole sampling chain end to
    /// end without a wrfout: uniform 40 dBZ column, uniform 10 m/s east wind,
    /// radar at the box centre. Every in-domain, in-height gate must read
    /// 40 dBZ, and a due-east 0°-tilt gate near the ground must read ~+10 Vr.
    #[test]
    fn synthetic_box_model_samples_ref_and_velocity() {
        let nx = 2;
        let ny = 2;
        let nz = 2;
        let cells = nx * ny;
        // Grid centred near (39, -95) with ~0.2° spacing.
        let lat = vec![38.9f32, 38.9, 39.1, 39.1];
        let lon = vec![-95.1f32, -94.9, -95.1, -94.9];
        let height_msl = {
            let mut h = vec![0.0f32; nz * cells];
            for c in 0..cells {
                h[c] = 100.0; // level 0 ~100 m MSL
                h[cells + c] = 8000.0; // level 1 ~8 km MSL
            }
            h
        };
        let dbz = vec![40.0f32; nz * cells];
        let u = vec![10.0f32; nz * cells];
        let v = vec![0.0f32; nz * cells];
        let w = vec![0.0f32; nz * cells];
        let terrain_m = vec![0.0f32; cells];
        let lut = InverseLut::build_with_shape(&lat, &lon, nx, ny).expect("lut");
        let fields = WrfRadarFields {
            nx,
            ny,
            nz,
            lat,
            lon,
            height_msl,
            dbz,
            u,
            v,
            w,
            terrain_m,
            ref_source: "test",
            lut,
        };

        let config = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5],
            azimuth_count: 360,
            gate_spacing_m: 250.0,
            max_range_m: 10_000.0,
            ..SyntheticRadarConfig::default()
        };
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let volume = build_synthetic_volume(&fields, time, &config);

        assert_eq!(volume.cuts.len(), 1);
        let cut = &volume.cuts[0];
        let ref_grid = &cut.moments[&MomentType::Reflectivity];
        let vel_grid = &cut.moments[&MomentType::Velocity];

        // Interior gates read the uniform 40 dBZ.
        let mut finite_ref = 0;
        if let MomentStorage::F32(values) = &ref_grid.storage {
            for value in values {
                if value.is_finite() {
                    finite_ref += 1;
                    assert!((value - 40.0).abs() < 0.5, "ref {value}");
                }
            }
        }
        assert!(
            finite_ref > 100,
            "expected many finite REF gates, got {finite_ref}"
        );

        // Radial nearest az=90° (due east): near-ground gates blow away from
        // the radar at ~+10 m/s.
        let east_radial = (90 * 360 / 360) as usize; // az index for 90°
        let vel = vel_grid
            .scaled_value(east_radial, 4)
            .expect("east radial near gate");
        assert!((vel - 10.0).abs() < 1.5, "due-east Vr = {vel}");

        // West radial (az=270°) is the mirror image: toward the radar.
        let west_vel = vel_grid.scaled_value(270, 4).expect("west radial");
        assert!((west_vel + 10.0).abs() < 1.5, "due-west Vr = {west_vel}");
    }

    /// Real-data verification (project rule: prove on REAL data). Gated on
    /// `BOWECHO_WRF_RADAR_FIXTURE=<wrfout path>`; when set, builds a synthetic
    /// volume from the real file, renders the lowest tilt to a PNG for eyeball
    /// review, and asserts the reflectivity CO-LOCATES with the model's own
    /// column-max reflectivity (a georef proof) and lands in a physical dBZ
    /// band. Set `BOWECHO_WRF_RADAR_PNG=<dir>` to choose the PNG output dir.
    #[test]
    fn real_wrfout_builds_and_colocates_with_model_composite() {
        let Some(path) = std::env::var_os("BOWECHO_WRF_RADAR_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let file = WrfFile::open(&path).expect("open real wrfout");
        let config = SyntheticRadarConfig {
            azimuth_count: 720,
            ..SyntheticRadarConfig::default()
        };
        let fields = read_wrf_radar_fields(&file, 0, config.prefer_refl_10cm)
            .expect("read WRF radar fields");
        eprintln!(
            "reflectivity source: {}  grid {}x{}x{}",
            fields.ref_source, fields.nx, fields.ny, fields.nz
        );

        // Model column-max reflectivity (composite) + its argmax cell.
        let cells = fields.cells();
        let mut composite = vec![f32::NEG_INFINITY; cells];
        for k in 0..fields.nz {
            for (c, comp) in composite.iter_mut().enumerate() {
                let value = fields.dbz[k * cells + c];
                if value.is_finite() && value > *comp {
                    *comp = value;
                }
            }
        }
        let (argmax_cell, &model_max) = composite
            .iter()
            .enumerate()
            .filter(|(_, value)| value.is_finite())
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("finite composite max");
        let model_max_lat = fields.lat[argmax_cell];
        let model_max_lon = fields.lon[argmax_cell];
        eprintln!(
            "model composite max {model_max:.1} dBZ at lat {model_max_lat:.3} lon {model_max_lon:.3}"
        );
        assert!(
            (5.0..=90.0).contains(&model_max),
            "model composite max {model_max} dBZ is non-physical"
        );

        let time = file
            .times()
            .ok()
            .and_then(|times| times.first().and_then(|raw| parse_wrf_time(raw)))
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        let volume = build_synthetic_volume(&fields, time, &config);
        assert_eq!(volume.cuts.len(), config.elevations_deg.len());

        // The synthetic scan must carry real echo, in a physical band, and its
        // strongest gates must sit near the model composite maximum — the
        // georeferencing proof (beam geometry + AEQD placement + sampling).
        let site_lat = f64::from(volume.site.latitude_deg.unwrap());
        let site_lon = f64::from(volume.site.longitude_deg.unwrap());
        let mut finite_gates = 0usize;
        let mut synth_max = f32::NEG_INFINITY;
        let mut best_dist_km = f64::INFINITY;
        for cut in &volume.cuts {
            let grid = &cut.moments[&MomentType::Reflectivity];
            let MomentStorage::F32(values) = &grid.storage else {
                panic!("REF must be F32");
            };
            let gate_count = grid.gate_range.gate_count;
            let spacing = grid.gate_range.gate_spacing_m as f64;
            for (row, radial) in cut.radials.iter().enumerate() {
                let az_rad = f64::from(radial.azimuth_deg).to_radians();
                for gate in 0..gate_count {
                    let value = values[row * gate_count + gate];
                    if !value.is_finite() {
                        continue;
                    }
                    finite_gates += 1;
                    synth_max = synth_max.max(value);
                    // Only the strongest gates (within 6 dBZ of the model peak)
                    // are required to co-locate; find the closest to the model
                    // composite argmax.
                    if value >= model_max - 6.0 {
                        let ground = beam_ground_range_m(
                            gate as f64 * spacing,
                            f64::from(radial.elevation_deg),
                        );
                        let east_km = ground * az_rad.sin() / 1000.0;
                        let north_km = ground * az_rad.cos() / 1000.0;
                        let (glat, glon) = aeqd_inverse_km(site_lat, site_lon, east_km, north_km);
                        let dist = haversine_km(
                            glat,
                            glon,
                            f64::from(model_max_lat),
                            f64::from(model_max_lon),
                        );
                        best_dist_km = best_dist_km.min(dist);
                    }
                }
            }
        }
        eprintln!(
            "synthetic: {finite_gates} finite REF gates, max {synth_max:.1} dBZ, \
             nearest strong gate {best_dist_km:.2} km from model composite peak"
        );
        assert!(finite_gates > 1000, "too few echo gates: {finite_gates}");
        assert!(
            (5.0..=90.0).contains(&synth_max),
            "synthetic max {synth_max} dBZ non-physical"
        );
        assert!(
            synth_max >= model_max - 6.0,
            "synthetic peak {synth_max} far below model composite {model_max}"
        );
        // Strong echo within a few grid cells of the model peak proves the
        // geometry + georeferencing are right (WRF grid ~1 km here).
        assert!(
            best_dist_km <= 8.0,
            "strongest synthetic echo is {best_dist_km:.1} km from the model \
             composite peak — georeferencing is off"
        );

        // Render the lowest tilt to a PNG for the mandatory eyeball check.
        let out_dir = std::env::var_os("BOWECHO_WRF_RADAR_PNG")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&out_dir);
        let ref_png = out_dir.join("wrf_synth_ref.png");
        render2d::render_moment_png(
            &volume,
            0,
            MomentType::Reflectivity,
            &ref_png,
            render2d::RasterOptions::default(),
        )
        .expect("render synthetic REF PNG");
        let vel_png = out_dir.join("wrf_synth_vel.png");
        render2d::render_moment_png(
            &volume,
            0,
            MomentType::Velocity,
            &vel_png,
            render2d::RasterOptions::default(),
        )
        .expect("render synthetic VEL PNG");
        eprintln!("wrote {} and {}", ref_png.display(), vel_png.display());
    }

    fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        let r = 6371.0;
        let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
        let dphi = (lat2 - lat1).to_radians();
        let dlam = (lon2 - lon1).to_radians();
        let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlam / 2.0).sin().powi(2);
        2.0 * r * a.sqrt().asin()
    }

    /// Wall-time profile of the REAL synthetic-radar path (read fields + build
    /// volume) on a real wrfout. Gated on `BOWECHO_WRF_RADAR_FIXTURE`. Prints
    /// per-stage timing so we can find/verify the bottleneck. Run with:
    /// `cargo test -p app_ui --release profile_real_wrfout -- --nocapture`.
    #[test]
    fn profile_real_wrfout() {
        use std::time::Instant;
        let Some(path) = std::env::var_os("BOWECHO_WRF_RADAR_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let t0 = Instant::now();
        let file = WrfFile::open(&path).expect("open real wrfout");
        eprintln!("[prof] open {:.2}s  dims {}x{}x{} nt={}", t0.elapsed().as_secs_f64(), file.nx, file.ny, file.nz, file.nt);
        let config = SyntheticRadarConfig::default();

        let tr = Instant::now();
        let fields = read_wrf_radar_fields(&file, 0, config.prefer_refl_10cm).expect("read fields");
        eprintln!("[prof] read_wrf_radar_fields {:.2}s  refl_source={}", tr.elapsed().as_secs_f64(), fields.ref_source);

        let time = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let tb = Instant::now();
        let volume = build_synthetic_volume(&fields, time, &config);
        eprintln!("[prof] build_synthetic_volume {:.2}s  cuts={} radials={}",
            tb.elapsed().as_secs_f64(), volume.cuts.len(), volume.metadata.decoded_radial_count);
        eprintln!("[prof] TOTAL {:.2}s", t0.elapsed().as_secs_f64());
    }

    /// The parallelized read must return BYTE-IDENTICAL fields to the original
    /// serial read (this is a speed change, not an accuracy change). Reads the
    /// four heavy fields both ways in one process and asserts every value
    /// matches bit-for-bit (NaN patterns included). Gated on the same fixture.
    #[test]
    fn parallel_read_matches_sequential_fields() {
        let Some(path) = std::env::var_os("BOWECHO_WRF_RADAR_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let file = WrfFile::open(&path).expect("open real wrfout");
        let nz = file.nz;
        let cells = file.nx * file.ny;

        // Original serial read logic (verbatim from before the parallelization).
        let seq = {
            let height = read_3d(&file, "height", 0, nz * cells).unwrap();
            let (dbz, _src) = read_reflectivity(&file, 0, nz * cells, true).unwrap();
            let (u, v) = match getvar(&file, "uvmet", Some(0), &ComputeOpts::default()) {
                Ok(uvmet) if uvmet.data.len() == 2 * nz * cells => {
                    let (ue, ve) = uvmet.data.split_at(nz * cells);
                    (to_f32(ue), to_f32(ve))
                }
                _ => {
                    let ua = read_3d(&file, "ua", 0, nz * cells).unwrap();
                    let va = read_3d(&file, "va", 0, nz * cells).unwrap();
                    (ua, va)
                }
            };
            let w = read_3d(&file, "wa", 0, nz * cells).unwrap();
            (height, dbz, u, v, w)
        };

        let par = read_wrf_radar_fields(&file, 0, true).unwrap();

        // Bit-identical comparison (compare raw bits so NaNs must match too).
        let same = |a: &[f32], b: &[f32]| -> bool {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
        };
        assert!(same(&seq.0, &par.height_msl), "height differs");
        assert!(same(&seq.1, &par.dbz), "dbz differs");
        assert!(same(&seq.2, &par.u), "u differs");
        assert!(same(&seq.3, &par.v), "v differs");
        assert!(same(&seq.4, &par.w), "w differs");
        eprintln!(
            "[equiv] parallel read == serial read: {} elems x 5 fields bit-identical",
            par.dbz.len()
        );
    }
}
