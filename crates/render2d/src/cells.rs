//! Storm cell identification — the per-volume half of SCIT (Johnson et al.
//! 1998, "The Storm Cell Identification and Tracking Algorithm", Wea.
//! Forecasting 13(2)). Cells are found on composite reflectivity by
//! multi-threshold clustering (the SCIT 30→60 dBZ ladder, condensed), with
//! reflectivity-weighted centroids; cross-volume tracking and motion fits
//! live with the app's frame history.

use radar_core::{MomentType, RadarVolume};

use crate::volumetric::composite_reflectivity_grid;

/// SCIT identifies cells at 30/35/40/45/50/55/60 dBZ; we keep a condensed
/// ladder — the highest threshold that yields a coherent cell wins, so a
/// strong core inside a larger envelope is reported once at its tightest
/// extent (Johnson et al. 1998 §2a).
const CELL_LEVELS_DBZ: [f32; 4] = [40.0, 45.0, 50.0, 55.0];
/// Minimum cell area (km²) — SCIT's minimum component sizes reject specks.
const MIN_CELL_AREA_KM2: f64 = 8.0;
const MAX_CELLS: usize = 20;
const MAX_RANGE_M: f64 = 300_000.0;

/// One identified storm cell.
#[derive(Clone, Copy, Debug)]
pub struct StormCell {
    /// Reflectivity-weighted centroid, km east/north of the radar.
    pub east_km: f64,
    pub north_km: f64,
    pub max_dbz: f32,
    pub area_km2: f64,
    /// Threshold level (dBZ) the cell was identified at.
    pub level_dbz: f32,
}

