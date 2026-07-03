//! Radar overlay layers — the [`OverlayView`] family (v0.29 spec §6 row 8,
//! `docs/v029-engine-spec.md`; extracted VERBATIM from `main.rs` as its own
//! Phase-4d extraction commit, per the layers_rail playbook).
//!
//! An overlay layer is one [`ui_core::loop_engine::LoopEngine`] plus the
//! display-side state the engine does not own (textures, receivers,
//! opacity). Overlays were the FIRST engine adoption (Phase 4c): live
//! refresh cadence, feed switches, history installs, and the state chip
//! all derive from the engine; the shared overlay render pool (Phase 4b)
//! renders every layer on `LaneId::Overlay(id)`.
//!
//! This module owns: the `OverlayView` type + state chip, the
//! add/refresh/load/drain family, the shared-pool render job +
//! drain/install routing, the coordinated-loop / Mosaic candidate and
//! window-load helpers, and the timeline-sync selection helpers. Anything
//! shared with the pane/primary paths (`fetch_intl_frame`,
//! `limit_archive_objects_for_event_loop`, `sweep_history_cut_at_or_before`,
//! pane refresh/follow) deliberately stays in `main.rs` until its own
//! phase (spec §6 rows 9-10).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use data_source::RadarSite;
use data_source::sites::{SiteKind, SiteRef};
use eframe::egui;
use radar_core::RadarVolume;
use settings::{SweepPolicy, SweepPolicyMode};
use ui_core::render_service::{
    DrainBudget, LaneId, RenderRoute, RepaintDecision, overlay_pool_worker_target,
    post_drain_repaint, render_route_for,
};

use crate::sites_ui::{NearestOverlayTarget, nearest_overlay_dispatch};
use crate::{
    ACTIVE_LOAD_POLL_MS, ArchiveHistoryLoadContext, ArchiveWindow, AsyncLoadResult,
    AsyncLoadUpdate, AsyncRenderResult, DEFAULT_RADAR_RANGE_KM, DecodedLoad, DecodedLoadBatch,
    DisplayProduct, EngineId, EngineRole, FeedSource, FrameHistoryEntry, FrameStatus,
    InstallSelection, IntlFrameResult, LOW_SWEEP_FILTER_ELEVATION_TOLERANCE_DEG, LatestLoadMode,
    Liveness, LoadTimings, LoopEngine, LowSweepCutKey, MAX_HISTORY_FRAME_LIMIT,
    MAX_RADAR_OVERLAY_LAYERS, RENDER_RESULT_POLL_MS, RenderRecycleBuffer, RenderRequest,
    RenderWorkerCacheMode, RenderWorkerCachePolicy, RenderWorkerGeometryCache,
    RenderWorkerMomentCache, RenderWorkerSampleCache, RenderWorkerViewportSignature,
    RenderedTexture, SelectionPolicy, TextureKey, ViewerApp, archive_browser,
    archive_object_scan_time_utc, best_cut_for_product, cache_dir, cut_start_time_utc,
    displayable_cuts_for_product, effective_worker_threads, fetch_intl_frame,
    is_displayable_on_cut, limit_archive_objects_for_event_loop,
    load_archive_history_objects_parallel, normalized_history_limit, radar_color_image_from_rgba,
    radar_texture_options, selected_grid_range_km_for, send_archive_progress, site_location,
    spawn_latest_level2_load_worker, sweep_cuts_for_history_entry, sweep_history_cut_at_or_before,
    texture_keys_match_data_and_style,
};

const STORM_VIDEO_SYNC_TOTAL_RADARS: usize = 5;
const STORM_VIDEO_SYNC_OVERLAY_RADARS: usize = STORM_VIDEO_SYNC_TOTAL_RADARS - 1;
/// A coordinated overlay holds the newest observation at-or-before the master
/// timeline time, but not indefinitely. This comfortably spans normal WSR-88D
/// volume gaps while preventing a failed/one-frame radar from becoming a
/// permanent ghost over the rest of the loop.
const COORDINATED_RADAR_MAX_STALENESS_SECONDS: i64 = 12 * 60;
const DEFAULT_RADAR_OVERLAY_ALPHA: u8 = 210;

// v0.29 Phase 4c: `IntlOverlayFeed` dissolved into the overlay engine
// (spec §8 row "IntlOverlayFeed"): provider/site → `engine.feed`,
// `last_identity` → `engine.live.dedupe_key`, `rx` → `OverlayView::intl_rx`
// (view-side until the engine grows worker slots at the pane/primary ports).

/// One radar overlay layer: a [`LoopEngine`] plus the display-side state
/// the engine does not own (v0.29 Phase 4c — the spec §3 `OverlayView`
/// shape; overlays are the FIRST engine adoption, spec §7). Textures and
/// the load/poll receivers stay view-side because the 4c engine core
/// deliberately has no worker/texture slots — those arrive with the
/// pane/primary ports (4d/4e).
///
/// Engine-owned state, with the legacy field it replaced (for grep
/// archaeology): `engine.history` ← `frame_history: Vec<FrameHistoryEntry>`
/// (now on the generation spine, census D12), `engine.cursor.index` ←
/// `selected_frame_index`, `engine.status` ← `status`, `engine.id.0` ←
/// `id`, `engine.live.last_refresh` ← `last_realtime_level2_refresh`,
/// `engine.feed` + `engine.live.dedupe_key` ← `IntlOverlayFeed`.
pub(crate) struct OverlayView {
    pub(crate) engine: LoopEngine,
    pub(crate) site: RadarSite,
    /// In-flight international catalog-probe fetch — the intl analog of
    /// `load_receiver` (was `IntlOverlayFeed::rx`).
    pub(crate) intl_rx: Option<mpsc::Receiver<IntlFrameResult>>,
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) volume: Option<Arc<RadarVolume>>,
    /// True only for overlays loaded by the Unified Player's coordinated
    /// archive-loop actions. Ordinary live/manual overlays keep their own
    /// newest frame and are not forced onto an archive cursor.
    pub(crate) timeline_sync: bool,
    /// Cut chosen for the coordinated timeline observation. This is separate
    /// from the primary radar's cut index: different radars/VCPs can put the
    /// same physical elevation at completely different indices.
    pub(crate) selected_cut: Option<usize>,
    pub(crate) load_timing: Option<LoadTimings>,
    pub(crate) texture: Option<egui::TextureHandle>,
    pub(crate) texture_key: Option<TextureKey>,
    pub(crate) pending_render_key: Option<TextureKey>,
    pub(crate) load_receiver: Option<mpsc::Receiver<AsyncLoadResult>>,
    pub(crate) opacity: u8,
    pub(crate) visible: bool,
    pub(crate) radar_range_km: f32,
    pub(crate) render_ms: Option<f32>,
    pub(crate) worker_ms: Option<f32>,
    pub(crate) texture_ms: Option<f32>,
}

impl OverlayView {
    /// A US Level-II overlay layer following `site` live. Since the
    /// Phase-4b pool flip a layer owns NO render worker: renders go
    /// through `ViewerApp::overlay_render_pool` on `LaneId::Overlay(id)`.
    pub(crate) fn new(id: u64, site: RadarSite) -> Self {
        let feed = FeedSource::Live(data_source::sites::SiteRef::Us {
            level2_id: site.level2_id.clone(),
        });
        Self::with_feed(id, site, feed)
    }

    /// An international overlay layer following `provider_id`/`site_id`
    /// live (`site` carries the display label + coordinates).
    pub(crate) fn new_intl(id: u64, site: RadarSite, provider_id: String, site_id: String) -> Self {
        let feed = FeedSource::Live(data_source::sites::SiteRef::Intl {
            provider_id,
            site_id,
        });
        Self::with_feed(id, site, feed)
    }

    fn with_feed(id: u64, site: RadarSite, feed: FeedSource) -> Self {
        let mut engine = LoopEngine::new(EngineId(id), EngineRole::Overlay, feed);
        engine.status = format!("Queued {}", site.level2_id);
        Self {
            engine,
            site,
            intl_rx: None,
            source_path: None,
            volume: None,
            timeline_sync: false,
            selected_cut: None,
            load_timing: None,
            texture: None,
            texture_key: None,
            pending_render_key: None,
            load_receiver: None,
            opacity: DEFAULT_RADAR_OVERLAY_ALPHA,
            visible: true,
            radar_range_km: DEFAULT_RADAR_RANGE_KM,
            render_ms: None,
            worker_ms: None,
            texture_ms: None,
        }
    }

    /// The provider/site pair when this layer follows an international
    /// source — live OR archive-window (a Mosaic ORD loop is still an
    /// international layer for refresh/promote routing).
    pub(crate) fn intl_site_ref(&self) -> Option<(&str, &str)> {
        match &self.engine.feed {
            FeedSource::Live(data_source::sites::SiteRef::Intl {
                provider_id,
                site_id,
            })
            | FeedSource::Archive {
                site:
                    data_source::sites::SiteRef::Intl {
                        provider_id,
                        site_id,
                    },
                ..
            } => Some((provider_id.as_str(), site_id.as_str())),
            _ => None,
        }
    }

    pub(crate) fn is_intl(&self) -> bool {
        self.intl_site_ref().is_some()
    }

    pub(crate) fn radar_location(&self) -> Option<(f32, f32)> {
        self.volume
            .as_ref()
            .and_then(|volume| Some((volume.site.latitude_deg?, volume.site.longitude_deg?)))
            .or_else(|| site_location(&self.site))
    }
}

