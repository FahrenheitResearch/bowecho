//! The ONE install path (census D1-D9, D12, D16): cross-site guard →
//! upsert by identity → sort by identity → trim to limits → cursor per
//! [`SelectionPolicy`]. Side effects (display writes, status strings,
//! storm caches, timeline sync) stay in the CALLER, driven by
//! [`InstallOutcome`] — the engine never reaches into siblings (spec §3).

use std::sync::Arc;

use radar_core::RadarVolume;

use super::LoopEngine;
use super::frame_history::{
    DecodedLoad, DecodedLoadBatch, FrameHistoryEntry, FrameIdentity,
    frame_history_entry_from_decoded, frame_identity_for_volume, frame_status_priority,
    history_contains_other_site, live_partial_frame_has_new_data,
};
use super::policy::{
    HistoryLimits, MAX_HISTORY_FRAME_LIMIT, SelectionPolicy, estimated_entry_bytes,
};

/// What one install did (census D6/D16). Supersedes the legacy bool
/// returns; [`InstallOutcome::anchor_selected`] preserves their exact
/// per-arm mapping because drain status strings key on it (D11/D16).
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use = "the caller owns every side effect: display writes, status strings, cache clears"]
pub struct InstallOutcome {
    /// The cursor/selection arm that ran.
    pub selection: InstallSelection,
    /// Present when the cross-site guard fired BEFORE the install (census
    /// D3). Primary-role callers key their extra clears (storm caches,
    /// loop render cache, product-cut memory — census D4) on this.
    pub cross_site_clear: Option<CrossSiteClear>,
    /// Census D9, owner decision "LIVE WINS": when the polled route
    /// installs while the loop plays, the DISPLAY must still show the new
    /// volume even though the cursor held. `Some(index)` = the caller
    /// writes `history[index]`'s volume to the display regardless of the
    /// selection arm. Only the `FollowNewestUnlessPlaying` route sets it.
    pub display_install: Option<usize>,
}

impl InstallOutcome {
    /// The legacy `install_decoded_load_batch` bool (census D16): true
    /// only when the anchor frame got selected — deferral, backfill, and
    /// cursor-hold arms all returned false.
    pub fn anchor_selected(&self) -> bool {
        matches!(self.selection, InstallSelection::SelectedAnchor { .. })
    }

    fn empty() -> Self {
        Self {
            selection: InstallSelection::NoFrames,
            cross_site_clear: None,
            display_install: None,
        }
    }
}

/// The selection arm an install ended in. One variant per census-D5 arm so
/// callers dispatch side effects with a plain match, no re-derivation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallSelection {
    /// Empty batch: nothing installed, nothing moved.
    NoFrames,
    /// The anchor was selected (`SelectAnchor` guards passed). The caller
    /// runs its role's select side effects (census D6: timeline sync and
    /// camera follow are Primary-only; pane select is pane-local).
    SelectedAnchor { index: usize },
    /// `FollowNewestUnlessPlaying`, not playing: the cursor followed the
    /// feed to the installed frame (legacy polled-route behavior).
    FollowedNewest { index: usize },
    /// `FollowNewestUnlessPlaying`, playing: the cursor held (census D5a)
    /// — but `display_install` still points at the newest frame (D9).
    HeldWhilePlaying { newest_index: usize },
    /// The anchor was NOT selected (guards blocked, or `Backfill` policy);
    /// the cursor was repositioned so the frame displayed before the
    /// install stays under it after indices shifted (census D5c,
    /// engine-common). Legacy status: `"Backfilled {site}"`.
    Backfilled { index: usize },
    /// Census D7: the anchor is a live-partial that cannot yet materialize
    /// the caller's product; the batch stayed INSTALLED, selection was
    /// withheld, and the cursor repositioned to the active frame. Legacy
    /// status: `"Waiting for {product} in {site}"` — the caller writes it
    /// (product names are app-side).
    DeferredLivePartial { anchor: FrameIdentity },
    /// `KeepCursor` policy, or a blocked select with no active frame to
    /// backfill onto: the cursor did not move (trims may still have
    /// clamped it into range).
    CursorUnchanged,
}

/// The cross-site guard fired (census D3). `diagnostic` is the greppable
/// legacy wording — `"history reset (site change to {site}, had {n}
/// frames)"` — returned for the caller to surface; per the census decision
/// the polled path GAINS this diagnostic (it was silent in legacy P3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossSiteClear {
    pub new_site_id: String,
    pub dropped_frames: usize,
    pub diagnostic: String,
}

impl LoopEngine {
    /// Install one decoded frame (the polled/live route and the overlay
    /// single-frame route). Equivalent to
    /// `install_batch(DecodedLoadBatch::single(decoded), …)` with no
    /// deferral hook: single-frame routes never defer (census D7 rows P3
    /// and P4 — polled frames are always `Complete`, overlays have no
    /// deferral logic).
    pub fn install_frame(
        &mut self,
        decoded: DecodedLoad,
        policy: &SelectionPolicy,
        active_volume: Option<&Arc<RadarVolume>>,
    ) -> InstallOutcome {
        self.install_batch(
            DecodedLoadBatch::single(decoded),
            policy,
            active_volume,
            |_anchor| false,
        )
    }

