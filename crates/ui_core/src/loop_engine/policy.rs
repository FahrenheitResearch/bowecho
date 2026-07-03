//! Declared per-role policy: what the census (docs/v029-phase4-behavior-census.md)
//! decided is a GENUINE role difference lives here as data; everything else
//! was consciously normalized into the shared install path.

use radar_core::RadarVolume;

use super::frame_history::FrameHistoryEntry;

/// Hard ceiling for any frame history — deployment folders legitimately
/// load 100+ volumes. Moved from app_ui (which re-imports it) so the
/// engine's grow/trim math and the legacy paths can never disagree on the
/// ceiling.
pub const MAX_HISTORY_FRAME_LIMIT: usize = 2000;

/// Default frame limit when a stored limit is 0/absent. Moved from app_ui
/// (which re-imports it).
pub const DEFAULT_HISTORY_FRAME_LIMIT: usize = 7;

/// Smallest selectable frame limit. Invariant: equals app_ui's
/// `HISTORY_SIZE_OPTIONS[0]` (the smallest combo choice) — the legacy
/// `normalized_history_limit` clamps to that value and the engine's
/// [`HistoryLimits::normalized_frame_limit`] must clamp identically for
/// the Phase-4e differential suite to hold.
pub const MIN_HISTORY_FRAME_LIMIT: usize = 3;

/// Per-engine byte budget for the OVERLAY role (census D8 + spec §3: frame
/// histories are the dominant unbudgeted store). Overlays today hold either
/// one live frame or an upstream-capped archive window, so this budget is a
/// guard rail against pathological archive-window growth, not a working
/// limit — sized so a full 40-frame super-res US loop (~50-80 MB/volume
/// decoded) fits. The adoption stage may tune it against telemetry.
pub const OVERLAY_HISTORY_BYTE_BUDGET: usize = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Per-engine byte budget for the PANE role (Phase 4d wiring of census D8).
/// A pane's working set mirrors the primary's frame limit (the shared-limits
/// coupling surfaced in `HistoryLimits::pane`), so like the overlay budget
/// this is a guard rail against pathological growth, not a working limit —
/// the same "full 40-frame super-res loop must fit" sizing. The primary
/// budget is a separate constant, not a fraction of this one, because the
/// two roles' legitimate working sets differ (see
/// [`PRIMARY_HISTORY_BYTE_BUDGET`]).
pub const PANE_HISTORY_BYTE_BUDGET: usize = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Per-engine byte budget for the PRIMARY role (Phase 4e wiring of census
/// D8). The primary legitimately holds the app's biggest loops — deployment
/// folders load 100+ volumes and the player exposes the 2000-frame ceiling —
/// so its guard rail is sized for a ~100-frame super-res loop (~50-80
/// MB/volume decoded ≈ 5-8 GB), four times the overlay/pane rail. Like the
/// other budgets this stops pathological growth, not normal use: the user's
/// frame limit governs first on every real machine. Wired at stage (i) but
/// INERT until the stage-(ii) unify routes the primary's trims through
/// [`LoopEngine::trim_history_to_limits`](super::LoopEngine) — the legacy
/// `trim_frame_history` body never reads it.
pub const PRIMARY_HISTORY_BYTE_BUDGET: usize = 8 * 1024 * 1024 * 1024; // 8 GiB

/// Frame-count and byte limits for one engine's history (census D8).
///
/// Invariants:
/// - `frame_limit` is normalized (clamped, never snapped to presets) by
///   every trim — exactly the legacy `normalized_history_limit` behavior,
///   including the write-back (the legacy primary trim persists the
///   normalized value; panes read the primary's limit and do NOT write
///   back — that coupling surfaces as a shared limits object in `PaneView`
///   construction at Phase 4d, never a silent copy).
/// - `byte_budget` trims oldest-first after the frame trim and never drops
///   the last remaining frame (a single oversized volume must still
///   display).
/// - `grow_to_fit` is the D8 INTENT flag: callers delivering an archive
///   loop state it here, and multi-frame batches raise `frame_limit` to
///   fit (capped at [`MAX_HISTORY_FRAME_LIMIT`]). This replaces the legacy
///   `decoded_load_is_ord_archive_frame` path-prefix sniffing and the
///   one-shot `pending_local_autoplay` grow — encode intent, not path
///   prefixes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryLimits {
    pub frame_limit: usize,
    pub byte_budget: Option<usize>,
    pub grow_to_fit: bool,
}

impl HistoryLimits {
    /// Limits for the primary view (Phase 4e): the user's frame limit plus
    /// the primary guard-rail byte budget
    /// ([`PRIMARY_HISTORY_BYTE_BUDGET`] — inert until the stage-(ii)
    /// unify routes primary trims through the engine).
    pub fn primary(frame_limit: usize) -> Self {
        Self {
            frame_limit,
            byte_budget: Some(PRIMARY_HISTORY_BYTE_BUDGET),
            grow_to_fit: false,
        }
    }

