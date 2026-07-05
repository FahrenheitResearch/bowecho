//! Literal Rust port of Py-ART's `dealias_region_based` per-sweep core.
//!
//! This module is intentionally separate from v4.  It mirrors the Py-ART
//! region algorithm defaults for one sweep: 3 interval splits, invalid-gate
//! filter only, skip gaps of 100 rays/gates, dynamic network reduction, and
//! centered sweep offset.  It does not apply v4 volume evidence, environmental
//! winds, temporal priors, couplet freeze, or repair passes.

use std::collections::BTreeMap;

use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType};

use crate::{
    DEALIASED_VELOCITY_NODATA, DEALIASED_VELOCITY_OFFSET, DEALIASED_VELOCITY_SCALE,
    copy_scaled_velocity_row, encode_dealiased_velocity, median_nyquist_mps, radial_azimuths,
    row_nyquist_mps, sweep_wraps,
};

const INTERVAL_SPLITS: usize = 3;
const SKIP_BETWEEN_RAYS: usize = 100;
const SKIP_ALONG_RAY: usize = 100;

pub fn dealias_velocity_grid_pyart_region(cut: &ElevationCut, source: &MomentGrid) -> MomentGrid {
    let rows = source.radial_count();
    let gates = source.gate_range.gate_count;
    let total = rows.saturating_mul(gates);
    let fallback_nyquist = median_nyquist_mps(cut, source);

    let mut nyq = vec![f32::NAN; rows.max(1)];
    for (row, slot) in nyq.iter_mut().enumerate().take(rows) {
        *slot = row_nyquist_mps(cut, source, row)
            .or(fallback_nyquist)
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(f32::NAN);
    }

    let mut observed = vec![f32::NAN; total];
    if total > 0 {
        let mut row_buf = vec![f32::NAN; gates];
        for row in 0..rows {
            copy_scaled_velocity_row(source, row, &mut row_buf);
            observed[row * gates..(row + 1) * gates].copy_from_slice(&row_buf);
        }
    }

    let azimuths = radial_azimuths(cut, source);
    let folds = pyart_region_folds(&observed, &nyq, rows, gates, sweep_wraps(&azimuths));

    let mut corrected = vec![DEALIASED_VELOCITY_NODATA; total];
    for (row, &n) in nyq.iter().enumerate().take(rows) {
        for gate in 0..gates {
            let idx = row * gates + gate;
            let value = observed[idx];
            if !value.is_finite() {
                continue;
            }
            let unfolded = if n.is_finite() && n > 0.0 {
                value + 2.0 * n * folds[idx] as f32
            } else {
                value
            };
            corrected[idx] = encode_dealiased_velocity(unfolded);
        }
    }

    MomentGrid {
        moment: MomentType::Velocity,
        gate_range: source.gate_range.clone(),
        scale: DEALIASED_VELOCITY_SCALE,
        offset: DEALIASED_VELOCITY_OFFSET,
        nodata: Some(DEALIASED_VELOCITY_NODATA),
        range_folded: None,
        radial_indices: source.radial_indices.clone(),
        storage: MomentStorage::U16(corrected),
    }
}

