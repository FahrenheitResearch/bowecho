//! Storm cell tracking — cross-volume association and motion estimation.
//!
//! Association is a GLOBAL ASSIGNMENT, not greedy nearest-neighbor: cost
//! matrix of distance + size + intensity consistency solved with the
//! Hungarian method, so clustered QLCS cells cannot steal each other's
//! tracks (TITAN: Dixon & Wiener 1993, J. Atmos. Oceanic Technol. 10(6),
//! 785–797, §3a). First-guess motion follows the SCIT fallback chain
//! (Johnson et al. 1998, Wea. Forecasting 13(2), 263–276, §2c); search
//! radii are speed-gated with a √(area/π) size term (Han et al. 2009,
//! JTECH 26(4); Lakshmanan & Smith 2010, Wea. Forecasting 25(2), 721–729);
//! splits and merges are linked by forecast-overlap (TITAN §3b). Motion is
//! an exponentially-weighted linear fit (TITAN §4).
//!
//! Pure module: no UI, no I/O — unit-testable and probe-able offline.

use chrono::{DateTime, Utc};
use std::collections::VecDeque;

use crate::cells::StormCell;

/// Association breaks entirely after this gap (Johnson et al. 1998
/// App. A #16: TIME = 20 min).
pub const TIME_GATE_S: f64 = 20.0 * 60.0;
/// Residual speed gate for tracks WITH a fitted motion (TITAN Eq. 4,
/// s_max = 60 km/h ≈ 16.7 m/s; rounded down).
const V_GATE_FITTED_MPS: f64 = 15.0;
/// Speed gate for tracks WITHOUT a motion estimate (SCIT App. A #15
/// default SPEED — the enlarged first-volume radius).
const V_GATE_UNFITTED_MPS: f64 = 30.0;
const GATE_FLOOR_KM: f64 = 5.0;
/// Cost weights (TITAN §3a study weights w1 = w2 = 1; w3 is ours — peak
/// reflectivity consistency per Lakshmanan & Smith 2010's finding that
/// intensity, not size, is the right consistency attribute).
const W1_DISTANCE: f64 = 1.0;
const W2_RADIUS: f64 = 1.0;
const W3_DBZ: f64 = 0.2;
/// Uniqueness pre-pass (Lakshmanan & Smith 2010, devised algorithm):
/// a single candidate within the cell's own radius AND 5 km associates
/// without entering the assignment.
const UNIQUE_MAX_KM: f64 = 5.0;
/// History depth (SCIT keeps up to 10 volumes, Johnson et al. 1998 §2c).
const HISTORY_CAP: usize = 10;
/// Motion fit window (TITAN n_t = 6 scans, Dixon & Wiener 1993 §4).
const FIT_WINDOW: usize = 6;
/// Exponential fit weights α^k, newest first (TITAN §4, Table 5 shows
/// skill flat for α ∈ 0.25–0.75).
const FIT_ALPHA: f64 = 0.5;
/// Fitted speeds above this keep the PREVIOUS motion and set `suspect`
/// (nulling the motion collapses the gate — the old QLCS breaker);
/// two consecutive violations null it.
const SPEED_SANITY_MPS: f64 = 60.0;
/// Coast unmatched tracks this many volumes before dropping (between
/// SCIT's 0 and Lakshmanan & Smith 2008's 3).
const COAST_VOLUMES: u32 = 2;

#[derive(Clone, Debug)]
pub struct StormTrack {
    pub id: u32,
    /// Observed fixes (time, east_km, north_km), newest last. Coasting adds
    /// no fixes.
    pub history: VecDeque<(DateTime<Utc>, f64, f64)>,
    pub max_dbz: f32,
    pub eq_radius_km: f64,
    /// Least-squares motion (east_mps, north_mps) — only from ≥2 fixes.
    pub fitted_motion: Option<(f64, f64)>,
    /// First-guess motion when no fit exists (SCIT §2c fallback chain or
    /// split inheritance). Never enters the fit.
    pub assumed_motion: Option<(f64, f64)>,
    pub missed: u32,
    /// Consecutive speed-sanity violations (2 nulls the motion).
    pub suspect: u32,
    /// Split lineage: the track this one budded from (TITAN §3b).
    pub parent_id: Option<u32>,
    /// Merge lineage: set when this track terminated into another.
    pub merged_into: Option<u32>,
}

