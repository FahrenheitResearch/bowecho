//! Radar raster quality: maps the user's Standard/High/Ultra choice (and the
//! "Apply high-res to the whole loop" toggle) onto the supersample factor used
//! for the displayed radar frame vs the loop render cache, plus the loop-cache
//! byte budget raise that lets whole-loop mode actually hold its frames.
//!
//! The pure decision helpers live here so they unit-test without building a
//! `ViewerApp`; the `ViewerApp` methods are thin wrappers over `app_settings`
//! plus playback state. `viewport_key_for_options` is shared with `main.rs` so
//! a supersampled [`ViewportRasterOptions`] gets a cache key matching how the
//! base raster is keyed in `viewport_raster_options_for_location`.

use render2d::ViewportRasterOptions;
use settings::{RasterQuality, loop_cache_estimate_bytes};

use crate::{ViewerApp, ViewportKey};

/// RGBA bytes for the largest raster a single frame can occupy — the 4096²
/// supersample ceiling. Used as the per-frame upper bound for the whole-loop
/// budget so no frame is ever evicted in that mode.
const MAX_VIEWPORT_RASTER_BYTES: usize =
    (ViewportRasterOptions::MAX_SUPERSAMPLED_DIMENSION as usize).pow(2) * 4;

/// Build the cache [`ViewportKey`] for a (possibly supersampled)
/// [`ViewportRasterOptions`]. Centralized so the displayed / loop render
/// requests key their supersampled rasters identically to how
/// `viewport_raster_options_for_location` keys the base raster.
pub(crate) fn viewport_key_for_options(options: ViewportRasterOptions) -> ViewportKey {
    ViewportKey {
        width: options.width,
        height: options.height,
        radar_x_px: (options.radar_x_px * 8.0).round() as i32,
        radar_y_px: (options.radar_y_px * 8.0).round() as i32,
        km_per_px_x: (options.km_per_px_x * 1_000_000.0).round() as i32,
        km_per_px_y: (options.km_per_px_y * 1_000_000.0).round() as i32,
        rotation_mrad: (options.rotation_rad * 1000.0).round() as i16,
    }
}

/// Supersample factor for the frame the user is looking at. During active loop
/// playback / recording the displayed frame rides the lightweight loop cache,
/// so it stays Standard unless the whole-loop toggle raises the whole cache.
fn displayed_supersample_factor(quality_factor: u32, whole_loop: bool, loop_active: bool) -> u32 {
    if whole_loop || !loop_active {
        quality_factor
    } else {
        1
    }
}

/// Supersample factor for loop-cache / prewarm frames: Standard unless the
/// whole-loop toggle is on.
fn loop_supersample_factor(quality_factor: u32, whole_loop: bool) -> u32 {
    if whole_loop { quality_factor } else { 1 }
}

/// Loop render-cache byte budget. When the whole-loop toggle is on the ceiling
/// is raised to hold every frame at the 4096² per-frame maximum — this WARNS
/// (via the settings estimate) but never blocks: the loop total is deliberately
/// not capped below the user's selection. The ceiling gates eviction only; the
/// memory actually used is the sum of the (smaller, rect-sized) frames inserted.
fn whole_loop_budget_bytes(base: usize, whole_loop: bool, frame_count: usize) -> usize {
    if whole_loop {
        base.max(frame_count.max(1).saturating_mul(MAX_VIEWPORT_RASTER_BYTES))
    } else {
        base
    }
}

impl ViewerApp {
    pub(crate) fn raster_quality(&self) -> RasterQuality {
        RasterQuality::from_slug(&self.app_settings.raster_quality)
    }

    /// True while the loop is playing or a loop recording is in progress — the
    /// window during which the displayed frame stays lightweight.
    fn raster_loop_active(&self) -> bool {
        self.primary.cursor.playing || self.media.loop_recording()
    }

    /// Supersample factor for the primary displayed frame and the extra panes.
    pub(crate) fn displayed_raster_supersample_factor(&self) -> u32 {
        displayed_supersample_factor(
            self.raster_quality().supersample_factor(),
            self.app_settings.raster_high_res_whole_loop,
            self.raster_loop_active(),
        )
    }

