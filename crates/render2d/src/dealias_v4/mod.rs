//! Dealias v4 — one volume-joint branch solve, per-tilt outputs, everything
//! deterministic (`docs/dealias-v4-spec.md` is the governing spec).
//!
//! The v1 region core (Jing & Wiener 1993, *JTECH* 10, 798–808) segments each
//! velocity tilt and resolves RELATIVE folds; what boundary evidence cannot
//! know is each connected group's ABSOLUTE branch (failures F1/F3/F5 in the
//! spec).  v4 decides all branches at once by minimizing one discrete energy
//! over per-super-region branch shifts k ∈ {−2…+2} across every velocity
//! tilt of the volume, fed by:
//!
//! - an optional caller-supplied [`EnvironmentalWindProfile`] (4DD sounding
//!   initialization, James & Houze 2001; ORPG environmental winds, Eilts &
//!   Smith 1990) — the only non-circular evidence in deep widespread
//!   aliasing;
//! - the previous volume's solution (temporal prior), weighted by its own
//!   confidence output so a persistent misbranch cannot reproduce itself
//!   (the F6 GIGO break);
//! - vertical co-location between adjacent tilts (the 3-D evidence);
//! - the v1 vote graph's weak edges as soft terms (the F3 fix).
//!
//! A post-solve repair gauntlet (UNRAVEL, Louf et al. 2020; R2D2 couplet
//! protection, Feldmann et al. 2020; despeckle, Holleman & Beekhuis 2003)
//! closes the long tail under strict do-no-harm rules.
//!
//! INVARIANTS
//! - render2d never fetches: the profile and the previous volume are caller
//!   values.  With neither, output degrades to v1-class behavior (pinned by
//!   test) — never worse.
//! - Whole ±2N moves only, per gate, everywhere (solver and gauntlet).
//! - Nyquist-less feeds (JMA staggered PRT) stay pass-through by design —
//!   same `dealias_skipped_no_nyquist` contract as v1.
//! - Same input volume (+ same priors) ⇒ byte-identical grids and
//!   confidence, across runs and thread counts.
//! - v1/v2/v3 remain untouched; v4 is additive and selectable.
//!
//! DEVIATION FROM SPEC §6 (documented): `TemporalPrior::Volume` solves the
//! bare previous volume with THIS solver (no temporal prior, same
//! environment) instead of a plain v1 pass.  A v1-only prior would re-inject
//! exactly the branch errors the temporal term is meant to correct (the
//! hybrid solved its prior with an upper-tilt reference for the same
//! reason); confidence propagation still applies.  Callers that keep the
//! previous [`V4VolumeSolution`] avoid the extra solve entirely.

mod confidence;
mod env_profile;
mod graph;
mod repair;
mod solve;
mod super_regions;

pub use confidence::ConfidenceGrid;
pub use env_profile::{EnvWindLevel, EnvironmentalWindProfile, project_environmental_winds};

use chrono::{DateTime, Utc};
use radar_core::{GateRange, MomentGrid, MomentStorage, MomentType, RadarVolume};
use rayon::prelude::*;

use crate::region_core::{self, RegionSolve};
use graph::MappedPrior;
use repair::{RepairContext, RepairDiagnostics};
use super_regions::{SUPERREGION_MIN_GATES, TiltSuperRegions, build_super_regions};

/// Temporal priors older than this are more likely to represent a different
/// storm structure than a useful continuity constraint (same constant as the
/// hybrid engine).
const TEMPORAL_MAX_AGE_SECONDS: i64 = 15 * 60;
/// Match the same nominal elevation between consecutive volumes.
const TEMPORAL_ELEVATION_TOLERANCE_DEG: f32 = 0.40;
/// Reject physically implausible reference samples (same as hybrid).
const MAX_REFERENCE_ABS_VELOCITY_MPS: f32 = 160.0;
/// Temporal reference gates below this confidence (≈ 64/255, the
/// "interior-consistent only" level) fall through to vertical/environmental
/// evidence in the repair reference stack.
const REPAIR_TEMPORAL_MIN_CONFIDENCE: f32 = 0.25;
/// Velocity-interval width (× Nyquist) for the segmentation hygiene split
/// (Helmus & Collis 2016; spec §4f/§5.1).  N/2 matches `REGION_JOIN_FRAC`,
/// so no pair that could directly join a region ever spans more than two
/// intervals.  Cuts F2 mega-region noise chains; smooth seams weld back as
/// strong vote edges.
const V4_INTERVAL_SPLIT_FRAC: f32 = 0.5;

