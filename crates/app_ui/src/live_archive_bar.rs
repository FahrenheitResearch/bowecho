//! The LIVE | ARCHIVE bar — the v0.29 Phase-5 two-button front
//! (`docs/v029-engine-spec.md` §7 Phase 5; §12b owner decision 5).
//!
//! The bar is the primary flow; the Unified Player survives VERBATIM as
//! its "Advanced" disclosure. Nothing in the widget loads data or owns a
//! worker: every click returns a [`LiveArchiveBarAction`] and the
//! `ViewerApp` dispatch (`handle_live_archive_bar_action`, below) maps
//! it onto the EXISTING paths — the latest-loop loaders for LIVE, the
//! Unified Player "Loop Ending At" arms for ARCHIVE, and the
//! [`UnifiedPlayerAction`] dispatch for every sweep/frame control — so
//! the bar structurally cannot drift from the machinery it fronts.
//!
//! Mode truth: [`bar_mode`] derives from the SAME engine [`Liveness`]
//! verdict as the canvas mode chip (`mode_chip_state`), so the lit
//! segment and the chip can never disagree — an archive display cannot
//! light LIVE (the R8 class stays unrepresentable).
//!
//! Sync defaults (§12b owner decision 5): entering EITHER mode through
//! the bar arms warning sync by default — the dispatch side
//! (`arm_live_archive_bar_sync_defaults`) sets
//! `unified_player.auto_sync_warnings` and calls the audited spine
//! (`arm_unified_player_timeline_warning_sync`); archive loops then sync
//! their window's polygons and live-follow loops release back to
//! current-warning refresh through the existing
//! `maybe_auto_sync_timeline_warnings` reconciliation.

use chrono::{DateTime, Utc};
use data_source::sites::SiteRef;
use eframe::egui;

use crate::LIVE_COLOR;
use crate::unified_player::{UnifiedPlayerAction, UnifiedPlayerState};
use ui_core::loop_engine::Liveness;

/// The archive segment's active tint — the amber family the ARCHIVE chip
/// uses (a fixed UI accent, not radar data; the chip's exact color stays
/// style-registry-driven).
const ARCHIVE_ACTIVE_COLOR: egui::Color32 = egui::Color32::from_rgb(226, 178, 92);

/// Which segment is lit. Derived ONLY from the engine's liveness verdict
/// (plus "no data yet"), never from load bookkeeping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiveArchiveBarMode {
    /// Live feed, fresh.
    Live,
    /// Live feed past the stale threshold — still the LIVE segment (the
    /// chip carries the STALE detail).
    LiveStale,
    /// Fixed record: archive window, local files, or a paused live feed.
    Archive,
    /// Empty history — nothing to be live or archived about.
    Empty,
}

/// The one mode derivation. `liveness` is
/// `LoopEngine::liveness_with_live_flag(...)` for the primary — exactly
/// what `mode_chip_state` consumes.
pub(crate) fn bar_mode(liveness: Option<Liveness>) -> LiveArchiveBarMode {
    match liveness {
        None => LiveArchiveBarMode::Empty,
        Some(Liveness::Live { stale: false, .. }) => LiveArchiveBarMode::Live,
        Some(Liveness::Live { stale: true, .. }) => LiveArchiveBarMode::LiveStale,
        Some(Liveness::Archive { .. }) => LiveArchiveBarMode::Archive,
    }
}

/// The sweep-policy quick modes — §12b owner decision 5's "one click
/// deep" surface for the community's low-level sweep loops. The four
/// quick rows map onto the legacy `LowSweepLoopFilter` world; `Custom`
/// is the state shown while a per-product `LoopSweepControl` override is
/// active (the Range-mode home), applied through the full editor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SweepQuickMode {
    /// Scan-only loops (`SweepPolicyMode::Off`).
    Off,
    /// All complete low tilts (`SweepPolicyMode::AllLow`) — the fluid
    /// SAILS-dense default.
    AllLow,
    /// Dominant low level (`SweepPolicyMode::SameLevel`).
    SameLevel,
    /// Lowest level only (`SweepPolicyMode::BaseOnly`).
    BaseOnly,
    /// A per-product-family override set is active (includes the fixed
    /// `Range` mode). Edited in the Sweep Control window.
    Custom,
}

impl SweepQuickMode {
    /// The one-click menu rows, in menu order.
    pub(crate) const QUICK: [SweepQuickMode; 4] = [
        SweepQuickMode::Off,
        SweepQuickMode::AllLow,
        SweepQuickMode::SameLevel,
        SweepQuickMode::BaseOnly,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            SweepQuickMode::Off => "Scan only (off)",
            SweepQuickMode::AllLow => "All complete low tilts",
            SweepQuickMode::SameLevel => "Dominant low level",
            SweepQuickMode::BaseOnly => "Lowest level only",
            SweepQuickMode::Custom => "Custom ranges (advanced)",
        }
    }
}

/// Derive the active quick mode from the same settings the Unified
/// Player reads: a stored `LoopSweepControl` override wins (Custom),
/// otherwise the legacy enabled-flag + filter index
/// (`LowSweepLoopFilter::ALL` order: All, SameLevel, BaseOnly).
pub(crate) fn sweep_quick_mode(
    low_sweeps_enabled: bool,
    has_custom_control: bool,
    filter_index: usize,
) -> SweepQuickMode {
    if has_custom_control {
        return SweepQuickMode::Custom;
    }
    if !low_sweeps_enabled {
        return SweepQuickMode::Off;
    }
    match filter_index {
        1 => SweepQuickMode::SameLevel,
        2 => SweepQuickMode::BaseOnly,
        _ => SweepQuickMode::AllLow,
    }
}

