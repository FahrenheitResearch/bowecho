//! Post-solve repair gauntlet (spec §7): strict → relaxed modules run to
//! convergence, after UNRAVEL's modular design (Louf et al. 2020, *JTECH* 37,
//! 741–758, doi:10.1175/JTECH-D-19-0020.1 — window ladder) with
//! Zhang & Wang's defer-don't-guess ordering (2006, *JTECH* 23, 1239–1248).
//!
//! GLOBAL DO-NO-HARM RULES (every module):
//! (a) moves are whole ±2N Nyquist multiples only (fold increments);
//! (b) a gate inside the shear-couplet mask is never modified (R2D2,
//!     Feldmann et al. 2020, *JTECH* 37, 2341–2356 — failure F11);
//! (c) a module that wants to touch more than [`REPAIR_MAX_FRACTION`] of the
//!     tilt's finite gates aborts, changes nothing, and sets a diagnostic;
//! (d) every applied change strictly reduces the local objective it tested —
//!     no speculative flips.
//!
//! Modules:
//! - **R0 couplet mask** — compact azimuthal opposite-sign couplets
//!   (candidate mesocyclones / TVSs) are masked and exempt from repair.
//! - **R1 speck snap** — the 4-neighbor median despeckle
//!   (Holleman & Beekhuis 2003, *JTECH* 20, 443–453; Altube et al. 2017,
//!   *JTECH* 34, 1529–1543), expressed as fold moves.
//! - **R2 coherent-patch branch repair with ring closure** — the hybrid's
//!   guarded per-gate branch proposal (3-of-5 neighbor coherence)
//!   generalized: margins scale with the Nyquist interval so the pass stays
//!   live in low-Nyquist regimes (F10), and after a patch flips, its
//!   boundary ring is re-tested and joins the patch whenever doing so
//!   strictly reduces the gate's local fold-boundary count — the specific
//!   fix for the measured hybrid patch-ring regression (spec §1.4
//!   finding 2 / failure F8).
//! - **R3 box-median ladder** — 5×2, 20×10, 40×20 (az × range) windows
//!   (UNRAVEL's ladder); a gate > 1.2·N off the window median moves by the
//!   whole-2N step that lands within 0.4·2N of it.
//! - **R4 least-squares plane check** — UNRAVEL's linear-fit module over
//!   the largest window; one pass after R3 converges, skipped if R3 capped.
//!
//! R3/R4 evaluate only gates adjacent to a residual fold boundary
//! (deviation from UNRAVEL, which scans all gates): a gate with no
//! discontinuous neighbor cannot reduce the residual-boundary objective, and
//! the candidate filter keeps the ladder inside the interactive budget.

const COUPLET_MAX_AZIMUTH_GATES: usize = 5;
const COUPLET_MIN_DELTA_NYQUIST_FRAC: f32 = 1.4;
const COUPLET_DILATE_GATES: usize = 2;
/// Do-no-harm change cap per module, as a fraction of finite gates.
const REPAIR_MAX_FRACTION: f64 = 0.15;
/// R2 margins: `min(fixed, frac·2N)` keeps the pass live at low Nyquist
/// (failure F10 — v3's fixed 8 + 4 m/s margins went inert at 2N ≈ 23).
const PATCH_MIN_IMPROVEMENT_MPS: f32 = 8.0;
const PATCH_MIN_IMPROVEMENT_INTERVAL_FRAC: f32 = 0.3;
const PATCH_MIN_SEPARATION_MPS: f32 = 4.0;
const PATCH_MIN_SEPARATION_INTERVAL_FRAC: f32 = 0.15;
const PATCH_MAX_REFERENCE_ERROR_MPS: f32 = 18.0;
const PATCH_MAX_REFERENCE_ERROR_NYQUIST_FRAC: f32 = 0.90;
/// 3 agreeing cells in the 5-cell cross (self + 4 direct neighbors).
const PATCH_MIN_NEIGHBORS: u8 = 3;
const RING_MAX_PASSES: usize = 3;
/// A patch component that INCREASES its boundary-pair count is kept only
/// when it is at least this large.  A genuine F8 pocket (a folded lobe fused
/// into an identical-valued background) is a coherent feature; boundary-
/// increasing flips smaller than this are reference-misalignment rings —
/// the exact artifact class that inflated the hybrid's KEAX boundary count.
const PATCH_KEEP_MIN_GATES: usize = 64;
/// ...and only when the flip lands ON the reference: mean post-flip
/// |v − ref| ≤ 0.25·2N.  A misaligned reference endorses a flip loosely; a
/// true branch error is corrected almost exactly.
const PATCH_KEEP_MAX_RESIDUAL_INTERVAL_FRAC: f64 = 0.25;
/// The UNRAVEL-style window ladder, (azimuth rows × range gates).
const BOX_WINDOWS: [(usize, usize); 3] = [(5, 2), (20, 10), (40, 20)];
const BOX_MIN_SAMPLES: usize = 6;
/// A pair is a residual fold boundary when |Δv| > 1.2·min(N_a, N_b).
const BOUNDARY_NYQUIST_FRAC: f32 = 1.2;
/// A box/plane move must land within 0.4·2N of the window statistic.
const BOX_SNAP_INTERVAL_FRAC: f32 = 0.4;
const PLANE_MIN_SAMPLES: usize = 30;
/// R4 acts only where the plane actually describes the window: a two-branch
/// mixture (e.g. a compact folded/unfolded lobe inside the window) has a
/// huge residual and must be left to reference-backed modules.
const PLANE_MAX_RMS_NYQUIST_FRAC: f32 = 0.4;
const GAUNTLET_MAX_ROUNDS: usize = 3;

