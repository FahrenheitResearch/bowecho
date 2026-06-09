//! Mesocyclone / TVS detection implementing the NSSL operational algorithms:
//!
//! - Stumpf et al. 1998, "The National Severe Storms Laboratory Mesocyclone
//!   Detection Algorithm for the WSR-88D", Wea. Forecasting 13(2): 2D feature
//!   strength RANKS (ΔV + shear floors), range taper, expanding-radius
//!   vertical association, and the 3-km-deep / base-below-5-km 3D rank core.
//! - Mitchell et al. 1998, "The National Severe Storms Laboratory Tornado
//!   Detection Algorithm", Wea. Forecasting 13(2): multi-threshold core
//!   extraction with the overlap-split rule, gate-to-gate ΔV (GTGVD), TVS
//!   criteria and depth floor.
//! - LLSD azimuthal shear per Smith & Elmore 2004 / Mahalik et al. 2019;
//!   reflectivity/CC masking in the spirit of MRMS rotation-track QC
//!   (Miller et al. 2013). CC masking is a dual-pol-era extension (postdates
//!   the 1998 papers) and is never applied where it could erase a TDS.
//!
//! The vertical-continuity requirement is the noise killer: single-tilt
//! speckle and residual dealias artifacts essentially never stack across
//! elevations at the same location.

use radar_core::{ElevationCut, MomentGrid, MomentType, RadarVolume, beam_height_above_radar_m};
use rayon::prelude::*;

use crate::dealias_velocity_grid;
use crate::shear::azimuthal_shear_grid;

/// Reflectivity floor for the display QC mask. The MDA's adaptable parameter
/// runs 0–20 dBZ (Stumpf 1998); a viewer wants the top of that bracket so an
/// empty radar can never grow markers.
const REFLECTIVITY_FLOOR_DBZ: f32 = 20.0;
/// CC mask (dual-pol extension): drop gates with ρhv below this ONLY where
/// REF < CC_MASK_MAX_DBZ — biota/chaff live there; tornado debris (low CC,
/// high REF) must never be erased.
const CC_FLOOR: f32 = 0.80;
const CC_MASK_MAX_DBZ: f32 = 10.0;
/// Processing range window (Stumpf 1998 / Mitchell 1998; lower bound is
/// engineering for the clutter/cone-of-silence zone).
const MIN_RANGE_M: f64 = 5_000.0;
const MAX_RANGE_M: f64 = 230_000.0;
/// TVS logic is restricted to ≤ 150 km (Mitchell 1998).
const TVS_MAX_RANGE_M: f64 = 150_000.0;
/// Only gates below 10 km ARL feed detection; 2D features whose centroid
/// sits above 12 km ARL are discarded (Stumpf 1998 / Mitchell 1998).
const MAX_GATE_HEIGHT_M: f64 = 10_000.0;
const MAX_FEATURE_HEIGHT_M: f64 = 12_000.0;
/// Multi-level core-extraction shear thresholds, m/s/km — the rank 1/3/5/7
/// shear floors (Stumpf 1998 Table 2), used with the TDA overlap-split rule
/// (Mitchell 1998 uses six descending ΔV passes; four levels is the
/// speed-constrained equivalent).
const CORE_LEVELS_SHEAR: [f32; 4] = [3.0, 4.5, 6.0, 7.5];
/// Implausible-shear cap (clutter residue), m/s/km.
const MAX_PLAUSIBLE_SHEAR: f32 = 150.0;
/// Feature geometry gates (Stumpf 1998): ≥ 4 range gates, ≥ 4 legacy-
/// equivalent radials (= 8 super-res), couplet diameter 1–10 km (floor
/// relaxed for TVS-scale cores), aspect ratio ≤ 2 (≤ 4 when GTGVD ≥ 25).
const MIN_RANGE_GATES: usize = 4;
/// The LLSD shear ridge of a couplet is azimuthally narrow (it peaks BETWEEN
/// the velocity extrema), so only a small radial minimum applies here —
/// Stumpf's >=4-segment rule stacks across RANGE (MIN_RANGE_GATES above);
/// the couplet's azimuthal scale is enforced by the 1-10 km diameter gate.
const MIN_SUPER_RES_RADIALS: usize = 3;
const MAX_DIAMETER_KM: f64 = 10.0;
const MIN_DIAMETER_KM: f64 = 1.0;
const TVS_MIN_DIAMETER_KM: f64 = 0.25;
const MAX_ASPECT: f64 = 2.0;
const TVS_RELAXED_ASPECT: f64 = 4.0;
/// GTGVD floors (Mitchell 1998): TVS candidacy ≥ 25 m/s; classic low-level
/// TVS alarm ≥ 36 m/s. GTGVD is measured across a 1.0° azimuth span
/// (= 2 super-res radials) because the published thresholds were calibrated
/// at legacy resolution.
const TVS_GTG_DV_MPS: f32 = 25.0;
const TVS_STRONG_GTG_DV_MPS: f32 = 36.0;
/// Vertical association: expanding search 2→8 km (Stumpf 1998); plain
/// acceptance ≤ 5 km, full 8 km only when both features are rank ≥ 5;
/// TVS continuity uses 2.5 km (Mitchell 1998).
const ASSOC_BASE_KM: f64 = 5.0;
const ASSOC_STRONG_KM: f64 = 8.0;
const ASSOC_STRONG_RANK: u8 = 5;
/// 3D criteria (Stumpf 1998): ≥ 2 associated features form a 3D feature;
/// ≥ 3 declare a display-worthy detection (Mitchell 1998) unless the column
/// is meso-strength or TVS; the rank core must be ≥ 3 km deep (beam-
/// extended) with base < 5 km ARL.
const DISPLAY_MIN_FEATURES: usize = 3;
const CORE_MIN_DEPTH_M: f64 = 3_000.0;
const CORE_MAX_BASE_M: f64 = 5_000.0;
/// TVS depth floor (Mitchell 1998).
const TVS_MIN_DEPTH_M: f64 = 1_500.0;
/// Tilts considered (lowest velocity-bearing elevations).
const MAX_TILTS: usize = 8;
const MAX_TILT_ELEVATION_DEG: f32 = 10.0;
const MAX_SITES: usize = 12;