/// The verbatim-dispatch contract (spec Phase 5): applying a quick mode
/// emits ONLY existing [`UnifiedPlayerAction`]s, replayed through
/// `handle_unified_player_action` — the bar adds zero new sweep
/// plumbing. Filter indices are `LowSweepLoopFilter::ALL` positions
/// (pinned by `live_archive_bar_sweep_filter_indices_match_player_order`
/// below).
pub(crate) fn sweep_quick_mode_player_actions(mode: SweepQuickMode) -> Vec<UnifiedPlayerAction> {
    match mode {
        SweepQuickMode::Off => vec![UnifiedPlayerAction::SetLowSweepsEnabled(false)],
        SweepQuickMode::AllLow => vec![
            UnifiedPlayerAction::SetLowSweepsEnabled(true),
            UnifiedPlayerAction::SetLowSweepFilter(0),
        ],
        SweepQuickMode::SameLevel => vec![
            UnifiedPlayerAction::SetLowSweepsEnabled(true),
            UnifiedPlayerAction::SetLowSweepFilter(1),
        ],
        SweepQuickMode::BaseOnly => vec![
            UnifiedPlayerAction::SetLowSweepsEnabled(true),
            UnifiedPlayerAction::SetLowSweepFilter(2),
        ],
        SweepQuickMode::Custom => vec![UnifiedPlayerAction::OpenSweepControls],
    }
}

/// The ARCHIVE popover's typed site override, resolved against the US
/// Level-II catalog by [`typed_site_status`]. Scope is deliberately
/// US-only: international archive loads need a provider and stay on the
/// loaded-radar path (click the site first).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TypedSiteStatus {
    /// Field empty, or it names the current US display owner: the load
    /// follows the loaded radar — today's behavior, the 90% case.
    FollowsOwner,
    /// A US catalog site different from the display owner: the load
    /// switches to it first, through the same path as a site-search
    /// pick (which releases an intl display owner correctly).
    ValidUs { level2_id: String },
    /// Not in the US catalog: Load is disabled with this input echoed
    /// in the reason.
    Unknown { input: String },
}

/// Resolve the popover's typed site field against the ONE union catalog
/// (`data_source::sites::resolve` — the Phase-3 rule; never iterate the
/// raw site list). `us_owner_id` is the display owner's Level-II ID
/// when a US site owns the display, `None` when an international site
/// does (any valid typed US ID is then a switch).
pub(crate) fn typed_site_status(input: &str, us_owner_id: Option<&str>) -> TypedSiteStatus {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return TypedSiteStatus::FollowsOwner;
    }
    if us_owner_id.is_some_and(|owner| owner.eq_ignore_ascii_case(trimmed)) {
        return TypedSiteStatus::FollowsOwner;
    }
    match data_source::sites::resolve(&SiteRef::Us {
        level2_id: trimmed.to_owned(),
    }) {
        Some(record) => match record.site {
            SiteRef::Us { level2_id } => TypedSiteStatus::ValidUs { level2_id },
            // A Us probe only resolves to a Us record; refuse defensively.
            SiteRef::Intl { .. } => TypedSiteStatus::Unknown {
                input: trimmed.to_owned(),
            },
        },
        None => TypedSiteStatus::Unknown {
            input: trimmed.to_owned(),
        },
    }
}

