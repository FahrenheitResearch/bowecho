//! Super-regions: strong/weak edge classification over the v1 vote graph.
//!
//! The v1 solver welds every resolved vote edge into a rigid body, so one
//! spurious low-support contact can misbranch an internally-consistent
//! subgraph (failure F3, `docs/dealias-fold-branch-analysis.md` root-cause 4:
//! KMBX region 600, 826 gates, +1 fold → +29.1 m/s outbound, wrong).  v4
//! keeps only STRONG edges as rigid unions; everything else becomes a soft
//! pairwise term in the volume energy (spec §5.1), so external evidence can
//! pull a misbranched subgraph back independently.
//!
//! INVARIANT: super-region ids are assigned in region-id order and the weak
//! edge list preserves the deterministic order of `RegionSolve::edges`
//! (strongest boundary first, then region-pair) — the volume solve is
//! byte-reproducible.

use crate::region_core::RegionSolve;

/// A vote edge is STRONG when its majority fold has this much boundary
/// support...
pub(crate) const STRONG_EDGE_MIN_SUPPORT: u32 = 12;
/// ...and this share of all votes on the edge.  Everything else is weak —
/// including every edge class the F3 post-mortem showed can silently
/// misbranch a subgraph.
pub(crate) const STRONG_EDGE_MIN_SHARE: f64 = 0.80;
/// Super-regions smaller than this skip the volume graph: they keep their
/// local v1 solve and are handled by the repair gauntlet (too few gates to
/// accumulate meaningful unary evidence).
pub(crate) const SUPERREGION_MIN_GATES: u64 = 64;
/// A seam is only unambiguous when its mean |Δv| sits within this many
/// folds of a whole fold count.  Near the half-fold mark every boundary
/// pair rounds the same (possibly wrong) way, so vote share alone reads as
/// unanimous — the measured KMBX weld class (mean jump ≈ 0.7 folds,
/// `docs/dealias-fold-branch-analysis.md` root-cause 4).  Such seams stay
/// soft no matter how much support they carry.
pub(crate) const STRONG_EDGE_MAX_FOLD_AMBIGUITY: f32 = 0.30;

/// A weak vote edge lifted to super-region granularity.  The energy charges
/// `λ_wk · share · min(|residual + k_hi − k_lo|, 2)` (spec §5.2): zero when
/// the final labels honor the observed boundary fold.
pub(crate) struct WeakEdge {
    pub(crate) lo_super: u32,
    pub(crate) hi_super: u32,
    /// `(v1 fold of hi region) − (v1 fold of lo region) − edge fold`: the
    /// boundary-fold violation already present in the v1 baseline, so the
    /// label term only charges *additional* violation.
    pub(crate) residual: i32,
    /// Winning-vote share of the edge, in (0, 1].
    pub(crate) share: f64,
}

/// Per-tilt super-region decomposition.
pub(crate) struct TiltSuperRegions {
    /// Super-region id per v1 region (compact, assigned in region-id order).
    pub(crate) super_of_region: Vec<u32>,
    /// Total gate count per super-region.
    pub(crate) super_gates: Vec<u64>,
    /// Weak edges whose endpoints landed in different super-regions.
    pub(crate) weak_edges: Vec<WeakEdge>,
}

impl TiltSuperRegions {
    pub(crate) fn super_count(&self) -> usize {
        self.super_gates.len()
    }
}

/// Build super-regions for one tilt from the v1 solve (spec §5.1): connected
/// components of regions over strong edges only, weak edges kept as soft
/// terms.  Interval-based region splitting (Helmus & Collis 2016) was tested
/// against F2 on the real KMBX volume and did not fix the branch (the bad
/// branch survives the vote graph — analysis doc root-cause 3), so v4 relies
/// on weak-edge softening + external unaries instead.
pub(crate) fn build_super_regions(solve: &RegionSolve) -> TiltSuperRegions {
    let region_count = solve.region_size.len();
    let mut union = crate::region_core::UnionFind::new(region_count);
    for edge in &solve.edges {
        if edge_is_strong(edge) {
            union.union(edge.lo, edge.hi);
        }
    }

    // Compact super ids in region-id order (deterministic).
    let mut super_of_region = vec![u32::MAX; region_count];
    let mut super_gates: Vec<u64> = Vec::new();
    let mut root_to_super: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for rid in 0..region_count as u32 {
        let root = union.find(rid);
        let next = super_gates.len() as u32;
        let sid = *root_to_super.entry(root).or_insert_with(|| {
            super_gates.push(0);
            next
        });
        super_of_region[rid as usize] = sid;
        super_gates[sid as usize] += u64::from(solve.region_size[rid as usize]);
    }

    // Weak edges between distinct super-regions become soft pairwise terms.
    // Seams whose mean jump is a rounding coin-flip carry NO branch
    // information and are dropped outright: their "residual repair" pressure
    // otherwise mass-shifts split subtrees toward whichever way the noise
    // rounded (measured on KEAX after interval splitting: 264 → 3090
    // boundary pairs without an environmental profile).
    let mut weak_edges = Vec::new();
    for edge in &solve.edges {
        if edge_is_strong(edge) {
            continue;
        }
        if (edge.mean_jump_folds - edge.mean_jump_folds.round()).abs()
            > STRONG_EDGE_MAX_FOLD_AMBIGUITY
        {
            continue;
        }
        let lo_super = super_of_region[edge.lo as usize];
        let hi_super = super_of_region[edge.hi as usize];
        if lo_super == hi_super {
            continue; // intra-super violation is constant under the labels
        }
        let residual =
            solve.region_fold[edge.hi as usize] - solve.region_fold[edge.lo as usize] - edge.fold;
        let share = if edge.total_votes > 0 {
            f64::from(edge.winning_votes) / f64::from(edge.total_votes)
        } else {
            0.0
        };
        weak_edges.push(WeakEdge {
            lo_super,
            hi_super,
            residual,
            share,
        });
    }

    TiltSuperRegions {
        super_of_region,
        super_gates,
        weak_edges,
    }
}