fn pyart_region_folds(
    observed: &[f32],
    nyq: &[f32],
    rows: usize,
    gates: usize,
    wraps: bool,
) -> Vec<i32> {
    let total = rows.saturating_mul(gates);
    let mut folds = vec![0i32; total];
    if rows == 0 || gates == 0 || observed.len() != total {
        return folds;
    }

    let nvel = sweep_nyquist(nyq).unwrap_or(f32::NAN);
    if !nvel.is_finite() || nvel <= 0.0 {
        return folds;
    }
    let nyquist_interval = 2.0 * nvel;
    let limits = interval_limits(nvel, observed);
    let (labels, region_sizes) = find_regions(observed, rows, gates, &limits);
    let nfeatures = region_sizes.len().saturating_sub(1);
    if nfeatures < 2 {
        return folds;
    }

    let edges = edge_sum_and_count(&labels, observed, rows, gates, wraps, nyquist_interval);
    if edges.is_empty() {
        return folds;
    }

    let mut regions = RegionTracker::new(region_sizes);
    let mut edge_tracker = EdgeTracker::new(edges, nfeatures + 1);
    while let Some((node1, node2, diff, edge_number)) = edge_tracker.pop_edge() {
        let mut rdiff = round_ties_even_to_i32(diff);
        let node1_size = regions.get_node_size(node1);
        let node2_size = regions.get_node_size(node2);
        let (base_node, merge_node) = if node1_size > node2_size {
            (node1, node2)
        } else {
            rdiff = -rdiff;
            (node2, node1)
        };
        if rdiff != 0 {
            regions.unwrap_node(merge_node, rdiff);
            edge_tracker.unwrap_node(merge_node, rdiff);
        }
        regions.merge_nodes(base_node, merge_node);
        edge_tracker.merge_nodes(base_node, merge_node, edge_number);
    }

    let gates_dealiased: u64 = regions
        .node_size
        .iter()
        .skip(1)
        .map(|&value| value as u64)
        .sum();
    if gates_dealiased > 0 {
        let total_folds: i64 = regions
            .original_region_sizes
            .iter()
            .enumerate()
            .skip(1)
            .map(|(region, &size)| size as i64 * regions.unwrap_number[region] as i64)
            .sum();
        let sweep_offset = round_ties_even_to_i32(total_folds as f64 / gates_dealiased as f64);
        if sweep_offset != 0 {
            for unwrap in &mut regions.unwrap_number {
                *unwrap -= sweep_offset;
            }
        }
    }

    for idx in 0..total {
        let label = labels[idx] as usize;
        if label != 0 {
            folds[idx] = regions.unwrap_number[label];
        }
    }
    folds
}

fn sweep_nyquist(nyq: &[f32]) -> Option<f32> {
    nyq.iter()
        .copied()
        .find(|value| value.is_finite() && *value > 0.0)
}

fn interval_limits(nyquist: f32, observed: &[f32]) -> Vec<f32> {
    let interval = (2.0 * nyquist) / INTERVAL_SPLITS as f32;
    let mut add_start = 0i32;
    let mut add_end = 0i32;
    let mut min_value = f32::INFINITY;
    let mut max_value = f32::NEG_INFINITY;
    let mut any = false;
    for value in observed.iter().copied().filter(|value| value.is_finite()) {
        any = true;
        min_value = min_value.min(value);
        max_value = max_value.max(value);
    }
    if any && (max_value > nyquist || min_value < -nyquist) {
        add_start = ((max_value - nyquist) / interval).ceil() as i32;
        add_end = (-(min_value + nyquist) / interval).ceil() as i32;
    }
    let start = -nyquist - add_start as f32 * interval;
    let end = nyquist + add_end as f32 * interval;
    let count = INTERVAL_SPLITS as i32 + 1 + add_start + add_end;
    if count <= 1 {
        return vec![start, end];
    }
    (0..count)
        .map(|i| start + (end - start) * i as f32 / (count - 1) as f32)
        .collect()
}

