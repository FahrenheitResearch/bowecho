//! MiniFeed — live poll → download → decode, all on `ui_core` slots
//! (miniderecho-spec §13 Task 3). One 1 s-cadence `WorkerSlot` runs the US
//! realtime chunk chain; a `StreamSlot` decodes with the bzip preview for
//! progressive first paint; one more `StreamSlot` backfills the loop from
//! the Level-II archive (Task 4). Jobs only send messages — status strings
//! are chosen by the app at drain time (the WorkerSlot rule).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use radar_core::RadarVolume;
use ui_core::worker_slot::{SlotMessage, SlotPoll, StreamSlot, StreamState, WorkerSlot};

/// Matches BowEcho's preview threshold (app_ui main.rs `MIN_DISPLAYABLE_RADIALS`).
const MIN_DISPLAYABLE_RADIALS: usize = 180;
/// Live poll cadence (§13 Task 3.1). Listing TTL/memoization inside
/// data_source keeps the upstream cost far below one list per tick.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Loop backfill: last N archive volumes, oldest-first (§13 Task 4).
const BACKFILL_DAYS_BACK: i64 = 1;
const BACKFILL_MAX_VOLUMES: usize = 8;
/// Per-frame drain budget for the decode/backfill streams.
const DRAIN_BUDGET: Duration = Duration::from_millis(4);

/// Live-volume dedupe key (§13 Task 3.1): `(site, volume_id, chunks.len())`.
/// Unchanged key ⇒ no download.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeKey {
    pub site: String,
    pub volume_id: u16,
    pub chunk_count: usize,
}

impl VolumeKey {
    pub fn of(volume: &data_source::RealtimeLevel2Volume) -> Self {
        Self {
            site: volume.site.clone(),
            volume_id: volume.volume_id,
            chunk_count: volume.chunks.len(),
        }
    }
}

/// The dedupe contract: download only when the latest key differs from the
/// last one the app installed.
pub fn should_download(last: Option<&VolumeKey>, latest: &VolumeKey) -> bool {
    last != Some(latest)
}

enum PollOutcome {
    /// Same `(site, volume_id, chunks.len())` as last time — nothing to do.
    Unchanged,
    /// New data was downloaded (or was already complete on disk).
    Downloaded {
        key: VolumeKey,
        path: PathBuf,
    },
    Failed(String),
}

pub enum DecodeMsg {
    /// First displayable cut(s) — progressive first paint.
    Preview(Arc<RadarVolume>),
    /// The complete volume; replaces the preview by `(site, time)` identity.
    Full(Arc<RadarVolume>),
    Failed(String),
}

impl SlotMessage for DecodeMsg {
    fn is_terminal(&self) -> bool {
        matches!(self, DecodeMsg::Full(_) | DecodeMsg::Failed(_))
    }
}

enum BackfillMsg {
    Volume(Arc<RadarVolume>),
    Done,
    Failed(String),
}

impl SlotMessage for BackfillMsg {
    fn is_terminal(&self) -> bool {
        matches!(self, BackfillMsg::Done | BackfillMsg::Failed(_))
    }
}

/// What a [`MiniFeed::tick`] drained this frame. The app installs volumes
/// into the ring and derives the status line — never the jobs.
#[derive(Default)]
pub struct FeedEvents {
    /// Decoded volumes, in arrival order (backfill arrives oldest-first).
    pub volumes: Vec<Arc<RadarVolume>>,
    /// A live (non-warm-cache) full decode landed in this drain.
    pub live_full_arrived: bool,
    pub errors: Vec<String>,
}

/// What the feed is doing right now (drain-time truth for the status strip).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedActivity {
    Idle,
    /// Listing / downloading the latest live volume.
    Fetching,
    /// Decoding a downloaded volume.
    Decoding,
    /// Streaming archive volumes into the loop.
    Backfilling,
}

pub struct MiniFeed {
    site_id: String,
    cache_dir: PathBuf,
    poll: WorkerSlot<PollOutcome>,
    decode: StreamSlot<DecodeMsg>,
    backfill: StreamSlot<BackfillMsg>,
    last_key: Option<VolumeKey>,
    next_poll_at: Option<Instant>,
    /// Newest disk-cached volume, decoded before the first poll so pixels
    /// appear < 1 s on relaunch (§13 Task 3.3).
    warm_path: Option<PathBuf>,
    /// A download that arrived while a decode was still in flight.
    pending_decode: Option<PathBuf>,
    /// Whether the in-flight decode came from the live poll (vs warm cache).
    decoding_live: bool,
    backfill_started: bool,
}

impl MiniFeed {
    /// `site_id` is the US Level-II id (`latest_realtime_level2_volume`
    /// takes the id string — the §3 US-only v1 scoping).
    pub fn new(site_id: String, cache_dir: PathBuf) -> Self {
        let warm_path = data_source::newest_cached_level2_path(&cache_dir)
            .ok()
            .flatten();
        Self {
            site_id,
            cache_dir,
            poll: WorkerSlot::idle("mini live poll"),
            decode: StreamSlot::idle("mini decode"),
            backfill: StreamSlot::idle("mini backfill"),
            last_key: None,
            next_poll_at: None,
            warm_path,
            pending_decode: None,
            decoding_live: false,
            backfill_started: false,
        }
    }