impl StormTrack {
    fn new(id: u32, time: DateTime<Utc>, cell: &StormCell) -> Self {
        let mut history = VecDeque::with_capacity(HISTORY_CAP);
        history.push_back((time, cell.east_km, cell.north_km));
        Self {
            id,
            history,
            max_dbz: cell.max_dbz,
            eq_radius_km: cell.eq_radius_km,
            fitted_motion: None,
            assumed_motion: None,
            missed: 0,
            suspect: 0,
            parent_id: None,
            merged_into: None,
        }
    }

    pub fn last_fix(&self) -> Option<(DateTime<Utc>, f64, f64)> {
        self.history.back().copied()
    }

    /// Motion used for prediction/drawing: fitted, else assumed.
    pub fn motion(&self) -> Option<(f64, f64)> {
        self.fitted_motion.or(self.assumed_motion)
    }
}

/// The tracker state the app holds per site.
#[derive(Default)]
pub struct StormTracker {
    pub tracks: Vec<StormTrack>,
    next_id: u32,
    last_time: Option<DateTime<Utc>>,
}

impl StormTracker {
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.last_time = None;
    }

    /// Mean fitted motion across live tracks — the SCIT default-motion
    /// fallback and the app's "SRV←tracks" source.
    pub fn mean_fitted_motion(&self) -> Option<(f64, f64)> {
        let motions: Vec<(f64, f64)> = self
            .tracks
            .iter()
            .filter(|t| t.merged_into.is_none())
            .filter_map(|t| t.fitted_motion)
            .collect();
        if motions.is_empty() {
            return None;
        }
        let n = motions.len() as f64;
        Some((
            motions.iter().map(|m| m.0).sum::<f64>() / n,
            motions.iter().map(|m| m.1).sum::<f64>() / n,
        ))
    }

    /// Associate one COMPLETE volume's cells (the caller enforces the
    /// live-partial policy and monotonic volume times).
    pub fn associate(
        &mut self,
        time: DateTime<Utc>,
        cells: &[StormCell],
        user_motion_mps: Option<(f64, f64)>,
    ) {
        // Step 0: outage gate (Johnson et al. 1998 App. A #16).
        let dt_s = self
            .last_time
            .map(|t0| (time - t0).num_milliseconds() as f64 / 1000.0);
        if let Some(dt) = dt_s {
            if dt <= 0.0 {
                return; // duplicate or out-of-order volume
            }
            if dt > TIME_GATE_S {
                self.tracks.clear();
            }
        }
        self.last_time = Some(time);
        // Drop merged tombstones from the previous round.
        self.tracks.retain(|t| t.merged_into.is_none());

        // Step 1: first guess (SCIT §2c chain: own fit → mean of fits →
        // user motion → zero with the enlarged gate).
        let default_motion = self.mean_fitted_motion().or(user_motion_mps);
        for track in &mut self.tracks {
            if track.fitted_motion.is_none() && track.assumed_motion.is_none() {
                track.assumed_motion = default_motion;
            }
        }
        let predictions: Vec<(f64, f64)> = self
            .tracks
            .iter()
            .map(|t| {
                let (t0, e, n) = t.last_fix().expect("track has a fix");
                let dt_pred = (time - t0).num_milliseconds() as f64 / 1000.0;
                let (ve, vn) = t.motion().unwrap_or((0.0, 0.0));
                (e + ve * dt_pred / 1000.0, n + vn * dt_pred / 1000.0)
            })
            .collect();

        // Step 2: feasibility (speed-dependent search radius + size term).
        let n_tracks = self.tracks.len();
        let n_cells = cells.len();
        let feasible = |ti: usize, cj: usize| -> Option<f64> {
            let track = &self.tracks[ti];
            let (pe, pn) = predictions[ti];
            let cell = &cells[cj];
            let d = ((cell.east_km - pe).powi(2) + (cell.north_km - pn).powi(2)).sqrt();
            let dt_pred = {
                let (t0, ..) = track.last_fix().expect("fix");
                ((time - t0).num_milliseconds() as f64 / 1000.0).max(1.0)
            };
            let v_gate = if track.fitted_motion.is_some() {
                V_GATE_FITTED_MPS
            } else {
                V_GATE_UNFITTED_MPS
            };
            let radius = (v_gate * dt_pred / 1000.0 + cell.eq_radius_km).max(GATE_FLOOR_KM);
            (d <= radius).then_some(d)
        };

        let mut track_match: Vec<Option<usize>> = vec![None; n_tracks];
        let mut cell_match: Vec<Option<usize>> = vec![None; n_cells];

        // Step 3: uniqueness pre-pass (Lakshmanan & Smith 2010) — longest
        // tracks first (their AGE finding).
        let mut order: Vec<usize> = (0..n_tracks).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(self.tracks[i].history.len()));
        loop {
            let mut changed = false;
            for &ti in &order {
                if track_match[ti].is_some() {
                    continue;
                }
                let own_radius = self.tracks[ti].eq_radius_km.max(1.0);
                let mut sole: Option<(usize, f64)> = None;
                let mut count = 0;
                for (cj, matched) in cell_match.iter().enumerate().take(n_cells) {
                    if matched.is_some() {
                        continue;
                    }
                    if let Some(d) = feasible(ti, cj)
                        && d <= own_radius
                    {
                        count += 1;
                        sole = Some((cj, d));
                    }
                }
                if count == 1
                    && let Some((cj, d)) = sole
                    && d <= UNIQUE_MAX_KM
                {
                    track_match[ti] = Some(cj);
                    cell_match[cj] = Some(ti);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Step 4: global assignment on the remainder (Hungarian — greedy
        // nearest steals matches in clusters, the QLCS failure mode;
        // Dixon & Wiener 1993 §3a).
        let rem_tracks: Vec<usize> = (0..n_tracks)
            .filter(|&i| track_match[i].is_none())
            .collect();
        let rem_cells: Vec<usize> = (0..n_cells).filter(|&j| cell_match[j].is_none()).collect();
        if !rem_tracks.is_empty() && !rem_cells.is_empty() {
            let n = rem_tracks.len().max(rem_cells.len());
            const BIG: i64 = 1_000_000_000;
            let mut cost = vec![vec![BIG; n]; n];
            let mut max_finite: i64 = 0;
            for (ri, &ti) in rem_tracks.iter().enumerate() {
                for (rj, &cj) in rem_cells.iter().enumerate() {
                    if let Some(d) = feasible(ti, cj) {
                        let track = &self.tracks[ti];
                        let cell = &cells[cj];
                        let c = W1_DISTANCE * d
                            + W2_RADIUS * (track.eq_radius_km - cell.eq_radius_km).abs()
                            + W3_DBZ * (track.max_dbz as f64 - cell.max_dbz as f64).abs();
                        let scaled = (c * 1000.0).round() as i64;
                        cost[ri][rj] = scaled;
                        max_finite = max_finite.max(scaled);
                    }
                }
            }
            // Pad infeasible/dummy entries at 10x the max finite cost so
            // births and deaths fall out of the assignment automatically.
            let pad = (max_finite * 10).max(1);
            for row in cost.iter_mut() {
                for entry in row.iter_mut() {
                    if *entry == BIG {
                        *entry = pad;
                    }
                }
            }
            let assignment = hungarian_min(&cost);
            for (ri, &rj) in assignment.iter().enumerate() {
                if ri < rem_tracks.len() && rj < rem_cells.len() && cost[ri][rj] < pad {
                    track_match[rem_tracks[ri]] = Some(rem_cells[rj]);
                    cell_match[rem_cells[rj]] = Some(rem_tracks[ri]);
                }
            }
        }

        // Step 5a: merge pass — unmatched track whose prediction lands
        // inside a MATCHED cell terminates into that cell's track.
        for (ti, matched) in track_match.iter().enumerate().take(n_tracks) {
            if matched.is_some() {
                continue;
            }
            let (pe, pn) = predictions[ti];
            for (cj, cell) in cells.iter().enumerate() {
                let Some(winner) = cell_match[cj] else {
                    continue;
                };
                let d = ((cell.east_km - pe).powi(2) + (cell.north_km - pn).powi(2)).sqrt();
                if d <= cell.eq_radius_km {
                    let winner_id = self.tracks[winner].id;
                    self.tracks[ti].merged_into = Some(winner_id);
                    break;
                }
            }
        }

        // Step 6: bookkeeping for matched tracks.
        for (ti, matched) in track_match.iter().enumerate().take(n_tracks) {
            if let Some(cj) = *matched {
                let cell = &cells[cj];
                let track = &mut self.tracks[ti];
                track.history.push_back((time, cell.east_km, cell.north_km));
                while track.history.len() > HISTORY_CAP {
                    track.history.pop_front();
                }
                track.max_dbz = cell.max_dbz;
                track.eq_radius_km = cell.eq_radius_km;
                track.missed = 0;
                refit_motion(track);
            }
        }
        // Unmatched (and not merged): coast or drop.
        for track in &mut self.tracks {
            if track.merged_into.is_some() {
                continue;
            }
            let matched = track.last_fix().map(|(t, ..)| t == time).unwrap_or(false);
            if !matched {
                track.missed += 1;
            }
        }
        self.tracks
            .retain(|t| t.merged_into.is_some() || t.missed <= COAST_VOLUMES);

        // Step 5b: split pass — unmatched cells landing inside a track's
        // forecast circle become children inheriting the parent's motion
        // (TITAN §3b; a bowing QLCS segment must not start cold at zero).
        type SplitSource = (u32, (f64, f64), f64, Option<(f64, f64)>);
        let split_sources: Vec<SplitSource> = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.merged_into.is_none())
            .filter_map(|(i, t)| {
                let pred = predictions.get(i).copied()?;
                Some((t.id, pred, t.eq_radius_km, t.motion()))
            })
            .collect();
        for (cj, cell) in cells.iter().enumerate() {
            if cell_match[cj].is_some() {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            let mut track = StormTrack::new(id, time, cell);
            track.assumed_motion = default_motion;
            for (pid, (pe, pn), pr, pmotion) in &split_sources {
                let d = ((cell.east_km - pe).powi(2) + (cell.north_km - pn).powi(2)).sqrt();
                if d <= pr.max(GATE_FLOOR_KM) {
                    track.parent_id = Some(*pid);
                    if pmotion.is_some() {
                        track.assumed_motion = *pmotion;
                    }
                    break;
                }
            }
            self.tracks.push(track);
        }
    }
}

/// Weighted linear least squares of position vs time over the last
/// FIT_WINDOW fixes, weights α^k newest-first (TITAN §4), one-pass outlier
/// rejection, speed sanity that KEEPS the previous fit on violation.
fn refit_motion(track: &mut StormTrack) {
    let fixes: Vec<(f64, f64, f64)> = track
        .history
        .iter()
        .rev()
        .take(FIT_WINDOW)
        .map(|&(t, e, n)| (t.timestamp_millis() as f64 / 1000.0, e, n))
        .collect();
    if fixes.len() < 2 || (fixes[0].0 - fixes[fixes.len() - 1].0).abs() < 60.0 {
        return;
    }
    let fit = |points: &[(f64, f64, f64)]| -> Option<(f64, f64)> {
        let t0 = points[0].0;
        let mut sw = 0.0;
        let mut swt = 0.0;
        let mut swt2 = 0.0;
        let mut swe = 0.0;
        let mut swte = 0.0;
        let mut swn = 0.0;
        let mut swtn = 0.0;
        for (k, &(t, e, n)) in points.iter().enumerate() {
            let w = FIT_ALPHA.powi(k as i32);
            let dt = t - t0;
            sw += w;
            swt += w * dt;
            swt2 += w * dt * dt;
            swe += w * e;
            swte += w * dt * e;
            swn += w * n;
            swtn += w * dt * n;
        }
        let denom = sw * swt2 - swt * swt;
        if denom.abs() < 1e-6 {
            return None;
        }
        // km per second × 1000 = m/s
        let ve = (sw * swte - swt * swe) / denom * 1000.0;
        let vn = (sw * swtn - swt * swn) / denom * 1000.0;
        Some((ve, vn))
    };
    let Some(mut motion) = fit(&fixes) else {
        return;
    };
    // One-pass outlier rejection: drop a single point whose residual
    // exceeds max(5 km, 2.5x RMS) and refit.
    if fixes.len() >= 4 {
        let t0 = fixes[0].0;
        let (e0, n0) = (fixes[0].1, fixes[0].2);
        let residuals: Vec<f64> = fixes
            .iter()
            .map(|&(t, e, n)| {
                let dt = t - t0;
                let pe = e0 + motion.0 * dt / 1000.0;
                let pn = n0 + motion.1 * dt / 1000.0;
                ((e - pe).powi(2) + (n - pn).powi(2)).sqrt()
            })
            .collect();
        let rms = (residuals.iter().map(|r| r * r).sum::<f64>() / residuals.len() as f64).sqrt();
        let threshold = (2.5 * rms).max(5.0);
        let outliers: Vec<usize> = residuals
            .iter()
            .enumerate()
            .filter(|(_, r)| **r > threshold)
            .map(|(i, _)| i)
            .collect();
        if outliers.len() == 1 {
            let pruned: Vec<(f64, f64, f64)> = fixes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != outliers[0])
                .map(|(_, &p)| p)
                .collect();
            if let Some(refit) = fit(&pruned) {
                motion = refit;
            }
        }
    }
    // Speed sanity: keep the previous fit on a violation (nulling the
    // motion collapses the association gate — the old QLCS breaker);
    // null only after two consecutive violations.
    let speed = (motion.0.powi(2) + motion.1.powi(2)).sqrt();
    if speed > SPEED_SANITY_MPS {
        track.suspect += 1;
        if track.suspect >= 2 {
            track.fitted_motion = None;
        }
        return;
    }
    track.suspect = 0;
    track.fitted_motion = Some(motion);
}

/// O(n³) Hungarian (Kuhn–Munkres) minimum-cost assignment on a square
/// matrix. Returns assignment[row] = column. Standard potentials +
/// augmenting-path formulation; n ≤ 64 here, so microseconds.
fn hungarian_min(cost: &[Vec<i64>]) -> Vec<usize> {
    let n = cost.len();
    if n == 0 {
        return Vec::new();
    }
    // 1-indexed potentials formulation (e-maxx).
    let inf = i64::MAX / 4;
    let mut u = vec![0i64; n + 1];
    let mut v = vec![0i64; n + 1];
    let mut p = vec![0usize; n + 1]; // p[col] = row matched to col
    let mut way = vec![0usize; n + 1];
    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![inf; n + 1];
        let mut used = vec![false; n + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = inf;
            let mut j1 = 0usize;
            for j in 1..=n {
                if used[j] {
                    continue;
                }
                let cur = cost[i0 - 1][j - 1] - u[i0] - v[j];
                if cur < minv[j] {
                    minv[j] = cur;
                    way[j] = j0;
                }
                if minv[j] < delta {
                    delta = minv[j];
                    j1 = j;
                }
            }
            for j in 0..=n {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    let mut assignment = vec![usize::MAX; n];
    for j in 1..=n {
        if p[j] != 0 {
            assignment[p[j] - 1] = j - 1;
        }
    }
    assignment
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn cell(east: f64, north: f64, area: f64, dbz: f32) -> StormCell {
        StormCell {
            east_km: east,
            north_km: north,
            max_dbz: dbz,
            area_km2: area,
            eq_radius_km: (area / std::f64::consts::PI).sqrt(),
            mass: area * 1000.0,
            hlevel_dbz: dbz - 5.0,
        }
    }

    fn t(minutes: f64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt((minutes * 60_000.0) as i64)
            .unwrap()
    }

    #[test]
    fn hungarian_solves_a_known_matrix() {
        // Classic 3x3 with optimal assignment (0->1, 1->0, 2->2) cost 5+3+9=17?
        let cost = vec![vec![4, 1, 3], vec![2, 0, 5], vec![3, 2, 2]];
        let a = hungarian_min(&cost);
        let total: i64 = a.iter().enumerate().map(|(i, &j)| cost[i][j]).sum();
        assert_eq!(total, 5, "{a:?}"); // 1 + 2 + 2
    }

    #[test]
    fn crossing_cells_do_not_swap_ids() {
        // Lakshmanan & Smith 2010 mismatch class: two cells on crossing
        // lines, closest approach < 5 km; size+intensity terms must hold
        // the identities (greedy nearest swaps them).
        let mut tracker = StormTracker::default();
        // A: big 300 km2 / 60 dBZ heading east at 25 m/s along y=0..
        // B: small 80 km2 / 45 dBZ heading northeast.
        let pos_a = |k: f64| (-30.0 + 25.0 * 0.3 * k, -8.0); // 7.5 km/vol east
        let pos_b = |k: f64| {
            let s = 25.0 * 0.3 * k / 2.0f64.sqrt();
            (-30.0 + s, -25.0 + s)
        };
        for k in 0..6 {
            let (ae, an) = pos_a(k as f64);
            let (be, bn) = pos_b(k as f64);
            tracker.associate(
                t(5.0 * k as f64),
                &[cell(ae, an, 300.0, 60.0), cell(be, bn, 80.0, 45.0)],
                None,
            );
        }
        let live: Vec<&StormTrack> = tracker
            .tracks
            .iter()
            .filter(|t| t.merged_into.is_none() && t.history.len() >= 5)
            .collect();
        assert_eq!(live.len(), 2, "{:?}", tracker.tracks.len());
        // Track that is big/60dBZ must end on line A (y = -8).
        for track in live {
            let (_, _, n) = track.last_fix().unwrap();
            if track.max_dbz > 55.0 {
                assert!((n + 8.0).abs() < 1.5, "big cell left its line: {n}");
            } else {
                assert!(n > -20.0, "small cell stuck: {n}");
            }
        }
    }

    #[test]
    fn qlcs_line_no_steal() {
        // Five cells, 12 km spacing, all moving 30 m/s from 240°
        // (≈ toward 060°: displacement 9 km/volume — comparable to spacing,
        // the field-report regime). i -> i mapping must hold (TITAN §3a).
        let mut tracker = StormTracker::default();
        let motion_e = 30.0 * (60.0f64).to_radians().sin(); // toward 060
        let motion_n = 30.0 * (60.0f64).to_radians().cos();
        let line = |i: usize, k: f64| {
            let along = i as f64 * 12.0;
            (
                -40.0 + along * 0.5 + motion_e * 0.3 * k,
                -20.0 - along * 0.866 + motion_n * 0.3 * k,
            )
        };
        for k in 0..4 {
            let cells: Vec<StormCell> = (0..5)
                .map(|i| {
                    let (e, n) = line(i, k as f64);
                    cell(e, n, 60.0 + i as f64 * 10.0, 50.0 + i as f32)
                })
                .collect();
            tracker.associate(t(5.0 * k as f64), &cells, None);
        }
        let live: Vec<&StormTrack> = tracker
            .tracks
            .iter()
            .filter(|t| t.merged_into.is_none())
            .collect();
        assert_eq!(live.len(), 5, "births/deaths in a steady line");
        for track in live {
            assert_eq!(track.history.len(), 4, "track dropped a fix: {track:?}");
        }
    }

    #[test]
    fn split_links_children_to_parent() {
        let mut tracker = StormTracker::default();
        // Parent moving east at 20 m/s (6 km / 5 min).
        for k in 0..3 {
            tracker.associate(
                t(5.0 * k as f64),
                &[cell(6.0 * k as f64, 0.0, 200.0, 55.0)],
                None,
            );
        }
        let parent_id = tracker.tracks[0].id;
        assert!(tracker.tracks[0].fitted_motion.is_some());
        // Next volume: two children at forecast ± 4 km lateral.
        tracker.associate(
            t(15.0),
            &[cell(18.0, 4.0, 100.0, 54.0), cell(18.0, -4.0, 100.0, 53.0)],
            None,
        );
        let live: Vec<&StormTrack> = tracker
            .tracks
            .iter()
            .filter(|t| t.merged_into.is_none())
            .collect();
        assert_eq!(live.len(), 2);
        let continued = live.iter().find(|t| t.id == parent_id);
        let child = live.iter().find(|t| t.id != parent_id);
        assert!(continued.is_some(), "one child continues the parent track");
        let child = child.expect("second child is a new track");
        assert_eq!(child.parent_id, Some(parent_id), "split lineage missing");
        let (ve, _) = child.assumed_motion.expect("child inherits motion");
        assert!((ve - 20.0).abs() < 6.0, "child motion not inherited: {ve}");
    }

    #[test]
    fn merge_terminates_the_loser_with_a_link() {
        let mut tracker = StormTracker::default();
        // Two parents converging at ±10 m/s toward y=0.
        for k in 0..3 {
            let dy = 12.0 - 3.0 * k as f64;
            tracker.associate(
                t(5.0 * k as f64),
                &[cell(0.0, dy, 150.0, 55.0), cell(0.0, -dy, 150.0, 50.0)],
                None,
            );
        }
        // One merged cell at the midpoint.
        tracker.associate(t(15.0), &[cell(0.0, 0.0, 250.0, 56.0)], None);
        let live: Vec<&StormTrack> = tracker
            .tracks
            .iter()
            .filter(|t| t.merged_into.is_none())
            .collect();
        let merged: Vec<&StormTrack> = tracker
            .tracks
            .iter()
            .filter(|t| t.merged_into.is_some())
            .collect();
        assert_eq!(live.len(), 1, "exactly one survivor");
        assert_eq!(merged.len(), 1, "exactly one merge tombstone");
        assert_eq!(merged[0].merged_into, Some(live[0].id));
    }

    #[test]
    fn speed_gate_rejects_a_teleporting_cell() {
        let mut tracker = StormTracker::default();
        for k in 0..3 {
            tracker.associate(
                t(5.0 * k as f64),
                &[cell(7.5 * k as f64, 0.0, 100.0, 55.0)],
                None,
            );
        }
        let id = tracker.tracks[0].id;
        let fixes_before = tracker.tracks[0].history.len();
        // Only candidate is 35 km north of the prediction: residual
        // ≈ 117 m/s >> the 15 m/s fitted gate (Dixon & Wiener 1993 Eq. 4).
        tracker.associate(t(15.0), &[cell(22.5, 35.0, 100.0, 55.0)], None);
        let old = tracker.tracks.iter().find(|t| t.id == id).expect("coasts");
        assert_eq!(old.missed, 1);
        assert_eq!(old.history.len(), fixes_before, "history must not grow");
        assert!(
            tracker.tracks.iter().any(|t| t.id != id),
            "teleporter starts a fresh track"
        );
    }

    #[test]
    fn coast_and_reacquire_keeps_the_id() {
        let mut tracker = StormTracker::default();
        tracker.associate(t(0.0), &[cell(0.0, 0.0, 100.0, 55.0)], None);
        tracker.associate(t(5.0), &[cell(7.5, 0.0, 100.0, 55.0)], None);
        let id = tracker.tracks[0].id;
        // Volume 3: identification dropout.
        tracker.associate(t(10.0), &[], None);
        // Volume 4: back within 2 km of the extrapolated position (22.5, 0).
        tracker.associate(t(15.0), &[cell(22.0, 1.0, 100.0, 55.0)], None);
        let track = tracker
            .tracks
            .iter()
            .find(|t| t.id == id)
            .expect("track survived the dropout");
        assert_eq!(track.history.len(), 3, "no fabricated volume-3 fix");
        assert_eq!(track.missed, 0);
    }

    #[test]
    fn time_gate_resets_everything() {
        let mut tracker = StormTracker::default();
        tracker.associate(t(0.0), &[cell(0.0, 0.0, 100.0, 55.0)], None);
        tracker.associate(t(5.0), &[cell(7.5, 0.0, 100.0, 55.0)], None);
        let old_id = tracker.tracks[0].id;
        // 25 min gap > TIME = 20 min (Johnson et al. 1998 App. A #16).
        tracker.associate(t(30.0), &[cell(15.0, 0.0, 100.0, 55.0)], None);
        assert_eq!(tracker.tracks.len(), 1);
        assert_ne!(tracker.tracks[0].id, old_id, "old track must not survive");
        assert_eq!(tracker.tracks[0].history.len(), 1);
    }
}
