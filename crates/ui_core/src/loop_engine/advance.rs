//! The ONE loop stepper (spec §3): the engine side of the legacy
//! `advance_primary_screen_loop` / `advance_extra_pane_screen_loop` pair,
//! whose frame-index math was already shared through
//! `next_frame_index_with_sweep_cuts` (app_ui). The engine wraps EXACTLY
//! that math over its own history and cursor; everything else about a step
//! stays in the caller, driven by [`StepOutcome`]:
//!
//! - WITHIN-frame low-sweep stepping (the cut-by-cut walk before the frame
//!   advances) runs BEFORE `advance_loop` and short-circuits it — it moves
//!   shared timeline state, not the cursor.
//! - Select side effects on `Stepped` (display install, timeline sync,
//!   satellite/model sync, camera follow, pane fan-out) are the caller's,
//!   per role — the engine never reaches into siblings.
//! - The caller re-evaluates `cursor.playing` after a step (legacy
//!   `history_playing = …_loop_can_step()`): can-step consults loop
//!   timeline summaries the engine does not own.
//! - The step CLOCK (`cursor.last_step`, texture-ready gating, repaint
//!   scheduling) stays in the caller's screen-loop scheduler.
//!
//! Landed at Phase 4e stage (i) engine-side only: the legacy fns still own
//! the live path until the stage-(ii) unify passes the differential gate
//! (fixture histories × sweep policies × starting cursors, identical index
//! sequences over 3 full revolutions).

use super::LoopEngine;
use super::frame_history::FrameHistoryEntry;

/// The caller's sweep policy for one step, reduced to what the frame-index
/// math actually consumes. Product/policy/disabled-cut knowledge is
/// app-side (census D7 pattern: caller-supplied predicate), so the engine
/// takes a predicate, not the types.
pub struct SweepContext<'a> {
    /// Sweep-range stepping is active — the legacy
    /// `app_settings.loop_low_sweeps && policy.mode == SweepPolicyMode::Range`
    /// gate. When false the step is a plain modulo advance and the
    /// predicate is never consulted.
    pub range_mode: bool,
    /// "Does this frame have any steppable sweep cuts for the active
    /// product?" — the legacy
    /// `!sweep_cuts_for_history_entry(frame, product, policy, disabled).is_empty()`.
    pub frame_has_cuts: &'a dyn Fn(&FrameHistoryEntry) -> bool,
}

impl SweepContext<'_> {
    /// The plain-advance context (`range_mode = false`): the predicate is
    /// never consulted, spelled out so call sites read as intent.
    pub const PLAIN: SweepContext<'static> = SweepContext {
        range_mode: false,
        frame_has_cuts: &|_| true,
    };
}

/// What one loop step did (spec §3): the caller keys every side effect on
/// this — the engine moved the cursor (or stopped playback) and nothing
/// else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// The cursor advanced to `index` (which may equal the previous index:
    /// a single-frame loop and a range-mode search that wraps clear around
    /// both land back where they started — legacy behavior, kept). The
    /// caller runs its role's select side effects and re-evaluates
    /// `cursor.playing` against its can-step rule.
    Stepped { index: usize },
    /// No steppable frame (empty history, or range mode found no frame
    /// with cuts): `cursor.playing` was cleared — the legacy
    /// stop-the-loop arm.
    Stopped,
}

impl LoopEngine {
    /// Advance the playback cursor one frame (the legacy stepper bodies'
    /// shared frame-index math, verbatim):
    ///
    /// - plain mode: `(index + 1) % len`, `Stopped` only when the history
    ///   is empty;
    /// - range mode: the first index at offsets `1..=len` (wrapping, so
    ///   the current index is the LAST candidate) whose frame passes
    ///   `sweep.frame_has_cuts`; `Stopped` when none does.
    ///
    /// Read-only over the history (debug-asserted): a step must never bump
    /// the generation — stepping is not a mutation of the frame strip.
    pub fn advance_loop(&mut self, sweep: &SweepContext) -> StepOutcome {
        let generation_before = self.history.generation();
        let next_index = if sweep.range_mode {
            self.next_index_with_sweep_cuts(sweep)
        } else {
            (!self.history.is_empty()).then(|| (self.cursor.index + 1) % self.history.len())
        };
        let outcome = match next_index {
            Some(index) => {
                self.cursor.index = index;
                StepOutcome::Stepped { index }
            }
            None => {
                self.cursor.playing = false;
                StepOutcome::Stopped
            }
        };
        debug_assert_eq!(
            self.history.generation(),
            generation_before,
            "advance_loop is read-only over the history and must never bump the generation"
        );
        outcome
    }

    /// The legacy `next_frame_index_with_sweep_cuts`, over the engine's own
    /// history/cursor: offsets `1..=len` so a lone steppable frame — even
    /// the current one — is still found, and an all-unsteppable history
    /// returns `None`.
    fn next_index_with_sweep_cuts(&self, sweep: &SweepContext) -> Option<usize> {
        if self.history.is_empty() {
            return None;
        }
        (1..=self.history.len())
            .map(|offset| (self.cursor.index + offset) % self.history.len())
            .find(|index| (sweep.frame_has_cuts)(&self.history[*index]))
    }
}

