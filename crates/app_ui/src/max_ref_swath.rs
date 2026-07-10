//! Max-value swath overlay — "where the storm has BEEN".
//!
//! Two toggleable overlay layers that accumulate the per-gate MAXIMUM of a
//! base-tilt moment across every frame of the loaded loop and draw it beneath
//! the live radar frame, so the operator sees a storm's whole footprint /
//! where it started without scrubbing the loop back:
//!
//!   * **Max REF swath** — peak reflectivity (the storm's track).
//!   * **Max VEL swath** — peak velocity magnitude, sign preserved (peak
//!     inbound/outbound; the tropical-cyclone velocity couplet envelope).
//!
//! The heavy lifting is [`render2d::max_value_swath`], which folds the loop's
//! base tilts into ONE synthetic single-tilt volume. That volume renders
//! through the *existing* viewport rasterizer and the *existing* reflectivity
//! / velocity color tables — this module is only cache + draw glue.
//!
//! Threading (finding #8, 2026-07 audit): the fold + viewport raster used to
//! run synchronously inside the paint path, so every `FrameHistory`
//! generation bump (one per streamed archive frame, one per live chunk
//! upsert) refolded the WHOLE loop on the UI thread — streamed loads were
//! O(N²) and every live install hitched the map while a swath was on. Both
//! steps now run on a background [`WorkerSlot`] job (the app's standard
//! one-job-in-flight worker idiom): paint keeps drawing the stale texture
//! until the fresh one lands, and rebuilds are DEBOUNCED while a load is
//! streaming — the fold is dispatched only once the generation has been
//! stable for [`SWATH_REBUILD_DEBOUNCE`] (the first build after enable goes
//! immediately; there is nothing stale to keep showing).
//!
//! Caching (spec: "so it isn't recomputed every frame"): the synthetic volume
//! is rebuilt only when the loop's frame set changes, keyed off
//! `FrameHistory::generation()` (an O(1) content stamp). The rasterized
//! texture is rebuilt only when the swath, palette, or (while the map is
//! idle) the viewport changes; during pan/zoom the cached texture is
//! reprojected as a quad, exactly like the primary radar layer between
//! re-renders.

use std::sync::Arc;
use std::time::{Duration, Instant};

use color_tables::{ColorTableFamily, ColorTableSet};
use eframe::egui;
use radar_core::{MomentType, RadarVolume};
use render2d::{
    SwathAggregation, ViewportMomentCache, ViewportRasterOptions, max_value_swath,
    viewport_rgba_buffer_len,
};
use ui_core::worker_slot::{SlotPoll, WorkerSlot};

use crate::{
    LayerRowOpacity, LayerRowSpec, LayerRowVis, ViewerApp, ViewportKey,
    anchored_radar_texture_rect, layer_row, paint_rotated_image, radar_color_image_from_rgba,
    radar_texture_options,
};

/// Which moment a swath layer accumulates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SwathMoment {
    Reflectivity,
    Velocity,
}

impl SwathMoment {
    fn moment(self) -> MomentType {
        match self {
            Self::Reflectivity => MomentType::Reflectivity,
            Self::Velocity => MomentType::Velocity,
        }
    }

    fn color_family(self) -> ColorTableFamily {
        match self {
            Self::Reflectivity => ColorTableFamily::Reflectivity,
            Self::Velocity => ColorTableFamily::Velocity,
        }
    }

    fn aggregation(self) -> SwathAggregation {
        match self {
            Self::Reflectivity => SwathAggregation::Max,
            // Peak velocity magnitude, sign preserved (the diverging velocity
            // table then colors inbound vs outbound extremes).
            Self::Velocity => SwathAggregation::MaxMagnitude,
        }
    }

    fn texture_name(self) -> &'static str {
        match self {
            Self::Reflectivity => "max-ref-swath",
            Self::Velocity => "max-vel-swath",
        }
    }
}

/// Default overlay alpha — a semi-transparent trail that reads as distinct
/// from the opaque live frame drawn over it.
const DEFAULT_SWATH_OPACITY: u8 = 190;

/// How long the loop's generation must hold still before a swath refold is
/// dispatched. Streamed archive loads and live chunk assembly bump the
/// generation many times per second; without this window every bump would
/// queue a full refold of all frames-so-far (the O(N²) shape of finding #8).
/// 250 ms after the last bump — i.e. right after the final frame of a load
/// lands — one rebuild folds everything.
const SWATH_REBUILD_DEBOUNCE: Duration = Duration::from_millis(250);

