//! TOR TRACKS — rotation tracks and tornado-debris-signature (TDS) flags.
//!
//! **Rotation tracks** accumulate the per-cell MAXIMUM low-level cyclonic
//! azimuthal shear over a frame sequence on a fixed radar-centered Cartesian
//! grid — the swath a translating mesocyclone paints. This is the
//! single-radar analogue of the MRMS rotation-tracks product family:
//!
//! - Mahalik et al. 2019, *Estimates of Gradients in Radar Moments Using a
//!   Linear Least Squares Derivative Technique*, Wea. Forecasting 34,
//!   1423–1447, doi:10.1175/WAF-D-18-0165.1 — the operational LLSD
//!   azimuthal-shear formulation and the 0–2 km AGL "low-level" layer.
//! - Miller et al. 2013, *A multi-sensor severe weather nowcast system using
//!   rotation tracks*, 28th Conf. IIPS, AMS — the rotation-tracks
//!   time-accumulation (running max) and its reflectivity-based QC.
//! - Smith et al. 2016, *Multi-Radar Multi-Sensor (MRMS) Severe Weather and
//!   Aviation Products*, BAMS 97, 1617–1630, doi:10.1175/BAMS-D-14-00173.1 —
//!   the MRMS product lineage (0.005° ≈ 500 m grid; we default to 500 m).
//!
//! Sign convention: the LLSD azimuthal shear of [`crate::shear`] is positive
//! for Northern-Hemisphere cyclonic rotation. Rotation tracks accumulate
//! **cyclonic shear only** (positive values), matching the operational MDA /
//! MRMS rotation-track convention; anticyclonic shear (outflow flanks,
//! anticyclonic members of vortex couplets) is intentionally excluded.
//!
//! **TDS flags** are a deterministic dual-pol physics criterion — NOT a
//! probability: low co-polar correlation inside real echo, co-located with a
//! rank-significant detected circulation, at the lowest dual-pol tilt:
//!
//! - Ryzhkov et al. 2005, *Polarimetric Tornado Detection*, J. Appl. Meteor.
//!   44, 557–570 — the polarimetric TDS (lofted debris: low ρhv, high Z,
//!   co-located with the vortex).
//! - Van Den Broeke & Jauernic 2014, *Spatial and Temporal Characteristics of
//!   Polarimetric Tornadic Debris Signatures*, J. Appl. Meteor. Climatol. 53,
//!   2217–2231 — operational criteria bracket (ρhv ≲ 0.82, Z ≳ 30 dBZ near
//!   the circulation center).
//! - Snyder & Ryzhkov 2015, *Automated Detection of Polarimetric Tornadic
//!   Debris Signatures Using a Hydrometeor Classification Algorithm*,
//!   J. Appl. Meteor. Climatol. 54, 1861–1870 — debris as a deterministic
//!   class from the same predictors.

use std::collections::HashSet;

use radar_core::{MomentGrid, MomentType, RadarVolume, beam_height_above_radar_m};

use crate::detect::{RotationSite, RotationStrength};
use crate::shear::azimuthal_shear_grid;

/// Top of the "low-level" layer (m above radar level, used as the AGL proxy —
/// the WSR-88D feedhorn sits only tens of meters above ground). Mahalik et
/// al. 2019 define the operational low-level azimuthal-shear product over
/// 0–2 km AGL; with a single radar the 0.5° beam exits this layer near
/// ~125 km range, which bounds the swath's range coverage.
const LOW_LEVEL_TOP_M: f64 = 2_000.0;
/// Tilts feeding the low-level composite: the lowest velocity-bearing
/// elevations (≤ 2.0°, at most 3 — e.g. 0.5/0.9/1.3 on VCP 12/212). Higher
/// tilts only contribute very close to the radar before they leave the
/// 0–2 km layer.
const LOW_LEVEL_MAX_TILT_DEG: f32 = 2.0;
const MAX_LOW_TILTS: usize = 3;
/// Range window. The lower bound excludes the clutter/sidelobe zone around
/// the radar (same engineering floor as the MDA port in [`crate::detect`]):
/// near-field clutter makes spurious LLSD shear that a running max would
/// paint permanently.
const TRACKS_MIN_RANGE_M: f64 = 5_000.0;
/// Reflectivity QC floor (dBZ): azimuthal shear is only accumulated inside
/// real echo, per the MRMS rotation-track QC practice (Miller et al. 2013).
/// Clear-air boundary-layer shear noise must never paint the swath.
const TRACKS_REFLECTIVITY_FLOOR_DBZ: f32 = 20.0;
/// Implausible-shear cap (clutter residue), ×10⁻³ s⁻¹.
const MAX_PLAUSIBLE_SHEAR_E3: f32 = 150.0;
/// Impulse rejection: every accumulated gate is capped at its 3×3
/// neighborhood median (rotation-track QC practice, Miller et al. 2013; the
/// KMKX 2026-06-11 QLCS validation showed single-gate dealias residue
/// reaching 0.1+ s⁻¹ that a running max would keep forever, while a reject
/// threshold alone left heavy speckle). A couplet's LLSD ridge spans several
/// radials — the LLSD window itself smooths it — so the median cap preserves
/// real swaths and kills 1–2-gate spikes.
const MEDIAN_CAP_MIN_NEIGHBORS: usize = 5;
/// Azimuth lookup resolution (0.25°/bin covers super-res 0.5° radials).
const AZ_BINS: usize = 1440;

