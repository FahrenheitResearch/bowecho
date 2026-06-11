//! GLM lightning layer: GOES Geostationary Lightning Mapper flashes on
//! the radar map, time-synced to the displayed frame.
//!
//! Data flow: rw-glm's follow engine runs in-process on a background
//! thread (S3 poll → granule decode → rolling `.rwl` store, ~20 s
//! granules), and the layer reads flashes back by time range + viewport
//! bbox. BowEcho owns its own GLM store dir — sharing a store between
//! apps was the sat-store lesson; rw-glm's writer locks make it safe but
//! separate stores avoid pruning-policy fights.
//!
//! Display follows the operational lightning-layer convention: flashes
//! from the trailing window before the FRAME time (so loops replay
//! lightning history in sync with the radar), age-faded, degraded-quality
//! flashes QC-filtered out.

use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Trailing window drawn at each frame, minutes.
pub const DISPLAY_WINDOW_MIN: i64 = 10;
/// Rolling store window (the follow engine prunes beyond this).
const STORE_WINDOW: Duration = Duration::from_secs(3 * 3600);

pub struct GlmWorker {
    pub satellite: String,
    cancel: Arc<AtomicBool>,
    /// Latest follow-engine status line (events forwarded from the sink).
    pub status_rx: mpsc::Receiver<String>,
    pub last_status: String,
    /// Cached flashes for the current read window + the read parameters
    /// that produced them.
    pub flashes: Vec<rw_glm::Flash>,
    pub fetched_at: Option<Instant>,
    store_root: PathBuf,
}

impl GlmWorker {
    /// Spawn the in-process follow engine and return the layer handle.
    pub fn spawn(ctx: &egui::Context, satellite: &str, store_root: PathBuf) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let (status_tx, status_rx) = mpsc::channel();
        let mut spec = rw_glm::GlmFollowSpec::new(satellite, store_root.clone());
        spec.window = STORE_WINDOW;
        let cancel_thread = Arc::clone(&cancel);
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            // Polite: background priority + bounded pool, same as ingest.
            rw_ingest::throttle::set_current_thread_background_priority();
            let mut sink = |event: rw_glm::GlmEvent| {
                let line = format!("{event:?}");
                let _ = status_tx.send(line);
                ctx_clone.request_repaint();
            };
            let result = rw_glm::follow_live(&spec, &mut sink, &cancel_thread);
            if let Err(error) = result
                && !error.is_cancelled()
            {
                // Channel may be gone if the layer was removed; best-effort.
                let _ = std::io::Write::write_all(
                    &mut std::io::sink(),
                    format!("glm follow ended: {error}").as_bytes(),
                );
            }
        });
        Self {
            satellite: satellite.to_owned(),
            cancel,
            status_rx,
            last_status: "starting GLM follow…".to_owned(),
            flashes: Vec::new(),
            fetched_at: None,
            store_root,
        }
    }

    /// Drain follow-engine events and refresh the flash cache (~every 10 s
    /// — granules land every ~20 s).
    pub fn pump(&mut self) {
        let mut got_event = false;
        while let Ok(line) = self.status_rx.try_recv() {
            self.last_status = line;
            got_event = true;
        }
        let stale = self
            .fetched_at
            .map(|at| at.elapsed() > Duration::from_secs(10))
            .unwrap_or(true);
        if got_event || stale {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let t0 = now_ms - (STORE_WINDOW.as_secs() as i64) * 1000;
            if let Ok(flashes) =
                rw_glm::read_flashes(&self.store_root, &self.satellite, t0, now_ms, None)
            {
                self.flashes = flashes;
            }
            self.fetched_at = Some(Instant::now());
        }
    }

    /// Flashes valid for a frame at `frame_ms`: trailing display window,
    /// QC-filtered. Age returned as 0..1 (0 = newest).
    pub fn frame_flashes(&self, frame_ms: i64) -> impl Iterator<Item = (&rw_glm::Flash, f32)> {
        let window_ms = DISPLAY_WINDOW_MIN * 60_000;
        self.flashes.iter().filter_map(move |flash| {
            if flash.is_degraded() {
                return None;
            }
            let age_ms = frame_ms - flash.time_unix_ms;
            (age_ms >= 0 && age_ms <= window_ms)
                .then_some((flash, age_ms as f32 / window_ms as f32))
        })
    }
}

impl Drop for GlmWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Age-faded flash color: fresh = near-white yellow, old = dim red-orange
/// (the convention every lightning display uses).
pub fn flash_color(age01: f32) -> egui::Color32 {
    let a = age01.clamp(0.0, 1.0);
    let r = 255.0;
    let g = 235.0 - 160.0 * a;
    let b = 120.0 - 110.0 * a;
    let alpha = 235.0 - 150.0 * a;
    egui::Color32::from_rgba_unmultiplied(r as u8, g as u8, b as u8, alpha as u8)
}
