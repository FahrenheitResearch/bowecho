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
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Retain enough event history for long operator loops. A 2000-frame radar loop
/// can span nearly a week at a 5-minute scan cadence, and each GLM flash is a
/// compact fixed-width record. Reads stay windowed to the active loop, so this
/// retention does not mean the UI constantly loads seven days of lightning.
const STORE_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_READ_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Live-mode reads anchor to the wall clock. Snapping the live edge UP to
/// this quantum keeps the memoized read range stable across repaints (~16 ms
/// apart), so `pump_for_window` reuses the cached flashes instead of
/// re-reading the store from disk at repaint rate; ingest events and the
/// 10 s stale fallback still refresh the live edge promptly.
const LIVE_READ_QUANTUM: Duration = Duration::from_secs(5);
/// Manual refresh starts near the live edge instead of downloading the whole
/// current UTC hour. Normal startup intentionally leaves the acquisition cutoff
/// unset: the follow engine already processes newest-first, and avoiding a
/// startup cutoff keeps clock/filter edge cases from making a healthy feed look
/// empty.
pub const LIVE_BOOTSTRAP_WINDOW_MINUTES: i64 = 10;
const REPAIR_SEEN_AHEAD_MS: i64 = 10 * 60_000;

pub struct GlmWorker {
    pub satellite: String,
    cancel: Arc<AtomicBool>,
    /// Latest follow-engine status line (events forwarded from the sink).
    pub status_rx: mpsc::Receiver<String>,
    pub last_status: String,
    /// Cached flashes for the current read window + the read parameters
    /// that produced them. Kept sorted ascending by `time_unix_ms`
    /// (`ingest_flashes` restores the invariant), so `frame_flashes` can
    /// binary-search the trailing display window instead of scanning the
    /// whole read window — which for a long loop can be a week of flashes.
    flashes: Vec<rw_glm::Flash>,
    pub fetched_at: Option<Instant>,
    pub last_read_count: usize,
    pub last_read_error: Option<String>,
    pub latest_flash_time_ms: Option<i64>,
    store_root: PathBuf,
    ignore_flashes_before_ms: Option<i64>,
    restart_requested: bool,
    last_health_check: Option<Instant>,
    last_read_range_ms: Option<(i64, i64)>,
}

fn glm_event_status(event: &rw_glm::GlmEvent) -> String {
    match event {
        rw_glm::GlmEvent::Listing { .. } => "checking newest GLM files".to_owned(),
        rw_glm::GlmEvent::GranuleFetched { bytes, .. } => {
            format!("downloading GLM file ({} KB)", bytes / 1024)
        }
        rw_glm::GlmEvent::GranuleDecoded { flashes, .. } => {
            format!("decoded {flashes} flashes")
        }
        rw_glm::GlmEvent::BucketWritten { records, .. } => {
            format!("stored {records} flashes")
        }
        rw_glm::GlmEvent::GranuleSkipped { reason, .. } => match reason {
            rw_glm::SkipReason::AlreadySeen => "current GLM file already processed".to_owned(),
            rw_glm::SkipReason::PermanentDecodeError => {
                "skipped corrupt GLM file; waiting for next".to_owned()
            }
            rw_glm::SkipReason::RetriesExhausted => {
                "GLM download retries exhausted; waiting for next".to_owned()
            }
            rw_glm::SkipReason::Holdback { retry_in_secs } => {
                format!("GLM file retry in {retry_in_secs}s")
            }
        },
        rw_glm::GlmEvent::Pruned { report } => {
            format!("trimmed {} old GLM buckets", report.removed_buckets)
        }
        rw_glm::GlmEvent::PollSleep { secs } => {
            format!("waiting {secs}s for next GLM poll")
        }
        rw_glm::GlmEvent::Info { message } => {
            if message.contains("dedup seeded") {
                "resuming GLM live state".to_owned()
            } else if message.contains("dedup not persisted") {
                "checked GLM file with no live flashes".to_owned()
            } else if message.contains("all older than the window cutoff") {
                "checked GLM file older than live window".to_owned()
            } else {
                compact_glm_status(message, 64)
            }
        }
        rw_glm::GlmEvent::Warning { message } => {
            format!("warning: {}", compact_glm_status(message, 56))
        }
    }
}