/// Display ramp window, ×10⁻³ s⁻¹: transparent below 0.003 s⁻¹, saturating
/// magenta at 0.02 s⁻¹ (brackets the strong-mesocyclone azimuthal-shear range
/// of the MRMS rotation-track display; Mahalik et al. 2019).
pub const TRACK_DISPLAY_FLOOR_E3: f32 = 3.0;
pub const TRACK_DISPLAY_CEIL_E3: f32 = 20.0;

/// TDS criteria (Ryzhkov et al. 2005; Van Den Broeke & Jauernic 2014):
/// ρhv below 0.82 inside ≥ 30 dBZ echo, within 5 km of a detected
/// circulation of 3-D rank ≥ 3 (or TVS). The rank anchor is the co-located
/// "significant azimuthal shear" requirement — debris-like dual-pol values
/// without a vortex (hail cores, biota) must not flag.
pub const TDS_CC_MAX: f32 = 0.82;
pub const TDS_MIN_DBZ: f32 = 30.0;
pub const TDS_ANCHOR_RADIUS_KM: f64 = 5.0;
pub const TDS_ANCHOR_MIN_RANK: u8 = 3;
/// Debris is a low-level signature (median TDS heights are well below
/// 1.5 km AGL; Van Den Broeke & Jauernic 2014) — gates whose beam center
/// sits above 3 km ARL never flag, whatever the anchor says.
const TDS_MAX_BEAM_HEIGHT_M: f64 = 3_000.0;
const MAX_TDS_GATES: usize = 4_096;

/// Geometry of the radar-centered accumulation grid: a square of
/// `size() × size()` cells in planar ENU km about the radar, row 0 at the
/// north edge (matches the radar raster's planar geometry, so the same
/// AEQD placement applies).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TracksGridSpec {
    pub half_extent_km: f32,
    pub cell_km: f32,
}

impl Default for TracksGridSpec {
    fn default() -> Self {
        // ±150 km at 500 m ≈ the MRMS 0.005° rotation-track grid (Smith et
        // al. 2016); 150 km also brackets where the 0.5° beam has long left
        // the 0–2 km layer.
        Self {
            half_extent_km: 150.0,
            cell_km: 0.5,
        }
    }
}

impl TracksGridSpec {
    /// Cells per side.
    pub fn size(&self) -> usize {
        ((2.0 * self.half_extent_km / self.cell_km).round() as usize).max(1)
    }

    /// Total cell count (`size²`).
    pub fn cell_count(&self) -> usize {
        self.size() * self.size()
    }

    /// ENU km of a cell center; row 0 = north edge, column 0 = west edge.
    pub fn cell_center_km(&self, column: usize, row: usize) -> (f32, f32) {
        let east = (column as f32 + 0.5) * self.cell_km - self.half_extent_km;
        let north = self.half_extent_km - (row as f32 + 0.5) * self.cell_km;
        (east, north)
    }

    /// Flat cell index containing an ENU point, if inside the grid.
    pub fn cell_index(&self, east_km: f32, north_km: f32) -> Option<usize> {
        let size = self.size();
        let column = ((east_km + self.half_extent_km) / self.cell_km).floor();
        let row = ((self.half_extent_km - north_km) / self.cell_km).floor();
        if column < 0.0 || row < 0.0 {
            return None;
        }
        let (column, row) = (column as usize, row as usize);
        if column >= size || row >= size {
            return None;
        }
        Some(row * size + column)
    }
}

/// One tilt's QC'd LLSD azimuthal shear with the lookup tables needed for
/// pull-resampling onto the Cartesian grid.
struct TiltShearField {
    /// ×10⁻³ s⁻¹; NaN = no data / QC-rejected; 0 = data, no rotation signal.
    shear: Vec<f32>,
    gates: usize,
    first_gate_m: f64,
    spacing_m: f64,
    /// Azimuth bin (0.25°) → grid row, `usize::MAX` when empty.
    az_row: Vec<usize>,
    min_gate: usize,
    /// Last gate whose beam center is still inside the 0–2 km layer.
    max_gate: usize,
}

