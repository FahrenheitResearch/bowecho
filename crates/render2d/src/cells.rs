//! Storm cell identification — the per-volume half of storm tracking.
//!
//! Cells are found on composite reflectivity with the ENHANCED WATERSHED of
//! Lakshmanan, Hondl & Rabin (2009, "An Efficient, General-Purpose Technique
//! for Identifying Storm Cells in Geospatial Images", J. Atmos. Oceanic
//! Technol. 26(3), 523–537) — the WDSS-II identifier, and the one
//! Lakshmanan & Smith (2010, Wea. Forecasting 25(2), 721–729) used as the
//! common base for objective tracker comparison. Unlike single/multi
//! threshold connected components, the watershed tests every threshold in
//! one immersion pass with a per-cell hysteresis depth, so two strong cores
//! inside one contiguous QLCS envelope split into two cells (the failure
//! mode that motivated ETITAN, Han et al. 2009, JTECH 26(4), 719–732).
//!
//! Centroids are mass-weighted with Z_linear^(4/7) per the Greene & Clark
//! (1972) liquid-water relation used by SCIT (Johnson et al. 1998, Wea.
//! Forecasting 13(2), 263–276, App. B Eq. B1).

use radar_core::{MomentType, RadarVolume};

use crate::volumetric::composite_reflectivity_grid;

/// Quantization (Lakshmanan et al. 2009 Eq. 2): a=30 dBZ floor (the SCIT
/// ladder's bottom rung, Johnson et al. 1998 App. A #12, and the cell
/// definition of Lakshmanan & Smith 2010 footnote 1), b=60 dBZ, δ=1 dBZ.
const QUANT_A_DBZ: f32 = 30.0;
const QUANT_B_DBZ: f32 = 60.0;
const QUANT_DELTA_DBZ: f32 = 1.0;
/// Per-cell hysteresis bound (Lakshmanan et al. 2009 "maximum depth").
const MAX_DEPTH: i32 = 8;
/// Saliency: minimum true gate area for a basin to be a cell
/// (Lakshmanan & Smith 2010 footnote 1).
const MIN_CELL_AREA_KM2: f64 = 20.0;
/// Gaussian smoothing sigma in grid pixels (Lakshmanan et al. 2009
/// smoothing stage; 7-tap separable, azimuth wraps).
const SMOOTH_SIGMA_PX: f32 = 1.5;
const MAX_CELLS: usize = 32;
const MAX_RANGE_M: f64 = 300_000.0;

/// One identified storm cell.
#[derive(Clone, Copy, Debug)]
pub struct StormCell {
    /// Mass-weighted centroid (Z^(4/7)·area), km east/north of the radar.
    pub east_km: f64,
    pub north_km: f64,
    /// Peak smoothed composite reflectivity in the basin.
    pub max_dbz: f32,
    pub area_km2: f64,
    /// √(area/π) — the size term used in association gates/costs
    /// (Han et al. 2009; Lakshmanan & Smith 2010).
    pub eq_radius_km: f64,
    /// Σ area·Z_lin^(4/7) — consistency attribute for association.
    pub mass: f64,
    /// The hysteresis level (dBZ) the basin was captured at.
    pub hlevel_dbz: f32,
}

