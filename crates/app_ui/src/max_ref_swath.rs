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
//! Caching (spec: "so it isn't recomputed every frame"): the synthetic volume
//! is rebuilt only when the loop's frame set changes, keyed off
//! `FrameHistory::generation()` (an O(1) content stamp). The rasterized
//! texture is rebuilt only when the swath, palette, or (while the map is
//! idle) the viewport changes; during pan/zoom the cached texture is
//! reprojected as a quad, exactly like the primary radar layer between
//! re-renders.

use std::sync::Arc;

use color_tables::{ColorTableFamily, ColorTableSet};
use eframe::egui;
use radar_core::{MomentType, RadarVolume};
use render2d::{
    SwathAggregation, ViewportMomentCache, ViewportRasterOptions, max_value_swath,
    viewport_rgba_buffer_len,
};

use crate::{
    LayerRowOpacity, LayerRowSpec, LayerRowVis, NAME_W_STD, ViewerApp, ViewportKey,
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
}

/// Default overlay alpha — a semi-transparent trail that reads as distinct
/// from the opaque live frame drawn over it.
const DEFAULT_SWATH_OPACITY: u8 = 190;

/// One swath overlay layer: its toggle, opacity, and lazily-built caches.
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
}

impl Default for SwathLayer {
    fn default() -> Self {
        Self {
            enabled: false,
            opacity: DEFAULT_SWATH_OPACITY,
            built_generation: None,
            volume: None,
            texture: None,
            viewport_key: None,
            rotation_rad: 0.0,
            color_signature: 0,
        }
    }
}

impl SwathLayer {
    /// Drop every cache (on toggle-off, or when a rebuild is forced) so a
    /// re-enable recomputes from the current loop.
    fn invalidate(&mut self) {
        self.built_generation = None;
        self.volume = None;
        self.texture = None;
        self.viewport_key = None;
    }
}

/// Both swath overlays.
#[derive(Default)]
pub(crate) struct SwathState {
    pub(crate) reflectivity: SwathLayer,
    pub(crate) velocity: SwathLayer,
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
            "Peak reflectivity accumulated over the loaded loop — the storm's track / where it has been (drawn beneath the live frame). Recomputed only when the loop changes.",
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
        let frame_count = self.primary.history.len();
        let trailing_text = if !self.swath.layer(moment).enabled {
            String::new()
        } else if built {
            format!("{frame_count} frames")
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
                name_width: NAME_W_STD,
                name_hover: hover,
                opacity: Some(LayerRowOpacity::U8 {
                    value: &mut layer.opacity,
                    min: 40,
                    max: 255,
                }),
                ..Default::default()
            },
            |ui| {
                if !trailing_text.is_empty() {
                    ui.weak(trailing_text);
                }
            },
        );
        if changed {
            // Toggling off frees the caches; toggling on rebuilds on the next
            // draw from the current loop.
            if was_enabled && !self.swath.layer(moment).enabled {
                self.swath.layer_mut(moment).invalidate();
            }
            self.save_overlay_defaults();
            ctx.request_repaint();
        }
    }

    /// Ensure caches are current and draw both enabled swaths. Called once per
    /// frame, beneath the primary radar layer.
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
        // synchronously (matches the primary radar layer's between-render
        // behavior).
        let reproject_only = self.loop_prewarm_paused_for_interaction()
            || self.smooth_camera_follow_playback_active();

        for moment in [SwathMoment::Reflectivity, SwathMoment::Velocity] {
            if !self.swath.layer(moment).enabled {
                continue;
            }
            self.ensure_swath_layer(moment, ctx, options, key, reproject_only);
            self.paint_swath_layer(moment, painter, rect, ctx, radar_lat, radar_lon);
        }
    }

    /// Rebuild the swath volume when the loop changed, then (re)rasterize its
    /// texture when the swath, palette, or resting viewport changed.
    fn ensure_swath_layer(
        &mut self,
        moment: SwathMoment,
        ctx: &egui::Context,
        options: ViewportRasterOptions,
        key: ViewportKey,
        reproject_only: bool,
    ) {
        let generation = self.primary.history.generation();
        let signature = self
            .color_tables
            .signature_for_family(moment.color_family());

        // 1. Rebuild the synthetic volume if the loop's frame set changed.
        if self.swath.layer(moment).built_generation != Some(generation) {
            let volume = build_swath_volume(&self.primary.history, moment).map(Arc::new);
            let layer = self.swath.layer_mut(moment);
            layer.volume = volume;
            layer.built_generation = Some(generation);
            layer.texture = None;
        }

        // 2. (Re)rasterize the texture when needed.
        let need_raster = {
            let layer = self.swath.layer(moment);
            layer.volume.is_some()
                && (layer.texture.is_none()
                    || layer.color_signature != signature
                    || (layer.viewport_key != Some(key) && !reproject_only))
        };
        if !need_raster {
            return;
        }
        let Some(volume) = self.swath.layer(moment).volume.clone() else {
            return;
        };
        let color_tables = self.color_tables.clone();
        let Some(texture) = raster_swath_texture(ctx, &volume, moment, &color_tables, options)
        else {
            return;
        };
        let layer = self.swath.layer_mut(moment);
        layer.texture = Some(texture);
        layer.viewport_key = Some(key);
        layer.rotation_rad = key.rotation_mrad as f32 / 1000.0;
        layer.color_signature = signature;
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

/// Build the synthetic swath volume from the loop history's frame volumes.
fn build_swath_volume(
    frames: &[crate::FrameHistoryEntry],
    moment: SwathMoment,
) -> Option<RadarVolume> {
    let volumes: Vec<&RadarVolume> = frames.iter().map(|frame| frame.volume.as_ref()).collect();
    max_value_swath(&volumes, moment.moment(), moment.aggregation())
}

/// Rasterize a swath volume to an egui texture through the normal viewport
/// moment path, using the family's configured color table.
fn raster_swath_texture(
    ctx: &egui::Context,
    volume: &RadarVolume,
    moment: SwathMoment,
    color_tables: &ColorTableSet,
    options: ViewportRasterOptions,
) -> Option<egui::TextureHandle> {
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
    let image = radar_color_image_from_rgba([width as usize, height as usize], &pixels);
    Some(ctx.load_texture(
        match moment {
            SwathMoment::Reflectivity => "max-ref-swath",
            SwathMoment::Velocity => "max-vel-swath",
        },
        image,
        radar_texture_options(),
    ))
}
