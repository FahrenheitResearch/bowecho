//! Region segmentation + relative-fold solve shared by every dealias engine.
//!
//! This is the v1 region core extracted VERBATIM from `lib.rs`
//! (`region_based_dealias_folds` steps 1–4) so the v4 volume solver can reuse
//! the exact same segmentation without re-implementing it. INVARIANT: zero
//! behavior change — `dealias_velocity_grid` output is pinned by the existing
//! test suite (`region_dealias_recovers_smooth_folded_ramp` and friends) and
//! must be byte-identical before/after this extraction.
//!
//! Method (Jing & Wiener 1993, *JTECH* 10, 798–808; Helmus & Collis 2016,
//! Py-ART `dealias_region_based`, *JORS* 4(1):e25; in the spirit of
//! Feldmann et al. 2020 R2D2, *JTECH* 37, 2341–2356):
//!
//! 1. flood-fill connected regions whose neighbouring gates differ by less
//!    than [`REGION_JOIN_FRAC`]·Nyquist (no fold occurs *within* a region);
//! 2. build a region-adjacency graph whose edges carry the consensus integer
//!    Nyquist fold between the two regions (majority vote over all shared
//!    boundary gate pairs);
//! 3. resolve folds strongest-boundary-first through [`FoldUnionFind`]
//!    (a weighted/potential DSU accumulating `k[region] − k[root]`);
//! 4. anchor each connected group so its largest region is fold 0.

use crate::sweep_wraps;

/// Fraction of the Nyquist interval below which two adjacent gates are assumed
/// to belong to the same (unfolded) region. Comfortably below 1.0 so a true
/// fold (a jump of ~2·Nyquist) always lands on a region boundary.
pub(crate) const REGION_JOIN_FRAC: f32 = 0.5;
/// Hard cap on the integer fold count applied to any region.
pub(crate) const REGION_MAX_FOLD: i32 = 5;
/// Minimum shared-boundary support for an inter-region fold to be trusted.
/// One is enough: region interiors are already coherent, so a single boundary
/// pair is a valid estimate, and because edges resolve strongest-first a lone
/// spurious contact only ever resolves last (as a no-op cycle).
pub(crate) const REGION_EDGE_MIN_SUPPORT: u32 = 1;

/// Plain union-find for connected-component region labelling.
pub(crate) struct UnionFind {
    parent: Vec<u32>,
    rank: Vec<u8>,
}

impl UnionFind {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
        }
    }

    pub(crate) fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let parent = self.parent[x as usize];
            self.parent[x as usize] = self.parent[parent as usize]; // path halving
            x = self.parent[x as usize];
        }
        x
    }

    pub(crate) fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (ra, rb) = if self.rank[ra as usize] < self.rank[rb as usize] {
            (rb, ra)
        } else {
            (ra, rb)
        };
        self.parent[rb as usize] = ra;
        if self.rank[ra as usize] == self.rank[rb as usize] {
            self.rank[ra as usize] += 1;
        }
    }
}

/// Union-find that also tracks an integer fold offset to the group root, so we
/// can accumulate "region B is k Nyquist intervals above region A" relations
/// and solve them all consistently (a weighted/potential DSU).
pub(crate) struct FoldUnionFind {
    parent: Vec<u32>,
    rank: Vec<u8>,
    /// fold[x] = k[x] - k[parent[x]]
    offset: Vec<i32>,
}

impl FoldUnionFind {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
            offset: vec![0; n],
        }
    }

    /// Returns (root, k[x] - k[root]).
    pub(crate) fn find(&mut self, x: u32) -> (u32, i32) {
        let mut node = x;
        let mut total = 0;
        while self.parent[node as usize] != node {
            total += self.offset[node as usize];
            node = self.parent[node as usize];
        }
        (node, total)
    }

    /// Enforce k[b] - k[a] = rel by merging the two groups.
    pub(crate) fn union(&mut self, a: u32, b: u32, rel: i32) {
        let (ra, oa) = self.find(a);
        let (rb, ob) = self.find(b);
        if ra == rb {
            return;
        }
        // k[rb] - k[ra] = rel + oa - ob
        let delta = rel + oa - ob;
        if self.rank[ra as usize] < self.rank[rb as usize] {
            self.parent[ra as usize] = rb;
            self.offset[ra as usize] = -delta;
        } else {
            self.parent[rb as usize] = ra;
            self.offset[rb as usize] = delta;
            if self.rank[ra as usize] == self.rank[rb as usize] {
                self.rank[ra as usize] += 1;
            }
        }
    }
}