/// Resample one volume's low-level (0–2 km ARL) cyclonic azimuthal shear onto
/// a radar-centered Cartesian grid (×10⁻³ s⁻¹; NaN = no coverage). Each cell
/// pull-samples its nearest gate on every contributing tilt and keeps the
/// maximum — one frame of the rotation-tracks accumulation.
pub fn low_level_azshear_cartesian(volume: &RadarVolume, spec: &TracksGridSpec) -> Vec<f32> {
    let fields = low_level_tilt_fields(volume);
    let size = spec.size();
    let mut out = vec![f32::NAN; spec.cell_count()];
    if fields.is_empty() {
        return out;
    }
    for row in 0..size {
        for column in 0..size {
            let (east, north) = spec.cell_center_km(column, row);
            let range_m = (east as f64).hypot(north as f64) * 1000.0;
            if range_m < TRACKS_MIN_RANGE_M {
                continue;
            }
            let az_deg = (east as f64).atan2(north as f64).to_degrees();
            let bin = ((az_deg.rem_euclid(360.0)) * (AZ_BINS as f64 / 360.0)) as usize % AZ_BINS;
            let mut best = f32::NAN;
            for field in &fields {
                let grid_row = field.az_row[bin];
                if grid_row == usize::MAX {
                    continue;
                }
                let gate = ((range_m - field.first_gate_m) / field.spacing_m).round();
                if gate < field.min_gate as f64 || gate > field.max_gate as f64 {
                    continue;
                }
                let value = field.shear[grid_row * field.gates + gate as usize];
                if value.is_finite() && (best.is_nan() || value > best) {
                    best = value;
                }
            }
            out[row * size + column] = best;
        }
    }
    out
}

/// Running max-composite: fold one frame into the accumulator. NaN cells in
/// the frame leave the accumulator untouched; finite values replace NaN or
/// smaller accumulated values (the rotation-tracks accumulation operator;
/// Miller et al. 2013).
pub fn max_composite_into(accumulator: &mut [f32], frame: &[f32]) {
    debug_assert_eq!(accumulator.len(), frame.len());
    for (acc, &value) in accumulator.iter_mut().zip(frame.iter()) {
        if value.is_finite() && (acc.is_nan() || value > *acc) {
            *acc = value;
        }
    }
}

/// Rotation-tracks display ramp (RGBA, non-premultiplied): transparent below
/// [`TRACK_DISPLAY_FLOOR_E3`], blue → yellow → red → magenta saturating at
/// [`TRACK_DISPLAY_CEIL_E3`] (0.003–0.02 s⁻¹).
pub fn rotation_track_color(shear_e3: f32) -> [u8; 4] {
    if !shear_e3.is_finite() || shear_e3 < TRACK_DISPLAY_FLOOR_E3 {
        return [0, 0, 0, 0];
    }
    // (threshold ×10⁻³ s⁻¹, r, g, b, a)
    const STOPS: [(f32, f32, f32, f32, f32); 4] = [
        (TRACK_DISPLAY_FLOOR_E3, 35.0, 70.0, 220.0, 150.0),
        (8.0, 255.0, 230.0, 70.0, 205.0),
        (14.0, 235.0, 35.0, 35.0, 235.0),
        (TRACK_DISPLAY_CEIL_E3, 255.0, 40.0, 255.0, 255.0),
    ];
    let last = STOPS[STOPS.len() - 1];
    if shear_e3 >= last.0 {
        return [last.1 as u8, last.2 as u8, last.3 as u8, last.4 as u8];
    }
    for pair in STOPS.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        if shear_e3 < hi.0 {
            let t = ((shear_e3 - lo.0) / (hi.0 - lo.0)).clamp(0.0, 1.0);
            let lerp = |a: f32, b: f32| (a + (b - a) * t).round() as u8;
            return [
                lerp(lo.1, hi.1),
                lerp(lo.2, hi.2),
                lerp(lo.3, hi.3),
                lerp(lo.4, hi.4),
            ];
        }
    }
    [last.1 as u8, last.2 as u8, last.3 as u8, last.4 as u8]
}

/// One TDS-flagged gate (planar ENU km about the radar).
#[derive(Clone, Copy, Debug)]
pub struct TdsGate {
    pub east_km: f32,
    pub north_km: f32,
    pub cc: f32,
    pub dbz: f32,
}

/// The per-gate dual-pol debris criterion (Ryzhkov et al. 2005; Van Den
/// Broeke & Jauernic 2014): ρhv < 0.82 inside > 30 dBZ echo. Co-location
/// with a significant circulation is enforced separately in
/// [`detect_tds_gates`].
pub fn tds_gate_criteria(cc: f32, dbz: f32) -> bool {
    cc.is_finite() && dbz.is_finite() && cc < TDS_CC_MAX && dbz > TDS_MIN_DBZ
}

