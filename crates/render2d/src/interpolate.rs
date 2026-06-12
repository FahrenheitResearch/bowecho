//! Inter-gate display interpolation: bilinear upsampling of a moment grid on
//! the polar lattice (azimuth × range). The pass runs ONCE per
//! volume/cut/product on the render worker (cached exactly like the binomial
//! Soften pass in `smooth.rs`) and the finer grid is drawn through the
//! unchanged nearest-gate fast path — pans stay full speed; only the cached
//! grid is bigger.
//!
//! Technique: standard separable bilinear resampling, applied on the polar
//! grid rather than the screen raster — the same "smoothed display" approach
//! popularized by GR2Analyst-class viewers (literally "adds grid cells in
//! between the scan lines"). No novel science. The per-moment-family guards
//! mirror `volumetric.rs` (`InterpPolicy`): correlation coefficient never
//! blends through a rho_hv minimum (Giangrande, Krause & Ryzhkov 2008 — the
//! minimum IS the melting-layer signature), and velocity never blends across
//! a spread larger than the volumetric module's 30 m/s guard (interpolating
//! across an aliasing fold or a couplet fabricates intermediate values).
//!
//! ## Upsample policy (input geometry → factors)
//!
//! Targets ≤ 0.25° azimuth and ≤ 250 m gates, capped at 4× per axis and a
//! 64 MB F32 grid budget. Coarse grids are upsampled aggressively, fine grids
//! mildly or not at all:
//!
//! | native cut                       | factors (az × rng) | result          |
//! |----------------------------------|--------------------|-----------------|
//! | 1.0° × 1000 m (legacy/intl)      | 4 × 4              | 0.25° × 250 m   |
//! | 1.0° × 500 m (European C-band)   | 4 × 2              | 0.25° × 250 m   |
//! | 1.0° × 250 m                     | 4 × 1              | 0.25° × 250 m   |
//! | 0.5° × 250 m (NEXRAD super-res)  | 2 × 1              | 0.25° × 250 m   |
//! | ≤ 0.25° × ≤ 250 m                | 1 × 1              | native (no pass)|
//!
//! Range factors are additionally constrained to keep the integer-meter gate
//! geometry exact (sub-spacing must divide evenly and the half-cell shift
//! must be a whole meter); failing factors step down. Row/gate counts are
//! clamped to the packed sample encoding's limits.
//!
//! ## Coverage discipline
//!
//! Interpolation must NOT grow echo coverage: a sub-cell renders only where
//! the native display would. Each sub-cell lies inside exactly one native
//! cell (its nearest parent); if that parent is missing/RF the sub-cell
//! stays empty, and if any of the four bilinear parents is missing the
//! sub-cell takes the nearest parent's value instead of a partial blend.
//! Sub-rows exactly on a native beam boundary (t = 0.5) render only where
//! BOTH bracketing beams carry echo — otherwise one side's echo would
//! reach half a sub-beam past the midpoint the native display stops at.
//! Azimuth wraps; range clamps. Like the Soften pass, RF gates render
//! transparent (the native display is the place to read the RF purple).
//! Sub-rows are synthesized only between beams whose azimuth gap is small
//! (sector-scan edges keep their native hole).

use crate::InterpPolicy;
use radar_core::{ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType};
use rayon::prelude::*;

/// Display target: no coarser than 0.25° between rendered radials.
pub const INTERP_TARGET_AZIMUTH_DEG: f32 = 0.25;
/// Display target: no coarser than 250 m between rendered gates.
pub const INTERP_TARGET_GATE_SPACING_M: i32 = 250;
/// Hard cap per axis — beyond 4× the cost outruns the visual return.
pub const INTERP_MAX_FACTOR: usize = 4;
/// Budget for the cached F32 grid (the moment-cache entry that holds it).
pub const INTERP_MAX_GRID_BYTES: usize = 64 << 20;

