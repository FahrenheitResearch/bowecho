//! # LoopEngine — the ONE "site + frame history + playback + liveness" machine
//!
//! v0.29 Phase 4c engine core (docs/v029-engine-spec.md §3, landing `pub`
//! here per §12a CP-1). The primary view, each extra pane, and each radar
//! overlay become three INSTANCES of this engine differing only in
//! declared policy — never divergent code. Adoption order (spec §7):
//! overlays first (4c), panes (4d), primary (4e); until a view adopts,
//! its legacy main.rs plumbing is the live path and this engine is
//! inert-but-tested.
//!
//! Behavior authority: every rule in this module cites a Decision row of
//! docs/v029-phase4-behavior-census.md (D1-D16, all FILLED and BINDING).
//! The module is deliberately split small (spec §12b): one file per
//! concern, boring explicit code, so a maintainer holding ONE file in a
//! small context window can follow it.
//!
//! | file | owns |
//! |---|---|
//! | `frame_history.rs` | entry types + the generation-stamped `FrameHistory` (VERBATIM from main.rs) + shared pure helpers |
//! | `feed.rs` | `FeedSource`/`ArchiveWindow`, `switch_policy`, settings shims |
//! | `policy.rs` | `SelectionPolicy` (census D5 fine print), `HistoryLimits` (D8), byte estimates |
//! | `install.rs` | `install_frame`/`install_batch` (D1-D3, D6-D9, D12, D16), trims, `clear_history` |
//! | `liveness.rs` | `Liveness`, cadence-aware stale floor, `poll_cadence` |

mod feed;
mod frame_history;
mod install;
mod liveness;
mod policy;

pub use feed::{ArchiveWindow, FeedSource, SwitchAction, switch_policy};
pub use frame_history::{
    DecodedLoad, DecodedLoadBatch, FrameHistory, FrameHistoryEntry, FrameIdentity, FrameStatus,
    LoadTimings, frame_history_entry_from_decoded, frame_identity_for_volume,
    frame_status_priority, history_contains_other_site, live_partial_frame_has_new_data,
    volume_total_radials,
};
pub use install::{CrossSiteClear, InstallOutcome, InstallSelection};
pub use liveness::{
    CUSTOM_URL_POLL_SECONDS, INTL_POLL_SECONDS, INTL_STALE_CHIP_FLOOR_SECONDS, Liveness,
    OVERLAY_REALTIME_LEVEL2_REFRESH_SECONDS, PRIMARY_LIVE_CHUNK_POLL_SECONDS, stale_floor_seconds,
};
pub use policy::{
    DEFAULT_HISTORY_FRAME_LIMIT, HistoryLimits, MAX_HISTORY_FRAME_LIMIT, MIN_HISTORY_FRAME_LIMIT,
    OVERLAY_HISTORY_BYTE_BUDGET, SelectionPolicy, estimated_entry_bytes, estimated_volume_bytes,
};

use std::time::Instant;

/// Stable engine identity; also the render-lane payload (an overlay
/// engine's id becomes `LaneId::Overlay(id.0)` at adoption). Invariant:
/// never reused within a session — the owner (ViewerApp) allocates
/// monotonically.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct EngineId(pub u64);

/// Which view this engine drives. Role picks DEFAULT policy (limits,
/// cadence); it must never grow behavior branches beyond what the census
/// recorded as a genuine role difference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineRole {
    Primary,
    Pane { slot: u8 },
    Overlay,
}

/// Playback cursor state. Invariants: `index` is kept in
/// `0..history.len()` (or 0 when empty) by install/trim clamps;
/// `browsing` means the user is hand-stepping frames (blocks anchor
/// selection per census D5 — but deliberately NOT the polled follow);
/// `last_step` is the loop stepper's clock, reset on clear.
#[derive(Clone, Debug, Default)]
pub struct FrameCursor {
    pub index: usize,
    pub playing: bool,
    pub browsing: bool,
    pub last_step: Option<Instant>,
}