/// A resolved inter-region vote edge. `fold` means `k[hi] = k[lo] + fold`
/// (regions identified by compact id, `lo < hi`). `winning_votes` is the
/// support for the majority fold; `total_votes` counts every shared boundary
/// gate pair. The v4 super-region builder classifies edges as strong/weak
/// from these counts (spec §5.1).
pub(crate) struct RegionEdge {
    pub(crate) lo: u32,
    pub(crate) hi: u32,
    pub(crate) fold: i32,
    pub(crate) winning_votes: u32,
    pub(crate) total_votes: u32,
    /// Mean |Δv| across the shared boundary in units of 2·Nyquist (fold
    /// counts).  A seam whose mean jump sits near a half-fold (≈ 0.5, 1.5…)
    /// votes by rounding coin-flip: every pair may round the same way and
    /// still be wrong (`docs/dealias-fold-branch-analysis.md` root-cause 4).
    /// The v4 super-region builder refuses to weld such seams.
    pub(crate) mean_jump_folds: f32,
}

/// Everything the v1 relative-fold solve knows about one sweep. Consumed by
/// `region_based_dealias_folds` (steps 5b/5c reference passes) and by the v4
/// volume solver (super-region construction).
pub(crate) struct RegionSolve {
    /// Per-gate compact region id; `u32::MAX` where no finite velocity.
    pub(crate) region_of: Vec<u32>,
    /// Gate count per region.
    pub(crate) region_size: Vec<u32>,
    /// Anchored baseline fold per region ("largest region in the connected
    /// group = fold 0"), clamped to ±[`REGION_MAX_FOLD`].
    pub(crate) region_fold: Vec<i32>,
    /// Fold offset of the region relative to its vote-graph group root
    /// (`k[region] − k[root]`, unclamped).
    pub(crate) region_offset: Vec<i32>,
    /// Compact vote-graph group id per region (assigned in region-id order,
    /// deterministic).
    pub(crate) region_group: Vec<u32>,
    /// Number of distinct vote-graph groups.
    pub(crate) group_count: usize,
    /// Resolved inter-region edges, strongest boundary first (the union
    /// order), each carrying its vote counts.
    pub(crate) edges: Vec<RegionEdge>,
}

impl RegionSolve {
    fn empty() -> Self {
        Self {
            region_of: Vec::new(),
            region_size: Vec::new(),
            region_fold: Vec::new(),
            region_offset: Vec::new(),
            region_group: Vec::new(),
            group_count: 0,
            edges: Vec::new(),
        }
    }
}

/// Steps 1–4 of the region-based fold solve (see module doc). Extracted from
/// `region_based_dealias_folds` with zero behavior change: every accumulation
/// runs in row-major gate order and every sort carries a full deterministic
/// tie chain, so the fold assignment is reproducible across runs and thread
/// counts.
pub(crate) fn solve_region_folds(
    observed: &[f32],
    nyq: &[f32],
    rows: usize,
    gates: usize,
    azimuths: &[f32],
) -> RegionSolve {
    solve_region_folds_impl(observed, nyq, rows, gates, azimuths, None)
}

/// [`solve_region_folds`] with Py-ART-style velocity-interval splitting
/// (Helmus & Collis 2016, *JORS* 4(1):e25, `dealias_region_based`): two
/// gates join a region only when they also fall in the same velocity
/// interval of width `interval_frac`·Nyquist.  This cuts the gradual-noise
/// chains that weld a whole sweep into one mega-region (failure F2 — the
/// measured KMBX 166k-gate region), so genuinely different branches become
/// separate vote-graph nodes.  The seams reappear as ordinary vote edges and
/// smooth gradients weld back through the strong-edge rule; splitting alone
/// does NOT fix the branch (tested on the real volume — analysis doc
/// root-cause 3), which is why only the v4 volume solver uses it, feeding
/// the split regions into the evidence energy.
pub(crate) fn solve_region_folds_split(
    observed: &[f32],
    nyq: &[f32],
    rows: usize,
    gates: usize,
    azimuths: &[f32],
    interval_frac: f32,
) -> RegionSolve {
    solve_region_folds_impl(observed, nyq, rows, gates, azimuths, Some(interval_frac))
}

