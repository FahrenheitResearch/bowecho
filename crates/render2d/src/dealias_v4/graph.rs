//! Evidence tables for the volume-joint branch energy (spec §5.2).
//!
//! Unknowns: one integer branch shift k ∈ {−2…+2} per super-region, applied
//! on top of its v1-anchored fold.  Evidence enters as:
//!
//! - **environmental unary** — capped mean |unfolded − v̂_env| against the
//!   caller-supplied wind profile (4DD sounding initialization, James & Houze
//!   2001, *JTECH* 18, 1674–1683; ORPG environmental winds precedent,
//!   Eilts & Smith 1990, *JTECH* 7, 118–128).  Robust (capped at
//!   [`LOSS_CAP_MPS`]) and low-weight so storm-scale deviations from the
//!   environment cannot buy a wrong branch — it decides only where
//!   storm-scale evidence is silent or branch-degenerate (failure F5).
//! - **temporal unary** — capped mean against the previous volume's solution
//!   mapped onto this tilt's lattice (the 4DD previous-volume idea), weighted
//!   by the previous solution's mean confidence over the mapped gates.  This
//!   confidence weighting is the designed break for the F6 GIGO loop: a
//!   persistent misbranch carries a low margin, so its testimony is
//!   discounted and the environmental term decides.
//! - **vertical pairwise** — capped mean |unfolded_a − unfolded_b| over
//!   co-located gates of vertically adjacent velocity tilts (≤ 3.0° apart),
//!   evaluated for all 25 label pairs (the Jing & Wiener 1993 global-solve
//!   lesson lifted from sweep scope to volume scope — failure F12).
//!   Same-elevation SAILS revisits get the same table with the temporal
//!   weight (they are separate times, not heights).
//! - **weak-edge pairwise** — the soft residual of every non-strong vote
//!   edge (spec §5.1, see `super_regions.rs`).
//!
//! All costs are in m/s so the λ weights are dimensionless.  Coverage enters
//! through one smooth saturation `w(cov) = cov/(cov + 64)` so 20 covered
//! gates don't outvote 20,000.
//!
//! DETERMINISM: every accumulation runs in row-major gate order per tilt /
//! per pair; parallelism is only across tilts and pairs, whose results merge
//! in fixed order.

use rayon::prelude::*;

use super::TiltField;
use super::env_profile::{EnvironmentalWindProfile, project_profile_to_tilt};