/// Everything the bar renders from — assembled read-only by
/// `ViewerApp::live_archive_bar_context`.
pub(crate) struct LiveArchiveBarContext {
    pub(crate) mode: LiveArchiveBarMode,
    /// The display owner's label (same string the Unified Player shows).
    pub(crate) owner_label: String,
    pub(crate) load_busy: bool,
    /// `Some(reason)` when the display owner has NO archive path — the
    /// derived spec-§1.3 value; the reason IS the greyed hover text.
    pub(crate) archive_reason: Option<&'static str>,
    pub(crate) sweep_mode: SweepQuickMode,
    /// Loop length for the archive load (the shared history frame
    /// limit; edits dispatch `SetHistoryFrameLimit` verbatim).
    pub(crate) frames: usize,
    pub(crate) frames_max: usize,
    /// Popover end-time seed: displayed frame time, else now.
    pub(crate) default_end_utc: DateTime<Utc>,
    /// Hint shown in the empty site field: the display owner's ID
    /// (an empty field means "that radar").
    pub(crate) owner_site_hint: String,
    /// The typed site-override verdict — derived by the SAME rule the
    /// dispatch re-applies ([`typed_site_status`]), so button state and
    /// load behavior cannot disagree.
    pub(crate) typed_site: TypedSiteStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiveArchiveBarAction {
    /// One click: re-arm the display owner's live feed + loop backfill
    /// through the existing latest-loop path.
    GoLive,
    /// Load N frames ending at the popover's UTC time through the
    /// Unified Player "Loop Ending At" arms (US, ORD, provider archive,
    /// honest-grey reason — one dispatch).
    LoadArchiveEndingAt,
    /// Edit the shared history frame limit (dispatches
    /// `UnifiedPlayerAction::SetHistoryFrameLimit`).
    SetFrames(usize),
    /// Apply a one-click sweep mode (dispatches the existing
    /// `UnifiedPlayerAction`s per [`sweep_quick_mode_player_actions`]).
    ApplySweepMode(SweepQuickMode),
    /// Open the full sweep-policy editor (per-product ranges).
    OpenSweepControls,
    /// Open the Unified Player — the bar's "Advanced" disclosure.
    OpenAdvanced,
    /// Jump to the Data tab's full archive browser.
    BrowseArchiveDays,
}

/// Render the bar. The ARCHIVE popover edits the SAME end-time fields
/// the Advanced player shows (`UnifiedPlayerState.end_*_input`) — one
/// set of values, no copying, one parser.
pub(crate) fn bar_ui(
    ui: &mut egui::Ui,
    context: &LiveArchiveBarContext,
    player: &mut UnifiedPlayerState,
) -> Option<LiveArchiveBarAction> {
    let mut action = None;
    player.ensure_end_time_inputs(context.default_end_utc);

    let live_lit = matches!(
        context.mode,
        LiveArchiveBarMode::Live | LiveArchiveBarMode::LiveStale
    );
    let live_text = if live_lit {
        egui::RichText::new("● LIVE").color(LIVE_COLOR).strong()
    } else {
        egui::RichText::new("● LIVE")
    };
    if ui
        .selectable_label(live_lit, live_text)
        .on_hover_text(
            "Go live: follow this radar's newest data and backfill a recent loop. \
             Synced warnings arm automatically.",
        )
        .clicked()
    {
        action = Some(LiveArchiveBarAction::GoLive);
    }

    let archive_lit = context.mode == LiveArchiveBarMode::Archive;
    let archive_text = if archive_lit {
        egui::RichText::new("ARCHIVE ▾")
            .color(ARCHIVE_ACTIVE_COLOR)
            .strong()
    } else {
        egui::RichText::new("ARCHIVE ▾")
    };
    // The popover holds text fields, so it must NOT use the default menu
    // close behavior (CloseOnClick): egui closes such a menu on any
    // click INSIDE it too, and a click into a TextEdit counts — the
    // popup vanished the moment you clicked a field (only drag-selecting
    // text survived, because a drag is not a click). CloseOnClickOutside
    // keeps it open while editing; Load/Browse still close it
    // explicitly via `ui.close()`.
    let (archive_button, archive_menu) = egui::containers::menu::MenuButton::new(archive_text)
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            ui.set_min_width(240.0);
            ui.label(egui::RichText::new("Loop ending at (UTC)").strong());
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut player.end_date_input)
                        .desired_width(88.0)
                        .hint_text("YYYY-MM-DD"),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut player.end_hour_input)
                        .desired_width(26.0)
                        .hint_text("HH"),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut player.end_minute_input)
                        .desired_width(26.0)
                        .hint_text("MM"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Site");
                ui.add(
                    egui::TextEdit::singleline(&mut player.archive_site_input)
                        .desired_width(56.0)
                        .hint_text(context.owner_site_hint.as_str()),
                )
                .on_hover_text(
                    "Which radar to load. Empty = the loaded radar; type a \
                     US site ID (KEAX, TOKC, …) to load a different one.",
                );
                ui.label("Frames");
                let mut frames = context.frames;
                if ui
                    .add(
                        egui::DragValue::new(&mut frames)
                            .range(1..=context.frames_max)
                            .speed(1.0),
                    )
                    .on_hover_text("Loop length: scans ending at the time above")
                    .changed()
                {
                    action = Some(LiveArchiveBarAction::SetFrames(frames));
                }
            });
            // Three honest Load states: an unknown typed site refuses
            // with the input echoed; the owner's derived no-archive
            // reason (spec §1.3) gates ONLY loads that follow the owner
            // — a valid typed US site always has the Level-II archive.
            let typed_unknown_reason = match &context.typed_site {
                TypedSiteStatus::Unknown { input } => {
                    Some(format!("{input} is not in the US site catalog"))
                }
                TypedSiteStatus::FollowsOwner | TypedSiteStatus::ValidUs { .. } => None,
            };
            let owner_reason = match &context.typed_site {
                TypedSiteStatus::FollowsOwner => context.archive_reason,
                TypedSiteStatus::ValidUs { .. } | TypedSiteStatus::Unknown { .. } => None,
            };
            if let Some(reason) = typed_unknown_reason {
                ui.add_enabled(false, egui::Button::new("Load archive loop"))
                    .on_disabled_hover_text(&reason);
                ui.weak(&reason);
            } else if let Some(reason) = owner_reason {
                // Honest grey (spec §1.3): the derived capability reason
                // IS the disabled explanation.
                ui.add_enabled(false, egui::Button::new("Load archive loop"))
                    .on_disabled_hover_text(reason);
                ui.weak(reason);
            } else {
                let load = ui
                    .add_enabled(!context.load_busy, egui::Button::new("Load archive loop"))
                    .on_hover_text(
                        "Load the loop ending at this UTC time. \
                         Synced warnings arm automatically.",
                    )
                    .on_disabled_hover_text("A radar load is already running");
                if load.clicked() {
                    action = Some(LiveArchiveBarAction::LoadArchiveEndingAt);
                    ui.close();
                }
            }
            ui.separator();
            if ui
                .button("Browse archive days…")
                .on_hover_text(
                    "The full archive browser (Data tab): day listings, hour chips, \
                     tornado-report jumps",
                )
                .clicked()
            {
                action = Some(LiveArchiveBarAction::BrowseArchiveDays);
                ui.close();
            }
            ui.weak(&context.owner_label);
        });
    archive_button.on_hover_text("Load a fixed past loop for the current radar");
    if archive_menu.is_none() {
        // Popover closed: the site override is popover-scoped — an
        // empty field follows whatever radar is loaded (the 90% case),
        // so leftover text must not silently redirect a later load.
        player.archive_site_input.clear();
    }

    ui.menu_button("Sweeps ▾", |ui| {
        ui.set_min_width(200.0);
        for mode in SweepQuickMode::QUICK {
            if ui
                .selectable_label(context.sweep_mode == mode, mode.label())
                .clicked()
            {
                action = Some(LiveArchiveBarAction::ApplySweepMode(mode));
                ui.close();
            }
        }
        ui.separator();
        if ui
            .selectable_label(
                context.sweep_mode == SweepQuickMode::Custom,
                SweepQuickMode::Custom.label(),
            )
            .on_hover_text(
                "The full sweep-policy editor: per-product-family modes and fixed \
                 elevation ranges, per pane",
            )
            .clicked()
        {
            action = Some(LiveArchiveBarAction::OpenSweepControls);
            ui.close();
        }
    })
    .response
    .on_hover_text(
        "Low-level sweep loops — how each scan expands into sweeps \
         (all-lowest is fluid, a fixed range is steadier)",
    );

    if ui
        .button("Advanced")
        .on_hover_text(
            "The full Unified Player: loads, archive windows, export, camera follow, \
             mosaics, warning/report/satellite/model sync",
        )
        .clicked()
    {
        action = Some(LiveArchiveBarAction::OpenAdvanced);
    }

    action
}