/// Display tier from the 3D strength rank (Stumpf 1998: rank ≥ 5 ≈ the
/// classic Vrot ≥ 15 m/s mesocyclone nomogram line, Andra 1997).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RotationStrength {
    /// 3D rank 1–2.
    WeakCirculation,
    /// 3D rank 3–4.
    ModerateCirculation,
    /// 3D rank ≥ 5 (meso-strength couplet).
    Mesocyclone,
    /// Gate-to-gate ΔV ≥ 25 m/s with ≥ 1.5 km depth within 150 km
    /// (≥ 36 m/s at the lowest tilt = classic TVS).
    Tvs,
}

/// A vertically-continuous rotation detection.
#[derive(Clone, Copy, Debug)]
pub struct RotationSite {
    /// Lowest-tilt feature position.
    pub azimuth_deg: f32,
    pub ground_range_m: f64,
    /// Best rotational velocity (ΔV/2) across the column (m/s).
    pub vrot_mps: f32,
    /// Best 1.0°-equivalent gate-to-gate ΔV on the lowest tilt (m/s).
    pub gate_to_gate_dv_mps: f32,
    /// 3D strength rank (Stumpf 1998 Table 2 scale).
    pub rank: u8,
    /// Tilts the circulation appears on (≥ 2).
    pub depth_tilts: usize,
    /// Core depth (m, beam-extended).
    pub depth_m: f64,
    pub base_elevation_deg: f32,
    pub strength: RotationStrength,
}

/// One per-tilt 2D circulation feature.
#[derive(Clone, Copy, Debug)]
struct Feature2D {
    east_km: f64,
    north_km: f64,
    azimuth_deg: f32,
    ground_range_m: f64,
    elevation_deg: f32,
    height_m: f64,
    half_beam_depth_m: f64,
    delta_v_mps: f32,
    gtg_dv_mps: f32,
    /// Peak LLSD shear (m/s/km) — kept for diagnostics/tuning output.
    #[allow(dead_code)]
    shear_ms_km: f32,
    rank: u8,
}

/// 2D strength rank (Stumpf 1998 Table 2 + combination rule): rank r needs
/// ΔV ≥ 5+5r m/s AND shear ≥ 2.25+0.75r m/s/km, with floors tapered beyond
/// 100 km (ΔV ×(1−0.25·(range−100)/100), shear to 50%, clamped at 200 km).
/// Final rank = max(rank(GTGVD on the ΔV scale), min(rank(ΔV), rank(shear))).
fn rank_2d(delta_v_mps: f32, shear_ms_km: f32, gtg_dv_mps: f32, range_km: f64) -> u8 {
    let taper = |fraction_at_200: f32| -> f32 {
        if range_km <= 100.0 {
            1.0
        } else {
            let t = (((range_km - 100.0) / 100.0).min(1.0)) as f32;
            1.0 - (1.0 - fraction_at_200) * t
        }
    };
    let dv_scale = taper(0.75);
    let shear_scale = taper(0.50);
    let rank_dv = |dv: f32| -> u8 {
        let mut rank = 0u8;
        for r in 1..=25u8 {
            if dv >= (5.0 + 5.0 * r as f32) * dv_scale {
                rank = r;
            } else {
                break;
            }
        }
        rank
    };
    let rank_shear = {
        let mut rank = 0u8;
        for r in 1..=25u8 {
            if shear_ms_km >= (2.25 + 0.75 * r as f32) * shear_scale {
                rank = r;
            } else {
                break;
            }
        }
        rank
    };
    rank_dv(gtg_dv_mps).max(rank_dv(delta_v_mps).min(rank_shear))
}

