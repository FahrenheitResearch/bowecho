//! FrameRing — miniDerecho's byte-budgeted frame history + cursor
//! (miniderecho-spec §13 Task 4). Deliberately shaped on the v0.29 spec's
//! names (`FollowNewestUnlessPlaying`, `HistoryLimits.byte_budget`) so the
//! swap onto `ui_core::loop_engine::LoopEngine` at v0.29 Phase 4c is
//! mechanical and DELETES this module (spec §10); mini's install/trim
//! tests transfer as acceptance tests.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use data_source::sites::SiteRef;
use radar_core::{MomentStorage, RadarVolume};

/// One retained frame. Identity is `(site, time)` — a bzip-preview install
/// followed by the full decode of the same volume upserts in place.
pub struct Frame {
    pub time: DateTime<Utc>,
    pub site: SiteRef,
    pub volume: Arc<RadarVolume>,
}

impl Frame {
    fn identity(&self) -> (SiteRef, DateTime<Utc>) {
        (self.site.clone(), self.time)
    }
}

/// Decoded size of a volume: the sum of its moment-grid buffer lengths in
/// bytes (U8/U16/F32 storage — radar_core). This is the per-entry cost the
/// byte budget charges.
pub fn volume_bytes(volume: &RadarVolume) -> usize {
    volume
        .cuts
        .iter()
        .flat_map(|cut| cut.moments.values())
        .map(|grid| match &grid.storage {
            MomentStorage::U8(values) => values.len(),
            MomentStorage::U16(values) => values.len() * 2,
            MomentStorage::F32(values) => values.len() * 4,
        })
        .sum()
}

/// Time-ordered frame history with a byte budget and the
/// `FollowNewestUnlessPlaying` cursor policy: the cursor follows a new
/// install only when it sat at the live edge and playback is not running;
/// otherwise it holds its frame (never its index).
pub struct FrameRing {
    frames: Vec<Frame>,
    cursor: usize,
    playing: bool,
    byte_budget: usize,
}

impl FrameRing {
    pub fn new(byte_budget: usize) -> Self {
        Self {
            frames: Vec::new(),
            cursor: 0,
            playing: false,
            byte_budget,
        }
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn playing(&self) -> bool {
        self.playing
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }

    pub fn current(&self) -> Option<&Frame> {
        self.frames.get(self.cursor)
    }

    /// At the live edge = showing the newest frame.
    pub fn at_live_edge(&self) -> bool {
        !self.frames.is_empty() && self.cursor + 1 == self.frames.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.frames
            .iter()
            .map(|frame| volume_bytes(&frame.volume))
            .sum()
    }

    /// Upsert by `(site, time)` identity, keep time order, trim to the byte
    /// budget (oldest first), and apply `FollowNewestUnlessPlaying`.
    pub fn install(&mut self, frame: Frame) {
        // Policy inputs are evaluated BEFORE the install mutates anything.
        let follow = self.frames.is_empty() || (self.at_live_edge() && !self.playing);
        let held = (!follow).then(|| self.frames[self.cursor].identity());

        let identity = frame.identity();
        match self.frames.iter().position(|f| f.identity() == identity) {
            Some(index) => self.frames[index] = frame,
            None => {
                self.frames.push(frame);
                // Stable sort: equal times keep install order.
                self.frames.sort_by(|a, b| a.time.cmp(&b.time));
            }
        }

        // Evict oldest while over budget; never evict the last frame.
        while self.frames.len() > 1 && self.total_bytes() > self.byte_budget {
            self.frames.remove(0);
        }

        self.cursor = match held {
            // Hold the same frame; if the trim evicted it, clamp to oldest.
            Some(identity) => self
                .frames
                .iter()
                .position(|f| f.identity() == identity)
                .unwrap_or(0),
            None => self.frames.len().saturating_sub(1),
        };
    }

    /// Manual step detaches from the live edge and pauses playback.
    pub fn step(&mut self, delta: i64) {
        if self.frames.is_empty() {
            return;
        }
        self.playing = false;
        let max = self.frames.len() as i64 - 1;
        self.cursor = (self.cursor as i64 + delta).clamp(0, max) as usize;
    }

    /// `L`: snap to newest and resume live-follow (paused at the edge is
    /// exactly the follow state under `FollowNewestUnlessPlaying`).
    pub fn jump_to_newest(&mut self) {
        self.playing = false;
        self.cursor = self.frames.len().saturating_sub(1);
    }

    /// Wrapping playback advance (the caller gates this on dwell timing and
    /// on the destination frame's render being available — hold, don't
    /// jitter).
    pub fn advance_wrapping(&mut self) {
        if self.frames.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.frames.len();
    }

    /// Index the wrapping advance would move to.
    pub fn next_index(&self) -> Option<usize> {
        (!self.frames.is_empty()).then(|| (self.cursor + 1) % self.frames.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use radar_core::{ElevationCut, GateRange, MomentGrid, MomentType, RadarSite};

    fn volume_of_bytes(bytes: usize) -> Arc<RadarVolume> {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: bytes,
        };
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range,
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        grid.push_u8_row_slice(0, &vec![7; bytes]).expect("row");
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.moments.insert(MomentType::Reflectivity, grid);
        let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());
        volume.cuts.push(cut);
        Arc::new(volume)
    }

    fn frame_at(minute: u32, bytes: usize) -> Frame {
        Frame {
            time: Utc.with_ymd_and_hms(2026, 6, 9, 5, minute, 0).unwrap(),
            site: SiteRef::parse_settings_key("KTLX"),
            volume: volume_of_bytes(bytes),
        }
    }

    #[test]
    fn volume_bytes_counts_storage_word_sizes() {
        let volume = volume_of_bytes(64);
        assert_eq!(volume_bytes(&volume), 64);

        // U16 and F32 grids cost 2 and 4 bytes per sample.
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 8,
        };
        let mut u16_grid = MomentGrid::new_u16(
            MomentType::Velocity,
            gate_range.clone(),
            2.0,
            64.0,
            Some(0),
            Some(1),
        );
        u16_grid
            .push_row(0, radar_core::MomentRow::U16(vec![9; 8]))
            .expect("u16 row");
        let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.moments.insert(MomentType::Velocity, u16_grid);
        volume.cuts.push(cut);
        assert_eq!(volume_bytes(&volume), 16);
    }