    pub fn activity(&self) -> FeedActivity {
        if self.decode.in_flight() {
            FeedActivity::Decoding
        } else if self.poll.in_flight() {
            FeedActivity::Fetching
        } else if self.backfill.in_flight() {
            FeedActivity::Backfilling
        } else {
            FeedActivity::Idle
        }
    }

    /// Drive the feed for one UI frame: spawn due jobs, drain results.
    pub fn tick(&mut self, ctx: &egui::Context) -> FeedEvents {
        let mut events = FeedEvents::default();

        // Warm launch: decode the newest cached volume before the first poll.
        if let Some(path) = self.warm_path.take() {
            self.start_decode(ctx, path, false);
        }

        self.spawn_poll_if_due(ctx);
        self.drain_poll(ctx, &mut events);
        self.drain_decode(ctx, &mut events);
        self.drain_backfill(&mut events);

        events
    }

    fn spawn_poll_if_due(&mut self, ctx: &egui::Context) {
        if self.poll.in_flight() {
            return;
        }
        if let Some(at) = self.next_poll_at
            && Instant::now() < at
        {
            return;
        }
        let site = self.site_id.clone();
        let cache_dir = self.cache_dir.clone();
        let last_key = self.last_key.clone();
        self.poll.spawn(ctx, move |tx| {
            let _ = tx.send(poll_job(&site, last_key.as_ref(), &cache_dir));
        });
    }

    fn drain_poll(&mut self, ctx: &egui::Context, events: &mut FeedEvents) {
        match self.poll.poll() {
            SlotPoll::Idle | SlotPoll::Pending => return,
            SlotPoll::Ready(PollOutcome::Unchanged) => {}
            SlotPoll::Ready(PollOutcome::Downloaded { key, path }) => {
                self.last_key = Some(key);
                self.start_decode(ctx, path, true);
            }
            SlotPoll::Ready(PollOutcome::Failed(error)) => events.errors.push(error),
            SlotPoll::Disconnected => events.errors.push("live poll worker panicked".to_owned()),
        }
        self.next_poll_at = Some(Instant::now() + POLL_INTERVAL);
    }

    fn start_decode(&mut self, ctx: &egui::Context, path: PathBuf, live: bool) {
        if self.decode.in_flight() {
            // Newest wins: a fresher download replaces any queued one.
            self.pending_decode = Some(path);
            return;
        }
        self.decoding_live = live;
        self.decode.spawn(ctx, move |tx| {
            let terminal = match std::fs::read(&path) {
                Ok(raw) => match nexrad_io::decode_volume_from_bytes_with_bzip_preview(
                    &raw,
                    MIN_DISPLAYABLE_RADIALS,
                    |preview| {
                        let _ = tx.send(DecodeMsg::Preview(Arc::new(preview)));
                    },
                ) {
                    Ok(mut volume) => {
                        volume.metadata.source_path = Some(path.display().to_string());
                        DecodeMsg::Full(Arc::new(volume))
                    }
                    Err(error) => DecodeMsg::Failed(error.to_string()),
                },
                Err(error) => DecodeMsg::Failed(format!("read {}: {error}", path.display())),
            };
            let _ = tx.send(terminal);
        });
    }

    fn drain_decode(&mut self, ctx: &egui::Context, events: &mut FeedEvents) {
        let (messages, state) = self.decode.drain(DRAIN_BUDGET);
        for message in messages {
            match message {
                DecodeMsg::Preview(volume) => events.volumes.push(volume),
                DecodeMsg::Full(volume) => {
                    events.volumes.push(volume);
                    if self.decoding_live {
                        events.live_full_arrived = true;
                    }
                }
                DecodeMsg::Failed(error) => events.errors.push(error),
            }
        }
        match state {
            StreamState::Finished | StreamState::Disconnected => {
                if state == StreamState::Disconnected {
                    events.errors.push("decode worker panicked".to_owned());
                }
                if let Some(path) = self.pending_decode.take() {
                    self.start_decode(ctx, path, true);
                }
            }
            StreamState::Idle | StreamState::Pending => {}
        }
    }

    /// One-shot loop backfill (§13 Task 4): last N archive volumes streamed
    /// oldest-first. The app calls this once the first live frame landed.
    pub fn start_backfill(&mut self, ctx: &egui::Context) -> bool {
        if self.backfill_started {
            return false;
        }
        self.backfill_started = true;
        let site = self.site_id.clone();
        let cache_dir = self.cache_dir.clone();
        self.backfill.spawn(ctx, move |tx| {
            let objects = match data_source::recent_level2_objects(
                &site,
                BACKFILL_DAYS_BACK,
                BACKFILL_MAX_VOLUMES,
            ) {
                Ok(objects) => objects,
                Err(error) => {
                    let _ = tx.send(BackfillMsg::Failed(error.to_string()));
                    return;
                }
            };
            // recent_level2_objects returns newest-first; stream oldest-first
            // so the loop builds forward in time.
            for object in objects.into_iter().rev() {
                let volume = data_source::download_object(
                    data_source::LEVEL2_ARCHIVE_BUCKET,
                    object,
                    &cache_dir,
                )
                .map_err(|error| error.to_string())
                .and_then(|downloaded| {
                    nexrad_io::decode_volume_from_path(&downloaded.path)
                        .map_err(|error| error.to_string())
                });
                let message = match volume {
                    Ok(volume) => BackfillMsg::Volume(Arc::new(volume)),
                    // One bad archive object must not kill the loop build.
                    Err(_) => continue,
                };
                if tx.send(message).is_err() {
                    return; // owner cancelled
                }
            }
            let _ = tx.send(BackfillMsg::Done);
        })
    }