/// Provisional weights (spec §5.2): current-time observation > continuity
/// from t−1 > model prior > ambiguous boundary.  The env term still wins when
/// everything else is silent (KMBX: temporal confidence low, vertical tilts
/// equally folded ⇒ C_vrt is branch-degenerate — shifting both tilts together
/// costs nothing — so the env unary is the only symmetry breaker; that
/// degeneracy is why v2/v3 fail F5 and why λ_env need not be large).
pub(crate) const LAMBDA_VRT: f64 = 1.0;
pub(crate) const LAMBDA_TMP: f64 = 0.8;
pub(crate) const LAMBDA_ENV: f64 = 0.6;
pub(crate) const LAMBDA_WK: f64 = 0.4;
/// Tie-break unary `λ_anchor·|k|` applied when NO environmental profile is
/// usable, so the solve reproduces v1's "largest region = fold 0" anchoring
/// when no external evidence exists (graceful degradation, pinned by test).
pub(crate) const LAMBDA_ANCHOR: f64 = 0.01;
/// Hard cap on the environmental unary's per-node label margin (max − min
/// over the 5 labels, in weighted m/s).  The profile is ONE spatially
/// correlated observation, not an independent measurement per super-region:
/// without this cap a vertically stacked column of wrapped nodes collects
/// the same (possibly wrong) model vote once per node and can outvote the
/// volume's single clean storm-scale anchor — measured on KMBX 2026-06-09
/// 23:54, where the RAP site profile is warm-sector southerly while the
/// aliased sector sits behind the gust front.  4 m/s is decisively above
/// every tie-break scale (weak edges ≤ 0.4, anchor 0.01) and decisively
/// below any real storm-scale evidence margin, which is exactly the spec's
/// §4a intent: the profile decides branches only where storm-scale evidence
/// is silent or degenerate.
pub(crate) const ENV_UNARY_MAX_MARGIN_MPS: f64 = 4.0;
/// Robust cap on every per-gate evidence residual (m/s) — the existing
/// `DENSE_REFERENCE_LOSS_CAP_MPS`.
pub(crate) const LOSS_CAP_MPS: f64 = 40.0;
/// `w(cov) = cov / (cov + 64)`: one smooth coverage saturation, no magic
/// curve.
pub(crate) const COVERAGE_HALF_GATES: f64 = 64.0;
/// Temporal evidence is included per node only when it is DECISIVE for a
/// branch change, mirroring the hybrid's guarded dense-reference overrides
/// (`DENSE_REFERENCE_GROUP_IMPROVEMENT_MPS` / `_RATIO`): the mapped prior
/// carries storm-motion displacement noise that is largest exactly on the
/// near-Nyquist interval strips, and letting a low-confidence prior vote on
/// every marginal node measurably degrades the no-profile solve (KEAX:
/// 280 → 512 solver-level boundary pairs).  A prior that merely AGREES with
/// k = 0 adds nothing the v1 anchor doesn't already say.
pub(crate) const TEMPORAL_MIN_IMPROVEMENT_MPS: f64 = 5.0;
pub(crate) const TEMPORAL_MAX_BEST_RATIO: f64 = 0.75;
/// Vertical evidence pairs tilts no further apart than this (the existing
/// `SPATIAL_REFERENCE_MAX_ELEVATION_DELTA_DEG`).
pub(crate) const VERTICAL_MAX_ELEVATION_DELTA_DEG: f32 = 3.0;
/// Below this elevation gap two cuts are the same nominal tilt (SAILS /
/// MESO-SAILS revisit) and pair temporally, not vertically.
pub(crate) const SIBLING_ELEVATION_DELTA_DEG: f32 = 0.12;
/// Row/gate stride for pairwise co-location sampling.  Statistics per
/// super-region pair stay in the thousands of samples on real sweeps while
/// the 25-label inner loop cost drops 4×; coverage weights use the sampled
/// count, so the saturation scale is unchanged in spirit.
const PAIR_SAMPLE_STRIDE: usize = 2;

pub(crate) fn coverage_weight(covered: u64) -> f64 {
    let covered = covered as f64;
    covered / (covered + COVERAGE_HALF_GATES)
}

/// Previous-volume solution mapped onto one target tilt's lattice.
pub(crate) struct MappedPrior {
    /// Dealiased velocity per target gate (NaN where unmapped).
    pub(crate) values: Vec<f32>,
    /// Previous solution confidence per target gate, in [0, 1].
    pub(crate) confidence: Vec<f32>,
}

pub(crate) struct PairwiseEdge {
    pub(crate) a: usize,
    pub(crate) b: usize,
    /// cost\[slot_a * 5 + slot_b\], fully weighted.
    pub(crate) table: [f64; 25],
    /// Coverage-scaled weight for maximum-spanning-forest ordering.
    pub(crate) strength: f64,
}

