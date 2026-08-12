//! Build isobaric sounding volumes from a WRF file.
//!
//! Ported verbatim from rusty-weather's `rusty-weather-ui` shell (rev
//! edb9d277) so BowEcho can ingest WRF locally without the standalone shell.
//!
//! WRF is on native (eta) levels, but the skew-T builder
//! ([`rw_ui::skewt::build_sounding_column`]) needs the same `*_iso` isobaric
//! 3D variables the model ingest writes for HRRR/GFS: `temperature_iso`,
//! `dewpoint_iso`, `u_iso`, `v_iso`, `height_iso`. This module reads WRF's 3D
//! fields through `wrf-core`'s `getvar` (which already handles destaggering,
//! theta -> T, geopotential -> height, and QVAPOR -> Td) and log-pressure
//! interpolates each column onto the canonical isobaric levels, so imported
//! WRF runs produce soundings exactly like the downloaded models do.
#![allow(dead_code)]
// `interpolate_iso_volumes` takes the five column fields + shape as separate
// slices by design (the shared raw/post-processed reader contract); factoring
// them into a struct would only obscure the call sites.
#![allow(clippy::too_many_arguments)]

use rayon::{ThreadPool, ThreadPoolBuilder};
use rw_store::PressureVolumeInput;
use std::sync::OnceLock;
use wrf_core::{ComputeOpts, VarOutput, WrfFile, getvar};

const MAX_WRF_ISO_THREADS: usize = 8;
const MIN_PARALLEL_ISO_CELLS: usize = 4_096;

/// Canonical isobaric levels (hPa), matching the model-ingest convention
/// (`100..=1000` step 25 -> 37 levels). Levels outside a column's model range
/// are left NaN and pruned by the sounding column builder.
fn standard_levels() -> Vec<u16> {
    (100..=1000u16).step_by(25).collect()
}

/// One isobaric volume ready for the store writer: owned row-major planes.
pub struct IsoVolume {
    pub name: String,
    pub units: String,
    /// `(level_hpa, plane)` where each plane holds `ny * nx` row-major values.
    pub levels: Vec<(u16, Vec<f32>)>,
}

impl IsoVolume {
    /// Borrowed view for the store writer's [`PressureVolumeInput`].
    pub fn as_input(&self) -> PressureVolumeInput<'_> {
        PressureVolumeInput {
            name: &self.name,
            units: &self.units,
            selector_template: serde_json::json!({
                "source": "wrf",
                "field": self.name,
                "vertical": "isobaric",
            }),
            levels: self
                .levels
                .iter()
                .map(|(hpa, plane)| (*hpa, plane.as_slice()))
                .collect(),
        }
    }
}

/// Lowest-model-level surface fallbacks, in the units the skew-T expects
/// (Pa, K, K, m/s, m/s). Used to synthesize the 2D surface fields a split
/// `wrf3d` file (CONUS404 / GDEX CONUS-II) omits — chiefly `PSFC` — so the
/// sounding can still start at the surface. Each plane is row-major `ny * nx`.
pub struct SurfaceFallback {
    pub surface_pressure_pa: Vec<f32>,
    pub temperature_2m_k: Vec<f32>,
    pub dewpoint_2m_k: Vec<f32>,
    pub u_10m: Vec<f32>,
    pub v_10m: Vec<f32>,
}

struct IsoPlanes {
    temperature: Vec<Vec<f32>>,
    dewpoint: Vec<Vec<f32>>,
    u_wind: Vec<Vec<f32>>,
    v_wind: Vec<Vec<f32>>,
    height: Vec<Vec<f32>>,
}

impl IsoPlanes {
    fn new(levels: usize, cells: usize) -> Self {
        Self {
            temperature: init_planes(levels, cells),
            dewpoint: init_planes(levels, cells),
            u_wind: init_planes(levels, cells),
            v_wind: init_planes(levels, cells),
            height: init_planes(levels, cells),
        }
    }
}