/// What the background job folds the swath from: a changed loop rebuilds the
/// synthetic volume from the frames' volumes; an unchanged loop re-rasters
/// the already-built volume (palette or resting-viewport change).
enum SwathVolumeSource {
    Rebuild(Vec<Arc<RadarVolume>>),
    Reuse(Arc<RadarVolume>),
}

/// One finished background build/raster job. The texture upload itself
/// happens at drain time on the UI thread (cheap copy), matching the render
/// lanes and overlay pool.
struct SwathJobOutcome {
    /// The `FrameHistory::generation()` the volume corresponds to.
    generation: u64,
    /// The (re)built synthetic swath volume; `None` when no frame of the
    /// loop carries the moment.
    volume: Option<Arc<RadarVolume>>,
    /// Full-viewport RGBA raster of `volume`; `None` clears the texture.
    image: Option<egui::ColorImage>,
    /// Viewport the raster was drawn at.
    key: ViewportKey,
    /// Palette signature the raster was drawn with.
    color_signature: u64,
}

/// One swath overlay layer: its toggle, opacity, lazily-built caches, and
/// the background worker that (re)builds them.
pub(crate) struct SwathLayer {
    pub(crate) enabled: bool,
    pub(crate) opacity: u8,
    /// Frame-history generation the synthetic volume was built from.
    built_generation: Option<u64>,
    /// Synthetic single-tilt swath volume (kept alive for the render cache's
    /// pointer-identity check), `None` when the loop has no such moment.
    volume: Option<Arc<RadarVolume>>,
    /// Rasterized overlay texture and the viewport / palette it was drawn at.
    texture: Option<egui::TextureHandle>,
    viewport_key: Option<ViewportKey>,
    rotation_rad: f32,
    color_signature: u64,
    /// Background fold + raster job — at most one in flight; paint keeps the
    /// stale texture until the result lands (finding #8: never block the
    /// paint thread on swath work).
    worker: WorkerSlot<SwathJobOutcome>,
    /// Debounce clock: the not-yet-built generation last observed and when
    /// it was first seen. A rebuild dispatches only once the generation has
    /// held still for [`SWATH_REBUILD_DEBOUNCE`].
    pending_generation: Option<(u64, Instant)>,
    /// Diagnostic: total volume-rebuild jobs dispatched. The streamed-load
    /// regression tests pin the debounce on this counter.
    #[cfg_attr(not(test), allow(dead_code))]
    builds_dispatched: u64,
}

impl SwathLayer {
    fn new(label: &'static str) -> Self {
        Self {
            enabled: false,
            opacity: DEFAULT_SWATH_OPACITY,
            built_generation: None,
            volume: None,
            texture: None,
            viewport_key: None,
            rotation_rad: 0.0,
            color_signature: 0,
            worker: WorkerSlot::idle(label),
            pending_generation: None,
            builds_dispatched: 0,
        }
    }

    /// Drop every cache and cancel any in-flight job (on toggle-off, or when
    /// a rebuild is forced) so a re-enable recomputes from the current loop.
    fn invalidate(&mut self) {
        self.worker.cancel();
        self.pending_generation = None;
        self.built_generation = None;
        self.volume = None;
        self.texture = None;
        self.viewport_key = None;
    }

    /// Keep the layer's caches converging toward (`generation`, palette,
    /// viewport) WITHOUT blocking: drain a finished background job, then
    /// dispatch at most one new job. Called from the paint path every frame
    /// the layer is enabled; the expensive fold/raster never runs here.
    #[allow(clippy::too_many_arguments)]
    fn ensure(
        &mut self,
        ctx: &egui::Context,
        moment: SwathMoment,
        generation: u64,
        frames: &[crate::FrameHistoryEntry],
        color_tables: &ColorTableSet,
        options: ViewportRasterOptions,
        key: ViewportKey,
        reproject_only: bool,
    ) {
        let signature = color_tables.signature_for_family(moment.color_family());

        // 0. Land a finished background job. Until it lands, paint keeps
        // drawing the previous texture. A `Disconnected` (worker panic) is
        // ignored like every other slot in the app — the next pass simply
        // re-evaluates; the old synchronous path would have crashed outright.
        if let SlotPoll::Ready(outcome) = self.worker.poll() {
            self.install(ctx, moment, outcome);
        }
        if self.worker.in_flight() {
            // One job at a time (slot contract). WorkerTx::send requests a
            // repaint when it finishes, so the result lands promptly.
            return;
        }

        // 1. Loop changed → rebuild the synthetic volume (+ raster),
        // debounced so a streaming load folds once, not once per frame.
        if self.built_generation != Some(generation) {
            if self.rebuild_due(ctx, generation) {
                let volumes: Vec<Arc<RadarVolume>> = frames
                    .iter()
                    .map(|frame| Arc::clone(&frame.volume))
                    .collect();
                self.spawn_job(
                    ctx,
                    moment,
                    SwathVolumeSource::Rebuild(volumes),
                    generation,
                    signature,
                    color_tables.clone(),
                    options,
                    key,
                );
            }
            return;
        }

        // 2. Palette or resting-viewport change → re-raster the existing
        // volume (no refold).
        let need_raster = self.volume.is_some()
            && (self.texture.is_none()
                || self.color_signature != signature
                || (self.viewport_key != Some(key) && !reproject_only));
        if !need_raster {
            return;
        }
        let Some(volume) = self.volume.clone() else {
            return;
        };
        self.spawn_job(
            ctx,
            moment,
            SwathVolumeSource::Reuse(volume),
            generation,
            signature,
            color_tables.clone(),
            options,
            key,
        );
    }