/// The overlay layer's state chip (Layers rail). Strings are the legacy
/// chip vocabulary, greppable-identical (census D11); the live-vs-queued
/// truth derives from [`LoopEngine::liveness`] — an engine with any frame
/// on a live feed reads "live" exactly where the legacy `volume.is_some()`
/// check did. `displaying` covers the one gap liveness cannot see: a
/// census-D14 display-only preview (volume painted, history still empty)
/// kept the legacy chip "live" and still does.
pub(crate) fn overlay_state_chip(
    loading: bool,
    timeline_sync: bool,
    liveness: Option<Liveness>,
    displaying: bool,
) -> &'static str {
    if loading {
        "loading"
    } else if timeline_sync {
        "timeline"
    } else if liveness.is_some() || displaying {
        "live"
    } else {
        "queued"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinatedOverlayLoadMode {
    Live,
    ArchiveWindow {
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
        max_frames: usize,
    },
}

/// A resolved coordinated-overlay pick — the mosaic candidate pool spans
/// both worlds (v0.29 Phase 3), and the add path dispatches on kind.
enum CoordinatedOverlaySite {
    Us(RadarSite),
    Intl(data_source::international::IntlSite),
}

/// Job body for one shared-overlay-pool worker (the Phase-4b flip): the
/// pre-4b per-layer worker's render path — Overlay cache policy, recycle
/// buffer reuse — minus two pieces that moved or were dead:
///
/// - the queue/coalescing loop moved into `OverlayPool`'s per-lane queue
///   (`ui_core::render_service::merge_lane_request`), and
/// - the speculative sample/velocity cache warming tail, which was
///   unreachable under `RenderWorkerCacheMode::Overlay` (pinned by
///   `overlay_cache_policy_keeps_background_radars_lightweight`).
///
/// Worker-local caches live in the returned closure, exactly like the
/// prewarm job above. All pool workers pull recycled buffers from the ONE
/// shared receiver; the best-match heuristic is the pre-4b one, verbatim.
pub(crate) fn overlay_pool_render_job(
    recycle_receiver: Arc<Mutex<mpsc::Receiver<RenderRecycleBuffer>>>,
) -> impl FnMut(RenderRequest) -> AsyncRenderResult {
    let cache_policy = RenderWorkerCachePolicy::detect(RenderWorkerCacheMode::Overlay);
    let mut reusable_pixels = Vec::new();
    let mut reusable_pixels_signature: Option<RenderWorkerViewportSignature> = None;
    let mut moment_caches: Vec<RenderWorkerMomentCache> = Vec::new();
    let mut sample_caches: Vec<RenderWorkerSampleCache> = Vec::new();
    let mut geometry_caches: Vec<RenderWorkerGeometryCache> = Vec::new();
    let mut last_direct_viewports: Vec<RenderWorkerViewportSignature> = Vec::new();
    move |request: RenderRequest| {
        let requested_buffer_signature = RenderWorkerViewportSignature::new(
            Arc::as_ptr(&request.volume) as usize,
            request.key.dealias_reference_volume_ptr,
            request.key.dealias_env_ptr,
            request.cut,
            request.product.clone(),
            request.product.base_moment(),
            request.render_dealiased_velocity,
            request.key.color_table_signature,
            request.key.storm_motion_key,
            request.key.hail_levels_key,
            request.key.smoothing,
            request.key.dealias_cascade,
            request.key.gate_filter_decidbz,
            request.key.viewport,
        );
        if let Ok(recycle_receiver) = recycle_receiver.lock() {
            while let Ok(recycled) = recycle_receiver.try_recv() {
                let recycled_matches =
                    recycled.signature.as_ref() == Some(&requested_buffer_signature);
                let current_matches =
                    reusable_pixels_signature.as_ref() == Some(&requested_buffer_signature);
                if reusable_pixels.is_empty()
                    || (recycled_matches && !current_matches)
                    || (recycled_matches == current_matches
                        && recycled.rgba.capacity() > reusable_pixels.capacity())
                {
                    reusable_pixels = recycled.rgba;
                    reusable_pixels_signature = recycled.signature;
                }
            }
        }
        let key = request.key.clone();
        let lane = request.lane;
        let result = ViewerApp::render_viewport_payload(
            &request,
            &mut reusable_pixels,
            &mut reusable_pixels_signature,
            &mut moment_caches,
            &mut sample_caches,
            &mut geometry_caches,
            &mut last_direct_viewports,
            cache_policy,
        );
        AsyncRenderResult { key, lane, result }
    }
}

impl ViewerApp {
    pub(crate) fn maybe_refresh_radar_layers(&mut self, ctx: &egui::Context) {
        if !self.primary.live.enabled {
            return;
        }

        let mut requested_repaint = false;
        for (index, layer) in self.radar_layers.iter_mut().enumerate() {
            if !layer.visible || layer.load_receiver.is_some() || layer.timeline_sync {
                // Coordinated archive overlays are owned by the master loop
                // cursor. Refreshing them through the live/latest path turns
                // one site into a static newest-frame ghost over the archive
                // loop and discards its synchronized history.
                continue;
            }
            // Cadence from the engine's (role, feed) table (spec §3):
            // 5 s for a US overlay chunk refresh, 60 s for an international
            // catalog probe, None for archive/local feeds (a fixed record
            // is structurally incapable of refresh). Same values as the
            // pre-engine constants — the TABLE is what moved.
            let Some(cadence) = layer.engine.poll_cadence() else {
                continue;
            };
            let refresh_after = cadence + Duration::from_millis((index as u64 % 8) * 350);
            let should_refresh = layer
                .engine
                .live
                .last_refresh
                .is_none_or(|last_refresh| last_refresh.elapsed() >= refresh_after);
            if !should_refresh {
                continue;
            }
            // International layers refresh via the provider catalog probe —
            // never the US Level-II chain.
            if layer.is_intl() {
                if layer.intl_rx.is_none() {
                    Self::start_intl_radar_layer_load(layer, ctx);
                    requested_repaint = true;
                }
            } else {
                Self::start_radar_layer_load(layer, LatestLoadMode::AutoRefresh, ctx);
                requested_repaint = true;
            }
        }

        if !requested_repaint && !self.radar_layers.is_empty() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// Ctrl+right-click: overlay the nearest radar — US or international,
    /// whichever marker is geographically closer (field request: intl
    /// multi-radar like CONUS). v0.21 behavior, restored after the v0.27.2
    /// regression left the intl branch dead and the US branch uncapped
    /// (Ctrl+right-click over Berlin silently added a transatlantic 88D).
    pub(crate) fn add_nearest_radar_overlay_at(
        &mut self,
        rect: egui::Rect,
        pointer: egui::Pos2,
        ctx: &egui::Context,
    ) {
        let (lon, lat) = self.screen_to_lon_lat(rect, pointer);
        match nearest_overlay_dispatch(
            &self.sites,
            data_source::international::intl_static_sites(),
            lat,
            lon,
        ) {
            Some(NearestOverlayTarget::Us(index)) => {
                if let Some(site) = self.sites.get(index).cloned() {
                    self.add_or_refresh_radar_layer_for_current_timeline(site, ctx);
                }
            }
            Some(NearestOverlayTarget::Intl(site)) => {
                self.add_or_refresh_intl_radar_layer(&site, ctx);
            }
            None => {
                self.status =
                    "No WSR-88D or international radar within 460 km to overlay".to_owned();
            }
        }
    }

    fn add_or_refresh_radar_layer_for_current_timeline(
        &mut self,
        site: RadarSite,
        ctx: &egui::Context,
    ) {
        match self.coordinated_overlay_load_mode() {
            CoordinatedOverlayLoadMode::Live => self.add_or_refresh_radar_layer(site, ctx),
            CoordinatedOverlayLoadMode::ArchiveWindow {
                start_utc,
                end_utc,
                max_frames,
            } => {
                self.add_or_refresh_radar_layer_archive_window(
                    site, start_utc, end_utc, max_frames, ctx,
                );
                self.sync_radar_overlay_layers_to_timeline(ctx);
            }
        }
    }

    pub(crate) fn add_or_refresh_radar_layer(&mut self, site: RadarSite, ctx: &egui::Context) {
        if let Some(index) = self
            .radar_layers
            .iter()
            .position(|layer| layer.site.level2_id == site.level2_id)
        {
            let layer = &mut self.radar_layers[index];
            layer.visible = true;
            layer.timeline_sync = false;
            layer.selected_cut = None;
            // GO LIVE is an explicit feed switch (spec §2). Same-site
            // live→live keeps the loop; archive→live clears the engine's
            // loop state (the incoming latest replaces it wholesale either
            // way — legacy replace-all). The DISPLAY deliberately survives:
            // same-site refresh keeps the existing texture until the
            // replacement render lands (pinned legacy behavior).
            let _ = layer
                .engine
                .set_feed(FeedSource::Live(data_source::sites::SiteRef::Us {
                    level2_id: site.level2_id.clone(),
                }));
            if layer.load_receiver.is_none() {
                Self::start_radar_layer_load(layer, LatestLoadMode::User, ctx);
            }
            self.status = format!("Refreshing overlay {}", site.level2_id);
            return;
        }

        if self.radar_layers.len() >= MAX_RADAR_OVERLAY_LAYERS {
            let remove_index = self
                .radar_layers
                .iter()
                .position(|layer| !layer.visible)
                .unwrap_or(0);
            self.radar_layers.remove(remove_index);
        }

        let id = self.next_radar_layer_id;
        self.next_radar_layer_id = self.next_radar_layer_id.saturating_add(1);
        let mut layer = OverlayView::new(id, site.clone());
        Self::start_radar_layer_load(&mut layer, LatestLoadMode::User, ctx);
        self.status = format!("Added overlay {}", site.level2_id);
        self.radar_layers.push(layer);
    }

    fn add_or_refresh_radar_layer_archive_window(
        &mut self,
        site: RadarSite,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
        max_frames: usize,
        ctx: &egui::Context,
    ) {
        if let Some(index) = self
            .radar_layers
            .iter()
            .position(|layer| layer.site.level2_id == site.level2_id)
        {
            let layer = &mut self.radar_layers[index];
            layer.visible = true;
            Self::start_radar_layer_archive_window_load(layer, start_utc, end_utc, max_frames, ctx);
            self.status = format!("Loading synced overlay loop {}", site.level2_id);
            return;
        }

        if self.radar_layers.len() >= MAX_RADAR_OVERLAY_LAYERS {
            let remove_index = self
                .radar_layers
                .iter()
                .position(|layer| !layer.visible)
                .unwrap_or(0);
            self.radar_layers.remove(remove_index);
        }

        let id = self.next_radar_layer_id;
        self.next_radar_layer_id = self.next_radar_layer_id.saturating_add(1);
        let mut layer = OverlayView::new(id, site.clone());
        Self::start_radar_layer_archive_window_load(
            &mut layer, start_utc, end_utc, max_frames, ctx,
        );
        self.status = format!("Added synced overlay loop {}", site.level2_id);
        self.radar_layers.push(layer);
    }

    /// Ctrl+right-click on (or near) an international marker: add that
    /// site as a radar overlay layer, mirroring the US multi-radar flow
    /// (field request). Dedupe by provider/site; respects the layer cap.
    pub(crate) fn add_or_refresh_intl_radar_layer(
        &mut self,
        intl: &data_source::international::IntlSite,
        ctx: &egui::Context,
    ) {
        if let Some(index) = self.radar_layers.iter().position(|layer| {
            layer.intl_site_ref() == Some((intl.provider_id, intl.site_id.as_str()))
        }) {
            let layer = &mut self.radar_layers[index];
            layer.visible = true;
            layer.timeline_sync = false;
            layer.selected_cut = None;
            // GO LIVE is an explicit feed switch (spec §2): a same-site
            // live restart keeps the loop; a Mosaic archive window on this
            // site switches back to live and clears the engine loop state.
            let _ = layer
                .engine
                .set_feed(FeedSource::Live(data_source::sites::SiteRef::Intl {
                    provider_id: intl.provider_id.to_owned(),
                    site_id: intl.site_id.clone(),
                }));
            if layer.intl_rx.is_none() {
                Self::start_intl_radar_layer_load(layer, ctx);
            }
            self.status = format!("Refreshing overlay {}", layer.site.level2_id);
            return;
        }

        if self.radar_layers.len() >= MAX_RADAR_OVERLAY_LAYERS {
            let remove_index = self
                .radar_layers
                .iter()
                .position(|layer| !layer.visible)
                .unwrap_or(0);
            self.radar_layers.remove(remove_index);
        }

        let id = self.next_radar_layer_id;
        self.next_radar_layer_id = self.next_radar_layer_id.saturating_add(1);
        let site = RadarSite {
            level2_id: intl.label.clone(),
            name: Some(format!("{}/{}", intl.provider_id, intl.site_id)),
            latitude_deg: intl.latitude_deg,
            longitude_deg: intl.longitude_deg,
        };
        let mut layer =
            OverlayView::new_intl(id, site, intl.provider_id.to_owned(), intl.site_id.clone());
        Self::start_intl_radar_layer_load(&mut layer, ctx);
        self.status = format!("Added overlay {}", intl.label);
        self.radar_layers.push(layer);
    }

    /// One catalog-probe + download for an international overlay layer —
    /// the layer-side analog of one intl poll tick. The dedupe key is the
    /// engine's (`live.dedupe_key`, was `IntlOverlayFeed::last_identity`):
    /// an unchanged frame costs one catalog probe and zero downloads.
    pub(crate) fn start_intl_radar_layer_load(layer: &mut OverlayView, ctx: &egui::Context) {
        let Some((provider_id, site_id)) = layer.intl_site_ref() else {
            return;
        };
        let provider_id = provider_id.to_owned();
        let site_id = site_id.to_owned();
        let (sender, receiver) = mpsc::channel();
        layer.intl_rx = Some(receiver);
        layer.engine.live.last_refresh = Some(Instant::now());
        let last_identity = layer.engine.live.dedupe_key.clone();
        layer.engine.status = format!("Loading {}", layer.site.level2_id);
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            let result = fetch_intl_frame(&provider_id, &site_id, last_identity.as_deref());
            let _ = sender.send(result);
            ctx_clone.request_repaint();
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    /// Drain international overlay-layer fetches (the intl analog of
    /// `poll_radar_layer_loads`).
    pub(crate) fn poll_intl_radar_layer_loads(&mut self, ctx: &egui::Context) {
        let mut saw_message = false;
        for layer in &mut self.radar_layers {
            let message = {
                let Some(receiver) = &layer.intl_rx else {
                    continue;
                };
                match receiver.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => continue,
                    Err(mpsc::TryRecvError::Disconnected) => None,
                    Ok(message) => Some(message),
                }
            };
            saw_message = true;
            let label = layer.site.level2_id.clone();
            layer.intl_rx = None;
            match message {
                Some(Ok(Some((identity, volume)))) => {
                    layer.engine.live.dedupe_key = Some(identity.clone());
                    Self::install_radar_layer_volume(
                        layer,
                        DecodedLoad {
                            path: PathBuf::from(format!("intl:{identity}")),
                            volume: Arc::new(volume),
                            timings: LoadTimings::default(),
                            status: FrameStatus::LiveComplete,
                            source_label: identity,
                        },
                    );
                    layer.engine.status = format!("Loaded {label}");
                }
                Some(Ok(None)) => layer.engine.status = format!("Current {label}"),
                Some(Err(err)) => {
                    layer.engine.status = format!("Load failed for {label}: {err}");
                }
                // Census D11: the intl arm's `({label})` disconnect suffix
                // deliberately keeps its wording drift until the two overlay
                // drains unify (the decision folds it in THAT commit).
                None => layer.engine.status = format!("Layer load worker disconnected ({label})"),
            }
        }
        if saw_message {
            ctx.request_repaint();
        }
    }

    pub(crate) fn start_radar_layer_load(
        layer: &mut OverlayView,
        mode: LatestLoadMode,
        ctx: &egui::Context,
    ) {
        layer.timeline_sync = false;
        layer.selected_cut = None;
        let site_id = layer.site.level2_id.clone();
        // The ONE feed-switch entry point (spec §2). AutoRefresh ticks are
        // same-site live→live (KeepHistory); the User arm detaching a
        // timeline overlay is archive→live (ClearAll of engine loop state;
        // the display survives until the replacement install, pinned by
        // same_site_refresh_keeps_existing_texture_until_replacement_render).
        let _ = layer
            .engine
            .set_feed(FeedSource::Live(data_source::sites::SiteRef::Us {
                level2_id: site_id.clone(),
            }));
        let (sender, receiver) = mpsc::channel();
        layer.load_receiver = Some(receiver);
        layer.engine.live.last_refresh = Some(Instant::now());
        layer.engine.status = if mode == LatestLoadMode::AutoRefresh {
            format!("Refreshing {site_id}")
        } else {
            format!("Loading {site_id}")
        };
        let current_source_path = (mode == LatestLoadMode::AutoRefresh)
            .then(|| layer.source_path.clone())
            .flatten();
        spawn_latest_level2_load_worker(
            layer.site.clone(),
            mode,
            current_source_path,
            BTreeSet::new(),
            None,
            1,
            0,
            false,
            sender,
        );
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    fn start_radar_layer_archive_window_load(
        layer: &mut OverlayView,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
        max_frames: usize,
        ctx: &egui::Context,
    ) {
        // Coordinated loops must never keep painting an unrelated previous or
        // final frame while their archive window is loading. The feed switch
        // (any→Archive = ClearAll, spec §2) clears the engine-owned loop
        // state; the display-side clears below are the CALLER's half of the
        // switch contract. Cached files are still read from NVMe, they are
        // simply not skipped as "already present" in an in-memory history we
        // just replaced.
        let site_id = layer.site.level2_id.clone();
        let max_frames = max_frames.clamp(1, MAX_HISTORY_FRAME_LIMIT);
        layer.timeline_sync = true;
        let _ = layer.engine.set_feed(FeedSource::Archive {
            site: data_source::sites::SiteRef::Us {
                level2_id: site_id.clone(),
            },
            window: ArchiveWindow {
                start_utc,
                end_utc,
                anchor_utc: None,
                max_frames,
            },
        });
        layer.selected_cut = None;
        layer.source_path = None;
        layer.volume = None;
        layer.texture = None;
        layer.texture_key = None;
        layer.pending_render_key = None;
        layer.render_ms = None;
        layer.worker_ms = None;
        layer.texture_ms = None;

        let (sender, receiver) = mpsc::channel();
        layer.load_receiver = Some(receiver);
        layer.engine.live.last_refresh = Some(Instant::now());
        layer.engine.status = format!(
            "Loading synced {site_id} {} to {}",
            start_utc.format("%H:%MZ"),
            end_utc.format("%H:%MZ")
        );
        let site_cache = cache_dir(&site_id);
        let known_frame_paths = BTreeSet::new();
        thread::spawn(move || {
            let total_start = Instant::now();
            let label = format!("Overlay loop {site_id}");
            let final_result = (|| -> Result<DecodedLoadBatch, String> {
                let mut dates = Vec::new();
                let mut date = start_utc.date_naive();
                let end_date = end_utc.date_naive();
                while date <= end_date {
                    dates.push(date);
                    let Some(next) = date.succ_opt() else {
                        break;
                    };
                    date = next;
                }
                let mut objects = Vec::new();
                for (index, date) in dates.iter().copied().enumerate() {
                    send_archive_progress(
                        &sender,
                        "Overlay loop",
                        format!("Listing {site_id} {date}"),
                        index,
                        dates.len(),
                    );
                    let listed = data_source::level2_objects_for_date(&site_id, date)
                        .map_err(|err| err.to_string())?;
                    objects.extend(listed.into_iter().filter_map(|object| {
                        let scan_time = archive_object_scan_time_utc(&object)?;
                        (scan_time >= start_utc && scan_time <= end_utc)
                            .then_some((scan_time, object))
                    }));
                }
                if objects.is_empty() {
                    return Err(format!(
                        "no {site_id} archive scans in {} to {}",
                        start_utc.format("%Y-%m-%d %H:%MZ"),
                        end_utc.format("%Y-%m-%d %H:%MZ")
                    ));
                }
                let original_count = objects.len();
                let objects = limit_archive_objects_for_event_loop(objects, max_frames);
                send_archive_progress(
                    &sender,
                    "Overlay loop",
                    format!("Decoding {} of {} scans", objects.len(), original_count),
                    0,
                    objects.len(),
                );
                let decode_objects = objects
                    .into_iter()
                    .enumerate()
                    .map(|(index, (_, object))| (index, object))
                    .collect::<Vec<_>>();
                let (mut decoded_frames, first_error) = load_archive_history_objects_parallel(
                    ArchiveHistoryLoadContext {
                        site_id: &site_id,
                        progress_label: "Overlay loop",
                        site_cache_dir: &site_cache,
                        known_frame_paths: &known_frame_paths,
                        archive_lookup_ms: None,
                        total_start,
                        sender: &sender,
                        progress_done_start: 0,
                        progress_total: decode_objects.len(),
                    },
                    decode_objects,
                );
                decoded_frames.sort_by_key(|decoded| decoded.volume.volume_time);
                if decoded_frames.is_empty() {
                    return Err(first_error.unwrap_or_else(|| {
                        format!("no displayable {site_id} scans decoded for overlay loop")
                    }));
                }
                let selected_index = decoded_frames.len() - 1;
                Ok(DecodedLoadBatch {
                    frames: decoded_frames,
                    selected_index,
                })
            })();
            let _ = sender.send(AsyncLoadResult {
                label,
                update: AsyncLoadUpdate::Final(final_result),
            });
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    /// Mosaic/coordinated archive window for an INTERNATIONAL overlay
    /// layer (v0.29 Phase 4c: coordinated loads work PER-SOURCE — spec §7
    /// 4c, "Mosaic near Copenhagen loads ORD sites as overlay layers").
    /// Callers gate on `archive_browser::archive_access` and only route
    /// `ArchiveAccess::Provider` sites here; the rest grey honestly.
    /// Field arguments, not `IntlSite`: callers hold either an `IntlSite`
    /// (coordinated input) or a union-catalog `SiteRecord` (Mosaic) and
    /// both carry exactly these.
    #[allow(clippy::too_many_arguments)]
    fn add_or_refresh_intl_radar_layer_archive_window(
        &mut self,
        provider_id: &str,
        site_id: &str,
        label: &str,
        latitude_deg: Option<f32>,
        longitude_deg: Option<f32>,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
        max_frames: usize,
        ctx: &egui::Context,
    ) {
        if let Some(index) = self
            .radar_layers
            .iter()
            .position(|layer| layer.intl_site_ref() == Some((provider_id, site_id)))
        {
            let layer = &mut self.radar_layers[index];
            layer.visible = true;
            Self::start_intl_radar_layer_archive_window_load(
                layer, start_utc, end_utc, max_frames, ctx,
            );
            self.status = format!("Loading synced overlay loop {}", layer.site.level2_id);
            return;
        }

        if self.radar_layers.len() >= MAX_RADAR_OVERLAY_LAYERS {
            let remove_index = self
                .radar_layers
                .iter()
                .position(|layer| !layer.visible)
                .unwrap_or(0);
            self.radar_layers.remove(remove_index);
        }

        let id = self.next_radar_layer_id;
        self.next_radar_layer_id = self.next_radar_layer_id.saturating_add(1);
        let site = RadarSite {
            level2_id: label.to_owned(),
            name: Some(format!("{provider_id}/{site_id}")),
            latitude_deg,
            longitude_deg,
        };
        let mut layer = OverlayView::new_intl(id, site, provider_id.to_owned(), site_id.to_owned());
        Self::start_intl_radar_layer_archive_window_load(
            &mut layer, start_utc, end_utc, max_frames, ctx,
        );
        self.status = format!("Added synced overlay loop {label}");
        self.radar_layers.push(layer);
    }

    /// Archive-window load for an international overlay layer through the
    /// provider's `archive_source()` — the intl sibling of
    /// `start_radar_layer_archive_window_load`. It shares the same drain
    /// (`poll_radar_layer_loads`: ArchiveProgress + Final arms) and the
    /// archive_browser worker body, whose window thinning applies census
    /// D15 even sampling wherever frame stamps parse.
    fn start_intl_radar_layer_archive_window_load(
        layer: &mut OverlayView,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
        max_frames: usize,
        ctx: &egui::Context,
    ) {
        let Some((provider_id, site_id)) = layer
            .intl_site_ref()
            .map(|(provider, site)| (provider.to_owned(), site.to_owned()))
        else {
            return;
        };
        let label_site = layer.site.level2_id.clone();
        let max_frames = max_frames.clamp(1, MAX_HISTORY_FRAME_LIMIT);
        // Same switch contract as the US arm: any→Archive clears the
        // engine loop state; the display-side clears are the caller's half.
        layer.timeline_sync = true;
        let _ = layer.engine.set_feed(FeedSource::Archive {
            site: data_source::sites::SiteRef::Intl {
                provider_id: provider_id.clone(),
                site_id: site_id.clone(),
            },
            window: ArchiveWindow {
                start_utc,
                end_utc,
                anchor_utc: None,
                max_frames,
            },
        });
        layer.selected_cut = None;
        layer.source_path = None;
        layer.volume = None;
        layer.texture = None;
        layer.texture_key = None;
        layer.pending_render_key = None;
        layer.render_ms = None;
        layer.worker_ms = None;
        layer.texture_ms = None;

        let (sender, receiver) = mpsc::channel();
        layer.load_receiver = Some(receiver);
        layer.engine.live.last_refresh = Some(Instant::now());
        layer.engine.status = format!(
            "Loading synced {label_site} {} to {}",
            start_utc.format("%H:%MZ"),
            end_utc.format("%H:%MZ")
        );
        let worker_label = format!("Overlay loop {label_site}");
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            archive_browser::fetch_intl_archive_window_batch(
                &provider_id,
                &site_id,
                start_utc,
                end_utc,
                max_frames,
                &worker_label,
                &sender,
            );
            ctx_clone.request_repaint();
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    pub(crate) fn poll_radar_layer_loads(&mut self, ctx: &egui::Context) {
        let mut saw_message = false;
        for layer in &mut self.radar_layers {
            while let Some(result) = layer.load_receiver.as_ref().map(mpsc::Receiver::try_recv) {
                match result {
                    Ok(message) => {
                        saw_message = true;
                        match message.update {
                            AsyncLoadUpdate::ArchiveProgress(progress) => {
                                layer.engine.status = progress.status_text();
                            }
                            AsyncLoadUpdate::Preview(decoded) => {
                                // Census D14 (decided): previews are
                                // DISPLAY-ONLY in every role — they never
                                // enter the engine history.
                                Self::install_radar_layer_preview(layer, decoded);
                                layer.engine.status = format!("Preview {}", message.label);
                            }
                            AsyncLoadUpdate::History(batch, select_frame) => {
                                if select_frame {
                                    Self::install_radar_layer_history(layer, batch);
                                    layer.engine.status = format!("Loaded {}", message.label);
                                } else {
                                    // Census D13 (conscious normalize of dead
                                    // behavior): a no-select backfill batch
                                    // INSTALLS with the P1/P2 semantics —
                                    // upsert into the existing history, no
                                    // selection — instead of being silently
                                    // dropped. Overlay workers never send it
                                    // today (live_preload_frame_count = 0,
                                    // pinned by live_preload_only_applies_
                                    // to_explicit_latest_loads).
                                    let active = layer.volume.clone();
                                    let outcome = layer.engine.install_batch(
                                        batch,
                                        &SelectionPolicy::Backfill,
                                        active.as_ref(),
                                        |_| false,
                                    );
                                    if let Some(clear) = outcome.cross_site_clear {
                                        // Census D3: the engine's cross-site
                                        // diagnostic surfaces on the layer's
                                        // local status, greppable-identical.
                                        layer.engine.status = clear.diagnostic;
                                    }
                                }
                            }
                            AsyncLoadUpdate::Unchanged { timings, reason } => {
                                if let Some(timings) = timings {
                                    layer.load_timing = Some(timings);
                                }
                                layer.load_receiver = None;
                                layer.engine.status =
                                    format!("Current {} ({reason})", message.label);
                                break;
                            }
                            AsyncLoadUpdate::Final(result) => {
                                layer.load_receiver = None;
                                match result {
                                    Ok(batch) => {
                                        Self::install_radar_layer_history(layer, batch);
                                        layer.engine.status = format!("Loaded {}", message.label);
                                    }
                                    Err(err) => {
                                        layer.engine.status =
                                            format!("Load failed for {}: {err}", message.label);
                                    }
                                }
                                break;
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        layer.load_receiver = None;
                        layer.engine.status = "Layer load worker disconnected".to_owned();
                        saw_message = true;
                        break;
                    }
                }
            }
        }

        if saw_message {
            self.sync_radar_overlay_layers_to_timeline(ctx);
            ctx.request_repaint();
        } else if self
            .radar_layers
            .iter()
            .any(|layer| layer.load_receiver.is_some())
        {
            ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
        }
    }

    fn install_radar_layer_volume(layer: &mut OverlayView, decoded: DecodedLoad) {
        Self::install_radar_layer_history(layer, DecodedLoadBatch::single(decoded));
    }

    /// The legacy overlay REPLACE-ALL install, spelled per census D1 as an
    /// explicit `clear_history()` + one engine `install_batch` — never a
    /// fourth upsert mode. Selection policy per call-site role (census D5):
    /// a plain overlay selects the batch anchor (`SelectAnchor`, no
    /// blank-display escape — that is Primary-only); a coordinated
    /// timeline overlay holds its cursor (`KeepCursor`) and lets
    /// `sync_radar_overlay_layers_to_timeline` re-select by time in the
    /// same drain pass.
    fn install_radar_layer_history(layer: &mut OverlayView, batch: DecodedLoadBatch) {
        if batch.frames.is_empty() {
            return;
        }
        layer.engine.clear_history();
        let policy = if layer.timeline_sync {
            SelectionPolicy::KeepCursor
        } else {
            SelectionPolicy::SelectAnchor {
                blank_display_overrides_browsing: false,
            }
        };
        // Replace-all makes the cross-site guard trivially quiet (census D3
        // row P4): the history was just cleared and no active volume is
        // passed. The guard's live overlay surface is the D13 backfill arm
        // in `poll_radar_layer_loads`.
        let outcome = layer.engine.install_batch(batch, &policy, None, |_| false);
        if let Some(clear) = outcome.cross_site_clear {
            layer.engine.status = clear.diagnostic;
        }
        if let InstallSelection::SelectedAnchor { index } = outcome.selection {
            Self::select_radar_layer_history_frame(layer, index);
        }
    }

    /// Census D14 (decided 2026-07-02): previews are DISPLAY-ONLY in every
    /// role. This writes the view's display state — exactly the display
    /// half of [`Self::select_radar_layer_history_frame`] — and never
    /// touches `engine.history` (the legacy overlay preview replaced the
    /// whole history, an artifact of the overlay having had no separate
    /// display-install path).
    fn install_radar_layer_preview(layer: &mut OverlayView, decoded: DecodedLoad) {
        let retained_texture = layer.timeline_sync.then(|| layer.texture.clone()).flatten();
        let retained_texture_key = retained_texture
            .as_ref()
            .and_then(|_| layer.texture_key.clone());
        layer.selected_cut = None;
        layer.source_path = Some(decoded.path);
        layer.load_timing = Some(decoded.timings);
        layer.volume = Some(decoded.volume);
        layer.texture = retained_texture;
        layer.texture_key = retained_texture_key;
        layer.pending_render_key = None;
        if !layer.timeline_sync || layer.texture.is_none() {
            layer.render_ms = None;
            layer.worker_ms = None;
            layer.texture_ms = None;
        }
    }

    fn select_radar_layer_history_frame(layer: &mut OverlayView, index: usize) -> bool {
        let Some(frame) = layer.engine.history.get(index).cloned() else {
            return false;
        };
        let retained_texture = layer.timeline_sync.then(|| layer.texture.clone()).flatten();
        let retained_texture_key = retained_texture
            .as_ref()
            .and_then(|_| layer.texture_key.clone());
        layer.engine.cursor.index = index;
        layer.selected_cut = None;
        layer.source_path = Some(frame.path);
        layer.load_timing = frame.timings;
        layer.volume = Some(frame.volume);
        layer.texture = retained_texture;
        layer.texture_key = retained_texture_key;
        layer.pending_render_key = None;
        if !layer.timeline_sync || layer.texture.is_none() {
            layer.render_ms = None;
            layer.worker_ms = None;
            layer.texture_ms = None;
        }
        true
    }

    fn set_radar_layer_selected_cut(layer: &mut OverlayView, cut: usize) -> bool {
        if layer.selected_cut == Some(cut) {
            return false;
        }
        let retained_texture = layer.timeline_sync.then(|| layer.texture.clone()).flatten();
        let retained_texture_key = retained_texture
            .as_ref()
            .and_then(|_| layer.texture_key.clone());
        layer.selected_cut = Some(cut);
        layer.texture = retained_texture;
        layer.texture_key = retained_texture_key;
        layer.pending_render_key = None;
        if !layer.timeline_sync || layer.texture.is_none() {
            layer.render_ms = None;
            layer.worker_ms = None;
            layer.texture_ms = None;
        }
        true
    }

    fn clear_radar_layer_timeline_display(layer: &mut OverlayView) -> bool {
        let changed = layer.volume.is_some()
            || layer.texture.is_some()
            || layer.texture_key.is_some()
            || layer.pending_render_key.is_some()
            || layer.selected_cut.is_some();
        layer.source_path = None;
        layer.volume = None;
        layer.selected_cut = None;
        layer.texture = None;
        layer.texture_key = None;
        layer.pending_render_key = None;
        layer.render_ms = None;
        layer.worker_ms = None;
        layer.texture_ms = None;
        changed
    }

    pub(crate) fn sync_radar_overlay_layers_to_timeline(&mut self, ctx: &egui::Context) {
        let Some(target_utc) = self.displayed_timeline_time_utc() else {
            return;
        };
        let product = self.selected_product.clone();
        let policy = self.primary_sweep_policy_for_product(&product);
        let use_sweep_policy =
            self.app_settings.loop_low_sweeps && policy.mode != SweepPolicyMode::Off;
        let disabled_cuts = &self.low_sweep_disabled_cuts;
        let primary_elevation = self
            .volume
            .as_ref()
            .and_then(|volume| volume.cuts.get(self.selected_cut))
            .map(|cut| cut.elevation_deg)
            .filter(|elevation| elevation.is_finite());

        let mut changed = false;
        for layer in &mut self.radar_layers {
            if !layer.timeline_sync {
                continue;
            }

            let selection = if use_sweep_policy {
                sweep_history_cut_at_or_before_near_elevation(
                    &layer.engine.history,
                    &product,
                    policy,
                    disabled_cuts,
                    target_utc,
                    primary_elevation,
                )
                .and_then(|(frame_index, cut)| {
                    let frame = layer.engine.history.get(frame_index)?;
                    let observation_time = cut_start_time_utc(frame.volume.as_ref(), cut)
                        .unwrap_or(frame.identity.scan_time_utc);
                    coordinated_observation_is_usable(observation_time, target_utc).then_some((
                        frame_index,
                        cut,
                        observation_time,
                    ))
                })
            } else {
                history_cut_at_or_before_near_elevation(
                    &layer.engine.history,
                    &product,
                    target_utc,
                    primary_elevation,
                )
            };

            let Some((frame_index, cut, observation_time)) = selection else {
                if Self::clear_radar_layer_timeline_display(layer) {
                    changed = true;
                }
                layer.engine.status = format!(
                    "No {} scan at-or-before {}",
                    layer.site.level2_id,
                    target_utc.format("%H:%M:%SZ")
                );
                continue;
            };

            let selected_volume_matches = layer
                .volume
                .as_ref()
                .zip(layer.engine.history.get(frame_index))
                .is_some_and(|(selected, frame)| Arc::ptr_eq(selected, &frame.volume));
            if (layer.engine.cursor.index != frame_index || !selected_volume_matches)
                && Self::select_radar_layer_history_frame(layer, frame_index)
            {
                changed = true;
            }
            if Self::set_radar_layer_selected_cut(layer, cut) {
                changed = true;
            }
            layer.engine.status = format!(
                "Synced {} {}/{} · {}",
                layer.site.level2_id,
                frame_index + 1,
                layer.engine.history.len(),
                observation_time.format("%H:%M:%SZ")
            );
        }
        if changed {
            ctx.request_repaint();
        }
    }

    /// `LaneId::Overlay`: since the Phase-4b pool flip every radar overlay
    /// layer renders through the ONE shared pool, so the drain is one pass
    /// over the pool's single result channel, routing each result to its
    /// layer by lane id. The budget is unchanged from 4a's pins: ONE clock
    /// shared across all overlay layers, so a burst on one layer spills
    /// every later result to the next frame.
    pub(crate) fn drain_overlay_render_lanes(&mut self, ctx: &egui::Context) {
        // The overlay budget is per-pass (shared by every layer); the lane
        // id passed here does not affect it.
        let mut budget = DrainBudget::for_lane(LaneId::Overlay(0));
        loop {
            if budget.should_stop() {
                ctx.request_repaint();
                break;
            }
            match self.overlay_render_pool.try_recv() {
                Ok(message) => {
                    budget.note_message();
                    self.install_overlay_render_result(ctx, message);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Unreachable while the pool is alive (it keeps a result
                    // sender for future workers); kept so a future pool
                    // change fails loud instead of repaint-spinning.
                    for layer in &mut self.radar_layers {
                        if layer.pending_render_key.take().is_some() {
                            layer.engine.status = "Layer render worker disconnected".to_owned();
                        }
                    }
                    budget.note_message();
                    break;
                }
            }
        }

        match post_drain_repaint(
            budget.saw_message(),
            self.radar_layers
                .iter()
                .any(|layer| layer.pending_render_key.is_some()),
        ) {
            RepaintDecision::Now => ctx.request_repaint(),
            RepaintDecision::PollSoon => {
                ctx.request_repaint_after(Duration::from_millis(RENDER_RESULT_POLL_MS));
            }
            RepaintDecision::Idle => {}
        }
    }

    /// Route one shared-pool result to its overlay layer by lane id. The
    /// accept rules are the pre-4b per-layer drain arms, verbatim: latest-ok
    /// installs, stale-ok recycles, latest-err clears pending + layer
    /// status, stale-err drops. A retired lane (layer removed while its
    /// render was in flight) misses the lookup and the buffer recycles.
    fn install_overlay_render_result(&mut self, ctx: &egui::Context, message: AsyncRenderResult) {
        let recycle = |rendered: RenderedTexture| {
            let _ = self
                .overlay_render_recycle_sender
                .send(RenderRecycleBuffer {
                    rgba: rendered.rgba,
                    signature: Some(rendered.buffer_signature),
                });
        };
        let LaneId::Overlay(layer_id) = message.lane else {
            debug_assert!(false, "non-overlay lane result on the overlay pool");
            if let Ok(rendered) = message.result {
                recycle(rendered);
            }
            return;
        };
        let Some(layer) = self
            .radar_layers
            .iter_mut()
            .find(|layer| layer.engine.id.0 == layer_id)
        else {
            if let Ok(rendered) = message.result {
                recycle(rendered);
            }
            return;
        };
        let is_latest = layer.pending_render_key.as_ref() == Some(&message.key);
        match message.result {
            Ok(rendered) if is_latest => {
                layer.pending_render_key = None;
                Self::install_radar_layer_texture(
                    ctx,
                    layer,
                    &self.overlay_render_recycle_sender,
                    message.key,
                    rendered,
                );
            }
            Ok(rendered) => recycle(rendered),
            Err(err) if is_latest => {
                layer.pending_render_key = None;
                layer.render_ms = None;
                layer.worker_ms = None;
                layer.texture_ms = None;
                layer.engine.status = format!("Render failed: {err}");
            }
            Err(_) => {}
        }
    }

    fn install_radar_layer_texture(
        ctx: &egui::Context,
        layer: &mut OverlayView,
        recycle_sender: &mpsc::Sender<RenderRecycleBuffer>,
        key: TextureKey,
        rendered: RenderedTexture,
    ) {
        let RenderedTexture {
            width,
            height,
            rgba,
            buffer_signature,
            render_ms,
            worker_ms,
            radar_range_km,
            ..
        } = rendered;
        let texture_start = Instant::now();
        let color_image = radar_color_image_from_rgba([width, height], &rgba);
        let can_update_texture = layer
            .texture_key
            .as_ref()
            .is_some_and(|old_key| old_key.viewport.dimensions() == key.viewport.dimensions());
        if can_update_texture && let Some(texture) = &mut layer.texture {
            texture.set(color_image, radar_texture_options());
        } else {
            layer.texture = Some(ctx.load_texture(
                format!(
                    "radar-layer-{}-{}-{}-{}x{}",
                    layer.engine.id.0,
                    key.cut,
                    key.product.label(),
                    key.viewport.width,
                    key.viewport.height
                ),
                color_image,
                radar_texture_options(),
            ));
        }
        layer.texture_key = Some(key);
        layer.render_ms = Some(render_ms);
        layer.worker_ms = Some(worker_ms);
        layer.texture_ms = Some(texture_start.elapsed().as_secs_f32() * 1000.0);
        layer.radar_range_km = radar_range_km;
        let _ = recycle_sender.send(RenderRecycleBuffer {
            rgba,
            signature: Some(buffer_signature),
        });
        if layer.load_receiver.is_none() {
            layer.engine.status = "Rendered".to_owned();
        }
    }

    /// Grow the shared overlay pool to `overlay_pool_worker_target` for the
    /// current layer count (K never shrinks; the bound is the prewarm sizing
    /// table). Called only when there are overlay render requests to send,
    /// so a session that never adds an overlay spawns no pool threads.
    fn ensure_overlay_render_workers(&mut self) {
        let target =
            overlay_pool_worker_target(self.radar_layers.len(), effective_worker_threads());
        if self.overlay_render_pool.worker_count() >= target {
            return;
        }
        let recycle_receiver = Arc::clone(&self.overlay_render_recycle_receiver);
        self.overlay_render_pool.ensure_workers(target, move || {
            overlay_pool_render_job(Arc::clone(&recycle_receiver))
        });
    }

    pub(crate) fn request_radar_layer_renders(&mut self, ctx: &egui::Context, rect: egui::Rect) {
        self.sync_radar_overlay_layers_to_timeline(ctx);
        let smooth_follow = self.smooth_camera_follow_playback_active();
        let mut requests = Vec::new();
        let mut clear_pending = Vec::new();
        for (index, layer) in self.radar_layers.iter().enumerate() {
            if !layer.visible {
                continue;
            }
            let Some(volume) = layer.volume.clone() else {
                continue;
            };
            let Some((radar_lat, radar_lon)) = layer.radar_location() else {
                continue;
            };
            let Some((viewport_options, viewport_key)) =
                self.viewport_raster_options_for_location(ctx, rect, radar_lat, radar_lon)
            else {
                continue;
            };
            let product = self.selected_product.clone();
            let cut = if layer.timeline_sync {
                layer
                    .selected_cut
                    .filter(|cut| is_displayable_on_cut(volume.as_ref(), *cut, &product))
            } else {
                best_cut_for_product(volume.as_ref(), self.selected_cut, &product)
            };
            let Some(cut) = cut else {
                continue;
            };
            let color_tables = self.render_color_tables_for_product(&product);
            let color_table_signature = color_tables.signature_for_family(product.color_family());
            let render_dealiased_velocity = self.product_render_uses_dealiased_velocity(&product);
            let smoothing = self.smoothing_for_product(&product);
            // Same read-only RAP-profile lookup as the primary/pane request
            // builders; overlay sites without a fetched profile (the usual
            // case — fetches follow the DISPLAYED site) run v4's no-env path,
            // exactly like the always-None previous_volume below.
            let dealias_env = (self.dealias_cascade && render_dealiased_velocity)
                .then(|| self.dealias_env_profile_for_volume(volume.as_ref()))
                .flatten();
            let dealias_env_ptr = dealias_env
                .as_ref()
                .map(|profile| Arc::as_ptr(profile) as usize)
                .unwrap_or(0);
            let key = TextureKey {
                volume_ptr: Arc::as_ptr(&volume) as usize,
                dealias_reference_volume_ptr: 0,
                dealias_env_ptr,
                cut,
                product: product.clone(),
                render_dealiased_velocity,
                color_table_signature,
                storm_motion_key: self.storm_motion_key(),
                hail_levels_key: self.hail_levels_key(),
                smoothing,
                dealias_cascade: self.dealias_cascade,
                gate_filter_decidbz: self.gate_filter_key(),
                viewport: viewport_key,
            };
            if layer.texture_key.as_ref() == Some(&key) {
                continue;
            }
            if smooth_follow
                && layer
                    .texture_key
                    .as_ref()
                    .is_some_and(|visible| texture_keys_match_data_and_style(visible, &key))
            {
                if layer
                    .pending_render_key
                    .as_ref()
                    .is_some_and(|pending| !texture_keys_match_data_and_style(pending, &key))
                {
                    clear_pending.push(index);
                }
                continue;
            }
            if layer.pending_render_key.as_ref() == Some(&key)
                || (smooth_follow
                    && layer
                        .pending_render_key
                        .as_ref()
                        .is_some_and(|pending| texture_keys_match_data_and_style(pending, &key)))
            {
                continue;
            }
            let radar_range_km = selected_grid_range_km_for(volume.as_ref(), cut, &product)
                .unwrap_or(DEFAULT_RADAR_RANGE_KM);
            requests.push((
                index,
                RenderRequest {
                    key,
                    lane: LaneId::Overlay(layer.engine.id.0),
                    volume,
                    previous_volume: None,
                    dealias_env,
                    cut,
                    product,
                    render_dealiased_velocity,
                    plain_velocity_render_dealiased: self.unfold_velocity_display,
                    color_tables,
                    storm_motion: self.current_storm_motion(),
                    hail_levels_m: self.hail_levels_m(),
                    smoothing,
                    dealias_cascade: self.dealias_cascade,
                    gate_filter_decidbz: self.gate_filter_key(),
                    viewport_options,
                    radar_range_km,
                },
            ));
        }

        for index in clear_pending {
            if let Some(layer) = self.radar_layers.get_mut(index) {
                layer.pending_render_key = None;
            }
        }

        if !requests.is_empty() {
            self.ensure_overlay_render_workers();
        }
        for (index, request) in requests {
            if let Some(layer) = self.radar_layers.get_mut(index) {
                let key = request.key.clone();
                // The routing policy for overlay lanes lives in ONE place:
                // ui_core::render_service::render_route_for (the Phase-4b
                // flip edited that map; its doc names the revert path).
                debug_assert_eq!(render_route_for(request.lane), RenderRoute::OverlayPool);
                match self.overlay_render_pool.submit(request.lane, request) {
                    Ok(()) => {
                        layer.pending_render_key = Some(key);
                        if layer.load_receiver.is_none() {
                            layer.engine.status = "Rendering".to_owned();
                        }
                    }
                    Err(_) => {
                        layer.pending_render_key = None;
                        layer.engine.status = "Layer render worker disconnected".to_owned();
                    }
                }
            }
        }

        if self
            .radar_layers
            .iter()
            .any(|layer| layer.pending_render_key.is_some())
        {
            ctx.request_repaint_after(Duration::from_millis(RENDER_RESULT_POLL_MS));
        }
    }

    pub(crate) fn populate_unified_player_nearby_sites(&mut self) {
        let radius = self
            .unified_player
            .coordinated_site_radius_km
            .clamp(25.0, 460.0);
        let candidates = self.nearby_coordinated_overlay_sites(8);
        if candidates.is_empty() {
            self.unified_player
                .mark_status(format!("No radar sites within {:.0} km", radius));
            return;
        }
        // Both worlds encode as settings keys: bare US ids as ever,
        // `intl:{provider}:{site}` case-preserved (the parser leaves
        // `:`-keys verbatim).
        self.unified_player.coordinated_sites_input = candidates
            .iter()
            .map(|(_, record)| record.site.settings_key())
            .collect::<Vec<_>>()
            .join(", ");
        self.unified_player
            .mark_status(format!("Found {} nearby radar sites", candidates.len()));
    }

    fn nearby_coordinated_overlay_sites(
        &self,
        max_count: usize,
    ) -> Vec<(f32, data_source::sites::SiteRecord)> {
        let (lat, lon) = self
            .radar_location()
            .unwrap_or((self.map_center_lat, self.map_center_lon));
        let radius = self
            .unified_player
            .coordinated_site_radius_km
            .clamp(25.0, 460.0);
        // Primary exclusion by SiteRef equality: an INTL primary now
        // excludes itself too (impossible under the old US-id compare).
        let primary = self.display_owner_site();
        let mut candidates: Vec<(f32, data_source::sites::SiteRecord)> =
            data_source::sites::sites_near(lat, lon, radius)
                .into_iter()
                .filter_map(|(record, distance_km)| {
                    // Union candidate pool, tagged by kind (v0.29 Phase 3):
                    // archive-backed WSR-88Ds plus international sites —
                    // archive-capable providers window-load per source
                    // (Phase 4c); the rest grey "newest scan only" at
                    // dispatch. TDWRs' ~90 km range and research feeds'
                    // live-only serving keep them out as ever.
                    match record.kind {
                        SiteKind::Wsr88d | SiteKind::Intl { .. } => {}
                        SiteKind::Tdwr | SiteKind::Research => return None,
                    }
                    if record.site == primary {
                        return None;
                    }
                    Some((distance_km, record))
                })
                .collect();
        candidates.truncate(max_count);
        candidates
    }

    pub(crate) fn add_unified_player_coordinated_site_overlays(&mut self, ctx: &egui::Context) {
        let (sites, skipped_primary, missing, had_input) =
            self.resolve_unified_player_coordinated_overlay_sites();
        if !had_input {
            self.unified_player
                .mark_status("Enter radar site IDs or use Find nearby first");
            return;
        }
        let mode = self.coordinated_overlay_load_mode();
        let mut added = 0usize;
        let mut intl_window_skipped: Vec<String> = Vec::new();
        for site in sites {
            match (site, mode) {
                (CoordinatedOverlaySite::Us(site), CoordinatedOverlayLoadMode::Live) => {
                    self.add_or_refresh_radar_layer(site, ctx);
                    added += 1;
                }
                (
                    CoordinatedOverlaySite::Us(site),
                    CoordinatedOverlayLoadMode::ArchiveWindow {
                        start_utc,
                        end_utc,
                        max_frames,
                    },
                ) => {
                    self.add_or_refresh_radar_layer_archive_window(
                        site, start_utc, end_utc, max_frames, ctx,
                    );
                    added += 1;
                }
                (CoordinatedOverlaySite::Intl(site), CoordinatedOverlayLoadMode::Live) => {
                    self.add_or_refresh_intl_radar_layer(&site, ctx);
                    added += 1;
                }
                (
                    CoordinatedOverlaySite::Intl(site),
                    CoordinatedOverlayLoadMode::ArchiveWindow {
                        start_utc,
                        end_utc,
                        max_frames,
                    },
                ) => {
                    // Per-source window loads (v0.29 Phase 4c): providers
                    // with an archive_source() window-load like US sites;
                    // the rest keep the honest newest-scan-only limit. The
                    // gate and the greyed reason are the SAME derived
                    // capability call (spec §1.3 ArchiveAccess).
                    let site_ref = data_source::sites::SiteRef::Intl {
                        provider_id: site.provider_id.to_owned(),
                        site_id: site.site_id.clone(),
                    };
                    match archive_browser::archive_access(&site_ref) {
                        archive_browser::ArchiveAccess::Level2S3
                        | archive_browser::ArchiveAccess::Provider => {
                            self.add_or_refresh_intl_radar_layer_archive_window(
                                site.provider_id,
                                &site.site_id,
                                &site.label,
                                site.latitude_deg,
                                site.longitude_deg,
                                start_utc,
                                end_utc,
                                max_frames,
                                ctx,
                            );
                            added += 1;
                        }
                        archive_browser::ArchiveAccess::None { .. } => {
                            intl_window_skipped.push(site.label.clone());
                        }
                    }
                }
            }
        }
        if missing.is_empty() && !skipped_primary && intl_window_skipped.is_empty() {
            self.unified_player
                .mark_status(self.coordinated_overlay_status(added, mode));
        } else {
            let mut details = Vec::new();
            if skipped_primary {
                details.push("skipped primary radar".to_owned());
            }
            if !missing.is_empty() {
                details.push(format!("unknown site(s): {}", missing.join(", ")));
            }
            if !intl_window_skipped.is_empty() {
                details.push(format!(
                    "international sites serve newest scans only, skipped for archive windows: {}",
                    intl_window_skipped.join(", ")
                ));
            }
            self.unified_player.mark_status(format!(
                "{}; {}",
                self.coordinated_overlay_status(added, mode),
                details.join("; ")
            ));
        }
    }

    pub(crate) fn sync_unified_player_nearby_radar_loops(&mut self, ctx: &egui::Context) {
        let mode = self.coordinated_overlay_load_mode();
        let CoordinatedOverlayLoadMode::ArchiveWindow {
            start_utc,
            end_utc,
            max_frames,
        } = mode
        else {
            self.unified_player
                .mark_status("Load a multi-frame radar loop before Mosaic 5");
            return;
        };
        let radius = self
            .unified_player
            .coordinated_site_radius_km
            .clamp(25.0, 460.0);
        let candidates = self.nearby_coordinated_overlay_sites(STORM_VIDEO_SYNC_OVERLAY_RADARS);
        // Per-source archive-window loads (v0.29 Phase 4c): US Level-II
        // loops as ever, plus every international candidate whose provider
        // has an archive_source() (ORD, SMHI, …). Providers without one
        // stay honestly out, named with the derived reason below.
        /// One archive-capable international Mosaic candidate — fields
        /// straight off the union catalog's SiteRecord, never a raw
        /// provider-registry iteration.
        struct IntlWindowCandidate {
            provider_id: String,
            site_id: String,
            label: String,
            lat_lon: Option<(f32, f32)>,
        }
        let mut us_sites: Vec<RadarSite> = Vec::new();
        let mut intl_sites: Vec<IntlWindowCandidate> = Vec::new();
        let mut newest_only: Vec<String> = Vec::new();
        for (_, record) in &candidates {
            match &record.site {
                SiteRef::Us { level2_id } => {
                    if let Some(site) = self
                        .sites
                        .iter()
                        .find(|site| site.level2_id.eq_ignore_ascii_case(level2_id))
                        .cloned()
                    {
                        us_sites.push(site);
                    }
                }
                SiteRef::Intl {
                    provider_id,
                    site_id,
                } => match archive_browser::archive_access(&record.site) {
                    archive_browser::ArchiveAccess::Level2S3
                    | archive_browser::ArchiveAccess::Provider => {
                        intl_sites.push(IntlWindowCandidate {
                            provider_id: provider_id.clone(),
                            site_id: site_id.clone(),
                            label: record.label.clone(),
                            lat_lon: record.lat_lon,
                        });
                    }
                    archive_browser::ArchiveAccess::None { .. } => {
                        // Single-frame provider: no archive window to load —
                        // honest "newest scan only" instead of a ghost frame
                        // over the archive loop.
                        newest_only.push(record.label.clone());
                    }
                },
            }
        }
        if us_sites.is_empty() && intl_sites.is_empty() {
            self.unified_player.mark_status(format!(
                "No loop-capable radar sites within {:.0} km",
                radius
            ));
            return;
        }

        let selected_ids = us_sites
            .iter()
            .map(|site| site.level2_id.clone())
            .chain(intl_sites.iter().map(|site| site.label.clone()))
            .collect::<BTreeSet<_>>();
        self.unified_player.coordinated_sites_input = us_sites
            .iter()
            .map(|site| site.level2_id.clone())
            .chain(intl_sites.iter().map(|site| {
                data_source::sites::SiteRef::Intl {
                    provider_id: site.provider_id.clone(),
                    site_id: site.site_id.clone(),
                }
                .settings_key()
            }))
            .collect::<Vec<_>>()
            .join(", ");
        for layer in &mut self.radar_layers {
            if !selected_ids.contains(&layer.site.level2_id) {
                layer.visible = false;
            }
        }
        for site in us_sites {
            self.add_or_refresh_radar_layer_archive_window(
                site, start_utc, end_utc, max_frames, ctx,
            );
        }
        for site in intl_sites {
            self.add_or_refresh_intl_radar_layer_archive_window(
                &site.provider_id,
                &site.site_id,
                &site.label,
                site.lat_lon.map(|(lat, _)| lat),
                site.lat_lon.map(|(_, lon)| lon),
                start_utc,
                end_utc,
                max_frames,
                ctx,
            );
        }
        self.sync_radar_overlay_layers_to_timeline(ctx);
        let total = selected_ids.len() + 1;
        let mut status = format!("Loading {total} synced radar sites for storm video");
        if !newest_only.is_empty() {
            status = format!(
                "{status}; newest scan only (no provider archive): {}",
                newest_only.join(", ")
            );
        }
        self.status = status.clone();
        self.unified_player.mark_status(status);
    }

    fn coordinated_overlay_load_mode(&self) -> CoordinatedOverlayLoadMode {
        let target = self.active_loop_timeline_target();
        if self.loop_timeline_step_count_for_target(target) > 1
            && let Some((start_utc, end_utc)) =
                self.loaded_loop_summary_time_window_for_target(target)
        {
            return CoordinatedOverlayLoadMode::ArchiveWindow {
                start_utc,
                end_utc,
                max_frames: normalized_history_limit(self.primary.limits.frame_limit).max(1),
            };
        }
        CoordinatedOverlayLoadMode::Live
    }

    fn coordinated_overlay_status(&self, count: usize, mode: CoordinatedOverlayLoadMode) -> String {
        match mode {
            CoordinatedOverlayLoadMode::Live => {
                format!("Added/refreshed {count} coordinated radar overlays")
            }
            CoordinatedOverlayLoadMode::ArchiveWindow {
                start_utc, end_utc, ..
            } => format!(
                "Loading {count} synced radar overlay loop(s) {} to {}",
                start_utc.format("%H:%MZ"),
                end_utc.format("%H:%MZ")
            ),
        }
    }

    fn resolve_unified_player_coordinated_overlay_sites(
        &self,
    ) -> (Vec<CoordinatedOverlaySite>, bool, Vec<String>, bool) {
        let site_ids = parse_coordinated_site_ids(&self.unified_player.coordinated_sites_input);
        if site_ids.is_empty() {
            return (Vec::new(), false, Vec::new(), false);
        }
        // Every entry decodes to a SiteRef: bare US ids as ever, `intl:`
        // settings keys case-preserved. The primary skip compares refs, so
        // an intl primary skips itself too.
        let primary = self.display_owner_site();
        let mut sites = Vec::new();
        let mut missing = Vec::new();
        let mut skipped_primary = false;
        for site_id in site_ids {
            let site_ref = SiteRef::parse_settings_key(&site_id);
            if site_ref == primary {
                skipped_primary = true;
                continue;
            }
            match &site_ref {
                SiteRef::Us { level2_id } => {
                    if let Some(site) = self
                        .sites
                        .iter()
                        .find(|site| site.level2_id.eq_ignore_ascii_case(level2_id))
                        .cloned()
                    {
                        sites.push(CoordinatedOverlaySite::Us(site));
                    } else {
                        missing.push(site_id);
                    }
                }
                SiteRef::Intl {
                    provider_id,
                    site_id: intl_site_id,
                } => {
                    if let Some(site) = Self::find_intl_site(provider_id, intl_site_id) {
                        sites.push(CoordinatedOverlaySite::Intl(site));
                    } else {
                        missing.push(site_id);
                    }
                }
            }
        }
        (sites, skipped_primary, missing, true)
    }
}

fn sweep_history_cut_at_or_before_near_elevation(
    frames: &[FrameHistoryEntry],
    product: &DisplayProduct,
    policy: SweepPolicy,
    disabled_cuts: &BTreeSet<LowSweepCutKey>,
    timeline_time: DateTime<Utc>,
    target_elevation_deg: Option<f32>,
) -> Option<(usize, usize)> {
    let Some(target_elevation_deg) = target_elevation_deg else {
        return sweep_history_cut_at_or_before(
            frames,
            product,
            policy,
            disabled_cuts,
            timeline_time,
        );
    };
    let elevation_tolerance = LOW_SWEEP_FILTER_ELEVATION_TOLERANCE_DEG * 1.5;
    let mut matched: Option<(DateTime<Utc>, f32, usize, usize)> = None;
    let mut fallback: Option<(DateTime<Utc>, usize, usize)> = None;
    for (frame_index, frame) in frames.iter().enumerate() {
        for cut in sweep_cuts_for_history_entry(frame, product, policy, disabled_cuts) {
            let cut_time = cut_start_time_utc(frame.volume.as_ref(), cut)
                .unwrap_or(frame.identity.scan_time_utc);
            if cut_time > timeline_time {
                continue;
            }
            if fallback
                .as_ref()
                .is_none_or(|(best_time, _, _)| cut_time > *best_time)
            {
                fallback = Some((cut_time, frame_index, cut));
            }
            let Some(elevation) = frame
                .volume
                .cuts
                .get(cut)
                .map(|cut| cut.elevation_deg)
                .filter(|elevation| elevation.is_finite())
            else {
                continue;
            };
            let delta = (elevation - target_elevation_deg).abs();
            if delta > elevation_tolerance {
                continue;
            }
            let replace = matched
                .as_ref()
                .is_none_or(|(best_time, best_delta, _, _)| {
                    cut_time > *best_time || (cut_time == *best_time && delta < *best_delta)
                });
            if replace {
                matched = Some((cut_time, delta, frame_index, cut));
            }
        }
    }
    matched
        .map(|(_, _, frame_index, cut)| (frame_index, cut))
        .or_else(|| fallback.map(|(_, frame_index, cut)| (frame_index, cut)))
}

fn history_cut_at_or_before_near_elevation(
    frames: &[FrameHistoryEntry],
    product: &DisplayProduct,
    timeline_time: DateTime<Utc>,
    target_elevation_deg: Option<f32>,
) -> Option<(usize, usize, DateTime<Utc>)> {
    let elevation_tolerance = LOW_SWEEP_FILTER_ELEVATION_TOLERANCE_DEG * 1.5;
    let mut matched: Option<(DateTime<Utc>, f32, usize, usize)> = None;
    let mut fallback: Option<(DateTime<Utc>, f32, usize, usize)> = None;

    for (frame_index, frame) in frames.iter().enumerate() {
        for cut in displayable_cuts_for_product(frame.volume.as_ref(), product) {
            let observation_time = cut_start_time_utc(frame.volume.as_ref(), cut)
                .unwrap_or(frame.identity.scan_time_utc);
            if !coordinated_observation_is_usable(observation_time, timeline_time) {
                continue;
            }

            let delta = frame
                .volume
                .cuts
                .get(cut)
                .and_then(|cut| {
                    let target = target_elevation_deg?;
                    cut.elevation_deg
                        .is_finite()
                        .then_some((cut.elevation_deg - target).abs())
                })
                .unwrap_or(f32::INFINITY);

            if fallback
                .as_ref()
                .is_none_or(|(best_time, best_delta, _, _)| {
                    observation_time > *best_time
                        || (observation_time == *best_time && delta < *best_delta)
                })
            {
                fallback = Some((observation_time, delta, frame_index, cut));
            }

            if delta > elevation_tolerance {
                continue;
            }
            if matched
                .as_ref()
                .is_none_or(|(best_time, best_delta, _, _)| {
                    observation_time > *best_time
                        || (observation_time == *best_time && delta < *best_delta)
                })
            {
                matched = Some((observation_time, delta, frame_index, cut));
            }
        }
    }

    matched
        .or(fallback)
        .map(|(observation_time, _, frame_index, cut)| (frame_index, cut, observation_time))
}

fn parse_coordinated_site_ids(input: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    input
        .split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .map(str::trim)
        .filter(|site_id| !site_id.is_empty())
        .map(|site_id| {
            // `intl:{provider}:{site}` settings keys are case-significant
            // and pass through VERBATIM — the same corruption trap the
            // favorites store fixed (spec §1.1); bare US ids keep the
            // uppercase + K-prefix conveniences.
            if site_id.contains(':') {
                return site_id.to_owned();
            }
            let mut site_id = site_id.to_ascii_uppercase();
            if site_id.len() == 3 && site_id.chars().all(|ch| ch.is_ascii_alphabetic()) {
                site_id.insert(0, 'K');
            }
            site_id
        })
        .filter(|site_id| seen.insert(site_id.clone()))
        .collect()
}

fn coordinated_observation_is_usable(
    observation_time: DateTime<Utc>,
    target_utc: DateTime<Utc>,
) -> bool {
    let age_seconds = target_utc
        .signed_duration_since(observation_time)
        .num_seconds();
    (0..=COORDINATED_RADAR_MAX_STALENESS_SECONDS).contains(&age_seconds)
}

#[cfg(test)]
fn history_frame_index_at_or_before(
    history: &[FrameHistoryEntry],
    target_utc: DateTime<Utc>,
    max_staleness_seconds: i64,
) -> Option<usize> {
    let index = history
        .partition_point(|frame| frame.identity.scan_time_utc <= target_utc)
        .checked_sub(1)?;
    let frame_time = history.get(index)?.identity.scan_time_utc;
    let age_seconds = target_utc.signed_duration_since(frame_time).num_seconds();
    (0..=max_staleness_seconds)
        .contains(&age_seconds)
        .then_some(index)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use app_ui::PanelLayout;
    use chrono::TimeZone;
    use radar_core::MomentType;
    use settings::{LoopSweepControl, SweepPolicySet, SweepProductGroup};

    use super::*;
    use crate::tests::{
        add_two_test_history_frames, test_decoded_from_volume,
        test_reflectivity_sails_volume_with_radials, test_screen_texture_key,
        test_velocity_sails_volume_with_radials, test_viewer_app_with_hazards,
        test_viewport_signature, test_volume_with_site_time, upsert_primary_history_frame,
    };
    use ui_core::loop_engine::OVERLAY_REALTIME_LEVEL2_REFRESH_SECONDS;

    use crate::{
        FrameHistory, community_feed_site, frame_history_entry_from_decoded,
        frame_identity_for_volume,
    };

    #[test]
    fn radar_overlay_layer_starts_visible_and_unrendered() {
        let site = RadarSite::new("KTLX");
        let layer = OverlayView::new(7, site);

        assert_eq!(layer.engine.id, EngineId(7));
        assert_eq!(layer.site.level2_id, "KTLX");
        assert!(layer.visible);
        assert_eq!(layer.opacity, DEFAULT_RADAR_OVERLAY_ALPHA);
        assert!(layer.volume.is_none());
        assert!(layer.texture.is_none());
        assert!(layer.load_receiver.is_none());
        assert!(layer.pending_render_key.is_none());
    }

    /// Phase-4b pin: shared-pool overlay results route by lane id with the
    /// pre-4b per-layer accept rules — latest-ok installs, stale-ok
    /// recycles, latest-err clears pending + status, and a retired lane
    /// (layer removed mid-flight) recycles instead of installing.
    #[test]
    fn overlay_pool_results_route_by_lane_with_pre_4b_accept_rules() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let (recycle_sender, recycle_receiver) = mpsc::channel::<RenderRecycleBuffer>();
        app.overlay_render_recycle_sender = recycle_sender;
        let ctx = egui::Context::default();
        let product = DisplayProduct::Moment(MomentType::Reflectivity);

        let mut layer = OverlayView::new(9, RadarSite::new("KTLX"));
        let pending_key = test_screen_texture_key(1, 0, product.clone());
        layer.pending_render_key = Some(pending_key.clone());
        app.radar_layers.push(layer);

        let rendered = |width: usize, height: usize| RenderedTexture {
            width,
            height,
            rgba: vec![0; width * height * 4],
            buffer_signature: test_viewport_signature(width as u32),
            render_ms: 1.0,
            worker_ms: 1.0,
            sample_cache_build_ms: None,
            used_sample_cache: false,
            radar_range_km: DEFAULT_RADAR_RANGE_KM,
        };

        // Retired lane: no layer with id 404 — the buffer must recycle and
        // the live layer must stay untouched.
        app.install_overlay_render_result(
            &ctx,
            AsyncRenderResult {
                key: pending_key.clone(),
                lane: LaneId::Overlay(404),
                result: Ok(rendered(4, 4)),
            },
        );
        assert!(recycle_receiver.try_recv().is_ok());
        assert_eq!(
            app.radar_layers[0].pending_render_key.as_ref(),
            Some(&pending_key)
        );

        // Stale key for the live lane: recycle, never install.
        app.install_overlay_render_result(
            &ctx,
            AsyncRenderResult {
                key: test_screen_texture_key(2, 0, product.clone()),
                lane: LaneId::Overlay(9),
                result: Ok(rendered(4, 4)),
            },
        );
        assert!(recycle_receiver.try_recv().is_ok());
        assert!(app.radar_layers[0].texture.is_none());
        assert_eq!(
            app.radar_layers[0].pending_render_key.as_ref(),
            Some(&pending_key)
        );

        // Latest key installs the texture and recycles the buffer after the
        // upload.
        app.install_overlay_render_result(
            &ctx,
            AsyncRenderResult {
                key: pending_key.clone(),
                lane: LaneId::Overlay(9),
                result: Ok(rendered(720, 480)),
            },
        );
        assert!(app.radar_layers[0].pending_render_key.is_none());
        assert!(app.radar_layers[0].texture.is_some());
        assert_eq!(app.radar_layers[0].texture_key.as_ref(), Some(&pending_key));
        assert!(recycle_receiver.try_recv().is_ok());

        // Latest-err clears pending and reports on the LAYER's status line.
        app.radar_layers[0].pending_render_key = Some(pending_key.clone());
        app.install_overlay_render_result(
            &ctx,
            AsyncRenderResult {
                key: pending_key,
                lane: LaneId::Overlay(9),
                result: Err("boom".to_owned()),
            },
        );
        assert!(app.radar_layers[0].pending_render_key.is_none());
        assert_eq!(app.radar_layers[0].engine.status, "Render failed: boom");
    }

    #[test]
    fn radar_overlay_history_syncs_to_latest_frame_at_or_before_timeline() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let ctx = egui::Context::default();
        let base = Utc.with_ymd_and_hms(2026, 6, 17, 18, 0, 0).unwrap();
        let primary_first = test_decoded_from_volume(
            PathBuf::from("KTLX-1800"),
            test_volume_with_site_time("KTLX", base),
            FrameStatus::LiveComplete,
        );
        let primary_second = test_decoded_from_volume(
            PathBuf::from("KTLX-1805"),
            test_volume_with_site_time("KTLX", base + chrono::Duration::minutes(5)),
            FrameStatus::LiveComplete,
        );
        upsert_primary_history_frame(&mut app, primary_first);
        upsert_primary_history_frame(&mut app, primary_second);
        app.primary
            .history
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        app.select_history_frame(0, false, &ctx);

        let mut layer = OverlayView::new(42, RadarSite::new("KOUN"));
        layer.timeline_sync = true;
        ViewerApp::install_radar_layer_history(
            &mut layer,
            DecodedLoadBatch {
                // Deliberately unsorted: the installer should preserve the
                // intended selected identity and then sync by timeline time.
                frames: vec![
                    test_decoded_from_volume(
                        PathBuf::from("KOUN-1805"),
                        test_volume_with_site_time("KOUN", base + chrono::Duration::minutes(5)),
                        FrameStatus::LiveComplete,
                    ),
                    test_decoded_from_volume(
                        PathBuf::from("KOUN-1800"),
                        test_volume_with_site_time("KOUN", base),
                        FrameStatus::LiveComplete,
                    ),
                ],
                selected_index: 1,
            },
        );
        app.radar_layers.push(layer);

        app.select_history_frame(1, false, &ctx);

        let layer = &app.radar_layers[0];
        assert_eq!(layer.engine.history.len(), 2);
        assert_eq!(layer.engine.cursor.index, 1);
        assert_eq!(
            layer.volume.as_ref().map(|volume| volume.volume_time),
            Some(base + chrono::Duration::minutes(5))
        );
        assert!(layer.engine.status.contains("Synced KOUN 2/2"));
    }

    #[test]
    fn history_frame_asof_join_never_selects_future_scan() {
        let base = Utc.with_ymd_and_hms(2026, 6, 17, 18, 0, 0).unwrap();
        let history = [0, 5, 10]
            .into_iter()
            .map(|minutes| {
                frame_history_entry_from_decoded(test_decoded_from_volume(
                    PathBuf::from(format!("KTLX-{minutes}")),
                    test_volume_with_site_time("KTLX", base + chrono::Duration::minutes(minutes)),
                    FrameStatus::LiveComplete,
                ))
            })
            .collect::<Vec<_>>();

        assert_eq!(
            history_frame_index_at_or_before(
                &history,
                base + chrono::Duration::minutes(7),
                COORDINATED_RADAR_MAX_STALENESS_SECONDS,
            ),
            Some(1)
        );
        assert_eq!(
            history_frame_index_at_or_before(
                &history,
                base + chrono::Duration::minutes(9),
                COORDINATED_RADAR_MAX_STALENESS_SECONDS,
            ),
            Some(1),
            "the 18:10 scan must not be shown at 18:09"
        );
        assert_eq!(
            history_frame_index_at_or_before(
                &history,
                base - chrono::Duration::seconds(1),
                COORDINATED_RADAR_MAX_STALENESS_SECONDS,
            ),
            None
        );
    }

    #[test]
    fn radar_overlay_timeline_sync_retains_visible_texture_until_replacement_render() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.selected_product = DisplayProduct::Moment(MomentType::Reflectivity);
        app.selected_cut = 0;
        let ctx = egui::Context::default();
        let first = Arc::new(test_reflectivity_sails_volume_with_radials(
            &[(0.5, 0)],
            720,
        ));
        let mut second = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 720);
        second.volume_time += chrono::Duration::minutes(3);
        let second = Arc::new(second);
        app.volume = Some(Arc::clone(&second));
        app.primary.cursor.index = 1;
        app.primary.history = [Arc::clone(&first), Arc::clone(&second)]
            .into_iter()
            .map(|volume| FrameHistoryEntry {
                identity: frame_identity_for_volume(volume.as_ref()),
                path: PathBuf::from(format!(
                    "overlay-sync-stale-texture-{}",
                    volume.volume_time.timestamp()
                )),
                volume,
                timings: None,
                status: FrameStatus::Complete,
                source_label: "test".to_owned(),
            })
            .collect();

        let mut layer = OverlayView::new(4, RadarSite::new("KBBB"));
        layer.visible = true;
        layer.timeline_sync = true;
        layer.volume = Some(Arc::clone(&first));
        layer.engine.history = FrameHistory::from(app.primary.history.to_vec());
        layer.engine.cursor.index = 0;
        let first_key = test_screen_texture_key(
            Arc::as_ptr(&first) as usize,
            0,
            app.selected_product.clone(),
        );
        layer.texture = Some(ctx.load_texture(
            "overlay-old-frame",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            radar_texture_options(),
        ));
        layer.texture_key = Some(first_key.clone());
        app.radar_layers.push(layer);

        app.sync_radar_overlay_layers_to_timeline(&ctx);

        assert_eq!(app.radar_layers[0].engine.cursor.index, 1);
        assert_eq!(app.radar_layers[0].selected_cut, Some(0));
        assert!(app.radar_layers[0].texture.is_some());
        assert_eq!(app.radar_layers[0].texture_key.as_ref(), Some(&first_key));
        assert!(app.radar_layers[0].pending_render_key.is_none());
        assert!(
            !app.radar_overlay_screen_loop_textures_ready(),
            "screen playback must wait for the matching overlay texture even while the old image stays visible"
        );
    }

    #[test]
    fn coordinated_archive_overlay_is_not_replaced_by_live_auto_refresh() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.primary.live.enabled = true;
        let mut layer = OverlayView::new(54, RadarSite::new("KBBB"));
        layer.timeline_sync = true;
        layer.engine.live.last_refresh = Some(
            Instant::now() - Duration::from_secs(OVERLAY_REALTIME_LEVEL2_REFRESH_SECONDS + 10),
        );
        app.radar_layers.push(layer);

        app.maybe_refresh_radar_layers(&egui::Context::default());

        assert!(app.radar_layers[0].load_receiver.is_none());
        assert!(app.radar_layers[0].timeline_sync);
    }

    /// Census D14 pinning test (decided 2026-07-02, landed WITH the 4c
    /// overlay port): previews are DISPLAY-ONLY in every role. The overlay
    /// Preview drain arm paints the layer's display but NEVER enters the
    /// engine history — the legacy path replaced the whole history with
    /// the preview frame, an untested artifact the census retired.
    #[test]
    fn overlay_preview_is_display_only_and_never_enters_history() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let ctx = egui::Context::default();
        let base = Utc.with_ymd_and_hms(2026, 6, 17, 18, 0, 0).unwrap();
        let mut layer = OverlayView::new(11, RadarSite::new("KTLX"));
        let (sender, receiver) = mpsc::channel();
        layer.load_receiver = Some(receiver);
        app.radar_layers.push(layer);

        sender
            .send(AsyncLoadResult {
                label: "L2 KTLX".to_owned(),
                update: AsyncLoadUpdate::Preview(test_decoded_from_volume(
                    PathBuf::from("overlay-preview"),
                    test_volume_with_site_time("KTLX", base),
                    FrameStatus::Preview,
                )),
            })
            .expect("drain still armed");
        app.poll_radar_layer_loads(&ctx);

        let layer = &app.radar_layers[0];
        assert!(layer.volume.is_some(), "the preview paints the display");
        assert_eq!(layer.source_path, Some(PathBuf::from("overlay-preview")));
        assert!(
            layer.engine.history.is_empty(),
            "census D14: a preview never enters the frame history"
        );
        assert_eq!(layer.engine.status, "Preview L2 KTLX");
    }

    /// Census D13 (conscious normalize of dead behavior): a History batch
    /// with select_frame=false INSTALLS into the existing overlay history
    /// (P1/P2 semantics — upsert, no selection, display untouched) instead
    /// of being silently dropped; and the census-D3 cross-site diagnostic
    /// surfaces on the layer's LOCAL status line, greppable-identical,
    /// where the engine outcome reports it.
    #[test]
    fn overlay_no_select_history_batch_installs_and_surfaces_cross_site_diagnostic() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let ctx = egui::Context::default();
        let base = Utc.with_ymd_and_hms(2026, 6, 17, 18, 0, 0).unwrap();
        let mut layer = OverlayView::new(12, RadarSite::new("KTLX"));
        let (sender, receiver) = mpsc::channel();
        layer.load_receiver = Some(receiver);
        app.radar_layers.push(layer);

        sender
            .send(AsyncLoadResult {
                label: "L2 KTLX".to_owned(),
                update: AsyncLoadUpdate::History(
                    DecodedLoadBatch {
                        frames: vec![
                            test_decoded_from_volume(
                                PathBuf::from("KTLX-1800"),
                                test_volume_with_site_time("KTLX", base),
                                FrameStatus::Complete,
                            ),
                            test_decoded_from_volume(
                                PathBuf::from("KTLX-1805"),
                                test_volume_with_site_time(
                                    "KTLX",
                                    base + chrono::Duration::minutes(5),
                                ),
                                FrameStatus::Complete,
                            ),
                        ],
                        selected_index: 1,
                    },
                    false,
                ),
            })
            .expect("drain still armed");
        app.poll_radar_layer_loads(&ctx);
        {
            let layer = &app.radar_layers[0];
            assert_eq!(
                layer.engine.history.len(),
                2,
                "census D13: the no-select batch installs instead of dropping"
            );
            assert!(layer.volume.is_none(), "no selection: display untouched");
        }

        // Cross-site arm: the displayed volume belongs to another site, so
        // the ONE engine guard fires before install (census D3).
        app.radar_layers[0].volume = Some(Arc::new(test_volume_with_site_time("KAAA", base)));
        let (sender, receiver) = mpsc::channel();
        app.radar_layers[0].load_receiver = Some(receiver);
        sender
            .send(AsyncLoadResult {
                label: "L2 KBBB".to_owned(),
                update: AsyncLoadUpdate::History(
                    DecodedLoadBatch {
                        frames: vec![test_decoded_from_volume(
                            PathBuf::from("KBBB-1810"),
                            test_volume_with_site_time(
                                "KBBB",
                                base + chrono::Duration::minutes(10),
                            ),
                            FrameStatus::Complete,
                        )],
                        selected_index: 0,
                    },
                    false,
                ),
            })
            .expect("drain still armed");
        app.poll_radar_layer_loads(&ctx);
        let layer = &app.radar_layers[0];
        assert_eq!(
            layer.engine.status, "history reset (site change to KBBB, had 2 frames)",
            "the D3 diagnostic wording stays greppable-identical"
        );
        assert_eq!(layer.engine.history.len(), 1);
        assert_eq!(layer.engine.history[0].identity.site_id, "KBBB");
    }

    /// Census D12: overlay history joined the generation spine — the
    /// overlay install routes bump the engine generation, and the
    /// replace-all stays replace-all (census D1: one frame per live tick
    /// via explicit clear + install, never an accumulating upsert).
    #[test]
    fn overlay_installs_bump_the_engine_generation_and_replace_all() {
        let base = Utc.with_ymd_and_hms(2026, 6, 17, 18, 0, 0).unwrap();
        let mut layer = OverlayView::new(13, RadarSite::new("KTLX"));
        let before = layer.engine.history.generation();
        ViewerApp::install_radar_layer_volume(
            &mut layer,
            test_decoded_from_volume(
                PathBuf::from("tick-1"),
                test_volume_with_site_time("KTLX", base),
                FrameStatus::LiveComplete,
            ),
        );
        assert_ne!(
            layer.engine.history.generation(),
            before,
            "census D12: the install route bumps the generation"
        );
        assert_eq!(layer.engine.history.len(), 1);
        assert_eq!(layer.engine.cursor.index, 0);
        assert!(layer.volume.is_some(), "plain install selects the anchor");

        ViewerApp::install_radar_layer_volume(
            &mut layer,
            test_decoded_from_volume(
                PathBuf::from("tick-2"),
                test_volume_with_site_time("KTLX", base + chrono::Duration::minutes(5)),
                FrameStatus::LiveComplete,
            ),
        );
        assert_eq!(
            layer.engine.history.len(),
            1,
            "census D1: the live overlay tick is replace-all (clear + install)"
        );
        assert_eq!(
            layer.engine.history[0].path,
            PathBuf::from("tick-2"),
            "the new tick replaced the old frame wholesale"
        );
    }

    /// The overlay state chip keeps the legacy strings (census D11) while
    /// deriving live-vs-queued from engine.liveness() — including the
    /// cadence-aware international stale floor (spec §12 owner decision 3)
    /// that supersedes the flat INTL_STALE_CHIP_FLOOR_SECONDS for overlays.
    #[test]
    fn overlay_state_chip_truth_table_derives_from_liveness() {
        assert_eq!(overlay_state_chip(true, false, None, false), "loading");
        assert_eq!(overlay_state_chip(false, true, None, false), "timeline");
        assert_eq!(overlay_state_chip(false, false, None, false), "queued");
        let live = Some(Liveness::Live {
            age_seconds: 60,
            stale: false,
        });
        assert_eq!(overlay_state_chip(false, false, live, true), "live");
        // A stale live feed still reads "live" in the rail (legacy truth
        // was volume-driven); the hover detail carries the staleness.
        let stale = Some(Liveness::Live {
            age_seconds: 4000,
            stale: true,
        });
        assert_eq!(overlay_state_chip(false, false, stale, true), "live");
        // Census D14 display-only preview: volume painted, history empty.
        assert_eq!(overlay_state_chip(false, false, None, true), "live");

        // The floor itself comes from the ENGINE: 20 minutes old on an
        // intl overlay feed is fresh (1800 s floor beats a 480 s user
        // threshold); 40 minutes is stale even for intl.
        let now = Utc.with_ymd_and_hms(2026, 6, 17, 18, 0, 0).unwrap();
        let mut layer = OverlayView::new_intl(
            14,
            RadarSite::new("Ängelholm"),
            "smhi".to_owned(),
            "angelholm".to_owned(),
        );
        let volume = Arc::new(test_volume_with_site_time(
            "angelholm",
            now - chrono::Duration::minutes(20),
        ));
        layer.engine.history.push(FrameHistoryEntry {
            identity: frame_identity_for_volume(volume.as_ref()),
            path: PathBuf::from("intl:fresh"),
            volume,
            timings: None,
            status: FrameStatus::LiveComplete,
            source_label: "intl".to_owned(),
        });
        assert_eq!(
            layer.engine.liveness(now, 480),
            Some(Liveness::Live {
                age_seconds: 1200,
                stale: false
            }),
            "the intl cadence floor keeps a 20-minute frame fresh"
        );
        assert_eq!(
            layer
                .engine
                .liveness(now + chrono::Duration::minutes(20), 480),
            Some(Liveness::Live {
                age_seconds: 2400,
                stale: true
            }),
            "past the floor a live intl overlay is honestly stale"
        );
    }

    /// v0.29 Phase 4c coordinated loads work PER-SOURCE: an
    /// archive-capable international site (ORD here) window-loads as a
    /// coordinated timeline overlay through its provider archive_source();
    /// the engine feed IS the archive window, and re-issuing the window
    /// dedupes onto the same layer.
    #[test]
    fn intl_overlay_archive_window_load_sets_archive_feed_and_timeline_sync() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let ctx = egui::Context::default();
        // Through the ONE union catalog (Phase 3), never the raw registry.
        let record = data_source::sites::all_sites()
            .find(|record| {
                matches!(
                    &record.site,
                    data_source::sites::SiteRef::Intl { provider_id, .. } if provider_id == "ord"
                )
            })
            .expect("ORD sites in the union catalog");
        let data_source::sites::SiteRef::Intl {
            provider_id,
            site_id,
        } = record.site.clone()
        else {
            unreachable!("filtered to Intl above");
        };
        let start = Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 6, 9, 6, 0, 0).unwrap();

        let (lat, lon) = (
            record.lat_lon.map(|(lat, _)| lat),
            record.lat_lon.map(|(_, lon)| lon),
        );
        app.add_or_refresh_intl_radar_layer_archive_window(
            &provider_id,
            &site_id,
            &record.label,
            lat,
            lon,
            start,
            end,
            20,
            &ctx,
        );
        {
            let layer = app.radar_layers.last().expect("layer added");
            assert!(
                layer.timeline_sync,
                "coordinated overlays are timeline-owned"
            );
            assert!(layer.load_receiver.is_some(), "window worker armed");
            assert!(
                matches!(
                    &layer.engine.feed,
                    FeedSource::Archive {
                        site: data_source::sites::SiteRef::Intl { provider_id, .. },
                        window,
                    } if provider_id == "ord"
                        && window.start_utc == start
                        && window.end_utc == end
                        && window.max_frames == 20
                ),
                "the engine feed IS the archive window"
            );
            assert!(layer.engine.status.starts_with("Loading synced"));
            assert!(
                layer.engine.poll_cadence().is_none(),
                "an archive overlay structurally cannot poll"
            );
        }

        app.add_or_refresh_intl_radar_layer_archive_window(
            &provider_id,
            &site_id,
            &record.label,
            lat,
            lon,
            start,
            end,
            20,
            &ctx,
        );
        assert_eq!(
            app.radar_layers.len(),
            1,
            "re-issuing the window dedupes onto the same layer"
        );
    }

    #[test]
    fn coordinated_one_frame_overlay_never_paints_a_future_final_scan() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.selected_product = DisplayProduct::Moment(MomentType::Reflectivity);
        app.selected_cut = 0;
        let base = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();
        let primary = Arc::new(test_volume_with_site_time("KAAA", base));
        app.volume = Some(Arc::clone(&primary));
        app.primary.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(primary.as_ref()),
            path: PathBuf::from("primary-1200"),
            volume: primary,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);

        let future = Arc::new(test_volume_with_site_time(
            "KBBB",
            base + chrono::Duration::minutes(5),
        ));
        let mut layer = OverlayView::new(55, RadarSite::new("KBBB"));
        layer.timeline_sync = true;
        layer.volume = Some(Arc::clone(&future));
        layer.engine.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(future.as_ref()),
            path: PathBuf::from("overlay-final-1205"),
            volume: Arc::clone(&future),
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);
        layer.texture_key = Some(test_screen_texture_key(
            Arc::as_ptr(&future) as usize,
            0,
            app.selected_product.clone(),
        ));
        app.radar_layers.push(layer);

        app.sync_radar_overlay_layers_to_timeline(&egui::Context::default());

        assert!(app.radar_layers[0].volume.is_none());
        assert!(app.radar_layers[0].texture_key.is_none());
        assert!(app.radar_layers[0].selected_cut.is_none());
    }

    #[test]
    fn coordinated_one_frame_overlay_holds_only_within_staleness_budget() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.selected_product = DisplayProduct::Moment(MomentType::Reflectivity);
        app.selected_cut = 0;
        let base = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();
        let target = base + chrono::Duration::minutes(8);
        let primary = Arc::new(test_volume_with_site_time("KAAA", target));
        app.volume = Some(Arc::clone(&primary));
        app.primary.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(primary.as_ref()),
            path: PathBuf::from("primary-1208"),
            volume: primary,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);

        let overlay = Arc::new(test_volume_with_site_time("KBBB", base));
        let mut layer = OverlayView::new(56, RadarSite::new("KBBB"));
        layer.timeline_sync = true;
        layer.engine.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(overlay.as_ref()),
            path: PathBuf::from("overlay-1200"),
            volume: Arc::clone(&overlay),
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);
        app.radar_layers.push(layer);

        app.sync_radar_overlay_layers_to_timeline(&egui::Context::default());
        assert!(app.radar_layers[0].volume.is_some());
        assert_eq!(app.radar_layers[0].selected_cut, Some(0));

        let stale_primary = Arc::new(test_volume_with_site_time(
            "KAAA",
            base + chrono::Duration::seconds(COORDINATED_RADAR_MAX_STALENESS_SECONDS + 1),
        ));
        app.volume = Some(Arc::clone(&stale_primary));
        app.primary.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(stale_primary.as_ref()),
            path: PathBuf::from("primary-stale"),
            volume: stale_primary,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);
        app.sync_radar_overlay_layers_to_timeline(&egui::Context::default());
        assert!(app.radar_layers[0].volume.is_none());
    }

    #[test]
    fn coordinated_low_sweep_overlay_joins_by_cut_time_not_primary_cut_index() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.app_settings.loop_low_sweeps = true;
        app.selected_product = DisplayProduct::Moment(MomentType::Reflectivity);
        let base = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();

        let mut primary =
            test_reflectivity_sails_volume_with_radials(&[(0.50, 0), (0.50, 30_000)], 720);
        primary.site = radar_core::RadarSite::new("KAAA");
        primary.volume_time = base;
        let primary = Arc::new(primary);
        app.selected_cut = 1;
        app.volume = Some(Arc::clone(&primary));
        app.primary.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(primary.as_ref()),
            path: PathBuf::from("primary-low-sweeps"),
            volume: primary,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);

        let mut overlay =
            test_reflectivity_sails_volume_with_radials(&[(0.62, 5_000), (0.44, 25_000)], 720);
        overlay.site = radar_core::RadarSite::new("KBBB");
        overlay.volume_time = base;
        let overlay = Arc::new(overlay);
        let mut layer = OverlayView::new(57, RadarSite::new("KBBB"));
        layer.timeline_sync = true;
        layer.engine.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(overlay.as_ref()),
            path: PathBuf::from("overlay-low-sweeps"),
            volume: overlay,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);
        app.radar_layers.push(layer);

        app.sync_radar_overlay_layers_to_timeline(&egui::Context::default());

        assert_eq!(app.radar_layers[0].selected_cut, Some(1));
    }

    #[test]
    fn coordinated_low_sweep_overlay_prefers_primary_elevation_family_over_newer_other_tilt() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.app_settings.loop_low_sweeps = true;
        app.selected_product = DisplayProduct::Moment(MomentType::Reflectivity);
        let base = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();

        let mut primary = test_reflectivity_sails_volume_with_radials(&[(0.50, 30_000)], 720);
        primary.site = radar_core::RadarSite::new("KAAA");
        primary.volume_time = base;
        let primary = Arc::new(primary);
        app.selected_cut = 0;
        app.volume = Some(Arc::clone(&primary));
        app.primary.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(primary.as_ref()),
            path: PathBuf::from("primary-family"),
            volume: primary,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);

        let mut overlay =
            test_reflectivity_sails_volume_with_radials(&[(0.48, 5_000), (1.20, 25_000)], 720);
        overlay.site = radar_core::RadarSite::new("KBBB");
        overlay.volume_time = base;
        let overlay = Arc::new(overlay);
        let mut layer = OverlayView::new(58, RadarSite::new("KBBB"));
        layer.timeline_sync = true;
        layer.engine.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(overlay.as_ref()),
            path: PathBuf::from("overlay-family"),
            volume: overlay,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);
        app.radar_layers.push(layer);

        app.sync_radar_overlay_layers_to_timeline(&egui::Context::default());

        assert_eq!(
            app.radar_layers[0].selected_cut,
            Some(0),
            "a newer 1.2-degree overlay cut must not replace the primary 0.5-degree family"
        );
    }

    #[test]
    fn coordinated_frame_overlay_velocity_does_not_show_future_cut() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.app_settings.loop_low_sweeps = false;
        app.selected_product = DisplayProduct::Moment(MomentType::Velocity);
        app.selected_cut = 0;
        let base = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();

        let mut primary = test_velocity_sails_volume_with_radials(&[(0.50, 30_000)], 720);
        primary.site = radar_core::RadarSite::new("KAAA");
        primary.volume_time = base;
        let primary = Arc::new(primary);
        app.volume = Some(Arc::clone(&primary));
        app.primary.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(primary.as_ref()),
            path: PathBuf::from("primary-vel-1200"),
            volume: primary,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);

        let mut previous = test_velocity_sails_volume_with_radials(&[(0.52, 30_000)], 720);
        previous.site = radar_core::RadarSite::new("KBBB");
        previous.volume_time = base - chrono::Duration::minutes(4);
        let previous = Arc::new(previous);

        let mut future_cut = test_velocity_sails_volume_with_radials(&[(0.50, 60_000)], 720);
        future_cut.site = radar_core::RadarSite::new("KBBB");
        future_cut.volume_time = base + chrono::Duration::seconds(20);
        let future_cut = Arc::new(future_cut);

        let mut layer = OverlayView::new(59, RadarSite::new("KBBB"));
        layer.timeline_sync = true;
        layer.engine.history = [Arc::clone(&previous), Arc::clone(&future_cut)]
            .into_iter()
            .map(|volume| FrameHistoryEntry {
                identity: frame_identity_for_volume(volume.as_ref()),
                path: PathBuf::from(format!("overlay-vel-{}", volume.volume_time.timestamp())),
                volume,
                timings: None,
                status: FrameStatus::Complete,
                source_label: "test".to_owned(),
            })
            .collect();
        app.radar_layers.push(layer);

        app.sync_radar_overlay_layers_to_timeline(&egui::Context::default());

        assert_eq!(app.radar_layers[0].engine.cursor.index, 0);
        assert_eq!(app.radar_layers[0].selected_cut, Some(0));
        assert_eq!(
            app.radar_layers[0]
                .volume
                .as_ref()
                .map(|volume| volume.volume_time.with_timezone(&Utc)),
            Some(base - chrono::Duration::minutes(4)),
            "normal frame sync must key off the selected cut time, not only the scan start"
        );
    }

    #[test]
    fn coordinated_range_overlay_velocity_does_not_show_future_cut() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.app_settings.loop_low_sweeps = true;
        app.app_settings.loop_sweep_control = Some(LoopSweepControl {
            primary: SweepPolicySet {
                product_groups: BTreeMap::from([(
                    SweepProductGroup::Velocity,
                    SweepPolicy::range_cdeg(13, 83),
                )]),
            },
            extra_pane_overrides: BTreeMap::new(),
        });
        app.selected_product = DisplayProduct::Moment(MomentType::Velocity);
        app.selected_cut = 0;
        let base = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();

        let mut primary = test_velocity_sails_volume_with_radials(&[(0.50, 30_000)], 720);
        primary.site = radar_core::RadarSite::new("KAAA");
        primary.volume_time = base;
        let primary = Arc::new(primary);
        app.volume = Some(Arc::clone(&primary));
        app.primary.history = FrameHistory::from(vec![FrameHistoryEntry {
            identity: frame_identity_for_volume(primary.as_ref()),
            path: PathBuf::from("primary-range-vel-1200"),
            volume: primary,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        }]);

        let mut previous = test_velocity_sails_volume_with_radials(&[(0.52, 30_000)], 720);
        previous.site = radar_core::RadarSite::new("KBBB");
        previous.volume_time = base - chrono::Duration::minutes(4);
        let previous = Arc::new(previous);

        let mut future_cut = test_velocity_sails_volume_with_radials(&[(0.50, 60_000)], 720);
        future_cut.site = radar_core::RadarSite::new("KBBB");
        future_cut.volume_time = base + chrono::Duration::seconds(20);
        let future_cut = Arc::new(future_cut);

        let mut layer = OverlayView::new(60, RadarSite::new("KBBB"));
        layer.timeline_sync = true;
        layer.engine.history = [Arc::clone(&previous), Arc::clone(&future_cut)]
            .into_iter()
            .map(|volume| FrameHistoryEntry {
                identity: frame_identity_for_volume(volume.as_ref()),
                path: PathBuf::from(format!(
                    "overlay-range-vel-{}",
                    volume.volume_time.timestamp()
                )),
                volume,
                timings: None,
                status: FrameStatus::Complete,
                source_label: "test".to_owned(),
            })
            .collect();
        app.radar_layers.push(layer);

        app.sync_radar_overlay_layers_to_timeline(&egui::Context::default());

        assert_eq!(app.radar_layers[0].engine.cursor.index, 0);
        assert_eq!(app.radar_layers[0].selected_cut, Some(0));
        assert_eq!(
            app.radar_layers[0]
                .volume
                .as_ref()
                .map(|volume| volume.volume_time.with_timezone(&Utc)),
            Some(base - chrono::Duration::minutes(4)),
            "range sync must never choose a cut collected after the primary timeline time"
        );
    }

    #[test]
    fn coordinated_site_ids_parse_common_separators_and_dedupe() {
        assert_eq!(
            parse_coordinated_site_ids("tlx, kinx KFDR;ktlx"),
            vec!["KTLX", "KINX", "KFDR"]
        );
        // `intl:` settings keys pass through VERBATIM (case-significant
        // provider site codes; the favorites-trap fix, spec §1.1).
        assert_eq!(
            parse_coordinated_site_ids("ktlx intl:smhi:angelholm INTL:ORD:DEESS"),
            vec!["KTLX", "intl:smhi:angelholm", "INTL:ORD:DEESS"]
        );
    }

    /// v0.29 Phase 3: the candidate pool queries the REAL union catalog
    /// (`sites_near`) — fixtures are the compiled-in world. Around the
    /// selected KTLX the pool lists nearby 88Ds, never the primary itself,
    /// never the colocated TOKC TDWR, never research feeds.
    #[test]
    fn unified_player_nearby_sites_exclude_primary_radar() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.sites = vec![RadarSite::new("KTLX").with_location(
            Some("Norman".to_owned()),
            Some(35.333),
            Some(-97.278),
        )];
        app.selected_site_index = 0;
        app.map_center_lat = 35.333;
        app.map_center_lon = -97.278;
        app.unified_player.coordinated_site_radius_km = 460.0;

        app.populate_unified_player_nearby_sites();

        let input = app.unified_player.coordinated_sites_input.clone();
        let ids = parse_coordinated_site_ids(&input);
        assert!(!ids.is_empty(), "real 88Ds surround Norman");
        assert!(
            !ids.iter().any(|id| id == "KTLX"),
            "primary radar excluded: {input}"
        );
        assert!(
            !ids.iter().any(|id| id == "TOKC"),
            "TDWRs never fill mosaics: {input}"
        );
        assert!(
            ids.iter().any(|id| id == "KVNX") || ids.iter().any(|id| id == "KINX"),
            "nearby real 88Ds fill the pool: {input}"
        );
    }

    #[test]
    fn nearby_coordinated_overlay_sites_returns_nearest_four_for_sync_five() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.sites = vec![RadarSite::new("KTLX").with_location(
            Some("Norman".to_owned()),
            Some(35.333),
            Some(-97.278),
        )];
        app.selected_site_index = 0;
        app.map_center_lat = 35.333;
        app.map_center_lon = -97.278;
        app.unified_player.coordinated_site_radius_km = 460.0;

        let candidates = app.nearby_coordinated_overlay_sites(STORM_VIDEO_SYNC_OVERLAY_RADARS);
        assert_eq!(candidates.len(), STORM_VIDEO_SYNC_OVERLAY_RADARS);
        assert!(
            candidates.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "nearest first"
        );
        assert!(
            candidates.iter().all(|(_, record)| {
                record.site
                    != SiteRef::Us {
                        level2_id: "KTLX".to_owned(),
                    }
            }),
            "primary radar never fills its own mosaic"
        );
    }

    #[test]
    fn mosaic_candidates_keep_tjua_wsr88d_but_skip_tdwrs_and_community_feeds() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let mut community = community_feed_site(
            data_source::community_feeds::community_feeds()
                .first()
                .expect("at least one community feed"),
        );
        community.latitude_deg = Some(18.30);
        community.longitude_deg = Some(-66.10);
        app.sites = vec![
            RadarSite::new("KAAA").with_location(
                Some("Primary".to_owned()),
                Some(18.20),
                Some(-66.05),
            ),
            // San Juan's WSR-88D shares the TDWR id prefix but must stay
            // mosaic-eligible.
            RadarSite::new("TJUA").with_location(
                Some("San Juan".to_owned()),
                Some(18.1157),
                Some(-66.0782),
            ),
            // The actual colocated San Juan TDWR.
            RadarSite::new("TSJU").with_location(
                Some("San Juan TDWR".to_owned()),
                Some(18.474),
                Some(-66.179),
            ),
            community,
        ];
        app.selected_site_index = 0;
        app.map_center_lat = 18.20;
        app.map_center_lon = -66.05;
        app.unified_player.coordinated_site_radius_km = 100.0;

        let site_ids = app
            .nearby_coordinated_overlay_sites(STORM_VIDEO_SYNC_OVERLAY_RADARS)
            .into_iter()
            .map(|(_, record)| record.site.settings_key())
            .collect::<Vec<_>>();

        assert_eq!(site_ids, vec!["TJUA"]);
    }

    #[test]
    fn unified_player_coordinated_overlay_resolution_skips_primary_radar() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.sites = vec![
            RadarSite::new("KAAA").with_location(
                Some("Primary".to_owned()),
                Some(35.0),
                Some(-97.0),
            ),
            RadarSite::new("KBBB").with_location(
                Some("Nearby".to_owned()),
                Some(35.4),
                Some(-97.2),
            ),
        ];
        app.selected_site_index = 0;
        app.unified_player.coordinated_sites_input =
            "aaa, bbb, kmissing, intl:smhi:angelholm".to_owned();

        let (sites, skipped_primary, missing, had_input) =
            app.resolve_unified_player_coordinated_overlay_sites();

        assert!(had_input);
        assert!(skipped_primary);
        assert_eq!(sites.len(), 2, "both worlds resolve into the pool");
        assert!(matches!(&sites[0], CoordinatedOverlaySite::Us(site) if site.level2_id == "KBBB"));
        assert!(matches!(
            &sites[1],
            CoordinatedOverlaySite::Intl(site)
                if site.provider_id == "smhi" && site.site_id == "angelholm"
        ));
        assert_eq!(missing, vec!["KMISSING"]);
    }

    #[test]
    fn coordinated_overlay_mode_uses_live_without_loaded_loop() {
        let app = test_viewer_app_with_hazards(Vec::new());

        assert_eq!(
            app.coordinated_overlay_load_mode(),
            CoordinatedOverlayLoadMode::Live
        );
    }

    #[test]
    fn coordinated_overlay_mode_uses_archive_window_for_loaded_loop() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.primary.limits.frame_limit = 96;
        add_two_test_history_frames(&mut app);
        let (start_utc, end_utc) = app.loaded_loop_time_window().expect("loaded loop window");

        assert_eq!(
            app.coordinated_overlay_load_mode(),
            CoordinatedOverlayLoadMode::ArchiveWindow {
                start_utc,
                end_utc,
                max_frames: 96,
            }
        );
        assert!(
            app.coordinated_overlay_status(2, app.coordinated_overlay_load_mode())
                .contains("synced radar overlay loop")
        );
    }

    #[test]
    fn adding_overlay_from_map_auto_syncs_to_loaded_loop() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.primary.limits.frame_limit = 7;
        add_two_test_history_frames(&mut app);
        let ctx = egui::Context::default();

        app.add_or_refresh_radar_layer_for_current_timeline(RadarSite::new("KBBB"), &ctx);

        let layer = app.radar_layers.first().expect("overlay layer");
        assert!(layer.timeline_sync);
        assert!(layer.load_receiver.is_some());
    }

    #[test]
    fn coordinated_overlay_mode_uses_focused_independent_pane_loop() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.app_settings.independent_panels = true;
        app.app_settings.grid_pane_count = 2;
        app.grid_layout = PanelLayout::TwoVertical;
        app.sync_extra_panes();
        app.active_pane = 1;
        app.primary.limits.frame_limit = 48;
        let base = Utc.with_ymd_and_hms(2026, 6, 17, 18, 0, 0).unwrap();
        let pane = &mut app.extra_panes[0];
        pane.pin = Some(SiteRef::Us {
            level2_id: "KIND".to_owned(),
        });
        pane.product = DisplayProduct::Moment(MomentType::Velocity);
        pane.cut = Some(0);
        for minutes in [0, 5] {
            let volume = Arc::new(test_volume_with_site_time(
                "KIND",
                base + chrono::Duration::minutes(minutes),
            ));
            if minutes == 0 {
                pane.volume = Some(Arc::clone(&volume));
            }
            pane.engine.history.push(FrameHistoryEntry {
                identity: frame_identity_for_volume(volume.as_ref()),
                path: PathBuf::from(format!("coordinated-independent-window-{minutes}")),
                volume,
                timings: None,
                status: FrameStatus::LiveComplete,
                source_label: "test".to_owned(),
            });
        }
        let (start_utc, end_utc) = app.loaded_loop_time_window().expect("pane loop window");

        assert_eq!(
            app.coordinated_overlay_load_mode(),
            CoordinatedOverlayLoadMode::ArchiveWindow {
                start_utc,
                end_utc,
                max_frames: 48,
            }
        );
    }
}