    /// Limits for an extra pane (Phase 4d, census D8): `frame_limit` is the
    /// PRIMARY's limit, passed explicitly at `PaneView` construction and
    /// re-passed at every pane install — the pane FOLLOWS the primary's
    /// frame limit (the legacy `trim_extra_pane_history` coupling, now a
    /// visible parameter instead of a silent field read) and never writes
    /// back to it (each pane normalizes its own copy). The byte budget is
    /// pane-local ([`PANE_HISTORY_BYTE_BUDGET`]).
    pub fn pane(frame_limit: usize) -> Self {
        Self {
            frame_limit,
            byte_budget: Some(PANE_HISTORY_BYTE_BUDGET),
            grow_to_fit: false,
        }
    }

    /// Limits for a radar overlay layer (Phase 4c): overlays historically
    /// had NO trim (census D8 row P4) — the ceiling stands in for "no
    /// limit" and the byte budget is the new guard rail that lands with
    /// the engine (spec §3).
    pub fn overlay() -> Self {
        Self {
            frame_limit: MAX_HISTORY_FRAME_LIMIT,
            byte_budget: Some(OVERLAY_HISTORY_BYTE_BUDGET),
            grow_to_fit: false,
        }
    }

    /// Clamp, don't snap to presets: deployment loads raise the limit past
    /// the combo options, and trims must not collapse it back to the
    /// default. Identical to the legacy `normalized_history_limit`
    /// (0 maps to the default; everything else clamps into
    /// `[MIN, MAX]`).
    pub fn normalized_frame_limit(limit: usize) -> usize {
        if limit == 0 {
            DEFAULT_HISTORY_FRAME_LIMIT
        } else {
            limit.clamp(MIN_HISTORY_FRAME_LIMIT, MAX_HISTORY_FRAME_LIMIT)
        }
    }
}

/// Cursor/selection rule applied after an install (census D5 — THE
/// SelectionPolicy payload). The fine print below is deliberate, decided
/// behavior; do not "fix" it during adoption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionPolicy {
    /// The polled/live route (legacy `install_polled_volume`): the cursor
    /// follows the installed frame UNLESS the loop is playing.
    ///
    /// Fine print (census D5a): this DELIBERATELY ignores `browsing` —
    /// browsing a live polled feed and getting yanked to newest on the
    /// next tick is current, owner-visible behavior and stays. The display
    /// installs the newest volume even while playing (census D9, owner
    /// decision: LIVE WINS) — the engine surfaces that as
    /// `InstallOutcome::display_install`, the caller does the write.
    FollowNewestUnlessPlaying,
    /// The batch-load route (legacy `install_decoded_load_batch` /
    /// `install_extra_pane_decoded_load_batch` with `select_loaded_frame`):
    /// select the batch anchor once at the END of install, only when
    /// `!playing && !browsing`.
    ///
    /// Fine print (census D5b): `blank_display_overrides_browsing` is the
    /// Primary-role-only escape — a blank display lets the anchor select
    /// (and force-exits browse mode) even while browsing. Panes pass
    /// `false`; do not normalize it into panes without an owner call.
    /// Select-time side effects (timeline sync, camera follow,
    /// `preserve_active_frame_cut`, first-low-sweep) stay in the CALLER
    /// per census D6 — the engine only reports `SelectedAnchor`.
    SelectAnchor {
        blank_display_overrides_browsing: bool,
    },
    /// The backfill route (legacy History batches with
    /// `select_frame=false`): install without selecting, then reposition
    /// the cursor so the DISPLAYED frame stays under it after indices
    /// shift. The reposition is engine-common (census D5c), not a
    /// per-role behavior.
    Backfill,
    /// The overlay timeline-sync route: hold the raw cursor; the timeline
    /// sync re-selects by time afterwards (`select_frame_nearest`, an
    /// adoption-stage method). Trims still clamp the cursor into range.
    KeepCursor,
}

/// Estimated resident bytes for one history entry. An ESTIMATE by design:
/// counts the dominant terms (moment storage, radial metadata, container
/// structs) and ignores allocator slack — good enough for a budget whose
/// job is stopping unbounded growth, cheap enough to run on every trim
/// (no per-gate work, only per-cut/per-moment).
pub fn estimated_entry_bytes(entry: &FrameHistoryEntry) -> usize {
    std::mem::size_of::<FrameHistoryEntry>()
        + entry.path.as_os_str().len()
        + entry.source_label.len()
        + estimated_volume_bytes(&entry.volume)
}