    pub fn backfill_started(&self) -> bool {
        self.backfill_started
    }

    fn drain_backfill(&mut self, events: &mut FeedEvents) {
        let (messages, _state) = self.backfill.drain(DRAIN_BUDGET);
        for message in messages {
            match message {
                BackfillMsg::Volume(volume) => events.volumes.push(volume),
                BackfillMsg::Done => {}
                BackfillMsg::Failed(error) => events.errors.push(format!("backfill: {error}")),
            }
        }
    }
}

fn poll_job(site: &str, last_key: Option<&VolumeKey>, cache_dir: &Path) -> PollOutcome {
    let volume = match data_source::latest_realtime_level2_volume(site) {
        Ok(volume) => volume,
        Err(error) => return PollOutcome::Failed(error.to_string()),
    };
    let key = VolumeKey::of(&volume);
    if !should_download(last_key, &key) {
        return PollOutcome::Unchanged;
    }
    match data_source::download_realtime_volume(&volume, cache_dir) {
        Ok(downloaded) => PollOutcome::Downloaded {
            key,
            path: downloaded.path,
        },
        Err(error) => PollOutcome::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use data_source::{RealtimeChunkObject, RealtimeChunkType, RealtimeLevel2Volume, S3Object};

    fn chunk(id: u16) -> RealtimeChunkObject {
        RealtimeChunkObject {
            object: S3Object {
                key: format!("KTLX/216/{id:03}"),
                size: 1024,
                last_modified: None,
            },
            site: "KTLX".to_owned(),
            volume_id: 216,
            volume_time: Utc::now(),
            chunk_id: id,
            chunk_type: if id == 1 {
                RealtimeChunkType::Start
            } else {
                RealtimeChunkType::Intermediate
            },
        }
    }

    fn realtime_volume(chunks: usize) -> RealtimeLevel2Volume {
        RealtimeLevel2Volume {
            site: "KTLX".to_owned(),
            volume_id: 216,
            volume_time: Utc::now(),
            chunks: (1..=chunks as u16).map(chunk).collect(),
            complete: false,
            total_size: chunks as u64 * 1024,
        }
    }

    #[test]
    fn dedupe_key_is_site_volume_id_and_chunk_count() {
        let key = VolumeKey::of(&realtime_volume(3));
        assert_eq!(
            key,
            VolumeKey {
                site: "KTLX".to_owned(),
                volume_id: 216,
                chunk_count: 3
            }
        );

        // Unchanged key ⇒ no download.
        assert!(!should_download(
            Some(&key),
            &VolumeKey::of(&realtime_volume(3))
        ));
        // A new chunk on the same volume ⇒ download.
        assert!(should_download(
            Some(&key),
            &VolumeKey::of(&realtime_volume(4))
        ));
        // A new volume id ⇒ download.
        let mut rolled = realtime_volume(1);
        rolled.volume_id = 217;
        rolled.chunks[0].volume_id = 217;
        assert!(should_download(Some(&key), &VolumeKey::of(&rolled)));
        // First poll ever ⇒ download.
        assert!(should_download(None, &key));
    }

    #[test]
    fn decode_messages_terminate_on_full_or_failed() {
        let volume = Arc::new(RadarVolume::new(
            radar_core::RadarSite::new("KTLX"),
            Utc::now(),
        ));
        assert!(!DecodeMsg::Preview(Arc::clone(&volume)).is_terminal());
        assert!(DecodeMsg::Full(volume).is_terminal());
        assert!(DecodeMsg::Failed("x".to_owned()).is_terminal());
    }

    #[test]
    fn decode_stream_drains_preview_then_full_through_a_slot() {
        // Proves the B3 test-util surface from a dependent crate: drive a
        // StreamSlot<DecodeMsg> without a worker thread.
        let mut slot: StreamSlot<DecodeMsg> = StreamSlot::idle("test-decode");
        let (sender, receiver) = std::sync::mpsc::channel();
        slot.inject_for_test(receiver);

        let volume = Arc::new(RadarVolume::new(
            radar_core::RadarSite::new("KTLX"),
            Utc::now(),
        ));
        sender
            .send(DecodeMsg::Preview(Arc::clone(&volume)))
            .unwrap();
        sender.send(DecodeMsg::Full(volume)).unwrap();

        let (messages, state) = slot.drain(Duration::MAX);
        assert_eq!(messages.len(), 2);
        assert_eq!(state, StreamState::Finished);
        assert!(!slot.in_flight());
    }
}
