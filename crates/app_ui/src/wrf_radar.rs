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

/// Optional sub-0.5° tilt (deg) prepended below [`DEFAULT_ELEVATIONS_DEG`] when
/// the user opts in. The community wrf-python/GR2 exports start their ladder
/// here; the 0.1° beam samples roughly half the height of our standard 0.5°
/// lowest tilt at range, so a hook echo is better defined near the ground.
pub const LOW_TILT_DEG: f64 = 0.1;

/// Build the elevation ladder for a synthetic scan. With `include_low_tilt`
/// the [`LOW_TILT_DEG`] tilt is prepended below the standard
/// [`DEFAULT_ELEVATIONS_DEG`] ladder; otherwise the standard ladder is
/// returned unchanged (bit-identical to the historical default).
pub fn elevation_ladder(include_low_tilt: bool) -> Vec<f64> {
    if include_low_tilt {
        std::iter::once(LOW_TILT_DEG)
            .chain(DEFAULT_ELEVATIONS_DEG.iter().copied())
            .collect()
    } else {
        DEFAULT_ELEVATIONS_DEG.to_vec()
    }
}

/// Reflectivity source label stamped when the model's own Thompson `REFL_10CM`
/// field is used directly.
pub const REFL_10CM_SOURCE: &str = "REFL_10CM";
/// Reflectivity source label stamped when the model-native operator falls back
/// to the computed `dbz` because the file carries no `REFL_10CM`.
pub const CALCDBZ_SOURCE: &str = "dbz/CALCDBZ";
/// Reflectivity source label stamped when the classic Stoelinga operator is
/// chosen, forcing the computed `dbz` (CALCDBZ) even when the file carries
/// `REFL_10CM`.
pub const STOELINGA_SOURCE: &str = "dbz/CALCDBZ (Stoelinga)";

/// Which reflectivity operator a synthetic scan uses. Both diagnostics are
/// legitimate: `REFL_10CM` is the model's own Thompson-native 10-cm
/// reflectivity (hotter/fatter in graupel/big-drop/melting regions of a
/// supercell), while the classic Stoelinga `dbz` (wrf-core `CALCDBZ`,
/// fixed Marshall-Palmer intercepts) is what the community wrf-python/GR2
/// pipeline renders. The choice is a per-import operator difference of roughly
/// +10..+20 dB in those regions — not a rendering artifact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectivityOperator {
    /// Prefer the model's own `REFL_10CM` when the file carries it, else fall
    /// back to the computed `dbz` (`CALCDBZ`). The historical default.
    #[default]
    ModelNative,
    /// Always compute `dbz` (wrf-core `CALCDBZ`, Stoelinga 2005 fixed
    /// intercepts) — the community wrf-python/GR2 look — even when the file
    /// carries `REFL_10CM`.
    ClassicStoelinga,
}

impl ReflectivityOperator {
    /// Whether to prefer the model's own `REFL_10CM` when the file carries it.
    /// Only the model-native operator does; Stoelinga always recomputes.
    pub fn prefers_refl_10cm(self) -> bool {
        matches!(self, Self::ModelNative)
    }

    /// The source label stamped when this operator uses the computed `dbz`
    /// (`CALCDBZ`) path — distinguishing a deliberate Stoelinga choice from a
    /// model-native fallback.
    pub fn computed_dbz_source(self) -> &'static str {
        match self {
            Self::ModelNative => CALCDBZ_SOURCE,
            Self::ClassicStoelinga => STOELINGA_SOURCE,
        }
    }
}