pub(crate) struct EvidenceTables {
    /// Fully weighted unary cost per node and label slot.
    pub(crate) unary: Vec<[f64; 5]>,
    /// Whether any external evidence (env / temporal / vertical) covered the
    /// node — nodes without it get the fixed "interior-consistent"
    /// confidence, not a margin-derived one (spec §8).
    pub(crate) has_external: Vec<bool>,
    /// Whether the node carries ABSOLUTE branch evidence (env or temporal
    /// unary coverage).  Pairwise terms constrain only RELATIVE branches —
    /// and not even that cleanly across tilts with different Nyquist, where
    /// a joint whole-component shift changes inter-tilt alignment by 2·ΔN
    /// and can masquerade as an alignment fix.  Components with no
    /// absolutely-anchored node are therefore renormalized after the solve
    /// (largest node → k = 0, v1's anchoring convention) — the measured
    /// failure was KEAX without a profile: seven ~50k-gate nodes drifting to
    /// k = −1 on pure pairwise energy.
    pub(crate) has_absolute: Vec<bool>,
    /// Total gates per node (for the renormalization anchor choice).
    pub(crate) node_gates: Vec<u64>,
    /// Mean Nyquist velocity over the node's gates.  The renormalization
    /// anchor prefers the HIGHEST-Nyquist node: a higher unambiguous range
    /// wraps less (the tilt-cascade doctrine; Louf et al. 2020 initialize
    /// from the least-aliased references for the same reason).  Note that a
    /// "fraction of gates near ±N" score cannot serve here: wrapping maps
    /// fast flow back into the interior, so a heavily aliased field looks
    /// deceptively far from its Nyquist.
    pub(crate) node_nyquist: Vec<f64>,
    /// Fraction of the node's gates within 20% of its Nyquist — the
    /// tie-break "how folded does the boundary structure look" score.
    pub(crate) node_near_nyquist: Vec<f64>,
    pub(crate) edges: Vec<PairwiseEdge>,
}