    /// Install a decoded batch: cross-site guard → upsert (3-tier, census
    /// D2) → sort ascending by identity → trim (frames, then bytes) →
    /// cursor per `policy`.
    ///
    /// - `active_volume` is the volume currently DISPLAYED by this
    ///   engine's view (the display stays in the caller until adoption
    ///   completes); it feeds the cross-site guard, the backfill
    ///   reposition, and the blank-display escape.
    /// - `defer_anchor` is the census-D7 hook: evaluated at most once, on
    ///   the post-upsert anchor entry, only when the anchor is about to be
    ///   selected. The predicate is caller-supplied because deferral
    ///   depends on the active product (app-side knowledge); `|_| false`
    ///   means "never defer".
    /// - Replace-all (the legacy overlay install, census D1) is spelled
    ///   `engine.clear_history(); engine.install_batch(...)` — an explicit
    ///   clear, never a fourth upsert mode.
    ///
    /// Invariant (census D12): every route through this method bumps the
    /// history generation — enforced by a debug assertion, not review.
    pub fn install_batch(
        &mut self,
        batch: DecodedLoadBatch,
        policy: &SelectionPolicy,
        active_volume: Option<&Arc<RadarVolume>>,
        defer_anchor: impl FnOnce(&FrameHistoryEntry) -> bool,
    ) -> InstallOutcome {
        if batch.frames.is_empty() {
            return InstallOutcome::empty();
        }
        let generation_before = self.history.generation();

        // Census D8: grow-to-fit is stated intent, not path sniffing.
        if self.limits.grow_to_fit && batch.frames.len() > 1 {
            self.limits.frame_limit = self
                .limits
                .frame_limit
                .max(batch.frames.len())
                .min(MAX_HISTORY_FRAME_LIMIT);
        }

        let selected_index = batch.selected_index.min(batch.frames.len() - 1);
        let selected_identity = frame_identity_for_volume(&batch.frames[selected_index].volume);
        let active_identity =
            active_volume.map(|volume| frame_identity_for_volume(volume.as_ref()));

        // Census D3: one cross-site guard for every route, with the
        // diagnostic wording every path now shares.
        let cross_site_clear = if active_volume
            .is_some_and(|volume| volume.site.id != selected_identity.site_id)
            || history_contains_other_site(&self.history, &selected_identity.site_id)
        {
            let dropped_frames = self.history.len();
            // Diagnostic: a surprise clear here is the "every frame
            // replaces the previous" failure mode — make it visible.
            let diagnostic = format!(
                "history reset (site change to {}, had {dropped_frames} frames)",
                selected_identity.site_id
            );
            self.clear_history();
            Some(CrossSiteClear {
                new_site_id: selected_identity.site_id.clone(),
                dropped_frames,
                diagnostic,
            })
        } else {
            None
        };

        for decoded in batch.frames {
            self.upsert_history_frame(decoded);
        }
        self.history
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        self.trim_history_to_limits();

        let selection = self.apply_selection_policy(
            policy,
            &selected_identity,
            active_identity.as_ref(),
            active_volume.is_none(),
            defer_anchor,
        );
        let display_install = match (&selection, policy) {
            // Census D9, owner decision LIVE WINS: the polled route
            // installs the display unconditionally, cursor or no cursor.
            (
                InstallSelection::FollowedNewest { index }
                | InstallSelection::HeldWhilePlaying {
                    newest_index: index,
                },
                SelectionPolicy::FollowNewestUnlessPlaying,
            ) => Some(*index),
            _ => None,
        };

        debug_assert_ne!(
            self.history.generation(),
            generation_before,
            "census D12: every install route must bump the history generation"
        );

        InstallOutcome {
            selection,
            cross_site_clear,
            display_install,
        }
    }

    /// Cursor/selection per census D5. Split out so the arms read as the
    /// census table, not as control flow inside the install body.
    fn apply_selection_policy(
        &mut self,
        policy: &SelectionPolicy,
        selected_identity: &FrameIdentity,
        active_identity: Option<&FrameIdentity>,
        display_is_blank: bool,
        defer_anchor: impl FnOnce(&FrameHistoryEntry) -> bool,
    ) -> InstallSelection {
        let anchor_index = self
            .history
            .iter()
            .position(|frame| frame.identity == *selected_identity)
            .unwrap_or_else(|| self.history.len().saturating_sub(1));
        match policy {
            SelectionPolicy::FollowNewestUnlessPlaying => {
                if self.cursor.playing {
                    InstallSelection::HeldWhilePlaying {
                        newest_index: anchor_index,
                    }
                } else {
                    // D5a: browsing does NOT block the follow — deliberate.
                    self.cursor.index = anchor_index;
                    InstallSelection::FollowedNewest {
                        index: anchor_index,
                    }
                }
            }
            SelectionPolicy::SelectAnchor {
                blank_display_overrides_browsing,
            } => {
                let should_select = !self.cursor.playing
                    && (!self.cursor.browsing
                        || (*blank_display_overrides_browsing && display_is_blank));
                if should_select {
                    if let Some(anchor) = self.history.get(anchor_index)
                        && defer_anchor(anchor)
                    {
                        // D7: leave the batch installed, withhold selection,
                        // repark the cursor on the active frame.
                        self.reposition_cursor_to(active_identity);
                        return InstallSelection::DeferredLivePartial {
                            anchor: selected_identity.clone(),
                        };
                    }
                    if display_is_blank {
                        // D5b: selecting into a blank display force-exits
                        // browse mode (Primary-only escape; unreachable
                        // with browsing=true when the flag is false).
                        self.cursor.browsing = false;
                    }
                    self.cursor.index = anchor_index;
                    InstallSelection::SelectedAnchor {
                        index: anchor_index,
                    }
                } else {
                    self.backfill_selection(active_identity)
                }
            }
            SelectionPolicy::Backfill => self.backfill_selection(active_identity),
            SelectionPolicy::KeepCursor => InstallSelection::CursorUnchanged,
        }
    }

