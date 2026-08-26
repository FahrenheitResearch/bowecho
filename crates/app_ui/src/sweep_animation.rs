//! Smooth presentation of bursty live-radar sweep arrivals.
//!
//! Unidata Level II chunks expose many new radials at once. This state machine
//! reveals only radials that have arrived, at the antenna rate measured from
//! the cut's radial timestamps. All progress is represented as degrees swept
//! from the first usable radial so crossing the 360/0 seam remains monotonic.
//!
//! Ported from GenericRadar's production sweep animator at commit
//! dc94b7039efac709e397a132b09d0b46456269b0.

// This module is intentionally landed before its pane/render integration.
#![allow(dead_code)]

use std::time::Duration;

use radar_core::{ElevationCut, MomentType, RadarVolume};

const FULL_TURN_DEG: f64 = 360.0;
const FALLBACK_RATE_DEG_PER_S: f32 = 20.0;
const MIN_PLAUSIBLE_RATE_DEG_PER_S: f64 = 1.0;
const MAX_PLAUSIBLE_RATE_DEG_PER_S: f64 = 120.0;
const SNAP_THRESHOLD_DEG: f64 = 270.0;
const COMPLETION_SLACK_RADIALS: f64 = 1.5;
const MAX_COMPLETION_SLACK_DEG: f64 = 3.0;
const MAX_BACKWARD_JITTER_DEG: f64 = 5.0;
const SAME_START_TOLERANCE_DEG: f64 = 0.25;
const SAME_ELEVATION_TOLERANCE_DEG: f32 = 0.05;
const PENDING_ROUNDING_SLACK_DEG: f64 = 0.01;
const CATCHUP_REFERENCE_DEG: f32 = 45.0;
const MAX_CATCHUP: f32 = 6.0;
const MATCHING_TILT_DEG: f32 = 0.5;

/// Renderer-facing state for one partially arrived sweep.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepState {
    /// Azimuth of the furthest radial that has actually arrived.
    pub frontier_deg: f32,
    /// Azimuth to which the display has eased.
    pub presentation_deg: f32,
    /// First usable radial azimuth.
    pub start_deg: f32,
    /// Antenna rate measured from this cut's timestamps.
    pub rate_deg_per_s: f32,
    /// Clockwise arc revealed from start_deg, in 0..=360.
    pub revealed_deg: f32,
    /// Sticky once a complete turn has arrived and been revealed.
    pub complete: bool,
}

impl SweepState {
    /// Degrees that have arrived but have not yet been presented.
    pub fn pending_deg(&self) -> f32 {
        if self.complete {
            return 0.0;
        }
        let ahead_deg = wrap_360(f64::from(self.frontier_deg) - f64::from(self.presentation_deg));
        if ahead_deg > FULL_TURN_DEG - PENDING_ROUNDING_SLACK_DEG {
            return 0.0;
        }
        ahead_deg as f32
    }
}

/// Turns bursty radial arrivals into a smooth clockwise reveal.
///
/// Keep one animator per radar pane. Reset it whenever that pane changes site,
/// product, volume, or cut.
#[derive(Default)]
pub struct SweepAnimator {
    track: Option<Track>,
    state: Option<SweepState>,
}

impl SweepAnimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe the current cut after a wall-clock interval.
    ///
    /// Radial timestamps determine rate; elapsed_since_last determines how far
    /// the presentation may advance. The result never passes arrived data.
    pub fn observe(
        &mut self,
        cut: &ElevationCut,
        elapsed_since_last: Duration,
    ) -> Option<SweepState> {
        let Some(measured) = MeasuredSweep::from_cut(cut) else {
            self.track = None;
            self.state = None;
            return None;
        };

        let earlier = self
            .track
            .filter(|earlier| earlier.is_same_sweep_as(&measured));

        let eased_swept_deg = match earlier {
            Some(earlier) if earlier.steps_smoothly_to(&measured) => {
                earlier.presentation_swept_deg
                    + f64::from(measured.rate_deg_per_s) * elapsed_since_last.as_secs_f64()
            }
            _ => measured.frontier_swept_deg,
        };

        // This clamp is the central safety invariant: never invent a radial.
        let presentation_swept_deg = eased_swept_deg.clamp(0.0, measured.frontier_swept_deg);
        let caught_up = presentation_swept_deg >= measured.frontier_swept_deg;
        let complete =
            earlier.is_some_and(|earlier| earlier.complete) || (measured.is_closed && caught_up);
        let revealed_deg = if complete {
            FULL_TURN_DEG
        } else {
            presentation_swept_deg
        };

        let state = SweepState {
            frontier_deg: measured.frontier_deg as f32,
            presentation_deg: wrap_360(measured.start_deg + revealed_deg) as f32,
            start_deg: measured.start_deg as f32,
            rate_deg_per_s: measured.rate_deg_per_s,
            revealed_deg: revealed_deg as f32,
            complete,
        };

        self.track = Some(Track {
            elevation_number: measured.elevation_number,
            elevation_deg: measured.elevation_deg,
            radial_count: measured.radial_count,
            start_deg: measured.start_deg,
            frontier_swept_deg: measured.frontier_swept_deg,
            presentation_swept_deg,
            complete,
        });
        self.state = Some(state);
        Some(state)
    }

    /// Forget the currently followed sweep.
    pub fn reset(&mut self) {
        self.track = None;
        self.state = None;
    }

    pub fn state(&self) -> Option<SweepState> {
        self.state
    }
}