#[derive(Clone, Copy)]
struct IsoInterpolationInputs<'a> {
    pressure_hpa: &'a [f64],
    temp_k: &'a [f64],
    dewpoint_k: &'a [f64],
    height_m: &'a [f64],
    u_ms: &'a [f64],
    v_ms: &'a [f64],
    nz: usize,
    cells: usize,
    levels: &'a [u16],
}

/// Matching cell slices from every level plane. Splitting this value splits
/// every output at the same cell boundary, so parallel workers never alias.
struct IsoPlaneSlices<'a> {
    temperature: Vec<&'a mut [f32]>,
    dewpoint: Vec<&'a mut [f32]>,
    u_wind: Vec<&'a mut [f32]>,
    v_wind: Vec<&'a mut [f32]>,
    height: Vec<&'a mut [f32]>,
}

impl<'a> IsoPlaneSlices<'a> {
    fn for_range(planes: &'a mut IsoPlanes, start: usize, end: usize) -> Self {
        fn field_slices(planes: &mut [Vec<f32>], start: usize, end: usize) -> Vec<&mut [f32]> {
            planes
                .iter_mut()
                .map(|plane| &mut plane[start..end])
                .collect()
        }

        Self {
            temperature: field_slices(&mut planes.temperature, start, end),
            dewpoint: field_slices(&mut planes.dewpoint, start, end),
            u_wind: field_slices(&mut planes.u_wind, start, end),
            v_wind: field_slices(&mut planes.v_wind, start, end),
            height: field_slices(&mut planes.height, start, end),
        }
    }

    fn len(&self) -> usize {
        self.temperature.first().map_or(0, |plane| plane.len())
    }

    fn split_at(self, mid: usize) -> (Self, Self) {
        fn split_field(planes: Vec<&mut [f32]>, mid: usize) -> (Vec<&mut [f32]>, Vec<&mut [f32]>) {
            let mut left = Vec::with_capacity(planes.len());
            let mut right = Vec::with_capacity(planes.len());
            for plane in planes {
                let (left_plane, right_plane) = plane.split_at_mut(mid);
                left.push(left_plane);
                right.push(right_plane);
            }
            (left, right)
        }

        let (temperature_left, temperature_right) = split_field(self.temperature, mid);
        let (dewpoint_left, dewpoint_right) = split_field(self.dewpoint, mid);
        let (u_wind_left, u_wind_right) = split_field(self.u_wind, mid);
        let (v_wind_left, v_wind_right) = split_field(self.v_wind, mid);
        let (height_left, height_right) = split_field(self.height, mid);
        (
            Self {
                temperature: temperature_left,
                dewpoint: dewpoint_left,
                u_wind: u_wind_left,
                v_wind: v_wind_left,
                height: height_left,
            },
            Self {
                temperature: temperature_right,
                dewpoint: dewpoint_right,
                u_wind: u_wind_right,
                v_wind: v_wind_right,
                height: height_right,
            },
        )
    }
}

fn wrf_iso_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().saturating_sub(1).clamp(1, MAX_WRF_ISO_THREADS))
        .unwrap_or(1)
}

/// A bounded pool prevents a large WRF import from occupying every process
/// worker. Calls already running on a Rayon worker use the serial path below,
/// avoiding nested pools and oversubscription.
fn wrf_iso_pool() -> Option<&'static ThreadPool> {
    static POOL: OnceLock<Option<ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let workers = wrf_iso_worker_count();
        if workers < 2 {
            return None;
        }
        ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("wrf-iso-{index}"))
            .build()
            .ok()
    })
    .as_ref()
}

fn try_pressure_columns(count: usize, nz: usize) -> Option<Vec<Vec<f64>>> {
    let mut columns = Vec::new();
    columns.try_reserve_exact(count).ok()?;
    for _ in 0..count {
        let mut column = Vec::new();
        column.try_reserve_exact(nz).ok()?;
        column.resize(nz, 0.0);
        columns.push(column);
    }
    Some(columns)
}