    /// Supersample factor for loop-cache / prewarm frames.
    pub(crate) fn loop_raster_supersample_factor(&self) -> u32 {
        loop_supersample_factor(
            self.raster_quality().supersample_factor(),
            self.app_settings.raster_high_res_whole_loop,
        )
    }

    /// Loop render-cache byte budget, raised in whole-loop mode.
    pub(crate) fn raster_quality_loop_cache_budget_bytes(&self) -> usize {
        whole_loop_budget_bytes(
            crate::loop_render_cache_budget_bytes_for_threads(crate::effective_worker_threads()),
            self.app_settings.raster_high_res_whole_loop,
            self.primary.history.len(),
        )
    }

    /// Estimated bytes to cache the whole current loop at the chosen quality —
    /// the figure shown next to the whole-loop toggle.
    pub(crate) fn whole_loop_cache_estimate_bytes(&self) -> u64 {
        loop_cache_estimate_bytes(self.raster_quality(), self.primary.history.len())
    }
}

/// Human-readable memory estimate for the settings UI: GB at/above ~1 GiB,
/// otherwise MB. Cosmetic; the byte math itself is tested in the settings crate.
pub(crate) fn format_estimate_bytes(bytes: u64) -> String {
    const MIB: f64 = (1u64 << 20) as f64;
    const GIB: f64 = (1u64 << 30) as f64;
    if bytes >= (1u64 << 30) {
        format!("{:.1} GB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MB", bytes as f64 / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displayed_factor_is_standard_during_playback_unless_whole_loop() {
        // whole-loop off: stopped uses the chosen factor, active playback drops
        // to Standard so the loop cache stays lightweight.
        assert_eq!(displayed_supersample_factor(4, false, false), 4);
        assert_eq!(displayed_supersample_factor(4, false, true), 1);
        // whole-loop on: always the chosen factor.
        assert_eq!(displayed_supersample_factor(4, true, true), 4);
        assert_eq!(displayed_supersample_factor(4, true, false), 4);
        // Standard quality is 1x regardless.
        assert_eq!(displayed_supersample_factor(1, false, false), 1);
        assert_eq!(displayed_supersample_factor(1, false, true), 1);
    }

    #[test]
    fn loop_factor_is_standard_unless_whole_loop() {
        assert_eq!(loop_supersample_factor(4, false), 1);
        assert_eq!(loop_supersample_factor(2, false), 1);
        assert_eq!(loop_supersample_factor(4, true), 4);
        assert_eq!(loop_supersample_factor(2, true), 2);
        assert_eq!(loop_supersample_factor(1, true), 1);
    }

    #[test]
    fn whole_loop_budget_raises_ceiling_only_when_toggled() {
        let base = 96 * 1024 * 1024;
        // Off: untouched (bit-identical to today).
        assert_eq!(whole_loop_budget_bytes(base, false, 60), base);
        // On: at least frames * 64 MiB, and never below the base budget.
        let raised = whole_loop_budget_bytes(base, true, 60);
        assert_eq!(raised, 60 * MAX_VIEWPORT_RASTER_BYTES);
        assert!(raised >= base);
        // A tiny loop still holds at least the base budget.
        assert_eq!(
            whole_loop_budget_bytes(base, true, 1),
            base.max(MAX_VIEWPORT_RASTER_BYTES)
        );
    }

    #[test]
    fn max_viewport_raster_bytes_is_64_mib() {
        assert_eq!(MAX_VIEWPORT_RASTER_BYTES, 64 << 20);
    }

    #[test]
    fn format_estimate_reads_mb_then_gb() {
        assert_eq!(format_estimate_bytes(64 << 20), "64 MB");
        assert_eq!(format_estimate_bytes(4 << 20), "4 MB");
        let ultra_60 = format_estimate_bytes(60 * (64 << 20));
        assert!(ultra_60.ends_with(" GB"), "{ultra_60}");
        assert!(ultra_60.starts_with("3.8"), "{ultra_60}");
    }
}