/// Build the full evidence table set for the volume.
pub(crate) fn build_evidence(
    tilts: &[TiltField],
    node_count: usize,
    environment: Option<&EnvironmentalWindProfile>,
    temporal: &[Option<MappedPrior>],
) -> EvidenceTables {
    let mut unary = vec![[0.0f64; 5]; node_count];
    let mut has_external = vec![false; node_count];
    let mut has_absolute = vec![false; node_count];
    let mut node_gates = vec![0u64; node_count];
    for tilt in tilts {
        for (sid, &node) in tilt.node_of_super.iter().enumerate() {
            if node != usize::MAX {
                node_gates[node] = tilt.supers.super_gates[sid];
            }
        }
    }

    // ---- unaries (env + temporal), parallel across tilts ----
    // Each tilt's nodes are disjoint, so per-tilt partials merge trivially;
    // merging in tilt order keeps the result independent of thread count.
    let partials: Vec<Vec<NodeUnary>> = tilts
        .par_iter()
        .zip(temporal.par_iter())
        .map(|(tilt, prior)| tilt_unaries(tilt, environment, prior.as_ref()))
        .collect();
    let mut node_near_nyquist = vec![0.0f64; node_count];
    let mut node_nyquist = vec![0.0f64; node_count];
    for (tilt, partial) in tilts.iter().zip(&partials) {
        for (local, acc) in partial.iter().enumerate() {
            let node = tilt.node_of_super[local];
            if node == usize::MAX {
                continue;
            }
            if acc.gates > 0 {
                node_near_nyquist[node] = acc.near_nyquist as f64 / acc.gates as f64;
                node_nyquist[node] = acc.nyquist_sum / acc.gates as f64;
            }
            if acc.env_covered > 0 {
                let weight = LAMBDA_ENV * coverage_weight(acc.env_covered);
                let samples = acc.env_covered as f64;
                let mut env_term = [0.0f64; 5];
                for (slot, term) in env_term.iter_mut().enumerate() {
                    *term = weight * acc.env_cost[slot] / samples;
                }
                // Bound the profile's per-node influence (see
                // ENV_UNARY_MAX_MARGIN_MPS).
                let low = env_term.iter().copied().fold(f64::INFINITY, f64::min);
                let high = env_term.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let margin = high - low;
                let scale = if margin > ENV_UNARY_MAX_MARGIN_MPS {
                    ENV_UNARY_MAX_MARGIN_MPS / margin
                } else {
                    1.0
                };
                for (slot, term) in env_term.iter().enumerate() {
                    unary[node][slot] += (term - low) * scale;
                }
                has_external[node] = true;
                has_absolute[node] = true;
            }
            if acc.tmp_covered > 0 {
                let samples = acc.tmp_covered as f64;
                let mut means = [0.0f64; 5];
                for (slot, mean) in means.iter_mut().enumerate() {
                    *mean = acc.tmp_cost[slot] / samples;
                }
                let best = means.iter().copied().fold(f64::INFINITY, f64::min);
                let current = means[2];
                let decisive = best < current
                    && current - best >= TEMPORAL_MIN_IMPROVEMENT_MPS
                    && best <= TEMPORAL_MAX_BEST_RATIO * current;
                if decisive {
                    let confidence = acc.tmp_confidence_sum / samples;
                    let weight = LAMBDA_TMP * confidence * coverage_weight(acc.tmp_covered);
                    for (slot, mean) in means.iter().enumerate() {
                        unary[node][slot] += weight * mean;
                    }
                    has_external[node] = true;
                    has_absolute[node] = true;
                }
            }
        }
    }

    // Anchor tie-break only without a usable environmental profile (§5.2).
    if environment.is_none() {
        for node_unary in unary.iter_mut() {
            for (slot, cost) in node_unary.iter_mut().enumerate() {
                *cost += LAMBDA_ANCHOR * (slot as f64 - 2.0).abs();
            }
        }
    }

    // ---- pairwise: vertical ladder + SAILS siblings ----
    let pairs = tilt_pairs(tilts);
    let pair_tables: Vec<Vec<PairwiseEdge>> = pairs
        .par_iter()
        .map(|pair| pair_evidence(&tilts[pair.lower], &tilts[pair.upper], pair.weight))
        .collect();
    let mut edges = Vec::new();
    for table in pair_tables {
        for edge in &table {
            has_external[edge.a] = true;
            has_external[edge.b] = true;
        }
        edges.extend(table);
    }

    // ---- pairwise: weak vote edges within each tilt ----
    // Multiple weak region contacts between the same two super-regions merge
    // into one edge (costs add).
    for tilt in tilts {
        let mut merged: std::collections::HashMap<(usize, usize), ([f64; 25], f64)> =
            std::collections::HashMap::new();
        for weak in &tilt.supers.weak_edges {
            let node_lo = tilt.node_of_super[weak.lo_super as usize];
            let node_hi = tilt.node_of_super[weak.hi_super as usize];
            if node_lo == usize::MAX || node_hi == usize::MAX || node_lo == node_hi {
                continue;
            }
            let (a, b, flip) = if node_lo < node_hi {
                (node_lo, node_hi, false)
            } else {
                (node_hi, node_lo, true)
            };
            let entry = merged.entry((a, b)).or_insert(([0.0; 25], 0.0));
            for slot_a in 0..5i32 {
                for slot_b in 0..5i32 {
                    // k_lo/k_hi from the (a, b) slot orientation.
                    let (k_lo, k_hi) = if flip {
                        (slot_b - 2, slot_a - 2)
                    } else {
                        (slot_a - 2, slot_b - 2)
                    };
                    let violation = (weak.residual + k_hi - k_lo).abs().min(2) as f64;
                    entry.0[(slot_a * 5 + slot_b) as usize] += LAMBDA_WK * weak.share * violation;
                }
            }
            entry.1 += LAMBDA_WK * weak.share;
        }
        let mut keys: Vec<(usize, usize)> = merged.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            let (table, strength) = merged.remove(&key).expect("key from map");
            edges.push(PairwiseEdge {
                a: key.0,
                b: key.1,
                table,
                strength,
            });
        }
    }

    EvidenceTables {
        unary,
        has_external,
        has_absolute,
        node_gates,
        node_nyquist,
        node_near_nyquist,
        edges,
    }
}

/// Raw (unweighted) unary accumulators for one super-region of one tilt.
#[derive(Clone)]
struct NodeUnary {
    env_cost: [f64; 5],
    env_covered: u64,
    tmp_cost: [f64; 5],
    tmp_covered: u64,
    tmp_confidence_sum: f64,
    gates: u64,
    near_nyquist: u64,
    nyquist_sum: f64,
}