fn interpolate_iso_cells(
    inputs: &IsoInterpolationInputs<'_>,
    mut planes: IsoPlaneSlices<'_>,
    cell_start: usize,
    col_p: &mut [f64],
) {
    for local_cell in 0..planes.len() {
        let cell = cell_start + local_cell;
        for (k, pressure) in col_p.iter_mut().enumerate().take(inputs.nz) {
            *pressure = inputs.pressure_hpa[k * inputs.cells + cell];
        }
        for (level_index, &level) in inputs.levels.iter().enumerate() {
            let Some((k, t)) = bracket(col_p, f64::from(level)) else {
                continue;
            };
            let (i0, i1) = (k * inputs.cells + cell, (k + 1) * inputs.cells + cell);
            if let Some(value) = lerp(inputs.temp_k[i0], inputs.temp_k[i1], t) {
                planes.temperature[level_index][local_cell] = value as f32;
            }
            if let Some(value) = lerp(inputs.dewpoint_k[i0], inputs.dewpoint_k[i1], t) {
                planes.dewpoint[level_index][local_cell] = value as f32;
            }
            if let Some(value) = lerp(inputs.u_ms[i0], inputs.u_ms[i1], t) {
                planes.u_wind[level_index][local_cell] = value as f32;
            }
            if let Some(value) = lerp(inputs.v_ms[i0], inputs.v_ms[i1], t) {
                planes.v_wind[level_index][local_cell] = value as f32;
            }
            if let Some(value) = lerp(inputs.height_m[i0], inputs.height_m[i1], t) {
                planes.height[level_index][local_cell] = value as f32;
            }
        }
    }
}

fn interpolate_iso_cells_parallel(
    inputs: &IsoInterpolationInputs<'_>,
    planes: IsoPlaneSlices<'_>,
    cell_start: usize,
    pressure_columns: &mut [Vec<f64>],
) {
    if pressure_columns.len() <= 1 || planes.len() <= 1 {
        interpolate_iso_cells(inputs, planes, cell_start, &mut pressure_columns[0]);
        return;
    }

    let left_workers = pressure_columns.len() / 2;
    let split_cell = planes.len() * left_workers / pressure_columns.len();
    let (left_planes, right_planes) = planes.split_at(split_cell);
    let (left_columns, right_columns) = pressure_columns.split_at_mut(left_workers);
    rayon::join(
        || interpolate_iso_cells_parallel(inputs, left_planes, cell_start, left_columns),
        || {
            interpolate_iso_cells_parallel(
                inputs,
                right_planes,
                cell_start + split_cell,
                right_columns,
            )
        },
    );
}

fn report_iso_progress(
    progress: &mut dyn FnMut(String),
    level_count: usize,
    cell: usize,
    cells: usize,
) {
    progress(format!(
        "interpolating 5 sounding fields to {level_count} isobaric levels — {}%",
        cell * 100 / cells
    ));
}