#[derive(Clone, Copy, Debug)]
struct Track {
    elevation_number: Option<u8>,
    elevation_deg: f32,
    radial_count: usize,
    start_deg: f64,
    frontier_swept_deg: f64,
    presentation_swept_deg: f64,
    complete: bool,
}

impl Track {
    fn is_same_sweep_as(&self, measured: &MeasuredSweep) -> bool {
        self.elevation_number == measured.elevation_number
            && (self.elevation_deg - measured.elevation_deg).abs() <= SAME_ELEVATION_TOLERANCE_DEG
            && (self.start_deg - measured.start_deg).abs() <= SAME_START_TOLERANCE_DEG
            && measured.radial_count >= self.radial_count
    }

    fn steps_smoothly_to(&self, measured: &MeasuredSweep) -> bool {
        let step_deg = measured.frontier_swept_deg - self.frontier_swept_deg;
        (0.0..=SNAP_THRESHOLD_DEG).contains(&step_deg)
    }
}

#[derive(Clone, Copy, Debug)]
struct MeasuredSweep {
    elevation_number: Option<u8>,
    elevation_deg: f32,
    radial_count: usize,
    start_deg: f64,
    frontier_deg: f64,
    frontier_swept_deg: f64,
    rate_deg_per_s: f32,
    is_closed: bool,
}

impl MeasuredSweep {
    fn from_cut(cut: &ElevationCut) -> Option<Self> {
        let first = cut.radials.first()?;
        let last = cut.radials.last()?;
        let mut azimuths_deg = cut
            .radials
            .iter()
            .map(|radial| f64::from(radial.azimuth_deg))
            .filter(|azimuth_deg| azimuth_deg.is_finite());
        let start_deg = azimuths_deg.next()?;

        let mut previous_deg = start_deg;
        let mut swept_deg = 0.0;
        let mut forward_steps = 0_usize;
        for azimuth_deg in azimuths_deg {
            let step_deg = forward_step_deg(previous_deg, azimuth_deg);
            // Ignore small backwards antenna jitter without moving the cursor.
            // Moving it backwards would count the return from the wobble twice.
            if step_deg > 0.0 {
                swept_deg += step_deg;
                forward_steps += 1;
                previous_deg = azimuth_deg;
            }
        }

        let frontier_swept_deg = swept_deg.min(FULL_TURN_DEG);
        let span_ms = i64::from(last.time_offset_ms) - i64::from(first.time_offset_ms);
        let rate_deg_per_s = measure_rate_deg_per_s(swept_deg, span_ms);
        let mean_step_deg = if forward_steps == 0 {
            0.0
        } else {
            swept_deg / forward_steps as f64
        };
        let completion_slack_deg =
            (mean_step_deg * COMPLETION_SLACK_RADIALS).min(MAX_COMPLETION_SLACK_DEG);
        let is_closed = swept_deg + completion_slack_deg >= FULL_TURN_DEG;

        Some(Self {
            elevation_number: cut.elevation_number,
            elevation_deg: cut.elevation_deg,
            radial_count: cut.radials.len(),
            start_deg,
            frontier_deg: wrap_360(start_deg + frontier_swept_deg),
            frontier_swept_deg,
            rate_deg_per_s,
            is_closed,
        })
    }
}