/// The previous volume's evidence, in order of preference.  Prefer passing
/// the previous *solution* so confidence propagates (F6); a bare previous
/// volume is accepted and solved internally as a fallback (see module doc).
pub enum TemporalPrior<'a> {
    Solution(&'a V4VolumeSolution),
    Volume(&'a RadarVolume),
}

/// Solver + gauntlet diagnostics; the eval battery regression-gates these.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct V4Diagnostics {
    pub velocity_tilts: usize,
    pub nodes: usize,
    pub graph_edges: usize,
    pub components: usize,
    pub enumerated_components: usize,
    /// Nonzero means the forest-DP + ICM heuristic missed an optimum that
    /// exhaustive enumeration found — watch it in the battery.
    pub enumeration_beat_heuristic: usize,
    pub energy: f64,
    pub env_profile_used: bool,
    pub temporal_prior_used: bool,
    pub couplet_masked: usize,
    pub speck_snapped: usize,
    pub patch_changed: usize,
    pub ring_closed: usize,
    /// Patch components reverted by the boundary-inflation audit (gates).
    pub patch_reverted: usize,
    pub box_moved: usize,
    pub plane_moved: usize,
    pub repair_aborts: u32,
}

/// One solved velocity tilt.
pub struct V4TiltSolution {
    cut_index: usize,
    elevation_deg: f32,
    azimuths: Vec<f32>,
    grid: MomentGrid,
    confidence: ConfidenceGrid,
}

/// The whole-volume solve result.  The app's render worker caches one of
/// these per volume; every velocity tilt is served from it in O(1).
pub struct V4VolumeSolution {
    site_id: String,
    volume_time: DateTime<Utc>,
    /// Indexed by cut index (None for cuts without velocity).
    tilts: Vec<Option<V4TiltSolution>>,
    diagnostics: V4Diagnostics,
}

impl V4VolumeSolution {
    /// Dealiased grid for a cut, if it carried velocity.  O(1).
    pub fn tilt_grid(&self, cut_index: usize) -> Option<&MomentGrid> {
        self.tilts.get(cut_index)?.as_ref().map(|tilt| &tilt.grid)
    }

    /// Per-gate branch confidence for a cut (spec §8).  O(1).
    pub fn tilt_confidence(&self, cut_index: usize) -> Option<&ConfidenceGrid> {
        self.tilts
            .get(cut_index)?
            .as_ref()
            .map(|tilt| &tilt.confidence)
    }

    pub fn diagnostics(&self) -> &V4Diagnostics {
        &self.diagnostics
    }

    /// Consume the solution, extracting one tilt's grid without a clone.
    pub fn into_tilt_grid(mut self, cut_index: usize) -> Option<MomentGrid> {
        self.tilts.get_mut(cut_index)?.take().map(|tilt| tilt.grid)
    }
}

/// Solve the whole volume once (spec §6 public surface).
///
/// `previous` enables the temporal prior; `environment` the absolute branch
/// anchor.  Both are optional and independently validated (site, age,
/// staleness) — an unusable input degrades gracefully, never errors.
pub fn dealias_volume_v4(
    volume: &RadarVolume,
    previous: Option<TemporalPrior<'_>>,
    environment: Option<&EnvironmentalWindProfile>,
) -> V4VolumeSolution {
    let environment = environment.filter(|profile| profile.usable_for(volume.volume_time));

    // ---- per-tilt fields + v1 region solves (parallel, order-stable) ----
    let velocity_cuts: Vec<usize> = (0..volume.cuts.len())
        .filter(|&cut_index| {
            volume.cuts[cut_index]
                .moments
                .get(&MomentType::Velocity)
                .is_some_and(|grid| grid.radial_count() > 0 && grid.gate_range.gate_count > 0)
        })
        .collect();
    let mut tilts: Vec<TiltField> = velocity_cuts
        .par_iter()
        .map(|&cut_index| build_tilt_field(volume, cut_index))
        .collect();

    // ---- global node assignment (deterministic: tilt order, super order) --
    let mut node_count = 0usize;
    for tilt in &mut tilts {
        tilt.node_of_super = tilt
            .supers
            .super_gates
            .iter()
            .map(|&gate_count| {
                if gate_count >= SUPERREGION_MIN_GATES {
                    let node = node_count;
                    node_count += 1;
                    node
                } else {
                    usize::MAX
                }
            })
            .collect();
    }

    // ---- temporal prior mapping ----
    let temporal = resolve_temporal(volume, &tilts, previous, environment);
    let temporal_prior_used = temporal.iter().any(Option::is_some);

    // ---- one volume energy, one solve ----
    let tables = graph::build_evidence(&tilts, node_count, environment, &temporal);
    let outcome = solve::solve_labels(&tables);

    // ---- apply labels: per-gate folds + confidence ----
    let mut per_tilt: Vec<(Vec<i32>, Vec<u8>)> = tilts
        .iter()
        .map(|tilt| apply_labels(tilt, &tables, &outcome))
        .collect();

    // ---- repair references from the PRE-repair solved fields ----
    // Built before any gauntlet runs so tilt repair order cannot matter.
    let folds_snapshot: Vec<&[i32]> = per_tilt.iter().map(|(folds, _)| folds.as_slice()).collect();
    let references: Vec<Option<Vec<f32>>> = (0..tilts.len())
        .into_par_iter()
        .map(|index| build_repair_reference(&tilts, index, &folds_snapshot, &temporal, environment))
        .collect();

    // ---- repair gauntlet per tilt (parallel; each tilt owns its arrays) ---
    let repair_results: Vec<RepairDiagnostics> = per_tilt
        .par_iter_mut()
        .zip(tilts.par_iter())
        .zip(references.par_iter())
        .map(|(((folds, confidence), tilt), reference)| {
            let ctx = RepairContext {
                observed: &tilt.observed,
                nyq: &tilt.nyq,
                rows: tilt.rows,
                gates: tilt.gates,
                wraps: tilt.wraps,
                reference: reference.as_deref(),
            };
            repair::run_gauntlet(&ctx, folds, confidence)
        })
        .collect();
    let mut repair_totals = RepairDiagnostics::default();
    for result in &repair_results {
        repair_totals.accumulate(result);
    }

    // ---- encode ----
    let mut solutions: Vec<Option<V4TiltSolution>> = (0..volume.cuts.len()).map(|_| None).collect();
    for (tilt, (folds, confidence_values)) in tilts.into_iter().zip(per_tilt) {
        let grid = encode_tilt(&tilt, &folds);
        let confidence = ConfidenceGrid::new(tilt.rows, tilt.gates, confidence_values);
        solutions[tilt.cut_index] = Some(V4TiltSolution {
            cut_index: tilt.cut_index,
            elevation_deg: tilt.elevation_deg,
            azimuths: tilt.azimuths,
            grid,
            confidence,
        });
    }

    let diagnostics = V4Diagnostics {
        velocity_tilts: velocity_cuts.len(),
        nodes: node_count,
        graph_edges: tables.edges.len(),
        components: outcome.components,
        enumerated_components: outcome.enumerated_components,
        enumeration_beat_heuristic: outcome.enumeration_beat_heuristic,
        energy: outcome.energy,
        env_profile_used: environment.is_some(),
        temporal_prior_used,
        couplet_masked: repair_totals.couplet_masked,
        speck_snapped: repair_totals.speck_snapped,
        patch_changed: repair_totals.patch_changed,
        ring_closed: repair_totals.ring_closed,
        patch_reverted: repair_totals.patch_reverted,
        box_moved: repair_totals.box_moved,
        plane_moved: repair_totals.plane_moved,
        repair_aborts: repair_totals.aborted_modules,
    };

    V4VolumeSolution {
        site_id: volume.site.id.clone(),
        volume_time: volume.volume_time,
        tilts: solutions,
        diagnostics,
    }
}