impl NodeUnary {
    fn zero() -> Self {
        Self {
            env_cost: [0.0; 5],
            env_covered: 0,
            tmp_cost: [0.0; 5],
            tmp_covered: 0,
            tmp_confidence_sum: 0.0,
            gates: 0,
            near_nyquist: 0,
            nyquist_sum: 0.0,
        }
    }
}

fn tilt_unaries(
    tilt: &TiltField,
    environment: Option<&EnvironmentalWindProfile>,
    prior: Option<&MappedPrior>,
) -> Vec<NodeUnary> {
    let mut accumulators = vec![NodeUnary::zero(); tilt.supers.super_count()];
    let env_field = environment.map(|profile| {
        project_profile_to_tilt(
            profile,
            tilt.elevation_deg,
            &tilt.azimuths,
            tilt.first_gate_m,
            tilt.gate_spacing_m,
            tilt.gates,
        )
    });

    for row in 0..tilt.rows {
        let n = tilt.nyq[row];
        if !n.is_finite() || n <= 0.0 {
            continue;
        }
        let interval = 2.0 * n;
        for gate in 0..tilt.gates {
            let idx = row * tilt.gates + gate;
            let observed = tilt.observed[idx];
            if !observed.is_finite() {
                continue;
            }
            let rid = tilt.solve.region_of[idx];
            if rid == u32::MAX {
                continue;
            }
            let sid = tilt.supers.super_of_region[rid as usize] as usize;
            if tilt.node_of_super[sid] == usize::MAX {
                continue;
            }
            let base = tilt.solve.region_fold[rid as usize];
            let acc = &mut accumulators[sid];
            acc.gates += 1;
            acc.near_nyquist += u64::from(observed.abs() > 0.8 * n);
            acc.nyquist_sum += f64::from(n);
            if let Some(env) = env_field.as_ref() {
                let predicted = env[idx];
                if predicted.is_finite() {
                    acc.env_covered += 1;
                    for (slot, cost) in acc.env_cost.iter_mut().enumerate() {
                        let k = slot as i32 - 2;
                        let unfolded = observed + (base + k) as f32 * interval;
                        *cost += (f64::from((unfolded - predicted).abs())).min(LOSS_CAP_MPS);
                    }
                }
            }
            if let Some(prior) = prior {
                let predicted = prior.values[idx];
                if predicted.is_finite() {
                    acc.tmp_covered += 1;
                    acc.tmp_confidence_sum += f64::from(prior.confidence[idx]);
                    for (slot, cost) in acc.tmp_cost.iter_mut().enumerate() {
                        let k = slot as i32 - 2;
                        let unfolded = observed + (base + k) as f32 * interval;
                        *cost += (f64::from((unfolded - predicted).abs())).min(LOSS_CAP_MPS);
                    }
                }
            }
        }
    }
    accumulators
}

struct TiltPair {
    lower: usize,
    upper: usize,
    weight: f64,
}

/// The volume's tilt-pair ladder: consecutive distinct elevations pair
/// vertically (gap ≤ 3.0°); same-elevation revisits pair as siblings with
/// the temporal weight.
fn tilt_pairs(tilts: &[TiltField]) -> Vec<TiltPair> {
    let mut order: Vec<usize> = (0..tilts.len()).collect();
    order.sort_by(|a, b| {
        tilts[*a]
            .elevation_deg
            .total_cmp(&tilts[*b].elevation_deg)
            .then_with(|| tilts[*a].cut_index.cmp(&tilts[*b].cut_index))
    });
    let mut pairs = Vec::new();
    for window in order.windows(2) {
        let (lower, upper) = (window[0], window[1]);
        let gap = tilts[upper].elevation_deg - tilts[lower].elevation_deg;
        if gap < SIBLING_ELEVATION_DELTA_DEG {
            pairs.push(TiltPair {
                lower,
                upper,
                weight: LAMBDA_TMP,
            });
        } else if gap <= VERTICAL_MAX_ELEVATION_DELTA_DEG {
            pairs.push(TiltPair {
                lower,
                upper,
                weight: LAMBDA_VRT,
            });
        }
    }
    pairs
}

