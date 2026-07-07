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

use rw_store::PressureVolumeInput;
use wrf_core::{ComputeOpts, VarOutput, WrfFile, getvar};

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
    let mut temp_iso = init_planes(levels.len(), cells);
    let mut dewp_iso = init_planes(levels.len(), cells);
    let mut u_iso = init_planes(levels.len(), cells);
    let mut v_iso = init_planes(levels.len(), cells);
    let mut hgt_iso = init_planes(levels.len(), cells);

    let progress_step = (cells / 10).max(1);
    let mut col_p = vec![0f64; nz];
    for c in 0..cells {
        if c % progress_step == 0 {
            progress(format!(
                "interpolating 5 sounding fields to {} isobaric levels — {}%",
                levels.len(),
                c * 100 / cells
            ));
        }
        for k in 0..nz {
            col_p[k] = pressure_hpa[k * cells + c];
        }
        for (li, &lev) in levels.iter().enumerate() {
            let Some((k, t)) = bracket(&col_p, f64::from(lev)) else {
                continue;
            };
            let (i0, i1) = (k * cells + c, (k + 1) * cells + c);
            if let Some(value) = lerp(temp_k[i0], temp_k[i1], t) {
                temp_iso[li][c] = value as f32;
            }
            if let Some(value) = lerp(dewpoint_k[i0], dewpoint_k[i1], t) {
                dewp_iso[li][c] = value as f32;
            }
            if let Some(value) = lerp(u_ms[i0], u_ms[i1], t) {
                u_iso[li][c] = value as f32;
            }
            if let Some(value) = lerp(v_ms[i0], v_ms[i1], t) {
                v_iso[li][c] = value as f32;
            }
            if let Some(value) = lerp(height_m[i0], height_m[i1], t) {
                hgt_iso[li][c] = value as f32;
            }
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
            levels: pack(&levels, temp_iso),
        },
        IsoVolume {
            name: "dewpoint_iso".to_string(),
            units: "K".to_string(),
            levels: pack(&levels, dewp_iso),
        },
        IsoVolume {
            name: "u_iso".to_string(),
            units: "m/s".to_string(),
            levels: pack(&levels, u_iso),
        },
        IsoVolume {
            name: "v_iso".to_string(),
            units: "m/s".to_string(),
            levels: pack(&levels, v_iso),
        },
        IsoVolume {
            name: "height_iso".to_string(),
            units: "gpm".to_string(),
            levels: pack(&levels, hgt_iso),
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