/// Drop-in per-cut convenience mirroring `dealias_velocity_grid_hybrid`.
/// Runs the volume solve internally — callers that need more than one tilt
/// should hold a [`V4VolumeSolution`] instead (one solve serves all tilts).
pub fn dealias_velocity_grid_v4(
    volume: &RadarVolume,
    cut_index: usize,
    previous: Option<&RadarVolume>,
    environment: Option<&EnvironmentalWindProfile>,
) -> Option<MomentGrid> {
    volume
        .cuts
        .get(cut_index)?
        .moments
        .get(&MomentType::Velocity)?;
    let solution = dealias_volume_v4(volume, previous.map(TemporalPrior::Volume), environment);
    solution.into_tilt_grid(cut_index)
}

/// Everything the solver knows about one velocity tilt.
pub(crate) struct TiltField {
    pub(crate) cut_index: usize,
    pub(crate) elevation_deg: f32,
    pub(crate) rows: usize,
    pub(crate) gates: usize,
    pub(crate) first_gate_m: i32,
    pub(crate) gate_spacing_m: i32,
    pub(crate) gate_range: GateRange,
    pub(crate) radial_indices: Vec<usize>,
    pub(crate) wraps: bool,
    pub(crate) azimuths: Vec<f32>,
    /// Per-row Nyquist (NaN where unknown — those rows pass through).
    pub(crate) nyq: Vec<f32>,
    /// Observed velocity, NaN for no-data / range-folded gates.
    pub(crate) observed: Vec<f32>,
    pub(crate) solve: RegionSolve,
    pub(crate) supers: TiltSuperRegions,
    /// Global node index per super-region (`usize::MAX` below the gate
    /// floor).
    pub(crate) node_of_super: Vec<usize>,
}

fn build_tilt_field(volume: &RadarVolume, cut_index: usize) -> TiltField {
    let cut = &volume.cuts[cut_index];
    let grid = cut
        .moments
        .get(&MomentType::Velocity)
        .expect("caller filtered to velocity cuts");
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    let total = rows.saturating_mul(gates);

    let azimuths = crate::radial_azimuths(cut, grid);
    let fallback_nyquist = crate::median_nyquist_mps(cut, grid);
    let mut nyq = vec![f32::NAN; rows.max(1)];
    for (row, slot) in nyq.iter_mut().enumerate().take(rows) {
        *slot = crate::row_nyquist_mps(cut, grid, row)
            .or(fallback_nyquist)
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(f32::NAN);
    }

    let mut observed = vec![f32::NAN; total];
    if total > 0 {
        let mut row_buffer = vec![f32::NAN; gates];
        for row in 0..rows {
            crate::copy_scaled_velocity_row(grid, row, &mut row_buffer);
            observed[row * gates..(row + 1) * gates].copy_from_slice(&row_buffer);
        }
    }

    let solve = region_core::solve_region_folds_split(
        &observed,
        &nyq,
        rows,
        gates,
        &azimuths,
        V4_INTERVAL_SPLIT_FRAC,
    );
    let supers = build_super_regions(&solve);
    let wraps = crate::sweep_wraps(&azimuths);

    TiltField {
        cut_index,
        elevation_deg: cut.elevation_deg,
        rows,
        gates,
        first_gate_m: grid.gate_range.first_gate_m,
        gate_spacing_m: grid.gate_range.gate_spacing_m,
        gate_range: grid.gate_range.clone(),
        radial_indices: grid.radial_indices.clone(),
        wraps,
        azimuths,
        nyq,
        observed,
        solve,
        supers,
        node_of_super: Vec::new(),
    }
}