#[cfg(test)]
mod tests {
    use super::super::feed::FeedSource;
    use super::super::frame_history::{FrameStatus, frame_identity_for_volume};
    use super::super::{EngineId, EngineRole, LoopEngine};
    use super::*;
    use chrono::{TimeZone, Utc};
    use data_source::sites::SiteRef;
    use radar_core::{RadarSite, RadarVolume};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn engine_with_frames(count: usize) -> LoopEngine {
        let mut engine = LoopEngine::new(
            EngineId(1),
            EngineRole::Primary,
            FeedSource::Live(SiteRef::Us {
                level2_id: "KEAX".to_owned(),
            }),
        );
        for minute in 0..count {
            let volume = Arc::new(RadarVolume::new(
                RadarSite::new("KEAX"),
                Utc.with_ymd_and_hms(2026, 6, 9, 5, minute as u32, 0)
                    .unwrap(),
            ));
            engine.history.push(FrameHistoryEntry {
                identity: frame_identity_for_volume(&volume),
                path: PathBuf::from(format!("frame-{minute}")),
                volume,
                timings: None,
                status: FrameStatus::Complete,
                source_label: "step-test".to_owned(),
            });
        }
        engine
    }

    /// Plain mode is the legacy `(index + 1) % len`: a full wrap over three
    /// revolutions produces the exact legacy index sequence, a single-frame
    /// loop steps to itself, and only an empty history stops.
    #[test]
    fn plain_advance_wraps_modulo_len_like_the_legacy_stepper() {
        let mut engine = engine_with_frames(3);
        engine.cursor.playing = true;
        let mut sequence = Vec::new();
        for _ in 0..9 {
            match engine.advance_loop(&SweepContext::PLAIN) {
                StepOutcome::Stepped { index } => sequence.push(index),
                StepOutcome::Stopped => panic!("a non-empty plain loop never stops"),
            }
        }
        assert_eq!(sequence, vec![1, 2, 0, 1, 2, 0, 1, 2, 0]);
        assert!(
            engine.cursor.playing,
            "the engine never clears playing on a step"
        );

        let mut single = engine_with_frames(1);
        assert_eq!(
            single.advance_loop(&SweepContext::PLAIN),
            StepOutcome::Stepped { index: 0 },
            "a single-frame loop steps back onto itself (legacy)"
        );

        let mut empty = engine_with_frames(0);
        empty.cursor.playing = true;
        assert_eq!(
            empty.advance_loop(&SweepContext::PLAIN),
            StepOutcome::Stopped
        );
        assert!(
            !empty.cursor.playing,
            "Stopped clears playing (legacy stop arm)"
        );
    }

    /// Range mode is the legacy `next_frame_index_with_sweep_cuts`: skip
    /// frames the predicate rejects, find the lone steppable frame even
    /// when it is the current one (offset == len), and stop the loop when
    /// nothing is steppable.
    #[test]
    fn range_mode_skips_cutless_frames_and_stops_when_none_are_steppable() {
        // Frames 0 and 2 steppable, 1 not: from 0 the step lands on 2.
        let mut engine = engine_with_frames(3);
        let skip_frame_1 = |frame: &FrameHistoryEntry| {
            chrono::Timelike::minute(&frame.identity.scan_time_utc) != 1
        };
        let sweep = SweepContext {
            range_mode: true,
            frame_has_cuts: &skip_frame_1,
        };
        assert_eq!(
            engine.advance_loop(&sweep),
            StepOutcome::Stepped { index: 2 }
        );
        assert_eq!(
            engine.advance_loop(&sweep),
            StepOutcome::Stepped { index: 0 }
        );

        // Only the CURRENT frame steppable: the 1..=len wrap finds it.
        let mut engine = engine_with_frames(3);
        engine.cursor.index = 1;
        let only_frame_1 = |frame: &FrameHistoryEntry| {
            chrono::Timelike::minute(&frame.identity.scan_time_utc) == 1
        };
        assert_eq!(
            engine.advance_loop(&SweepContext {
                range_mode: true,
                frame_has_cuts: &only_frame_1,
            }),
            StepOutcome::Stepped { index: 1 }
        );

        // Nothing steppable: stop, clear playing (legacy None arm).
        let mut engine = engine_with_frames(3);
        engine.cursor.playing = true;
        assert_eq!(
            engine.advance_loop(&SweepContext {
                range_mode: true,
                frame_has_cuts: &|_| false,
            }),
            StepOutcome::Stopped
        );
        assert!(!engine.cursor.playing);
    }

    /// A step never bumps the history generation — stepping is playback,
    /// not a frame-strip mutation (the read-only half of the census-D12
    /// assertion harness).
    #[test]
    fn advance_loop_never_bumps_the_generation() {
        let mut engine = engine_with_frames(3);
        let before = engine.history.generation();
        let _ = engine.advance_loop(&SweepContext::PLAIN);
        let _ = engine.advance_loop(&SweepContext {
            range_mode: true,
            frame_has_cuts: &|_| true,
        });
        assert_eq!(engine.history.generation(), before);
    }
}