    /// Census D5c (engine-common): keep the DISPLAYED frame under the
    /// cursor after indices shift; if it is gone, leave the cursor where
    /// the trim clamped it.
    fn backfill_selection(&mut self, active_identity: Option<&FrameIdentity>) -> InstallSelection {
        if let Some(index) = active_identity.and_then(|identity| {
            self.history
                .iter()
                .position(|frame| frame.identity == *identity)
        }) {
            self.cursor.index = index;
            InstallSelection::Backfilled { index }
        } else {
            InstallSelection::CursorUnchanged
        }
    }

    fn reposition_cursor_to(&mut self, active_identity: Option<&FrameIdentity>) {
        if let Some(index) = active_identity.and_then(|identity| {
            self.history
                .iter()
                .position(|frame| frame.identity == *identity)
        }) {
            self.cursor.index = index;
        }
    }

    /// The 3-tier upsert (census D2, consciously normalized from the
    /// legacy P1/P2 bodies — the polled path's unconditional replace is
    /// covered by rule (c) because a poll's `poll://{name}` path changes
    /// per delivered file; see the differential fixture below):
    /// (a) a live-partial refetch with MORE radials replaces;
    /// (b) same path + same status refresh `timings`/`source_label` only;
    /// (c) replace only at strictly higher status priority, or equal
    ///     priority from a different path.
    fn upsert_history_frame(&mut self, decoded: DecodedLoad) {
        let frame = frame_history_entry_from_decoded(decoded);
        let identity = frame.identity.clone();
        if let Some(existing) = self
            .history
            .iter_mut()
            .find(|candidate| candidate.identity == identity)
        {
            if live_partial_frame_has_new_data(&frame, existing) {
                *existing = frame;
            } else if frame.path == existing.path && frame.status == existing.status {
                existing.timings = frame.timings;
                existing.source_label = frame.source_label;
            } else if frame_status_priority(frame.status) > frame_status_priority(existing.status)
                || (frame_status_priority(frame.status) == frame_status_priority(existing.status)
                    && frame.path != existing.path)
            {
                *existing = frame;
            }
        } else {
            self.history.push(frame);
        }
    }

    /// Trim to limits (census D8): normalize + write back the frame limit
    /// (legacy `trim_frame_history` semantics), drop oldest-first, then
    /// enforce the byte budget the same way — never dropping the last
    /// remaining frame — and clamp the cursor into range.
    pub(super) fn trim_history_to_limits(&mut self) {
        self.limits.frame_limit = HistoryLimits::normalized_frame_limit(self.limits.frame_limit);
        while self.history.len() > self.limits.frame_limit {
            self.history.remove(0);
        }
        if let Some(budget) = self.limits.byte_budget {
            // Recomputed per drop: trims past the budget are rare and the
            // estimate is O(cuts × moments), not O(gates).
            while self.history.len() > 1
                && self
                    .history
                    .iter()
                    .map(estimated_entry_bytes)
                    .sum::<usize>()
                    > budget
            {
                self.history.remove(0);
            }
        }
        self.cursor.index = self.cursor.index.min(self.history.len().saturating_sub(1));
    }

    /// Re-apply a changed frame limit outside an install (census D8, the
    /// shared-limits surface): the caller passes the limit — for panes,
    /// the PRIMARY's — and the engine normalizes its own copy, trims
    /// oldest-first (frames then bytes), and clamps the cursor. This is
    /// the legacy trim-on-limit-change behavior for secondary views.
    pub fn set_frame_limit(&mut self, frame_limit: usize) {
        self.limits.frame_limit = frame_limit;
        self.trim_history_to_limits();
    }

    /// Timeline-sync selection (spec §3, engine-common; panes are the first
    /// consumer, Phase 4d): move the cursor to the NEWEST frame at-or-before
    /// `t`. Histories are ascending by identity (census Appendix B) and a
    /// cross-site guard keeps them single-site, so "newest at-or-before" is
    /// the partition point on scan time. Returns the selected index; `None`
    /// (cursor untouched) when the history is empty or every frame is after
    /// `t`. Staleness budgets are caller policy — the overlay coordinated-
    /// loop budget stays in overlays.rs.
    pub fn select_frame_nearest(&mut self, t: chrono::DateTime<chrono::Utc>) -> Option<usize> {
        let index = self
            .history
            .partition_point(|frame| frame.identity.scan_time_utc <= t)
            .checked_sub(1)?;
        self.cursor.index = index;
        Some(index)
    }