// ---------------------------------------------------------------------
// ViewerApp integration: context assembly + the action dispatch. Lives
// here (not main.rs) per the §12b extraction discipline — one file holds
// the whole bar story.
// ---------------------------------------------------------------------

use crate::{
    FeedSource, LowSweepLoopFilter, MAX_HISTORY_FRAME_LIMIT, SidebarTab, ViewerApp,
    archive_browser, dock,
};

impl ViewerApp {
    // -----------------------------------------------------------------
    // The LIVE | ARCHIVE bar (v0.29 Phase 5, live_archive_bar.rs).
    // The bar renders from a read-only context and returns an action;
    // everything it can do maps onto EXISTING paths below.
    // -----------------------------------------------------------------

    /// Assemble the bar's read-only context. The mode derives from the
    /// SAME engine liveness verdict as `mode_chip_state` (feed variant +
    /// bridged live flag + newest-frame age), so the lit segment and the
    /// canvas chip cannot disagree.
    fn live_archive_bar_context(&self) -> LiveArchiveBarContext {
        let user_stale_chip_seconds = self.style_registry.radar_age().stale_chip_seconds;
        let liveness = self.primary.liveness_with_live_flag(
            Utc::now(),
            user_stale_chip_seconds,
            self.primary_chip_live_flag(),
        );
        let archive_reason = match archive_browser::archive_access(&self.display_owner_site()) {
            archive_browser::ArchiveAccess::None { reason } => Some(reason),
            archive_browser::ArchiveAccess::Level2S3 | archive_browser::ArchiveAccess::Provider => {
                None
            }
        };
        let filter_index = LowSweepLoopFilter::ALL
            .iter()
            .position(|filter| *filter == self.low_sweep_loop_filter())
            .unwrap_or(0);
        LiveArchiveBarContext {
            mode: bar_mode(liveness),
            owner_label: self.unified_player_source_label(),
            load_busy: self.unified_player_load_busy(),
            archive_reason,
            sweep_mode: sweep_quick_mode(
                self.app_settings.loop_low_sweeps,
                self.app_settings.loop_sweep_control.is_some(),
                filter_index,
            ),
            frames: self.primary.limits.frame_limit,
            frames_max: MAX_HISTORY_FRAME_LIMIT,
            default_end_utc: self.displayed_timeline_time_utc().unwrap_or_else(Utc::now),
            owner_site_hint: match self.display_owner_site() {
                SiteRef::Us { level2_id } => level2_id,
                SiteRef::Intl { site_id, .. } => site_id,
            },
            typed_site: self.live_archive_bar_typed_site(),
        }
    }

    /// The popover's typed site override, resolved against the catalog —
    /// used by the context (button state) AND the dispatch (load
    /// behavior), so the two cannot disagree.
    fn live_archive_bar_typed_site(&self) -> TypedSiteStatus {
        let us_owner_id = match self.display_owner_site() {
            SiteRef::Us { level2_id } => Some(level2_id),
            SiteRef::Intl { .. } => None,
        };
        typed_site_status(
            &self.unified_player.archive_site_input,
            us_owner_id.as_deref(),
        )
    }

    pub(crate) fn live_archive_bar_ui(&mut self, ui: &mut egui::Ui) {
        let context = self.live_archive_bar_context();
        let action = bar_ui(ui, &context, &mut self.unified_player);
        let ctx = ui.ctx().clone();
        self.handle_live_archive_bar_action(action, &ctx);
    }

    /// §12b owner decision 5: entering EITHER bar mode arms the synced-
    /// warning defaults. Sets the auto-sync flag, then calls the audited
    /// spine fn (`arm_unified_player_timeline_warning_sync`: warnings
    /// visible, all-statuses, timeline-owned refresh). The existing
    /// reconciliation keeps both modes honest afterwards: archive loops
    /// sync their window's polygons (`maybe_auto_sync_timeline_warnings`)
    /// and live-follow loops release back to authoritative current-
    /// warning refresh (its live-follow arm).
    fn arm_live_archive_bar_sync_defaults(&mut self) {
        self.unified_player.auto_sync_warnings = true;
        self.arm_unified_player_timeline_warning_sync();
    }