/// Detect vertically-continuous rotation in a volume (background-thread
/// work: ~100–300 ms on a dense super-res volume; tilts run in parallel).
pub fn detect_rotation_sites(volume: &RadarVolume) -> Vec<RotationSite> {
    let mut velocity_cuts: Vec<usize> = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.moments.contains_key(&MomentType::Velocity)
                && c.elevation_deg <= MAX_TILT_ELEVATION_DEG
        })
        .map(|(i, _)| i)
        .collect();
    velocity_cuts.sort_by(|a, b| {
        volume.cuts[*a]
            .elevation_deg
            .total_cmp(&volume.cuts[*b].elevation_deg)
    });
    velocity_cuts.truncate(MAX_TILTS);
    if velocity_cuts.len() < 2 {
        return Vec::new();
    }

    let per_tilt: Vec<Vec<Feature2D>> = velocity_cuts
        .par_iter()
        .map(|&cut_index| tilt_features(volume, cut_index))
        .collect();

    associate_vertically(&per_tilt)
}

/// Diagnostic: per-tilt (elevation, 2D feature count, best rank) — for
/// threshold tuning against real volumes.
pub fn rotation_features_per_tilt(volume: &RadarVolume) -> Vec<(f32, usize, u8)> {
    let mut velocity_cuts: Vec<usize> = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.moments.contains_key(&MomentType::Velocity)
                && c.elevation_deg <= MAX_TILT_ELEVATION_DEG
        })
        .map(|(i, _)| i)
        .collect();
    velocity_cuts.sort_by(|a, b| {
        volume.cuts[*a]
            .elevation_deg
            .total_cmp(&volume.cuts[*b].elevation_deg)
    });
    velocity_cuts.truncate(MAX_TILTS);
    velocity_cuts
        .iter()
        .map(|&cut_index| {
            let features = tilt_features(volume, cut_index);
            let best = features.iter().map(|f| f.rank).max().unwrap_or(0);
            (volume.cuts[cut_index].elevation_deg, features.len(), best)
        })
        .collect()
}

/// Vertical association + 3D ranking (Stumpf 1998 §3c; Mitchell 1998 §2).
fn associate_vertically(per_tilt: &[Vec<Feature2D>]) -> Vec<RotationSite> {
    let mut used: Vec<Vec<bool>> = per_tilt.iter().map(|f| vec![false; f.len()]).collect();

    // Strongest-first seeding (1-to-1 assignment; conflicts resolve to the
    // stronger feature, per Stumpf 1998).
    let mut seeds: Vec<(usize, usize, u8)> = Vec::new();
    for (tilt, features) in per_tilt.iter().enumerate() {
        for (index, feature) in features.iter().enumerate() {
            seeds.push((tilt, index, feature.rank));
        }
    }
    seeds.sort_by(|a, b| b.2.cmp(&a.2));

    let mut sites = Vec::new();
    for (seed_tilt, seed_index, _) in seeds {
        if used[seed_tilt][seed_index] {
            continue;
        }
        used[seed_tilt][seed_index] = true;
        let seed = per_tilt[seed_tilt][seed_index];
        let mut column = vec![seed];
        // Grow across ADJACENT tilts in both directions (no gaps: a missing
        // level ends the column, per Stumpf 1998).
        for direction in [1i64, -1i64] {
            let mut current = seed;
            let mut tilt = seed_tilt as i64;
            loop {
                tilt += direction;
                if tilt < 0 || tilt as usize >= per_tilt.len() {
                    break;
                }
                let features = &per_tilt[tilt as usize];
                let mut best: Option<(usize, f64)> = None;
                for (index, feature) in features.iter().enumerate() {
                    if used[tilt as usize][index] {
                        continue;
                    }
                    let distance = ((feature.east_km - current.east_km).powi(2)
                        + (feature.north_km - current.north_km).powi(2))
                    .sqrt();
                    let limit =
                        if feature.rank >= ASSOC_STRONG_RANK && current.rank >= ASSOC_STRONG_RANK {
                            ASSOC_STRONG_KM
                        } else {
                            ASSOC_BASE_KM
                        };
                    if distance <= limit && best.is_none_or(|(_, d)| distance < d) {
                        best = Some((index, distance));
                    }
                }
                let Some((index, _)) = best else {
                    break;
                };
                used[tilt as usize][index] = true;
                current = features[index];
                column.push(current);
            }
        }
        if column.len() < 2 {
            continue;
        }
        column.sort_by(|a, b| a.height_m.total_cmp(&b.height_m));

        // 3D rank: highest R whose CONTINUOUS rank≥R core is ≥ 3 km deep
        // (beam-extended) with base below 5 km ARL (Stumpf 1998, Table 3).
        let mut rank3d = 0u8;
        let max_rank = column.iter().map(|f| f.rank).max().unwrap_or(0);
        for required in 1..=max_rank {
            let mut qualifies = false;
            let mut run: Vec<&Feature2D> = Vec::new();
            let sentinel = Feature2D {
                rank: 0,
                ..column[0]
            };
            for feature in column.iter().chain(std::iter::once(&sentinel)) {
                if feature.rank >= required {
                    run.push(feature);
                } else {
                    if !run.is_empty() {
                        let base = run[0].height_m - run[0].half_beam_depth_m;
                        let top =
                            run[run.len() - 1].height_m + run[run.len() - 1].half_beam_depth_m;
                        if top - base >= CORE_MIN_DEPTH_M && base < CORE_MAX_BASE_M {
                            qualifies = true;
                        }
                    }
                    run.clear();
                }
            }
            if qualifies {
                rank3d = required;
            }
        }

        let base = column[0];
        let depth_m = (column[column.len() - 1].height_m
            + column[column.len() - 1].half_beam_depth_m)
            - (base.height_m - base.half_beam_depth_m);
        let best_vrot = column
            .iter()
            .map(|f| f.delta_v_mps * 0.5)
            .fold(0.0f32, f32::max);
        let base_gtg = base.gtg_dv_mps;

        // TVS (Mitchell 1998): GTGVD ≥ 25 with ≥ 1.5 km depth inside 150 km;
        // ≥ 36 m/s at the lowest tilt is the classic alarm.
        let tvs = base.ground_range_m <= TVS_MAX_RANGE_M
            && depth_m >= TVS_MIN_DEPTH_M
            && (base_gtg >= TVS_STRONG_GTG_DV_MPS
                || (base_gtg >= TVS_GTG_DV_MPS
                    && column
                        .iter()
                        .filter(|f| f.gtg_dv_mps >= TVS_GTG_DV_MPS)
                        .count()
                        >= 2));

        // Display rule (Mitchell 1998): ≥ 3 associated features, unless the
        // column is meso-strength or TVS.
        if column.len() < DISPLAY_MIN_FEATURES && rank3d < ASSOC_STRONG_RANK && !tvs {
            continue;
        }
        if rank3d == 0 && !tvs {
            continue;
        }

        let strength = if tvs {
            RotationStrength::Tvs
        } else if rank3d >= 5 {
            RotationStrength::Mesocyclone
        } else if rank3d >= 3 {
            RotationStrength::ModerateCirculation
        } else {
            RotationStrength::WeakCirculation
        };
        sites.push(RotationSite {
            azimuth_deg: base.azimuth_deg,
            ground_range_m: base.ground_range_m,
            vrot_mps: best_vrot,
            gate_to_gate_dv_mps: base_gtg,
            rank: rank3d,
            depth_tilts: column.len(),
            depth_m,
            base_elevation_deg: base.elevation_deg,
            strength,
        });
    }
    sites.sort_by(|a, b| {
        (b.strength == RotationStrength::Tvs)
            .cmp(&(a.strength == RotationStrength::Tvs))
            .then(b.rank.cmp(&a.rank))
            .then(b.vrot_mps.total_cmp(&a.vrot_mps))
    });
    sites.truncate(MAX_SITES);
    sites
}