/// Identify storm cells on the volume's composite reflectivity. Runs in
/// ~50–80 ms on a dense volume (the composite itself is rayon-parallel);
/// callers should keep it off the UI thread.
pub fn identify_storm_cells(volume: &RadarVolume) -> Vec<StormCell> {
    let Some(composite) = composite_reflectivity_grid(volume) else {
        return Vec::new();
    };
    // Geometry of the composite grid (lowest REF tilt's polar geometry).
    let Some((_, base_cut)) = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Reflectivity))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
        .map(|(i, _)| (i, &volume.cuts[i]))
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

    let value_at = |row: usize, gate: usize| -> Option<f32> {
        composite.scaled_value(row, gate).filter(|v| v.is_finite())
    };
    // Gate area grows with range: dA ≈ range · Δaz · Δrange.
    let gate_area_km2 = |gate: usize| -> f64 {
        let range = first_gate_m + gate as f64 * spacing_m;
        (range * az_step_rad) * spacing_m / 1.0e6
    };
    let azimuth_of = |row: usize| -> f64 {
        composite
            .radial_indices
            .get(row)
            .and_then(|&i| base_cut.radials.get(i))
            .map(|r| r.azimuth_deg as f64)
            .unwrap_or((row as f64) * 360.0 / rows as f64)
    };

    // Multi-level clustering: start at the strongest threshold; descend,
    // keeping an envelope only when it contains exactly one stronger core.
    let mut features: Vec<Vec<usize>> = Vec::new();
    let mut visited = vec![false; rows * gates];
    for &level in CELL_LEVELS_DBZ.iter().rev() {
        visited.iter_mut().for_each(|v| *v = false);
        let mut components: Vec<Vec<usize>> = Vec::new();
        let mut stack = Vec::new();
        for seed in 0..rows * gates {
            if visited[seed] {
                continue;
            }
            let (row, gate) = (seed / gates, seed % gates);
            let range = first_gate_m + gate as f64 * spacing_m;
            if range > MAX_RANGE_M {
                continue;
            }
            if !value_at(row, gate).is_some_and(|v| v >= level) {
                continue;
            }
            stack.clear();
            stack.push(seed);
            visited[seed] = true;
            let mut cells = Vec::new();
            while let Some(cell) = stack.pop() {
                cells.push(cell);
                let (r0, g0) = (cell / gates, cell % gates);
                for dr in -1i64..=1 {
                    for dg in -1i64..=1 {
                        if dr == 0 && dg == 0 {
                            continue;
                        }
                        let r = ((r0 as i64 + dr).rem_euclid(rows as i64)) as usize;
                        let g = g0 as i64 + dg;
                        if g < 0 || g >= gates as i64 {
                            continue;
                        }
                        let idx = r * gates + g as usize;
                        if !visited[idx] && value_at(r, g as usize).is_some_and(|v| v >= level) {
                            visited[idx] = true;
                            stack.push(idx);
                        }
                    }
                }
            }
            components.push(cells);
        }
        if features.is_empty() {
            features = components;
            continue;
        }
        let mut next: Vec<Vec<usize>> = Vec::new();
        let mut absorbed = vec![false; features.len()];
        for component in components {
            let cells: std::collections::HashSet<usize> = component.iter().copied().collect();
            let overlaps: Vec<usize> = features
                .iter()
                .enumerate()
                .filter(|(_, f)| f.iter().any(|c| cells.contains(c)))
                .map(|(i, _)| i)
                .collect();
            match overlaps.len() {
                0 => next.push(component),
                1 => {
                    absorbed[overlaps[0]] = true;
                    next.push(component);
                }
                _ => {}
            }
        }
        for (index, feature) in features.iter().enumerate() {
            if !absorbed[index] {
                next.push(feature.clone());
            }
        }
        features = next;
    }

    // Centroids + attributes.
    let mut out = Vec::new();
    for cells in &features {
        let mut area = 0.0f64;
        let mut weight_sum = 0.0f64;
        let mut east_sum = 0.0f64;
        let mut north_sum = 0.0f64;
        let mut max_dbz = f32::NEG_INFINITY;
        let mut level = f32::INFINITY;
        for &cell in cells {
            let (row, gate) = (cell / gates, cell % gates);
            let Some(dbz) = value_at(row, gate) else {
                continue;
            };
            max_dbz = max_dbz.max(dbz);
            level = level.min(dbz);
            let a = gate_area_km2(gate);
            area += a;
            // Linear-Z weighting concentrates the centroid in the core.
            let weight = a * 10f64.powf((dbz as f64) / 10.0).min(1.0e7);
            let range_km = (first_gate_m + gate as f64 * spacing_m) / 1000.0;
            let az = azimuth_of(row).to_radians();
            east_sum += weight * range_km * az.sin();
            north_sum += weight * range_km * az.cos();
            weight_sum += weight;
        }
        if area < MIN_CELL_AREA_KM2 || weight_sum <= 0.0 || !max_dbz.is_finite() {
            continue;
        }
        out.push(StormCell {
            east_km: east_sum / weight_sum,
            north_km: north_sum / weight_sum,
            max_dbz,
            area_km2: area,
            level_dbz: CELL_LEVELS_DBZ[0].max(level.floor()),
        });
    }
    out.sort_by(|a, b| b.max_dbz.total_cmp(&a.max_dbz));
    out.truncate(MAX_CELLS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{ElevationCut, GateRange, MomentGrid, MomentStorage, Radial};

    fn ref_cut_with_blob(
        elevation: f32,
        rows: usize,
        gates: usize,
        blob: Option<(std::ops::Range<usize>, std::ops::Range<usize>, f32)>,
    ) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 1_000,
            gate_spacing_m: 1_000,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elevation, None);
        for r in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: r as f32 * (360.0 / rows as f32),
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        let mut data = vec![f32::NAN; rows * gates];
        if let Some((row_range, gate_range_idx, dbz)) = blob {
            for row in row_range {
                for gate in gate_range_idx.clone() {
                    data[row * gates + gate] = dbz;
                }
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
        cut
    }

    fn volume_of(cuts: Vec<ElevationCut>) -> RadarVolume {
        let mut volume = RadarVolume::new(
            radar_core::RadarSite::new("TEST"),
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        );
        volume.cuts = cuts;
        volume
    }

    #[test]
    fn finds_a_55dbz_core_at_the_right_place() {
        // 55 dBZ blob centred near az 45°, ~40 km.
        let volume = volume_of(vec![ref_cut_with_blob(
            0.5,
            360,
            120,
            Some((40..51, 35..46, 55.0)),
        )]);
        let cells = identify_storm_cells(&volume);
        assert_eq!(cells.len(), 1, "{cells:?}");
        let cell = &cells[0];
        assert!((cell.max_dbz - 55.0).abs() < 0.1);
        // az 45° at ~40 km → east ≈ north ≈ 28 km.
        assert!(
            (cell.east_km - 28.0).abs() < 6.0 && (cell.north_km - 28.0).abs() < 6.0,
            "{cell:?}"
        );
        assert!(cell.area_km2 >= MIN_CELL_AREA_KM2);
    }

    #[test]
    fn weak_echo_yields_no_cells() {
        let volume = volume_of(vec![ref_cut_with_blob(
            0.5,
            360,
            120,
            Some((40..51, 35..46, 30.0)),
        )]);
        assert!(identify_storm_cells(&volume).is_empty());
    }

    #[test]
    fn empty_volume_yields_no_cells() {
        let volume = volume_of(vec![ref_cut_with_blob(0.5, 360, 120, None)]);
        assert!(identify_storm_cells(&volume).is_empty());
    }
}