/// Immutable per-tilt inputs to the gauntlet.
pub(crate) struct RepairContext<'a> {
    pub(crate) observed: &'a [f32],
    pub(crate) nyq: &'a [f32],
    pub(crate) rows: usize,
    pub(crate) gates: usize,
    /// Whether the sweep closes 360° (row 0 and row rows−1 adjacent).
    pub(crate) wraps: bool,
    /// Combined repair reference (NaN = no opinion), priority already
    /// resolved by the caller: temporal·confidence, vertical, environmental.
    pub(crate) reference: Option<&'a [f32]>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepairDiagnostics {
    pub(crate) couplet_masked: usize,
    pub(crate) speck_snapped: usize,
    pub(crate) patch_changed: usize,
    pub(crate) ring_closed: usize,
    pub(crate) patch_reverted: usize,
    pub(crate) box_moved: usize,
    pub(crate) plane_moved: usize,
    /// Modules that hit the [`REPAIR_MAX_FRACTION`] cap and aborted.
    pub(crate) aborted_modules: u32,
}

impl RepairDiagnostics {
    pub(crate) fn accumulate(&mut self, other: &RepairDiagnostics) {
        self.couplet_masked += other.couplet_masked;
        self.speck_snapped += other.speck_snapped;
        self.patch_changed += other.patch_changed;
        self.ring_closed += other.ring_closed;
        self.patch_reverted += other.patch_reverted;
        self.box_moved += other.box_moved;
        self.plane_moved += other.plane_moved;
        self.aborted_modules += other.aborted_modules;
    }
}

struct Gauntlet<'a> {
    ctx: &'a RepairContext<'a>,
    folds: &'a mut [i32],
    confidence: &'a mut [u8],
    mask: Vec<bool>,
    finite_gates: usize,
    diagnostics: RepairDiagnostics,
}

/// Run the full gauntlet on one tilt.  `folds` and `confidence` are mutated
/// in place; the diagnostics report per-module change counts for the battery
/// to regression-gate.
pub(crate) fn run_gauntlet(
    ctx: &RepairContext<'_>,
    folds: &mut [i32],
    confidence: &mut [u8],
) -> RepairDiagnostics {
    let total = ctx.rows.saturating_mul(ctx.gates);
    if total == 0 || folds.len() != total || confidence.len() != total {
        return RepairDiagnostics::default();
    }
    let finite_gates = ctx
        .observed
        .iter()
        .filter(|value| value.is_finite())
        .count();
    if finite_gates == 0 {
        return RepairDiagnostics::default();
    }
    let mut gauntlet = Gauntlet {
        ctx,
        folds,
        confidence,
        mask: Vec::new(),
        finite_gates,
        diagnostics: RepairDiagnostics::default(),
    };
    gauntlet.mask = gauntlet.build_couplet_mask();
    gauntlet.diagnostics.couplet_masked = gauntlet.mask.iter().filter(|flag| **flag).count();

    gauntlet.speck_snap();
    gauntlet.patch_repair_with_ring_closure();
    for _ in 0..GAUNTLET_MAX_ROUNDS {
        let box_changes = gauntlet.box_median_ladder();
        let plane_changes = if box_changes.hit_cap {
            0
        } else {
            gauntlet.plane_check()
        };
        if box_changes.changes == 0 && plane_changes == 0 {
            break;
        }
    }
    gauntlet.diagnostics
}

struct ModuleOutcome {
    changes: usize,
    hit_cap: bool,
}