    /// Debounce gate for volume rebuilds: true when the changed generation
    /// has held still for [`SWATH_REBUILD_DEBOUNCE`]. The first build after
    /// enable is exempt (nothing stale is on screen yet). While waiting,
    /// schedules the repaint that will re-check, so the rebuild fires even
    /// on an otherwise idle map.
    fn rebuild_due(&mut self, ctx: &egui::Context, generation: u64) -> bool {
        if self.built_generation.is_none() {
            self.pending_generation = None;
            return true;
        }
        match self.pending_generation {
            Some((pending, since)) if pending == generation => {
                let elapsed = since.elapsed();
                if elapsed >= SWATH_REBUILD_DEBOUNCE {
                    self.pending_generation = None;
                    true
                } else {
                    ctx.request_repaint_after(SWATH_REBUILD_DEBOUNCE - elapsed);
                    false
                }
            }
            _ => {
                // New (or newer) generation: (re)arm the stability clock.
                self.pending_generation = Some((generation, Instant::now()));
                ctx.request_repaint_after(SWATH_REBUILD_DEBOUNCE);
                false
            }
        }
    }

    /// Dispatch one fold/raster job to the background worker.
    #[allow(clippy::too_many_arguments)]
    fn spawn_job(
        &mut self,
        ctx: &egui::Context,
        moment: SwathMoment,
        source: SwathVolumeSource,
        generation: u64,
        color_signature: u64,
        color_tables: ColorTableSet,
        options: ViewportRasterOptions,
        key: ViewportKey,
    ) {
        if matches!(source, SwathVolumeSource::Rebuild(_)) {
            self.builds_dispatched += 1;
        }
        self.worker.spawn(ctx, move |tx| {
            let volume = match source {
                SwathVolumeSource::Rebuild(volumes) => {
                    build_swath_volume(&volumes, moment).map(Arc::new)
                }
                SwathVolumeSource::Reuse(volume) => Some(volume),
            };
            let image = volume
                .as_ref()
                .and_then(|volume| raster_swath_image(volume, moment, &color_tables, options));
            let _ = tx.send(SwathJobOutcome {
                generation,
                volume,
                image,
                key,
                color_signature,
            });
        });
    }

    /// Install a finished job's volume + texture (UI thread; only the
    /// texture upload happens here).
    fn install(&mut self, ctx: &egui::Context, moment: SwathMoment, outcome: SwathJobOutcome) {
        self.built_generation = Some(outcome.generation);
        self.volume = outcome.volume;
        self.color_signature = outcome.color_signature;
        match outcome.image {
            Some(image) => {
                self.texture =
                    Some(ctx.load_texture(moment.texture_name(), image, radar_texture_options()));
                self.viewport_key = Some(outcome.key);
                self.rotation_rad = outcome.key.rotation_mrad as f32 / 1000.0;
            }
            None => {
                // The loop carries no such moment (or the raster failed):
                // nothing to draw.
                self.texture = None;
                self.viewport_key = None;
            }
        }
    }
}

/// Both swath overlays.
pub(crate) struct SwathState {
    pub(crate) reflectivity: SwathLayer,
    pub(crate) velocity: SwathLayer,
}

impl Default for SwathState {
    fn default() -> Self {
        Self {
            reflectivity: SwathLayer::new("max-ref-swath"),
            velocity: SwathLayer::new("max-vel-swath"),
        }
    }
}