    /// The bar's ARCHIVE load tail: arm the sync defaults, run the
    /// player's "Loop Ending At" dispatch verbatim (US Level-II, ORD's
    /// edge-preserving loader, the generic provider-archive arm, and
    /// the honest-grey reason all live there already), and mirror the
    /// player's feedback into the global status bar — the player window
    /// may be closed while the bar drives it.
    fn load_archive_ending_at_from_bar(&mut self, ctx: &egui::Context) {
        self.arm_live_archive_bar_sync_defaults();
        self.load_archive_loop_ending_at_for_unified_player(ctx);
        let player_status = self.unified_player.status_text().to_owned();
        if !player_status.is_empty() {
            self.status = player_status;
        }
    }

    /// One-click LIVE (spec Phase 5): re-arm the display owner's live
    /// feed, then backfill a loop through the EXISTING latest-loop path
    /// (`load_loop_for_unified_player` — the same code the player's Load
    /// Loop button runs). Per owner: international feeds re-arm via
    /// `start_intl_poll` — the explicit GO-LIVE feed switch, whose
    /// same-site-active KEEP / inactive CLEAR rule is the
    /// `switch_policy` table; an ACTIVE custom dir.list poll resumes in
    /// place (its catalog has no backfill); everything else — including
    /// the parked, inactive `CustomUrl` the legacy default leaves behind
    /// for ordinary US sessions — resolves to the selected US site,
    /// exactly like `display_owner_site()`.
    fn go_live_from_bar(&mut self, ctx: &egui::Context) {
        match self.primary.feed.clone() {
            FeedSource::Live(SiteRef::Intl {
                provider_id,
                site_id,
            }) if !provider_id.is_empty() && !site_id.is_empty() => {
                self.start_intl_poll(provider_id, site_id, ctx);
                self.load_loop_for_unified_player(ctx);
            }
            // Only an ACTIVE custom poll owns LIVE (a paused one is the
            // legacy parking state — poll_url survives for Start).
            FeedSource::CustomUrl(_) if self.poll_active => {
                self.primary.live.enabled = true;
                // Fire on the next tick, not a full cadence from now.
                self.poll_next = None;
            }
            FeedSource::Live(SiteRef::Us { .. })
            | FeedSource::Live(SiteRef::Intl { .. })
            | FeedSource::CustomUrl(_)
            | FeedSource::Archive { .. }
            | FeedSource::LocalFiles { .. } => {
                // Everything else resolves to the selected US site (the
                // display-owner rule). Arm the US live refresh so the
                // chip and the chunk chain follow, then loop-backfill
                // (which pauses an inactive custom poll's URL politely —
                // poll_url is preserved for its Start button).
                self.primary.live.enabled = true;
                self.load_loop_for_unified_player(ctx);
            }
        }
    }