/// Per-gate folds and confidence from the label solve (pre-repair).
fn apply_labels(
    tilt: &TiltField,
    tables: &graph::EvidenceTables,
    outcome: &solve::SolveOutcome,
) -> (Vec<i32>, Vec<u8>) {
    let total = tilt.rows.saturating_mul(tilt.gates);
    let mut folds = vec![0i32; total];
    let mut confidence_values = vec![0u8; total];
    for idx in 0..total {
        if !tilt.observed[idx].is_finite() {
            continue;
        }
        let rid = tilt.solve.region_of[idx];
        if rid == u32::MAX {
            continue;
        }
        let base = tilt.solve.region_fold[rid as usize];
        let sid = tilt.supers.super_of_region[rid as usize] as usize;
        let node = tilt.node_of_super[sid];
        let (fold, gate_confidence) = if node == usize::MAX {
            (base, confidence::INTERIOR_ONLY)
        } else {
            let shifted = (base + outcome.labels[node])
                .clamp(-region_core::REGION_MAX_FOLD, region_core::REGION_MAX_FOLD);
            let gate_confidence = if tables.has_external[node] && !outcome.renormalized[node] {
                confidence::margin_to_confidence(outcome.margins[node])
            } else {
                // No external evidence, or an absolutely-degenerate
                // (re-anchored) component: interior-consistent only.
                confidence::INTERIOR_ONLY
            };
            (shifted, gate_confidence)
        };
        folds[idx] = fold;
        confidence_values[idx] = gate_confidence;
    }
    (folds, confidence_values)
}

/// Resolve the temporal prior into per-tilt mapped fields.
fn resolve_temporal(
    volume: &RadarVolume,
    tilts: &[TiltField],
    previous: Option<TemporalPrior<'_>>,
    environment: Option<&EnvironmentalWindProfile>,
) -> Vec<Option<MappedPrior>> {
    let none = || (0..tilts.len()).map(|_| None).collect::<Vec<_>>();
    match previous {
        None => none(),
        Some(TemporalPrior::Solution(solution)) => {
            if !temporal_usable(volume, &solution.site_id, solution.volume_time) {
                return none();
            }
            map_solution(tilts, solution)
        }
        Some(TemporalPrior::Volume(previous_volume)) => {
            if !temporal_usable(
                volume,
                &previous_volume.site.id,
                previous_volume.volume_time,
            ) {
                return none();
            }
            // See module doc: the bare volume is solved with this engine
            // (no temporal recursion) so the prior does not re-inject the
            // exact branch errors the temporal term exists to correct.
            let prior_solution = dealias_volume_v4(previous_volume, None, environment);
            map_solution(tilts, &prior_solution)
        }
    }
}

fn temporal_usable(current: &RadarVolume, prior_site: &str, prior_time: DateTime<Utc>) -> bool {
    if !current.site.id.eq_ignore_ascii_case(prior_site) {
        return false;
    }
    let age_seconds = current
        .volume_time
        .signed_duration_since(prior_time)
        .num_seconds();
    age_seconds > 0 && age_seconds <= TEMPORAL_MAX_AGE_SECONDS
}

fn map_solution(tilts: &[TiltField], solution: &V4VolumeSolution) -> Vec<Option<MappedPrior>> {
    let sources: Vec<&V4TiltSolution> = solution.tilts.iter().flatten().collect();
    tilts
        .iter()
        .map(|tilt| {
            let source = sources
                .iter()
                .filter_map(|source| {
                    let delta = (source.elevation_deg - tilt.elevation_deg).abs();
                    (delta <= TEMPORAL_ELEVATION_TOLERANCE_DEG).then_some((source, delta))
                })
                .min_by(|left, right| {
                    left.1
                        .total_cmp(&right.1)
                        .then_with(|| left.0.cut_index.cmp(&right.0.cut_index))
                })?
                .0;
            map_solution_tilt(tilt, source)
        })
        .collect()
}

