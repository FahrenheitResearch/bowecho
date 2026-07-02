//! One coalescing render lane over render2d's PUBLIC API
//! (miniderecho-spec §13 Task 5). This thread deliberately owns its two
//! mpsc channels — it IS mini's RenderService placeholder and is DELETED
//! when `ui_core::render_service` lands at v0.29 Phase 4a/4b (spec §10).
//!
//! Queue discipline: newest-replaces-queued (queue depth 1) — the
//! `merge_render_request` pattern (app_ui main.rs) degenerate to one lane.
//! The worker reuses ONE `ViewportMomentCache` keyed on
//! `(volume Arc ptr, cut, moment)` and one recycled pixel buffer.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use eframe::egui;
use radar_core::{MomentType, RadarVolume};
use render2d::{StormMotion, ViewportMomentCache, ViewportRasterOptions, viewport_rgba_buffer_len};

/// Set to any value to log coalescing (M0 acceptance: "requests coalesced").
const RENDER_DEBUG_ENV: &str = "MINIDERECHO_RENDER_DEBUG";

/// What the lane renders (spec §3.9): a plain moment through
/// `ViewportMomentCache::new`, the dealias family, or storm-relative
/// velocity over a plain VEL cache. Color tables are the render2d/
/// color_tables defaults (Analyst Velocity HD + house REF).
#[derive(Clone, Debug, PartialEq)]
pub enum RenderProduct {
    Moment(MomentType),
    DealiasedVelocity,
    StormRelative { storm_motion: StormMotion },
}

pub struct RenderReq {
    pub volume: Arc<RadarVolume>,
    pub cut_index: usize,
    pub product: RenderProduct,
    pub options: ViewportRasterOptions,
    /// Caller-chosen identity for matching results to requests; stale
    /// results are dropped by key at the drain site.
    pub key: u64,
}

pub enum RenderMsg {
    Done {
        key: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    Failed {
        key: u64,
        error: String,
    },
}

pub struct RenderWorker {
    req_tx: mpsc::Sender<RenderReq>,
    result_rx: mpsc::Receiver<RenderMsg>,
    recycle_tx: mpsc::Sender<Vec<u8>>,
}

impl RenderWorker {
    pub fn spawn(ctx: egui::Context) -> Self {
        Self::spawn_inner(ctx, None)
    }

    fn spawn_inner(ctx: egui::Context, hook: Option<TestHook>) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<RenderReq>();
        let (result_tx, result_rx) = mpsc::channel::<RenderMsg>();
        let (recycle_tx, recycle_rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || worker_loop(req_rx, result_tx, recycle_rx, ctx, hook));
        Self {
            req_tx,
            result_rx,
            recycle_tx,
        }
    }

    /// Fire-and-forget: the lane's newest-wins discipline does the throttling.
    pub fn request(&self, req: RenderReq) {
        let _ = self.req_tx.send(req);
    }

    /// Drain-time results; never blocks.
    pub fn poll(&self) -> Vec<RenderMsg> {
        self.result_rx.try_iter().collect()
    }

    /// Return a consumed pixel buffer to the worker's recycle pool.
    pub fn recycle(&self, pixels: Vec<u8>) {
        let _ = self.recycle_tx.send(pixels);
    }
}

/// Test seam for the queue-discipline contract: the worker reports which
/// request it is about to render, then waits for a release.
struct TestHook {
    started_tx: mpsc::Sender<u64>,
    release_rx: mpsc::Receiver<()>,
}

fn worker_loop(
    req_rx: mpsc::Receiver<RenderReq>,
    result_tx: mpsc::Sender<RenderMsg>,
    recycle_rx: mpsc::Receiver<Vec<u8>>,
    ctx: egui::Context,
    hook: Option<TestHook>,
) {
    let mut cache: Option<(usize, usize, RenderProduct, ViewportMomentCache)> = None;
    let mut recycled: Option<Vec<u8>> = None;
    let debug = std::env::var_os(RENDER_DEBUG_ENV).is_some();
    while let Ok(first) = req_rx.recv() {
        let (req, skipped) = drain_to_newest(first, &req_rx);
        if debug && skipped > 0 {
            eprintln!("render: coalesced {skipped} stale request(s)");
        }
        if let Some(hook) = &hook {
            let _ = hook.started_tx.send(req.key);
            let _ = hook.release_rx.recv();
        }
        while let Ok(buffer) = recycle_rx.try_recv() {
            recycled = Some(buffer);
        }
        let message = render_one(&mut cache, &mut recycled, &req);
        if result_tx.send(message).is_err() {
            break;
        }
        ctx.request_repaint();
    }
}