/// Accumulate the 25-label co-location tables between two tilts.  The upper
/// tilt is mapped onto the lower tilt's lattice by nearest azimuth row and
/// nearest range gate (the `hybrid.rs` mapping, reimplemented on raw arrays
/// so it can run before any `MomentGrid` is materialized).
fn pair_evidence(lower: &TiltField, upper: &TiltField, weight: f64) -> Vec<PairwiseEdge> {
    let row_map = nearest_row_map(&lower.azimuths, &upper.azimuths);
    let gate_map = nearest_gate_map(
        lower.first_gate_m,
        lower.gate_spacing_m,
        lower.gates,
        upper.first_gate_m,
        upper.gate_spacing_m,
        upper.gates,
    );

    let mut tables: std::collections::HashMap<(usize, usize), ([f64; 25], u64)> =
        std::collections::HashMap::new();
    for row in (0..lower.rows).step_by(PAIR_SAMPLE_STRIDE) {
        let n_lower = lower.nyq[row];
        if !n_lower.is_finite() || n_lower <= 0.0 {
            continue;
        }
        let Some(source_row) = row_map[row] else {
            continue;
        };
        let n_upper = upper.nyq[source_row];
        if !n_upper.is_finite() || n_upper <= 0.0 {
            continue;
        }
        let (interval_lower, interval_upper) = (2.0 * n_lower, 2.0 * n_upper);
        for gate in (0..lower.gates).step_by(PAIR_SAMPLE_STRIDE) {
            let idx = row * lower.gates + gate;
            let observed_lower = lower.observed[idx];
            if !observed_lower.is_finite() {
                continue;
            }
            let Some(source_gate) = gate_map[gate] else {
                continue;
            };
            let source_idx = source_row * upper.gates + source_gate;
            let observed_upper = upper.observed[source_idx];
            if !observed_upper.is_finite() {
                continue;
            }
            let node_a = node_at(lower, idx);
            let node_b = node_at(upper, source_idx);
            let (Some((node_a, base_a)), Some((node_b, base_b))) = (node_a, node_b) else {
                continue;
            };
            let entry = tables.entry((node_a, node_b)).or_insert(([0.0; 25], 0));
            entry.1 += 1;
            for slot_a in 0..5 {
                let unfolded_a =
                    observed_lower + (base_a + slot_a as i32 - 2) as f32 * interval_lower;
                for slot_b in 0..5 {
                    let unfolded_b =
                        observed_upper + (base_b + slot_b as i32 - 2) as f32 * interval_upper;
                    entry.0[slot_a * 5 + slot_b] +=
                        (f64::from((unfolded_a - unfolded_b).abs())).min(LOSS_CAP_MPS);
                }
            }
        }
    }

    let mut keys: Vec<(usize, usize)> = tables.keys().copied().collect();
    keys.sort_unstable();
    let mut edges = Vec::with_capacity(keys.len());
    for key in keys {
        let (sums, covered) = tables.remove(&key).expect("key from map");
        let scale = weight * coverage_weight(covered) / covered as f64;
        let (a, b, flip) = if key.0 <= key.1 {
            (key.0, key.1, false)
        } else {
            (key.1, key.0, true)
        };
        let mut table = [0.0f64; 25];
        for slot_a in 0..5 {
            for slot_b in 0..5 {
                let source = if flip {
                    sums[slot_b * 5 + slot_a]
                } else {
                    sums[slot_a * 5 + slot_b]
                };
                table[slot_a * 5 + slot_b] = scale * source;
            }
        }
        edges.push(PairwiseEdge {
            a,
            b,
            table,
            strength: weight * coverage_weight(covered),
        });
    }
    edges
}

/// (global node, v1 base fold) at a gate, if the gate belongs to a
/// graph-participating super-region.
#[inline]
fn node_at(tilt: &TiltField, idx: usize) -> Option<(usize, i32)> {
    let rid = tilt.solve.region_of[idx];
    if rid == u32::MAX {
        return None;
    }
    let sid = tilt.supers.super_of_region[rid as usize] as usize;
    let node = tilt.node_of_super[sid];
    if node == usize::MAX {
        return None;
    }
    Some((node, tilt.solve.region_fold[rid as usize]))
}