    /// Drop every frame and reset the engine-owned loop state (census D4:
    /// history, cursor, playing/browsing, step clock). Role-specific blast
    /// radius — storm caches, loop render cache, product-cut memory —
    /// stays in the caller, keyed on [`InstallOutcome::cross_site_clear`]
    /// or on having called this directly (the overlay replace-all).
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.cursor.index = 0;
        self.cursor.playing = false;
        self.cursor.browsing = false;
        self.cursor.last_step = None;
    }
}

#[cfg(test)]
mod tests {
    use super::super::feed::FeedSource;
    use super::super::{EngineId, EngineRole, LoopEngine};
    use super::*;
    use chrono::{TimeZone, Utc};
    use data_source::sites::SiteRef;
    use radar_core::RadarSite;
    use std::path::PathBuf;

    fn engine() -> LoopEngine {
        LoopEngine::new(
            EngineId(1),
            EngineRole::Overlay,
            FeedSource::Live(SiteRef::Us {
                level2_id: "KEAX".to_owned(),
            }),
        )
    }

    fn decoded(site: &str, minute: u32, path: &str) -> DecodedLoad {
        decoded_with_status(site, minute, path, super::super::FrameStatus::Complete)
    }

    fn decoded_with_status(
        site: &str,
        minute: u32,
        path: &str,
        status: super::super::FrameStatus,
    ) -> DecodedLoad {
        DecodedLoad {
            path: PathBuf::from(path),
            volume: Arc::new(RadarVolume::new(
                RadarSite::new(site),
                Utc.with_ymd_and_hms(2026, 6, 9, 5, minute, 0).unwrap(),
            )),
            timings: super::super::LoadTimings::default(),
            status,
            source_label: format!("test {site}"),
        }
    }

    fn batch(frames: Vec<DecodedLoad>) -> DecodedLoadBatch {
        let selected_index = frames.len().saturating_sub(1);
        DecodedLoadBatch {
            frames,
            selected_index,
        }
    }

    const SELECT_PRIMARY: SelectionPolicy = SelectionPolicy::SelectAnchor {
        blank_display_overrides_browsing: true,
    };
    const SELECT_PANE: SelectionPolicy = SelectionPolicy::SelectAnchor {
        blank_display_overrides_browsing: false,
    };

    /// Census D1: install is upsert+sort+trim — out-of-order delivery
    /// sorts ascending by identity, re-delivered identities never
    /// duplicate, and the trim drops oldest-first.
    #[test]
    fn install_batch_upserts_sorts_and_trims_oldest_first() {
        let mut engine = engine();
        engine.limits.frame_limit = 3;
        let outcome = engine.install_batch(
            batch(vec![
                decoded("KEAX", 30, "c"),
                decoded("KEAX", 10, "a"),
                decoded("KEAX", 20, "b"),
                decoded("KEAX", 10, "a"), // duplicate identity, same path
            ]),
            &SELECT_PRIMARY,
            None,
            |_| false,
        );
        assert_eq!(engine.history.len(), 3);
        let minutes: Vec<u32> = engine
            .history
            .iter()
            .map(|frame| chrono::Timelike::minute(&frame.identity.scan_time_utc))
            .collect();
        assert_eq!(minutes, vec![10, 20, 30], "ascending by identity");
        assert!(outcome.anchor_selected());

        // Trim: limit 3 already holds 3; a newer batch drops the oldest.
        let _ = engine.install_batch(
            batch(vec![decoded("KEAX", 40, "d")]),
            &SELECT_PRIMARY,
            None,
            |_| false,
        );
        assert_eq!(engine.history.len(), 3);
        assert_eq!(
            chrono::Timelike::minute(&engine.history[0].identity.scan_time_utc),
            20,
            "oldest dropped from the front"
        );
    }

    /// Census D1: the legacy overlay replace-all is spelled as an explicit
    /// clear + install_batch and ends in the identical history state.
    #[test]
    fn replace_all_is_an_explicit_clear_then_install() {
        let mut engine = engine();
        let _ = engine.install_batch(
            batch(vec![
                decoded("KEAX", 10, "old-a"),
                decoded("KEAX", 20, "old-b"),
            ]),
            &SELECT_PANE,
            None,
            |_| false,
        );
        engine.clear_history();
        let outcome = engine.install_batch(
            batch(vec![decoded("KEAX", 15, "new-a")]),
            &SELECT_PANE,
            None,
            |_| false,
        );
        assert_eq!(engine.history.len(), 1);
        assert_eq!(engine.history[0].path, PathBuf::from("new-a"));
        assert_eq!(
            outcome.selection,
            InstallSelection::SelectedAnchor { index: 0 }
        );
    }