    fn handle_live_archive_bar_action(
        &mut self,
        action: Option<LiveArchiveBarAction>,
        ctx: &egui::Context,
    ) {
        match action {
            Some(LiveArchiveBarAction::GoLive) => {
                self.arm_live_archive_bar_sync_defaults();
                self.go_live_from_bar(ctx);
            }
            Some(LiveArchiveBarAction::LoadArchiveEndingAt) => {
                match self.live_archive_bar_typed_site() {
                    TypedSiteStatus::Unknown { input } => {
                        // Unreachable through the UI (the Load button
                        // greys), but the dispatch stays honest on its
                        // own: refuse rather than load the wrong radar.
                        self.status = format!("{input} is not in the US site catalog");
                    }
                    TypedSiteStatus::ValidUs { level2_id } => {
                        // Switch first, through the SAME path as a
                        // site-search pick (it releases an intl display
                        // owner correctly), then load. The override is
                        // one-shot: the field clears, and the loaded
                        // radar IS this site afterwards.
                        self.activate_site_search_pick(&SiteRef::Us { level2_id }, None, ctx);
                        self.unified_player.archive_site_input.clear();
                        self.load_archive_ending_at_from_bar(ctx);
                    }
                    TypedSiteStatus::FollowsOwner => {
                        self.load_archive_ending_at_from_bar(ctx);
                    }
                }
            }
            Some(LiveArchiveBarAction::SetFrames(frames)) => {
                self.handle_unified_player_action(
                    Some(UnifiedPlayerAction::SetHistoryFrameLimit(frames)),
                    ctx,
                );
            }
            Some(LiveArchiveBarAction::ApplySweepMode(mode)) => {
                for player_action in sweep_quick_mode_player_actions(mode) {
                    self.handle_unified_player_action(Some(player_action), ctx);
                }
            }
            Some(LiveArchiveBarAction::OpenSweepControls) => {
                self.handle_unified_player_action(
                    Some(UnifiedPlayerAction::OpenSweepControls),
                    ctx,
                );
            }
            Some(LiveArchiveBarAction::OpenAdvanced) => {
                self.open_viewer(dock::WorkspacePane::UnifiedPlayer);
            }
            Some(LiveArchiveBarAction::BrowseArchiveDays) => {
                self.sidebar_tab = SidebarTab::Data;
                ctx.request_repaint();
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{set_primary_chip_frame, test_viewer_app_with_hazards};
    use chrono::Utc;
    use data_source::RadarSite;
    use radar_core::RadarVolume;
    use std::sync::Arc;

    /// The lit segment derives from the engine's liveness verdict — the
    /// same value the mode chip renders — so bar and chip cannot
    /// disagree. Archive (incl. paused live feeds) never lights LIVE.
    #[test]
    fn bar_mode_truth_table_matches_the_liveness_verdicts() {
        assert_eq!(bar_mode(None), LiveArchiveBarMode::Empty);
        assert_eq!(
            bar_mode(Some(Liveness::Live {
                age_seconds: 30,
                stale: false
            })),
            LiveArchiveBarMode::Live
        );
        assert_eq!(
            bar_mode(Some(Liveness::Live {
                age_seconds: 4000,
                stale: true
            })),
            LiveArchiveBarMode::LiveStale
        );
        assert_eq!(
            bar_mode(Some(Liveness::Archive { age_seconds: 120 })),
            LiveArchiveBarMode::Archive
        );
    }

    /// The quick-mode derivation reads the same settings the player
    /// shows: custom control wins, then the enabled flag, then the
    /// `LowSweepLoopFilter::ALL` index.
    #[test]
    fn sweep_quick_mode_derivation_table() {
        assert_eq!(sweep_quick_mode(false, false, 0), SweepQuickMode::Off);
        assert_eq!(sweep_quick_mode(true, false, 0), SweepQuickMode::AllLow);
        assert_eq!(sweep_quick_mode(true, false, 1), SweepQuickMode::SameLevel);
        assert_eq!(sweep_quick_mode(true, false, 2), SweepQuickMode::BaseOnly);
        // Out-of-range index falls back to the AllLow default, exactly
        // like the player's `.unwrap_or(0)` index clamp.
        assert_eq!(sweep_quick_mode(true, false, 9), SweepQuickMode::AllLow);
        // A stored LoopSweepControl override wins regardless of the
        // legacy flags (it is what the loop actually plays).
        assert_eq!(sweep_quick_mode(false, true, 0), SweepQuickMode::Custom);
        assert_eq!(sweep_quick_mode(true, true, 2), SweepQuickMode::Custom);
    }

    /// The bar adds NO new sweep plumbing: every quick mode replays
    /// existing `UnifiedPlayerAction`s verbatim, and the spec's four
    /// policy classes (Off / AllLow / BaseOnly / Range) are all reachable
    /// one click deep — Range through the sweep-control editor.
    #[test]
    fn sweep_quick_modes_map_onto_the_unified_player_dispatch_verbatim() {
        assert_eq!(
            sweep_quick_mode_player_actions(SweepQuickMode::Off),
            vec![UnifiedPlayerAction::SetLowSweepsEnabled(false)]
        );
        assert_eq!(
            sweep_quick_mode_player_actions(SweepQuickMode::AllLow),
            vec![
                UnifiedPlayerAction::SetLowSweepsEnabled(true),
                UnifiedPlayerAction::SetLowSweepFilter(0),
            ]
        );
        assert_eq!(
            sweep_quick_mode_player_actions(SweepQuickMode::SameLevel),
            vec![
                UnifiedPlayerAction::SetLowSweepsEnabled(true),
                UnifiedPlayerAction::SetLowSweepFilter(1),
            ]
        );
        assert_eq!(
            sweep_quick_mode_player_actions(SweepQuickMode::BaseOnly),
            vec![
                UnifiedPlayerAction::SetLowSweepsEnabled(true),
                UnifiedPlayerAction::SetLowSweepFilter(2),
            ]
        );
        assert_eq!(
            sweep_quick_mode_player_actions(SweepQuickMode::Custom),
            vec![UnifiedPlayerAction::OpenSweepControls]
        );
        // The menu shows all four one-click rows.
        assert_eq!(SweepQuickMode::QUICK.len(), 4);
    }

    /// Phase 5 bar truth: the LIVE|ARCHIVE segments derive from the SAME
    /// engine liveness verdict as the canvas chip, cell by cell — a lit
    /// LIVE segment over an archive display is unrepresentable.
    #[test]
    fn live_archive_bar_mode_agrees_with_the_primary_chip() {
        use LiveArchiveBarMode;

        let mut app = test_viewer_app_with_hazards(Vec::new());
        assert_eq!(
            app.live_archive_bar_context().mode,
            LiveArchiveBarMode::Empty,
            "empty history lights neither segment"
        );

        app.primary.live.enabled = true;
        set_primary_chip_frame(
            &mut app,
            Arc::new(RadarVolume::new(
                radar_core::RadarSite::new("KTLX"),
                Utc::now() - chrono::Duration::seconds(10),
            )),
        );
        let (_, _, chip_kind) = app.mode_chip_state().expect("live chip");
        assert_eq!(chip_kind, "LIVE");
        assert_eq!(
            app.live_archive_bar_context().mode,
            LiveArchiveBarMode::Live
        );

        set_primary_chip_frame(
            &mut app,
            Arc::new(RadarVolume::new(
                radar_core::RadarSite::new("KTLX"),
                Utc::now() - chrono::Duration::hours(2),
            )),
        );
        let (_, _, chip_kind) = app.mode_chip_state().expect("stale chip");
        assert_eq!(chip_kind, "STALE");
        assert_eq!(
            app.live_archive_bar_context().mode,
            LiveArchiveBarMode::LiveStale,
            "a stale live feed stays on the LIVE segment (the chip carries the detail)"
        );

        app.primary.live.enabled = false;
        let (_, _, chip_kind) = app.mode_chip_state().expect("archive chip");
        assert_eq!(chip_kind, "ARCH");
        assert_eq!(
            app.live_archive_bar_context().mode,
            LiveArchiveBarMode::Archive,
            "a paused/fixed display lights ARCHIVE"
        );
    }

    /// §12b owner decision 5, the ARCHIVE arm: entering archive mode
    /// through the bar arms the synced-warning defaults BEFORE the load
    /// dispatch runs — even when the load itself then fails input
    /// validation (deliberately empty end-time inputs here, so no worker
    /// spawns), and the player's honest feedback lands in the global
    /// status bar.
    #[test]
    fn live_archive_bar_archive_entry_arms_synced_warning_defaults() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.sites = vec![RadarSite::new("KTLX")];
        app.selected_site_index = 0;
        assert!(
            !app.unified_player.auto_sync_warnings,
            "old default is opt-in"
        );
        app.hazards_visible = false;
        app.hazards_active_only = true;
        app.live_hazard_auto_refresh = true;

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::LoadArchiveEndingAt),
            &egui::Context::default(),
        );

        assert!(
            app.unified_player.auto_sync_warnings,
            "bar entry arms auto-sync"
        );
        assert!(app.hazards_visible, "warnings become visible");
        assert!(!app.hazards_active_only, "timeline sync shows all statuses");
        assert!(
            !app.live_hazard_auto_refresh,
            "timeline-owned refresh (the audited spine fn ran)"
        );
        assert!(app.load_receiver.is_none(), "invalid inputs spawn nothing");
        assert!(
            app.status.contains("End date must be YYYY-MM-DD"),
            "player feedback mirrors into the global status bar: {}",
            app.status
        );
    }