fn find_regions(
    observed: &[f32],
    rows: usize,
    gates: usize,
    limits: &[f32],
) -> (Vec<i32>, Vec<u32>) {
    let mut labels = vec![0i32; rows * gates];
    let mut region_sizes = vec![0u32];
    let mut next_label = 1i32;
    let mut stack = Vec::new();

    for pair in limits.windows(2) {
        let lmin = pair[0];
        let lmax = pair[1];
        for row in 0..rows {
            for gate in 0..gates {
                let idx = row * gates + gate;
                if labels[idx] != 0 || !in_interval(observed[idx], lmin, lmax) {
                    continue;
                }
                let label = next_label;
                next_label += 1;
                region_sizes.push(0);
                labels[idx] = label;
                stack.push((row, gate));
                while let Some((r, g)) = stack.pop() {
                    region_sizes[label as usize] += 1;
                    if r > 0 {
                        try_label_neighbor(
                            observed,
                            &mut labels,
                            rows,
                            gates,
                            r - 1,
                            g,
                            lmin,
                            lmax,
                            label,
                            &mut stack,
                        );
                    }
                    if r + 1 < rows {
                        try_label_neighbor(
                            observed,
                            &mut labels,
                            rows,
                            gates,
                            r + 1,
                            g,
                            lmin,
                            lmax,
                            label,
                            &mut stack,
                        );
                    }
                    if g > 0 {
                        try_label_neighbor(
                            observed,
                            &mut labels,
                            rows,
                            gates,
                            r,
                            g - 1,
                            lmin,
                            lmax,
                            label,
                            &mut stack,
                        );
                    }
                    if g + 1 < gates {
                        try_label_neighbor(
                            observed,
                            &mut labels,
                            rows,
                            gates,
                            r,
                            g + 1,
                            lmin,
                            lmax,
                            label,
                            &mut stack,
                        );
                    }
                }
            }
        }
    }
    (labels, region_sizes)
}

fn in_interval(value: f32, lmin: f32, lmax: f32) -> bool {
    value.is_finite() && lmin <= value && value < lmax
}

#[allow(clippy::too_many_arguments)]
fn try_label_neighbor(
    observed: &[f32],
    labels: &mut [i32],
    rows: usize,
    gates: usize,
    row: usize,
    gate: usize,
    lmin: f32,
    lmax: f32,
    label: i32,
    stack: &mut Vec<(usize, usize)>,
) {
    let idx = row * gates + gate;
    if labels[idx] == 0 && in_interval(observed[idx], lmin, lmax) {
        debug_assert!(row < rows && gate < gates);
        labels[idx] = label;
        stack.push((row, gate));
    }
}

#[derive(Clone, Copy, Default)]
struct EdgeAccum {
    count: u32,
    vel_sum: f64,
    nvel_sum: f64,
}

#[derive(Clone)]
struct EdgeState {
    alpha: usize,
    beta: usize,
    sum_diff: f64,
    weight: i32,
}

fn edge_sum_and_count(
    labels: &[i32],
    observed: &[f32],
    rows: usize,
    gates: usize,
    wraps: bool,
    nyquist_interval: f32,
) -> Vec<EdgeState> {
    let mut acc: BTreeMap<(i32, i32), EdgeAccum> = BTreeMap::new();
    for row in 0..rows {
        for gate in 0..gates {
            let idx = row * gates + gate;
            let label = labels[idx];
            if label == 0 {
                continue;
            }
            let vel = observed[idx];

            if let Some(nrow) = scan_row_gap(labels, rows, gates, row, gate, -1, wraps) {
                add_directed_edge(
                    &mut acc,
                    label,
                    labels[nrow * gates + gate],
                    vel,
                    observed[nrow * gates + gate],
                );
            }
            if let Some(nrow) = scan_row_gap(labels, rows, gates, row, gate, 1, wraps) {
                add_directed_edge(
                    &mut acc,
                    label,
                    labels[nrow * gates + gate],
                    vel,
                    observed[nrow * gates + gate],
                );
            }
            if let Some(ngate) = scan_gate_gap(labels, rows, gates, row, gate, -1) {
                add_directed_edge(
                    &mut acc,
                    label,
                    labels[row * gates + ngate],
                    vel,
                    observed[row * gates + ngate],
                );
            }
            if let Some(ngate) = scan_gate_gap(labels, rows, gates, row, gate, 1) {
                add_directed_edge(
                    &mut acc,
                    label,
                    labels[row * gates + ngate],
                    vel,
                    observed[row * gates + ngate],
                );
            }
        }
    }

    let mut entries: Vec<_> = acc.into_iter().collect();
    entries.sort_by_key(|((label, neighbor), _)| (*neighbor, *label));

    let mut edges = Vec::new();
    for ((label, neighbor), edge) in entries {
        if label < neighbor || edge.count == 0 {
            continue;
        }
        edges.push(EdgeState {
            alpha: label as usize,
            beta: neighbor as usize,
            sum_diff: (edge.vel_sum - edge.nvel_sum) / f64::from(nyquist_interval),
            weight: edge.count as i32,
        });
    }
    edges
}