impl SwathState {
    fn layer(&self, moment: SwathMoment) -> &SwathLayer {
        match moment {
            SwathMoment::Reflectivity => &self.reflectivity,
            SwathMoment::Velocity => &self.velocity,
        }
    }

    fn layer_mut(&mut self, moment: SwathMoment) -> &mut SwathLayer {
        match moment {
            SwathMoment::Reflectivity => &mut self.reflectivity,
            SwathMoment::Velocity => &mut self.velocity,
        }
    }
}

impl ViewerApp {
    /// The two swath rows in the BASE group of the layer rail. Placed with the
    /// radar-derived rows because a swath is a pure product of the loaded
    /// radar loop.
    pub(crate) fn max_swath_rail_rows(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.max_swath_rail_row(
            ui,
            ctx,
            SwathMoment::Reflectivity,
            "Max REF swath",
            "Peak reflectivity accumulated over the loaded loop — the storm's track / where it has been (drawn beneath the live frame). Recomputed in the background only when the loop changes.",
        );
        self.max_swath_rail_row(
            ui,
            ctx,
            SwathMoment::Velocity,
            "Max VEL swath",
            "Peak base-tilt velocity magnitude over the loaded loop, inbound/outbound sign preserved (raw, may alias near the Nyquist limit).",
        );
    }

    fn max_swath_rail_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        moment: SwathMoment,
        name: &str,
        hover: &str,
    ) {
        // Owned locals first so the row can hold disjoint &mut borrows of the
        // layer without also borrowing the rest of `self`.
        let built = self.swath.layer(moment).volume.is_some();
        let building = self.swath.layer(moment).worker.in_flight();
        let frame_count = self.primary.history.len();
        let trailing_text = if !self.swath.layer(moment).enabled {
            String::new()
        } else if built {
            format!("{frame_count} frames")
        } else if building {
            "building…".to_owned()
        } else if frame_count < 2 {
            "need loop".to_owned()
        } else {
            String::new()
        };

        let layer = self.swath.layer_mut(moment);
        let was_enabled = layer.enabled;
        let changed = layer_row(
            ui,
            LayerRowSpec {
                vis: LayerRowVis::Toggle {
                    value: &mut layer.enabled,
                    hover,
                },
                name,
                name_hover: hover,
                count: (!trailing_text.is_empty()).then_some(trailing_text.as_str()),
                opacity: Some(LayerRowOpacity::U8 {
                    value: &mut layer.opacity,
                    min: 40,
                    max: 255,
                }),
                ..Default::default()
            },
            |_ui| {},
        );
        if changed {
            // Toggling off frees the caches (and cancels any in-flight
            // build); toggling on rebuilds via the worker on the next draw.
            if was_enabled && !self.swath.layer(moment).enabled {
                self.swath.layer_mut(moment).invalidate();
            }
            self.save_overlay_defaults();
            ctx.request_repaint();
        }
    }

    /// Drain/dispatch the background jobs and draw both enabled swaths.
    /// Called once per frame, beneath the primary radar layer. Never blocks:
    /// the fold + raster run on a worker thread and the stale texture keeps
    /// drawing until the fresh one lands.
    pub(crate) fn draw_swath_overlays(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: egui::Rect,
    ) {
        if !self.swath.reflectivity.enabled && !self.swath.velocity.enabled {
            return;
        }
        let Some((radar_lat, radar_lon)) = self.radar_location() else {
            return;
        };
        let Some((options, key)) =
            self.viewport_raster_options_for_location(ctx, rect, radar_lat, radar_lon)
        else {
            return;
        };
        // During interaction / camera-follow the viewport changes every
        // repaint; reproject the cached texture instead of re-rastering
        // (matches the primary radar layer's between-render behavior).
        let reproject_only = self.loop_prewarm_paused_for_interaction()
            || self.smooth_camera_follow_playback_active();
        let generation = self.primary.history.generation();

        for moment in [SwathMoment::Reflectivity, SwathMoment::Velocity] {
            if !self.swath.layer(moment).enabled {
                continue;
            }
            {
                let frames = self.primary.history.as_slice();
                let color_tables = &self.color_tables;
                self.swath.layer_mut(moment).ensure(
                    ctx,
                    moment,
                    generation,
                    frames,
                    color_tables,
                    options,
                    key,
                    reproject_only,
                );
            }
            self.paint_swath_layer(moment, painter, rect, ctx, radar_lat, radar_lon);
        }
    }

    /// Paint the cached swath texture, reprojected to the current viewport and
    /// AEQD-aligned, exactly like the primary radar layer.
    fn paint_swath_layer(
        &self,
        moment: SwathMoment,
        painter: &egui::Painter,
        rect: egui::Rect,
        ctx: &egui::Context,
        radar_lat: f32,
        radar_lon: f32,
    ) {
        let layer = self.swath.layer(moment);
        let (Some(texture), Some(viewport_key)) = (&layer.texture, layer.viewport_key) else {
            return;
        };
        let texture_id = texture.id();
        let opacity = layer.opacity;
        let baked = layer.rotation_rad;

        let image_rect = self
            .viewport_raster_options_for_location(ctx, rect, radar_lat, radar_lon)
            .map(|(current, _)| {
                anchored_radar_texture_rect(rect, ctx.pixels_per_point(), viewport_key, current)
            })
            .unwrap_or(rect);
        paint_rotated_image(
            painter,
            texture_id,
            image_rect,
            self.lon_lat_to_screen(rect, radar_lon, radar_lat),
            self.aeqd_north_angle(rect, radar_lat, radar_lon) - baked,
            egui::Color32::from_white_alpha(opacity),
        );
    }
}