/// Identify storm cells on the volume's composite reflectivity (enhanced
/// watershed). Runs in a few ms on a dense composite (plus the composite
/// build itself, which is rayon-parallel); callers keep it off the UI thread.
pub fn identify_storm_cells(volume: &RadarVolume) -> Vec<StormCell> {
    let Some(composite) = composite_reflectivity_grid(volume) else {
        return Vec::new();
    };
    let Some(base_cut) = volume
        .cuts
        .iter()
        .filter(|c| c.moments.contains_key(&MomentType::Reflectivity))
        .min_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg))
    else {
        return Vec::new();
    };
    let rows = composite.radial_count();
    let gates = composite.gate_range.gate_count;
    if rows == 0 || gates == 0 {
        return Vec::new();
    }
    let spacing_m = composite.gate_range.gate_spacing_m.max(1) as f64;
    let first_gate_m = composite.gate_range.first_gate_m as f64;
    let az_step_rad = (360.0 / rows as f64).to_radians();
    let max_gate = (((MAX_RANGE_M - first_gate_m) / spacing_m) as usize).min(gates);

    // ---- raw dBZ field (NaN where missing / beyond range cap) ----
    let mut field = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..max_gate {
            if let Some(v) = composite.scaled_value(row, gate).filter(|v| v.is_finite()) {
                field[row * gates + gate] = v;
            }
        }
    }

    // ---- NaN-aware separable Gaussian smooth (azimuth wraps) ----
    let smoothed = smooth_field(&field, rows, gates, SMOOTH_SIGMA_PX);

    // ---- quantize (Eq. 2) ----
    let max_level = ((QUANT_B_DBZ - QUANT_A_DBZ) / QUANT_DELTA_DBZ).round() as i32; // 30
    let quantized: Vec<i32> = smoothed
        .iter()
        .map(|&v| {
            if !v.is_finite() || v <= QUANT_A_DBZ {
                -1
            } else {
                (((v.min(QUANT_B_DBZ) - QUANT_A_DBZ) / QUANT_DELTA_DBZ).round() as i32)
                    .min(max_level)
            }
        })
        .collect();

    // ---- candidate maxima bucketed by level, strongest first, with
    // 8-neighbor suppression of already-accepted candidates (their
    // pre-pruning) ----
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); (max_level + 1) as usize];
    for (idx, &level) in quantized.iter().enumerate() {
        if level >= 1 {
            buckets[level as usize].push(idx);
        }
    }
    let neighbors = |idx: usize, out: &mut [usize; 8]| -> usize {
        let (row, gate) = (idx / gates, idx % gates);
        let mut count = 0;
        for dr in -1i64..=1 {
            for dg in -1i64..=1 {
                if dr == 0 && dg == 0 {
                    continue;
                }
                let r = ((row as i64 + dr).rem_euclid(rows as i64)) as usize;
                let g = gate as i64 + dg;
                if g < 0 || g >= gates as i64 {
                    continue;
                }
                out[count] = r * gates + g as usize;
                count += 1;
            }
        }
        count
    };
    let mut is_candidate = vec![false; rows * gates];
    let mut candidates: Vec<(i32, usize)> = Vec::new();
    let mut scratch = [0usize; 8];
    for level in (1..=max_level).rev() {
        'pixel: for &idx in &buckets[level as usize] {
            let n = neighbors(idx, &mut scratch);
            for &nb in &scratch[..n] {
                if is_candidate[nb] || quantized[nb] > level {
                    continue 'pixel;
                }
            }
            is_candidate[idx] = true;
            candidates.push((level, idx));
        }
    }

    // ---- immersion (Procedures 2–3): depth-major, level-minor, BFS basin
    // growth with true-gate-area saliency; failed centers re-queue lower ----
    let gate_area_km2 = |idx: usize| -> f64 {
        let gate = idx % gates;
        let range = first_gate_m + gate as f64 * spacing_m;
        (range * az_step_rad) * spacing_m / 1.0e6
    };
    // Candidates are processed strongest-level first; each candidate retries
    // IMMEDIATELY with increasing depth (hlevel = level − depth) until the
    // basin is salient or MAX_DEPTH is exhausted (Lakshmanan et al. 2009
    // Procedure 3 — deferring retries lets weaker maxima swallow a core
    // that merely failed saliency at depth 0). After a successful capture
    // the FOOTHILLS — contiguous pixels below the capture level down to
    // hlevel − MAX_DEPTH — are reserved so lower maxima cannot claim the
    // annulus around a captured core as a separate ring cell (the paper's
    // foothills reservation).
    let mut label = vec![u32::MAX; rows * gates];
    let mut foothill = vec![false; rows * gates];
    let mut basins: Vec<Vec<usize>> = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    let mut captured = Vec::new();
    for &(level, center) in &candidates {
        if label[center] != u32::MAX || foothill[center] {
            continue;
        }
        for depth in 0..=MAX_DEPTH {
            let hlevel = level - depth;
            if hlevel < 1 {
                break;
            }
            // BFS-capture contiguous pixels with Q >= hlevel.
            queue.clear();
            captured.clear();
            let mut area = 0.0f64;
            queue.push_back(center);
            let mark = basins.len() as u32;
            label[center] = mark;
            while let Some(idx) = queue.pop_front() {
                captured.push(idx);
                area += gate_area_km2(idx);
                let n = neighbors(idx, &mut scratch);
                for &nb in &scratch[..n] {
                    if label[nb] == u32::MAX && !foothill[nb] && quantized[nb] >= hlevel {
                        label[nb] = mark;
                        queue.push_back(nb);
                    }
                }
            }
            if area >= MIN_CELL_AREA_KM2 {
                if std::env::var_os("BOWECHO_CELL_DEBUG").is_some() {
                    let (cr, cg) = (center / gates, center % gates);
                    eprintln!(
                        "CAPTURE level={level} depth={depth} hlevel={hlevel} area={area:.1} px={} center=({cr},{cg})",
                        captured.len()
                    );
                }
                // Reserve the foothills: contiguous Q in
                // [hlevel − MAX_DEPTH, hlevel) around the basin.
                let foot_floor = (hlevel - MAX_DEPTH).max(1);
                queue.clear();
                for &idx in &captured {
                    queue.push_back(idx);
                }
                while let Some(idx) = queue.pop_front() {
                    let n = neighbors(idx, &mut scratch);
                    for &nb in &scratch[..n] {
                        if label[nb] == u32::MAX
                            && !foothill[nb]
                            && quantized[nb] >= foot_floor
                            && quantized[nb] < hlevel
                        {
                            foothill[nb] = true;
                            queue.push_back(nb);
                        }
                    }
                }
                basins.push(captured.clone());
                break;
            }
            // Roll back; retry one level deeper immediately.
            for &idx in &captured {
                label[idx] = u32::MAX;
            }
        }
    }

    // ---- centroids: mass = area · Z_lin^(4/7) (Greene & Clark 1972 via
    // Johnson et al. 1998 App. B Eq. B1) ----
    let mut cells: Vec<StormCell> = basins
        .iter()
        .filter_map(|pixels| {
            let mut mass = 0.0f64;
            let mut east = 0.0f64;
            let mut north = 0.0f64;
            let mut area = 0.0f64;
            let mut max_dbz = f32::NEG_INFINITY;
            let mut hlevel = f32::NEG_INFINITY;
            for &idx in pixels {
                let (row, gate) = (idx / gates, idx % gates);
                let dbz = smoothed[idx];
                if !dbz.is_finite() {
                    continue;
                }
                let z_lin = 10.0f64.powf(dbz as f64 / 10.0);
                let cell_area = gate_area_km2(idx);
                let w = cell_area * z_lin.powf(4.0 / 7.0);
                let azimuth = composite
                    .radial_indices
                    .get(row)
                    .and_then(|&i| base_cut.radials.get(i))
                    .map(|r| r.azimuth_deg as f64)
                    .unwrap_or((row as f64) * 360.0 / rows as f64)
                    .to_radians();
                let range_km = (first_gate_m + gate as f64 * spacing_m) / 1000.0;
                east += w * range_km * azimuth.sin();
                north += w * range_km * azimuth.cos();
                mass += w;
                area += cell_area;
                max_dbz = max_dbz.max(dbz);
                hlevel = hlevel.max(QUANT_A_DBZ + quantized[idx] as f32 * QUANT_DELTA_DBZ);
            }
            if mass <= 0.0 || area < MIN_CELL_AREA_KM2 {
                return None;
            }
            Some(StormCell {
                east_km: east / mass,
                north_km: north / mass,
                max_dbz,
                area_km2: area,
                eq_radius_km: (area / std::f64::consts::PI).sqrt(),
                mass,
                hlevel_dbz: hlevel,
            })
        })
        .collect();
    cells.sort_by(|a, b| b.max_dbz.total_cmp(&a.max_dbz));
    cells.truncate(MAX_CELLS);
    cells
}