/// Newest-replaces-queued: everything already queued behind `first` is
/// stale; only the newest unstarted request renders.
fn drain_to_newest(first: RenderReq, rx: &mpsc::Receiver<RenderReq>) -> (RenderReq, usize) {
    let mut newest = first;
    let mut skipped = 0;
    while let Ok(next) = rx.try_recv() {
        newest = next;
        skipped += 1;
    }
    (newest, skipped)
}

/// The `ViewportMomentCache` builder for a render product. The cache is
/// keyed on `(volume Arc ptr, cut, product)` — a storm-relative render
/// shares nothing with a dealiased one even on the same cut.
fn build_cache(
    volume: &RadarVolume,
    cut_index: usize,
    product: &RenderProduct,
) -> render2d::Result<ViewportMomentCache> {
    match product {
        RenderProduct::Moment(moment) => {
            ViewportMomentCache::new(volume, cut_index, moment.clone())
        }
        RenderProduct::DealiasedVelocity => {
            ViewportMomentCache::new_dealiased_velocity(volume, cut_index)
        }
        // SRV subtracts the motion at render time over a plain VEL cache
        // (the cache builds the storm-motion basis when moment == VEL).
        RenderProduct::StormRelative { .. } => {
            ViewportMomentCache::new(volume, cut_index, MomentType::Velocity)
        }
    }
}