    /// The typed site override resolves by one pure rule against the
    /// union catalog (empty or owner-matching input follows the loaded
    /// radar; a catalog match is a switch, canonical-cased; anything
    /// else is refused) — the same rule gates the Load button and the
    /// dispatch.
    #[test]
    fn typed_site_override_resolution_table() {
        assert_eq!(
            typed_site_status("", Some("KTLX")),
            TypedSiteStatus::FollowsOwner,
            "empty follows the loaded radar"
        );
        assert_eq!(
            typed_site_status("  ktlx ", Some("KTLX")),
            TypedSiteStatus::FollowsOwner,
            "naming the owner (any case) is not a switch"
        );
        assert_eq!(
            typed_site_status(" keax ", Some("KTLX")),
            TypedSiteStatus::ValidUs {
                level2_id: "KEAX".to_owned()
            },
            "a catalog match resolves to the canonical ID"
        );
        assert_eq!(
            typed_site_status("ktlx", None),
            TypedSiteStatus::ValidUs {
                level2_id: "KTLX".to_owned()
            },
            "with an intl display owner, any valid US ID is a switch"
        );
        assert_eq!(
            typed_site_status("XXXX", Some("KTLX")),
            TypedSiteStatus::Unknown {
                input: "XXXX".to_owned()
            },
            "unknown IDs are refused, echoing the input"
        );
    }

