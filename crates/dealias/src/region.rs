//! The core region-based fold solver (steps 1–4 of the crate-level docs).
//!
//! Region growing + boundary voting follows the two-dimensional dealiasing
//! family of Jing & Wiener (1993, *JTECH* 10, 798–808) and Py-ART's
//! `dealias_region_based` (Helmus & Collis 2016, *J. Open Res. Softw.* 4(1),
//! e25, doi:10.5334/jors.119); resolving whole regions at once is the same
//! design principle as R2D2 (Feldmann et al. 2020, *JTECH* 37,
//! doi:10.1175/JTECH-D-20-0054.1). The external-reference branch checks are
//! in the spirit of UNRAVEL (Louf et al. 2020, *JTECH* 37(5), 741–758,
//! doi:10.1175/JTECH-D-19-0020.1).

use std::collections::HashMap;

use crate::reference::RangeBandReference;

/// Fraction of the Nyquist interval below which two adjacent gates are assumed
/// to belong to the same (unfolded) region. Comfortably below 1.0 so a true
/// fold (a jump of ~2·Nyquist) always lands on a region boundary.
const REGION_JOIN_FRAC: f32 = 0.5;
/// Hard cap on the integer fold count applied to any region.
const REGION_MAX_FOLD: i32 = 5;
/// Minimum shared-boundary support for an inter-region fold to be trusted.
/// One is enough: region interiors are already coherent, so a single boundary
/// pair is a valid estimate, and because edges resolve strongest-first a lone
/// spurious contact only ever resolves last (as a no-op cycle).
const REGION_EDGE_MIN_SUPPORT: u32 = 1;