/// Build the synthetic swath volume from the loop's frame volumes. Runs on
/// the background worker — this is the expensive whole-loop fold.
fn build_swath_volume(volumes: &[Arc<RadarVolume>], moment: SwathMoment) -> Option<RadarVolume> {
    let volumes: Vec<&RadarVolume> = volumes.iter().map(|volume| volume.as_ref()).collect();
    max_value_swath(&volumes, moment.moment(), moment.aggregation())
}

/// Rasterize a swath volume to a full-viewport RGBA image through the normal
/// viewport moment path, using the family's configured color table. Runs on
/// the background worker; the caller uploads the returned image as a texture
/// on the UI thread.
fn raster_swath_image(
    volume: &RadarVolume,
    moment: SwathMoment,
    color_tables: &ColorTableSet,
    options: ViewportRasterOptions,
) -> Option<egui::ColorImage> {
    let cache = ViewportMomentCache::new_with_color_tables_for_family(
        volume,
        0,
        moment.moment(),
        color_tables,
        Some(moment.color_family()),
    )
    .ok()?;
    let mut pixels = vec![0u8; viewport_rgba_buffer_len(options)];
    let (width, height) = cache
        .render_moment_rgba_into(volume, options, &mut pixels)
        .ok()?;
    Some(radar_color_image_from_rgba(
        [width as usize, height as usize],
        &pixels,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::thread;

    use chrono::{DateTime, Utc};
    use radar_core::{ElevationCut, GateRange, MomentGrid, MomentRow, RadarSite, Radial};

    use crate::{FrameHistoryEntry, FrameStatus, frame_identity_for_volume};

    fn gate_range(gate_count: usize) -> GateRange {
        GateRange {
            first_gate_m: 0,
            gate_spacing_m: 250,
            gate_count,
        }
    }

    /// A single-tilt reflectivity volume with real polar geometry: `nrows`
    /// radials over 360°, value in row `r`, gate `g` from `sample` (raw u8,
    /// scale 2.0 / offset 66, nodata 0 — the WSR-88D dBZ encoding).
    fn volume_with(
        nrows: usize,
        gate_count: usize,
        time_s: i64,
        mut sample: impl FnMut(usize, usize) -> u8,
    ) -> RadarVolume {
        let mut cut = ElevationCut::new(0.5, Some(1));
        for r in 0..nrows {
            cut.radials.push(Radial {
                azimuth_deg: r as f32 * (360.0 / nrows as f32),
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range(gate_count),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range(gate_count),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        for r in 0..nrows {
            let row: Vec<u8> = (0..gate_count).map(|g| sample(r, g)).collect();
            grid.push_row(r, MomentRow::U8(row)).unwrap();
        }
        cut.moments.insert(MomentType::Reflectivity, grid);
        let mut volume = RadarVolume::new(
            RadarSite::new("KEAX"),
            DateTime::<Utc>::from_timestamp(time_s, 0).unwrap(),
        );
        volume.cuts.push(cut);
        volume
    }

    fn dbz_raw(dbz: f32) -> u8 {
        (dbz * 2.0 + 66.0).round() as u8
    }

    /// Frame `index` lights gate `index` (a "moving echo" across the loop).
    fn frame(index: usize) -> FrameHistoryEntry {
        let volume = Arc::new(volume_with(8, 16, 100 + index as i64 * 60, move |_r, g| {
            if g == index { dbz_raw(50.0) } else { 0 }
        }));
        FrameHistoryEntry {
            identity: frame_identity_for_volume(&volume),
            path: PathBuf::from(format!("swath-test-{index}")),
            volume,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "swath test".to_owned(),
        }
    }

    fn viewport() -> (ViewportRasterOptions, ViewportKey) {
        let options = ViewportRasterOptions {
            width: 64,
            height: 64,
            radar_x_px: 32.0,
            radar_y_px: 32.0,
            km_per_px_x: 0.25,
            km_per_px_y: 0.25,
            rotation_rad: 0.0,
        };
        let key = ViewportKey {
            width: 64,
            height: 64,
            radar_x_px: 256,
            radar_y_px: 256,
            km_per_px_x: 250_000,
            km_per_px_y: 250_000,
            rotation_mrad: 0,
        };
        (options, key)
    }

    /// Deadline-poll `ensure` until the layer has built `generation`; panics
    /// if the worker never delivers (hang guard).
    #[allow(clippy::too_many_arguments)]
    fn wait_built(
        layer: &mut SwathLayer,
        ctx: &egui::Context,
        generation: u64,
        frames: &[FrameHistoryEntry],
        tables: &ColorTableSet,
        options: ViewportRasterOptions,
        key: ViewportKey,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while layer.built_generation != Some(generation) || layer.worker.in_flight() {
            assert!(
                Instant::now() < deadline,
                "swath worker never delivered generation {generation}"
            );
            layer.ensure(
                ctx,
                SwathMoment::Reflectivity,
                generation,
                frames,
                tables,
                options,
                key,
                false,
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Finding #8 regression: the paint-path `ensure` call must DISPATCH the
    /// fold to a background worker and return, not run it inline. On the old
    /// code the equivalent call built + rasterized synchronously, so the
    /// texture existed the moment it returned; now the job is in flight and
    /// the result lands asynchronously.
    #[test]
    fn first_build_runs_off_the_paint_thread_and_lands_asynchronously() {
        let ctx = egui::Context::default();
        let (options, key) = viewport();
        let tables = ColorTableSet::default();
        let frames = vec![frame(1), frame(2)];
        let mut layer = SwathLayer::new("test-swath");
        layer.enabled = true;

        layer.ensure(
            &ctx,
            SwathMoment::Reflectivity,
            7,
            &frames,
            &tables,
            options,
            key,
            false,
        );
        assert!(
            layer.worker.in_flight(),
            "the fold must be dispatched to a background worker"
        );
        assert!(
            layer.volume.is_none() && layer.texture.is_none(),
            "the paint path must not block on the fold (old code built inline here)"
        );
        assert_eq!(layer.builds_dispatched, 1);

        wait_built(&mut layer, &ctx, 7, &frames, &tables, options, key);
        assert!(layer.volume.is_some(), "swath volume must land");
        assert!(layer.texture.is_some(), "swath texture must land");
        assert_eq!(layer.viewport_key, Some(key));
    }

    /// Finding #8 regression: while a load streams (generation bumps faster
    /// than the debounce window), NO refold may be dispatched and the stale
    /// texture must keep drawing; once the generation holds still, exactly
    /// one rebuild folds the whole loop. The old code refolded synchronously
    /// on every bump (O(N²) over a streamed load).
    #[test]
    fn streamed_generation_bumps_debounce_into_one_rebuild_keeping_stale_texture() {
        let ctx = egui::Context::default();
        let (options, key) = viewport();
        let tables = ColorTableSet::default();
        let mut frames = vec![frame(1)];
        let mut layer = SwathLayer::new("test-swath");
        layer.enabled = true;

        // First build (immediate — nothing stale on screen yet).
        layer.ensure(
            &ctx,
            SwathMoment::Reflectivity,
            1,
            &frames,
            &tables,
            options,
            key,
            false,
        );
        wait_built(&mut layer, &ctx, 1, &frames, &tables, options, key);
        assert_eq!(layer.builds_dispatched, 1);
        let stale_texture = layer.texture.as_ref().expect("first texture").id();

        // Stream 10 more frames: each push bumps the generation, with a
        // paint-path ensure between pushes (all far faster than the
        // debounce window).
        for generation in 2..=11u64 {
            frames.push(frame(generation as usize));
            layer.ensure(
                &ctx,
                SwathMoment::Reflectivity,
                generation,
                &frames,
                &tables,
                options,
                key,
                false,
            );
            assert!(
                !layer.worker.in_flight(),
                "no refold may be dispatched while the generation is still moving"
            );
            assert_eq!(
                layer.builds_dispatched, 1,
                "streamed bumps must not queue refolds"
            );
            assert_eq!(
                layer.texture.as_ref().map(|texture| texture.id()),
                Some(stale_texture),
                "the stale texture must keep drawing while the stream is live"
            );
        }

        // Stream settles: after the debounce window one rebuild folds ALL
        // frames.
        thread::sleep(SWATH_REBUILD_DEBOUNCE + Duration::from_millis(50));
        layer.ensure(
            &ctx,
            SwathMoment::Reflectivity,
            11,
            &frames,
            &tables,
            options,
            key,
            false,
        );
        assert_eq!(
            layer.builds_dispatched, 2,
            "a settled stream folds exactly once"
        );
        wait_built(&mut layer, &ctx, 11, &frames, &tables, options, key);

        // The single settled rebuild covered the union of every streamed
        // frame (frame i lights gate i).
        let volume = layer.volume.as_ref().expect("rebuilt swath volume");
        let grid = &volume.cuts[0].moments[&MomentType::Reflectivity];
        for gate in 1..=11 {
            assert!(
                (0..grid.radial_count())
                    .any(|row| grid.scaled_value(row, gate).is_some_and(f32::is_finite)),
                "streamed frame lighting gate {gate} missing from the settled swath"
            );
        }
    }

    /// Toggle-off must cancel the in-flight background job along with the
    /// caches, so a disabled layer does no work and a re-enable starts clean.
    #[test]
    fn invalidate_cancels_the_in_flight_build() {
        let ctx = egui::Context::default();
        let (options, key) = viewport();
        let tables = ColorTableSet::default();
        let frames = vec![frame(1), frame(2)];
        let mut layer = SwathLayer::new("test-swath");
        layer.enabled = true;

        layer.ensure(
            &ctx,
            SwathMoment::Reflectivity,
            3,
            &frames,
            &tables,
            options,
            key,
            false,
        );
        assert!(layer.worker.in_flight());
        layer.invalidate();
        assert!(!layer.worker.in_flight(), "cancel must clear the slot");
        assert!(layer.built_generation.is_none() && layer.volume.is_none());
    }

    /// REAL-DATA proof for finding #8 (run explicitly, in release):
    ///
    /// ```text
    /// cargo test -p app_ui --release -- --ignored swath_real_loop
    /// ```
    ///
    /// Streams a cached KEAX (2026-06-09 derecho) loop frame-by-frame
    /// through the paint-path `ensure` and requires that (a) no paint-path
    /// call ever costs more than a small fraction of one synchronous fold —
    /// on the old code the first call ran the whole fold inline — and (b)
    /// after the stream settles the background rebuild lands with every
    /// frame folded in (the swath's per-gate max equals the loop-wide max
    /// and its coverage is at least any single frame's).
    ///
    /// Reads the loop directory from `BOWECHO_SWATH_LOOP_DIR`, defaulting to
    /// the app's own level2 cache for KEAX.
    #[test]
    #[ignore = "heavy real-data proof; needs a cached KEAX/PGUA loop (see doc comment)"]
    fn swath_real_loop_paint_never_blocks_and_folds_all_frames() {
        let dir = std::env::var_os("BOWECHO_SWATH_LOOP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::cache_dir("KEAX"));
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("loop dir {} unreadable: {err}", dir.display()))
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        paths.sort();
        let paths: Vec<PathBuf> = paths.into_iter().take(8).collect();
        assert!(
            paths.len() >= 4,
            "need at least 4 cached volumes in {} for a loop",
            dir.display()
        );

        let frames: Vec<FrameHistoryEntry> = paths
            .iter()
            .map(|path| {
                let raw = std::fs::read(path).expect("read cached volume");
                let volume = Arc::new(
                    nexrad_io::decode_volume_from_bytes(&raw)
                        .unwrap_or_else(|err| panic!("decode {}: {err}", path.display())),
                );
                FrameHistoryEntry {
                    identity: frame_identity_for_volume(&volume),
                    path: path.clone(),
                    volume,
                    timings: None,
                    status: FrameStatus::Complete,
                    source_label: "cached loop".to_owned(),
                }
            })
            .collect();

        // Blocking budget: one synchronous whole-loop fold, measured on the
        // same data. On the old code the first paint-path call cost at least
        // this much; the fix's paint calls must be a small fraction of it.
        let volumes: Vec<Arc<RadarVolume>> = frames
            .iter()
            .map(|frame| Arc::clone(&frame.volume))
            .collect();
        let fold_start = Instant::now();
        let reference = build_swath_volume(&volumes, SwathMoment::Reflectivity)
            .expect("KEAX loop must fold a reflectivity swath");
        let sync_fold = fold_start.elapsed();
        eprintln!(
            "synchronous fold of {} frames: {:.1} ms",
            frames.len(),
            sync_fold.as_secs_f64() * 1000.0
        );

        let ctx = egui::Context::default();
        let tables = ColorTableSet::default();
        // A realistic full-viewport raster target.
        let options = ViewportRasterOptions {
            width: 1600,
            height: 900,
            radar_x_px: 800.0,
            radar_y_px: 450.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            rotation_rad: 0.0,
        };
        let key = ViewportKey {
            width: 1600,
            height: 900,
            radar_x_px: 6400,
            radar_y_px: 3600,
            km_per_px_x: 500_000,
            km_per_px_y: 500_000,
            rotation_mrad: 0,
        };

        let mut layer = SwathLayer::new("test-swath-real");
        layer.enabled = true;
        let mut streamed: Vec<FrameHistoryEntry> = Vec::new();
        let mut max_ensure = Duration::ZERO;
        for (index, frame) in frames.iter().enumerate() {
            streamed.push(frame.clone());
            let generation = index as u64 + 1;
            let ensure_start = Instant::now();
            layer.ensure(
                &ctx,
                SwathMoment::Reflectivity,
                generation,
                &streamed,
                &tables,
                options,
                key,
                false,
            );
            max_ensure = max_ensure.max(ensure_start.elapsed());
        }
        eprintln!(
            "slowest paint-path ensure during the stream: {:.3} ms",
            max_ensure.as_secs_f64() * 1000.0
        );
        assert!(
            max_ensure < sync_fold / 5,
            "paint path blocked: slowest ensure {max_ensure:?} vs synchronous fold {sync_fold:?}"
        );

        // Stream settles → exactly one debounced rebuild folds everything.
        thread::sleep(SWATH_REBUILD_DEBOUNCE + Duration::from_millis(50));
        let final_generation = frames.len() as u64;
        wait_built(
            &mut layer,
            &ctx,
            final_generation,
            &streamed,
            &tables,
            options,
            key,
        );
        assert!(
            layer.builds_dispatched <= 3,
            "streamed load must not refold per frame (got {} folds for {} frames)",
            layer.builds_dispatched,
            frames.len()
        );
        let volume = layer.volume.as_ref().expect("settled swath volume");
        assert!(layer.texture.is_some(), "settled swath texture must land");

        // Correctness on the real loop: the swath's max equals the loop-wide
        // per-frame base-tilt max, and its coverage is at least any single
        // frame's (it is the union of the loop).
        let grid_max = |volume: &RadarVolume| -> (f32, usize) {
            let cut_index =
                render2d::base_tilt_cut(volume, &MomentType::Reflectivity).expect("base tilt");
            let cut = &volume.cuts[cut_index];
            let grid = &cut.moments[&MomentType::Reflectivity];
            let mut max = f32::NEG_INFINITY;
            let mut finite = 0usize;
            for row in 0..grid.radial_count() {
                for gate in 0..grid.gate_range.gate_count {
                    if let Some(value) = grid.scaled_value(row, gate)
                        && value.is_finite()
                    {
                        max = max.max(value);
                        finite += 1;
                    }
                }
            }
            (max, finite)
        };
        let (swath_max, swath_finite) = grid_max(volume);
        let (reference_max, reference_finite) = grid_max(&reference);
        let mut frames_max = f32::NEG_INFINITY;
        let mut best_frame_finite = 0usize;
        for frame in &frames {
            let (frame_max, frame_finite) = grid_max(&frame.volume);
            frames_max = frames_max.max(frame_max);
            best_frame_finite = best_frame_finite.max(frame_finite);
        }
        eprintln!(
            "swath max {swath_max:.1} dBZ over {swath_finite} gates; loop max {frames_max:.1}; densest single frame {best_frame_finite} gates"
        );
        assert!(
            (swath_max - frames_max).abs() < 0.01,
            "swath max {swath_max} must equal loop-wide max {frames_max}"
        );
        assert!(
            (swath_max - reference_max).abs() < 0.01 && swath_finite == reference_finite,
            "worker fold must match the synchronous reference fold"
        );
        assert!(
            swath_finite >= best_frame_finite,
            "swath coverage {swath_finite} must be at least the densest frame's {best_frame_finite}"
        );
    }
}