fn map_solution_tilt(tilt: &TiltField, source: &V4TiltSolution) -> Option<MappedPrior> {
    let total = tilt.rows.saturating_mul(tilt.gates);
    if total == 0 {
        return None;
    }
    let row_map = graph::nearest_row_map(&tilt.azimuths, &source.azimuths);
    let gate_map = graph::nearest_gate_map(
        tilt.first_gate_m,
        tilt.gate_spacing_m,
        tilt.gates,
        source.grid.gate_range.first_gate_m,
        source.grid.gate_range.gate_spacing_m,
        source.grid.gate_range.gate_count,
    );
    let mut values = vec![f32::NAN; total];
    let mut confidence = vec![0.0f32; total];
    let mut any = false;
    for (row, mapped_row) in row_map.iter().enumerate() {
        let Some(source_row) = *mapped_row else {
            continue;
        };
        for (gate, mapped_gate) in gate_map.iter().enumerate() {
            let Some(source_gate) = *mapped_gate else {
                continue;
            };
            let Some(value) = source
                .grid
                .scaled_value(source_row, source_gate)
                .filter(|value| value.is_finite() && value.abs() <= MAX_REFERENCE_ABS_VELOCITY_MPS)
            else {
                continue;
            };
            let idx = row * tilt.gates + gate;
            values[idx] = value;
            confidence[idx] = f32::from(
                source
                    .confidence
                    .value(source_row, source_gate)
                    .unwrap_or(0),
            ) / 255.0;
            any = true;
        }
    }
    any.then_some(MappedPrior { values, confidence })
}

/// The gauntlet's covering reference: temporal (confidence-gated), then the
/// nearest vertical neighbor's solved field, then the environmental
/// projection (spec §7 R2 priority order).
fn build_repair_reference(
    tilts: &[TiltField],
    index: usize,
    folds: &[&[i32]],
    temporal: &[Option<MappedPrior>],
    environment: Option<&EnvironmentalWindProfile>,
) -> Option<Vec<f32>> {
    let tilt = &tilts[index];
    let total = tilt.rows.saturating_mul(tilt.gates);
    if total == 0 {
        return None;
    }
    let mut reference = vec![f32::NAN; total];
    let mut any = false;

    if let Some(prior) = &temporal[index] {
        for ((slot, value), gate_confidence) in reference
            .iter_mut()
            .zip(&prior.values)
            .zip(&prior.confidence)
        {
            if value.is_finite() && *gate_confidence >= REPAIR_TEMPORAL_MIN_CONFIDENCE {
                *slot = *value;
                any = true;
            }
        }
    }

    // Nearest vertical neighbor: prefer above (usually cleaner), else below.
    let neighbor = (0..tilts.len())
        .filter(|&other| other != index)
        .filter_map(|other| {
            let signed = tilts[other].elevation_deg - tilt.elevation_deg;
            let delta = signed.abs();
            (graph::SIBLING_ELEVATION_DELTA_DEG..=graph::VERTICAL_MAX_ELEVATION_DELTA_DEG)
                .contains(&delta)
                .then_some((other, signed < 0.0, delta))
        })
        .min_by(|left, right| {
            left.1
                .cmp(&right.1) // false (above) sorts before true (below)
                .then_with(|| left.2.total_cmp(&right.2))
        })
        .map(|(other, _, _)| other);
    if let Some(other) = neighbor {
        let source = &tilts[other];
        let source_folds = folds[other];
        let row_map = graph::nearest_row_map(&tilt.azimuths, &source.azimuths);
        let gate_map = graph::nearest_gate_map(
            tilt.first_gate_m,
            tilt.gate_spacing_m,
            tilt.gates,
            source.first_gate_m,
            source.gate_spacing_m,
            source.gates,
        );
        for (row, mapped_row) in row_map.iter().enumerate() {
            let Some(source_row) = *mapped_row else {
                continue;
            };
            let source_n = source.nyq[source_row];
            for (gate, mapped_gate) in gate_map.iter().enumerate() {
                let idx = row * tilt.gates + gate;
                if reference[idx].is_finite() {
                    continue;
                }
                let Some(source_gate) = *mapped_gate else {
                    continue;
                };
                let source_idx = source_row * source.gates + source_gate;
                let observed = source.observed[source_idx];
                if !observed.is_finite() {
                    continue;
                }
                let value = if source_n.is_finite() {
                    observed + 2.0 * source_n * source_folds[source_idx] as f32
                } else {
                    observed
                };
                if value.abs() <= MAX_REFERENCE_ABS_VELOCITY_MPS {
                    reference[idx] = value;
                    any = true;
                }
            }
        }
    }

    if let Some(profile) = environment {
        let env_field = env_profile::project_profile_to_tilt(
            profile,
            tilt.elevation_deg,
            &tilt.azimuths,
            tilt.first_gate_m,
            tilt.gate_spacing_m,
            tilt.gates,
        );
        for idx in 0..total {
            if !reference[idx].is_finite() && env_field[idx].is_finite() {
                reference[idx] = env_field[idx];
                any = true;
            }
        }
    }

    any.then_some(reference)
}