fn compact_glm_status(status: &str, max_chars: usize) -> String {
    let trimmed = status.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out = trimmed.chars().take(keep).collect::<String>();
    out.push_str("...");
    out
}

fn append_glm_debug(store_root: &Path, satellite: &str, message: impl AsRef<str>) {
    if std::env::var_os("BOWECHO_GLM_DEBUG").is_none() {
        return;
    }
    let path = store_root.join("glm-worker.log");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = writeln!(
        file,
        "{} pid={} sat={} {}",
        chrono::Utc::now().to_rfc3339(),
        std::process::id(),
        satellite,
        message.as_ref()
    );
}

impl GlmWorker {
    /// Spawn the in-process follow engine and return the layer handle.
    pub fn spawn(
        ctx: &egui::Context,
        satellite: &str,
        store_root: PathBuf,
        ignore_flashes_before_ms: Option<i64>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let (status_tx, status_rx) = mpsc::channel();
        if let Some(message) = repair_seen_only_manifest(&store_root, satellite) {
            append_glm_debug(&store_root, satellite, &message);
            let _ = status_tx.send(message);
        }
        let mut spec = rw_glm::GlmFollowSpec::new(satellite, store_root.clone());
        spec.window = STORE_WINDOW;
        spec.min_granule_start_unix_ms = ignore_flashes_before_ms;
        let cancel_thread = Arc::clone(&cancel);
        let ctx_clone = ctx.clone();
        let thread_store_root = store_root.clone();
        let thread_satellite = satellite.to_owned();
        append_glm_debug(
            &store_root,
            satellite,
            format!(
                "spawn store={} cutoff={ignore_flashes_before_ms:?}",
                store_root.display()
            ),
        );
        thread::spawn(move || {
            append_glm_debug(
                &thread_store_root,
                &thread_satellite,
                "thread started at normal priority",
            );
            let mut sink = |event: rw_glm::GlmEvent| {
                append_glm_debug(&thread_store_root, &thread_satellite, format!("{event:?}"));
                let line = glm_event_status(&event);
                let _ = status_tx.send(line);
                ctx_clone.request_repaint();
            };
            let result = rw_glm::follow_live(&spec, &mut sink, &cancel_thread);
            append_glm_debug(
                &thread_store_root,
                &thread_satellite,
                format!("follow_live returned {result:?}"),
            );
            if let Err(error) = result
                && !error.is_cancelled()
            {
                // Channel may be gone if the layer was removed; best-effort.
                let _ = status_tx.send(format!("GLM lightning stopped: {error}"));
                ctx_clone.request_repaint();
            }
        });
        Self {
            satellite: satellite.to_owned(),
            cancel,
            status_rx,
            last_status: format!("starting GLM follow in {}", store_root.display()),
            flashes: Vec::new(),
            fetched_at: None,
            last_read_count: 0,
            last_read_error: None,
            latest_flash_time_ms: None,
            store_root,
            ignore_flashes_before_ms,
            restart_requested: false,
            last_health_check: None,
            last_read_range_ms: None,
        }
    }