    /// Typing an adjacent radar into the ARCHIVE popover switches the
    /// selection FIRST — through the same path as a site-search pick —
    /// then runs the normal archive load for it; the override is
    /// one-shot (the field clears). Empty end-time inputs keep the load
    /// on its validation error, so no worker spawns.
    #[test]
    fn typed_site_archive_load_switches_the_site_first() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.sites = vec![RadarSite::new("KTLX"), RadarSite::new("KEAX")];
        app.selected_site_index = 0;
        app.unified_player.archive_site_input = "keax".to_owned();

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::LoadArchiveEndingAt),
            &egui::Context::default(),
        );

        assert_eq!(
            app.selected_site().map(|site| site.level2_id.clone()),
            Some("KEAX".to_owned()),
            "the load targets the typed site"
        );
        assert!(
            app.unified_player.archive_site_input.is_empty(),
            "the override is one-shot"
        );
        assert!(
            app.unified_player.auto_sync_warnings,
            "the archive entry still arms the sync defaults"
        );
        assert!(app.load_receiver.is_none(), "invalid inputs spawn nothing");
        assert!(
            app.status.contains("End date must be YYYY-MM-DD"),
            "the load ran (and failed validation) for the new site: {}",
            app.status
        );
    }

    /// An unknown typed site refuses the load outright — no site
    /// switch, no worker, no archive-mode side effects — with the input
    /// echoed honestly in the status bar.
    #[test]
    fn typed_site_archive_load_refuses_an_unknown_site() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.sites = vec![RadarSite::new("KTLX")];
        app.selected_site_index = 0;
        app.unified_player.archive_site_input = "XXXX".to_owned();

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::LoadArchiveEndingAt),
            &egui::Context::default(),
        );

        assert_eq!(
            app.selected_site().map(|site| site.level2_id.clone()),
            Some("KTLX".to_owned()),
            "no switch"
        );
        assert!(app.load_receiver.is_none(), "no load spawns");
        assert!(
            !app.unified_player.auto_sync_warnings,
            "a refused load does not enter archive mode"
        );
        assert_eq!(app.status, "XXXX is not in the US site catalog");
    }

    /// §12b owner decision 5, the LIVE arm: one click re-arms the intl
    /// poll (the explicit GO-LIVE feed switch), starts the loop backfill
    /// through the existing latest-loop path, and arms the same synced-
    /// warning defaults. Provider "zz" does not exist, so the backfill
    /// worker reports an immediate error without touching the network.
    #[test]
    fn live_archive_bar_go_live_arms_defaults_and_rearms_the_intl_poll() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        // An intl ARCHIVE display owner: feed owned, poll inactive.
        app.set_intl_archive_primary_source("zz", "nowhere");
        assert!(!app.poll_active);
        assert!(!app.unified_player.auto_sync_warnings);

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::GoLive),
            &egui::Context::default(),
        );

        assert!(app.poll_active, "GO LIVE re-arms the intl poll");
        assert!(
            matches!(
                &app.primary.feed,
                FeedSource::Live(SiteRef::Intl { provider_id, site_id })
                    if provider_id == "zz" && site_id == "nowhere"
            ),
            "the feed switch targets the same display owner"
        );
        assert!(
            app.intl_loop_rx.is_some(),
            "loop backfill started through the existing intl latest-loop path"
        );
        assert!(
            app.unified_player.auto_sync_warnings,
            "LIVE entry arms auto-sync"
        );
        assert!(app.hazards_visible);
    }

    /// The US arm of GO LIVE flips the live chunk-refresh switch even
    /// when no site is selected (the backfill then reports "No site
    /// selected" instead of spawning a load).
    #[test]
    fn live_archive_bar_go_live_arms_us_live_refresh() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.sites = Vec::new();
        app.primary.feed = FeedSource::Live(SiteRef::Us {
            level2_id: "KTLX".to_owned(),
        });
        assert!(!app.primary.live.enabled);

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::GoLive),
            &egui::Context::default(),
        );

        assert!(
            app.primary.live.enabled,
            "US GO LIVE arms the chunk refresh"
        );
        assert!(
            app.load_receiver.is_none(),
            "no site selected spawns nothing"
        );
        assert!(app.unified_player.auto_sync_warnings);
    }

    /// Cold start: the legacy default feed is a PARKED, inactive
    /// `CustomUrl` — for an ordinary US session GO LIVE must take the
    /// US arm (the display-owner rule), never resume the empty URL
    /// poll. An ACTIVE custom poll, by contrast, resumes in place.
    #[test]
    fn live_archive_bar_go_live_parked_custom_url_takes_the_us_arm() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.sites = Vec::new();
        // The test harness default IS the cold-start shape: an empty
        // CustomUrl feed with the poll inactive.
        assert!(matches!(&app.primary.feed, FeedSource::CustomUrl(url) if url.is_empty()));
        assert!(!app.poll_active);

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::GoLive),
            &egui::Context::default(),
        );

        assert!(
            !app.poll_active,
            "a parked custom URL must not start polling an empty URL"
        );
        assert!(
            app.primary.live.enabled,
            "cold-start GO LIVE arms the US live refresh"
        );

        // An ACTIVE custom poll owns LIVE and resumes in place.
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.primary.feed = FeedSource::CustomUrl("http://example.com/dow8".to_owned());
        app.poll_active = true;
        app.poll_next = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::GoLive),
            &egui::Context::default(),
        );

        assert!(app.poll_active, "the active custom poll stays armed");
        assert!(
            app.poll_next.is_none(),
            "resume fires on the next tick, not a full cadence from now"
        );
        assert!(
            app.load_receiver.is_none(),
            "no US load spawns under an active custom poll"
        );
    }

    /// Tripwire: the bar's sweep quick-mode filter indices are
    /// `LowSweepLoopFilter::ALL` positions. If the order ever changes,
    /// this pins the remap.
    #[test]
    fn live_archive_bar_sweep_filter_indices_match_player_order() {
        assert_eq!(LowSweepLoopFilter::ALL[0], LowSweepLoopFilter::All);
        assert_eq!(LowSweepLoopFilter::ALL[1], LowSweepLoopFilter::SameLevel);
        assert_eq!(LowSweepLoopFilter::ALL[2], LowSweepLoopFilter::BaseOnly);
    }

    /// The bar's sweep menu drives the VERBATIM UnifiedPlayerAction
    /// dispatch: applying a quick mode lands in the same settings the
    /// player's own controls write.
    #[test]
    fn live_archive_bar_sweep_mode_dispatch_writes_the_player_settings() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let ctx = egui::Context::default();

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::ApplySweepMode(
                SweepQuickMode::BaseOnly,
            )),
            &ctx,
        );
        assert!(app.app_settings.loop_low_sweeps);
        assert_eq!(app.app_settings.loop_low_sweep_filter, "base");
        assert_eq!(
            app.app_settings.loop_sweep_control, None,
            "picking a quick mode clears a custom control, exactly like the player's combo"
        );
        assert_eq!(
            app.live_archive_bar_context().sweep_mode,
            SweepQuickMode::BaseOnly
        );

        app.handle_live_archive_bar_action(
            Some(LiveArchiveBarAction::ApplySweepMode(SweepQuickMode::Off)),
            &ctx,
        );
        assert!(!app.app_settings.loop_low_sweeps);
        assert_eq!(
            app.live_archive_bar_context().sweep_mode,
            SweepQuickMode::Off
        );

        app.handle_live_archive_bar_action(Some(LiveArchiveBarAction::OpenSweepControls), &ctx);
        assert!(app.sweep_controls_open, "Range editor opens one click deep");
    }
}