/// Strong = high support, high vote share, AND an unambiguous jump (see the
/// constants above): only such seams may rigidly weld two regions.
fn edge_is_strong(edge: &crate::region_core::RegionEdge) -> bool {
    let unambiguous = (edge.mean_jump_folds - edge.mean_jump_folds.round()).abs()
        <= STRONG_EDGE_MAX_FOLD_AMBIGUITY;
    edge.winning_votes >= STRONG_EDGE_MIN_SUPPORT
        && f64::from(edge.winning_votes) >= STRONG_EDGE_MIN_SHARE * f64::from(edge.total_votes)
        && unambiguous
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region_core::solve_region_folds;

    /// Two coherent blocks joined by a single-gate-pair contact: the lone
    /// edge is weak (support 1 < 12), so the blocks land in different
    /// super-regions and the contact survives only as a soft weak edge.
    #[test]
    fn single_contact_edge_is_weak_and_splits_super_regions() {
        let rows = 8;
        let gates = 8;
        let nyq = vec![20.0f32; rows];
        let azimuths: Vec<f32> = (0..rows).map(|r| r as f32).collect();
        let mut observed = vec![f32::NAN; rows * gates];
        // Left block: value −5 in gates 0..3, all rows. Right block: +12 in
        // gates 5..8. One bridge gate pair at row 0, gates 3/4/5 → exactly one
        // shared boundary pair between the two regions... bridge value chosen
        // inside REGION_JOIN_FRAC of the left block only.
        for row in 0..rows {
            for gate in 0..3 {
                observed[row * gates + gate] = -5.0;
            }
            for gate in 5..8 {
                observed[row * gates + gate] = 12.0;
            }
        }
        observed[3] = -3.0; // row 0 gate 3: joins left block (|Δ| = 2 < 10)
        // row 0 gate 4: joins right block (|Δ| = 4 < 10); the 3|4 contact is
        // a boundary pair (|Δ| = 11 > 10) whose jump (0.275 folds) is still
        // unambiguous, so it survives as a WEAK edge (1 vote < 12 support).
        observed[4] = 8.0;
        for row in 0..rows {
            for gate in 5..8 {
                observed[row * gates + gate] = 8.0; // keep the right block joined
            }
        }

        let solve = solve_region_folds(&observed, &nyq, rows, gates, &azimuths);
        assert_eq!(solve.region_size.len(), 2, "two regions expected");
        let supers = build_super_regions(&solve);
        assert_eq!(supers.super_count(), 2, "weak edge must not weld");
        assert_eq!(supers.weak_edges.len(), 1);
        assert!((supers.weak_edges[0].share - 1.0).abs() < 1e-9);
    }

    /// A long shared boundary with a unanimous fold vote is strong: the two
    /// regions weld into one super-region and no weak edge remains.
    #[test]
    fn high_support_edge_welds_a_super_region() {
        let rows = 16;
        let gates = 8;
        let nyq = vec![20.0f32; rows];
        let azimuths: Vec<f32> = (0..rows).map(|r| r as f32).collect();
        let mut observed = vec![f32::NAN; rows * gates];
        for row in 0..rows {
            for gate in 0..4 {
                observed[row * gates + gate] = -18.0;
            }
            for gate in 4..8 {
                observed[row * gates + gate] = 15.0; // fold: −18 vs +15 → Δ=33 ≈ 2N
            }
        }
        let solve = solve_region_folds(&observed, &nyq, rows, gates, &azimuths);
        assert_eq!(solve.region_size.len(), 2);
        // 16 shared boundary pairs, unanimous fold −1 vote → strong.
        let supers = build_super_regions(&solve);
        assert_eq!(supers.super_count(), 1, "strong edge must weld");
        assert!(supers.weak_edges.is_empty());
    }
}
