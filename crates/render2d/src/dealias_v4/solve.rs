//! Discrete label solver for the volume branch energy (spec §5.3).
//!
//! Graph size on real volumes is tiny (tens to a few hundred nodes, 5 labels
//! each), so the solver is boring, deterministic, and exact where cheap:
//!
//! 1. **Spanning-forest dynamic program** — maximum spanning forest over
//!    pairwise-evidence strength (Kruskal; edges pre-sorted by
//!    (strength desc, a, b) for determinism), then an exact tree DP over the
//!    5 labels: O(nodes · 5²).
//! 2. **ICM refinement** over the full graph including non-forest edges
//!    (Besag 1986, *J. Roy. Stat. Soc. B* 48, 259–302): sweep nodes in fixed
//!    ascending order re-optimizing each label given its neighbors, repeat to
//!    a fixed point (max [`MAX_ICM_SWEEPS`]).  Deterministic; monotonically
//!    non-increasing energy.
//! 3. **Exhaustive enumeration** for connected components of ≤
//!    [`ENUMERATE_MAX_NODES`] nodes (5⁸ = 390,625 assignments of table
//!    lookups).  If enumeration beats DP+ICM the exact answer wins and the
//!    disagreement is counted in diagnostics — a nonzero count means the
//!    heuristic missed; the battery watches it.
//!
//! Per-gate MRF graph cuts (Boykov, Veksler & Zabih 2001, *IEEE TPAMI* 23,
//! 1222–1239) and external ILP solvers were REJECTED (spec §4b): heavy
//! dependencies, unnecessary at super-region granularity, and determinism
//! risk.
//!
//! Ties prefer the smaller |k| (then negative k) via [`SLOT_PREFERENCE`], so
//! a node with no evidence at all keeps its v1 branch.

use super::graph::{EvidenceTables, PairwiseEdge};

/// Label slots 0..5 map to k = slot − 2.  Argmin scans visit slots in this
/// order and replace only on strictly smaller cost, so exact ties resolve to
/// the smallest |k|.
pub(crate) const SLOT_PREFERENCE: [usize; 5] = [2, 1, 3, 0, 4];
const ENUMERATE_MAX_NODES: usize = 8;
const MAX_ICM_SWEEPS: usize = 10;

pub(crate) struct SolveOutcome {
    /// Chosen branch shift k per node.
    pub(crate) labels: Vec<i32>,
    /// Per-node local decision margin (second-best minus best local cost with
    /// all neighbors fixed), in weighted m/s.  ≥ 0 at any local optimum.
    pub(crate) margins: Vec<f64>,
    /// Nodes whose component had no absolute evidence and was re-anchored.
    /// Their single-node margins are FALSELY large (the degeneracy is a
    /// joint direction), so their confidence is capped at interior-only —
    /// this is what keeps a cold-start solve from poisoning the next
    /// volume's temporal prior with high-confidence guesses (F6).
    pub(crate) renormalized: Vec<bool>,
    pub(crate) components: usize,
    pub(crate) enumerated_components: usize,
    pub(crate) enumeration_beat_heuristic: usize,
    /// Final total energy (unaries + pairwise), for diagnostics.
    pub(crate) energy: f64,
}