/// Read WRF 3D fields for `timeidx` and interpolate them to the canonical
/// isobaric levels, returning the five `*_iso` volumes the skew-T needs plus
/// the lowest-model-level [`SurfaceFallback`] (so callers can fill in any 2D
/// surface field the file omits).
///
/// `cells` is the horizontal grid size (`ny * nx`) of the hour being written;
/// every returned plane matches it. Fails (leaving the caller to skip volumes
/// and still write the 2D fields) if the required 3D fields are unreadable.
///
/// `progress` receives per-stage messages (which 3D field is being read /
/// getvar'd, then interpolation percentage) — on a 250 m grid each stage is
/// tens of seconds, and both import paths surface these lines in the dock.
pub fn build_iso_volumes(
    file: &WrfFile,
    timeidx: usize,
    cells: usize,
    progress: &mut dyn FnMut(String),
) -> Result<(Vec<IsoVolume>, SurfaceFallback), String> {
    if cells == 0 {
        return Err("WRF grid has zero cells".to_string());
    }
    let read = |name: &str, stage: &str| -> Result<VarOutput, String> {
        getvar(file, name, Some(timeidx), &ComputeOpts::default())
            .map_err(|err| format!("read WRF {name} ({stage}): {err}"))
    };

    progress("reading WRF pressure (sounding field 1/5)".to_string());
    let pressure = read("pressure", "sounding field 1/5")?; // hPa, [nz, ny, nx]
    let nz = pressure.data.len() / cells;
    if nz < 2 || nz * cells != pressure.data.len() {
        return Err(format!(
            "WRF pressure field has {} values, not a whole number of {cells}-cell levels",
            pressure.data.len()
        ));
    }

    progress("reading WRF temperature (sounding field 2/5)".to_string());
    let temp = read("temp", "sounding field 2/5")?; // K
    progress("reading WRF dewpoint (sounding field 3/5)".to_string());
    let td = read("td", "sounding field 3/5")?; // degC
    progress("reading WRF height (sounding field 4/5)".to_string());
    let height = read("height", "sounding field 4/5")?; // m MSL
    check_len(&temp, nz * cells, "temp")?;
    check_len(&td, nz * cells, "td")?;
    check_len(&height, nz * cells, "height")?;

    // Earth-relative winds. `uvmet` returns [u_earth.., v_earth..]
    // (2 * nz * cells); fall back to grid-relative ua/va if it is unavailable
    // or the interleaved layout is unexpected. Split without copying: on a
    // 50 M-cell grid the two halves are ~400 MB each, and `to_vec`-ing them
    // while the 800 MB source was still alive measurably spiked the peak
    // working set of the whole import.
    progress("reading WRF winds (sounding field 5/5)".to_string());
    let (u_wind, v_wind) = match read("uvmet", "sounding field 5/5") {
        Ok(uvmet) if uvmet.data.len() == 2 * nz * cells => {
            let mut u = uvmet.data;
            let v = u.split_off(nz * cells);
            (u, v)
        }
        _ => {
            let ua = read("ua", "sounding field 5/5")?;
            let va = read("va", "sounding field 5/5")?;
            check_len(&ua, nz * cells, "ua")?;
            check_len(&va, nz * cells, "va")?;
            (ua.data, va.data)
        }
    };

    // The hour's LAST `getvar` is behind us, and every input the interpolator
    // needs is owned above — release wrf-core's memoized 3-D f64 intermediates
    // NOW, before the interpolation loop and the store write. `getvar`
    // memoizes every intermediate (full pressure, theta, temperature,
    // geopotential, heights, QVAPOR, destaggered winds, …) inside `WrfFile`
    // and only evicts on a timestep CHANGE; on the 800×800×79 Enderlin grid
    // that cache is ~5 GB of dead weight from here on. Clearing any EARLIER
    // was measured to more than double the peak (every read recomputes its
    // whole dependency chain — see docs/wrf-import-large-grids.md); clearing
    // here costs zero recompute. catch_unwind: a poisoned cache mutex (from a
    // caught diagnostic panic upstream) must not fail the volumes.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| file.clear_cache()));

    // Dewpoint arrives in degC from wrf-core's `td`; the shared interpolator
    // works in Kelvin like every other field. Convert in place — a separate
    // Kelvin copy is another ~400 MB on large grids.
    let mut dewpoint_k = td.data;
    for value in &mut dewpoint_k {
        *value += 273.15;
    }
    Ok(interpolate_iso_volumes(
        &pressure.data,
        &temp.data,
        &dewpoint_k,
        &height.data,
        &u_wind,
        &v_wind,
        nz,
        cells,
        progress,
    ))
}