    /// Drain follow-engine events and refresh the flash cache for the active
    /// map/loop time window. `read_window_ms` should already include the
    /// display trailing-window lead-in before the first radar frame.
    pub fn pump_for_window(&mut self, read_window_ms: Option<(i64, i64)>) {
        let mut got_event = false;
        while let Ok(line) = self.status_rx.try_recv() {
            self.last_status = line;
            got_event = true;
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (t0, t1) = requested_read_range_ms(read_window_ms, now_ms);
        let requested_range = Some((t0, t1));
        let range_changed = self.last_read_range_ms != requested_range;
        let stale = self
            .fetched_at
            .map(|at| at.elapsed() > Duration::from_secs(10))
            .unwrap_or(true);
        if got_event || stale || range_changed {
            match rw_glm::read_flashes(&self.store_root, &self.satellite, t0, t1, None) {
                Ok(flashes) => {
                    self.last_read_count = flashes.len();
                    self.last_read_error = None;
                    self.ingest_flashes(flashes);
                    self.latest_flash_time_ms = self.flashes.last().map(|flash| flash.time_unix_ms);
                    if got_event {
                        append_glm_debug(
                            &self.store_root,
                            &self.satellite,
                            format!(
                                "read window {t0}..{t1} count={} latest={:?}",
                                self.flashes.len(),
                                self.latest_flash_time_ms
                            ),
                        );
                    }
                    self.last_read_range_ms = requested_range;
                }
                Err(error) => {
                    self.last_read_count = 0;
                    self.last_read_error = Some(error.to_string());
                    self.last_read_range_ms = requested_range;
                    append_glm_debug(
                        &self.store_root,
                        &self.satellite,
                        format!("read error: {error}"),
                    );
                }
            }
            self.fetched_at = Some(Instant::now());
        }
        // The health check reads window.json (and may scan the store dir) on
        // the UI thread, so keep it infrequent: the poisoned-store signature
        // it repairs only appears after >10 min of drift (REPAIR_SEEN_AHEAD_MS),
        // so a 60 s cadence loses nothing.
        let health_due = self
            .last_health_check
            .map(|checked| checked.elapsed() > Duration::from_secs(60))
            .unwrap_or(true);
        if health_due {
            self.last_health_check = Some(Instant::now());
            if let Some(message) = repair_seen_only_manifest(&self.store_root, &self.satellite) {
                append_glm_debug(&self.store_root, &self.satellite, &message);
                self.last_status = format!("{message}; restarting");
                self.restart_requested = true;
                self.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn restart_requested(&self) -> bool {
        self.restart_requested
    }

    pub fn read_range_covers(&self, t0_ms: i64, t1_ms: i64) -> bool {
        self.last_read_range_ms
            .is_some_and(|(read_t0, read_t1)| read_t0 <= t0_ms && read_t1 >= t1_ms)
    }

    pub fn ignore_flashes_before_ms(&self) -> Option<i64> {
        self.ignore_flashes_before_ms
    }

    /// Install a fresh store read as the flash cache, restoring the
    /// ascending-time invariant `frame_flashes` binary-searches on.
    /// `rw_glm::read_flashes` already returns ascending time order (buckets
    /// visited ascending, records sorted within each bucket by the store
    /// writer), so the sort normally never runs — the O(n) sortedness check
    /// guards against an upstream ordering change silently breaking the
    /// range queries. Stable sort so equal-time flashes keep arrival order.
    fn ingest_flashes(&mut self, mut flashes: Vec<rw_glm::Flash>) {
        if !flashes.is_sorted_by_key(|flash| flash.time_unix_ms) {
            flashes.sort_by_key(|flash| flash.time_unix_ms);
        }
        self.flashes = flashes;
    }

    /// Flashes valid for a frame at `frame_ms`: trailing display window
    /// (`window_min` minutes, the style registry's `glm.window_minutes`),
    /// QC-filtered. Age returned as 0..1 (0 = newest).
    ///
    /// `flashes` is sorted ascending by time, so the window
    /// `[frame_ms - window_ms, frame_ms]` is a contiguous slice located by
    /// two binary searches — O(log n + matches) per repaint instead of a
    /// linear scan of the whole read window.
    pub fn frame_flashes(
        &self,
        frame_ms: i64,
        window_min: i64,
    ) -> impl Iterator<Item = (&rw_glm::Flash, f32)> {
        let window_ms = window_min.max(1) * 60_000;
        let start = self
            .flashes
            .partition_point(|flash| flash.time_unix_ms < frame_ms.saturating_sub(window_ms));
        let end = self
            .flashes
            .partition_point(|flash| flash.time_unix_ms <= frame_ms);
        self.flashes[start..end].iter().filter_map(move |flash| {
            if flash.is_degraded() {
                return None;
            }
            let age_ms = frame_ms - flash.time_unix_ms;
            Some((flash, age_ms as f32 / window_ms as f32))
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(satellite: &str, last_read_range_ms: Option<(i64, i64)>) -> Self {
        let (_status_tx, status_rx) = mpsc::channel();
        Self {
            satellite: satellite.to_owned(),
            cancel: Arc::new(AtomicBool::new(false)),
            status_rx,
            last_status: "test GLM worker".to_owned(),
            flashes: Vec::new(),
            fetched_at: Some(Instant::now()),
            last_read_count: 0,
            last_read_error: None,
            latest_flash_time_ms: None,
            store_root: PathBuf::new(),
            ignore_flashes_before_ms: None,
            restart_requested: false,
            last_health_check: None,
            last_read_range_ms,
        }
    }
}

/// Effective store read range for `pump_for_window`. Explicit loop windows
/// pass through exactly (so a loop change re-keys the flash memo and forces
/// an immediate re-read), while live mode derives a `DEFAULT_READ_WINDOW`
/// range whose end is `now` rounded UP to `LIVE_READ_QUANTUM` — the range,
/// and the disk read keyed on it, only changes when the window slides
/// materially, never per repaint. The ceil keeps the true live edge inside
/// the range, and the store holds no future flashes, so reading a few
/// seconds past `now` is harmless.
fn requested_read_range_ms(read_window_ms: Option<(i64, i64)>, now_ms: i64) -> (i64, i64) {
    let quantum_ms = LIVE_READ_QUANTUM.as_millis() as i64;
    let live_edge_ms = (now_ms.div_euclid(quantum_ms) + 1) * quantum_ms;
    let (mut t0, mut t1) = read_window_ms.unwrap_or_else(|| {
        (
            live_edge_ms - (DEFAULT_READ_WINDOW.as_secs() as i64) * 1000,
            live_edge_ms,
        )
    });
    if t1 < t0 {
        std::mem::swap(&mut t0, &mut t1);
    }
    t1 = t1.min(live_edge_ms);
    if t1 < t0 {
        t0 = t1;
    }
    (t0, t1)
}

/// Self-heal a poisoned GLM store: if a prior run recorded granule keys in
/// `window.json` but the manifest has no usable time extent, the follow engine
/// can dedup current granules while the UI has no reliable read window/status.
/// Removing only that manifest keeps bucket data intact and lets the next poll
/// backfill/rebuild the current hour normally.
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
    let max_extent_ms = manifest
        .get("time_max_unix_ms")
        .and_then(serde_json::Value::as_i64);
    let latest_seen_ms = latest_seen_granule_start_ms(&manifest);
    let seen_newer_than_buckets = latest_seen_ms
        .zip(max_extent_ms)
        .is_some_and(|(seen, max)| seen > max.saturating_add(REPAIR_SEEN_AHEAD_MS));
    if has_time_extent && !seen_newer_than_buckets {
        return None;
    }
    let bucket_note = if has_rwl_buckets(&sat_dir) {
        if seen_newer_than_buckets {
            "seen keys newer than buckets"
        } else {
            "missing time extent"
        }
    } else {
        "no buckets"
    };
    match fs::remove_file(&manifest_path) {
        Ok(()) => Some(format!(
            "GLM repaired unusable store manifest ({seen_count} seen keys, {bucket_note}); backfilling"
        )),
        Err(error) => Some(format!("GLM empty manifest repair failed: {error}")),
    }
}

fn latest_seen_granule_start_ms(manifest: &serde_json::Value) -> Option<i64> {
    manifest
        .get("seen_granule_keys")?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str())
        .filter_map(parse_glm_granule_start_ms)
        .max()
}

fn parse_glm_granule_start_ms(key: &str) -> Option<i64> {
    let start = key.find("_s")? + 2;
    let stamp = key.get(start..start + 13)?;
    let year = stamp.get(0..4)?.parse::<i32>().ok()?;
    let doy = stamp.get(4..7)?.parse::<u32>().ok()?;
    let hour = stamp.get(7..9)?.parse::<u32>().ok()?;
    let minute = stamp.get(9..11)?.parse::<u32>().ok()?;
    let second = stamp.get(11..13)?.parse::<u32>().ok()?;
    let date = chrono::NaiveDate::from_yo_opt(year, doy)?;
    let time = date.and_hms_opt(hour, minute, second)?;
    Some(
        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(time, chrono::Utc)
            .timestamp_millis(),
    )
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
        append_glm_debug(&self.store_root, &self.satellite, "drop/cancel");
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
                "time_min_unix_ms": 1767225600000,
                "time_max_unix_ms": 1767226200000,
                "seen_granule_keys": ["OR_GLM-L2-LCFA_G19_s1_e2_c3"]
            }"#,
        )
        .unwrap();
        fs::write(sat_dir.join("20260619").join("t2000.rwl"), b"placeholder").unwrap();

        assert!(repair_seen_only_manifest(&root, "goes19").is_none());
        assert!(sat_dir.join("window.json").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn glm_repair_removes_manifest_with_buckets_but_no_extent() {
        let root = std::env::temp_dir().join(format!(
            "bowecho-glm-repair-bucket-no-extent-{}",
            std::process::id()
        ));
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

        let message = repair_seen_only_manifest(&root, "goes19").unwrap();

        assert!(message.contains("missing time extent"));
        assert!(!sat_dir.join("window.json").exists());
        assert!(sat_dir.join("20260619").join("t2000.rwl").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn glm_repair_removes_manifest_when_seen_keys_outrun_buckets() {
        let root = std::env::temp_dir().join(format!(
            "bowecho-glm-repair-seen-ahead-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let sat_dir = root.join("glm").join("goes18");
        fs::create_dir_all(sat_dir.join("20260619")).unwrap();
        let written_max =
            parse_glm_granule_start_ms("OR_GLM-L2-LCFA_G18_s20261702145000_e_x").unwrap();
        fs::write(
            sat_dir.join("window.json"),
            format!(
                r#"{{
                "schema": "rw-glm.window.v1",
                "satellite": "goes18",
                "time_min_unix_ms": {},
                "time_max_unix_ms": {},
                "seen_granule_keys": [
                    "OR_GLM-L2-LCFA_G18_s20261702206000_e20261702206200_c20261702206218"
                ]
            }}"#,
                written_max - 600_000,
                written_max
            ),
        )
        .unwrap();
        fs::write(sat_dir.join("20260619").join("t2140.rwl"), b"placeholder").unwrap();

        let message = repair_seen_only_manifest(&root, "goes18").unwrap();

        assert!(message.contains("seen keys newer than buckets"));
        assert!(!sat_dir.join("window.json").exists());
        assert!(sat_dir.join("20260619").join("t2140.rwl").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn glm_repair_keeps_manifest_when_seen_keys_match_extent() {
        let root = std::env::temp_dir().join(format!(
            "bowecho-glm-repair-seen-current-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let sat_dir = root.join("glm").join("goes18");
        fs::create_dir_all(sat_dir.join("20260619")).unwrap();
        let written_max =
            parse_glm_granule_start_ms("OR_GLM-L2-LCFA_G18_s20261702206000_e_x").unwrap();
        fs::write(
            sat_dir.join("window.json"),
            format!(
                r#"{{
                "schema": "rw-glm.window.v1",
                "satellite": "goes18",
                "time_min_unix_ms": {},
                "time_max_unix_ms": {},
                "seen_granule_keys": [
                    "OR_GLM-L2-LCFA_G18_s20261702206000_e20261702206200_c20261702206218"
                ]
            }}"#,
                written_max - 600_000,
                written_max
            ),
        )
        .unwrap();
        fs::write(sat_dir.join("20260619").join("t2200.rwl"), b"placeholder").unwrap();

        assert!(repair_seen_only_manifest(&root, "goes18").is_none());
        assert!(sat_dir.join("window.json").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn glm_live_read_range_reuses_memo_across_repaints() {
        let quantum_ms = LIVE_READ_QUANTUM.as_millis() as i64;
        let base = 1_766_188_800_000_i64; // arbitrary UTC instant, quantum-aligned
        assert_eq!(base % quantum_ms, 0);

        // Two repaints ~16 ms apart inside one quantum must produce the same
        // memo key, so pump_for_window reuses the cached flashes instead of
        // re-reading the store from disk.
        let first = requested_read_range_ms(None, base + 1);
        let second = requested_read_range_ms(None, base + 17);

        assert_eq!(first, second);
        assert!(first.1 >= base + 17, "range must cover the live edge");
        assert_eq!(
            first.1 - first.0,
            (DEFAULT_READ_WINDOW.as_secs() as i64) * 1000
        );
    }

    #[test]
    fn glm_live_read_range_advances_only_per_quantum() {
        let quantum_ms = LIVE_READ_QUANTUM.as_millis() as i64;
        let base = 1_766_188_800_000_i64;

        let before = requested_read_range_ms(None, base + 1);
        let after = requested_read_range_ms(None, base + quantum_ms + 1);

        assert_eq!(after.0 - before.0, quantum_ms);
        assert_eq!(after.1 - before.1, quantum_ms);
    }

    #[test]
    fn glm_explicit_read_range_changes_rekey_the_memo() {
        let now_ms = 1_766_188_800_000_i64;

        // Loop windows pass through exactly (past ranges untouched), so a
        // loop change invalidates the memo immediately.
        assert_eq!(
            requested_read_range_ms(Some((1_000, 2_000)), now_ms),
            (1_000, 2_000)
        );
        assert_eq!(
            requested_read_range_ms(Some((1_500, 2_500)), now_ms),
            (1_500, 2_500)
        );
        // Reversed windows still normalize, future ends still clamp near live.
        assert_eq!(
            requested_read_range_ms(Some((2_000, 1_000)), now_ms),
            (1_000, 2_000)
        );
        let (t0, t1) = requested_read_range_ms(Some((now_ms, now_ms + 3_600_000)), now_ms);
        assert_eq!(t0, now_ms);
        assert!(t1 <= now_ms + LIVE_READ_QUANTUM.as_millis() as i64);
    }

    fn test_flash(time_unix_ms: i64, flash_id: u32, degraded: bool) -> rw_glm::Flash {
        rw_glm::Flash {
            time_unix_ms,
            lat: 35.0,
            lon: -97.0,
            energy: 1.0e-15,
            area: 120.0,
            flash_id,
            flags: if degraded { 1 } else { 0 }, // bit 0 = degraded quality
            duration_ms: 250,
        }
    }

    /// The pre-index `frame_flashes` body: a linear scan of every cached
    /// flash. Kept as the behavioral reference the indexed version must match.
    fn linear_reference_selection(
        flashes: &[rw_glm::Flash],
        frame_ms: i64,
        window_min: i64,
    ) -> Vec<(u32, f32)> {
        let window_ms = window_min.max(1) * 60_000;
        flashes
            .iter()
            .filter_map(|flash| {
                if flash.is_degraded() {
                    return None;
                }
                let age_ms = frame_ms - flash.time_unix_ms;
                (age_ms >= 0 && age_ms <= window_ms)
                    .then_some((flash.flash_id, age_ms as f32 / window_ms as f32))
            })
            .collect()
    }

    #[test]
    fn glm_frame_flashes_indexed_matches_linear_scan_on_out_of_order_ingest() {
        let frame_ms = 1_766_188_800_000_i64;
        let window_min = 10_i64;
        let window_ms = window_min * 60_000;
        // Synthetic out-of-order set spanning both window edges exactly,
        // interior hits, misses on both sides, a degraded flash inside the
        // window, and a timestamp tie.
        let raw = vec![
            test_flash(frame_ms - window_ms / 2, 1, false), // interior hit
            test_flash(frame_ms + 1, 2, false),             // after frame: miss
            test_flash(frame_ms, 3, false),                 // at frame: hit, age 0
            test_flash(frame_ms - window_ms, 4, false),     // oldest edge: hit, age 1
            test_flash(frame_ms - window_ms - 1, 5, false), // too old: miss
            test_flash(frame_ms - 90_000, 6, true),         // degraded inside: filtered
            test_flash(frame_ms - 90_000, 7, false),        // tie with the degraded one
            test_flash(frame_ms - window_ms * 3, 8, false), // deep history: miss
        ];
        let mut worker = GlmWorker::new_for_test("goes19", None);
        worker.ingest_flashes(raw.clone());

        // Ingest must restore the ascending-time invariant the binary search
        // relies on — with the sort removed, partition_point on this
        // out-of-order set returns wrong window bounds.
        assert!(
            worker.flashes.is_sorted_by_key(|flash| flash.time_unix_ms),
            "ingest_flashes must leave the cache time-sorted"
        );
        assert_eq!(worker.flashes.len(), raw.len(), "ingest drops no flashes");

        let indexed: Vec<(u32, f32)> = worker
            .frame_flashes(frame_ms, window_min)
            .map(|(flash, age)| (flash.flash_id, age))
            .collect();
        let reference = linear_reference_selection(&worker.flashes, frame_ms, window_min);

        assert_eq!(
            indexed, reference,
            "indexed selection diverged from linear scan"
        );
        let ids: Vec<u32> = indexed.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![4, 1, 7, 3], "expected exact window membership");
        assert_eq!(
            indexed.first().unwrap().1,
            1.0,
            "oldest edge flash ages to 1.0"
        );
        assert_eq!(
            indexed.last().unwrap().1,
            0.0,
            "frame-time flash ages to 0.0"
        );
    }

    #[test]
    fn glm_frame_flashes_indexed_matches_linear_scan_across_sweep() {
        // Sweep frame times across a dense synthetic history (including
        // duplicates and degraded flashes) so every partition boundary case
        // gets exercised against the linear reference.
        let base = 1_766_188_800_000_i64;
        let mut raw = Vec::new();
        for i in 0..500_i64 {
            raw.push(test_flash(base + i * 7_000, i as u32, i % 11 == 0));
            if i % 5 == 0 {
                raw.push(test_flash(base + i * 7_000, 1000 + i as u32, false));
            }
        }
        // Shuffle deterministically to force the ingest sort path.
        raw.reverse();
        raw.swap(3, 250);
        let mut worker = GlmWorker::new_for_test("goes19", None);
        worker.ingest_flashes(raw);

        for step in [-2_i64, 0, 1, 137, 499] {
            for offset in [-1_i64, 0, 1] {
                let frame_ms = base + step * 7_000 + offset;
                let indexed: Vec<(u32, f32)> = worker
                    .frame_flashes(frame_ms, 5)
                    .map(|(flash, age)| (flash.flash_id, age))
                    .collect();
                let reference = linear_reference_selection(&worker.flashes, frame_ms, 5);
                assert_eq!(indexed, reference, "diverged at frame_ms={frame_ms}");
            }
        }
    }

    #[test]
    fn glm_pump_skips_disk_reread_between_repaints() {
        let root =
            std::env::temp_dir().join(format!("bowecho-glm-pump-memo-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut worker = GlmWorker::new_for_test("goes19", None);
        worker.store_root = root.clone();

        worker.pump_for_window(None);
        let first = worker
            .last_read_range_ms
            .expect("first pump records a range");
        thread::sleep(Duration::from_millis(20)); // one repaint later
        worker.pump_for_window(None);
        let second = worker
            .last_read_range_ms
            .expect("second pump keeps a range");

        // The memoized live range must be identical (cache reused) or advance
        // by exactly one quantum if the sleep straddled a boundary — never
        // slide with the wall clock per repaint.
        let quantum_ms = LIVE_READ_QUANTUM.as_millis() as i64;
        assert!(
            second == first || second == (first.0 + quantum_ms, first.1 + quantum_ms),
            "live read range slid per-repaint: {first:?} -> {second:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