fn solve_region_folds_impl(
    observed: &[f32],
    nyq: &[f32],
    rows: usize,
    gates: usize,
    azimuths: &[f32],
    interval_frac: Option<f32>,
) -> RegionSolve {
    let total = rows.saturating_mul(gates);
    // Union-find nodes are indexed by `idx as u32`; bail (no unfolding) rather
    // than truncate if a grid were ever absurdly large. Real sweeps are ~1.3M
    // gates, far under u32::MAX.
    if total == 0 || observed.len() != total || total > u32::MAX as usize {
        return RegionSolve::empty();
    }

    let same_region = |a: usize, b: usize, n: f32| -> bool {
        if !(n.is_finite()
            && observed[a].is_finite()
            && observed[b].is_finite()
            && (observed[a] - observed[b]).abs() <= REGION_JOIN_FRAC * n)
        {
            return false;
        }
        match interval_frac {
            None => true,
            Some(frac) => {
                // Same velocity interval of width frac·N (intervals anchored
                // at −N so the quantization is deterministic).
                let width = frac * n;
                let bin_a = ((observed[a] + n) / width).floor();
                let bin_b = ((observed[b] + n) / width).floor();
                bin_a == bin_b
            }
        }
    };

    // ---- 1. label connected regions ----
    let mut labels = UnionFind::new(total);
    let wrap = sweep_wraps(azimuths);
    for row in 0..rows {
        let row_n = nyq[row];
        for gate in 0..gates {
            let idx = row * gates + gate;
            if !observed[idx].is_finite() {
                continue;
            }
            if gate + 1 < gates && same_region(idx, idx + 1, row_n) {
                labels.union(idx as u32, (idx + 1) as u32);
            }
            if row + 1 < rows {
                let down = (row + 1) * gates + gate;
                let n = row_n.min(nyq[row + 1]);
                if same_region(idx, down, n) {
                    labels.union(idx as u32, down as u32);
                }
            }
        }
    }
    if wrap {
        let n = nyq[rows - 1].min(nyq[0]);
        for gate in 0..gates {
            let a = (rows - 1) * gates + gate;
            let b = gate;
            if same_region(a, b, n) {
                labels.union(a as u32, b as u32);
            }
        }
    }

    // Compact region ids + sizes.
    let mut region_of = vec![u32::MAX; total];
    let mut region_size: Vec<u32> = Vec::new();
    for idx in 0..total {
        if !observed[idx].is_finite() {
            continue;
        }
        let root = labels.find(idx as u32);
        let rid = &mut region_of[root as usize];
        if *rid == u32::MAX {
            *rid = region_size.len() as u32;
            region_size.push(0);
        }
        let rid = *rid;
        region_of[idx] = rid;
        region_size[rid as usize] += 1;
    }
    let region_count = region_size.len();
    if region_count == 0 {
        return RegionSolve {
            region_of,
            ..RegionSolve::empty()
        };
    }

    // ---- 2. accumulate inter-region fold votes over shared boundaries ----
    // key: (lo_region, hi_region) -> map fold -> count, where fold f means
    // k[hi] = k[lo] + f, f = round((v_lo - v_hi) / (2·Nyquist)).
    let mut edges: std::collections::HashMap<
        (u32, u32),
        (std::collections::HashMap<i32, u32>, f64),
    > = std::collections::HashMap::new();
    let mut vote = |ra: u32, va: f32, rb: u32, vb: f32, n: f32| {
        if ra == rb || !n.is_finite() {
            return;
        }
        let (lo, vlo, hi, vhi) = if ra < rb {
            (ra, va, rb, vb)
        } else {
            (rb, vb, ra, va)
        };
        let jump_folds = (vlo - vhi) / (2.0 * n);
        let f = jump_folds.round() as i32;
        if f.abs() > 2 * REGION_MAX_FOLD {
            return;
        }
        let entry = edges.entry((lo, hi)).or_default();
        *entry.0.entry(f).or_insert(0) += 1;
        entry.1 += f64::from(jump_folds.abs());
    };
    for row in 0..rows {
        let row_n = nyq[row];
        for gate in 0..gates {
            let idx = row * gates + gate;
            let ra = region_of[idx];
            if ra == u32::MAX {
                continue;
            }
            if gate + 1 < gates {
                let rb = region_of[idx + 1];
                if rb != u32::MAX {
                    vote(ra, observed[idx], rb, observed[idx + 1], row_n);
                }
            }
            if row + 1 < rows {
                let down = (row + 1) * gates + gate;
                let rb = region_of[down];
                if rb != u32::MAX {
                    vote(
                        ra,
                        observed[idx],
                        rb,
                        observed[down],
                        row_n.min(nyq[row + 1]),
                    );
                }
            }
        }
    }
    if wrap {
        let n = nyq[rows - 1].min(nyq[0]);
        for gate in 0..gates {
            let a = (rows - 1) * gates + gate;
            let b = gate;
            let (ra, rb) = (region_of[a], region_of[b]);
            if ra != u32::MAX && rb != u32::MAX {
                vote(ra, observed[a], rb, observed[b], n);
            }
        }
    }

    // ---- 3. resolve folds, strongest shared boundary first ----
    let mut resolved: Vec<RegionEdge> = edges
        .into_iter()
        .filter_map(|(key, (votes, jump_sum))| {
            let total_support: u32 = votes.values().sum();
            // Tied vote counts must resolve deterministically (HashMap
            // iteration order varies per instance): prefer the smaller
            // |fold|, then the smaller fold.
            let (fold, support) = votes.into_iter().max_by_key(|(fold, count)| {
                (
                    *count,
                    std::cmp::Reverse(fold.abs()),
                    std::cmp::Reverse(*fold),
                )
            })?;
            (support >= REGION_EDGE_MIN_SUPPORT).then_some(RegionEdge {
                lo: key.0,
                hi: key.1,
                fold,
                winning_votes: support,
                total_votes: total_support,
                mean_jump_folds: if total_support > 0 {
                    (jump_sum / f64::from(total_support)) as f32
                } else {
                    0.0
                },
            })
        })
        .collect();
    // Strongest boundary first; tie-break on the (unique) region pair so the
    // union order — and therefore the unfolded field — is reproducible.
    resolved.sort_by(|a, b| {
        b.total_votes
            .cmp(&a.total_votes)
            .then_with(|| (a.lo, a.hi).cmp(&(b.lo, b.hi)))
    });

    let mut dsu = FoldUnionFind::new(region_count);
    for edge in &resolved {
        dsu.union(edge.lo, edge.hi, edge.fold);
    }

    // ---- 4. anchor each connected group so its largest region is unfolded ----
    // For every group root, remember the offset of the biggest region in it.
    let mut anchor_offset: std::collections::HashMap<u32, (u32, i32)> =
        std::collections::HashMap::new();
    for rid in 0..region_count as u32 {
        let (root, off) = dsu.find(rid);
        let size = region_size[rid as usize];
        anchor_offset
            .entry(root)
            .and_modify(|(best_size, best_off)| {
                if size > *best_size {
                    *best_size = size;
                    *best_off = off;
                }
            })
            .or_insert((size, off));
    }

    // Per-region fold = (region offset) - (anchor offset of its group), plus
    // compact group ids assigned in region-id order (this reproduces exactly
    // the `root_to_group` mapping the dense-reference pass used to build).
    let mut region_fold = vec![0i32; region_count];
    let mut region_offset = vec![0i32; region_count];
    let mut region_group = vec![0u32; region_count];
    let mut root_to_group: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for rid in 0..region_count as u32 {
        let (root, off) = dsu.find(rid);
        let anchor = anchor_offset.get(&root).map(|(_, o)| *o).unwrap_or(0);
        region_fold[rid as usize] = (off - anchor).clamp(-REGION_MAX_FOLD, REGION_MAX_FOLD);
        region_offset[rid as usize] = off;
        let next_group = root_to_group.len() as u32;
        region_group[rid as usize] = *root_to_group.entry(root).or_insert(next_group);
    }
    let group_count = root_to_group.len();

    RegionSolve {
        region_of,
        region_size,
        region_fold,
        region_offset,
        region_group,
        group_count,
        edges: resolved,
    }
}