    /// Census D2 differential fixture: the engine's 3-tier upsert must
    /// keep every current polled caller winning. Replays the pinned
    /// legacy P3 sequence (`polled_volume_same_site_upserts_and_playing_
    /// loop_keeps_its_cursor`): a re-polled identity arriving under a NEW
    /// `poll://` path replaces the stored entry via rule (c) — equal
    /// priority (Complete == Complete), different path.
    #[test]
    fn upsert_differential_polled_repeat_identity_with_new_path_keeps_winning() {
        let mut engine = engine();
        let _ = engine.install_frame(
            decoded("FWLX", 33, "poll://FWLX20260608_0133"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        let repeat = decoded("FWLX", 33, "poll://FWLX20260608_0133b");
        let repeat_volume = Arc::clone(&repeat.volume);
        let outcome =
            engine.install_frame(repeat, &SelectionPolicy::FollowNewestUnlessPlaying, None);
        assert_eq!(
            engine.history.len(),
            1,
            "identity match replaces, never duplicates"
        );
        assert_eq!(
            engine.history[0].path,
            PathBuf::from("poll://FWLX20260608_0133b"),
            "rule (c): equal priority + different path replaces — the polled caller wins"
        );
        assert!(
            Arc::ptr_eq(&engine.history[0].volume, &repeat_volume),
            "the NEW volume Arc is stored"
        );
        assert_eq!(
            outcome.display_install,
            Some(0),
            "polled route always installs the display (D9)"
        );
    }

    /// Census D2, the KNOWN divergence the fixture exposes for the 4e
    /// differential gate: a growing same-NAME file re-poll (dir.list
    /// signature grew, name did not — pinned live behavior in
    /// `dir_list_signatures_keep_growing_same_name_pollable`) re-delivers
    /// the same identity under the SAME `poll://` path. Legacy P3
    /// replaced unconditionally; the normalized rule (b) refreshes
    /// metadata only and keeps the OLD volume Arc in history (the display
    /// still shows the new volume via the D9 unconditional install). This
    /// test pins the ENGINE behavior so any resolution at the 4e gate is
    /// a visible, deliberate change.
    #[test]
    fn same_path_same_status_repoll_refreshes_metadata_only_the_4e_growing_file_gap() {
        let mut engine = engine();
        let _ = engine.install_frame(
            decoded("FWLX", 33, "poll://FWLX20260608_0133"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        let stored_volume = Arc::clone(&engine.history[0].volume);
        let mut regrown = decoded("FWLX", 33, "poll://FWLX20260608_0133");
        regrown.source_label = "polled FWLX (regrown)".to_owned();
        let _ = engine.install_frame(regrown, &SelectionPolicy::FollowNewestUnlessPlaying, None);
        assert_eq!(engine.history.len(), 1);
        assert!(
            Arc::ptr_eq(&engine.history[0].volume, &stored_volume),
            "rule (b) keeps the stored volume — DIVERGES from legacy P3's \
             unconditional replace; resolve at the Phase-4e differential gate"
        );
        assert_eq!(
            engine.history[0].source_label, "polled FWLX (regrown)",
            "rule (b) still refreshes the metadata"
        );
    }

    /// Census D2 rules (a) and (c) status arms: a live-partial refetch
    /// with more radials replaces; a lower-priority frame never replaces a
    /// higher one.
    #[test]
    fn upsert_three_tier_status_ladder_holds() {
        let mut engine = engine();
        let partial =
            decoded_with_status("KEAX", 10, "chunk", super::super::FrameStatus::LivePartial);
        let _ = engine.install_batch(batch(vec![partial.clone()]), &SELECT_PRIMARY, None, |_| {
            false
        });

        // Rule (a): same path, still LivePartial, more radials -> replace.
        let mut grown = partial.clone();
        let mut grown_volume = (*grown.volume).clone();
        grown_volume.metadata.decoded_radial_count = 100;
        grown.volume = Arc::new(grown_volume);
        let grown_arc = Arc::clone(&grown.volume);
        let _ = engine.install_batch(batch(vec![grown]), &SELECT_PRIMARY, None, |_| false);
        assert!(Arc::ptr_eq(&engine.history[0].volume, &grown_arc));

        // Rule (c) downgrade arm: a Preview for the same identity from
        // another path must NOT replace the live-partial.
        let preview = decoded_with_status(
            "KEAX",
            10,
            "preview-path",
            super::super::FrameStatus::Preview,
        );
        let _ = engine.install_batch(batch(vec![preview]), &SELECT_PRIMARY, None, |_| false);
        assert_eq!(
            engine.history[0].status,
            super::super::FrameStatus::LivePartial,
            "lower priority never replaces"
        );

        // Rule (c) upgrade arm: the Complete from a new path replaces.
        let complete = decoded("KEAX", 10, "archive-path");
        let _ = engine.install_batch(batch(vec![complete]), &SELECT_PRIMARY, None, |_| false);
        assert_eq!(
            engine.history[0].status,
            super::super::FrameStatus::Complete
        );
    }

    /// Census D3: the cross-site guard clears BEFORE install and the
    /// outcome carries the greppable-identical legacy diagnostic. The
    /// polled route gains the same diagnostic (decision D3).
    #[test]
    fn cross_site_clear_reports_the_legacy_diagnostic_wording() {
        let mut engine = engine();
        let _ = engine.install_batch(
            batch(vec![
                decoded("KEAX", 10, "a"),
                decoded("KEAX", 20, "b"),
                decoded("KEAX", 30, "c"),
            ]),
            &SELECT_PRIMARY,
            None,
            |_| false,
        );
        let outcome = engine.install_frame(
            decoded("KTLX", 40, "poll://KTLX-tick"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        let clear = outcome.cross_site_clear.expect("guard must fire");
        assert_eq!(
            clear.diagnostic, "history reset (site change to KTLX, had 3 frames)",
            "wording must stay greppable-identical to the legacy string"
        );
        assert_eq!(clear.dropped_frames, 3);
        assert_eq!(engine.history.len(), 1);
        assert_eq!(engine.history[0].identity.site_id, "KTLX");
    }

    /// Census D3: the guard also fires on the ACTIVE volume's site, even
    /// with an empty history (the displayed frame is another site's).
    #[test]
    fn cross_site_clear_keys_on_the_active_volume_too() {
        let mut engine = engine();
        let active = Arc::new(RadarVolume::new(
            RadarSite::new("KEAX"),
            Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap(),
        ));
        let outcome = engine.install_batch(
            batch(vec![decoded("KTLX", 10, "a")]),
            &SELECT_PRIMARY,
            Some(&active),
            |_| false,
        );
        assert!(outcome.cross_site_clear.is_some());
    }

    /// Census D5a + D9 (owner: LIVE WINS), the un-regressable pinning
    /// test: while playing, the polled route holds the cursor but still
    /// reports a display install; while browsing (not playing), the
    /// follow is NOT blocked.
    #[test]
    fn follow_newest_holds_cursor_while_playing_but_still_installs_display() {
        let mut engine = engine();
        let _ = engine.install_frame(
            decoded("KEAX", 10, "poll://t10"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        engine.cursor.playing = true;
        engine.cursor.index = 0;
        let outcome = engine.install_frame(
            decoded("KEAX", 20, "poll://t20"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        assert_eq!(engine.cursor.index, 0, "playing loop keeps its cursor");
        assert_eq!(
            outcome.selection,
            InstallSelection::HeldWhilePlaying { newest_index: 1 }
        );
        assert_eq!(
            outcome.display_install,
            Some(1),
            "LIVE WINS: display still installs while the cursor holds"
        );

        // D5a: browsing does NOT block the follow — deliberate.
        engine.cursor.playing = false;
        engine.cursor.browsing = true;
        let outcome = engine.install_frame(
            decoded("KEAX", 30, "poll://t30"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        assert_eq!(engine.cursor.index, 2, "browsing is deliberately ignored");
        assert_eq!(
            outcome.selection,
            InstallSelection::FollowedNewest { index: 2 }
        );
    }

    /// Census D5: SelectAnchor's playing/browsing guards block selection
    /// and fall into the engine-common backfill reposition (D5c).
    #[test]
    fn select_anchor_respects_playing_and_browsing_guards_and_backfills() {
        let mut engine = engine();
        let first = decoded("KEAX", 10, "a");
        let active = Arc::clone(&first.volume);
        let _ = engine.install_batch(batch(vec![first]), &SELECT_PRIMARY, None, |_| false);

        engine.cursor.playing = true;
        let outcome = engine.install_batch(
            batch(vec![
                decoded("KEAX", 5, "older"),
                decoded("KEAX", 20, "newer"),
            ]),
            &SELECT_PRIMARY,
            Some(&active),
            |_| false,
        );
        assert!(
            !outcome.anchor_selected(),
            "playing blocks the select (D16 false arm)"
        );
        assert_eq!(
            outcome.selection,
            InstallSelection::Backfilled { index: 1 },
            "cursor repositioned to the displayed frame after indices shifted"
        );
        assert_eq!(engine.cursor.index, 1);

        engine.cursor.playing = false;
        engine.cursor.browsing = true;
        let outcome = engine.install_batch(
            batch(vec![decoded("KEAX", 25, "newest")]),
            &SELECT_PANE,
            Some(&active),
            |_| false,
        );
        assert!(!outcome.anchor_selected(), "browsing blocks the select");
        assert_eq!(outcome.selection, InstallSelection::Backfilled { index: 1 });
    }

    /// Census D5b: the blank-display escape is Primary-only — with the
    /// flag it selects (and force-exits browse mode); without it the
    /// browsing guard holds even on a blank display.
    #[test]
    fn blank_display_escape_selects_and_exits_browse_mode_primary_only() {
        let mut engine = engine();
        engine.cursor.browsing = true;
        let outcome = engine.install_batch(
            batch(vec![decoded("KEAX", 10, "a")]),
            &SELECT_PRIMARY,
            None, // blank display
            |_| false,
        );
        assert!(outcome.anchor_selected());
        assert!(
            !engine.cursor.browsing,
            "selecting into a blank display exits browse mode"
        );

        let mut pane_engine = engine_for_pane();
        pane_engine.cursor.browsing = true;
        let outcome = pane_engine.install_batch(
            batch(vec![decoded("KEAX", 10, "a")]),
            &SELECT_PANE,
            None,
            |_| false,
        );
        assert!(
            !outcome.anchor_selected(),
            "panes have no blank-display escape (census D5b)"
        );
        assert!(pane_engine.cursor.browsing, "pane browse state survives");
    }

    fn engine_for_pane() -> LoopEngine {
        LoopEngine::new(
            EngineId(2),
            EngineRole::Pane { slot: 0 },
            FeedSource::Live(SiteRef::Us {
                level2_id: "KEAX".to_owned(),
            }),
        )
    }

    /// Census D7: the deferral hook withholds selection, leaves the batch
    /// installed, and reparks the cursor on the active frame; the outcome
    /// carries the anchor identity for the caller's "Waiting for {product}
    /// in {site}" status.
    #[test]
    fn deferred_live_partial_keeps_batch_installed_and_reparks_cursor() {
        let mut engine = engine();
        let first = decoded("KEAX", 10, "a");
        let active = Arc::clone(&first.volume);
        let _ = engine.install_batch(batch(vec![first]), &SELECT_PRIMARY, None, |_| false);

        let outcome = engine.install_batch(
            batch(vec![decoded_with_status(
                "KEAX",
                20,
                "chunk",
                super::super::FrameStatus::LivePartial,
            )]),
            &SELECT_PRIMARY,
            Some(&active),
            |anchor| anchor.status == super::super::FrameStatus::LivePartial,
        );
        assert_eq!(
            engine.history.len(),
            2,
            "deferral leaves the batch installed"
        );
        assert!(
            !outcome.anchor_selected(),
            "D16: deferral returns the false arm"
        );
        match &outcome.selection {
            InstallSelection::DeferredLivePartial { anchor } => {
                assert_eq!(anchor.site_id, "KEAX");
            }
            other => panic!("expected deferral, got {other:?}"),
        }
        assert_eq!(
            engine.cursor.index, 0,
            "cursor reparked on the active frame"
        );
    }

    /// Census D8: grow_to_fit intent raises the frame limit to the batch
    /// size (capped), replacing the ORD path-sniffing and the local
    /// autoplay one-shot; without the intent the limit trims as usual.
    #[test]
    fn grow_to_fit_raises_frame_limit_to_batch_len_capped() {
        let mut engine = engine();
        engine.limits.frame_limit = 7;
        engine.limits.grow_to_fit = true;
        let frames: Vec<DecodedLoad> = (0..10)
            .map(|i| decoded("KEAX", i as u32, &format!("ord-archive:{i}")))
            .collect();
        let outcome = engine.install_batch(batch(frames), &SELECT_PRIMARY, None, |_| false);
        assert_eq!(engine.limits.frame_limit, 10, "limit grew to fit the batch");
        assert_eq!(engine.history.len(), 10, "all frames land");
        assert_eq!(
            outcome.selection,
            InstallSelection::SelectedAnchor { index: 9 },
            "anchor (last frame) selected — mirrors ord_archive_batch_grows_history_limit_to_fetch_count"
        );

        // Without the intent, the same delivery trims to the limit.
        let mut plain = engine_for_pane();
        plain.limits.frame_limit = 7;
        let frames: Vec<DecodedLoad> = (0..10)
            .map(|i| decoded("KEAX", i as u32, &format!("plain:{i}")))
            .collect();
        let _ = plain.install_batch(batch(frames), &SELECT_PANE, None, |_| false);
        assert_eq!(plain.history.len(), 7);
        assert_eq!(plain.limits.frame_limit, 7);
    }

    /// The byte budget trims oldest-first after the frame trim and never
    /// drops the last remaining frame, and the cursor clamps into range.
    #[test]
    fn byte_budget_trims_oldest_first_and_never_drops_the_last_frame() {
        let mut engine = engine();
        // Budget below one entry's size: the last frame must survive.
        engine.limits.byte_budget = Some(1);
        let _ = engine.install_batch(
            batch(vec![
                decoded("KEAX", 10, "a"),
                decoded("KEAX", 20, "b"),
                decoded("KEAX", 30, "c"),
            ]),
            &SELECT_PANE,
            None,
            |_| false,
        );
        assert_eq!(
            engine.history.len(),
            1,
            "budget trims to (but never past) the newest frame"
        );
        assert_eq!(
            chrono::Timelike::minute(&engine.history[0].identity.scan_time_utc),
            30,
            "oldest dropped first"
        );
        assert_eq!(engine.cursor.index, 0, "cursor clamped into range");

        // A generous budget keeps everything.
        let mut roomy = engine_for_pane();
        roomy.limits.byte_budget = Some(usize::MAX);
        let _ = roomy.install_batch(
            batch(vec![decoded("KEAX", 10, "a"), decoded("KEAX", 20, "b")]),
            &SELECT_PANE,
            None,
            |_| false,
        );
        assert_eq!(roomy.history.len(), 2);
    }

    /// Census D12: every install route bumps the generation (upsert,
    /// refresh-only upsert, cross-site clear, trim) — the debug assertion
    /// harness inside install_batch enforces it in debug builds; this test
    /// asserts it observably.
    #[test]
    fn every_install_route_bumps_the_generation() {
        let mut engine = engine();
        let mut last = engine.history.generation();
        let mut expect_bump = |engine: &LoopEngine, route: &str| {
            assert_ne!(engine.history.generation(), last, "{route} must bump");
            last = engine.history.generation();
        };

        let _ = engine.install_frame(
            decoded("KEAX", 10, "poll://a"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        expect_bump(&engine, "fresh install");
        let _ = engine.install_frame(
            decoded("KEAX", 10, "poll://a"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        expect_bump(&engine, "refresh-only upsert (rule b)");
        let _ = engine.install_frame(
            decoded("KTLX", 20, "poll://b"),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            None,
        );
        expect_bump(&engine, "cross-site clear + install");
        engine.clear_history();
        expect_bump(&engine, "explicit clear");
    }

    /// Census D16: the InstallOutcome-to-legacy-bool mapping — true only
    /// for SelectedAnchor.
    #[test]
    fn install_outcome_bool_mapping_matches_the_legacy_returns() {
        let selected = InstallOutcome {
            selection: InstallSelection::SelectedAnchor { index: 0 },
            cross_site_clear: None,
            display_install: None,
        };
        assert!(selected.anchor_selected());
        for selection in [
            InstallSelection::NoFrames,
            InstallSelection::FollowedNewest { index: 0 },
            InstallSelection::HeldWhilePlaying { newest_index: 0 },
            InstallSelection::Backfilled { index: 0 },
            InstallSelection::DeferredLivePartial {
                anchor: FrameIdentity {
                    site_id: "KEAX".to_owned(),
                    scan_time_utc: Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap(),
                },
            },
            InstallSelection::CursorUnchanged,
        ] {
            let outcome = InstallOutcome {
                selection,
                cross_site_clear: None,
                display_install: None,
            };
            assert!(!outcome.anchor_selected());
        }
    }

    /// The Backfill policy (legacy History batches, select_frame=false)
    /// installs without selecting and repositions onto the displayed
    /// frame; with no active frame the cursor stays put.
    #[test]
    fn backfill_policy_installs_without_selecting() {
        let mut engine = engine();
        let first = decoded("KEAX", 20, "a");
        let active = Arc::clone(&first.volume);
        let _ = engine.install_batch(batch(vec![first]), &SELECT_PRIMARY, None, |_| false);
        let outcome = engine.install_batch(
            batch(vec![decoded("KEAX", 10, "older")]),
            &SelectionPolicy::Backfill,
            Some(&active),
            |_| false,
        );
        assert_eq!(engine.history.len(), 2);
        assert_eq!(
            outcome.selection,
            InstallSelection::Backfilled { index: 1 },
            "displayed frame followed to its shifted index"
        );
        assert_eq!(engine.cursor.index, 1);

        let outcome = engine.install_batch(
            batch(vec![decoded("KEAX", 5, "oldest")]),
            &SelectionPolicy::Backfill,
            None,
            |_| false,
        );
        assert_eq!(outcome.selection, InstallSelection::CursorUnchanged);
    }

    /// select_frame_nearest is at-or-before semantics on the ascending
    /// history: exact hit selects it, between-frames selects the older,
    /// before-all/no-frames leaves the cursor untouched and returns None.
    #[test]
    fn select_frame_nearest_picks_newest_at_or_before() {
        let mut engine = engine();
        let _ = engine.install_batch(
            batch(vec![
                decoded("KEAX", 10, "a"),
                decoded("KEAX", 20, "b"),
                decoded("KEAX", 30, "c"),
            ]),
            &SELECT_PRIMARY,
            None,
            |_| false,
        );
        let t = |minute: u32| Utc.with_ymd_and_hms(2026, 6, 9, 5, minute, 0).unwrap();

        assert_eq!(engine.select_frame_nearest(t(20)), Some(1), "exact hit");
        assert_eq!(engine.cursor.index, 1);
        assert_eq!(
            engine.select_frame_nearest(t(25)),
            Some(1),
            "between frames selects the older (at-or-before)"
        );
        assert_eq!(engine.select_frame_nearest(t(45)), Some(2), "after all");
        assert_eq!(engine.cursor.index, 2);
        assert_eq!(
            engine.select_frame_nearest(t(5)),
            None,
            "before all frames: nothing at-or-before"
        );
        assert_eq!(engine.cursor.index, 2, "a miss leaves the cursor alone");

        let mut empty = engine_for_pane();
        assert_eq!(empty.select_frame_nearest(t(20)), None);
        assert_eq!(empty.cursor.index, 0);
    }

    /// KeepCursor (overlay timeline-sync) never moves the cursor on
    /// install; only the trim clamp applies.
    #[test]
    fn keep_cursor_policy_leaves_the_cursor_alone() {
        let mut engine = engine();
        let _ = engine.install_batch(
            batch(vec![decoded("KEAX", 10, "a"), decoded("KEAX", 20, "b")]),
            &SELECT_PANE,
            None,
            |_| false,
        );
        engine.cursor.index = 0;
        let outcome = engine.install_batch(
            batch(vec![decoded("KEAX", 30, "c")]),
            &SelectionPolicy::KeepCursor,
            None,
            |_| false,
        );
        assert_eq!(outcome.selection, InstallSelection::CursorUnchanged);
        assert_eq!(engine.cursor.index, 0);
    }
}