fn add_directed_edge(
    acc: &mut BTreeMap<(i32, i32), EdgeAccum>,
    label: i32,
    neighbor: i32,
    vel: f32,
    nvel: f32,
) {
    if neighbor == label || neighbor == 0 {
        return;
    }
    let entry = acc.entry((label, neighbor)).or_default();
    entry.count += 1;
    entry.vel_sum += f64::from(vel);
    entry.nvel_sum += f64::from(nvel);
}

fn scan_row_gap(
    labels: &[i32],
    rows: usize,
    gates: usize,
    row: usize,
    gate: usize,
    step: isize,
    wraps: bool,
) -> Option<usize> {
    let mut check = row as isize + step;
    if check < 0 {
        if wraps {
            check = rows as isize - 1;
        } else {
            return None;
        }
    } else if check == rows as isize {
        if wraps {
            check = 0;
        } else {
            return None;
        }
    }
    if labels[check as usize * gates + gate] != 0 {
        return Some(check as usize);
    }
    for _ in 0..SKIP_BETWEEN_RAYS {
        check += step;
        if check < 0 {
            if wraps {
                check = rows as isize - 1;
            } else {
                break;
            }
        } else if check == rows as isize {
            if wraps {
                check = 0;
            } else {
                break;
            }
        }
        if labels[check as usize * gates + gate] != 0 {
            return Some(check as usize);
        }
    }
    None
}

fn scan_gate_gap(
    labels: &[i32],
    _rows: usize,
    gates: usize,
    row: usize,
    gate: usize,
    step: isize,
) -> Option<usize> {
    let mut check = gate as isize + step;
    if check < 0 || check == gates as isize {
        return None;
    }
    if labels[row * gates + check as usize] != 0 {
        return Some(check as usize);
    }
    for _ in 0..SKIP_ALONG_RAY {
        check += step;
        if check < 0 || check == gates as isize {
            break;
        }
        if labels[row * gates + check as usize] != 0 {
            return Some(check as usize);
        }
    }
    None
}

struct RegionTracker {
    node_size: Vec<u32>,
    original_region_sizes: Vec<u32>,
    regions_in_node: Vec<Vec<usize>>,
    unwrap_number: Vec<i32>,
}

impl RegionTracker {
    fn new(region_sizes: Vec<u32>) -> Self {
        let nregions = region_sizes.len();
        Self {
            node_size: region_sizes.clone(),
            original_region_sizes: region_sizes,
            regions_in_node: (0..nregions).map(|region| vec![region]).collect(),
            unwrap_number: vec![0; nregions],
        }
    }

    fn merge_nodes(&mut self, node_a: usize, node_b: usize) {
        let regions_to_merge = std::mem::take(&mut self.regions_in_node[node_b]);
        self.regions_in_node[node_a].extend(regions_to_merge);
        self.node_size[node_a] += self.node_size[node_b];
        self.node_size[node_b] = 0;
    }

    fn unwrap_node(&mut self, node: usize, nwrap: i32) {
        if nwrap == 0 {
            return;
        }
        for &region in &self.regions_in_node[node] {
            self.unwrap_number[region] += nwrap;
        }
    }

    fn get_node_size(&self, node: usize) -> u32 {
        self.node_size[node]
    }
}

struct EdgeTracker {
    edges: Vec<EdgeState>,
    edges_in_node: Vec<Vec<usize>>,
    common_finder: Vec<bool>,
    common_index: Vec<usize>,
    last_base_node: Option<usize>,
}

impl EdgeTracker {
    fn new(edges: Vec<EdgeState>, nnodes: usize) -> Self {
        let mut edges_in_node = vec![Vec::new(); nnodes];
        for (edge_index, edge) in edges.iter().enumerate() {
            edges_in_node[edge.alpha].push(edge_index);
            edges_in_node[edge.beta].push(edge_index);
        }
        Self {
            edges,
            edges_in_node,
            common_finder: vec![false; nnodes],
            common_index: vec![0; nnodes],
            last_base_node: None,
        }
    }