/// Same threshold as `volumetric.rs`'s `InterpPolicy::VelocityGuard`.
const VELOCITY_GUARD_SPREAD_MPS: f32 = 30.0;
/// Same floor as `volumetric.rs`'s `InterpPolicy::CcGuard`.
const CC_GUARD_FLOOR: f32 = 0.97;
/// Beams closer than this are duplicates — nothing to synthesize between.
const MIN_SYNTH_DELTA_DEG: f32 = 0.01;

/// A moment grid upsampled for display plus the per-row azimuths the
/// synthetic rows render at (native rows keep their exact beam azimuth).
pub struct InterpolatedGrid {
    pub grid: MomentGrid,
    pub row_azimuths_deg: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpsampleFactors {
    pub azimuth: usize,
    pub range: usize,
}

impl UpsampleFactors {
    pub fn is_identity(self) -> bool {
        self.azimuth <= 1 && self.range <= 1
    }
}

/// Sub-spacing must stay whole meters and the cell-centered half-shift
/// `(spacing - spacing/factor) / 2` must be a whole meter too, so the
/// upsampled annulus is EXACTLY the native one.
fn range_factor_is_exact(spacing_m: i32, factor: usize) -> bool {
    let factor = factor as i32;
    factor > 0 && spacing_m % factor == 0 && (spacing_m - spacing_m / factor) % 2 == 0
}

/// Pick adaptive upsample factors for a cut's nominal geometry (see the
/// module-level policy table).
pub fn upsample_factors(
    nominal_azimuth_deg: f32,
    gate_spacing_m: i32,
    rows: usize,
    gates: usize,
) -> UpsampleFactors {
    let mut azimuth = 1usize;
    if nominal_azimuth_deg.is_finite() && nominal_azimuth_deg > 0.0 {
        while azimuth < INTERP_MAX_FACTOR
            && nominal_azimuth_deg / azimuth as f32 > INTERP_TARGET_AZIMUTH_DEG + 1e-3
        {
            azimuth += 1;
        }
    }
    let mut range = 1usize;
    if gate_spacing_m > 0 {
        while range < INTERP_MAX_FACTOR
            && gate_spacing_m as f32 / range as f32 > INTERP_TARGET_GATE_SPACING_M as f32
        {
            range += 1;
        }
        while range > 1 && !range_factor_is_exact(gate_spacing_m, range) {
            range -= 1;
        }
    }
    // The fast path packs (row, gate) into 31 bits — stay inside it.
    while azimuth > 1 && rows.saturating_mul(azimuth) >= crate::CachedSample::ROW_LIMIT {
        azimuth -= 1;
    }
    while range > 1 && gates.saturating_mul(range) > crate::CachedSample::GATE_MASK as usize {
        range -= 1;
    }
    // Memory budget for the cached F32 grid. Range steps down first (gates
    // are usually already the finer axis; the azimuth seams are what the
    // interpolated mode exists to fill).
    while rows
        .saturating_mul(azimuth)
        .saturating_mul(gates)
        .saturating_mul(range)
        .saturating_mul(std::mem::size_of::<f32>())
        > INTERP_MAX_GRID_BYTES
    {
        if range > 1 {
            range -= 1;
        } else if azimuth > 1 {
            azimuth -= 1;
        } else {
            break;
        }
    }
    UpsampleFactors { azimuth, range }
}

/// Per-moment-family interpolation policy, mirroring the volumetric module's
/// cross-section policies (`volumetric.rs`): velocity guards against
/// blending across folds/couplets, CC against blending through the melting
/// layer; everything else (REF/ZDR/SW/PHI/KDP) blends linearly.
fn interp_policy_for_moment(moment: &MomentType) -> InterpPolicy {
    match moment {
        MomentType::Velocity => InterpPolicy::VelocityGuard,
        MomentType::CorrelationCoefficient => InterpPolicy::CcGuard,
        _ => InterpPolicy::LinearAngle,
    }
}

/// Shortest signed angular step from `from` to `to`, in (-180, 180].
fn signed_delta_deg(from_deg: f32, to_deg: f32) -> f32 {
    let delta = (to_deg - from_deg).rem_euclid(360.0);
    if delta > 180.0 { delta - 360.0 } else { delta }
}

/// One output row's parents: bilinear weight `t` between scan-order rows
/// `lo` and `hi` (t = 0 reproduces the native row `lo` exactly).
struct RowPlan {
    lo: usize,
    hi: usize,
    t: f32,
    azimuth_deg: f32,
}

/// One output gate's parents: weight `u` between native gate centers `lo`
/// and `hi`; `nearest` is the native gate whose cell contains the sub-gate
/// (the coverage authority).
struct GatePlan {
    lo: usize,
    hi: usize,
    u: f32,
    nearest: usize,
}

/// Upsample a moment grid for display. Returns `None` when the grid is
/// already at/finer than the display targets (callers fall back to the
/// native path), when the cut has too few radials, or when the radial
/// linkage is broken.
pub fn upsample_moment_grid(cut: &ElevationCut, grid: &MomentGrid) -> Option<InterpolatedGrid> {
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    if rows < 2 || gates == 0 {
        return None;
    }
    let mut azimuths = Vec::with_capacity(rows);
    for radial_index in &grid.radial_indices {
        azimuths.push(
            cut.radials
                .get(*radial_index)?
                .azimuth_deg
                .rem_euclid(360.0),
        );
    }
    // Scan-order azimuth steps (signed shortest; handles CW and CCW sweeps
    // and the wrap pair alike). The median is the nominal beam spacing —
    // robust to a sector scan's single large wrap gap.
    let deltas: Vec<f32> = (0..rows)
        .map(|row| signed_delta_deg(azimuths[row], azimuths[(row + 1) % rows]))
        .collect();
    let mut magnitudes: Vec<f32> = deltas
        .iter()
        .map(|delta| delta.abs())
        .filter(|delta| delta.is_finite())
        .collect();
    if magnitudes.is_empty() {
        return None;
    }
    magnitudes.sort_by(f32::total_cmp);
    let nominal_deg = magnitudes[magnitudes.len() / 2];
    let factors = upsample_factors(nominal_deg, grid.gate_range.gate_spacing_m, rows, gates);
    if factors.is_identity() {
        return None;
    }

    // Sub-rows only between beams whose gap is believably adjacent: wider
    // gaps (sector-scan edges, dropped radials) keep their native hole so
    // azimuth coverage cannot grow. Native bins fill at most
    // MAX_AZIMUTH_HALF_WIDTH_DEG to each side, hence the absolute cap.
    let gap_limit_deg = (nominal_deg * 2.0).min(crate::MAX_AZIMUTH_HALF_WIDTH_DEG * 2.0);
    let mut row_plan = Vec::with_capacity(rows * factors.azimuth);
    for row in 0..rows {
        row_plan.push(RowPlan {
            lo: row,
            hi: row,
            t: 0.0,
            azimuth_deg: azimuths[row],
        });
        let delta = deltas[row];
        if factors.azimuth > 1 && delta.abs() >= MIN_SYNTH_DELTA_DEG && delta.abs() <= gap_limit_deg
        {
            let hi = (row + 1) % rows;
            for step in 1..factors.azimuth {
                let t = step as f32 / factors.azimuth as f32;
                row_plan.push(RowPlan {
                    lo: row,
                    hi,
                    t,
                    azimuth_deg: (azimuths[row] + t * delta).rem_euclid(360.0),
                });
            }
        }
    }

    // Cell-centered range subdivision: native gate g's cell splits into R
    // sub-cells whose centers interpolate between the surrounding native
    // gate CENTERS; ends clamp. first_gate_m is a gate center
    // (gate = round((range - first)/spacing) in the fast path), so
    // new_first = first + (sub - spacing)/2 keeps the rendered annulus
    // [first - spacing/2, first + (count - 0.5)·spacing) EXACTLY.
    let range_factor = factors.range;
    let new_gates = gates * range_factor;
    let mut gate_plan = Vec::with_capacity(new_gates);
    for sub_gate in 0..new_gates {
        let x = (sub_gate as f32 + 0.5) / range_factor as f32 - 0.5;
        let nearest = sub_gate / range_factor;
        let (lo, hi, u) = if x <= 0.0 {
            (0, 0, 0.0)
        } else if x >= (gates - 1) as f32 {
            (gates - 1, gates - 1, 0.0)
        } else {
            let lo = x.floor() as usize;
            (lo, lo + 1, x - lo as f32)
        };
        gate_plan.push(GatePlan { lo, hi, u, nearest });
    }

    // Materialize scaled values once (NaN for missing/RF), as in smooth.rs.
    let mut source = vec![f32::NAN; rows * gates];
    source
        .par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, out_row)| {
            for (gate, cell) in out_row.iter_mut().enumerate() {
                if let Some(value) = grid
                    .scaled_value(row, gate)
                    .filter(|value| value.is_finite())
                {
                    *cell = value;
                }
            }
        });

    let policy = interp_policy_for_moment(&grid.moment);
    let mut values = vec![f32::NAN; row_plan.len() * new_gates];
    values
        .par_chunks_mut(new_gates)
        .zip(row_plan.par_iter())
        .for_each(|(out_row, plan)| {
            let row_lo = &source[plan.lo * gates..(plan.lo + 1) * gates];
            let row_hi = &source[plan.hi * gates..(plan.hi + 1) * gates];
            let nearest_row = if plan.t <= 0.5 { row_lo } else { row_hi };
            // A sub-row exactly on the native beam boundary (t = 0.5)
            // belongs to NEITHER beam: painting it from one side would
            // push that side's echo half a sub-beam past the midpoint the
            // native display stops at. It renders only where both
            // bracketing beams have echo (strict no-growth; at worst the
            // boundary row goes empty at an echo's azimuth edge).
            let on_beam_boundary = plan.t == 0.5;
            for (cell, gate) in out_row.iter_mut().zip(&gate_plan) {
                let nearest = nearest_row[gate.nearest];
                if !nearest.is_finite() {
                    // The native cell containing this sub-cell is empty —
                    // it stays empty (coverage never grows).
                    continue;
                }
                if on_beam_boundary && !row_hi[gate.nearest].is_finite() {
                    continue;
                }
                let v00 = row_lo[gate.lo];
                let v01 = row_lo[gate.hi];
                let v10 = row_hi[gate.lo];
                let v11 = row_hi[gate.hi];
                if !(v00.is_finite() && v01.is_finite() && v10.is_finite() && v11.is_finite()) {
                    // An echo edge: no partial blends, the nearest parent's
                    // value carries through unchanged.
                    *cell = nearest;
                    continue;
                }
                let min = v00.min(v01).min(v10).min(v11);
                let max = v00.max(v01).max(v10).max(v11);
                let guarded = match policy {
                    InterpPolicy::CcGuard => min < CC_GUARD_FLOOR,
                    InterpPolicy::VelocityGuard => max - min > VELOCITY_GUARD_SPREAD_MPS,
                    InterpPolicy::LinearAngle => false,
                };
                *cell = if guarded {
                    nearest
                } else {
                    let lo = v00 + (v01 - v00) * gate.u;
                    let hi = v10 + (v11 - v10) * gate.u;
                    lo + (hi - lo) * plan.t
                };
            }
        });

    let sub_spacing_m = grid.gate_range.gate_spacing_m / range_factor as i32;
    let gate_range = GateRange {
        first_gate_m: grid.gate_range.first_gate_m
            + (sub_spacing_m - grid.gate_range.gate_spacing_m) / 2,
        gate_spacing_m: sub_spacing_m,
        gate_count: new_gates,
    };
    // Each output row links back to its nearest parent's radial so
    // cut-radial lookups (Nyquist, beam azimuth basis) stay valid.
    let radial_indices = row_plan
        .iter()
        .map(|plan| grid.radial_indices[if plan.t <= 0.5 { plan.lo } else { plan.hi }])
        .collect();
    let row_azimuths_deg = row_plan.iter().map(|plan| plan.azimuth_deg).collect();
    Some(InterpolatedGrid {
        grid: MomentGrid {
            moment: grid.moment.clone(),
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices,
            storage: MomentStorage::F32(values),
        },
        row_azimuths_deg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::Radial;
    use std::collections::BTreeMap;

    fn radial(azimuth_deg: f32, gate_range: GateRange) -> Radial {
        Radial {
            azimuth_deg,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range,
            radial_status: None,
            nyquist_velocity_mps: Some(26.0),
        }
    }

    fn cut_and_grid(
        moment: MomentType,
        azimuths: &[f32],
        gates: usize,
        spacing_m: i32,
        data: Vec<f32>,
    ) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: spacing_m,
            gate_spacing_m: spacing_m,
            gate_count: gates,
        };
        let cut = ElevationCut {
            elevation_deg: 0.5,
            elevation_number: Some(1),
            radials: azimuths
                .iter()
                .map(|az| radial(*az, gate_range.clone()))
                .collect(),
            moments: BTreeMap::new(),
        };
        let grid = MomentGrid {
            moment,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..azimuths.len()).collect(),
            storage: MomentStorage::F32(data),
        };
        (cut, grid)
    }

    fn full_sweep_azimuths(count: usize) -> Vec<f32> {
        (0..count)
            .map(|i| i as f32 * 360.0 / count as f32)
            .collect()
    }

    #[test]
    fn factor_policy_table() {
        // (nominal az, spacing) → (az factor, range factor)
        let cases = [
            ((1.0, 1000), (4, 4)),
            ((1.0, 500), (4, 2)),
            ((1.0, 250), (4, 1)),
            ((0.5, 250), (2, 1)),
            ((0.25, 250), (1, 1)),
            ((0.5, 1000), (2, 4)),
        ];
        for ((az, sp), (fa, fr)) in cases {
            let factors = upsample_factors(az, sp, 360, 1000);
            assert_eq!(
                (factors.azimuth, factors.range),
                (fa, fr),
                "policy for {az}° × {sp} m"
            );
        }
    }

    #[test]
    fn factor_policy_respects_exact_geometry_and_budget() {
        // 900 m gates: 4× would need a fractional half-shift (225 m sub,
        // 337.5 m shift) — steps down to 3× (300 m, exact).
        let factors = upsample_factors(1.0, 900, 360, 1000);
        assert_eq!(factors.range, 3);
        // Budget: 720 × 8000 cells at 2×2 would be 92 MB — range steps
        // down first.
        let factors = upsample_factors(0.5, 500, 720, 8000);
        assert_eq!((factors.azimuth, factors.range), (2, 1));
    }

    #[test]
    fn geometry_subdivides_exactly() {
        // 1.0° × 1000 m, 360 rows × 4 gates → 4× both axes.
        let azimuths = full_sweep_azimuths(360);
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![30.0; 360 * 4],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("coarse grid upsamples");
        assert_eq!(up.grid.gate_range.gate_spacing_m, 250);
        assert_eq!(up.grid.gate_range.gate_count, 16);
        // first_gate_m is a gate CENTER: new_first = first + (sub - sp)/2.
        assert_eq!(up.grid.gate_range.first_gate_m, 1000 + (250 - 1000) / 2);
        // The rendered annulus is preserved exactly:
        // first + (count - 0.5)·spacing matches on both grids.
        let native_edge = grid.gate_range.first_gate_m as f32
            + (grid.gate_range.gate_count as f32 - 0.5) * grid.gate_range.gate_spacing_m as f32;
        let up_edge = up.grid.gate_range.first_gate_m as f32
            + (up.grid.gate_range.gate_count as f32 - 0.5)
                * up.grid.gate_range.gate_spacing_m as f32;
        assert_eq!(native_edge, up_edge);
        let native_inner =
            grid.gate_range.first_gate_m as f32 - grid.gate_range.gate_spacing_m as f32 / 2.0;
        let up_inner =
            up.grid.gate_range.first_gate_m as f32 - up.grid.gate_range.gate_spacing_m as f32 / 2.0;
        assert_eq!(native_inner, up_inner);
        // 360 rows × 4 az factor, every native row at its exact azimuth.
        assert_eq!(up.row_azimuths_deg.len(), 1440);
        assert_eq!(up.grid.radial_count(), 1440);
        for (row, az) in azimuths.iter().enumerate() {
            assert_eq!(up.row_azimuths_deg[row * 4], *az, "native row {row}");
        }
        // Synthetic rows fall between (1° spacing / 4 = 0.25° steps).
        assert!((up.row_azimuths_deg[1] - 0.25).abs() < 1e-3);
        assert!((up.row_azimuths_deg[2] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn azimuth_wraps_between_last_and_first_row() {
        let azimuths = full_sweep_azimuths(360);
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![30.0; 360 * 4],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        // Last native row is 359°; sub-rows climb toward 360 and wrap.
        let tail: Vec<f32> = up.row_azimuths_deg[1437..].to_vec();
        assert!((tail[0] - 359.25).abs() < 1e-3, "{tail:?}");
        assert!((tail[2] - 359.75).abs() < 1e-3, "{tail:?}");
    }

    #[test]
    fn uniform_field_is_unchanged_and_fine_grids_pass_through() {
        let azimuths = full_sweep_azimuths(360);
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            8,
            500,
            vec![35.0; 360 * 8],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 35.0).abs() < 1e-4,
                    "row {row} gate {gate}: {value}"
                );
            }
        }
        // Already at target: no pass.
        let azimuths = full_sweep_azimuths(1440);
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            250,
            vec![35.0; 1440 * 4],
        );
        assert!(upsample_moment_grid(&cut, &grid).is_none());
    }

    #[test]
    fn coverage_does_not_grow() {
        // Rows 0..180 carry echo, rows 180..360 are empty: every sub-cell
        // whose containing native cell is empty must stay empty.
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..180 {
            for gate in 0..4 {
                data[row * 4 + gate] = 20.0;
            }
        }
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let mut covered_rows = 0;
        for row in 0..up.grid.radial_count() {
            if (0..up.grid.gate_range.gate_count).any(|gate| {
                up.grid
                    .scaled_value(row, gate)
                    .is_some_and(|v| v.is_finite())
            }) {
                covered_rows += 1;
            }
        }
        // Exactly the rows whose NEAREST parent is an echo row render
        // (and the beam-boundary rows at t = 0.5 only where BOTH parents
        // carry echo): 180 native echo rows, 179 interior segments × 3
        // sub-rows, the trailing edge of row 179 (t = 0.25 toward empty
        // row 180 — its t = 0.5 boundary row stays empty), and the
        // leading edge of row 0 (t = 0.75 from empty row 359) — 719 of
        // 1440 rows, never more than the echo's native angular footprint.
        assert_eq!(covered_rows, 180 + 179 * 3 + 1 + 1);
    }

    #[test]
    fn echo_edges_use_nearest_parent_not_partial_blends() {
        // One echo column next to an empty column: sub-cells inside the
        // echo column keep the exact parent value (no fade-out ramp).
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..360 {
            data[row * 4 + 1] = 40.0;
        }
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for sub in 0..4 {
                let gate = 4 + sub; // native gate 1's sub-cells
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 40.0).abs() < 1e-4,
                    "row {row} sub {sub}: {value} (partial blend leaked)"
                );
            }
            // Native gates 0 and 2 are empty: their sub-cells stay empty.
            for gate in (0..4).chain(8..12) {
                assert!(
                    up.grid
                        .scaled_value(row, gate)
                        .is_none_or(|value| value.is_nan()),
                    "row {row} gate {gate} grew coverage"
                );
            }
        }
    }

    #[test]
    fn velocity_fold_guard_uses_nearest_parent() {
        // Neighboring radials at +20 / -22 m/s (spread 42 > 30): blending
        // would fabricate near-zero gates inside the couplet/fold.
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..360 {
            let value = if row % 2 == 0 { 20.0 } else { -22.0 };
            for gate in 0..4 {
                data[row * 4 + gate] = value;
            }
        }
        let (cut, grid) = cut_and_grid(MomentType::Velocity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 20.0).abs() < 1e-4 || (value + 22.0).abs() < 1e-4,
                    "row {row} gate {gate}: fabricated intermediate {value}"
                );
            }
        }
        // Small spreads DO blend (same field, ±5 m/s).
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..360 {
            let value = if row % 2 == 0 { 5.0 } else { -5.0 };
            for gate in 0..4 {
                data[row * 4 + gate] = value;
            }
        }
        let (cut, grid) = cut_and_grid(MomentType::Velocity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let blended = (0..up.grid.radial_count()).any(|row| {
            (0..up.grid.gate_range.gate_count).any(|gate| {
                up.grid
                    .scaled_value(row, gate)
                    .is_some_and(|value| value.abs() < 4.0)
            })
        });
        assert!(blended, "small velocity spreads should interpolate");
    }

    #[test]
    fn cc_guard_never_blends_through_the_melting_layer() {
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..360 {
            for gate in 0..4 {
                // 0.92 / 1.0 alternating along range: the rho_hv minimum
                // must survive (no 0.96 fabrications).
                data[row * 4 + gate] = if gate % 2 == 0 { 0.92 } else { 1.0 };
            }
        }
        let (cut, grid) =
            cut_and_grid(MomentType::CorrelationCoefficient, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 0.92).abs() < 1e-4 || (value - 1.0).abs() < 1e-4,
                    "row {row} gate {gate}: blended through the CC minimum ({value})"
                );
            }
        }
    }

    #[test]
    fn sector_scan_gap_stays_native() {
        // A 91-radial sector (0..90°) — no sub-rows across the 270° gap.
        let azimuths: Vec<f32> = (0..91).map(|i| i as f32).collect();
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![30.0; 91 * 4],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for az in &up.row_azimuths_deg {
            assert!(
                *az <= 90.0 + 1e-3,
                "synthetic row at {az}° bridges the sector gap"
            );
        }
        // …but inside the sector the rows did refine to 0.25°.
        assert_eq!(up.row_azimuths_deg.len(), 91 + 90 * 3);
    }

    #[test]
    fn upsample_cost_smoke() {
        // NEXRAD super-res-shaped cut (720 × 1832 at 0.5° × 250 m → 2×1)
        // and a European-shaped cut (360 × 960 at 1.0° × 500 m → 4×2):
        // one pass each, wall-clock printed for the perf report.
        let azimuths = full_sweep_azimuths(720);
        let data: Vec<f32> = (0..720 * 1832).map(|i| (i % 70) as f32).collect();
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 1832, 250, data);
        let start = std::time::Instant::now();
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let super_res_ms = start.elapsed().as_secs_f32() * 1000.0;
        assert_eq!(up.grid.radial_count(), 1440);
        assert_eq!(up.grid.gate_range.gate_count, 1832);

        let azimuths = full_sweep_azimuths(360);
        let data: Vec<f32> = (0..360 * 960).map(|i| (i % 70) as f32).collect();
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 960, 500, data);
        let start = std::time::Instant::now();
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let euro_ms = start.elapsed().as_secs_f32() * 1000.0;
        assert_eq!(up.grid.radial_count(), 1440);
        assert_eq!(up.grid.gate_range.gate_count, 1920);

        // Worst realistic 4×4 case: a long-range 1.0° × 1000 m cut
        // (360 × 2000 → 1440 × 8000 = 11.5M cells, 46 MB F32).
        let azimuths = full_sweep_azimuths(360);
        let data: Vec<f32> = (0..360 * 2000).map(|i| (i % 70) as f32).collect();
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 2000, 1000, data);
        let start = std::time::Instant::now();
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let long_range_ms = start.elapsed().as_secs_f32() * 1000.0;
        assert_eq!(up.grid.radial_count(), 1440);
        assert_eq!(up.grid.gate_range.gate_count, 8000);
        println!(
            "upsample cost: super-res 720x1832 (2x1) {super_res_ms:.2} ms, \
             euro 360x960 (4x2) {euro_ms:.2} ms, \
             long-range 360x2000 (4x4) {long_range_ms:.2} ms"
        );
        // Generous bound — this is a smoke test, not a benchmark gate.
        assert!(super_res_ms < 2000.0 && euro_ms < 2000.0 && long_range_ms < 2000.0);
    }
}