/// Plain union-find for connected-component region labelling.
struct UnionFind {
    parent: Vec<u32>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let parent = self.parent[x as usize];
            self.parent[x as usize] = self.parent[parent as usize]; // path halving
            x = self.parent[x as usize];
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
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
struct FoldUnionFind {
    parent: Vec<u32>,
    rank: Vec<u8>,
    /// fold[x] = k[x] - k[parent[x]]
    offset: Vec<i32>,
}

impl FoldUnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
            offset: vec![0; n],
        }
    }

    /// Returns (root, k[x] - k[root]).
    fn find(&mut self, x: u32) -> (u32, i32) {
        let mut node = x;
        let mut total = 0;
        while self.parent[node as usize] != node {
            total += self.offset[node as usize];
            node = self.parent[node as usize];
        }
        (node, total)
    }

    /// Enforce k[b] - k[a] = rel by merging the two groups.
    fn union(&mut self, a: u32, b: u32, rel: i32) {
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

/// Whether row order closes a full 360° sweep (so the last radial is azimuthally
/// adjacent to the first). True for any normal NEXRAD PPI.
fn sweep_wraps(azimuths: &[f32]) -> bool {
    let rows = azimuths.len();
    if rows < 8 {
        return false;
    }
    let (Some(first), Some(last)) = (azimuths.first(), azimuths.last()) else {
        return false;
    };
    if !first.is_finite() || !last.is_finite() {
        return false;
    }
    let gap = (first - last)
        .rem_euclid(360.0)
        .min((last - first).rem_euclid(360.0));
    let typical = 360.0 / rows as f32;
    gap <= 3.0 * typical
}

/// Core region-based fold solver. Returns the integer Nyquist fold for every
/// gate (0 where unknown / no data). `nyq` must already be resolved (one
/// finite value per radial, or NaN when the whole sweep lacks one).
pub(crate) fn solve_folds(
    observed: &[f32],
    nyq: &[f32],
    rows: usize,
    gates: usize,
    azimuths: &[f32],
    reference: Option<&RangeBandReference>,
) -> Vec<i32> {
    let total = rows.saturating_mul(gates);
    let mut folds = vec![0i32; total];
    // Union-find nodes are indexed by `idx as u32`; bail (no unfolding) rather
    // than truncate if a grid were ever absurdly large. Real sweeps are ~1.3M
    // gates, far under u32::MAX.
    if total == 0 || observed.len() != total || total > u32::MAX as usize {
        return folds;
    }

    let same_region = |a: usize, b: usize, n: f32| -> bool {
        n.is_finite()
            && observed[a].is_finite()
            && observed[b].is_finite()
            && (observed[a] - observed[b]).abs() <= REGION_JOIN_FRAC * n
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
        return folds;
    }

    // ---- 2. accumulate inter-region fold votes over shared boundaries ----
    // key: (lo_region, hi_region) -> map fold -> count, where fold f means
    // k[hi] = k[lo] + f, f = round((v_lo - v_hi) / (2·Nyquist)).
    let mut edges: HashMap<(u32, u32), HashMap<i32, u32>> = HashMap::new();
    let mut vote = |ra: u32, va: f32, rb: u32, vb: f32, n: f32| {
        if ra == rb || !n.is_finite() {
            return;
        }
        let (lo, vlo, hi, vhi) = if ra < rb {
            (ra, va, rb, vb)
        } else {
            (rb, vb, ra, va)
        };
        let f = ((vlo - vhi) / (2.0 * n)).round() as i32;
        if f.abs() > 2 * REGION_MAX_FOLD {
            return;
        }
        *edges.entry((lo, hi)).or_default().entry(f).or_insert(0) += 1;
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
    let mut resolved: Vec<((u32, u32), i32, u32)> = edges
        .into_iter()
        .filter_map(|(key, votes)| {
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
            (support >= REGION_EDGE_MIN_SUPPORT).then_some((key, fold, total_support))
        })
        .collect();
    // Strongest boundary first; tie-break on the (unique) region pair so the
    // union order — and therefore the unfolded field — is reproducible.
    resolved.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

    let mut dsu = FoldUnionFind::new(region_count);
    for ((lo, hi), fold, _) in resolved {
        dsu.union(lo, hi, fold);
    }

    // ---- 4. anchor each connected group so its largest region is unfolded ----
    // For every group root, remember the offset of the biggest region in it.
    let mut anchor_offset: HashMap<u32, (u32, i32)> = HashMap::new();
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

    // ---- per-gate fold = (region offset) - (anchor offset of its group) ----
    let mut region_fold = vec![0i32; region_count];
    for rid in 0..region_count as u32 {
        let (root, off) = dsu.find(rid);
        let anchor = anchor_offset.get(&root).map(|(_, o)| *o).unwrap_or(0);
        region_fold[rid as usize] = (off - anchor).clamp(-REGION_MAX_FOLD, REGION_MAX_FOLD);
    }

    // ---- external-reference checks (e.g. the tilt cascade) ----
    // Boundary votes lock RELATIVE folds, but each connected group's absolute
    // branch — and any vote-graph misbranch — needs independent evidence.
    // A clean reference (the less aliased, higher-Nyquist tilt above, or model
    // winds) supplies it: choose each group's branch against the reference,
    // then re-test each region individually and override only when decisive.
    if let Some(reference) = reference {
        let mut row_trig = vec![(0.0f32, 0.0f32); rows];
        for row in 0..rows {
            let az = azimuths[row].to_radians();
            row_trig[row] = (az.sin(), az.cos());
        }
        // Group branch: cost per (root, g) for g ∈ −2..=+2.
        let mut group_cost: HashMap<u32, ([f64; 5], u64, u64)> = HashMap::new();
        for row in 0..rows {
            let n = nyq[row];
            if !n.is_finite() || n <= 0.0 {
                continue;
            }
            let (sin_az, cos_az) = row_trig[row];
            for gate in 0..gates {
                let idx = row * gates + gate;
                let rid = region_of[idx];
                if rid == u32::MAX {
                    continue;
                }
                let (root, off) = dsu.find(rid);
                let entry = group_cost.entry(root).or_insert(([0.0; 5], 0, 0));
                entry.2 += 1;
                let Some(predicted) = reference.eval_trig(sin_az, cos_az, gate) else {
                    continue;
                };
                entry.1 += 1;
                let v = observed[idx];
                for (slot, g) in (-2i32..=2).enumerate() {
                    let unfolded = v + (off + g) as f32 * 2.0 * n;
                    entry.0[slot] += (unfolded - predicted).abs() as f64;
                }
            }
        }
        for (root, (costs, covered, total_gates)) in &group_cost {
            if *total_gates == 0 || (*covered as f64) < 0.5 * *total_gates as f64 {
                continue;
            }
            let (best_slot, _) = costs
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(b.1))
                .expect("five branches");
            let branch = best_slot as i32 - 2;
            for rid in 0..region_count as u32 {
                let (r, off) = dsu.find(rid);
                if r == *root {
                    region_fold[rid as usize] =
                        (off + branch).clamp(-REGION_MAX_FOLD, REGION_MAX_FOLD);
                }
            }
        }
        // Per-region override: repairs vote-graph misbranches that survive
        // group selection (a subgraph can be internally consistent yet wrong).
        let mut cost = vec![[0.0f64; 3]; region_count];
        let mut covered = vec![0u32; region_count];
        for row in 0..rows {
            let n = nyq[row];
            if !n.is_finite() || n <= 0.0 {
                continue;
            }
            let (sin_az, cos_az) = row_trig[row];
            for gate in 0..gates {
                let idx = row * gates + gate;
                let rid = region_of[idx];
                if rid == u32::MAX {
                    continue;
                }
                let Some(predicted) = reference.eval_trig(sin_az, cos_az, gate) else {
                    continue;
                };
                let v = observed[idx];
                let fold = region_fold[rid as usize];
                covered[rid as usize] += 1;
                for (slot, dg) in (-1i32..=1).enumerate() {
                    let unfolded = v + (fold + dg) as f32 * 2.0 * n;
                    cost[rid as usize][slot] += (unfolded - predicted).abs() as f64;
                }
            }
        }
        for rid in 0..region_count {
            if (covered[rid] as f64) < 0.6 * region_size[rid] as f64 {
                continue;
            }
            let current = cost[rid][1];
            let (best_slot, best_cost) = cost[rid]
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(b.1))
                .expect("three slots");
            if best_slot != 1 && *best_cost < 0.6 * current {
                let dg = best_slot as i32 - 1;
                region_fold[rid] = (region_fold[rid] + dg).clamp(-REGION_MAX_FOLD, REGION_MAX_FOLD);
            }
        }
    }

    for idx in 0..total {
        let rid = region_of[idx];
        if rid != u32::MAX {
            folds[idx] = region_fold[rid as usize];
        }
    }
    folds
}