fn measure_rate_deg_per_s(swept_deg: f64, span_ms: i64) -> f32 {
    if span_ms <= 0 || swept_deg <= 0.0 {
        return FALLBACK_RATE_DEG_PER_S;
    }
    let rate_deg_per_s = swept_deg / (span_ms as f64 / 1000.0);
    if !rate_deg_per_s.is_finite()
        || !(MIN_PLAUSIBLE_RATE_DEG_PER_S..=MAX_PLAUSIBLE_RATE_DEG_PER_S).contains(&rate_deg_per_s)
    {
        return FALLBACK_RATE_DEG_PER_S;
    }
    rate_deg_per_s as f32
}

fn wrap_360(degrees: f64) -> f64 {
    let wrapped = degrees % FULL_TURN_DEG;
    if wrapped < 0.0 {
        wrapped + FULL_TURN_DEG
    } else {
        wrapped
    }
}

/// Read the step as clockwise travel unless it is a small backwards jitter.
fn forward_step_deg(from_deg: f64, to_deg: f64) -> f64 {
    let forward_deg = wrap_360(to_deg - from_deg);
    if forward_deg > FULL_TURN_DEG - MAX_BACKWARD_JITTER_DEG {
        forward_deg - FULL_TURN_DEG
    } else {
        forward_deg
    }
}

/// Multiplier for the caller's elapsed time while presentation trails arrival.
///
/// The reported antenna rate remains honest; only presentation catch-up is
/// accelerated. Invalid or non-positive backlog degrades to normal speed.
pub fn catch_up_factor(pending_deg: f32) -> f32 {
    if !pending_deg.is_finite() || pending_deg <= 0.0 {
        return 1.0;
    }
    (1.0 + pending_deg / CATCHUP_REFERENCE_DEG).min(MAX_CATCHUP)
}