pub(crate) fn solve_labels(tables: &EvidenceTables) -> SolveOutcome {
    let node_count = tables.unary.len();
    let mut labels_slot = vec![2usize; node_count]; // start at k = 0

    // Adjacency: edge indices per node.
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    for (edge_index, edge) in tables.edges.iter().enumerate() {
        adjacency[edge.a].push(edge_index);
        adjacency[edge.b].push(edge_index);
    }

    // Connected components over evidence edges, in ascending node order.
    let mut component_of = vec![usize::MAX; node_count];
    let mut components: Vec<Vec<usize>> = Vec::new();
    for start in 0..node_count {
        if component_of[start] != usize::MAX {
            continue;
        }
        let component_index = components.len();
        let mut stack = vec![start];
        let mut members = Vec::new();
        component_of[start] = component_index;
        while let Some(node) = stack.pop() {
            members.push(node);
            for &edge_index in &adjacency[node] {
                let edge = &tables.edges[edge_index];
                let other = if edge.a == node { edge.b } else { edge.a };
                if component_of[other] == usize::MAX {
                    component_of[other] = component_index;
                    stack.push(other);
                }
            }
        }
        members.sort_unstable();
        components.push(members);
    }

    let mut enumerated_components = 0usize;
    let mut enumeration_beat_heuristic = 0usize;

    for members in &components {
        if members.len() == 1 {
            let node = members[0];
            labels_slot[node] = argmin_slot(&tables.unary[node]);
            continue;
        }

        // Heuristic: forest DP + ICM.
        let mut slots = forest_dp(tables, members, &adjacency);
        icm(tables, members, &adjacency, &mut slots);
        let heuristic_energy = component_energy(tables, members, &slots);

        // Exact check for small components.
        if members.len() <= ENUMERATE_MAX_NODES {
            enumerated_components += 1;
            let (exact_slots, exact_energy) = enumerate_component(tables, members);
            if exact_energy + 1e-9 < heuristic_energy {
                enumeration_beat_heuristic += 1;
                for (member, slot) in members.iter().zip(exact_slots) {
                    labels_slot[*member] = slot;
                }
                continue;
            }
        }
        for member in members {
            labels_slot[*member] = slots[member];
        }
    }

    // Graceful-degradation renormalization: a component with NO absolutely
    // anchored node (no env/temporal unary coverage anywhere) has only
    // relative evidence, and its joint-shift direction is degenerate —
    // worse, across tilts with different Nyquist a joint shift changes
    // inter-tilt alignment by 2·ΔN and can look like an alignment gain
    // (measured: KEAX without a profile drifted seven ~50k-gate nodes to
    // k = −1).  Re-anchor such components so their LEAST-ALIASED substantial
    // node keeps its v1 branch — UNRAVEL initializes from the least-aliased
    // references for the same reason (Louf et al. 2020), and it is the
    // tilt-cascade's "cleaner tilt anchors" assumption at node granularity.
    let mut renormalized = vec![false; node_count];
    for members in &components {
        if members.iter().any(|&node| tables.has_absolute[node]) {
            continue;
        }
        for &member in members {
            renormalized[member] = true;
        }
        let max_gates = members
            .iter()
            .map(|&node| tables.node_gates[node])
            .max()
            .unwrap_or(0);
        let anchor = members
            .iter()
            .copied()
            // Substantial nodes only: a 70-gate sliver with 0% near-Nyquist
            // must not out-anchor a 50k-gate field at 8%.
            .filter(|&node| tables.node_gates[node] * 8 >= max_gates)
            .min_by(|&left, &right| {
                // Highest Nyquist first (wraps least), then the least
                // near-Nyquist boundary structure, then size, then id.
                tables.node_nyquist[right]
                    .total_cmp(&tables.node_nyquist[left])
                    .then_with(|| {
                        tables.node_near_nyquist[left].total_cmp(&tables.node_near_nyquist[right])
                    })
                    .then_with(|| tables.node_gates[right].cmp(&tables.node_gates[left]))
                    .then_with(|| left.cmp(&right))
            })
            .unwrap_or(members[0]);
        let delta = labels_slot[anchor] as i32 - 2;
        if delta != 0 {
            for &member in members {
                let shifted = (labels_slot[member] as i32 - 2 - delta).clamp(-2, 2);
                labels_slot[member] = (shifted + 2) as usize;
            }
        }
    }

    // Margins at the final assignment (local, neighbors fixed).
    let mut margins = vec![0.0f64; node_count];
    for (node, margin) in margins.iter_mut().enumerate() {
        let costs = local_costs(tables, node, &adjacency, &labels_slot);
        let chosen = costs[labels_slot[node]];
        let mut second = f64::INFINITY;
        for (slot, cost) in costs.iter().enumerate() {
            if slot != labels_slot[node] && *cost < second {
                second = *cost;
            }
        }
        *margin = if second.is_finite() {
            (second - chosen).max(0.0)
        } else {
            0.0
        };
    }

    let mut energy = 0.0;
    for (node_unary, &slot) in tables.unary.iter().zip(&labels_slot) {
        energy += node_unary[slot];
    }
    for edge in &tables.edges {
        energy += edge.table[labels_slot[edge.a] * 5 + labels_slot[edge.b]];
    }

    SolveOutcome {
        labels: labels_slot.iter().map(|slot| *slot as i32 - 2).collect(),
        margins,
        renormalized,
        components: components.len(),
        enumerated_components,
        enumeration_beat_heuristic,
        energy,
    }
}

fn argmin_slot(costs: &[f64; 5]) -> usize {
    let mut best = SLOT_PREFERENCE[0];
    for &slot in &SLOT_PREFERENCE[1..] {
        if costs[slot] < costs[best] {
            best = slot;
        }
    }
    best
}