/// Interpolate pre-read WRF column fields onto the canonical isobaric levels
/// and derive the lowest-level surface fallback. All inputs are row-major
/// `[nz, ny, nx]` (index `k * cells + c`) in skew-T units: pressure hPa,
/// temperature K, dewpoint K, height m, winds m/s. Shared by the raw-wrfout
/// (`build_iso_volumes`) and post-processed (`TK`/`Z`/`P`) reader paths.
///
/// `progress` gets a message roughly every 10% of the columns — on a 50 M-cell
/// grid this loop alone is tens of seconds, and the dock shows the latest line.
pub fn interpolate_iso_volumes(
    pressure_hpa: &[f64],
    temp_k: &[f64],
    dewpoint_k: &[f64],
    height_m: &[f64],
    u_ms: &[f64],
    v_ms: &[f64],
    nz: usize,
    cells: usize,
    progress: &mut dyn FnMut(String),
) -> (Vec<IsoVolume>, SurfaceFallback) {
    let levels = standard_levels();
    let mut planes = IsoPlanes::new(levels.len(), cells);
    let inputs = IsoInterpolationInputs {
        pressure_hpa,
        temp_k,
        dewpoint_k,
        height_m,
        u_ms,
        v_ms,
        nz,
        cells,
        levels: &levels,
    };
    let progress_step = (cells / 10).max(1);
    let mut col_p = vec![0f64; nz];
    let pool = match (
        cells >= MIN_PARALLEL_ISO_CELLS,
        rayon::current_thread_index(),
    ) {
        (true, None) => wrf_iso_pool(),
        _ => None,
    };
    let mut pressure_columns =
        pool.and_then(|pool| try_pressure_columns(pool.current_num_threads(), nz));
    for start in (0..cells).step_by(progress_step) {
        report_iso_progress(progress, levels.len(), start, cells);
        let end = start.saturating_add(progress_step).min(cells);
        let plane_slices = IsoPlaneSlices::for_range(&mut planes, start, end);
        if let (Some(pool), Some(columns)) = (pool, pressure_columns.as_mut()) {
            pool.install(|| {
                interpolate_iso_cells_parallel(&inputs, plane_slices, start, columns.as_mut_slice())
            });
        } else {
            interpolate_iso_cells(&inputs, plane_slices, start, &mut col_p);
        }
    }

    // Lowest model level (k=0) as a surface fallback, in skew-T units. Split
    // wrf3d files omit PSFC (and sometimes T2/Td2/winds); the k=0 level sits a
    // few metres above ground, close enough to anchor the sounding surface.
    let level0 = |data: &[f64]| -> Vec<f32> { (0..cells).map(|c| data[c] as f32).collect() };
    let surface = SurfaceFallback {
        surface_pressure_pa: (0..cells)
            .map(|c| (pressure_hpa[c] * 100.0) as f32)
            .collect(),
        temperature_2m_k: level0(temp_k),
        dewpoint_2m_k: level0(dewpoint_k),
        u_10m: level0(u_ms),
        v_10m: level0(v_ms),
    };

    let volumes = vec![
        IsoVolume {
            name: "temperature_iso".to_string(),
            units: "K".to_string(),
            levels: pack(&levels, planes.temperature),
        },
        IsoVolume {
            name: "dewpoint_iso".to_string(),
            units: "K".to_string(),
            levels: pack(&levels, planes.dewpoint),
        },
        IsoVolume {
            name: "u_iso".to_string(),
            units: "m/s".to_string(),
            levels: pack(&levels, planes.u_wind),
        },
        IsoVolume {
            name: "v_iso".to_string(),
            units: "m/s".to_string(),
            levels: pack(&levels, planes.v_wind),
        },
        IsoVolume {
            name: "height_iso".to_string(),
            units: "gpm".to_string(),
            levels: pack(&levels, planes.height),
        },
    ];
    (volumes, surface)
}

fn check_len(out: &VarOutput, expected: usize, name: &str) -> Result<(), String> {
    if out.data.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "WRF {name} has {} values, expected {expected}",
            out.data.len()
        ))
    }
}

fn init_planes(levels: usize, cells: usize) -> Vec<Vec<f32>> {
    vec![vec![f32::NAN; cells]; levels]
}

fn pack(levels: &[u16], planes: Vec<Vec<f32>>) -> Vec<(u16, Vec<f32>)> {
    levels.iter().copied().zip(planes).collect()
}