    #[test]
    fn install_into_empty_ring_shows_the_frame() {
        let mut ring = FrameRing::new(1_000);
        ring.install(frame_at(0, 10));
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.cursor(), 0);
        assert!(ring.at_live_edge());
    }

    #[test]
    fn follow_newest_unless_playing_truth_table() {
        // at edge × not playing → follow.
        let mut ring = FrameRing::new(1_000);
        ring.install(frame_at(0, 10));
        ring.install(frame_at(5, 10));
        assert_eq!(ring.cursor(), 1, "at-edge paused install follows newest");

        // at edge × playing → hold the frame under the cursor.
        ring.set_playing(true);
        ring.install(frame_at(10, 10));
        assert_eq!(ring.cursor(), 1, "playing install never moves the cursor");
        assert_eq!(ring.current().unwrap().time, frame_at(5, 10).time);

        // detached × not playing → hold.
        ring.set_playing(false);
        ring.step(-1); // cursor to frame 0
        ring.install(frame_at(15, 10));
        assert_eq!(ring.cursor(), 0, "detached paused install holds");

        // detached × playing → hold.
        ring.set_playing(true);
        ring.install(frame_at(20, 10));
        assert_eq!(ring.current().unwrap().time, frame_at(0, 10).time);
    }

    #[test]
    fn upsert_by_identity_replaces_the_volume_in_place() {
        let mut ring = FrameRing::new(1_000);
        ring.install(frame_at(0, 10));
        ring.install(frame_at(5, 10)); // preview
        assert_eq!(ring.len(), 2);

        // The full decode of the same (site, time) replaces the preview —
        // no growth, cursor stable, byte accounting updated.
        ring.install(frame_at(5, 40));
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.cursor(), 1);
        assert_eq!(ring.total_bytes(), 50);
    }

    #[test]
    fn byte_budget_evicts_oldest_and_keeps_cursor_on_its_frame() {
        // Budget for exactly three 10-byte frames.
        let mut ring = FrameRing::new(30);
        for minute in [0, 5, 10] {
            ring.install(frame_at(minute, 10));
        }
        // Detach onto the middle frame (t=5).
        ring.step(-1);
        assert_eq!(ring.current().unwrap().time, frame_at(5, 10).time);

        // Fourth frame busts the budget: t=0 evicted, order preserved,
        // cursor still on the t=5 frame (index shifted 1 → 0).
        ring.install(frame_at(15, 10));
        assert_eq!(ring.len(), 3);
        let times: Vec<_> = ring.frames().iter().map(|f| f.time).collect();
        assert_eq!(
            times,
            vec![
                frame_at(5, 10).time,
                frame_at(10, 10).time,
                frame_at(15, 10).time
            ]
        );
        assert_eq!(ring.cursor(), 0);
        assert_eq!(ring.current().unwrap().time, frame_at(5, 10).time);
    }

    #[test]
    fn eviction_of_the_cursor_frame_clamps_to_the_new_oldest() {
        let mut ring = FrameRing::new(30);
        for minute in [0, 5, 10] {
            ring.install(frame_at(minute, 10));
        }
        ring.step(-2); // cursor on t=0, the eviction candidate
        ring.install(frame_at(15, 10));
        assert_eq!(ring.cursor(), 0);
        assert_eq!(ring.current().unwrap().time, frame_at(5, 10).time);
    }

    #[test]
    fn a_single_oversized_frame_is_never_evicted() {
        let mut ring = FrameRing::new(5);
        ring.install(frame_at(0, 10));
        assert_eq!(
            ring.len(),
            1,
            "the only frame survives an over-budget install"
        );
        ring.install(frame_at(5, 10));
        assert_eq!(
            ring.len(),
            1,
            "over-budget ring holds exactly the newest frame"
        );
        assert_eq!(ring.current().unwrap().time, frame_at(5, 10).time);
    }

    #[test]
    fn step_clamps_and_detaches_and_jump_reattaches() {
        let mut ring = FrameRing::new(1_000);
        for minute in [0, 5, 10] {
            ring.install(frame_at(minute, 10));
        }
        ring.set_playing(true);
        ring.step(-10);
        assert_eq!(ring.cursor(), 0, "step clamps at the oldest frame");
        assert!(!ring.playing(), "stepping pauses playback");
        ring.step(1);
        assert_eq!(ring.cursor(), 1);

        ring.jump_to_newest();
        assert!(ring.at_live_edge());
        assert!(!ring.playing());
        // Reattached: the next install follows again.
        ring.install(frame_at(15, 10));
        assert!(ring.at_live_edge());
        assert_eq!(ring.current().unwrap().time, frame_at(15, 10).time);
    }

    #[test]
    fn advance_wrapping_loops_to_the_oldest_frame() {
        let mut ring = FrameRing::new(1_000);
        for minute in [0, 5] {
            ring.install(frame_at(minute, 10));
        }
        assert_eq!(ring.next_index(), Some(0));
        ring.advance_wrapping();
        assert_eq!(ring.cursor(), 0);
        ring.advance_wrapping();
        assert_eq!(ring.cursor(), 1);
    }
}