fn render_one(
    cache: &mut Option<(usize, usize, RenderProduct, ViewportMomentCache)>,
    recycled: &mut Option<Vec<u8>>,
    req: &RenderReq,
) -> RenderMsg {
    let volume_ptr = Arc::as_ptr(&req.volume) as usize;
    let cache_hit = matches!(
        cache,
        Some((ptr, cut, product, _))
            if *ptr == volume_ptr && *cut == req.cut_index && *product == req.product
    );
    if !cache_hit {
        match build_cache(&req.volume, req.cut_index, &req.product) {
            Ok(built) => *cache = Some((volume_ptr, req.cut_index, req.product.clone(), built)),
            Err(error) => {
                *cache = None;
                return RenderMsg::Failed {
                    key: req.key,
                    error: error.to_string(),
                };
            }
        }
    }
    let Some((_, _, _, moment_cache)) = cache else {
        unreachable!("cache populated above");
    };

    let len = viewport_rgba_buffer_len(req.options);
    let mut pixels = recycled.take().unwrap_or_default();
    pixels.clear();
    pixels.resize(len, 0);
    let rendered = match &req.product {
        RenderProduct::StormRelative { storm_motion } => moment_cache
            .render_storm_relative_velocity_rgba_into(
                &req.volume,
                *storm_motion,
                req.options,
                &mut pixels,
            ),
        _ => moment_cache.render_moment_rgba_into(&req.volume, req.options, &mut pixels),
    };
    match rendered {
        Ok((width, height)) => RenderMsg::Done {
            key: req.key,
            width,
            height,
            pixels,
        },
        Err(error) => {
            *recycled = Some(pixels);
            RenderMsg::Failed {
                key: req.key,
                error: error.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use radar_core::{ElevationCut, GateRange, MomentGrid, RadarSite, Radial};
    use std::time::Duration;

    fn test_volume() -> Arc<RadarVolume> {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 6,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        for azimuth_deg in [0.0, 90.0, 180.0, 270.0] {
            cut.radials.push(Radial {
                azimuth_deg,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(32.0),
                radial_status: None,
            });
        }
        let mut reflectivity = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        let mut velocity = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            2.0,
            129.0,
            Some(0),
            Some(1),
        );
        for radial_index in 0..4 {
            reflectivity
                .push_u8_row_slice(radial_index, &[20, 30, 40, 50, 60, 70])
                .expect("reflectivity row");
            velocity
                .push_u8_row_slice(radial_index, &[100, 110, 120, 130, 140, 150])
                .expect("velocity row");
        }
        cut.moments.insert(MomentType::Reflectivity, reflectivity);
        cut.moments.insert(MomentType::Velocity, velocity);
        let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());
        volume.cuts.push(cut);
        Arc::new(volume)
    }

    fn options() -> ViewportRasterOptions {
        ViewportRasterOptions {
            width: 64,
            height: 64,
            radar_x_px: 32.0,
            radar_y_px: 32.0,
            km_per_px_x: 0.25,
            km_per_px_y: 0.25,
            rotation_rad: 0.0,
        }
    }

    fn req(volume: &Arc<RadarVolume>, key: u64) -> RenderReq {
        RenderReq {
            volume: Arc::clone(volume),
            cut_index: 0,
            product: RenderProduct::Moment(MomentType::Reflectivity),
            options: options(),
            key,
        }
    }

    #[test]
    fn drain_to_newest_keeps_only_the_last_queued_request() {
        let volume = test_volume();
        let (tx, rx) = mpsc::channel();
        tx.send(req(&volume, 2)).unwrap();
        tx.send(req(&volume, 3)).unwrap();

        let (newest, skipped) = drain_to_newest(req(&volume, 1), &rx);
        assert_eq!(newest.key, 3);
        assert_eq!(skipped, 2);
    }

    /// §13 Task 5's required test: push 3 requests while the worker is
    /// blocked on the first job; only the newest unstarted one renders.
    #[test]
    fn queue_discipline_renders_first_then_newest_only() {
        let volume = test_volume();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = RenderWorker::spawn_inner(
            egui::Context::default(),
            Some(TestHook {
                started_tx,
                release_rx,
            }),
        );

        let timeout = Duration::from_secs(10);
        worker.request(req(&volume, 1));
        assert_eq!(started_rx.recv_timeout(timeout).unwrap(), 1);
        // Worker is now blocked "rendering" request 1: queue three more.
        worker.request(req(&volume, 2));
        worker.request(req(&volume, 3));
        worker.request(req(&volume, 4));
        release_tx.send(()).unwrap();

        // Requests 2 and 3 were overwritten while queued; 4 starts next.
        assert_eq!(started_rx.recv_timeout(timeout).unwrap(), 4);
        release_tx.send(()).unwrap();

        let mut rendered = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        while rendered.len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "results never arrived"
            );
            for message in worker.poll() {
                match message {
                    RenderMsg::Done {
                        key,
                        width,
                        height,
                        pixels,
                    } => {
                        assert_eq!((width, height), (64, 64));
                        assert_eq!(pixels.len(), 64 * 64 * 4);
                        rendered.push(key);
                    }
                    RenderMsg::Failed { error, .. } => panic!("render failed: {error}"),
                }
            }
            thread::yield_now();
        }
        assert_eq!(rendered, vec![1, 4]);
        assert!(
            started_rx.try_recv().is_err(),
            "requests 2/3 must never start"
        );
    }

    /// Every M1 render-product family produces a raster through the one
    /// cache/recycle path (M1: DVEL via the dealias family, SRV via the
    /// storm-relative render over a plain VEL cache).
    #[test]
    fn every_render_product_family_renders_through_the_shared_cache_path() {
        let volume = test_volume();
        let mut cache = None;
        let mut recycled = None;
        for (key, product) in [
            (1, RenderProduct::Moment(MomentType::Reflectivity)),
            (2, RenderProduct::Moment(MomentType::Velocity)),
            (3, RenderProduct::DealiasedVelocity),
            (
                4,
                RenderProduct::StormRelative {
                    storm_motion: StormMotion {
                        direction_deg: 45.0,
                        speed_mps: 18.0,
                    },
                },
            ),
        ] {
            let message = render_one(
                &mut cache,
                &mut recycled,
                &RenderReq {
                    volume: Arc::clone(&volume),
                    cut_index: 0,
                    product: product.clone(),
                    options: options(),
                    key,
                },
            );
            match message {
                RenderMsg::Done {
                    key: got,
                    width,
                    height,
                    pixels,
                } => {
                    assert_eq!(got, key);
                    assert_eq!((width, height), (64, 64));
                    assert_eq!(pixels.len(), 64 * 64 * 4);
                    recycled = Some(pixels);
                }
                RenderMsg::Failed { error, .. } => panic!("{product:?} failed: {error}"),
            }
            // The cache is keyed on the product: each switch rebuilt it.
            let (_, _, cached_product, _) = cache.as_ref().expect("cache populated");
            assert_eq!(cached_product, &product);
        }

        // A missing base moment is a Failed message, not a panic.
        let message = render_one(
            &mut cache,
            &mut recycled,
            &RenderReq {
                volume: Arc::clone(&volume),
                cut_index: 0,
                product: RenderProduct::Moment(MomentType::CorrelationCoefficient),
                options: options(),
                key: 9,
            },
        );
        assert!(matches!(message, RenderMsg::Failed { key: 9, .. }));
    }
}