/// Per-tilt: QC mask → multi-level core extraction → attribute measurement.
fn tilt_features(volume: &RadarVolume, cut_index: usize) -> Vec<Feature2D> {
    let cut = &volume.cuts[cut_index];
    let Some(velocity) = cut.moments.get(&MomentType::Velocity) else {
        return Vec::new();
    };
    let dealiased = dealias_velocity_grid(cut, velocity);
    let shear = azimuthal_shear_grid(cut, velocity);
    let rows = shear.radial_count();
    let gates = shear.gate_range.gate_count;
    if rows == 0 || gates == 0 {
        return Vec::new();
    }
    let spacing_m = shear.gate_range.gate_spacing_m.max(1) as f64;
    let first_gate_m = shear.gate_range.first_gate_m as f64;
    let elevation = cut.elevation_deg as f64;
    let reflectivity = cut.moments.get(&MomentType::Reflectivity);
    // Row orderings differ between grids on the same cut: map shear rows to
    // REF rows through the radial indices, and ranges through REF's own gate
    // geometry (sampling by raw row index misaligns the mask).
    let ref_row_by_radial: Vec<usize> = if let Some(grid) = reflectivity {
        let mut map = vec![usize::MAX; cut.radials.len()];
        for (row, &radial_index) in grid.radial_indices.iter().enumerate() {
            if let Some(slot) = map.get_mut(radial_index) {
                *slot = row;
            }
        }
        map
    } else {
        Vec::new()
    };
    let cc_source = correlation_source(volume, cut_index);

    // The display shear grid is ×1000 ≡ m/s/km — the unit the rank tables use.
    let shear_at = |row: usize, gate: usize| -> Option<f32> {
        shear.scaled_value(row, gate).filter(|v| v.is_finite())
    };
    let vel_at = |row: usize, gate: usize| -> Option<f32> {
        dealiased.scaled_value(row, gate).filter(|v| v.is_finite())
    };

    // QC + multi-level seed mask (cyclonic only — the operational MDA
    // detects cyclonic circulations).
    let debug = std::env::var_os("BOWECHO_ROT_DEBUG").is_some();
    let mut n_shear = 0usize;
    let mut n_ref = 0usize;
    let mut n_median = 0usize;
    let mut level_mask = vec![0u8; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            let range = first_gate_m + gate as f64 * spacing_m;
            if range > MAX_RANGE_M {
                break;
            }
            if range < MIN_RANGE_M
                || beam_height_above_radar_m(range, elevation) > MAX_GATE_HEIGHT_M
            {
                continue;
            }
            let Some(s) = shear_at(row, gate) else {
                continue;
            };
            if !(CORE_LEVELS_SHEAR[0]..=MAX_PLAUSIBLE_SHEAR).contains(&s) {
                continue;
            }
            n_shear += 1;
            // Reflectivity floor: rotation markers only inside actual echo.
            let dbz = reflectivity.and_then(|grid| {
                let radial_index = *shear.radial_indices.get(row)?;
                let ref_row = *ref_row_by_radial.get(radial_index)?;
                if ref_row == usize::MAX {
                    return None;
                }
                let ref_gate = ((range - grid.gate_range.first_gate_m as f64)
                    / grid.gate_range.gate_spacing_m.max(1) as f64)
                    .round();
                if ref_gate < 0.0 || ref_gate as usize >= grid.gate_range.gate_count {
                    return None;
                }
                grid.scaled_value(ref_row, ref_gate as usize)
                    .filter(|v| v.is_finite())
            });
            if !dbz.is_some_and(|v| v >= REFLECTIVITY_FLOOR_DBZ) {
                continue;
            }
            n_ref += 1;
            // CC mask only in weak echo (never erases a debris signature).
            if dbz.is_some_and(|v| v < CC_MASK_MAX_DBZ)
                && let Some((cc_cut, cc_grid, az_lookup)) = &cc_source
            {
                let az = cut
                    .radials
                    .get(*shear.radial_indices.get(row).unwrap_or(&0))
                    .map(|r| r.azimuth_deg)
                    .unwrap_or(0.0);
                if let Some(cc) = sample_by_az_range(cc_cut, cc_grid, az_lookup, az, range)
                    && cc < CC_FLOOR
                {
                    continue;
                }
            }
            // Median-of-9 speckle rejection (rotation-track QC practice).
            let mut neighborhood = [0.0f32; 9];
            let mut n = 0;
            for dr in -1i64..=1 {
                for dg in -1i64..=1 {
                    let r = ((row as i64 + dr).rem_euclid(rows as i64)) as usize;
                    let g = gate as i64 + dg;
                    if g < 0 || g >= gates as i64 {
                        continue;
                    }
                    if let Some(v) = shear_at(r, g as usize) {
                        neighborhood[n] = v;
                        n += 1;
                    }
                }
            }
            if n < 5 {
                continue;
            }
            neighborhood[..n].sort_by(f32::total_cmp);
            if neighborhood[n / 2] < CORE_LEVELS_SHEAR[0] {
                continue;
            }
            n_median += 1;
            let mut level = 1u8;
            for (i, &threshold) in CORE_LEVELS_SHEAR.iter().enumerate().skip(1) {
                if s >= threshold {
                    level = i as u8 + 1;
                }
            }
            level_mask[row * gates + gate] = level;
        }
    }

    // Multi-level connected components with the overlap-split rule
    // (Mitchell 1998): descend from the strongest level; an envelope that
    // encloses a single core replaces it, one bridging several cores is
    // dropped so the cores stay separate (the QLCS line-vs-vortex isolator).
    let mut features: Vec<Vec<usize>> = Vec::new();
    for level in (1..=CORE_LEVELS_SHEAR.len() as u8).rev() {
        let components = connected_components(&level_mask, rows, gates, level);
        if features.is_empty() {
            features = components;
            continue;
        }
        let mut next: Vec<Vec<usize>> = Vec::new();
        let mut absorbed = vec![false; features.len()];
        for component in components {
            let cells: std::collections::HashSet<usize> = component.iter().copied().collect();
            let mut overlaps = Vec::new();
            for (index, feature) in features.iter().enumerate() {
                if feature.iter().any(|cell| cells.contains(cell)) {
                    overlaps.push(index);
                }
            }
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

    if debug {
        eprintln!(
            "tilt {:.2}: shear-pass {} -> ref-pass {} -> median-pass {} -> {} components",
            cut.elevation_deg,
            n_shear,
            n_ref,
            n_median,
            features.len()
        );
    }
    if debug {
        let mut sizes: Vec<(usize, usize, usize)> = features
            .iter()
            .map(|cells| {
                let mut rows_seen = std::collections::HashSet::new();
                let mut gmin = usize::MAX;
                let mut gmax = 0usize;
                for &cell in cells {
                    rows_seen.insert(cell / gates);
                    gmin = gmin.min(cell % gates);
                    gmax = gmax.max(cell % gates);
                }
                (cells.len(), rows_seen.len(), gmax.saturating_sub(gmin) + 1)
            })
            .collect();
        sizes.sort_by(|a, b| b.0.cmp(&a.0));
        for (n, r, g) in sizes.iter().take(5) {
            eprintln!("  comp: {n} cells, {r} rows, {g} gate-span");
        }
    }
    // Attribute extraction + geometry gates (Stumpf 1998 attribute stage).
    let mut out = Vec::new();
    let mut rej = [0usize; 6];
    for cells in &features {
        let mut min_gate = usize::MAX;
        let mut max_gate = 0usize;
        let mut row_set: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut peak_shear = 0.0f32;
        let mut peak_cell = cells[0];
        for &cell in cells {
            let (row, gate) = (cell / gates, cell % gates);
            row_set.insert(row);
            min_gate = min_gate.min(gate);
            max_gate = max_gate.max(gate);
            if let Some(s) = shear_at(row, gate)
                && s > peak_shear
            {
                peak_shear = s;
                peak_cell = cell;
            }
        }
        if max_gate.saturating_sub(min_gate) + 1 < MIN_RANGE_GATES
            || row_set.len() < MIN_SUPER_RES_RADIALS
        {
            rej[0] += 1;
            continue;
        }
        let (peak_row, peak_gate) = (peak_cell / gates, peak_cell % gates);
        let range_m = first_gate_m + peak_gate as f64 * spacing_m;
        let height_m = beam_height_above_radar_m(range_m, elevation);
        if height_m > MAX_FEATURE_HEIGHT_M {
            rej[1] += 1;
            continue;
        }

        // ΔV / Vrot / GTGVD from the dealiased velocity over the feature's
        // bounding sector plus a small pad. GTGVD spans 2 super-res radials
        // (the 1.0° legacy calibration of the published thresholds).
        let mut rows_sorted: Vec<i64> = row_set.iter().map(|&r| r as i64).collect();
        rows_sorted.sort_unstable();
        let pad_rows = 4i64;
        let pad_gates = 3i64;
        let (row_lo, row_hi) = (
            rows_sorted[0] - pad_rows,
            rows_sorted[rows_sorted.len() - 1] + pad_rows,
        );
        let mut v_min = f32::INFINITY;
        let mut v_min_pos = (0usize, 0usize);
        let mut v_max = f32::NEG_INFINITY;
        let mut v_max_pos = (0usize, 0usize);
        let mut gtg = 0.0f32;
        let mut valid = 0usize;
        let mut total = 0usize;
        for raw_row in row_lo..=row_hi {
            let row = (raw_row.rem_euclid(rows as i64)) as usize;
            let row_2 = ((raw_row + 2).rem_euclid(rows as i64)) as usize;
            for raw_gate in (min_gate as i64 - pad_gates)..=(max_gate as i64 + pad_gates) {
                if raw_gate < 0 || raw_gate >= gates as i64 {
                    continue;
                }
                let gate = raw_gate as usize;
                total += 1;
                let Some(v) = vel_at(row, gate) else {
                    continue;
                };
                valid += 1;
                if v < v_min {
                    v_min = v;
                    v_min_pos = (row, gate);
                }
                if v > v_max {
                    v_max = v;
                    v_max_pos = (row, gate);
                }
                if let Some(v2) = vel_at(row_2, gate) {
                    gtg = gtg.max((v - v2).abs());
                }
            }
        }
        // Validity fraction (engineering default; Mitchell 1998 specifies a
        // skip rule rather than a fraction).
        if total == 0 || (valid as f32) / (total as f32) < 0.5 {
            rej[2] += 1;
            continue;
        }
        if !v_min.is_finite() || !v_max.is_finite() {
            continue;
        }
        let delta_v = v_max - v_min;

        // Couplet diameter = distance between the velocity extrema
        // (1–10 km vortex scale; floor relaxed for TVS-scale cores).
        let to_xy = |(row, gate): (usize, usize)| -> (f64, f64) {
            let az = shear
                .radial_indices
                .get(row)
                .and_then(|&i| cut.radials.get(i))
                .map(|r| r.azimuth_deg as f64)
                .unwrap_or(0.0)
                .to_radians();
            let r = (first_gate_m + gate as f64 * spacing_m) / 1000.0;
            (r * az.sin(), r * az.cos())
        };
        let (x1, y1) = to_xy(v_min_pos);
        let (x2, y2) = to_xy(v_max_pos);
        let diameter_km = ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt();
        let min_diameter = if gtg >= TVS_GTG_DV_MPS {
            TVS_MIN_DIAMETER_KM
        } else {
            MIN_DIAMETER_KM
        };
        if diameter_km < min_diameter || diameter_km > MAX_DIAMETER_KM {
            rej[3] += 1;
            continue;
        }
        // Aspect ratio gate: long thin radial features are gust fronts /
        // convergence lines, not vortices (relaxed for TVS-scale cores).
        let radial_extent_km = ((max_gate - min_gate + 1) as f64) * spacing_m / 1000.0;
        let azimuthal_extent_km =
            (row_set.len() as f64) * (360.0 / rows as f64).to_radians() * range_m / 1000.0;
        if azimuthal_extent_km > 0.0 {
            let aspect = radial_extent_km / azimuthal_extent_km;
            let limit = if gtg >= TVS_GTG_DV_MPS {
                TVS_RELAXED_ASPECT
            } else {
                MAX_ASPECT
            };
            if aspect > limit {
                rej[4] += 1;
                continue;
            }
        }

        let rank = rank_2d(delta_v, peak_shear, gtg, range_m / 1000.0);
        if rank == 0 {
            rej[5] += 1;
            continue;
        }
        let Some(radial) = shear
            .radial_indices
            .get(peak_row)
            .and_then(|&index| cut.radials.get(index))
        else {
            continue;
        };
        let az_rad = (radial.azimuth_deg as f64).to_radians();
        // Half-power beam depth ≈ range × half the 1° beamwidth.
        let half_beam_depth_m = range_m * (1.0_f64.to_radians() / 2.0).tan();
        out.push(Feature2D {
            east_km: range_m / 1000.0 * az_rad.sin(),
            north_km: range_m / 1000.0 * az_rad.cos(),
            azimuth_deg: radial.azimuth_deg,
            ground_range_m: range_m,
            elevation_deg: cut.elevation_deg,
            height_m,
            half_beam_depth_m,
            delta_v_mps: delta_v,
            gtg_dv_mps: gtg,
            shear_ms_km: peak_shear,
            rank,
        });
    }
    if debug {
        eprintln!(
            "tilt {:.2}: rejections size {} height {} validity {} diameter {} aspect {} rank {} -> kept {}",
            cut.elevation_deg,
            rej[0],
            rej[1],
            rej[2],
            rej[3],
            rej[4],
            rej[5],
            out.len()
        );
    }
    out
}

fn connected_components(
    level_mask: &[u8],
    rows: usize,
    gates: usize,
    min_level: u8,
) -> Vec<Vec<usize>> {
    let mut visited = vec![false; rows * gates];
    let mut components = Vec::new();
    let mut stack = Vec::new();
    for seed in 0..rows * gates {
        if level_mask[seed] < min_level || visited[seed] {
            continue;
        }
        stack.clear();
        stack.push(seed);
        visited[seed] = true;
        let mut cells = Vec::new();
        while let Some(cell) = stack.pop() {
            cells.push(cell);
            let (row, gate) = (cell / gates, cell % gates);
            // 8-connected; azimuth wraps.
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
                    let idx = r * gates + g as usize;
                    if level_mask[idx] >= min_level && !visited[idx] {
                        visited[idx] = true;
                        stack.push(idx);
                    }
                }
            }
        }
        components.push(cells);
    }
    components
}