impl Gauntlet<'_> {
    #[inline]
    fn value(&self, idx: usize) -> f32 {
        let n = self.ctx.nyq[idx / self.ctx.gates];
        if n.is_finite() {
            self.ctx.observed[idx] + 2.0 * n * self.folds[idx] as f32
        } else {
            self.ctx.observed[idx]
        }
    }

    #[inline]
    fn row_above(&self, row: usize) -> Option<usize> {
        if row + 1 < self.ctx.rows {
            Some(row + 1)
        } else if self.ctx.wraps {
            Some(0)
        } else {
            None
        }
    }

    #[inline]
    fn row_below(&self, row: usize) -> Option<usize> {
        if row > 0 {
            Some(row - 1)
        } else if self.ctx.wraps {
            Some(self.ctx.rows - 1)
        } else {
            None
        }
    }

    fn change_cap(&self) -> usize {
        (REPAIR_MAX_FRACTION * self.finite_gates as f64).floor() as usize
    }

    /// True when the gate has at least two finite same-sign 8-neighbors —
    /// the "coherent lobe" prerequisite that keeps 1–2-gate speckle out of
    /// the couplet mask (R2D2 masks *features*, not isolated outliers; an
    /// unmaskable speck is exactly what R1/R3 exist to snap).
    fn has_same_sign_lobe(&self, idx: usize) -> bool {
        let (rows, gates) = (self.ctx.rows, self.ctx.gates);
        let (row, gate) = (idx / gates, idx % gates);
        let value = self.value(idx);
        let mut support = 0;
        for delta_row in -1i64..=1 {
            let mut sample_row = row as i64 + delta_row;
            if self.ctx.wraps {
                sample_row = sample_row.rem_euclid(rows as i64);
            } else if sample_row < 0 || sample_row >= rows as i64 {
                continue;
            }
            for delta_gate in -1i64..=1 {
                if delta_row == 0 && delta_gate == 0 {
                    continue;
                }
                let sample_gate = gate as i64 + delta_gate;
                if sample_gate < 0 || sample_gate >= gates as i64 {
                    continue;
                }
                let sample = sample_row as usize * gates + sample_gate as usize;
                if self.ctx.observed[sample].is_finite() && self.value(sample) * value > 0.0 {
                    support += 1;
                    if support >= 2 {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// R0: mask compact azimuthal opposite-sign couplets (|Δv| ≥ 1.4·N
    /// within ≤ 5 rows at the same range gate, both sides coherent lobes),
    /// dilated by 2 gates.
    fn build_couplet_mask(&self) -> Vec<bool> {
        let (rows, gates) = (self.ctx.rows, self.ctx.gates);
        let mut seeds: Vec<usize> = Vec::new();
        let mut mask = vec![false; rows * gates];
        for row in 0..rows {
            let n = self.ctx.nyq[row];
            if !n.is_finite() || n <= 0.0 {
                continue;
            }
            for gate in 0..gates {
                let idx = row * gates + gate;
                let value = self.value(idx);
                if !self.ctx.observed[idx].is_finite() {
                    continue;
                }
                let mut other_row = row;
                for _ in 0..COUPLET_MAX_AZIMUTH_GATES {
                    let Some(next) = self.row_above(other_row) else {
                        break;
                    };
                    other_row = next;
                    if other_row == row {
                        break; // tiny wrapped sweeps
                    }
                    let other_idx = other_row * gates + gate;
                    if !self.ctx.observed[other_idx].is_finite() {
                        continue;
                    }
                    let other_n = self.ctx.nyq[other_row];
                    if !other_n.is_finite() || other_n <= 0.0 {
                        continue;
                    }
                    let other_value = self.value(other_idx);
                    let threshold = COUPLET_MIN_DELTA_NYQUIST_FRAC * n.min(other_n);
                    if value * other_value < 0.0
                        && (value - other_value).abs() >= threshold
                        && self.has_same_sign_lobe(idx)
                        && self.has_same_sign_lobe(other_idx)
                    {
                        if !mask[idx] {
                            mask[idx] = true;
                            seeds.push(idx);
                        }
                        if !mask[other_idx] {
                            mask[other_idx] = true;
                            seeds.push(other_idx);
                        }
                    }
                }
            }
        }
        // Dilate by 2 gates in both dimensions (row wrap respected).
        let mut dilated = mask.clone();
        for idx in seeds {
            let (row, gate) = (idx / gates, idx % gates);
            for delta_row in -(COUPLET_DILATE_GATES as i64)..=(COUPLET_DILATE_GATES as i64) {
                let mut neighbor_row = row as i64 + delta_row;
                if self.ctx.wraps {
                    neighbor_row = neighbor_row.rem_euclid(rows as i64);
                } else if neighbor_row < 0 || neighbor_row >= rows as i64 {
                    continue;
                }
                for delta_gate in -(COUPLET_DILATE_GATES as i64)..=(COUPLET_DILATE_GATES as i64) {
                    let neighbor_gate = gate as i64 + delta_gate;
                    if neighbor_gate < 0 || neighbor_gate >= gates as i64 {
                        continue;
                    }
                    dilated[neighbor_row as usize * gates + neighbor_gate as usize] = true;
                }
            }
        }
        dilated
    }

    /// Finite 4-neighbor indices (azimuth wrap respected).
    fn neighbors(&self, idx: usize, out: &mut [usize; 4]) -> usize {
        let (rows, gates) = (self.ctx.rows, self.ctx.gates);
        let (row, gate) = (idx / gates, idx % gates);
        let mut count = 0;
        if let Some(below) = self.row_below(row) {
            out[count] = below * gates + gate;
            count += 1;
        }
        if let Some(above) = self.row_above(row)
            && !(rows == 1 || above == row)
        {
            out[count] = above * gates + gate;
            count += 1;
        }
        if gate > 0 {
            out[count] = idx - 1;
            count += 1;
        }
        if gate + 1 < gates {
            out[count] = idx + 1;
            count += 1;
        }
        count
    }

    /// R1: snap isolated outliers onto the nearest Nyquist multiple of the
    /// finite 4-neighbor median.  Proposals are computed against a snapshot,
    /// then applied atomically (or the module aborts on the cap).
    fn speck_snap(&mut self) {
        let mut proposals: Vec<(usize, i32)> = Vec::new();
        let mut neighbor_indices = [0usize; 4];
        for row in 0..self.ctx.rows {
            let n = self.ctx.nyq[row];
            if !n.is_finite() || n <= 0.0 {
                continue;
            }
            for gate in 0..self.ctx.gates {
                let idx = row * self.ctx.gates + gate;
                if self.mask[idx] || !self.ctx.observed[idx].is_finite() {
                    continue;
                }
                let value = self.value(idx);
                let count = self.neighbors(idx, &mut neighbor_indices);
                let mut finite = [0.0f32; 4];
                let mut finite_count = 0;
                for &neighbor in neighbor_indices.iter().take(count) {
                    if self.ctx.observed[neighbor].is_finite() {
                        finite[finite_count] = self.value(neighbor);
                        finite_count += 1;
                    }
                }
                if finite_count < 3 {
                    continue;
                }
                let median = median_small(&mut finite, finite_count);
                if (value - median).abs() > n {
                    let fold = ((median - value) / (2.0 * n)).round() as i32;
                    if fold != 0 {
                        proposals.push((idx, fold));
                    }
                }
            }
        }
        if proposals.len() > self.change_cap() {
            self.diagnostics.aborted_modules += 1;
            return;
        }
        for (idx, fold) in proposals {
            self.folds[idx] += fold;
            self.confidence[idx] = self.confidence[idx].min(super::confidence::SPECK_SNAPPED);
            self.diagnostics.speck_snapped += 1;
        }
    }

    /// R2: coherent-patch branch repair against the covering reference, plus
    /// ring closure.  All-or-nothing per the cap (rule c): the pre-module
    /// folds are restored on abort.
    fn patch_repair_with_ring_closure(&mut self) {
        let Some(reference) = self.ctx.reference else {
            return;
        };
        let total = self.ctx.rows * self.ctx.gates;
        let backup = self.folds.to_vec();

        // Per-gate proposals (hybrid's analytic nearest-branch form).
        let mut proposal = vec![0i8; total];
        for row in 0..self.ctx.rows {
            let n = self.ctx.nyq[row];
            if !n.is_finite() || n <= 0.0 {
                continue;
            }
            let interval = 2.0 * n;
            let improvement =
                PATCH_MIN_IMPROVEMENT_MPS.min(PATCH_MIN_IMPROVEMENT_INTERVAL_FRAC * interval);
            let separation =
                PATCH_MIN_SEPARATION_MPS.min(PATCH_MIN_SEPARATION_INTERVAL_FRAC * interval);
            let max_reference_error =
                PATCH_MAX_REFERENCE_ERROR_MPS.min(PATCH_MAX_REFERENCE_ERROR_NYQUIST_FRAC * n);
            for gate in 0..self.ctx.gates {
                let idx = row * self.ctx.gates + gate;
                if self.mask[idx] || !self.ctx.observed[idx].is_finite() {
                    continue;
                }
                let predicted = reference[idx];
                if !predicted.is_finite() {
                    continue;
                }
                let current = self.value(idx);
                let branch_float = ((predicted - current) / interval).round();
                if !branch_float.is_finite() || !(-2.0..=2.0).contains(&branch_float) {
                    continue;
                }
                let branch = branch_float as i8;
                if branch == 0 {
                    continue;
                }
                let best_error = (current + f32::from(branch) * interval - predicted).abs();
                let current_error = (current - predicted).abs();
                let second_error = interval - best_error;
                if current_error - best_error >= improvement
                    && second_error - best_error >= separation
                    && best_error <= max_reference_error
                {
                    proposal[idx] = branch;
                }
            }
        }

        // 3-of-5 coherence vote, then apply.
        let mut applied = vec![0i8; total];
        let mut changed: Vec<usize> = Vec::new();
        let mut neighbor_indices = [0usize; 4];
        for idx in 0..total {
            let delta = proposal[idx];
            if delta == 0 {
                continue;
            }
            let mut agreeing = 1u8;
            let count = self.neighbors(idx, &mut neighbor_indices);
            for &neighbor in neighbor_indices.iter().take(count) {
                agreeing += u8::from(proposal[neighbor] == delta);
            }
            if agreeing >= PATCH_MIN_NEIGHBORS {
                applied[idx] = delta;
                changed.push(idx);
            }
        }
        for &idx in &changed {
            self.folds[idx] += i32::from(applied[idx]);
        }

        // Ring closure: an unmoved gate adjacent to moved gates joins the
        // patch when taking the neighboring shift strictly reduces its local
        // fold-boundary count (rule d's local objective).
        let mut ring_closed: Vec<usize> = Vec::new();
        for _ in 0..RING_MAX_PASSES {
            let mut pass: Vec<(usize, i8)> = Vec::new();
            for idx in 0..total {
                if applied[idx] != 0 || self.mask[idx] || !self.ctx.observed[idx].is_finite() {
                    continue;
                }
                let row = idx / self.ctx.gates;
                let n = self.ctx.nyq[row];
                if !n.is_finite() || n <= 0.0 {
                    continue;
                }
                // Majority shift among moved 4-neighbors (ties → smaller
                // |δ|, then smaller δ — deterministic).
                let count = self.neighbors(idx, &mut neighbor_indices);
                let mut candidates: Vec<i8> = Vec::with_capacity(4);
                for &neighbor in neighbor_indices.iter().take(count) {
                    if applied[neighbor] != 0 {
                        candidates.push(applied[neighbor]);
                    }
                }
                if candidates.is_empty() {
                    continue;
                }
                candidates.sort_by_key(|delta| (delta.abs(), *delta));
                let mut best = (0usize, 0i8);
                let mut cursor = 0;
                while cursor < candidates.len() {
                    let delta = candidates[cursor];
                    let run = candidates[cursor..]
                        .iter()
                        .take_while(|other| **other == delta)
                        .count();
                    if run > best.0 {
                        best = (run, delta);
                    }
                    cursor += run;
                }
                let delta = best.1;
                let before = self.local_boundary_count(idx, 0.0);
                let after = self.local_boundary_count(idx, f32::from(delta) * 2.0 * n);
                if after < before {
                    pass.push((idx, delta));
                }
            }
            if pass.is_empty() {
                break;
            }
            for &(idx, delta) in &pass {
                self.folds[idx] += i32::from(delta);
                applied[idx] = delta;
                ring_closed.push(idx);
            }
        }

        let module_changes = changed.len() + ring_closed.len();
        if module_changes > self.change_cap() {
            self.folds.copy_from_slice(&backup);
            self.diagnostics.aborted_modules += 1;
            return;
        }

        // Per-patch do-no-harm audit (the §1.4 finding-2 fix, stated as an
        // objective): a genuinely wrapped lobe is CONTINUOUS with its
        // surroundings in truth, so flipping it must not increase the
        // residual boundary pairs on its perimeter.  A patch that inflates
        // boundaries is kept only when the covering reference endorses it
        // decisively (mean improvement ≥ half a Nyquist interval — a real
        // branch separation, not mapping noise).  This is what the hybrid's
        // patch pass lacked (KEAX 0.44°: 264 → 527 boundary pairs).
        let reverted = self.audit_patch_components(&mut applied, reference);
        self.diagnostics.patch_reverted += reverted;

        let mut kept_changed = 0usize;
        let mut kept_ring = 0usize;
        for &idx in &changed {
            if applied[idx] != 0 {
                self.confidence[idx] = self.confidence[idx].min(super::confidence::REPAIR_CHANGED);
                kept_changed += 1;
            }
        }
        for &idx in &ring_closed {
            if applied[idx] != 0 {
                self.confidence[idx] = self.confidence[idx].min(super::confidence::REPAIR_CHANGED);
                kept_ring += 1;
            }
        }
        self.diagnostics.patch_changed += kept_changed;
        self.diagnostics.ring_closed += kept_ring;
    }

    /// Group the applied patch gates into 4-connected components and revert
    /// every component that increases its local boundary-pair count without
    /// decisive reference backing.  Returns the number of reverted gates.
    fn audit_patch_components(&mut self, applied: &mut [i8], reference: &[f32]) -> usize {
        let total = self.ctx.rows * self.ctx.gates;
        let mut component_of = vec![u32::MAX; total];
        let mut components: Vec<Vec<usize>> = Vec::new();
        for start in 0..total {
            if applied[start] == 0 || component_of[start] != u32::MAX {
                continue;
            }
            let id = components.len() as u32;
            let mut members = Vec::new();
            let mut stack = vec![start];
            component_of[start] = id;
            let mut neighbor_indices = [0usize; 4];
            while let Some(idx) = stack.pop() {
                members.push(idx);
                let count = self.neighbors(idx, &mut neighbor_indices);
                for &neighbor in neighbor_indices.iter().take(count) {
                    if applied[neighbor] != 0 && component_of[neighbor] == u32::MAX {
                        component_of[neighbor] = id;
                        stack.push(neighbor);
                    }
                }
            }
            members.sort_unstable();
            components.push(members);
        }

        let mut reverted_gates = 0usize;
        let mut neighbor_indices = [0usize; 4];
        for (id, members) in components.iter().enumerate() {
            let id = id as u32;
            // Perimeter + internal boundary pairs, flipped vs reverted.
            let mut pairs_flipped = 0usize;
            let mut pairs_reverted = 0usize;
            let mut improvement_sum = 0.0f64;
            let mut flipped_residual_sum = 0.0f64;
            let mut interval_sum = 0.0f64;
            for &idx in members {
                let row = idx / self.ctx.gates;
                let n = self.ctx.nyq[row];
                let interval = 2.0 * n;
                let flipped = self.value(idx);
                let reverted = flipped - f32::from(applied[idx]) * interval;
                let predicted = reference[idx];
                if predicted.is_finite() {
                    improvement_sum +=
                        f64::from((reverted - predicted).abs() - (flipped - predicted).abs());
                    flipped_residual_sum += f64::from((flipped - predicted).abs());
                    interval_sum += f64::from(interval);
                }
                let count = self.neighbors(idx, &mut neighbor_indices);
                for &neighbor in neighbor_indices.iter().take(count) {
                    if !self.ctx.observed[neighbor].is_finite() {
                        continue;
                    }
                    let in_component = component_of[neighbor] == id;
                    if in_component && neighbor < idx {
                        continue; // count internal pairs once
                    }
                    let neighbor_n = self.ctx.nyq[neighbor / self.ctx.gates];
                    let threshold = BOUNDARY_NYQUIST_FRAC * n.min(neighbor_n);
                    if !threshold.is_finite() {
                        continue;
                    }
                    let neighbor_flipped = self.value(neighbor);
                    let neighbor_reverted = if in_component {
                        neighbor_flipped
                            - f32::from(applied[neighbor])
                                * 2.0
                                * self.ctx.nyq[neighbor / self.ctx.gates]
                    } else {
                        neighbor_flipped
                    };
                    pairs_flipped += usize::from((flipped - neighbor_flipped).abs() > threshold);
                    pairs_reverted += usize::from((reverted - neighbor_reverted).abs() > threshold);
                }
            }
            // Boundary-reducing (or neutral) components always stay: that is
            // the ring-closure objective itself.  Boundary-INCREASING
            // components must be coherent F8-class pocket repairs: large,
            // decisively endorsed (mean improvement ≥ half an interval), and
            // landing on the reference (mean residual ≤ a quarter interval).
            let decisive = interval_sum > 0.0
                && improvement_sum >= 0.5 * interval_sum
                && flipped_residual_sum <= PATCH_KEEP_MAX_RESIDUAL_INTERVAL_FRAC * interval_sum;
            let keep = pairs_flipped <= pairs_reverted
                || (members.len() >= PATCH_KEEP_MIN_GATES && decisive);
            if !keep {
                for &idx in members {
                    self.folds[idx] -= i32::from(applied[idx]);
                    applied[idx] = 0;
                    reverted_gates += 1;
                }
            }
        }
        reverted_gates
    }

    /// Window statistics never override the covering reference (do-no-harm
    /// corollary): a compact feature that the solver or R2 placed on a
    /// reference-backed branch looks identical to a wrong patch to a
    /// reference-free window median — both are minorities in a large window —
    /// so R3/R4 moves are vetoed when they would worsen agreement with the
    /// covering reference.
    fn reference_vetoes(&self, idx: usize, value: f32, moved: f32) -> bool {
        let Some(reference) = self.ctx.reference else {
            return false;
        };
        let predicted = reference[idx];
        if !predicted.is_finite() {
            return false;
        }
        (moved - predicted).abs() > (value - predicted).abs()
    }

    /// Residual fold-boundary pairs between `idx` (shifted by `shift` m/s)
    /// and its finite 4-neighbors.
    fn local_boundary_count(&self, idx: usize, shift: f32) -> usize {
        let row = idx / self.ctx.gates;
        let n = self.ctx.nyq[row];
        let value = self.value(idx) + shift;
        let mut neighbor_indices = [0usize; 4];
        let count = self.neighbors(idx, &mut neighbor_indices);
        let mut boundaries = 0;
        for &neighbor in neighbor_indices.iter().take(count) {
            if !self.ctx.observed[neighbor].is_finite() {
                continue;
            }
            let neighbor_n = self.ctx.nyq[neighbor / self.ctx.gates];
            let threshold = BOUNDARY_NYQUIST_FRAC * n.min(neighbor_n);
            if !threshold.is_finite() {
                continue;
            }
            if (value - self.value(neighbor)).abs() > threshold {
                boundaries += 1;
            }
        }
        boundaries
    }

    /// Gates adjacent to at least one residual fold boundary — the only
    /// gates R3/R4 test (see module doc).  Row-major order.
    fn boundary_candidates(&self) -> Vec<usize> {
        let (rows, gates) = (self.ctx.rows, self.ctx.gates);
        let mut flagged = vec![false; rows * gates];
        let flag_pair = |a: usize, b: usize, flagged: &mut Vec<bool>| {
            if !self.ctx.observed[a].is_finite() || !self.ctx.observed[b].is_finite() {
                return;
            }
            let threshold =
                BOUNDARY_NYQUIST_FRAC * self.ctx.nyq[a / gates].min(self.ctx.nyq[b / gates]);
            if threshold.is_finite() && (self.value(a) - self.value(b)).abs() > threshold {
                flagged[a] = true;
                flagged[b] = true;
            }
        };
        for row in 0..rows {
            for gate in 0..gates {
                let idx = row * gates + gate;
                if gate + 1 < gates {
                    flag_pair(idx, idx + 1, &mut flagged);
                }
                if row + 1 < rows {
                    flag_pair(idx, idx + gates, &mut flagged);
                }
            }
        }
        if self.ctx.wraps && rows > 1 {
            for gate in 0..gates {
                flag_pair((rows - 1) * gates + gate, gate, &mut flagged);
            }
        }
        (0..rows * gates).filter(|&idx| flagged[idx]).collect()
    }

    /// Window sample collection around `idx`, azimuth-wrapped, range-clamped.
    fn window_values(&self, idx: usize, window_rows: usize, window_gates: usize) -> Vec<f32> {
        let (rows, gates) = (self.ctx.rows, self.ctx.gates);
        let (row, gate) = (idx / gates, idx % gates);
        let half_rows = (window_rows / 2) as i64;
        let half_gates = (window_gates / 2) as i64;
        let mut values = Vec::with_capacity(window_rows * window_gates);
        for delta_row in -half_rows..=half_rows {
            let mut sample_row = row as i64 + delta_row;
            if self.ctx.wraps {
                sample_row = sample_row.rem_euclid(rows as i64);
            } else if sample_row < 0 || sample_row >= rows as i64 {
                continue;
            }
            let row_base = sample_row as usize * gates;
            for delta_gate in -half_gates..=half_gates {
                let sample_gate = gate as i64 + delta_gate;
                if sample_gate < 0 || sample_gate >= gates as i64 {
                    continue;
                }
                let sample = row_base + sample_gate as usize;
                if self.ctx.observed[sample].is_finite() {
                    values.push(self.value(sample));
                }
            }
        }
        values
    }

    /// R3: the box-median window ladder.  Defer-don't-guess (Zhang & Wang
    /// 2006): gates with no covering reference are LEFT ALONE — measured on
    /// the KEAX cold start, a reference-free ladder chases real storm
    /// structure at residual boundaries (280 → 454 pairs) instead of fixing
    /// it.
    fn box_median_ladder(&mut self) -> ModuleOutcome {
        let Some(reference) = self.ctx.reference else {
            return ModuleOutcome {
                changes: 0,
                hit_cap: false,
            };
        };
        let cap = self.change_cap();
        let mut module_changes = 0usize;
        for _ in 0..GAUNTLET_MAX_ROUNDS {
            let mut round_changes = 0usize;
            for &(window_rows, window_gates) in &BOX_WINDOWS {
                let candidates = self.boundary_candidates();
                for idx in candidates {
                    if self.mask[idx] || !reference[idx].is_finite() {
                        continue;
                    }
                    let row = idx / self.ctx.gates;
                    let n = self.ctx.nyq[row];
                    if !n.is_finite() || n <= 0.0 {
                        continue;
                    }
                    let mut values = self.window_values(idx, window_rows, window_gates);
                    if values.len() < BOX_MIN_SAMPLES {
                        continue;
                    }
                    let middle = values.len() / 2;
                    values.select_nth_unstable_by(middle, |left, right| left.total_cmp(right));
                    let median = values[middle];
                    let value = self.value(idx);
                    if (value - median).abs() <= BOUNDARY_NYQUIST_FRAC * n {
                        continue;
                    }
                    let interval = 2.0 * n;
                    let fold = ((median - value) / interval).round() as i32;
                    if fold == 0 {
                        continue;
                    }
                    let moved = value + fold as f32 * interval;
                    if (moved - median).abs() > BOX_SNAP_INTERVAL_FRAC * interval
                        || self.reference_vetoes(idx, value, moved)
                    {
                        continue;
                    }
                    if module_changes >= cap {
                        self.diagnostics.aborted_modules += 1;
                        return ModuleOutcome {
                            changes: module_changes,
                            hit_cap: true,
                        };
                    }
                    self.folds[idx] += fold;
                    self.confidence[idx] =
                        self.confidence[idx].min(super::confidence::REPAIR_CHANGED);
                    module_changes += 1;
                    round_changes += 1;
                    self.diagnostics.box_moved += 1;
                }
            }
            if round_changes == 0 {
                break;
            }
        }
        ModuleOutcome {
            changes: module_changes,
            hit_cap: false,
        }
    }

    /// R4: least-squares plane fit v(az, range) over the largest window;
    /// off-plane gates get the R3 move test against the plane value.  Same
    /// defer rule as R3: no covering reference, no move.
    fn plane_check(&mut self) -> usize {
        let Some(reference) = self.ctx.reference else {
            return 0;
        };
        let (window_rows, window_gates) = BOX_WINDOWS[2];
        let candidates = self.boundary_candidates();
        let mut changes = 0usize;
        let cap = self.change_cap();
        for idx in candidates {
            if self.mask[idx] || !reference[idx].is_finite() {
                continue;
            }
            let row = idx / self.ctx.gates;
            let n = self.ctx.nyq[row];
            if !n.is_finite() || n <= 0.0 {
                continue;
            }
            let Some((predicted, residual_rms)) = self.plane_fit(idx, window_rows, window_gates)
            else {
                continue;
            };
            if residual_rms > PLANE_MAX_RMS_NYQUIST_FRAC * n {
                continue; // the plane does not describe this window
            }
            let value = self.value(idx);
            if (value - predicted).abs() <= BOUNDARY_NYQUIST_FRAC * n {
                continue;
            }
            let interval = 2.0 * n;
            let fold = ((predicted - value) / interval).round() as i32;
            if fold == 0 {
                continue;
            }
            let moved = value + fold as f32 * interval;
            if (moved - predicted).abs() > BOX_SNAP_INTERVAL_FRAC * interval
                || self.reference_vetoes(idx, value, moved)
            {
                continue;
            }
            if changes >= cap {
                self.diagnostics.aborted_modules += 1;
                break;
            }
            self.folds[idx] += fold;
            self.confidence[idx] = self.confidence[idx].min(super::confidence::REPAIR_CHANGED);
            changes += 1;
            self.diagnostics.plane_moved += 1;
        }
        changes
    }

    /// Least-squares plane v = a + b·Δrow + c·Δgate over the window.
    /// Returns (value at the center, residual RMS); `None` when
    /// under-determined.
    fn plane_fit(&self, idx: usize, window_rows: usize, window_gates: usize) -> Option<(f32, f32)> {
        let (rows, gates) = (self.ctx.rows, self.ctx.gates);
        let (row, gate) = (idx / gates, idx % gates);
        let half_rows = (window_rows / 2) as i64;
        let half_gates = (window_gates / 2) as i64;
        // Normal equations for [a, b, c].
        let (mut s1, mut sr, mut sg, mut srr, mut srg, mut sgg) = (0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0);
        let (mut sv, mut srv, mut sgv, mut svv) = (0.0f64, 0.0, 0.0, 0.0);
        let mut samples = 0usize;
        for delta_row in -half_rows..=half_rows {
            let mut sample_row = row as i64 + delta_row;
            if self.ctx.wraps {
                sample_row = sample_row.rem_euclid(rows as i64);
            } else if sample_row < 0 || sample_row >= rows as i64 {
                continue;
            }
            let row_base = sample_row as usize * gates;
            for delta_gate in -half_gates..=half_gates {
                let sample_gate = gate as i64 + delta_gate;
                if sample_gate < 0 || sample_gate >= gates as i64 {
                    continue;
                }
                let sample = row_base + sample_gate as usize;
                if !self.ctx.observed[sample].is_finite() {
                    continue;
                }
                let value = f64::from(self.value(sample));
                let (dr, dg) = (delta_row as f64, delta_gate as f64);
                s1 += 1.0;
                sr += dr;
                sg += dg;
                srr += dr * dr;
                srg += dr * dg;
                sgg += dg * dg;
                sv += value;
                srv += dr * value;
                sgv += dg * value;
                svv += value * value;
                samples += 1;
            }
        }
        if samples < PLANE_MIN_SAMPLES {
            return None;
        }
        // Solve the 3×3 system by Cramer's rule.
        let det =
            s1 * (srr * sgg - srg * srg) - sr * (sr * sgg - srg * sg) + sg * (sr * srg - srr * sg);
        if det.abs() < 1e-6 {
            return None;
        }
        let det_a = sv * (srr * sgg - srg * srg) - sr * (srv * sgg - srg * sgv)
            + sg * (srv * srg - srr * sgv);
        let det_b =
            s1 * (srv * sgg - srg * sgv) - sv * (sr * sgg - srg * sg) + sg * (sr * sgv - srv * sg);
        let det_c =
            s1 * (srr * sgv - srv * srg) - sr * (sr * sgv - srv * sg) + sv * (sr * srg - srr * sg);
        let (a, b, c) = (det_a / det, det_b / det, det_c / det);
        // SSE = Σv² − θᵀ(Xᵀy) at the least-squares optimum.
        let sse = (svv - (a * sv + b * srv + c * sgv)).max(0.0);
        Some((a as f32, (sse / samples as f64).sqrt() as f32))
    }
}

fn median_small(values: &mut [f32; 4], count: usize) -> f32 {
    let slice = &mut values[..count];
    slice.sort_by(|left, right| left.total_cmp(right));
    if count % 2 == 1 {
        slice[count / 2]
    } else {
        0.5 * (slice[count / 2 - 1] + slice[count / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform_ctx<'a>(
        observed: &'a [f32],
        nyq: &'a [f32],
        rows: usize,
        gates: usize,
        reference: Option<&'a [f32]>,
    ) -> RepairContext<'a> {
        RepairContext {
            observed,
            nyq,
            rows,
            gates,
            wraps: true,
            reference,
        }
    }

    /// F11: a synthetic mesocyclone couplet (opposite-sign gate-to-gate
    /// azimuthal shear near 2N) must survive the whole gauntlet untouched.
    #[test]
    fn meso_couplet_survives_untouched() {
        let (rows, gates) = (60, 40);
        let nyq = vec![20.0f32; rows];
        let mut observed = vec![-2.0f32; rows * gates];
        // Couplet: rows 28-29 vs 30-31 at gates 18..22, ±17 m/s.
        for row in 28..30 {
            for gate in 18..22 {
                observed[row * gates + gate] = -17.0;
            }
        }
        for row in 30..32 {
            for gate in 18..22 {
                observed[row * gates + gate] = 17.0;
            }
        }
        let ctx = uniform_ctx(&observed, &nyq, rows, gates, None);
        let mut folds = vec![0i32; rows * gates];
        let mut confidence = vec![200u8; rows * gates];
        let diagnostics = run_gauntlet(&ctx, &mut folds, &mut confidence);
        assert!(diagnostics.couplet_masked > 0, "couplet must be masked");
        assert!(
            folds.iter().all(|fold| *fold == 0),
            "no gauntlet module may touch the couplet"
        );
    }

    /// The §1.4 finding-2 pin: when the covering reference has holes inside
    /// a folded lobe (exactly how the hybrid's quorum left ring/hole
    /// artifacts), R2 + ring closure must flip the WHOLE lobe — the repaired
    /// field may carry no more boundary pairs than the truth field does.
    #[test]
    fn patch_repair_closes_its_ring() {
        let (rows, gates) = (40, 40);
        let nyquist = 20.0f32;
        let nyq = vec![nyquist; rows];
        // Background truth −5; folded lobe truth +35 (observed −5 after
        // wrapping) in rows 10..20, gates 10..20.  Reference sees truth
        // except at three interior holes.
        let observed = vec![-5.0f32; rows * gates];
        let mut truth = vec![-5.0f32; rows * gates];
        let mut reference = vec![-5.0f32; rows * gates];
        for row in 10..20 {
            for gate in 10..20 {
                truth[row * gates + gate] = 35.0;
                reference[row * gates + gate] = 35.0;
            }
        }
        let holes = [12 * gates + 13, 15 * gates + 16, 17 * gates + 12];
        for &hole in &holes {
            reference[hole] = f32::NAN;
        }
        let ctx = uniform_ctx(&observed, &nyq, rows, gates, Some(&reference));
        let mut folds = vec![0i32; rows * gates];
        let mut confidence = vec![200u8; rows * gates];
        let diagnostics = run_gauntlet(&ctx, &mut folds, &mut confidence);
        assert!(diagnostics.patch_changed > 0, "patch must flip");
        assert!(
            diagnostics.ring_closed >= holes.len(),
            "reference holes must be closed by the ring pass: {diagnostics:?}"
        );
        for &hole in &holes {
            assert_eq!(folds[hole], 1, "hole gate must join the patch");
        }
        let count_boundaries = |field: &dyn Fn(usize) -> f32| {
            let mut boundaries = 0;
            for row in 0..rows {
                for gate in 0..gates {
                    let idx = row * gates + gate;
                    if gate + 1 < gates {
                        boundaries +=
                            usize::from((field(idx) - field(idx + 1)).abs() > 1.2 * nyquist);
                    }
                    let below = ((row + 1) % rows) * gates + gate;
                    boundaries += usize::from((field(idx) - field(below)).abs() > 1.2 * nyquist);
                }
            }
            boundaries
        };
        let repaired = |idx: usize| observed[idx] + 2.0 * nyquist * folds[idx] as f32;
        let truth_field = |idx: usize| truth[idx];
        assert_eq!(
            count_boundaries(&repaired),
            count_boundaries(&truth_field),
            "repair must not leave EXTRA boundary pairs beyond the truth lobe's own perimeter"
        );
    }

    /// Rule (c): a module that wants to touch more than 15% of the finite
    /// gates aborts and changes nothing.
    #[test]
    fn change_cap_aborts_the_patch_module() {
        let (rows, gates) = (20, 20);
        let nyq = vec![20.0f32; rows];
        let observed = vec![-5.0f32; rows * gates];
        // Reference claims EVERYTHING is +35: flipping all 400 gates would
        // exceed the 15% cap, so the module must abort.
        let reference = vec![35.0f32; rows * gates];
        let ctx = uniform_ctx(&observed, &nyq, rows, gates, Some(&reference));
        let mut folds = vec![0i32; rows * gates];
        let mut confidence = vec![200u8; rows * gates];
        let diagnostics = run_gauntlet(&ctx, &mut folds, &mut confidence);
        assert!(diagnostics.aborted_modules >= 1);
        assert_eq!(diagnostics.patch_changed, 0);
        assert!(folds.iter().all(|fold| *fold == 0));
    }

    /// R3 ladder: an isolated 2-gate clump a full fold off snaps to the box
    /// median and the ladder converges within its round budget.
    #[test]
    fn box_median_ladder_snaps_speckle_and_converges() {
        let (rows, gates) = (30, 30);
        let nyquist = 15.0f32;
        let nyq = vec![nyquist; rows];
        let mut observed = vec![3.0f32; rows * gates];
        // 2-gate clump wrapped a fold down: −27 observed (truth +3).
        observed[15 * gates + 15] = 3.0 - 2.0 * nyquist;
        observed[15 * gates + 16] = 3.0 - 2.0 * nyquist;
        let ctx = uniform_ctx(&observed, &nyq, rows, gates, None);
        let mut folds = vec![0i32; rows * gates];
        let mut confidence = vec![200u8; rows * gates];
        let diagnostics = run_gauntlet(&ctx, &mut folds, &mut confidence);
        // R1 needs 3 finite neighbors agreeing; the pair survives it but the
        // 5×2 box median catches both.
        assert!(
            diagnostics.speck_snapped + diagnostics.box_moved >= 2,
            "clump must snap: {diagnostics:?}"
        );
        assert_eq!(folds[15 * gates + 15], 1);
        assert_eq!(folds[15 * gates + 16], 1);
        assert_eq!(
            confidence[15 * gates + 15],
            confidence[15 * gates + 15].min(96),
            "repaired gates are demoted"
        );
    }
}