/// Nearest-azimuth row map (target row → source row), `None` beyond 2.5×
/// the typical source spacing.  Mirrors `hybrid.rs::nearest_azimuth_row_map`.
pub(crate) fn nearest_row_map(
    target_azimuths: &[f32],
    source_azimuths: &[f32],
) -> Vec<Option<usize>> {
    let mut source_rows: Vec<(f32, usize)> = source_azimuths
        .iter()
        .enumerate()
        .filter(|(_, azimuth)| azimuth.is_finite())
        .map(|(row, azimuth)| (azimuth.rem_euclid(360.0), row))
        .collect();
    source_rows.sort_by(|left, right| left.0.total_cmp(&right.0));
    if source_rows.is_empty() {
        return vec![None; target_azimuths.len()];
    }
    let max_gap = (typical_azimuth_spacing(&source_rows) * 2.5).clamp(0.75, 3.0);

    target_azimuths
        .iter()
        .map(|azimuth| {
            if !azimuth.is_finite() {
                return None;
            }
            let target = azimuth.rem_euclid(360.0);
            let position = source_rows.partition_point(|(source, _)| *source < target);
            let mut best: Option<(usize, f32)> = None;
            for candidate in [
                position.checked_sub(1),
                (position < source_rows.len()).then_some(position),
                Some(0),
                source_rows.len().checked_sub(1),
            ]
            .into_iter()
            .flatten()
            {
                let (source_azimuth, source_row) = source_rows[candidate];
                let distance = angular_distance_deg(target, source_azimuth);
                if best.is_none_or(|(_, best_distance)| distance < best_distance) {
                    best = Some((source_row, distance));
                }
            }
            best.and_then(|(row, distance)| (distance <= max_gap).then_some(row))
        })
        .collect()
}

fn typical_azimuth_spacing(rows: &[(f32, usize)]) -> f32 {
    if rows.len() < 2 {
        return 1.0;
    }
    let mut gaps = rows
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .filter(|gap| gap.is_finite() && *gap > 0.001 && *gap < 10.0)
        .collect::<Vec<_>>();
    if gaps.is_empty() {
        return (360.0 / rows.len() as f32).clamp(0.25, 2.0);
    }
    let middle = gaps.len() / 2;
    gaps.select_nth_unstable_by(middle, |left, right| left.total_cmp(right));
    gaps[middle].clamp(0.20, 2.0)
}

fn angular_distance_deg(left: f32, right: f32) -> f32 {
    ((left - right + 180.0).rem_euclid(360.0) - 180.0).abs()
}

/// Nearest-range gate map (target gate → source gate), `None` beyond 0.6×
/// the coarser spacing.  Mirrors `hybrid.rs::nearest_range_gate_map`.
pub(crate) fn nearest_gate_map(
    target_first_m: i32,
    target_spacing_m: i32,
    target_gates: usize,
    source_first_m: i32,
    source_spacing_m: i32,
    source_gates: usize,
) -> Vec<Option<usize>> {
    let target_first = f64::from(target_first_m);
    let target_spacing = f64::from(target_spacing_m.max(1));
    let source_first = f64::from(source_first_m);
    let source_spacing = f64::from(source_spacing_m.max(1));
    let tolerance = 0.60 * target_spacing.max(source_spacing) + 1.0;

    (0..target_gates)
        .map(|gate| {
            let target_range = target_first + gate as f64 * target_spacing;
            let source_position = (target_range - source_first) / source_spacing;
            let source_gate = source_position.round();
            if source_gate < 0.0 || source_gate >= source_gates as f64 {
                return None;
            }
            let source_range = source_first + source_gate * source_spacing;
            ((source_range - target_range).abs() <= tolerance).then_some(source_gate as usize)
        })
        .collect()
}