/// Find the compatible prior cut used to underpaint an arriving sweep.
///
/// Elevation number wins because it distinguishes SAILS repeats. If it is not
/// usable, choose the fullest moment-bearing cut within the angle tolerance.
pub fn matching_cut_index(
    volume: &RadarVolume,
    target: &ElevationCut,
    moment: &MomentType,
) -> Option<usize> {
    let carries_moment =
        |cut: &ElevationCut| cut.moments.contains_key(moment) && !cut.radials.is_empty();

    if let Some(number) = target.elevation_number
        && let Some(index) = volume
            .cuts
            .iter()
            .position(|cut| cut.elevation_number == Some(number) && carries_moment(cut))
    {
        return Some(index);
    }

    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| carries_moment(cut))
        .filter(|(_, cut)| (cut.elevation_deg - target.elevation_deg).abs() <= MATCHING_TILT_DEG)
        .max_by_key(|(_, cut)| cut.radials.len())
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    use radar_core::{GateRange, MomentGrid, RadarSite, Radial};

    fn radial(azimuth_deg: f32, time_offset_ms: i32) -> Radial {
        Radial {
            azimuth_deg,
            elevation_deg: 0.5,
            time_offset_ms,
            gate_range: GateRange {
                first_gate_m: 2_125,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            nyquist_velocity_mps: Some(26.0),
            radial_status: None,
        }
    }

    fn cut(start_deg: f32, step_deg: f32, count: usize, ms_per_radial: i32) -> ElevationCut {
        cut_at_elevation(0.5, Some(1), start_deg, step_deg, count, ms_per_radial)
    }

    fn cut_at_elevation(
        elevation_deg: f32,
        elevation_number: Option<u8>,
        start_deg: f32,
        step_deg: f32,
        count: usize,
        ms_per_radial: i32,
    ) -> ElevationCut {
        let mut cut = ElevationCut::new(elevation_deg, elevation_number);
        for index in 0..count {
            let azimuth_deg =
                wrap_360(f64::from(start_deg) + f64::from(step_deg) * index as f64) as f32;
            cut.radials
                .push(radial(azimuth_deg, ms_per_radial * index as i32));
        }
        cut
    }

    fn observed(animator: &mut SweepAnimator, cut: &ElevationCut, elapsed: Duration) -> SweepState {
        animator
            .observe(cut, elapsed)
            .expect("a populated cut must produce sweep state")
    }

    #[test]
    fn the_rate_is_measured_from_the_cuts_own_timestamps() {
        let state = observed(
            &mut SweepAnimator::new(),
            &cut(0.0, 1.0, 360, 40),
            Duration::ZERO,
        );
        assert!((state.rate_deg_per_s - 25.0).abs() < 1e-3);
    }

    #[test]
    fn a_zero_seam_hole_is_measured_as_clockwise_progress() {
        let partial = cut(197.5, 1.0, 240, 40);
        let state = observed(&mut SweepAnimator::new(), &partial, Duration::ZERO);
        assert_eq!(state.start_deg, 197.5);
        assert_eq!(state.frontier_deg, 76.5);
        assert!((state.revealed_deg - 239.0).abs() < 1e-3);
        assert!(!state.complete);
        assert_eq!(state.pending_deg(), 0.0);
    }

    #[test]
    fn a_chunk_sized_frontier_jump_eases_instead_of_snapping() {
        let mut animator = SweepAnimator::new();
        let first = observed(&mut animator, &cut(0.0, 0.5, 240, 30), Duration::ZERO);
        assert_eq!(first.revealed_deg, 119.5);

        let grown = cut(0.0, 0.5, 480, 30);
        let state = observed(&mut animator, &grown, Duration::from_millis(16));
        assert!((state.revealed_deg - 119.766_67).abs() < 1e-3);
        assert!(state.revealed_deg < 239.5);
    }

    #[test]
    fn presentation_never_passes_the_arrived_frontier() {
        let mut animator = SweepAnimator::new();
        observed(&mut animator, &cut(0.0, 1.0, 120, 40), Duration::ZERO);
        let grown = cut(0.0, 1.0, 180, 40);
        for _ in 0..600 {
            let state = observed(&mut animator, &grown, Duration::from_millis(16));
            assert!(state.revealed_deg <= 179.0 + 1e-4);
            assert!(state.pending_deg() <= 60.0 + 1e-4);
        }
        assert!((animator.state().unwrap().revealed_deg - 179.0).abs() < 1e-3);
    }

    #[test]
    fn completion_is_sticky_and_never_wraps_back_to_empty() {
        let mut animator = SweepAnimator::new();
        let full = cut(197.5, 1.0, 360, 40);
        for elapsed in [
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(5),
        ] {
            let state = observed(&mut animator, &full, elapsed);
            assert!(state.complete);
            assert_eq!(state.revealed_deg, 360.0);
            assert_eq!(state.presentation_deg, 197.5);
            assert_eq!(state.pending_deg(), 0.0);
        }
    }

    #[test]
    fn completion_slack_closes_real_dense_sweeps_not_sparse_spokes() {
        let dense = observed(
            &mut SweepAnimator::new(),
            &cut(88.0, 0.5, 720, 30),
            Duration::ZERO,
        );
        assert!(dense.complete);

        let mut sparse = ElevationCut::new(0.5, Some(1));
        sparse.radials.push(radial(0.0, 0));
        sparse.radials.push(radial(120.0, 4_000));
        sparse.radials.push(radial(240.0, 8_000));
        let sparse = observed(&mut SweepAnimator::new(), &sparse, Duration::ZERO);
        assert!(!sparse.complete);
        assert_eq!(sparse.revealed_deg, 240.0);
    }

    #[test]
    fn reset_and_a_restarted_volume_do_not_inherit_old_progress() {
        let mut animator = SweepAnimator::new();
        assert!(observed(&mut animator, &cut(0.0, 1.0, 360, 40), Duration::ZERO,).complete);
        let restarted = observed(
            &mut animator,
            &cut(0.0, 1.0, 40, 40),
            Duration::from_millis(16),
        );
        assert_eq!(restarted.revealed_deg, 39.0);
        assert!(!restarted.complete);

        animator.reset();
        assert_eq!(animator.state(), None);
        let after_reset = observed(&mut animator, &cut(0.0, 1.0, 180, 40), Duration::ZERO);
        assert_eq!(after_reset.revealed_deg, 179.0);
    }

    #[test]
    fn non_finite_azimuths_are_skipped_without_poisoning_state() {
        let mut poisoned = cut(0.0, 1.0, 90, 40);
        poisoned.radials[0].azimuth_deg = f32::NAN;
        poisoned.radials[45].azimuth_deg = f32::INFINITY;
        let state = observed(&mut SweepAnimator::new(), &poisoned, Duration::ZERO);
        assert_eq!(state.start_deg, 1.0);
        assert!(state.frontier_deg.is_finite());
        assert!(state.presentation_deg.is_finite());
        assert!(state.revealed_deg.is_finite());
        assert!(state.rate_deg_per_s.is_finite());

        for radial in &mut poisoned.radials {
            radial.azimuth_deg = f32::NAN;
        }
        assert_eq!(
            SweepAnimator::new().observe(&poisoned, Duration::ZERO),
            None
        );
    }

    #[test]
    fn randomized_arrivals_preserve_all_reveal_invariants() {
        let mut seed = 0x2026_0817_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for trial in 0..150 {
            let start_deg = f64::from(next() as u32 % 3600) / 10.0;
            let step_deg = if trial % 2 == 0 { 1.0 } else { 0.5 };
            let ms_per_radial = if trial % 2 == 0 { 40 } else { 30 };
            let radial_limit = (FULL_TURN_DEG / step_deg) as usize;
            let mut animator = SweepAnimator::new();
            let mut count = 0;
            let mut previous_revealed = 0.0;
            let mut was_complete = false;

            for _ in 0..60 {
                if next() % 3 == 0 {
                    count = (count + 1 + next() as usize % 300).min(radial_limit);
                }
                if count == 0 {
                    continue;
                }
                let state = observed(
                    &mut animator,
                    &cut(start_deg as f32, step_deg as f32, count, ms_per_radial),
                    Duration::from_millis(u64::from(next() as u32 % 3_000)),
                );
                let frontier = ((count - 1) as f64 * step_deg).min(FULL_TURN_DEG);
                assert!(state.revealed_deg.is_finite());
                assert!((0.0..=360.0).contains(&state.revealed_deg));
                assert!(f64::from(state.revealed_deg) <= frontier + 1e-3 || state.complete);
                assert!(state.revealed_deg >= previous_revealed - 1e-3);
                assert!(!was_complete || state.complete);
                let expected = wrap_360(start_deg + f64::from(state.revealed_deg)) as f32;
                assert!((state.presentation_deg - expected).abs() < 1e-2);
                previous_revealed = state.revealed_deg;
                was_complete = state.complete;
            }
        }
    }

    #[test]
    fn catch_up_factor_is_bounded_and_rejects_invalid_backlog() {
        assert_eq!(catch_up_factor(0.0), 1.0);
        assert_eq!(catch_up_factor(-30.0), 1.0);
        assert_eq!(catch_up_factor(f32::NAN), 1.0);
        assert!((catch_up_factor(120.0) - 3.666_667).abs() < 1e-3);
        assert_eq!(catch_up_factor(359.0), MAX_CATCHUP);
    }

    fn tilt(
        elevation_number: Option<u8>,
        elevation_deg: f32,
        radial_count: usize,
        moment: MomentType,
    ) -> ElevationCut {
        let mut cut = cut_at_elevation(
            elevation_deg,
            elevation_number,
            0.0,
            360.0 / radial_count as f32,
            radial_count,
            30,
        );
        let grid = MomentGrid::new_u8(
            moment.clone(),
            GateRange {
                first_gate_m: 2_000,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            1.0,
            0.0,
            None,
            None,
        );
        cut.moments.insert(moment, grid);
        cut
    }

    fn volume_of(cuts: Vec<ElevationCut>) -> RadarVolume {
        let mut volume = RadarVolume::default();
        volume.site = RadarSite::new("KTLX");
        volume.cuts = cuts;
        volume
    }

    #[test]
    fn matching_prefers_elevation_number_for_sails_repeats() {
        let previous = volume_of(vec![
            tilt(Some(1), 0.53, 720, MomentType::Velocity),
            tilt(Some(2), 0.48, 720, MomentType::Velocity),
            tilt(Some(15), 0.51, 720, MomentType::Velocity),
        ]);
        let arriving = tilt(Some(15), 0.62, 90, MomentType::Velocity);
        assert_eq!(
            matching_cut_index(&previous, &arriving, &MomentType::Velocity),
            Some(2)
        );
    }

    #[test]
    fn matching_requires_the_moment_and_falls_back_to_the_fullest_nearby_cut() {
        let previous = volume_of(vec![
            tilt(Some(1), 0.50, 120, MomentType::Reflectivity),
            tilt(None, 0.55, 720, MomentType::Velocity),
            tilt(None, 2.40, 720, MomentType::Velocity),
        ]);
        let arriving = tilt(Some(1), 0.64, 30, MomentType::Velocity);

        assert_eq!(
            matching_cut_index(&previous, &arriving, &MomentType::Velocity),
            Some(1)
        );
        assert_eq!(
            matching_cut_index(&previous, &arriving, &MomentType::SpectrumWidth),
            None
        );
    }
}