/// Live-refresh state for a feed. Invariants: `enabled` is the pause
/// switch (a paused Live feed reads ARCHIVE, census/R8); `dedupe_key` is
/// the fetch-side no-change marker (legacy `poll_last_file` /
/// `IntlOverlayFeed.last_identity`) — its SCHEME is a provider fact owned
/// by the feed adapter (census D10), the engine only stores/clears it;
/// `last_refresh` feeds cadence checks at adoption.
#[derive(Clone, Debug, Default)]
pub struct LiveState {
    pub enabled: bool,
    pub last_refresh: Option<Instant>,
    pub dedupe_key: Option<String>,
}

/// One "site + frame history + playback + liveness" engine (spec §3).
///
/// Phase 4c lands the CORE: identity, feed, history, cursor, limits,
/// live state, and engine-local status. The worker/texture slots the spec
/// sketches (`poll`, `loads`, `loop_stream`, `tex`) arrive WITH the view
/// that adopts them — adding them unused would be dead weight a weaker
/// maintainer has to reason about (spec §12b, YAGNI worker-slot
/// discipline §4.1).
///
/// Field-mutation contract: `history` self-enforces its generation bump
/// discipline (no `DerefMut` — see `frame_history.rs`); composite
/// operations (install, clear, feed switch) MUST go through the engine
/// methods so census invariants (sort order, trim, cursor clamps,
/// cross-site guard) hold. Direct field access is for reads and for
/// simple cursor/live toggles the census assigns to callers.
/// (`Clone` only: `FrameHistory` deliberately has no `Debug` — a debug
/// print of 2000 volumes is never what anyone wants.)
#[derive(Clone)]
pub struct LoopEngine {
    /// Stable identity; render-lane payload at adoption.
    pub id: EngineId,
    /// Which view this engine drives (default-policy picker only).
    pub role: EngineRole,
    /// What the engine follows. Change ONLY via [`Self::set_feed`] — it
    /// applies [`switch_policy`], the one home for history-clear rules.
    pub feed: FeedSource,
    /// The frame strip, kept ascending by [`FrameIdentity`] by every
    /// install route (census Appendix B).
    pub history: FrameHistory,
    /// Playback cursor; clamped into range by installs and trims.
    pub cursor: FrameCursor,
    /// Frame/byte limits (census D8). Overlay role carries the byte
    /// budget today; primary/pane budgets are 4d/4e decisions.
    pub limits: HistoryLimits,
    /// Live-refresh state; `enabled` gates [`Self::liveness`].
    pub live: LiveState,
    /// Engine-local status line; NEVER the global status bar (census
    /// D11: the global bar is a drain-time owner concern).
    pub status: String,
}

impl LoopEngine {
    /// A fresh engine for `role` following `feed`, with the role's
    /// default [`HistoryLimits`] and `live.enabled` armed exactly when
    /// the feed is a live variant (an Archive feed can never start
    /// enabled — spec §2).
    pub fn new(id: EngineId, role: EngineRole, feed: FeedSource) -> Self {
        let limits = match role {
            EngineRole::Primary => HistoryLimits::primary(DEFAULT_HISTORY_FRAME_LIMIT),
            EngineRole::Pane { .. } => HistoryLimits::pane(DEFAULT_HISTORY_FRAME_LIMIT),
            EngineRole::Overlay => HistoryLimits::overlay(),
        };
        let enabled = feed.is_live_variant();
        Self {
            id,
            role,
            feed,
            history: FrameHistory::new(),
            cursor: FrameCursor::default(),
            limits,
            live: LiveState {
                enabled,
                last_refresh: None,
                dedupe_key: None,
            },
            status: String::new(),
        }
    }