/// Whether a detected circulation can anchor TDS gates: 3-D rank ≥ 3
/// (moderate circulation) or TVS — the "co-located significant azimuthal
/// shear" requirement of the TDS literature, reusing the MDA/TDA port of
/// [`crate::detect`].
pub fn tds_anchor(site: &RotationSite) -> bool {
    site.strength == RotationStrength::Tvs || site.rank >= TDS_ANCHOR_MIN_RANK
}

/// Per-gate TDS flags on the lowest dual-pol tilt: every gate within
/// [`TDS_ANCHOR_RADIUS_KM`] of an anchoring circulation that satisfies
/// [`tds_gate_criteria`]. Deterministic physics flag — never a probability.
pub fn detect_tds_gates(volume: &RadarVolume, sites: &[RotationSite]) -> Vec<TdsGate> {
    let anchors: Vec<(f64, f64)> = sites
        .iter()
        .filter(|site| tds_anchor(site))
        .map(|site| {
            let az = (site.azimuth_deg as f64).to_radians();
            let range_km = site.ground_range_m / 1000.0;
            (range_km * az.sin(), range_km * az.cos())
        })
        .collect();
    if anchors.is_empty() {
        return Vec::new();
    }

    // Lowest tilt carrying both ρhv and Z (the split-cut surveillance tilt).
    let Some((cut_index, _)) = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| {
            cut.elevation_deg <= LOW_LEVEL_MAX_TILT_DEG
                && cut
                    .moments
                    .contains_key(&MomentType::CorrelationCoefficient)
                && cut.moments.contains_key(&MomentType::Reflectivity)
        })
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
    else {
        return Vec::new();
    };
    let cut = &volume.cuts[cut_index];
    let Some(cc_grid) = cut.moments.get(&MomentType::CorrelationCoefficient) else {
        return Vec::new();
    };
    let Some(ref_grid) = cut.moments.get(&MomentType::Reflectivity) else {
        return Vec::new();
    };
    let ref_row_by_radial = row_by_radial(ref_grid, cut.radials.len());
    let elevation = cut.elevation_deg as f64;
    let spacing_m = cc_grid.gate_range.gate_spacing_m.max(1) as f64;
    let first_gate_m = cc_grid.gate_range.first_gate_m as f64;
    let gate_count = cc_grid.gate_range.gate_count;

    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut out = Vec::new();
    'rows: for (row, &radial_index) in cc_grid.radial_indices.iter().enumerate() {
        let Some(radial) = cut.radials.get(radial_index) else {
            continue;
        };
        let az = (radial.azimuth_deg as f64).to_radians();
        let (ux, uy) = (az.sin(), az.cos());
        for &(anchor_east, anchor_north) in &anchors {
            // Closest approach of this ray to the anchor; the ray intersects
            // the 5-km disc over [t* − s, t* + s].
            let along_km = anchor_east * ux + anchor_north * uy;
            let perp_km = (anchor_east * uy - anchor_north * ux).abs();
            if along_km <= 0.0 || perp_km > TDS_ANCHOR_RADIUS_KM {
                continue;
            }
            let half_span_km =
                (TDS_ANCHOR_RADIUS_KM * TDS_ANCHOR_RADIUS_KM - perp_km * perp_km).sqrt();
            let range_lo_m = ((along_km - half_span_km) * 1000.0).max(TRACKS_MIN_RANGE_M);
            let range_hi_m = (along_km + half_span_km) * 1000.0;
            let gate_lo = ((range_lo_m - first_gate_m) / spacing_m).ceil().max(0.0) as usize;
            let gate_hi = ((range_hi_m - first_gate_m) / spacing_m).floor();
            if gate_hi < 0.0 {
                continue;
            }
            let gate_hi = (gate_hi as usize).min(gate_count.saturating_sub(1));
            for gate in gate_lo..=gate_hi {
                if seen.contains(&(row, gate)) {
                    continue;
                }
                let range_m = first_gate_m + gate as f64 * spacing_m;
                if beam_height_above_radar_m(range_m, elevation) > TDS_MAX_BEAM_HEIGHT_M {
                    break;
                }
                let Some(cc) = cc_grid.scaled_value(row, gate).filter(|v| v.is_finite()) else {
                    continue;
                };
                let dbz =
                    sample_by_radial_range(ref_grid, &ref_row_by_radial, radial_index, range_m);
                let Some(dbz) = dbz else {
                    continue;
                };
                if !tds_gate_criteria(cc, dbz) {
                    continue;
                }
                seen.insert((row, gate));
                out.push(TdsGate {
                    east_km: (range_m / 1000.0 * az.sin()) as f32,
                    north_km: (range_m / 1000.0 * az.cos()) as f32,
                    cc,
                    dbz,
                });
                if out.len() >= MAX_TDS_GATES {
                    break 'rows;
                }
            }
        }
    }
    out
}