/// NaN-aware separable Gaussian on the polar grid (wraps in azimuth,
/// clamps in range). 7-tap for sigma 1.5.
fn smooth_field(field: &[f32], rows: usize, gates: usize, sigma: f32) -> Vec<f32> {
    let radius = 3i64;
    let kernel: Vec<f32> = (-radius..=radius)
        .map(|k| (-(k as f32).powi(2) / (2.0 * sigma * sigma)).exp())
        .collect();
    let mut tmp = vec![f32::NAN; rows * gates];
    // Pass 1: along range (clamped).
    for row in 0..rows {
        for gate in 0..gates {
            if !field[row * gates + gate].is_finite() {
                continue;
            }
            let mut sum = 0.0f32;
            let mut weight = 0.0f32;
            for (ki, k) in (-radius..=radius).zip(kernel.iter()) {
                let g = gate as i64 + ki;
                if g < 0 || g >= gates as i64 {
                    continue;
                }
                let v = field[row * gates + g as usize];
                if v.is_finite() {
                    sum += v * k;
                    weight += k;
                }
            }
            if weight > 0.0 {
                tmp[row * gates + gate] = sum / weight;
            }
        }
    }
    // Pass 2: along azimuth (wraps).
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            if !tmp[row * gates + gate].is_finite() {
                continue;
            }
            let mut sum = 0.0f32;
            let mut weight = 0.0f32;
            for (ki, k) in (-radius..=radius).zip(kernel.iter()) {
                let r = ((row as i64 + ki).rem_euclid(rows as i64)) as usize;
                let v = tmp[r * gates + gate];
                if v.is_finite() {
                    sum += v * k;
                    weight += k;
                }
            }
            if weight > 0.0 {
                out[row * gates + gate] = sum / weight;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{ElevationCut, GateRange, MomentGrid, MomentStorage, RadarSite, Radial};

    /// Synthetic volume with one REF tilt whose field is given by a closure
    /// of (east_km, north_km).
    fn volume_with_field(rows: usize, gates: usize, f: impl Fn(f64, f64) -> f32) -> RadarVolume {
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 500,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(0.5, None);
        let mut data = vec![f32::NAN; rows * gates];
        for row in 0..rows {
            let az = (row as f64) * 360.0 / rows as f64;
            cut.radials.push(Radial {
                azimuth_deg: az as f32,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            for gate in 0..gates {
                let range_km = (1000.0 + gate as f64 * 500.0) / 1000.0;
                let east = range_km * az.to_radians().sin();
                let north = range_km * az.to_radians().cos();
                data[row * gates + gate] = f(east, north);
            }
        }
        cut.moments.insert(
            MomentType::Reflectivity,
            MomentGrid {
                moment: MomentType::Reflectivity,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: (0..rows).collect(),
                storage: MomentStorage::F32(data),
            },
        );
        let mut volume = RadarVolume::new(
            RadarSite::new("TEST"),
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        );
        volume.cuts = vec![cut];
        volume
    }

    fn gaussian_core(east: f64, north: f64, cx: f64, cy: f64, peak: f32, sigma_km: f64) -> f32 {
        let d2 = (east - cx).powi(2) + (north - cy).powi(2);
        peak * (-d2 / (2.0 * sigma_km * sigma_km)).exp() as f32
    }

    #[test]
    fn finds_a_single_strong_cell_at_the_right_place() {
        let volume = volume_with_field(360, 240, |e, n| {
            20.0 + gaussian_core(e, n, 30.0, 40.0, 35.0, 6.0)
        });
        let cells = identify_storm_cells(&volume);
        assert_eq!(cells.len(), 1, "{cells:?}");
        let cell = &cells[0];
        assert!((cell.east_km - 30.0).abs() < 2.0, "{cell:?}");
        assert!((cell.north_km - 40.0).abs() < 2.0, "{cell:?}");
        assert!(cell.max_dbz > 45.0);
    }

    #[test]
    fn weak_echo_yields_no_cells() {
        let volume = volume_with_field(180, 120, |_, _| 22.0);
        assert!(identify_storm_cells(&volume).is_empty());
    }

    #[test]
    fn empty_volume_yields_no_cells() {
        let volume = volume_with_field(180, 120, |_, _| f32::NAN);
        assert!(identify_storm_cells(&volume).is_empty());
    }

    #[test]
    fn watershed_splits_two_cores_in_one_envelope() {
        // Two 55+ dBZ cores 12 km apart inside one contiguous ~42 dBZ QLCS
        // envelope — single-threshold CC at 40 dBZ returns ONE blob (the
        // ETITAN motivating case, Han et al. 2009); the watershed must
        // return TWO cells near the cores.
        let volume = volume_with_field(360, 240, |e, n| {
            let envelope = gaussian_core(e, n, 30.0, 34.0, 22.0, 14.0);
            let core_a = gaussian_core(e, n, 24.0, 30.0, 35.0, 2.8);
            let core_b = gaussian_core(e, n, 36.0, 38.0, 33.0, 2.8);
            20.0 + envelope + core_a + core_b
        });
        let cells = identify_storm_cells(&volume);
        assert!(cells.len() >= 2, "expected the cores to split: {cells:?}");
        let near = |cx: f64, cy: f64| {
            cells
                .iter()
                .any(|c| ((c.east_km - cx).powi(2) + (c.north_km - cy).powi(2)).sqrt() < 4.0)
        };
        assert!(near(24.0, 30.0), "missing core A: {cells:?}");
        assert!(near(36.0, 38.0), "missing core B: {cells:?}");
    }
}