/// Local cost of each label for `node` with every neighbor fixed.
fn local_costs(
    tables: &EvidenceTables,
    node: usize,
    adjacency: &[Vec<usize>],
    slots: &[usize],
) -> [f64; 5] {
    let mut costs = tables.unary[node];
    for &edge_index in &adjacency[node] {
        let edge = &tables.edges[edge_index];
        if edge.a == node {
            let other_slot = slots[edge.b];
            for (slot, cost) in costs.iter_mut().enumerate() {
                *cost += edge.table[slot * 5 + other_slot];
            }
        } else {
            let other_slot = slots[edge.a];
            for (slot, cost) in costs.iter_mut().enumerate() {
                *cost += edge.table[other_slot * 5 + slot];
            }
        }
    }
    costs
}

/// Maximum spanning forest (by evidence strength) + exact tree DP.
fn forest_dp(
    tables: &EvidenceTables,
    members: &[usize],
    adjacency: &[Vec<usize>],
) -> std::collections::HashMap<usize, usize> {
    // Collect this component's edges once, strongest first.
    let mut edge_indices: Vec<usize> = Vec::new();
    for &node in members {
        for &edge_index in &adjacency[node] {
            if tables.edges[edge_index].a == node {
                edge_indices.push(edge_index);
            }
        }
    }
    edge_indices.sort_by(|left, right| {
        let (le, re) = (&tables.edges[*left], &tables.edges[*right]);
        re.strength
            .total_cmp(&le.strength)
            .then_with(|| (le.a, le.b).cmp(&(re.a, re.b)))
    });

    // Kruskal on node ids (union by min id keeps determinism trivial).
    let mut forest_parent: std::collections::HashMap<usize, usize> =
        members.iter().map(|&node| (node, node)).collect();
    fn find(parent: &mut std::collections::HashMap<usize, usize>, mut x: usize) -> usize {
        while parent[&x] != x {
            let up = parent[&x];
            let up2 = parent[&up];
            parent.insert(x, up2);
            x = up2;
        }
        x
    }
    let mut tree_adjacency: std::collections::HashMap<usize, Vec<usize>> =
        members.iter().map(|&node| (node, Vec::new())).collect();
    for &edge_index in &edge_indices {
        let edge = &tables.edges[edge_index];
        let (ra, rb) = (
            find(&mut forest_parent, edge.a),
            find(&mut forest_parent, edge.b),
        );
        if ra != rb {
            forest_parent.insert(ra.max(rb), ra.min(rb));
            tree_adjacency
                .get_mut(&edge.a)
                .expect("member")
                .push(edge_index);
            tree_adjacency
                .get_mut(&edge.b)
                .expect("member")
                .push(edge_index);
        }
    }

    // Iterative rooted traversal per tree in the forest; roots in ascending
    // node order.
    let mut slots: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for &root in members {
        if visited.contains(&root) {
            continue;
        }
        // Build parent pointers + post-order.
        let mut order = Vec::new();
        let mut parent_edge: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        let mut stack = vec![root];
        visited.insert(root);
        while let Some(node) = stack.pop() {
            order.push(node);
            for &edge_index in &tree_adjacency[&node] {
                let edge = &tables.edges[edge_index];
                let child = if edge.a == node { edge.b } else { edge.a };
                if visited.insert(child) {
                    parent_edge.insert(child, edge_index);
                    stack.push(child);
                }
            }
        }
        // dp[node][slot] = unary + Σ_children min over child slot of
        // (dp[child] + edge cost); children processed before parents by
        // walking `order` in reverse.
        let mut dp: std::collections::HashMap<usize, [f64; 5]> = order
            .iter()
            .map(|&node| (node, tables.unary[node]))
            .collect();
        let mut best_child_slot: std::collections::HashMap<(usize, usize), usize> =
            std::collections::HashMap::new();
        for &node in order.iter().rev() {
            if node == root {
                continue;
            }
            let edge_index = parent_edge[&node];
            let edge = &tables.edges[edge_index];
            let parent = if edge.a == node { edge.b } else { edge.a };
            let child_dp = dp[&node];
            let mut addition = [0.0f64; 5];
            for (parent_slot, slot_cost) in addition.iter_mut().enumerate() {
                let mut best_slot = SLOT_PREFERENCE[0];
                let mut best_cost =
                    child_dp[best_slot] + edge_cost(edge, node, best_slot, parent_slot);
                for &slot in &SLOT_PREFERENCE[1..] {
                    let cost = child_dp[slot] + edge_cost(edge, node, slot, parent_slot);
                    if cost < best_cost {
                        best_cost = cost;
                        best_slot = slot;
                    }
                }
                *slot_cost = best_cost;
                best_child_slot.insert((node, parent_slot), best_slot);
            }
            let parent_dp = dp.get_mut(&parent).expect("parent in dp");
            for parent_slot in 0..5 {
                parent_dp[parent_slot] += addition[parent_slot];
            }
        }
        // Backtrack from the root down `order`.
        slots.insert(root, argmin_slot(&dp[&root]));
        for &node in &order {
            if node == root {
                continue;
            }
            let edge_index = parent_edge[&node];
            let edge = &tables.edges[edge_index];
            let parent = if edge.a == node { edge.b } else { edge.a };
            let parent_slot = slots[&parent];
            slots.insert(node, best_child_slot[&(node, parent_slot)]);
        }
    }
    slots
}

