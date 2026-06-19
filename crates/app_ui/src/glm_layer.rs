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
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
        if let Some(message) = repair_seen_only_manifest(&store_root, satellite) {
            let _ = status_tx.send(message);
        }
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
                let _ = status_tx.send(format!("GLM follow ended: {error}"));
                ctx_clone.request_repaint();
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

    /// Flashes valid for a frame at `frame_ms`: trailing display window
    /// (`window_min` minutes, the style registry's `glm.window_minutes`),
    /// QC-filtered. Age returned as 0..1 (0 = newest).
    pub fn frame_flashes(
        &self,
        frame_ms: i64,
        window_min: i64,
    ) -> impl Iterator<Item = (&rw_glm::Flash, f32)> {
        let window_ms = window_min.max(1) * 60_000;
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

/// Self-heal a poisoned GLM store: if a prior run recorded granule keys in
/// `window.json` but no flash buckets survived, the follow engine would dedup
/// those granules forever and the layer would stay blank. Removing only that
/// empty manifest lets the next poll backfill the current hour normally.
fn repair_seen_only_manifest(store_root: &Path, satellite: &str) -> Option<String> {
    let sat_dir = store_root.join("glm").join(satellite);
    let manifest_path = sat_dir.join("window.json");
    let bytes = fs::read(&manifest_path).ok()?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let seen_count = manifest
        .get("seen_granule_keys")
        .and_then(|value| value.as_array())
        .map_or(0, |items| items.len());
    if seen_count == 0 {
        return None;
    }
    let has_time_extent = manifest
        .get("time_min_unix_ms")
        .is_some_and(|value| !value.is_null())
        || manifest
            .get("time_max_unix_ms")
            .is_some_and(|value| !value.is_null());
    if has_time_extent || has_rwl_buckets(&sat_dir) {
        return None;
    }
    match fs::remove_file(&manifest_path) {
        Ok(()) => Some(format!(
            "GLM repaired empty store manifest ({seen_count} seen keys, no buckets); backfilling"
        )),
        Err(error) => Some(format!("GLM empty manifest repair failed: {error}")),
    }
}

fn has_rwl_buckets(sat_dir: &Path) -> bool {
    let Ok(days) = fs::read_dir(sat_dir) else {
        return false;
    };
    for day in days.flatten() {
        let day_path = day.path();
        if !day_path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(day_path) else {
            continue;
        };
        for file in files.flatten() {
            if file
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rwl"))
            {
                return true;
            }
        }
    }
    false
}

impl Drop for GlmWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Age-faded flash color: a per-channel ramp from the style's fresh color
/// (default near-white yellow) to its aged color (default dim red-orange —
/// the convention every lightning display uses). Truncating `as u8` casts
/// kept bit-identical to the original hard-coded ramp.
pub fn flash_color(age01: f32, style: &styles::GlmStyle) -> egui::Color32 {
    let a = age01.clamp(0.0, 1.0);
    let channel = |fresh: u8, aged: u8| (fresh as f32 + (aged as f32 - fresh as f32) * a) as u8;
    egui::Color32::from_rgba_unmultiplied(
        channel(style.fresh_color[0], style.aged_color[0]),
        channel(style.fresh_color[1], style.aged_color[1]),
        channel(style.fresh_color[2], style.aged_color[2]),
        channel(style.fresh_color[3], style.aged_color[3]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_repair_removes_seen_only_manifest_without_buckets() {
        let root = std::env::temp_dir().join(format!("bowecho-glm-repair-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sat_dir = root.join("glm").join("goes19");
        fs::create_dir_all(&sat_dir).unwrap();
        fs::write(
            sat_dir.join("window.json"),
            r#"{
                "schema": "rw-glm.window.v1",
                "satellite": "goes19",
                "time_min_unix_ms": null,
                "time_max_unix_ms": null,
                "seen_granule_keys": ["OR_GLM-L2-LCFA_G19_s1_e2_c3"]
            }"#,
        )
        .unwrap();

        let message = repair_seen_only_manifest(&root, "goes19").unwrap();

        assert!(message.contains("repaired"));
        assert!(!sat_dir.join("window.json").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn glm_repair_keeps_manifest_when_bucket_exists() {
        let root =
            std::env::temp_dir().join(format!("bowecho-glm-repair-bucket-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sat_dir = root.join("glm").join("goes19");
        fs::create_dir_all(sat_dir.join("20260619")).unwrap();
        fs::write(
            sat_dir.join("window.json"),
            r#"{
                "schema": "rw-glm.window.v1",
                "satellite": "goes19",
                "time_min_unix_ms": null,
                "time_max_unix_ms": null,
                "seen_granule_keys": ["OR_GLM-L2-LCFA_G19_s1_e2_c3"]
            }"#,
        )
        .unwrap();
        fs::write(sat_dir.join("20260619").join("t2000.rwl"), b"placeholder").unwrap();

        assert!(repair_seen_only_manifest(&root, "goes19").is_none());
        assert!(sat_dir.join("window.json").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