/// Encode the final folds into the shared dealiased-velocity grid format
/// (same scale/offset contract as v1/v2/v3 so every consumer downstream is
/// untouched).
fn encode_tilt(tilt: &TiltField, folds: &[i32]) -> MomentGrid {
    let total = tilt.rows.saturating_mul(tilt.gates);
    let mut corrected = vec![crate::DEALIASED_VELOCITY_NODATA; total];
    for row in 0..tilt.rows {
        let n = tilt.nyq[row];
        for gate in 0..tilt.gates {
            let idx = row * tilt.gates + gate;
            let value = tilt.observed[idx];
            if !value.is_finite() {
                continue;
            }
            let unfolded = if n.is_finite() {
                value + 2.0 * n * folds[idx] as f32
            } else {
                value
            };
            corrected[idx] = crate::encode_dealiased_velocity(unfolded);
        }
    }
    MomentGrid {
        moment: MomentType::Velocity,
        gate_range: tilt.gate_range.clone(),
        scale: crate::DEALIASED_VELOCITY_SCALE,
        offset: crate::DEALIASED_VELOCITY_OFFSET,
        nodata: Some(crate::DEALIASED_VELOCITY_NODATA),
        range_folded: None,
        radial_indices: tilt.radial_indices.clone(),
        storage: MomentStorage::U16(corrected),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dealias_velocity_grid;
    use chrono::Duration;
    use radar_core::{ElevationCut, RadarSite, Radial};

    fn utc(hours: i64) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + Duration::hours(hours)
    }

    fn velocity_cut(
        elevation: f32,
        nyquist: f32,
        rows: usize,
        gates: usize,
        value_at: impl Fn(usize, usize) -> f32,
    ) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elevation, None);
        let mut data = vec![f32::NAN; rows * gates];
        for row in 0..rows {
            let azimuth = row as f32 * (360.0 / rows as f32);
            cut.radials.push(Radial {
                azimuth_deg: azimuth,
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(nyquist),
                radial_status: None,
            });
            for gate in 0..gates {
                data[row * gates + gate] = value_at(row, gate);
            }
        }
        cut.moments.insert(
            MomentType::Velocity,
            MomentGrid {
                moment: MomentType::Velocity,
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

    fn wrap(value: f32, nyquist: f32) -> f32 {
        (value + nyquist).rem_euclid(2.0 * nyquist) - nyquist
    }

    fn wind_cut(
        elevation: f32,
        speed: f32,
        toward_deg: f32,
        nyquist: f32,
        rows: usize,
        gates: usize,
    ) -> ElevationCut {
        velocity_cut(elevation, nyquist, rows, gates, move |row, _| {
            let azimuth = row as f32 * (360.0 / rows as f32);
            wrap(speed * (azimuth - toward_deg).to_radians().cos(), nyquist)
        })
    }

    fn wind_error(grid: &MomentGrid, rows: usize, gates: usize, speed: f32, toward: f32) -> f32 {
        let mut worst = 0.0f32;
        for row in 0..rows {
            let azimuth = row as f32 * (360.0 / rows as f32);
            let truth = speed * (azimuth - toward).to_radians().cos();
            for gate in (0..gates).step_by(7) {
                if let Some(value) = grid.scaled_value(row, gate).filter(|v| v.is_finite()) {
                    worst = worst.max((value - truth).abs());
                }
            }
        }
        worst
    }

    fn uniform_env(
        speed: f32,
        toward_deg: f32,
        valid_time: DateTime<Utc>,
    ) -> EnvironmentalWindProfile {
        let (u, v) = (
            speed * toward_deg.to_radians().sin(),
            speed * toward_deg.to_radians().cos(),
        );
        EnvironmentalWindProfile {
            levels: vec![
                EnvWindLevel {
                    height_m_arl: 0.0,
                    u_mps: u,
                    v_mps: v,
                },
                EnvWindLevel {
                    height_m_arl: 15_000.0,
                    u_mps: u,
                    v_mps: v,
                },
            ],
            valid_time,
        }
    }

    // ---- hybrid test-suite scenarios ported to v4 (spec Stage 5) ----

    #[test]
    fn v4_temporal_reference_recovers_a_topmost_aliased_high_tilt() {
        let base = utc(1);
        let mut previous = RadarVolume::new(RadarSite::new("TEST"), base);
        previous.cuts = vec![
            wind_cut(1.23, 35.0, 180.0, 20.0, 360, 120),
            wind_cut(2.4, 35.0, 180.0, 40.0, 360, 120),
        ];
        let mut current = RadarVolume::new(RadarSite::new("TEST"), base + Duration::minutes(5));
        current.cuts = vec![wind_cut(1.23, 35.0, 180.0, 20.0, 360, 120)];

        let grid = dealias_velocity_grid_v4(&current, 0, Some(&previous), None).expect("v4");
        assert!(
            wind_error(&grid, 360, 120, 35.0, 180.0) < 2.0,
            "temporal prior must recover the branch"
        );
    }

    #[test]
    fn v4_lower_current_tilt_can_reference_a_folded_higher_tilt() {
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), utc(2));
        volume.cuts = vec![
            wind_cut(0.5, 35.0, 90.0, 40.0, 360, 100),
            wind_cut(1.23, 35.0, 90.0, 20.0, 720, 100),
        ];
        let grid = dealias_velocity_grid_v4(&volume, 1, None, None).expect("v4");
        assert!(
            wind_error(&grid, 720, 100, 35.0, 90.0) < 2.0,
            "vertical evidence must recover the branch"
        );
    }

    #[test]
    fn v4_current_lower_tilt_fixes_an_isolated_high_tilt_branch() {
        let isolated = |elevation: f32, nyquist: f32| {
            velocity_cut(elevation, nyquist, 360, 80, move |row, gate| {
                if (80..=110).contains(&row) && (20..45).contains(&gate) {
                    wrap(35.0, nyquist)
                } else {
                    f32::NAN
                }
            })
        };
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), utc(3));
        volume.cuts = vec![isolated(0.5, 45.0), isolated(1.23, 20.0)];

        let grid = dealias_velocity_grid_v4(&volume, 1, None, None).expect("v4");
        assert!(
            (grid.scaled_value(90, 30).expect("value") - 35.0).abs() < 1.0,
            "vertical evidence must choose the +1 branch"
        );
        assert!(
            grid.scaled_value(20, 30).is_none(),
            "must not invent gates outside native coverage"
        );
    }

    #[test]
    fn v4_repairs_a_folded_patch_fused_into_legitimate_inbound() {
        let patch = |elevation: f32, nyquist: f32| {
            velocity_cut(elevation, nyquist, 360, 100, move |row, gate| {
                let truth = if (120..=180).contains(&row) && (30..=70).contains(&gate) {
                    35.0
                } else {
                    -5.0
                };
                wrap(truth, nyquist)
            })
        };
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), utc(4));
        volume.cuts = vec![patch(0.5, 45.0), patch(1.23, 20.0)];

        let grid = dealias_velocity_grid_v4(&volume, 1, None, None).expect("v4");
        assert!(
            (grid.scaled_value(150, 50).expect("patch") - 35.0).abs() < 1.0,
            "folded lobe must be recovered"
        );
        assert!(
            (grid.scaled_value(60, 50).expect("background") + 5.0).abs() < 1.0,
            "legitimate inbound background must stay"
        );
    }

    #[test]
    fn v4_stale_temporal_volume_is_ignored() {
        let base = utc(5);
        let mut previous = RadarVolume::new(RadarSite::new("TEST"), base);
        previous.cuts = vec![wind_cut(1.23, 35.0, 180.0, 40.0, 360, 80)];
        let mut current = RadarVolume::new(RadarSite::new("TEST"), base + Duration::minutes(30));
        current.cuts = vec![wind_cut(1.23, 15.0, 180.0, 25.0, 360, 80)];

        let with_stale = dealias_velocity_grid_v4(&current, 0, Some(&previous), None).expect("v4");
        let without = dealias_velocity_grid_v4(&current, 0, None, None).expect("v4");
        assert_eq!(with_stale.storage, without.storage);
    }

    // ---- v4-specific stage tests ----

    /// F3 reproduction: an internally-consistent aliased island attached to
    /// the main field by ONE low-support (weak) edge whose vote says
    /// "same branch".  Without external evidence v4 must reproduce v1
    /// exactly (graceful degradation); with the environmental profile the
    /// island must rebranch.
    #[test]
    fn v4_weak_edge_subgraph_rebranches_only_with_environmental_evidence() {
        let nyquist = 20.0;
        // Main field: gates 0..30 everywhere at −5 (plus a 5-row bridge at
        // gate 30).  Island: rows 100..140, gates 31..61 at +12 (truth −28).
        let cut = velocity_cut(0.5, nyquist, 360, 70, move |row, gate| {
            if gate < 30 {
                -5.0
            } else if gate == 30 {
                if (118..123).contains(&row) {
                    -5.0
                } else {
                    f32::NAN
                }
            } else if (100..140).contains(&row) && (31..61).contains(&gate) {
                12.0
            } else {
                f32::NAN
            }
        });
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), utc(6));
        volume.cuts = vec![cut];

        let v1 = {
            let cut = &volume.cuts[0];
            dealias_velocity_grid(cut, cut.moments.get(&MomentType::Velocity).expect("vel"))
        };
        assert!(
            (v1.scaled_value(120, 45).expect("v1 island") - 12.0).abs() < 1.0,
            "v1 must exhibit the F3 misbranch for this test to be meaningful"
        );

        let no_env = dealias_velocity_grid_v4(&volume, 0, None, None).expect("v4");
        assert_eq!(
            no_env.storage, v1.storage,
            "without external evidence v4 must degrade to v1 exactly"
        );

        // Environment: 28 m/s toward azimuth 300° ⇒ v̂ ≈ −28 in the island's
        // sector (az 100–140°) and mildly positive/negative elsewhere.
        let env = uniform_env(28.0, 300.0, volume.volume_time);
        let with_env = dealias_velocity_grid_v4(&volume, 0, None, Some(&env)).expect("v4");
        assert!(
            (with_env.scaled_value(120, 45).expect("island") + 28.0).abs() < 1.0,
            "the weak-edge island must rebranch to −28"
        );
        assert!(
            (with_env.scaled_value(200, 10).expect("main") + 5.0).abs() < 1.0,
            "the main field must not move"
        );
    }

    /// F5 reproduction: a 30 m/s uniform wind under Nyquist 20 aliases both
    /// tilts identically, so the vertical pairwise term is branch-degenerate
    /// (shifting both tilts together costs nothing) and v1's largest-region
    /// anchor lands on a WRAPPED band.  Only the environmental anchor can
    /// decide; with it, both tilts must land on the true wind everywhere.
    #[test]
    fn v4_branch_degenerate_volume_is_decided_by_the_environment() {
        let nyquist = 20.0;
        let make = |elevation: f32| wind_cut(elevation, 30.0, 0.0, nyquist, 360, 80);
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), utc(7));
        volume.cuts = vec![make(0.5), make(1.4)];

        // Probe azimuth 0 (row 0): truth 30, wrapped observation −10.
        // Without env the absolute branch is under-determined (the anchor
        // may land on either a wrapped or an unwrapped band — segmentation
        // detail, not evidence): the output must be a whole-2N multiple of
        // the observation, nothing in between.
        let no_env = dealias_velocity_grid_v4(&volume, 0, None, None).expect("v4");
        let probed = no_env.scaled_value(0, 40).expect("no env");
        let folds = (probed + 10.0) / 40.0;
        assert!(
            (folds - folds.round()).abs() < 0.05,
            "no-env output must sit a whole number of folds from the raw value, got {probed}"
        );

        let env = uniform_env(30.0, 0.0, volume.volume_time);
        let with_env = dealias_velocity_grid_v4(&volume, 0, None, Some(&env)).expect("v4");
        assert!(
            wind_error(&with_env, 360, 80, 30.0, 0.0) < 2.0,
            "env anchor must unfold the whole tilt"
        );
        let upper = dealias_volume_v4(&volume, None, Some(&env));
        let upper_grid = upper.tilt_grid(1).expect("upper tilt");
        assert!(
            wind_error(upper_grid, 360, 80, 30.0, 0.0) < 2.5,
            "both tilts must move together (volume consistency)"
        );
    }

    /// A stale profile must behave exactly like no profile (spec §4a).
    #[test]
    fn v4_stale_environment_profile_is_ignored() {
        let nyquist = 20.0;
        let make = |elevation: f32| {
            velocity_cut(elevation, nyquist, 360, 80, move |_, _| wrap(30.0, nyquist))
        };
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), utc(8));
        volume.cuts = vec![make(0.5)];

        let stale = uniform_env(30.0, 0.0, volume.volume_time - Duration::hours(4));
        let with_stale = dealias_velocity_grid_v4(&volume, 0, None, Some(&stale)).expect("v4");
        let without = dealias_velocity_grid_v4(&volume, 0, None, None).expect("v4");
        assert_eq!(with_stale.storage, without.storage);
    }

    /// Determinism pin (spec §5.4): identical inputs ⇒ byte-identical grids
    /// and confidence across repeated solves.
    #[test]
    fn v4_solve_is_deterministic_across_runs() {
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), utc(9));
        volume.cuts = vec![
            wind_cut(0.5, 35.0, 90.0, 40.0, 360, 100),
            wind_cut(1.23, 35.0, 90.0, 20.0, 720, 100),
        ];
        let env = uniform_env(35.0, 90.0, volume.volume_time);
        let first = dealias_volume_v4(&volume, None, Some(&env));
        let second = dealias_volume_v4(&volume, None, Some(&env));
        for cut_index in 0..volume.cuts.len() {
            assert_eq!(
                first.tilt_grid(cut_index).map(|grid| &grid.storage),
                second.tilt_grid(cut_index).map(|grid| &grid.storage),
                "grids must be byte-identical (cut {cut_index})"
            );
            assert_eq!(
                first.tilt_confidence(cut_index).map(ConfidenceGrid::values),
                second
                    .tilt_confidence(cut_index)
                    .map(ConfidenceGrid::values),
                "confidence must be byte-identical (cut {cut_index})"
            );
        }
    }

    /// Confidence output sanity: solved nodes with external evidence carry a
    /// margin-derived confidence; the temporal consumer can read it back.
    #[test]
    fn v4_confidence_grid_reflects_decision_margins() {
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), utc(10));
        volume.cuts = vec![wind_cut(0.5, 30.0, 0.0, 40.0, 360, 60)];
        let env = uniform_env(30.0, 0.0, volume.volume_time);
        let solution = dealias_volume_v4(&volume, None, Some(&env));
        let confidence = solution.tilt_confidence(0).expect("confidence");
        let sampled = confidence.value(10, 30).expect("gate");
        assert!(
            sampled > confidence::INTERIOR_ONLY,
            "an env-covered unambiguous field must be confident, got {sampled}"
        );
        assert_eq!(solution.diagnostics().velocity_tilts, 1);
        assert!(solution.diagnostics().env_profile_used);
    }
}