    fn pop_edge(&self) -> Option<(usize, usize, f64, usize)> {
        let mut best_index = 0usize;
        let mut best_weight = i32::MIN;
        for (index, edge) in self.edges.iter().enumerate() {
            if edge.weight > best_weight {
                best_weight = edge.weight;
                best_index = index;
            }
        }
        if best_weight < 0 {
            return None;
        }
        let edge = &self.edges[best_index];
        Some((
            edge.alpha,
            edge.beta,
            edge.sum_diff / f64::from(edge.weight),
            best_index,
        ))
    }

    fn merge_nodes(&mut self, base_node: usize, merge_node: usize, foo_edge: usize) {
        self.edges[foo_edge].weight = -999;
        remove_edge(&mut self.edges_in_node[merge_node], foo_edge);
        remove_edge(&mut self.edges_in_node[base_node], foo_edge);
        self.common_finder[merge_node] = false;

        let edges_in_merge = self.edges_in_node[merge_node].clone();
        if self.last_base_node != Some(base_node) {
            self.common_finder.fill(false);
            let edges_in_base = self.edges_in_node[base_node].clone();
            for edge_num in edges_in_base {
                if self.edges[edge_num].beta == base_node {
                    self.reverse_edge_direction(edge_num);
                }
                debug_assert_eq!(self.edges[edge_num].alpha, base_node);
                let neighbor = self.edges[edge_num].beta;
                self.common_finder[neighbor] = true;
                self.common_index[neighbor] = edge_num;
            }
        }

        for edge_num in edges_in_merge {
            if self.edges[edge_num].beta == merge_node {
                self.reverse_edge_direction(edge_num);
            }
            debug_assert_eq!(self.edges[edge_num].alpha, merge_node);
            self.edges[edge_num].alpha = base_node;
            let neighbor = self.edges[edge_num].beta;
            if self.common_finder[neighbor] {
                let base_edge_num = self.common_index[neighbor];
                self.combine_edges(base_edge_num, edge_num, merge_node, neighbor);
            } else {
                self.common_finder[neighbor] = true;
                self.common_index[neighbor] = edge_num;
            }
        }

        let moved_edges = std::mem::take(&mut self.edges_in_node[merge_node]);
        self.edges_in_node[base_node].extend(moved_edges);
        self.last_base_node = Some(base_node);
    }

    fn combine_edges(
        &mut self,
        base_edge: usize,
        merge_edge: usize,
        merge_node: usize,
        neighbor_node: usize,
    ) {
        self.edges[base_edge].weight += self.edges[merge_edge].weight;
        self.edges[merge_edge].weight = -999;
        self.edges[base_edge].sum_diff += self.edges[merge_edge].sum_diff;
        remove_edge(&mut self.edges_in_node[merge_node], merge_edge);
        remove_edge(&mut self.edges_in_node[neighbor_node], merge_edge);
    }

    fn reverse_edge_direction(&mut self, edge: usize) {
        let edge = &mut self.edges[edge];
        std::mem::swap(&mut edge.alpha, &mut edge.beta);
        edge.sum_diff = -edge.sum_diff;
    }

    fn unwrap_node(&mut self, node: usize, nwrap: i32) {
        if nwrap == 0 {
            return;
        }
        for &edge_index in &self.edges_in_node[node] {
            let edge = &mut self.edges[edge_index];
            let delta = edge.weight * nwrap;
            if node == edge.alpha {
                edge.sum_diff += f64::from(delta);
            } else {
                debug_assert_eq!(node, edge.beta);
                edge.sum_diff -= f64::from(delta);
            }
        }
    }
}

fn remove_edge(edges: &mut Vec<usize>, edge: usize) {
    if let Some(pos) = edges.iter().position(|candidate| *candidate == edge) {
        edges.remove(pos);
    }
}

fn round_ties_even_to_i32(value: f64) -> i32 {
    value.round_ties_even() as i32
}