/// Build the QC'd per-tilt shear fields feeding the Cartesian resample.
fn low_level_tilt_fields(volume: &RadarVolume) -> Vec<TiltShearField> {
    let mut velocity_cuts: Vec<usize> = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| {
            cut.moments.contains_key(&MomentType::Velocity)
                && cut.elevation_deg <= LOW_LEVEL_MAX_TILT_DEG
        })
        .map(|(index, _)| index)
        .collect();
    velocity_cuts.sort_by(|a, b| {
        volume.cuts[*a]
            .elevation_deg
            .total_cmp(&volume.cuts[*b].elevation_deg)
    });
    velocity_cuts.truncate(MAX_LOW_TILTS);

    let mut fields = Vec::new();
    for cut_index in velocity_cuts {
        let cut = &volume.cuts[cut_index];
        let Some(velocity) = cut.moments.get(&MomentType::Velocity) else {
            continue;
        };
        let shear = azimuthal_shear_grid(cut, velocity);
        let rows = shear.radial_count();
        let gates = shear.gate_range.gate_count;
        if rows == 0 || gates == 0 {
            continue;
        }
        let spacing_m = shear.gate_range.gate_spacing_m.max(1) as f64;
        let first_gate_m = shear.gate_range.first_gate_m as f64;
        let elevation = cut.elevation_deg as f64;

        // Range window: clutter floor up to where the beam exits 0–2 km.
        let min_gate = ((TRACKS_MIN_RANGE_M - first_gate_m) / spacing_m)
            .ceil()
            .max(0.0) as usize;
        let mut max_gate = None;
        for gate in (min_gate..gates).rev() {
            let range_m = first_gate_m + gate as f64 * spacing_m;
            if beam_height_above_radar_m(range_m, elevation) <= LOW_LEVEL_TOP_M {
                max_gate = Some(gate);
                break;
            }
        }
        let Some(max_gate) = max_gate else {
            continue;
        };
        if min_gate > max_gate {
            continue;
        }

        let reflectivity = cut.moments.get(&MomentType::Reflectivity);
        let ref_row_by_radial = reflectivity
            .map(|grid| row_by_radial(grid, cut.radials.len()))
            .unwrap_or_default();

        let shear_at = |row: usize, gate: usize| -> Option<f32> {
            shear.scaled_value(row, gate).filter(|v| v.is_finite())
        };
        let mut qc = vec![f32::NAN; rows * gates];
        for row in 0..rows {
            for gate in min_gate..=max_gate {
                let Some(value) = shear_at(row, gate) else {
                    continue;
                };
                // Cyclonic only; clutter-residue cap.
                if value > MAX_PLAUSIBLE_SHEAR_E3 {
                    continue;
                }
                if value < 0.0 {
                    qc[row * gates + gate] = 0.0;
                    continue;
                }
                // Reflectivity QC: shear only accumulates inside real echo.
                let range_m = first_gate_m + gate as f64 * spacing_m;
                let dbz = reflectivity.and_then(|grid| {
                    let radial_index = *shear.radial_indices.get(row)?;
                    sample_by_radial_range(grid, &ref_row_by_radial, radial_index, range_m)
                });
                if !dbz.is_some_and(|v| v >= TRACKS_REFLECTIVITY_FLOOR_DBZ) {
                    continue;
                }
                // Impulse rejection: cap at the 3×3 neighborhood median. A
                // real couplet's LLSD ridge is spatially coherent (the LLSD
                // window itself smooths it), so min(value, median) preserves
                // swaths while 1–2-gate dealias spikes collapse to their
                // quiet surroundings.
                let mut neighborhood = [0.0f32; 9];
                let mut count = 0usize;
                for dr in -1i64..=1 {
                    for dg in -1i64..=1 {
                        let r = ((row as i64 + dr).rem_euclid(rows as i64)) as usize;
                        let g = gate as i64 + dg;
                        if g < 0 || g >= gates as i64 {
                            continue;
                        }
                        if let Some(v) = shear_at(r, g as usize) {
                            neighborhood[count] = v;
                            count += 1;
                        }
                    }
                }
                if count < MEDIAN_CAP_MIN_NEIGHBORS {
                    continue;
                }
                neighborhood[..count].sort_by(f32::total_cmp);
                qc[row * gates + gate] = value.min(neighborhood[count / 2].max(0.0));
            }
        }

        // Azimuth → row lookup with nearest-fill (the detect.rs pattern).
        let mut az_row = vec![usize::MAX; AZ_BINS];
        for (row, &radial_index) in shear.radial_indices.iter().enumerate() {
            if let Some(radial) = cut.radials.get(radial_index) {
                let bin = ((radial.azimuth_deg.rem_euclid(360.0)) * (AZ_BINS as f32 / 360.0))
                    as usize
                    % AZ_BINS;
                az_row[bin] = row;
            }
        }
        let filled: Vec<usize> = (0..AZ_BINS)
            .map(|bin| {
                (0..4)
                    .flat_map(|step| [(bin + step) % AZ_BINS, (bin + AZ_BINS - step) % AZ_BINS])
                    .map(|b| az_row[b])
                    .find(|&row| row != usize::MAX)
                    .unwrap_or(usize::MAX)
            })
            .collect();

        fields.push(TiltShearField {
            shear: qc,
            gates,
            first_gate_m,
            spacing_m,
            az_row: filled,
            min_gate,
            max_gate,
        });
    }
    fields
}