type CcSource<'a> = (&'a ElevationCut, &'a MomentGrid, Vec<usize>);

/// CC for a velocity cut: same cut when present, else the nearest-elevation
/// cut carrying CC (the paired surveillance cut on split-cut VCPs).
fn correlation_source(volume: &RadarVolume, cut_index: usize) -> Option<CcSource<'_>> {
    let elevation = volume.cuts[cut_index].elevation_deg;
    let (cc_cut_index, _) = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::CorrelationCoefficient))
        .map(|(i, c)| (i, (c.elevation_deg - elevation).abs()))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, diff)| *diff <= 0.5)?;
    let cc_cut = &volume.cuts[cc_cut_index];
    let grid = cc_cut.moments.get(&MomentType::CorrelationCoefficient)?;
    let mut lookup = vec![usize::MAX; 720];
    for (row, &radial_index) in grid.radial_indices.iter().enumerate() {
        if let Some(radial) = cc_cut.radials.get(radial_index) {
            let bin = ((radial.azimuth_deg.rem_euclid(360.0)) * 2.0) as usize % 720;
            lookup[bin] = row;
        }
    }
    let filled: Vec<usize> = (0..720)
        .map(|bin| {
            (0..8)
                .flat_map(|step| [(bin + step) % 720, (bin + 720 - step) % 720])
                .map(|b| lookup[b])
                .find(|&row| row != usize::MAX)
                .unwrap_or(usize::MAX)
        })
        .collect();
    Some((cc_cut, grid, filled))
}