/// Locate the native levels bracketing `target` hPa in a WRF column (pressure
/// decreasing with index, level 0 nearest the surface) and return the lower
/// level index plus the log-pressure interpolation weight. `None` when the
/// target sits below the lowest level or above the model top.
fn bracket(col_p: &[f64], target: f64) -> Option<(usize, f64)> {
    for k in 0..col_p.len().saturating_sub(1) {
        let (pk, pk1) = (col_p[k], col_p[k + 1]);
        if !pk.is_finite() || !pk1.is_finite() || pk == pk1 {
            continue;
        }
        let (hi, lo) = if pk >= pk1 { (pk, pk1) } else { (pk1, pk) };
        if target <= hi && target >= lo {
            let t = (target.ln() - pk.ln()) / (pk1.ln() - pk.ln());
            return Some((k, t));
        }
    }
    None
}

fn lerp(a: f64, b: f64, t: f64) -> Option<f64> {
    (a.is_finite() && b.is_finite()).then_some(a + t * (b - a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_levels_span_the_isobaric_ladder() {
        let levels = standard_levels();
        assert_eq!(levels.len(), 37);
        assert_eq!(*levels.first().unwrap(), 100);
        assert_eq!(*levels.last().unwrap(), 1000);
    }

    #[test]
    fn bracket_interpolates_in_log_pressure_and_clamps_to_range() {
        // Decreasing pressure with index (level 0 nearest the surface).
        let col = [1000.0, 850.0, 700.0, 500.0];
        // Midway between 1000 and 850 in ln-p.
        let (k, t) = bracket(&col, 925.0).expect("in range");
        assert_eq!(k, 0);
        let expected = (925f64.ln() - 1000f64.ln()) / (850f64.ln() - 1000f64.ln());
        assert!((t - expected).abs() < 1e-9);
        // Below the lowest level and above the top are both out of range.
        assert!(bracket(&col, 1013.0).is_none());
        assert!(bracket(&col, 300.0).is_none());
    }

    #[test]
    fn lerp_skips_non_finite_endpoints() {
        assert_eq!(lerp(0.0, 10.0, 0.5), Some(5.0));
        assert_eq!(lerp(f64::NAN, 10.0, 0.5), None);
        assert_eq!(lerp(0.0, f64::NAN, 0.5), None);
    }

    #[test]
    fn wrf_processing_multicore_interpolation_is_bit_exact_with_serial_cell_order() {
        const CELLS: usize = 4_123;
        const NZ: usize = 9;
        const WORKERS: usize = 4;

        let levels = standard_levels();
        let native_len = NZ * CELLS;
        let mut pressure = Vec::with_capacity(native_len);
        let mut temp = Vec::with_capacity(native_len);
        let mut dewp = Vec::with_capacity(native_len);
        let mut height = Vec::with_capacity(native_len);
        let mut u = Vec::with_capacity(native_len);
        let mut v = Vec::with_capacity(native_len);
        for k in 0..NZ {
            for cell in 0..CELLS {
                let cell_term = cell as f64 * 0.003;
                let level_term = k as f64;
                pressure.push(1_012.75 - f64::from((cell % 17) as u8) * 0.25 - level_term * 112.5);
                temp.push(302.0 - level_term * 5.125 + cell_term);
                dewp.push(296.0 - level_term * 5.375 + cell_term * 0.75);
                height.push(125.0 + level_term * 950.75 + cell_term * 2.0);
                u.push(-12.0 + level_term * 1.75 - cell_term);
                v.push(8.0 - level_term * 0.875 + cell_term * 0.5);
            }
        }

        // Exercise skipped pressure pairs, equal-pressure pairs, and
        // non-finite field endpoints in addition to ordinary columns.
        for cell in (0..CELLS).step_by(257) {
            pressure[3 * CELLS + cell] = f64::NAN;
            temp[5 * CELLS + cell] = f64::NAN;
            dewp[6 * CELLS + cell] = f64::INFINITY;
        }
        for cell in (11..CELLS).step_by(389) {
            pressure[5 * CELLS + cell] = pressure[4 * CELLS + cell];
        }

        let inputs = IsoInterpolationInputs {
            pressure_hpa: &pressure,
            temp_k: &temp,
            dewpoint_k: &dewp,
            height_m: &height,
            u_ms: &u,
            v_ms: &v,
            nz: NZ,
            cells: CELLS,
            levels: &levels,
        };
        let mut serial = IsoPlanes::new(levels.len(), CELLS);
        let mut parallel = IsoPlanes::new(levels.len(), CELLS);
        let mut serial_pressure = vec![0.0; NZ];
        interpolate_iso_cells(
            &inputs,
            IsoPlaneSlices::for_range(&mut serial, 0, CELLS),
            0,
            &mut serial_pressure,
        );

        let pool = ThreadPoolBuilder::new()
            .num_threads(WORKERS)
            .build()
            .expect("test pool");
        let mut parallel_pressure =
            try_pressure_columns(WORKERS, NZ).expect("parallel pressure scratch");
        pool.install(|| {
            interpolate_iso_cells_parallel(
                &inputs,
                IsoPlaneSlices::for_range(&mut parallel, 0, CELLS),
                0,
                &mut parallel_pressure,
            )
        });

        fn assert_field_bits(field: &str, serial: &[Vec<f32>], parallel: &[Vec<f32>]) {
            assert_eq!(serial.len(), parallel.len());
            for (level, (serial_plane, parallel_plane)) in serial.iter().zip(parallel).enumerate() {
                assert_eq!(serial_plane.len(), parallel_plane.len());
                for (cell, (serial_value, parallel_value)) in
                    serial_plane.iter().zip(parallel_plane).enumerate()
                {
                    assert_eq!(
                        serial_value.to_bits(),
                        parallel_value.to_bits(),
                        "{field} differs at level {level}, cell {cell}"
                    );
                }
            }
        }

        assert_field_bits("temperature", &serial.temperature, &parallel.temperature);
        assert_field_bits("dewpoint", &serial.dewpoint, &parallel.dewpoint);
        assert_field_bits("u wind", &serial.u_wind, &parallel.u_wind);
        assert_field_bits("v wind", &serial.v_wind, &parallel.v_wind);
        assert_field_bits("height", &serial.height, &parallel.height);
    }

    /// The shared interpolator must stream progress (both import paths surface
    /// it) and still produce correct planes — guard for the progress plumbing.
    #[test]
    fn interpolate_streams_progress_and_interpolates() {
        // 2 columns × 3 levels, pressure decreasing with index.
        let pressure = vec![1000.0, 1000.0, 850.0, 850.0, 700.0, 700.0];
        let temp = vec![300.0, 301.0, 290.0, 291.0, 280.0, 281.0];
        let dewp = vec![295.0, 296.0, 285.0, 286.0, 275.0, 276.0];
        let height = vec![100.0, 110.0, 1500.0, 1510.0, 3000.0, 3010.0];
        let u = vec![1.0; 6];
        let v = vec![2.0; 6];

        let mut messages = Vec::new();
        let (volumes, surface) = interpolate_iso_volumes(
            &pressure,
            &temp,
            &dewp,
            &height,
            &u,
            &v,
            3,
            2,
            &mut |message| messages.push(message),
        );

        assert!(
            messages
                .iter()
                .all(|message| message.contains("isobaric levels")),
            "unexpected progress lines: {messages:?}"
        );
        assert!(!messages.is_empty(), "interpolation must report progress");

        // 850 hPa is an exact native level: temperature lands unchanged.
        let temps = &volumes[0];
        assert_eq!(temps.name, "temperature_iso");
        let (_, plane_850) = temps
            .levels
            .iter()
            .find(|(hpa, _)| *hpa == 850)
            .expect("850 hPa plane");
        assert!((plane_850[0] - 290.0).abs() < 1e-3);
        assert!((plane_850[1] - 291.0).abs() < 1e-3);
        // Surface fallback comes from level 0 in Pa/K.
        assert!((surface.surface_pressure_pa[0] - 100_000.0).abs() < 1e-3);
        assert!((surface.temperature_2m_k[1] - 301.0).abs() < 1e-3);
    }
}