/// The reflectivity source label a synthetic scan will stamp, given the
/// operator choice and whether the file carries `REFL_10CM`. Assumes the
/// `REFL_10CM` read succeeds when attempted; a failed/short read falls through
/// to the computed `dbz`, mirrored exactly by [`read_reflectivity`]. This is
/// the pure decision the reflectivity read makes, factored out so both operator
/// branches are testable without a WRF file on disk.
pub fn planned_ref_source(
    operator: ReflectivityOperator,
    file_has_refl_10cm: bool,
) -> &'static str {
    if operator.prefers_refl_10cm() && file_has_refl_10cm {
        REFL_10CM_SOURCE
    } else {
        operator.computed_dbz_source()
    }
}

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
    /// Which reflectivity operator populates the sampled `dbz`: the model's own
    /// Thompson-native `REFL_10CM` ([`ReflectivityOperator::ModelNative`]) or the
    /// classic Stoelinga `CALCDBZ` community diagnostic
    /// ([`ReflectivityOperator::ClassicStoelinga`]).
    pub reflectivity_operator: ReflectivityOperator,
    /// Opt-in "gate texture": deterministic, range-correlated speckle added
    /// to the sampled moments so the synthetic gates read like real
    /// Level-II texture instead of a perfectly smooth trilinear field. OFF
    /// is the product default, and an OFF build is bit-identical to a build
    /// without the feature — the perturbation code never touches a value.
    pub gate_texture: bool,
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
            // Default operator. Flip this one line to ClassicStoelinga to
            // re-default the community look after the owner's comparison.
            reflectivity_operator: ReflectivityOperator::ModelNative,
            gate_texture: false,
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
/// Reflectivity: `REFL_10CM` (the model's own Thompson 10-cm reflectivity) when
/// present and the operator is [`ReflectivityOperator::ModelNative`], else
/// `wrf-core`'s `dbz` (`CALCDBZ`, Stoelinga 2005) — the same diagnostic
/// BowEcho's composite reflectivity uses, so a synthetic scan co-locates with
/// the model's own composite. [`ReflectivityOperator::ClassicStoelinga`] forces
/// the `CALCDBZ` path even when the file carries `REFL_10CM`. Raw wrfout carries
/// the hydrometeor mixing ratios `dbz` needs; a post-processed / climate wrfout
/// may not — this returns an `Err` (empty/absent reflectivity) so the caller can
/// warn rather than emit an all-NaN scan.
pub fn read_wrf_radar_fields(
    file: &WrfFile,
    timeidx: usize,
    operator: ReflectivityOperator,
) -> Result<WrfRadarFields, String> {
    read_wrf_radar_fields_reporting(file, timeidx, operator, &|_| {})
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
    operator: ReflectivityOperator,
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
        let th_refl = scope.spawn(|| read_reflectivity(file, timeidx, nz * cells, operator));
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
    // Domain-bounded: a WRF grid's row/col perimeter IS its true data edge,
    // so gates past that edge (but still inside the rectangular lat/lon
    // bbox) must sample nothing — without the bound, the LUT's hole-fill
    // dilation leaked nearest-edge values into a ~3-bin smeared ring around
    // the domain boundary.
    let lut = InverseLut::build_with_shape_domain_bounded(&lat_f32, &lon_f32, nx, ny)
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
    operator: ReflectivityOperator,
) -> Result<(Vec<f32>, &'static str), String> {
    // The same decision the tests assert through `planned_ref_source`: only the
    // model-native operator reads REFL_10CM, and only when the file carries it.
    if planned_ref_source(operator, file.has_var("REFL_10CM")) == REFL_10CM_SOURCE
        && let Ok(raw) = file.read_var("REFL_10CM", timeidx)
        && raw.len() == expected
    {
        return Ok((to_f32(&raw), REFL_10CM_SOURCE));
    }
    // wrf-core `dbz` = CALCDBZ (Stoelinga 2005), the same source BowEcho's
    // composite reflectivity uses. Constant intercepts / no bright-band
    // correction (ComputeOpts default) to match that composite exactly.
    // The classic-Stoelinga operator forces this path even when the file
    // carries REFL_10CM; the source label records which case applied.
    let dbz = read_3d(file, "dbz", timeidx, expected)
        .map_err(|err| format!("no REFL_10CM and computed dbz failed: {err}"))?;
    Ok((dbz, operator.computed_dbz_source()))
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
    let antenna_msl = config.antenna_msl_m.unwrap_or_else(|| {
        // Default antenna height: MODEL terrain under the ANTENNA plus a
        // short tower — a virtual site placed off-centre (explicit lat/lon
        // or a real NEXRAD id) must stand on its own ground, not the domain
        // centre's. A site outside the domain falls back to centre terrain.
        let site_cell = fields
            .lut
            .lookup(site_lat as f32, site_lon as f32)
            .unwrap_or(center);
        fields.terrain_m[site_cell] as f64 + DEFAULT_TOWER_M
    });

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
                // Opt-in gate texture: perturb BEFORE the floor test so echo
                // edges go ragged the way marginal-SNR gates do on a real
                // scope. When off, `dbz` is `sample.dbz` untouched — the
                // output stays bit-identical to a textureless build.
                let mut dbz = sample.dbz;
                if config.gate_texture {
                    dbz += ref_gate_texture_db(cut_index, iaz, gate);
                }
                if !dbz.is_finite() || dbz < floor {
                    continue;
                }
                ref_row[gate] = dbz;
                // Radial velocity: wind projected onto the beam unit vector
                // (east, north, up) = (sinAz·cosEl, cosAz·cosEl, sinEl), with
                // azimuth clockwise from north. Positive = away from the radar
                // (NEXRAD convention). Sun & Crook (1997).
                let vr = sample.u * (sin_az as f32) * (cos_el as f32)
                    + sample.v * (cos_az as f32) * (cos_el as f32)
                    + sample.w * (sin_el as f32);
                if vr.is_finite() {
                    vel_row[gate] = if config.gate_texture {
                        vr + vel_gate_texture_mps(cut_index, iaz, gate)
                    } else {
                        vr
                    };
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

// ── Gate texture (opt-in speckle) ─────────────────────────────────────────────
//
// Deliberately hash-based, NOT an RNG: every perturbation is a pure function
// of (tilt, azimuth, gate), so rebuilding a frame is bit-identical and a loop
// never shimmers between rebuilds of the same hour. Magnitudes are tuned
// against real Level-II: reflectivity texture on the order of ±2 dB,
// correlated over a few gates in range (the pulse volume smears neighbours)
// plus a smaller fully independent per-gate jitter; velocity gets only a
// gentle ±0.5 m/s wobble — pulse-pair Vr estimates are far smoother than
// reflectivity at decent SNR, and a noisy Vr would pollute the dealias/GBVTD
// consumers downstream.

/// Peak amplitude (dB) of the range-correlated reflectivity texture.
const REF_TEXTURE_CORRELATED_DB: f32 = 1.8;
/// Peak amplitude (dB) of the independent per-gate reflectivity jitter.
const REF_TEXTURE_JITTER_DB: f32 = 0.7;
/// Peak amplitude (m/s) of the (correlated) velocity wobble.
const VEL_TEXTURE_MPS: f32 = 0.5;
/// Range correlation length of the correlated components, in gates.
const TEXTURE_CORR_GATES: usize = 3;

const TEXTURE_SALT_REF: u32 = 0x5265_6631; // "Ref1"
const TEXTURE_SALT_REF_JITTER: u32 = 0x5265_664A; // "RefJ"
const TEXTURE_SALT_VEL: u32 = 0x5665_6C31; // "Vel1"

/// 32-bit avalanche mix (splitmix32 finalizer) — platform-independent.
fn texture_mix(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// Uniform noise in [-1, 1) from (tilt, azimuth, knot) + a salt.
fn texture_noise(tilt: usize, az: usize, knot: usize, salt: u32) -> f32 {
    let mixed = texture_mix(
        (tilt as u32).wrapping_mul(0x9e37_79b9)
            ^ (az as u32).wrapping_mul(0x85eb_ca6b)
            ^ (knot as u32).wrapping_mul(0xc2b2_ae35)
            ^ salt,
    );
    ((mixed >> 8) as f32) * (2.0 / 16_777_216.0) - 1.0
}

/// Range-correlated value noise in [-1, 1): hashed knots every
/// [`TEXTURE_CORR_GATES`] gates, linearly interpolated between them.
fn texture_correlated_noise(tilt: usize, az: usize, gate: usize, salt: u32) -> f32 {
    let knot = gate / TEXTURE_CORR_GATES;
    let t = (gate % TEXTURE_CORR_GATES) as f32 / TEXTURE_CORR_GATES as f32;
    let n0 = texture_noise(tilt, az, knot, salt);
    let n1 = texture_noise(tilt, az, knot + 1, salt);
    n0 + t * (n1 - n0)
}

/// Reflectivity gate-texture perturbation (dB), peak ±2.5 dB.
fn ref_gate_texture_db(tilt: usize, az: usize, gate: usize) -> f32 {
    REF_TEXTURE_CORRELATED_DB * texture_correlated_noise(tilt, az, gate, TEXTURE_SALT_REF)
        + REF_TEXTURE_JITTER_DB * texture_noise(tilt, az, gate, TEXTURE_SALT_REF_JITTER)
}

/// Velocity gate-texture perturbation (m/s), peak ±0.5 m/s.
fn vel_gate_texture_mps(tilt: usize, az: usize, gate: usize) -> f32 {
    VEL_TEXTURE_MPS * texture_correlated_noise(tilt, az, gate, TEXTURE_SALT_VEL)
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
    // Multi-select / folder picks arrive in arbitrary order: sort by the WRF
    // valid time parsed from the filename so the loop plays in model time.
    files.sort_by_cached_key(|path| wrf_time_sort_key(path));
    if files.is_empty() {
        return Err("No supported WRF files selected".to_string());
    }

    let mut volumes = Vec::new();
    let mut notes = Vec::new();
    let mut fallback_index = 0u32;
    let file_total = files.len();
    for (file_index, path) in files.iter().enumerate() {
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
            // multi-minute) freeze with no feedback. Multi-file loops lead
            // with "file 2/5: …" so the owner sees the loop building.
            let frame_prefix = match (file_total > 1, nt > 1) {
                (true, true) => format!(
                    "file {}/{file_total} ({name}, time {}/{nt}): ",
                    file_index + 1,
                    timeidx + 1
                ),
                (true, false) => format!("file {}/{file_total} ({name}): ", file_index + 1),
                (false, true) => format!("Simulating {name} (time {}/{nt}): ", timeidx + 1),
                (false, false) => format!("Simulating {name}: "),
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
                config.reflectivity_operator,
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
                volume.metadata.decoded_radial_count, fields.ref_source
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
    // The loop keys on the volume's own scan time: sort on it so frames play
    // in valid-time order even when a filename stamp and the file's internal
    // `Times` disagree (stable — equal times keep the filename order).
    volumes.sort_by_key(|volume| volume.volume_time);
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

/// Multi-file loop ordering key: the WRF valid time parsed from the filename
/// (`wrfout_d03_2025-06-21_01_30_00` → `"20250621013000"`), then the bare
/// filename as the tiebreak/fallback — so a shuffled multi-select or a folder
/// pick always builds (and reports "file k/n" progress) in model-time order.
/// Files without a parsable stamp sort together by name, ahead of nothing.
fn wrf_time_sort_key(path: &Path) -> (String, String) {
    let name = display_name(path);
    (
        crate::wrf_process::parse_wrf_timestamp(&name).unwrap_or_default(),
        name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_file_sort_key_orders_by_parsed_wrf_time() {
        // Shuffled pick order (what rfd multi-select hands back) must sort by
        // the model time embedded in the name, not the pick order. Paths are
        // built with join so the directory strips on every OS (a literal
        // `C:\run\...` is ONE component on Linux and broke the CI gate).
        let mut paths = [
            PathBuf::from("run").join("wrfout_d03_2025-06-21_02_15_00"),
            PathBuf::from("run").join("wrfout_d03_2025-06-21_01_30_00"),
            PathBuf::from("run").join("wrfout_d03_2025-06-21_02_30_00"),
            PathBuf::from("run").join("wrfout_d03_2025-06-21_02_00_00"),
            PathBuf::from("run").join("wrfout_d03_2025-06-21_01_45_00"),
        ];
        paths.sort_by_cached_key(|path| wrf_time_sort_key(path));
        let names: Vec<String> = paths.iter().map(|path| display_name(path)).collect();
        assert_eq!(
            names,
            vec![
                "wrfout_d03_2025-06-21_01_30_00",
                "wrfout_d03_2025-06-21_01_45_00",
                "wrfout_d03_2025-06-21_02_00_00",
                "wrfout_d03_2025-06-21_02_15_00",
                "wrfout_d03_2025-06-21_02_30_00",
            ]
        );

        // Colon-form stamps sort identically; unstamped names fall back to
        // filename order without panicking.
        let colon = wrf_time_sort_key(Path::new("wrfout_d02_2025-06-21_01:45:00"));
        let underscore = wrf_time_sort_key(Path::new("wrfout_d03_2025-06-21_01_45_00"));
        assert_eq!(colon.0, underscore.0);
        assert_eq!(colon.0, "20250621014500");
        let plain = wrf_time_sort_key(Path::new("some_model_output.nc"));
        assert!(plain.0.is_empty());
        assert_eq!(plain.1, "some_model_output.nc");
    }

    /// REAL-data proof for the multi-frame loop path (project rule: prove on
    /// real data). Gated on `BOWECHO_WRF_RADAR_MULTI_FIXTURE` = a `;`-joined
    /// list of real wrfout paths (e.g. the five Enderlin tornado files).
    /// Deliberately feeds the paths in REVERSED order, then asserts:
    ///  - one volume per file, in strictly ascending scan time;
    ///  - per-file progress streamed ("file 2/5 … building tilt …");
    ///  - the volumes install into the real loop engine as one batch, land in
    ///    time order, and the loop cursor advances through all frames and
    ///    wraps — the exact contract `install_synthetic_radar_volumes` relies
    ///    on. Run in RELEASE (`cargo test -p app_ui --release … -- --nocapture`).
    #[test]
    fn real_multi_file_loop_builds_time_ordered_frames_and_advances() {
        use ui_core::loop_engine::{
            DecodedLoad, DecodedLoadBatch, EngineId, EngineRole, FeedSource, FrameStatus,
            LoadTimings, LoopEngine, SelectionPolicy, StepOutcome, SweepContext,
        };

        let Some(raw) = std::env::var_os("BOWECHO_WRF_RADAR_MULTI_FIXTURE") else {
            return;
        };
        let raw = raw.to_string_lossy().into_owned();
        let mut paths: Vec<PathBuf> = raw
            .split(';')
            .filter(|part| !part.trim().is_empty())
            .map(PathBuf::from)
            .collect();
        assert!(
            paths.len() >= 2,
            "need at least two real wrfout paths, got {}",
            paths.len()
        );
        // Hand the worker the WRONG order on purpose: the sort must fix it.
        paths.reverse();
        let expected_frames = paths.len();

        let config = SyntheticRadarConfig::default();
        let (tx, rx) = channel();
        let output = build_synthetic_from_paths(&paths, &config, "multi-file test", &tx)
            .expect("build synthetic volumes from real multi-file fixture");
        drop(tx);

        // One frame per file, strictly ascending in scan time.
        assert_eq!(output.volumes.len(), expected_frames, "one volume per file");
        let times: Vec<DateTime<Utc>> = output
            .volumes
            .iter()
            .map(|volume| volume.volume_time)
            .collect();
        eprintln!("[multi] frame times: {times:?}");
        for pair in times.windows(2) {
            assert!(
                pair[0] < pair[1],
                "scan times must strictly ascend: {pair:?}"
            );
        }
        // Every frame carries real echo (these are tornado-hour files).
        for volume in &output.volumes {
            assert!(
                volume.metadata.decoded_radial_count > 0,
                "frame {} has no radials",
                volume.volume_time
            );
        }

        // Per-file progress streamed in sorted order.
        let progress: Vec<String> = std::iter::from_fn(|| match rx.try_recv() {
            Ok(SyntheticRadarMessage::Progress(message)) => Some(message),
            _ => None,
        })
        .collect();
        let marker = format!("file 2/{expected_frames}");
        assert!(
            progress
                .iter()
                .any(|message| message.contains(&marker) && message.contains("building tilt")),
            "expected a '{marker} … building tilt …' progress line, got: {progress:?}"
        );

        // Feed the whole Vec to the real loop engine exactly like
        // `install_synthetic_radar_volumes` does, then advance the loop.
        let mut engine = LoopEngine::new(
            EngineId(1),
            EngineRole::Primary,
            FeedSource::LocalFiles {
                label: "wrf-synth multi-file test".to_owned(),
            },
        );
        engine.limits.grow_to_fit = true;
        let frames: Vec<DecodedLoad> = output
            .volumes
            .iter()
            .enumerate()
            .map(|(index, volume)| {
                let stamp = volume.volume_time.format("%Y%m%d_%H%M%S").to_string();
                DecodedLoad {
                    path: PathBuf::from(format!(
                        "wrf-synth://{}/{index:04}_{stamp}",
                        volume.site.id
                    )),
                    volume: Arc::clone(volume),
                    timings: LoadTimings::default(),
                    status: FrameStatus::Local,
                    source_label: format!("simulated WRF {stamp}"),
                }
            })
            .collect();
        let outcome = engine.install_batch(
            DecodedLoadBatch {
                frames,
                selected_index: 0,
            },
            &SelectionPolicy::SelectAnchor {
                blank_display_overrides_browsing: true,
            },
            None,
            |_anchor| false,
        );
        assert!(
            outcome.cross_site_clear.is_none(),
            "one shared site id: the cross-site guard must not clear"
        );
        assert_eq!(engine.history.len(), expected_frames, "all frames install");
        let installed: Vec<DateTime<Utc>> = engine
            .history
            .iter()
            .map(|entry| entry.identity.scan_time_utc)
            .collect();
        assert_eq!(installed, times, "history holds the frames in time order");

        // The loop advances through every frame and wraps back to 0.
        engine.cursor.playing = true;
        let mut sequence = Vec::new();
        for _ in 0..expected_frames {
            match engine.advance_loop(&SweepContext::PLAIN) {
                StepOutcome::Stepped { index } => sequence.push(index),
                StepOutcome::Stopped => panic!("a populated loop must never stop"),
            }
        }
        let expected_sequence: Vec<usize> = (1..expected_frames).chain([0]).collect();
        assert_eq!(sequence, expected_sequence, "loop advances then wraps");
        assert!(engine.cursor.playing);
        eprintln!(
            "[multi] {} frames installed in time order; loop stepped {sequence:?}",
            engine.history.len()
        );
    }

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

    /// A tiny 2×2×2 uniform box model (40 dBZ everywhere, 10 m/s east wind,
    /// levels at ~100 m / ~8 km MSL) centred near (39, -95) — the smallest
    /// grid the whole sampling chain accepts.
    fn uniform_box_fields() -> WrfRadarFields {
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
        let lut = InverseLut::build_with_shape_domain_bounded(&lat, &lon, nx, ny).expect("lut");
        WrfRadarFields {
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
        }
    }

    /// A tiny synthetic 2×2×2 model verifies the whole sampling chain end to
    /// end without a wrfout: uniform 40 dBZ column, uniform 10 m/s east wind,
    /// radar at the box centre. Every in-domain, in-height gate must read
    /// 40 dBZ, and a due-east 0°-tilt gate near the ground must read ~+10 Vr.
    #[test]
    fn synthetic_box_model_samples_ref_and_velocity() {
        let fields = uniform_box_fields();

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

    /// All F32 moment values of a volume as raw bits (NaN patterns
    /// included), for exact equality comparisons.
    fn moment_bits(volume: &RadarVolume) -> Vec<u32> {
        let mut bits = Vec::new();
        for cut in &volume.cuts {
            for moment in [MomentType::Reflectivity, MomentType::Velocity] {
                let MomentStorage::F32(values) = &cut.moments[&moment].storage else {
                    panic!("synthetic moments must be F32");
                };
                bits.extend(values.iter().map(|value| value.to_bits()));
            }
        }
        bits
    }

    fn box_model_config() -> SyntheticRadarConfig {
        SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(200.0),
            elevations_deg: vec![0.5, 3.1],
            azimuth_count: 360,
            gate_spacing_m: 250.0,
            max_range_m: 10_000.0,
            ..SyntheticRadarConfig::default()
        }
    }

    /// The reflectivity-operator choice resolves to the right source label for
    /// each branch. `read_reflectivity` needs a real WRF file (WrfFile only
    /// opens from a path), so the operator→source decision it makes is factored
    /// into `planned_ref_source` and asserted here for both operators, with and
    /// without a file-carried REFL_10CM.
    #[test]
    fn reflectivity_operator_selects_the_expected_source() {
        use ReflectivityOperator::{ClassicStoelinga, ModelNative};

        // Model native: prefers REFL_10CM when present, falls back to CALCDBZ.
        assert_eq!(planned_ref_source(ModelNative, true), REFL_10CM_SOURCE);
        assert_eq!(planned_ref_source(ModelNative, false), CALCDBZ_SOURCE);

        // Classic Stoelinga forces CALCDBZ EVEN when REFL_10CM is present, and
        // stamps a distinct label so a run documents the deliberate choice
        // rather than a fallback.
        assert_eq!(planned_ref_source(ClassicStoelinga, true), STOELINGA_SOURCE);
        assert_eq!(
            planned_ref_source(ClassicStoelinga, false),
            STOELINGA_SOURCE
        );

        // Only the model-native operator ever reads REFL_10CM.
        assert!(ModelNative.prefers_refl_10cm());
        assert!(!ClassicStoelinga.prefers_refl_10cm());

        // Default operator is model native (the historical behavior).
        assert_eq!(ReflectivityOperator::default(), ModelNative);
        assert_eq!(
            SyntheticRadarConfig::default().reflectivity_operator,
            ModelNative
        );

        // The three labels are distinct so the import note is unambiguous.
        assert_ne!(REFL_10CM_SOURCE, CALCDBZ_SOURCE);
        assert_ne!(CALCDBZ_SOURCE, STOELINGA_SOURCE);
    }

    /// The optional 0.1° low tilt is prepended below the standard ladder when
    /// opted in, and off by default it is bit-identical to the classic ladder.
    #[test]
    fn low_tilt_option_prepends_the_community_lowest_tilt() {
        let standard = elevation_ladder(false);
        assert_eq!(
            standard, DEFAULT_ELEVATIONS_DEG,
            "off = the classic ladder, unchanged"
        );

        let with_low = elevation_ladder(true);
        assert_eq!(
            with_low.len(),
            DEFAULT_ELEVATIONS_DEG.len() + 1,
            "one extra tilt"
        );
        assert_eq!(with_low[0], LOW_TILT_DEG, "0.1° comes first");
        assert!(
            with_low[0] < DEFAULT_ELEVATIONS_DEG[0],
            "the extra tilt is below the standard lowest tilt"
        );
        assert_eq!(
            &with_low[1..],
            DEFAULT_ELEVATIONS_DEG,
            "the standard ladder follows unchanged"
        );
    }

    /// The DEFAULT-OFF contract for the opt-in gate texture: the default
    /// config leaves it off, and an off build is bit-identical to an
    /// explicit `gate_texture: false` build — when disabled, the texture
    /// path must not touch a single output value.
    #[test]
    fn gate_texture_defaults_off_and_off_builds_are_bit_identical() {
        assert!(
            !SyntheticRadarConfig::default().gate_texture,
            "gate texture must be opt-in — the smooth look is the default"
        );
        let fields = uniform_box_fields();
        let config = box_model_config();
        assert!(!config.gate_texture);
        let off = SyntheticRadarConfig {
            gate_texture: false,
            ..config.clone()
        };
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let a = build_synthetic_volume(&fields, time, &config);
        let b = build_synthetic_volume(&fields, time, &off);
        assert!(
            moment_bits(&a) == moment_bits(&b),
            "gate_texture=false must be bit-identical to the default build"
        );
    }

    /// Texture ON: reproducible (hash-seeded, no RNG state — two builds are
    /// bit-identical), bounded (REF within ±2.5 dB of the smooth build, VEL
    /// within ±0.5 m/s), actually visible on REF, and gentle on velocity.
    #[test]
    fn gate_texture_is_deterministic_bounded_and_gentle_on_velocity() {
        let fields = uniform_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let off_config = box_model_config();
        let on_config = SyntheticRadarConfig {
            gate_texture: true,
            ..off_config.clone()
        };
        let off = build_synthetic_volume(&fields, time, &off_config);
        let on = build_synthetic_volume(&fields, time, &on_config);
        let on_again = build_synthetic_volume(&fields, time, &on_config);
        assert!(
            moment_bits(&on) == moment_bits(&on_again),
            "texture must be deterministic frame to frame"
        );

        let mut compared = 0usize;
        let mut ref_changed = 0usize;
        let mut max_ref_diff = 0.0f32;
        for (cut_off, cut_on) in off.cuts.iter().zip(&on.cuts) {
            for (moment, bound) in [
                (MomentType::Reflectivity, 2.51f32),
                (MomentType::Velocity, 0.51f32),
            ] {
                let MomentStorage::F32(a) = &cut_off.moments[&moment].storage else {
                    panic!("F32");
                };
                let MomentStorage::F32(b) = &cut_on.moments[&moment].storage else {
                    panic!("F32");
                };
                for (va, vb) in a.iter().zip(b) {
                    // Uniform 40 dBZ against a 0 dBZ floor: texture can
                    // never flip a gate across the floor here, so finite
                    // patterns must match exactly.
                    assert_eq!(va.is_finite(), vb.is_finite());
                    if !va.is_finite() {
                        continue;
                    }
                    let diff = (vb - va).abs();
                    assert!(diff <= bound, "{moment:?} perturbed by {diff}");
                    if moment == MomentType::Reflectivity {
                        compared += 1;
                        max_ref_diff = max_ref_diff.max(diff);
                        if diff > 0.05 {
                            ref_changed += 1;
                        }
                    }
                }
            }
        }
        assert!(compared > 10_000, "too few echo gates: {compared}");
        assert!(
            ref_changed * 2 > compared,
            "texture must actually move most REF gates ({ref_changed}/{compared})"
        );
        assert!(
            max_ref_diff > 1.5,
            "texture peak {max_ref_diff} dB is too tame to read as speckle"
        );
    }

    /// Texture is applied BEFORE the reflectivity floor, so echo edges go
    /// ragged like a marginal-SNR scope: with the floor parked just above
    /// the model's uniform 40 dBZ, the smooth build shows nothing while the
    /// textured build pokes gates through — all within the noise peak.
    #[test]
    fn gate_texture_pokes_ragged_gates_through_the_ref_floor() {
        let fields = uniform_box_fields();
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let off_config = SyntheticRadarConfig {
            ref_floor_dbz: 40.5,
            ..box_model_config()
        };
        let on_config = SyntheticRadarConfig {
            gate_texture: true,
            ..off_config.clone()
        };
        let count_finite = |volume: &RadarVolume| {
            moment_bits(volume)
                .iter()
                .filter(|bits| f32::from_bits(**bits).is_finite())
                .count()
        };
        let off = build_synthetic_volume(&fields, time, &off_config);
        assert_eq!(
            count_finite(&off),
            0,
            "smooth 40 dBZ under a 40.5 floor must stay empty"
        );
        let on = build_synthetic_volume(&fields, time, &on_config);
        assert!(
            count_finite(&on) > 0,
            "texture must poke some gates through the floor"
        );
        for cut in &on.cuts {
            let MomentStorage::F32(values) = &cut.moments[&MomentType::Reflectivity].storage else {
                panic!("F32");
            };
            for value in values.iter().filter(|value| value.is_finite()) {
                assert!(
                    (40.5..=42.6).contains(value),
                    "ragged-edge gate {value} outside the floor..floor+peak band"
                );
            }
        }
    }

    /// A rotated curvilinear domain (nz = 2, uniform 40 dBZ, 10 m/s east
    /// wind): its true edge is NOT the lat/lon bbox, which is exactly where
    /// the pre-fix LUT leaked nearest-edge values (the smeared boundary
    /// ring). `bounded` picks the fixed vs the old LUT build.
    fn rotated_domain_fields(
        n: usize,
        spacing: f32,
        theta_deg: f32,
        bounded: bool,
    ) -> WrfRadarFields {
        let c = (n as f32 - 1.0) / 2.0;
        let (sin_t, cos_t) = theta_deg.to_radians().sin_cos();
        let cells = n * n;
        let nz = 2;
        let mut lat = Vec::with_capacity(cells);
        let mut lon = Vec::with_capacity(cells);
        for j in 0..n {
            for i in 0..n {
                let x = (i as f32 - c) * spacing;
                let y = (j as f32 - c) * spacing;
                lon.push(-95.0 + x * cos_t - y * sin_t);
                lat.push(39.0 + x * sin_t + y * cos_t);
            }
        }
        let mut height_msl = vec![100.0f32; nz * cells];
        height_msl[cells..].fill(8000.0);
        let lut = if bounded {
            InverseLut::build_with_shape_domain_bounded(&lat, &lon, n, n).expect("bounded lut")
        } else {
            InverseLut::build_with_shape(&lat, &lon, n, n).expect("plain lut")
        };
        WrfRadarFields {
            nx: n,
            ny: n,
            nz,
            lat,
            lon,
            height_msl,
            dbz: vec![40.0f32; nz * cells],
            u: vec![10.0f32; nz * cells],
            v: vec![0.0f32; nz * cells],
            w: vec![0.0f32; nz * cells],
            terrain_m: vec![0.0f32; cells],
            ref_source: "test",
            lut,
        }
    }

    /// The domain-edge fix, end to end: on a rotated domain every finite
    /// gate must georeference to inside the true (rotated) domain edge —
    /// and the OLD, unbounded LUT demonstrably leaks a ring of finite gates
    /// past that edge on the same fixture, proving the assertion has teeth.
    #[test]
    fn synthetic_gates_stay_nan_outside_the_true_domain_edge() {
        let n = 41usize;
        let spacing = 0.02f32; // deg → half-width 0.4° (~44 km)
        let theta = 30.0f32;
        let config = SyntheticRadarConfig {
            site_lat_deg: Some(39.0),
            site_lon_deg: Some(-95.0),
            antenna_msl_m: Some(300.0),
            elevations_deg: vec![0.5],
            azimuth_count: 360,
            gate_spacing_m: 250.0,
            max_range_m: 120_000.0,
            ..SyntheticRadarConfig::default()
        };
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();

        let half = (n as f32 - 1.0) / 2.0 * spacing;
        // Allowed slack past the edge: one LUT bin (a bin the boundary
        // merely clips legitimately resolves) plus in-bin position.
        let bin = spacing * theta.to_radians().cos() * 1.25;
        let tol = 2.0 * bin;
        let (sin_t, cos_t) = theta.to_radians().sin_cos();

        // Worst finite-gate excursion in the domain's own (rotated) frame.
        let worst_extent = |volume: &RadarVolume| -> (f32, usize) {
            let mut worst = 0.0f32;
            let mut finite = 0usize;
            for cut in &volume.cuts {
                let grid = &cut.moments[&MomentType::Reflectivity];
                let MomentStorage::F32(values) = &grid.storage else {
                    panic!("F32");
                };
                let gate_count = grid.gate_range.gate_count;
                let spacing_m = f64::from(grid.gate_range.gate_spacing_m);
                for (row, radial) in cut.radials.iter().enumerate() {
                    let az_rad = f64::from(radial.azimuth_deg).to_radians();
                    for gate in 0..gate_count {
                        if !values[row * gate_count + gate].is_finite() {
                            continue;
                        }
                        finite += 1;
                        let ground = beam_ground_range_m(
                            gate as f64 * spacing_m,
                            f64::from(radial.elevation_deg),
                        );
                        let (glat, glon) = aeqd_inverse_km(
                            39.0,
                            -95.0,
                            ground * az_rad.sin() / 1000.0,
                            ground * az_rad.cos() / 1000.0,
                        );
                        let (dlat, dlon) = ((glat - 39.0) as f32, (glon + 95.0) as f32);
                        let x = dlon * cos_t + dlat * sin_t;
                        let y = -dlon * sin_t + dlat * cos_t;
                        worst = worst.max(x.abs().max(y.abs()));
                    }
                }
            }
            (worst, finite)
        };

        let fixed = build_synthetic_volume(
            &rotated_domain_fields(n, spacing, theta, true),
            time,
            &config,
        );
        let (worst, finite) = worst_extent(&fixed);
        assert!(
            finite > 10_000,
            "beam must cover the domain: {finite} gates"
        );
        assert!(
            worst <= half + tol,
            "finite gate {worst}° from centre exceeds the true edge ({})",
            half + tol
        );

        let leaky = build_synthetic_volume(
            &rotated_domain_fields(n, spacing, theta, false),
            time,
            &config,
        );
        let (worst_leak, _) = worst_extent(&leaky);
        assert!(
            worst_leak > half + tol,
            "the unbounded LUT should leak past the edge (got {worst_leak}°) — \
             otherwise this test can't catch the bug"
        );
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
        let fields = read_wrf_radar_fields(&file, 0, config.reflectivity_operator)
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

    /// Crop a square window centred on (cx, cy) out of a rendered PPI and
    /// save it (clamped to the image bounds).
    fn save_crop(image: &image::RgbaImage, cx: f32, cy: f32, window: u32, path: &Path) {
        let window = window.min(image.width()).min(image.height());
        let x0 = ((cx - window as f32 / 2.0).round().max(0.0) as u32).min(image.width() - window);
        let y0 = ((cy - window as f32 / 2.0).round().max(0.0) as u32).min(image.height() - window);
        image::imageops::crop_imm(image, x0, y0, window, window)
            .to_image()
            .save(path)
            .expect("save crop");
    }

    /// CLOSING PROOF for the gates-polish track (v0.29.3): ONE pass over a
    /// real Enderlin wrfout, run on the build node in release. Gated on
    /// `BOWECHO_GATES_PROBE_FIXTURE=<wrfout path>`; PNGs land in
    /// `BOWECHO_GATES_PROBE_PNG` (default: temp dir):
    ///  - `gates_default.png` (+`_zoom`) — lowest-tilt REF PPI, default
    ///    config: must look identical to the current shipped gates;
    ///  - `gates_speckle.png` (+`_zoom`) — the same PPI with the opt-in
    ///    `gate_texture` speckle;
    ///  - `edge_before_zoom.png` / `edge_after_zoom.png` — the WRF domain
    ///    edge with the old bbox-dilated LUT (smeared off-domain ring) vs
    ///    the domain-bounded LUT (clean NaN cutoff). The "before" swaps the
    ///    unbounded LUT into the same fields — test-only, never a product
    ///    path. Also counts finite gates georeferencing off-domain: must be
    ///    zero after the fix, nonzero before (the ring, quantified).
    #[test]
    fn gates_polish_probe_renders_proof_pngs() {
        let Some(path) = std::env::var_os("BOWECHO_GATES_PROBE_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        let out_dir = std::env::var_os("BOWECHO_GATES_PROBE_PNG")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&out_dir);

        let file = WrfFile::open(&path).expect("open real wrfout");
        let config = SyntheticRadarConfig::default();
        let mut fields = read_wrf_radar_fields(&file, 0, config.reflectivity_operator)
            .expect("read WRF radar fields");
        let time = file
            .times()
            .ok()
            .and_then(|times| times.first().and_then(|raw| parse_wrf_time(raw)))
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());

        let raster = render2d::RasterOptions {
            width: 4096,
            height: 4096,
            range_fraction: 100,
        };
        let centre_px = (raster.width as f32 - 1.0) / 2.0;
        let to_px = |east_m: f64, north_m: f64, range_m: f64| -> (f32, f32) {
            (
                centre_px + (east_m / range_m) as f32 * centre_px,
                centre_px - (north_m / range_m) as f32 * centre_px,
            )
        };

        // (a) Default mode — the shipped look, must be visually identical
        // to the current gates.
        let default_volume = build_synthetic_volume(&fields, time, &config);
        let default_img =
            render2d::render_moment_image(&default_volume, 0, MomentType::Reflectivity, raster)
                .expect("render default PPI");
        default_img
            .save(out_dir.join("gates_default.png"))
            .expect("save default PPI");

        // Strongest low-tilt echo — the zoom anchor for the texture crops.
        let cut = &default_volume.cuts[0];
        let grid = &cut.moments[&MomentType::Reflectivity];
        let MomentStorage::F32(values) = &grid.storage else {
            panic!("REF must be F32");
        };
        let gate_count = grid.gate_range.gate_count;
        let spacing_m = f64::from(grid.gate_range.gate_spacing_m);
        let mut best = (f32::NEG_INFINITY, 0usize, 0usize);
        for (row, _) in cut.radials.iter().enumerate() {
            for gate in 0..gate_count {
                let value = values[row * gate_count + gate];
                if value.is_finite() && value > best.0 {
                    best = (value, row, gate);
                }
            }
        }
        let az_rad = f64::from(cut.radials[best.1].azimuth_deg).to_radians();
        let ground = beam_ground_range_m(best.2 as f64 * spacing_m, f64::from(cut.elevation_deg));
        let (echo_x, echo_y) = to_px(
            ground * az_rad.sin(),
            ground * az_rad.cos(),
            config.max_range_m,
        );
        save_crop(
            &default_img,
            echo_x,
            echo_y,
            768,
            &out_dir.join("gates_default_zoom.png"),
        );

        // (b) Speckle mode.
        let speckle_config = SyntheticRadarConfig {
            gate_texture: true,
            ..config.clone()
        };
        let speckle_volume = build_synthetic_volume(&fields, time, &speckle_config);
        let speckle_img =
            render2d::render_moment_image(&speckle_volume, 0, MomentType::Reflectivity, raster)
                .expect("render speckle PPI");
        speckle_img
            .save(out_dir.join("gates_speckle.png"))
            .expect("save speckle PPI");
        save_crop(
            &speckle_img,
            echo_x,
            echo_y,
            768,
            &out_dir.join("gates_speckle_zoom.png"),
        );

        // (c) Domain-edge zoom, before/after. Aim at the closest domain-side
        // midpoint and scan just past it so the edge fills the frame.
        let site_lat = f64::from(default_volume.site.latitude_deg.unwrap());
        let site_lon = f64::from(default_volume.site.longitude_deg.unwrap());
        let (nx, ny) = (fields.nx, fields.ny);
        let side_mids = [
            (ny - 1) * nx + nx / 2, // north
            (ny / 2) * nx + nx - 1, // east
            nx / 2,                 // south
            (ny / 2) * nx,          // west
        ];
        let (edge_east_km, edge_north_km) = side_mids
            .iter()
            .map(|&cell| {
                ui_core::geo::aeqd_forward_km(
                    site_lat,
                    site_lon,
                    f64::from(fields.lat[cell]),
                    f64::from(fields.lon[cell]),
                )
            })
            .min_by(|a, b| a.0.hypot(a.1).total_cmp(&b.0.hypot(b.1)))
            .expect("four side midpoints");
        let edge_dist_m = edge_east_km.hypot(edge_north_km) * 1000.0;
        let edge_config = SyntheticRadarConfig {
            elevations_deg: vec![config.elevations_deg[0]],
            max_range_m: (edge_dist_m * 1.25).max(60_000.0),
            ..config.clone()
        };

        // Off-domain finite-gate counter (the bounded LUT is the truth);
        // also returns the strongest leaked gate's (east, north) metres so
        // the edge zoom frames the actual smear, not empty boundary.
        let bounded_lut = InverseLut::build_with_shape_domain_bounded(
            &fields.lat,
            &fields.lon,
            fields.nx,
            fields.ny,
        )
        .expect("bounded LUT");
        // Leaked gate: finite in the volume but off the true domain (bounded
        // LUT says None). Returns (azimuth row, east m, north m, dBZ) per
        // leak, exact — azimuths rebuilt in f64 from the radial index just
        // like build_cut, so each gate re-maps to the very lat/lon the
        // sampler used.
        let off_domain_finite = |volume: &RadarVolume| -> Vec<(usize, f64, f64, f32)> {
            let cut = &volume.cuts[0];
            let grid = &cut.moments[&MomentType::Reflectivity];
            let MomentStorage::F32(values) = &grid.storage else {
                panic!("REF must be F32");
            };
            let gate_count = grid.gate_range.gate_count;
            let spacing_m = f64::from(grid.gate_range.gate_spacing_m);
            let naz = cut.radials.len();
            let mut leaks = Vec::new();
            for (row, radial) in cut.radials.iter().enumerate() {
                let az_rad = (row as f64 * 360.0 / naz as f64).to_radians();
                for gate in 0..gate_count {
                    let value = values[row * gate_count + gate];
                    if !value.is_finite() {
                        continue;
                    }
                    let ground = beam_ground_range_m(
                        gate as f64 * spacing_m,
                        f64::from(radial.elevation_deg),
                    );
                    let (east_m, north_m) = (ground * az_rad.sin(), ground * az_rad.cos());
                    let (glat, glon) =
                        aeqd_inverse_km(site_lat, site_lon, east_m / 1000.0, north_m / 1000.0);
                    if bounded_lut.lookup(glat as f32, glon as f32).is_none() {
                        leaks.push((row, east_m, north_m, value));
                    }
                }
            }
            leaks
        };

        let edge_after = build_synthetic_volume(&fields, time, &edge_config);
        let after_img =
            render2d::render_moment_image(&edge_after, 0, MomentType::Reflectivity, raster)
                .expect("render edge-after PPI");
        let after_leak = off_domain_finite(&edge_after).len();

        // "Before": the OLD bbox-dilated LUT swapped into the same fields —
        // test-only; the product path always builds domain-bounded.
        fields.lut = InverseLut::build_with_shape(&fields.lat, &fields.lon, fields.nx, fields.ny)
            .expect("unbounded LUT");
        let edge_before = build_synthetic_volume(&fields, time, &edge_config);
        let before_img =
            render2d::render_moment_image(&edge_before, 0, MomentType::Reflectivity, raster)
                .expect("render edge-before PPI");
        let leaks = off_domain_finite(&edge_before);
        let before_leak = leaks.len();

        // Zoom anchor: the DENSEST leak cluster (mode azimuth ±15 radials).
        // The ring touches the boundary at several separate arcs, so a
        // global centroid averages into the domain interior and frames
        // nothing; fall back to the nearest side midpoint when echo never
        // reaches the boundary at all.
        let leak_center = (!leaks.is_empty()).then(|| {
            let naz = edge_before.cuts[0].radials.len();
            let mut per_row = vec![0usize; naz];
            for &(row, ..) in &leaks {
                per_row[row] += 1;
            }
            let best_row = (0..naz).max_by_key(|&row| per_row[row]).unwrap_or(0);
            let near: Vec<_> = leaks
                .iter()
                .filter(|(row, ..)| row.abs_diff(best_row) <= 15)
                .collect();
            let n = near.len().max(1) as f64;
            (
                near.iter().map(|(_, east, ..)| east).sum::<f64>() / n,
                near.iter().map(|(_, _, north, _)| north).sum::<f64>() / n,
            )
        });
        let (leak_east_m, leak_north_m) =
            leak_center.unwrap_or((edge_east_km * 1000.0, edge_north_km * 1000.0));
        let (edge_x, edge_y) = to_px(leak_east_m, leak_north_m, edge_config.max_range_m);
        save_crop(
            &after_img,
            edge_x,
            edge_y,
            1024,
            &out_dir.join("edge_after_zoom.png"),
        );
        save_crop(
            &before_img,
            edge_x,
            edge_y,
            1024,
            &out_dir.join("edge_before_zoom.png"),
        );
        // Unambiguous ring picture: every leaked GATE painted magenta on the
        // fixed render, straight from the volume data. The leak here is
        // 0-5 dBZ fringe echo — below the REF colour table's first visible
        // level — so it is invisible in a plain before/after PNG pair and a
        // pixel diff only catches renderer smoothing; painting the gate
        // footprints shows exactly where the old LUT smeared past the edge.
        let mut ring_img = after_img.clone();
        let (width_px, height_px) = (ring_img.width() as i64, ring_img.height() as i64);
        for &(_, east_m, north_m, _) in &leaks {
            let (px, py) = to_px(east_m, north_m, edge_config.max_range_m);
            for dy in -2i64..=2 {
                for dx in -2i64..=2 {
                    let (x, y) = (px.round() as i64 + dx, py.round() as i64 + dy);
                    if (0..width_px).contains(&x) && (0..height_px).contains(&y) {
                        ring_img.put_pixel(x as u32, y as u32, image::Rgba([255, 0, 255, 255]));
                    }
                }
            }
        }
        save_crop(
            &ring_img,
            edge_x,
            edge_y,
            1024,
            &out_dir.join("edge_ring_highlight_zoom.png"),
        );

        // Interior integrity, on the REAL file: the fix may only REMOVE
        // off-domain gates — every gate finite in BOTH builds must carry
        // bit-identical values (verified: 0 differing gates on Enderlin
        // 02:15Z; the fix touches nothing inside the domain).
        {
            let MomentStorage::F32(vb) =
                &edge_before.cuts[0].moments[&MomentType::Reflectivity].storage
            else {
                panic!("F32");
            };
            let MomentStorage::F32(va) =
                &edge_after.cuts[0].moments[&MomentType::Reflectivity].storage
            else {
                panic!("F32");
            };
            let value_diffs = vb
                .iter()
                .zip(va)
                .filter(|(b, a)| b.is_finite() && a.is_finite() && b.to_bits() != a.to_bits())
                .count();
            assert_eq!(
                value_diffs, 0,
                "domain bound must not change any in-domain gate value"
            );
        }
        let (leak_min, leak_max) = leaks.iter().fold(
            (f32::INFINITY, f32::NEG_INFINITY),
            |(lo, hi), &(.., value)| (lo.min(value), hi.max(value)),
        );
        eprintln!(
            "[probe] peak echo {:.1} dBZ; domain edge {:.1} km out; \
             off-domain finite gates before={before_leak} after={after_leak} \
             (leak dBZ {leak_min:.1}..{leak_max:.1}); PNGs in {}",
            best.0,
            edge_dist_m / 1000.0,
            out_dir.display()
        );
        assert_eq!(
            after_leak, 0,
            "domain-bounded scan must not leak off-domain gates"
        );
        assert!(
            before_leak > 0,
            "unbounded LUT should show the ring this track fixes"
        );
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
        eprintln!(
            "[prof] open {:.2}s  dims {}x{}x{} nt={}",
            t0.elapsed().as_secs_f64(),
            file.nx,
            file.ny,
            file.nz,
            file.nt
        );
        let config = SyntheticRadarConfig::default();

        let tr = Instant::now();
        let fields =
            read_wrf_radar_fields(&file, 0, config.reflectivity_operator).expect("read fields");
        eprintln!(
            "[prof] read_wrf_radar_fields {:.2}s  refl_source={}",
            tr.elapsed().as_secs_f64(),
            fields.ref_source
        );

        let time = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let tb = Instant::now();
        let volume = build_synthetic_volume(&fields, time, &config);
        eprintln!(
            "[prof] build_synthetic_volume {:.2}s  cuts={} radials={}",
            tb.elapsed().as_secs_f64(),
            volume.cuts.len(),
            volume.metadata.decoded_radial_count
        );
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
            let (dbz, _src) =
                read_reflectivity(&file, 0, nz * cells, ReflectivityOperator::ModelNative).unwrap();
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

        let par = read_wrf_radar_fields(&file, 0, ReflectivityOperator::ModelNative).unwrap();

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