/// Pairwise cost for `edge` with `node` at `node_slot` and the OTHER endpoint
/// at `other_slot`.
#[inline]
fn edge_cost(edge: &PairwiseEdge, node: usize, node_slot: usize, other_slot: usize) -> f64 {
    if edge.a == node {
        edge.table[node_slot * 5 + other_slot]
    } else {
        edge.table[other_slot * 5 + node_slot]
    }
}

/// ICM sweeps to a fixed point (Besag 1986).  Every accepted move strictly
/// lowers the energy, so termination is guaranteed even without the sweep
/// cap.
fn icm(
    tables: &EvidenceTables,
    members: &[usize],
    adjacency: &[Vec<usize>],
    slots: &mut std::collections::HashMap<usize, usize>,
) {
    // Work on the full-size slot table for O(1) neighbor lookup.
    let mut full = vec![2usize; tables.unary.len()];
    for (&node, &slot) in slots.iter() {
        full[node] = slot;
    }
    for _ in 0..MAX_ICM_SWEEPS {
        let mut changed = false;
        for &node in members {
            let costs = local_costs(tables, node, adjacency, &full);
            let current = full[node];
            let best = argmin_slot(&costs);
            if best != current && costs[best] < costs[current] {
                full[node] = best;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for &node in members {
        slots.insert(node, full[node]);
    }
}

fn component_energy(
    tables: &EvidenceTables,
    members: &[usize],
    slots: &std::collections::HashMap<usize, usize>,
) -> f64 {
    let mut energy = 0.0;
    for &node in members {
        energy += tables.unary[node][slots[&node]];
    }
    for &node in members {
        // Count each edge once from its `a` endpoint.
        for edge in tables.edges.iter().filter(|edge| edge.a == node) {
            if let (Some(&slot_a), Some(&slot_b)) = (slots.get(&edge.a), slots.get(&edge.b)) {
                energy += edge.table[slot_a * 5 + slot_b];
            }
        }
    }
    energy
}

/// Exhaustive enumeration over ≤ 5⁸ assignments.  Slots iterate in
/// preference order so the FIRST minimal assignment found is also the
/// tie-preferred one.
fn enumerate_component(tables: &EvidenceTables, members: &[usize]) -> (Vec<usize>, f64) {
    // This component's edges with member-local indices.
    let index_of: std::collections::HashMap<usize, usize> = members
        .iter()
        .enumerate()
        .map(|(local, &node)| (node, local))
        .collect();
    let mut local_edges: Vec<(usize, usize, &[f64; 25])> = Vec::new();
    for edge in &tables.edges {
        if let (Some(&la), Some(&lb)) = (index_of.get(&edge.a), index_of.get(&edge.b)) {
            local_edges.push((la, lb, &edge.table));
        }
    }

    let n = members.len();
    let mut counters = vec![0usize; n]; // indices into SLOT_PREFERENCE
    let mut best_energy = f64::INFINITY;
    let mut best = vec![2usize; n];
    loop {
        let slots: Vec<usize> = counters
            .iter()
            .map(|&counter| SLOT_PREFERENCE[counter])
            .collect();
        let mut energy = 0.0;
        for (local, &node) in members.iter().enumerate() {
            energy += tables.unary[node][slots[local]];
        }
        for &(la, lb, table) in &local_edges {
            energy += table[slots[la] * 5 + slots[lb]];
        }
        if energy < best_energy {
            best_energy = energy;
            best.copy_from_slice(&slots);
        }
        // Odometer increment.
        let mut position = 0;
        loop {
            if position == n {
                return (best, best_energy);
            }
            counters[position] += 1;
            if counters[position] < 5 {
                break;
            }
            counters[position] = 0;
            position += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dealias_v4::graph::PairwiseEdge;

    fn tables(unary: Vec<[f64; 5]>, edges: Vec<PairwiseEdge>) -> EvidenceTables {
        let nodes = unary.len();
        EvidenceTables {
            unary,
            has_external: vec![true; nodes],
            has_absolute: vec![true; nodes],
            node_gates: vec![1000; nodes],
            node_nyquist: vec![25.0; nodes],
            node_near_nyquist: vec![0.0; nodes],
            edges,
        }
    }

    /// A chain where the pairwise terms want equal labels and one strong
    /// unary anchors the branch: the whole chain must follow the anchor.
    #[test]
    fn chain_follows_the_anchored_branch() {
        let mut anchored = [10.0f64; 5];
        anchored[3] = 0.0; // wants k = +1
        let neutral = [0.1f64; 5];
        // Pairwise: cost 0 when labels equal, 5 per unit difference.
        let mut equal_table = [0.0f64; 25];
        for a in 0..5i32 {
            for b in 0..5i32 {
                equal_table[(a * 5 + b) as usize] = 5.0 * f64::from((a - b).abs());
            }
        }
        let t = tables(
            vec![anchored, neutral, neutral],
            vec![
                PairwiseEdge {
                    a: 0,
                    b: 1,
                    table: equal_table,
                    strength: 1.0,
                },
                PairwiseEdge {
                    a: 1,
                    b: 2,
                    table: equal_table,
                    strength: 0.9,
                },
            ],
        );
        let outcome = solve_labels(&t);
        assert_eq!(outcome.labels, vec![1, 1, 1]);
        assert_eq!(outcome.components, 1);
        assert_eq!(outcome.enumerated_components, 1);
        assert_eq!(outcome.enumeration_beat_heuristic, 0);
        assert!(outcome.margins.iter().all(|margin| *margin > 0.0));
    }

    /// No evidence at all: every node keeps k = 0 (v1 behavior; the tie
    /// preference guarantees it even with all-zero costs).
    #[test]
    fn evidence_free_nodes_keep_the_v1_branch() {
        let t = tables(vec![[0.0; 5], [0.0; 5]], Vec::new());
        let outcome = solve_labels(&t);
        assert_eq!(outcome.labels, vec![0, 0]);
    }

    /// A frustrated cycle (triangle) where ICM from a bad start could stick:
    /// enumeration must still deliver the global optimum for ≤ 8 nodes.
    #[test]
    fn small_components_are_solved_exactly() {
        // Unaries pull node 0 to −1 and node 2 to +1; edges strongly want
        // 0==1 and 1==2, weakly want 0==2. Global optimum picks one side.
        let mut pull_minus = [4.0f64; 5];
        pull_minus[1] = 0.0;
        let mut pull_plus = [4.0f64; 5];
        pull_plus[3] = 0.0;
        let mut equal_strong = [0.0f64; 25];
        let mut equal_weak = [0.0f64; 25];
        for a in 0..5i32 {
            for b in 0..5i32 {
                equal_strong[(a * 5 + b) as usize] = 6.0 * f64::from((a - b).abs().min(2));
                equal_weak[(a * 5 + b) as usize] = 0.5 * f64::from((a - b).abs().min(2));
            }
        }
        let t = tables(
            vec![pull_minus, [0.0; 5], pull_plus],
            vec![
                PairwiseEdge {
                    a: 0,
                    b: 1,
                    table: equal_strong,
                    strength: 1.0,
                },
                PairwiseEdge {
                    a: 1,
                    b: 2,
                    table: equal_strong,
                    strength: 1.0,
                },
                PairwiseEdge {
                    a: 0,
                    b: 2,
                    table: equal_weak,
                    strength: 0.1,
                },
            ],
        );
        let outcome = solve_labels(&t);
        // Exhaustive reference over all 125 assignments.
        let mut best = (f64::INFINITY, vec![0usize; 3]);
        for a in 0..5 {
            for b in 0..5 {
                for c in 0..5 {
                    let energy = t.unary[0][a]
                        + t.unary[1][b]
                        + t.unary[2][c]
                        + t.edges[0].table[a * 5 + b]
                        + t.edges[1].table[b * 5 + c]
                        + t.edges[2].table[a * 5 + c];
                    if energy < best.0 {
                        best = (energy, vec![a, b, c]);
                    }
                }
            }
        }
        assert!(
            (outcome.energy - best.0).abs() < 1e-9,
            "must match exact optimum"
        );
    }
}