/// Map radial index → grid row for grids whose row order differs (the REF
/// alignment fix from [`crate::detect`]).
fn row_by_radial(grid: &MomentGrid, radial_count: usize) -> Vec<usize> {
    let mut map = vec![usize::MAX; radial_count];
    for (row, &radial_index) in grid.radial_indices.iter().enumerate() {
        if let Some(slot) = map.get_mut(radial_index) {
            *slot = row;
        }
    }
    map
}

fn sample_by_radial_range(
    grid: &MomentGrid,
    row_by_radial: &[usize],
    radial_index: usize,
    range_m: f64,
) -> Option<f32> {
    let row = *row_by_radial.get(radial_index)?;
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
    use radar_core::{ElevationCut, GateRange, MomentStorage, Radial};

    #[test]
    fn max_composite_keeps_running_maximum() {
        let mut acc = vec![f32::NAN, 2.0, 5.0, f32::NAN];
        max_composite_into(&mut acc, &[1.0, f32::NAN, 3.0, f32::NAN]);
        assert_eq!(acc[0], 1.0, "finite frame value replaces NaN");
        assert_eq!(acc[1], 2.0, "NaN frame cell leaves accumulator");
        assert_eq!(acc[2], 5.0, "smaller frame value never lowers the max");
        assert!(acc[3].is_nan(), "NaN over NaN stays NaN");
        max_composite_into(&mut acc, &[4.0, 9.0, 6.0, 0.0]);
        assert_eq!(acc, vec![4.0, 9.0, 6.0, 0.0]);
    }

    #[test]
    fn grid_spec_round_trips_cells() {
        let spec = TracksGridSpec {
            half_extent_km: 30.0,
            cell_km: 0.5,
        };
        assert_eq!(spec.size(), 120);
        for &(column, row) in &[(0usize, 0usize), (59, 60), (119, 119), (3, 117)] {
            let (east, north) = spec.cell_center_km(column, row);
            assert_eq!(
                spec.cell_index(east, north),
                Some(row * spec.size() + column),
                "cell ({column},{row}) center must map back to itself"
            );
        }
        // A gate at az 90°, 20 km lands in the cell containing (east 20, north 0).
        let (az, range_km) = (90.0f64.to_radians(), 20.0f64);
        let east = (range_km * az.sin()) as f32;
        let north = (range_km * az.cos()) as f32;
        let index = spec.cell_index(east, north).expect("inside grid");
        let (ce, cn) = spec.cell_center_km(index % spec.size(), index / spec.size());
        assert!((ce - east).abs() <= spec.cell_km * 0.5 + 1e-4);
        assert!((cn - north).abs() <= spec.cell_km * 0.5 + 1e-4);
        // Outside the square → None.
        assert_eq!(spec.cell_index(30.4, 0.0), None);
        assert_eq!(spec.cell_index(0.0, -30.4), None);
    }

    #[test]
    fn tds_criteria_thresholds() {
        assert!(tds_gate_criteria(0.70, 45.0), "classic debris values flag");
        assert!(!tds_gate_criteria(0.95, 45.0), "rain-grade ρhv never flags");
        assert!(!tds_gate_criteria(0.70, 12.0), "weak echo never flags");
        assert!(!tds_gate_criteria(f32::NAN, 45.0));
        assert!(!tds_gate_criteria(0.70, f32::NAN));
        assert!(tds_gate_criteria(TDS_CC_MAX - 0.001, TDS_MIN_DBZ + 0.1));
        assert!(!tds_gate_criteria(TDS_CC_MAX, TDS_MIN_DBZ + 0.1));
        assert!(!tds_gate_criteria(TDS_CC_MAX - 0.001, TDS_MIN_DBZ));
    }

    // ---- synthetic-volume helpers (the detect.rs test pattern) ----

    fn gate_range(gates: usize) -> GateRange {
        GateRange {
            first_gate_m: 250,
            gate_spacing_m: 250,
            gate_count: gates,
        }
    }

    fn f32_grid(moment: MomentType, range: &GateRange, rows: usize, data: Vec<f32>) -> MomentGrid {
        MomentGrid {
            moment,
            gate_range: range.clone(),
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..rows).collect(),
            storage: MomentStorage::F32(data),
        }
    }

    fn full_circle_cut(elevation: f32, rows: usize, gates: usize) -> ElevationCut {
        let range = gate_range(gates);
        let mut cut = ElevationCut::new(elevation, None);
        for r in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: r as f32 * (360.0 / rows as f32),
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: range.clone(),
                nyquist_velocity_mps: Some(60.0),
                radial_status: None,
            });
        }
        cut
    }

    /// Full-circle tilt with a cyclonic Rankine-style couplet centered at
    /// az ~90° and the given gate span, inside 50 dBZ echo.
    fn couplet_tilt(
        elevation: f32,
        gates: usize,
        couplet_gates: std::ops::Range<usize>,
    ) -> ElevationCut {
        let rows = 720usize;
        let mut cut = full_circle_cut(elevation, rows, gates);
        let range = gate_range(gates);
        let mut velocity = vec![0.0f32; rows * gates];
        for row in 166..=198usize {
            let v = if row < 174 {
                -25.0
            } else if row <= 186 {
                -25.0 + 50.0 * (row - 174) as f32 / 12.0
            } else {
                25.0
            };
            for gate in couplet_gates.clone() {
                velocity[row * gates + gate] = v;
            }
        }
        cut.moments.insert(
            MomentType::Velocity,
            f32_grid(MomentType::Velocity, &range, rows, velocity),
        );
        let mut dbz = vec![f32::NAN; rows * gates];
        for row in 150..210 {
            for gate in 0..gates {
                dbz[row * gates + gate] = 50.0;
            }
        }
        cut.moments.insert(
            MomentType::Reflectivity,
            f32_grid(MomentType::Reflectivity, &range, rows, dbz),
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

    /// The couplet sits at az 90°, gates 80..100 (20–25 km). The Cartesian
    /// frame must carry display-strength shear at that ENU location, nothing
    /// on the opposite side, and NaN inside the clutter floor.
    #[test]
    fn cartesian_frame_paints_couplet_location() {
        let volume = volume_of(vec![couplet_tilt(0.5, 200, 80..100)]);
        let spec = TracksGridSpec {
            half_extent_km: 50.0,
            cell_km: 0.5,
        };
        let frame = low_level_azshear_cartesian(&volume, &spec);
        let at = |east: f32, north: f32| -> f32 {
            frame[spec.cell_index(east, north).expect("in grid")]
        };
        // az 90°, 22 km → ENU (22, 0).
        let couplet = at(22.0, 0.0);
        assert!(
            couplet.is_finite() && couplet >= TRACK_DISPLAY_FLOOR_E3,
            "display-strength shear at the couplet: {couplet}"
        );
        // Opposite azimuth: data (velocity 0 everywhere) but no echo → the
        // reflectivity QC masks it to NaN.
        assert!(
            at(-22.0, 0.0).is_nan(),
            "no-echo side must be masked by the 20 dBZ floor"
        );
        // Inside the clutter floor: no accumulation ever.
        assert!(at(2.0, 2.0).is_nan(), "sub-5-km cells stay NaN");
        // Outside the echo sector entirely (az 180°): masked → NaN.
        assert!(
            at(0.0, -35.0).is_nan(),
            "az 180 has no echo (echo sector is az 75°–105°)"
        );
        // Inside the echo sector but away from the couplet: zero signal,
        // not NaN (data present, no rotation).
        let echo_no_rotation = at(30.0, -6.0); // az ≈ 101°, r ≈ 30.6 km
        assert!(
            echo_no_rotation.is_finite() && echo_no_rotation < TRACK_DISPLAY_FLOOR_E3,
            "in-echo, no-rotation cells read ~0: {echo_no_rotation}"
        );
    }

    /// 0.5° beam exits the 0–2 km layer near ~125 km: far cells stay NaN even
    /// where gates exist (the documented single-radar range bound).
    #[test]
    fn height_cap_bounds_range_coverage() {
        // Couplet at gates 430..450 (~108–113 km) and echo everywhere along
        // the couplet rows out to 150 km.
        let volume = volume_of(vec![couplet_tilt(0.5, 600, 430..450)]);
        let spec = TracksGridSpec {
            half_extent_km: 150.0,
            cell_km: 1.0,
        };
        let frame = low_level_azshear_cartesian(&volume, &spec);
        let at = |east: f32, north: f32| -> f32 {
            frame[spec.cell_index(east, north).expect("in grid")]
        };
        let near = at(110.0, 0.0);
        assert!(
            near.is_finite() && near >= TRACK_DISPLAY_FLOOR_E3,
            "couplet at 110 km (beam ~1.7 km ARL) accumulates: {near}"
        );
        assert!(
            at(140.0, 0.0).is_nan(),
            "beyond ~125 km the 0.5° beam is above 2 km ARL — must stay NaN"
        );
    }

    #[test]
    fn rotation_track_ramp_endpoints() {
        assert_eq!(rotation_track_color(f32::NAN)[3], 0);
        assert_eq!(rotation_track_color(0.0)[3], 0);
        assert_eq!(rotation_track_color(TRACK_DISPLAY_FLOOR_E3 - 0.01)[3], 0);
        let floor = rotation_track_color(TRACK_DISPLAY_FLOOR_E3);
        assert_eq!(floor, [35, 70, 220, 150], "floor is translucent blue");
        let ceil = rotation_track_color(TRACK_DISPLAY_CEIL_E3);
        assert_eq!(ceil, [255, 40, 255, 255], "ceiling is opaque magenta");
        assert_eq!(
            rotation_track_color(99.0),
            ceil,
            "values above the ceiling clamp"
        );
        // Monotone alpha across the ramp.
        let mut last_alpha = 0u8;
        for i in 0..=40 {
            let v = TRACK_DISPLAY_FLOOR_E3
                + (TRACK_DISPLAY_CEIL_E3 - TRACK_DISPLAY_FLOOR_E3) * (i as f32 / 40.0);
            let alpha = rotation_track_color(v)[3];
            assert!(
                alpha >= last_alpha,
                "alpha must not decrease along the ramp"
            );
            last_alpha = alpha;
        }
    }

    /// TDS gates flag only where the dual-pol criteria hold AND the gate is
    /// within 5 km of a rank-significant circulation.
    #[test]
    fn tds_gates_require_anchor_proximity_and_criteria() {
        let rows = 720usize;
        let gates = 200usize;
        let range = gate_range(gates);
        let mut cut = full_circle_cut(0.5, rows, gates);
        // 50 dBZ echo everywhere; ρhv 0.99 except a debris patch at
        // az 88°–92° (rows 176..184), gates 76..84 (~19–21 km) and a far
        // low-CC patch at az 268°–272° with NO circulation nearby.
        let dbz = vec![50.0f32; rows * gates];
        let mut cc = vec![0.99f32; rows * gates];
        for row in 176..184 {
            for gate in 76..84 {
                cc[row * gates + gate] = 0.55;
            }
        }
        for row in 536..544 {
            for gate in 76..84 {
                cc[row * gates + gate] = 0.55;
            }
        }
        cut.moments.insert(
            MomentType::Reflectivity,
            f32_grid(MomentType::Reflectivity, &range, rows, dbz),
        );
        cut.moments.insert(
            MomentType::CorrelationCoefficient,
            f32_grid(MomentType::CorrelationCoefficient, &range, rows, cc),
        );
        let volume = volume_of(vec![cut]);
        let anchor = RotationSite {
            azimuth_deg: 90.0,
            ground_range_m: 20_000.0,
            vrot_mps: 25.0,
            gate_to_gate_dv_mps: 30.0,
            rank: 5,
            depth_tilts: 3,
            depth_m: 4_000.0,
            base_elevation_deg: 0.5,
            strength: RotationStrength::Mesocyclone,
        };
        let flagged = detect_tds_gates(&volume, &[anchor]);
        assert!(
            !flagged.is_empty(),
            "debris patch beside the meso must flag"
        );
        for gate in &flagged {
            let distance = ((gate.east_km - 20.0).powi(2) + gate.north_km.powi(2)).sqrt() as f64;
            assert!(
                distance <= TDS_ANCHOR_RADIUS_KM + 0.3,
                "all flags within the anchor radius: {distance:.2} km"
            );
            assert!(gate.cc < TDS_CC_MAX && gate.dbz > TDS_MIN_DBZ);
            assert!(
                gate.east_km > 0.0,
                "the az-270 patch (west) has no anchor and must not flag"
            );
        }

        // Weak anchor (rank 1, not TVS) → nothing flags.
        let weak = RotationSite {
            rank: 1,
            strength: RotationStrength::WeakCirculation,
            ..anchor
        };
        assert!(
            detect_tds_gates(&volume, &[weak]).is_empty(),
            "rank-insignificant circulations never anchor TDS"
        );
        // No anchors at all → nothing flags.
        assert!(detect_tds_gates(&volume, &[]).is_empty());
    }
}