fn sample_by_az_range(
    _cut: &ElevationCut,
    grid: &MomentGrid,
    az_lookup: &[usize],
    azimuth_deg: f32,
    range_m: f64,
) -> Option<f32> {
    let bin = ((azimuth_deg.rem_euclid(360.0)) * 2.0) as usize % 720;
    let row = *az_lookup.get(bin)?;
    if row == usize::MAX {
        return None;
    }
    let gate = ((range_m - grid.gate_range.first_gate_m as f64)
        / grid.gate_range.gate_spacing_m.max(1) as f64)
        .round();
    if gate < 0.0 || gate as usize >= grid.gate_range.gate_count {
        return None;
    }
    grid.scaled_value(row, gate as usize)
        .filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentStorage, Radial};

    /// Stumpf et al. 1998 worked example: range 75 km, ΔV 22.3 m/s, shear
    /// 3.82 m/s/km, GTGVD 24.5 m/s → 2D rank 3.
    #[test]
    fn stumpf_worked_example_ranks_3() {
        assert_eq!(rank_2d(22.3, 3.82, 24.5, 75.0), 3);
    }

    /// Stumpf et al. 1998 Table 3: 2D ranks (4,5,6,3,3) at heights
    /// (1.7, 3.5, 5.3, 7.3, 9.2) km → 3D rank 5 (the rank-6 core alone is
    /// too shallow / based too high).
    #[test]
    fn stumpf_table3_vertical_rank() {
        let heights = [1_700.0, 3_500.0, 5_300.0, 7_300.0, 9_200.0];
        let ranks = [4u8, 5, 6, 3, 3];
        let per_tilt: Vec<Vec<Feature2D>> = heights
            .iter()
            .zip(ranks.iter())
            .map(|(&height_m, &rank)| {
                vec![Feature2D {
                    east_km: 0.0,
                    north_km: 0.0,
                    azimuth_deg: 0.0,
                    ground_range_m: 60_000.0,
                    elevation_deg: 1.0,
                    height_m,
                    half_beam_depth_m: 1_050.0,
                    delta_v_mps: 5.0 + 5.0 * rank as f32,
                    gtg_dv_mps: 0.0,
                    shear_ms_km: 2.25 + 0.75 * rank as f32,
                    rank,
                }]
            })
            .collect();
        let sites = associate_vertically(&per_tilt);
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].rank, 5, "Table 3 3D rank must be 5: {sites:?}");
        assert_eq!(sites[0].strength, RotationStrength::Mesocyclone);
    }

    fn velocity_cut(elevation: f32, rows: usize, gates: usize) -> (ElevationCut, GateRange) {
        let gate_range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elevation, None);
        for r in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: r as f32 * (360.0 / rows as f32),
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(60.0),
                radial_status: None,
            });
        }
        (cut, gate_range)
    }

    fn f32_grid(
        moment: MomentType,
        gate_range: &GateRange,
        rows: usize,
        data: Vec<f32>,
    ) -> MomentGrid {
        MomentGrid {
            moment,
            gate_range: gate_range.clone(),
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..rows).collect(),
            storage: MomentStorage::F32(data),
        }
    }

    /// A cyclonic couplet at az ~90°, gates 78..94 (~20 km), ±25 m/s,
    /// embedded in 50 dBZ echo.
    /// Couplet at az ~90 deg, ~60 km range (gates 230..250), +/-25 m/s,
    /// inside 50 dBZ echo - far enough out that a 0.5-3.5 deg column spans
    /// more than the 3 km core-depth floor.
    fn tilt(elevation: f32, with_echo: bool, with_couplet: bool) -> ElevationCut {
        let (mut cut, gate_range) = velocity_cut(elevation, 720, 300);
        let rows = 720;
        let gates = 300;
        let mut velocity = vec![0.0f32; rows * gates];
        if with_couplet {
            // az 90 deg = row 180 at 0.5 deg spacing. Cyclonic seen from the
            // radar: inbound on the low-azimuth side, outbound on the high.
            // Rankine-style core: linear ramp -25 -> +25 m/s across rows
            // 174..186 (solid-body rotation), with plateaus either side —
            // shear spreads across the whole core like a real couplet.
            for row in 166..=198usize {
                let v = if row < 174 {
                    -25.0
                } else if row <= 186 {
                    -25.0 + 50.0 * (row - 174) as f32 / 12.0
                } else {
                    25.0
                };
                for gate in 230..250 {
                    velocity[row * gates + gate] = v;
                }
            }
        }
        cut.moments.insert(
            MomentType::Velocity,
            f32_grid(MomentType::Velocity, &gate_range, rows, velocity),
        );
        if with_echo {
            let mut dbz = vec![f32::NAN; rows * gates];
            for row in 150..210 {
                for gate in 200..290 {
                    dbz[row * gates + gate] = 50.0;
                }
            }
            cut.moments.insert(
                MomentType::Reflectivity,
                f32_grid(MomentType::Reflectivity, &gate_range, rows, dbz),
            );
        }
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
    fn vertically_continuous_couplet_is_detected() {
        let volume = volume_of(vec![
            tilt(0.5, true, true),
            tilt(1.5, true, true),
            tilt(2.4, true, true),
            tilt(3.4, true, true),
        ]);
        let sites = detect_rotation_sites(&volume);
        assert_eq!(sites.len(), 1, "expected one site: {sites:?}");
        let site = &sites[0];
        assert!(
            (site.azimuth_deg - 90.0).abs() <= 4.0,
            "az {}",
            site.azimuth_deg
        );
        assert!(site.depth_tilts >= 3);
        assert!(site.vrot_mps >= 20.0, "vrot {}", site.vrot_mps);
        assert!(matches!(
            site.strength,
            RotationStrength::Mesocyclone | RotationStrength::Tvs
        ));
    }

    #[test]
    fn single_tilt_couplet_is_rejected() {
        let volume = volume_of(vec![
            tilt(0.5, true, true),
            tilt(1.5, true, false),
            tilt(2.4, true, false),
        ]);
        assert!(detect_rotation_sites(&volume).is_empty());
    }

    #[test]
    fn couplet_without_echo_is_rejected() {
        // The "20 circles on an empty radar" failure mode: strong couplets,
        // zero reflectivity → the QC mask refuses all of it.
        let volume = volume_of(vec![
            tilt(0.5, false, true),
            tilt(1.5, false, true),
            tilt(2.4, false, true),
        ]);
        assert!(detect_rotation_sites(&volume).is_empty());
    }

    #[test]
    fn quiet_volume_detects_nothing() {
        let volume = volume_of(vec![tilt(0.5, true, false), tilt(1.5, true, false)]);
        assert!(detect_rotation_sites(&volume).is_empty());
    }
}