/// Estimated resident bytes for a decoded volume (see
/// [`estimated_entry_bytes`] for the estimate contract).
pub fn estimated_volume_bytes(volume: &RadarVolume) -> usize {
    let mut bytes = std::mem::size_of::<RadarVolume>();
    for cut in &volume.cuts {
        bytes += std::mem::size_of_val(cut);
        bytes += cut.radials.len() * std::mem::size_of::<radar_core::Radial>();
        for grid in cut.moments.values() {
            bytes += std::mem::size_of_val(grid);
            bytes += grid.radial_indices.len() * std::mem::size_of::<usize>();
            bytes += grid.storage.len() * usize::from(grid.storage.word_size_bits() / 8);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentGrid, MomentType, RadarSite};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Pins the clamp semantics shared with the legacy
    /// `normalized_history_limit`: 0 -> default, low clamps to the
    /// smallest combo option, high clamps to the ceiling, in-range values
    /// pass through un-snapped.
    #[test]
    fn normalized_frame_limit_matches_the_legacy_clamp() {
        assert_eq!(
            HistoryLimits::normalized_frame_limit(0),
            DEFAULT_HISTORY_FRAME_LIMIT
        );
        assert_eq!(
            HistoryLimits::normalized_frame_limit(1),
            MIN_HISTORY_FRAME_LIMIT
        );
        assert_eq!(HistoryLimits::normalized_frame_limit(42), 42);
        assert_eq!(
            HistoryLimits::normalized_frame_limit(5000),
            MAX_HISTORY_FRAME_LIMIT
        );
    }

    /// The role constructors pin where the byte budget is WIRED today:
    /// overlays got one at Phase 4c, panes at 4d, the primary at 4e stage
    /// (i) — sized 4× the secondary rails for its bigger legitimate loops.
    #[test]
    fn every_role_gets_a_byte_budget_primary_is_the_largest() {
        assert_eq!(
            HistoryLimits::overlay().byte_budget,
            Some(OVERLAY_HISTORY_BYTE_BUDGET)
        );
        assert_eq!(
            HistoryLimits::primary(7).byte_budget,
            Some(PRIMARY_HISTORY_BYTE_BUDGET)
        );
        // Compare the WIRED budgets (through the constructors, so the
        // ordering claim tracks what engines actually get, and clippy's
        // assertions_on_constants stays quiet).
        let primary_budget = HistoryLimits::primary(7)
            .byte_budget
            .expect("primary budget wired at 4e stage (i)");
        assert!(
            primary_budget > HistoryLimits::pane(7).byte_budget.expect("pane budget")
                && primary_budget
                    > HistoryLimits::overlay()
                        .byte_budget
                        .expect("overlay budget"),
            "the primary's legitimate working set is the largest"
        );
        assert_eq!(
            HistoryLimits::pane(7).byte_budget,
            Some(PANE_HISTORY_BYTE_BUDGET)
        );
        assert_eq!(
            HistoryLimits::pane(7).frame_limit,
            7,
            "the pane frame limit IS the primary's, passed explicitly (census D8)"
        );
        assert_eq!(
            HistoryLimits::overlay().frame_limit,
            MAX_HISTORY_FRAME_LIMIT,
            "overlay frame limit stands in for the legacy no-trim behavior"
        );
    }

    /// The byte estimate must scale with moment storage — the dominant
    /// term — so the budget actually tracks decoded volume size.
    #[test]
    fn estimated_volume_bytes_scales_with_moment_storage() {
        let mut small = radar_core::RadarVolume::new(RadarSite::new("KEAX"), chrono::Utc::now());
        let mut cut = radar_core::ElevationCut::new(0.5, Some(1));
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 250,
            gate_count: 100,
        };
        cut.moments.insert(
            MomentType::Reflectivity,
            MomentGrid::new_u8(
                MomentType::Reflectivity,
                gate_range.clone(),
                2.0,
                66.0,
                Some(0),
                Some(1),
            ),
        );
        small.cuts.push(cut.clone());
        let small_bytes = estimated_volume_bytes(&small);

        let mut large = small.clone();
        if let Some(grid) = large.cuts[0].moments.get_mut(&MomentType::Reflectivity)
            && let radar_core::MomentStorage::U8(values) = &mut grid.storage
        {
            let grown = values.len() + 1_000_000;
            values.resize(grown, 0u8);
        }
        let large_bytes = estimated_volume_bytes(&large);
        assert!(
            large_bytes >= small_bytes + 1_000_000,
            "storage bytes must dominate: {small_bytes} -> {large_bytes}"
        );

        let entry = crate::loop_engine::FrameHistoryEntry {
            identity: crate::loop_engine::frame_identity_for_volume(&large),
            path: PathBuf::from("bytes-test"),
            volume: Arc::new(large),
            timings: None,
            status: crate::loop_engine::FrameStatus::Complete,
            source_label: "estimate".to_owned(),
        };
        assert!(estimated_entry_bytes(&entry) >= large_bytes);
    }
}