    /// THE one entry point for changing what the engine displays (spec
    /// §3): applies [`switch_policy`] against the current feed and live
    /// state, clears engine-owned loop state on `ClearAll`, arms
    /// `live.enabled` for live variants, and ALWAYS resets the dedupe key
    /// (a restarted poll must install its first tick — pinned by the
    /// same-site KEEP arm of the legacy intl-poll test, which still
    /// resets `poll_last_file`).
    ///
    /// Caller-owned switch side effects (dropping in-flight receivers,
    /// clearing the displayed volume, persisting settings) key on the
    /// returned [`SwitchAction`], exactly like the install outcome
    /// pattern.
    pub fn set_feed(&mut self, feed: FeedSource) -> SwitchAction {
        let action = switch_policy(&self.feed, self.live.enabled, &feed);
        if action == SwitchAction::ClearAll {
            self.clear_history();
        }
        self.live.enabled = feed.is_live_variant();
        self.live.dedupe_key = None;
        self.feed = feed;
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_source::sites::SiteRef;

    fn intl(provider: &str, site: &str) -> FeedSource {
        FeedSource::Live(SiteRef::Intl {
            provider_id: provider.to_owned(),
            site_id: site.to_owned(),
        })
    }

    /// set_feed is switch_policy plus the engine-owned side of a switch:
    /// same-site keeps history but still resets the dedupe key; cross-site
    /// clears history and cursor state.
    #[test]
    fn set_feed_applies_switch_policy_and_always_resets_the_dedupe_key() {
        let mut engine =
            LoopEngine::new(EngineId(1), EngineRole::Primary, intl("smhi", "angelholm"));
        engine.history.push(test_frame("angelholm"));
        engine.cursor.index = 0;
        engine.cursor.playing = true;
        engine.live.dedupe_key = Some("smhi/angelholm 2026-06-30T12:00Z".to_owned());

        // KEEP: active same-site restart.
        let action = engine.set_feed(intl("smhi", "angelholm"));
        assert_eq!(action, SwitchAction::KeepHistory);
        assert_eq!(engine.history.len(), 1, "same-site restart keeps history");
        assert_eq!(
            engine.live.dedupe_key, None,
            "dedupe key resets so the next tick installs"
        );
        assert!(engine.cursor.playing, "keep does not stop playback");

        // CLEAR: cross-site switch.
        engine.live.dedupe_key = Some("stale".to_owned());
        let action = engine.set_feed(intl("smhi", "sella"));
        assert_eq!(action, SwitchAction::ClearAll);
        assert!(
            engine.history.is_empty(),
            "cross-site switch clears the loop"
        );
        assert!(!engine.cursor.playing);
        assert_eq!(engine.live.dedupe_key, None);

        // CLEAR: same ids but the live state is disabled (the
        // "poll_active gates the keep" arm).
        engine.history.push(test_frame("sella"));
        engine.live.enabled = false;
        let action = engine.set_feed(intl("smhi", "sella"));
        assert_eq!(action, SwitchAction::ClearAll);
        assert!(engine.history.is_empty());
        assert!(engine.live.enabled, "a live feed switch re-arms the poll");
    }

    /// Role constructors: overlay carries the byte budget (4c wiring);
    /// archive feeds never start live-enabled.
    #[test]
    fn new_engine_role_defaults_match_the_phase_plan() {
        let overlay = LoopEngine::new(EngineId(1), EngineRole::Overlay, intl("ord", "deess"));
        assert_eq!(overlay.limits, HistoryLimits::overlay());
        assert!(overlay.live.enabled);

        let archive = LoopEngine::new(
            EngineId(2),
            EngineRole::Primary,
            FeedSource::Archive {
                site: SiteRef::Us {
                    level2_id: "KEAX".to_owned(),
                },
                window: ArchiveWindow {
                    start_utc: chrono::Utc::now(),
                    end_utc: chrono::Utc::now(),
                    anchor_utc: None,
                    max_frames: 20,
                },
            },
        );
        assert!(
            !archive.live.enabled,
            "an archive feed structurally cannot start live"
        );
    }

    fn test_frame(site: &str) -> FrameHistoryEntry {
        let volume = std::sync::Arc::new(radar_core::RadarVolume::new(
            radar_core::RadarSite::new(site),
            chrono::Utc::now(),
        ));
        FrameHistoryEntry {
            identity: frame_identity_for_volume(&volume),
            path: std::path::PathBuf::from(format!("intl:{site}")),
            volume,
            timings: None,
            status: FrameStatus::Complete,
            source_label: format!("smhi {site}"),
        }
    }
}
