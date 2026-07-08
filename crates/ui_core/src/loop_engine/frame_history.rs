//! The frame-history substrate: entry types, the generation-stamped
//! [`FrameHistory`] newtype, and the pure helper functions the install
//! paths share.
//!
//! Everything here moved VERBATIM from `crates/app_ui/src/main.rs` in
//! v0.29 Phase 4c (spec §8 row "FrameHistory + generation fn +
//! FrameHistoryEntry + FrameIdentity"). Semantics are pinned by the
//! app_ui tests that stayed behind (they now exercise these items through
//! re-exports) and by the unit tests below. The generation bump
//! discipline is the owner's invalidation spine (spec §9): build ON it,
//! never weaken it.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use data_source::RealtimeChunkType;
use radar_core::RadarVolume;

/// Per-load timing + realtime-chunk telemetry carried alongside a decoded
/// volume. Invariant: this is measurement plumbing only — nothing in the
/// engine branches on it except the same-path upsert refresh (rule (b) of
/// [`LoopEngine::install_batch`](super::LoopEngine::install_batch)), which
/// overwrites it wholesale.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadTimings {
    pub total_ms: f32,
    pub lookup_ms: Option<f32>,
    pub lookup_cache_hit: Option<bool>,
    pub fetch_ms: Option<f32>,
    pub fetch_cache_hit: Option<bool>,
    pub read_ms: Option<f32>,
    pub decode_ms: f32,
    pub preview_ms: Option<f32>,
    pub realtime_poll_start_utc: Option<DateTime<Utc>>,
    pub realtime_poll_end_utc: Option<DateTime<Utc>>,
    pub realtime_volume_id: Option<u16>,
    pub realtime_chunk_count: Option<usize>,
    pub realtime_last_chunk_id: Option<u16>,
    pub realtime_last_chunk_type: Option<RealtimeChunkType>,
    pub realtime_complete: Option<bool>,
    pub realtime_total_size: Option<u64>,
    pub realtime_assembled_size: Option<u64>,
    pub realtime_last_modified_utc: Option<DateTime<Utc>>,
    pub realtime_volume_time_utc: Option<DateTime<Utc>>,
}

impl LoadTimings {
    /// Stamp `total_ms` from a wall-clock start. Call exactly once, at the
    /// end of the load that owns these timings.
    pub fn finish(mut self, total_start: Instant) -> Self {
        self.total_ms = total_start.elapsed().as_secs_f32() * 1000.0;
        self
    }
}

/// One decoded volume as delivered by a load worker, before it becomes a
/// [`FrameHistoryEntry`]. Invariant: `path` is the stable dedupe/refresh
/// key for the upsert rules — workers must keep it unique per distinct
/// payload (`poll://{name}`, `ord-archive:…`, a real file path).
#[derive(Clone)]
pub struct DecodedLoad {
    pub path: PathBuf,
    pub volume: Arc<RadarVolume>,
    pub timings: LoadTimings,
    pub status: FrameStatus,
    pub source_label: String,
}

/// A batch of decoded volumes plus the index of the frame the loader wants
/// displayed (the "anchor"). Invariant: `selected_index` refers to the
/// PRE-sort order of `frames`; install maps it through [`FrameIdentity`]
/// after sorting, so callers never need to pre-sort.
#[derive(Clone)]
pub struct DecodedLoadBatch {
    pub frames: Vec<DecodedLoad>,
    pub selected_index: usize,
}

impl DecodedLoadBatch {
    /// A one-frame batch anchored on its only frame.
    pub fn single(decoded: DecodedLoad) -> Self {
        Self {
            frames: vec![decoded],
            selected_index: 0,
        }
    }
}

/// One frame in a loop history. Invariant: `identity` is derived from
/// `volume` by [`frame_identity_for_volume`] and never edited
/// independently — the sort order, upsert key, and cross-site guard all
/// assume they agree.
#[derive(Clone)]
pub struct FrameHistoryEntry {
    pub identity: FrameIdentity,
    pub path: PathBuf,
    pub volume: Arc<RadarVolume>,
    pub timings: Option<LoadTimings>,
    pub status: FrameStatus,
    pub source_label: String,
}

/// Monotonic source for [`FrameHistory`] generation stamps. Process-wide so
/// a stamp value can never be reused by a different content snapshot: clones
/// keep the stamp of the content they copied, and every mutation takes a
/// fresh one. Equal stamps therefore guarantee equal contents, letting O(1)
/// generation compares replace O(frames) re-hashing in cache keys.
fn next_frame_history_generation() -> u64 {
    static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// `Vec<FrameHistoryEntry>` plus a generation stamp bumped on every mutable
/// access, so the loop-timeline summary cache invalidates exactly when a
/// history can have changed. `Deref` exposes the read-only Vec API; every
/// mutation funnels through the bumping methods below (there is deliberately
/// no `DerefMut`, so a forgotten bump is a compile error, not a stale cache).
#[derive(Clone)]
pub struct FrameHistory {
    entries: Vec<FrameHistoryEntry>,
    generation: u64,
}

impl FrameHistory {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            generation: next_frame_history_generation(),
        }
    }

    /// The current content stamp. Equal stamps guarantee equal contents;
    /// a changed stamp only means the contents MAY have changed (mutable
    /// iteration bumps up front, see [`Self::iter_mut`]).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn bump_generation(&mut self) {
        self.generation = next_frame_history_generation();
    }

    pub fn push(&mut self, entry: FrameHistoryEntry) {
        self.bump_generation();
        self.entries.push(entry);
    }

    pub fn remove(&mut self, index: usize) -> FrameHistoryEntry {
        self.bump_generation();
        self.entries.remove(index)
    }

    pub fn clear(&mut self) {
        self.bump_generation();
        self.entries.clear();
    }

    /// Drain every entry OUT, leaving an empty (freshly stamped) history.
    /// The generation bump keeps the invalidation spine honest — the
    /// history is now empty, a distinct content snapshot — exactly as
    /// [`Self::clear`] does. The difference is only where the drop happens:
    /// `clear` releases the entries in place; `take_entries` hands them to
    /// the caller, so the large cross-site install path can drop the
    /// multi-GB `Arc<RadarVolume>` frames on a reaper thread instead of
    /// stalling the UI thread (the synthetic-loop -> Live freeze fix).
    pub fn take_entries(&mut self) -> Vec<FrameHistoryEntry> {
        self.bump_generation();
        std::mem::take(&mut self.entries)
    }

    pub fn sort_by(
        &mut self,
        compare: impl FnMut(&FrameHistoryEntry, &FrameHistoryEntry) -> Ordering,
    ) {
        self.bump_generation();
        self.entries.sort_by(compare);
    }

    /// Mutable iteration bumps up front: callers may rewrite any entry
    /// (upsert replace, advanced-product volume write-back), and a spurious
    /// bump only costs one summary recompute — never a stale cache.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, FrameHistoryEntry> {
        self.bump_generation();
        self.entries.iter_mut()
    }
}

impl Default for FrameHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for FrameHistory {
    type Target = Vec<FrameHistoryEntry>;

    fn deref(&self) -> &Vec<FrameHistoryEntry> {
        &self.entries
    }
}

impl std::ops::Index<usize> for FrameHistory {
    type Output = FrameHistoryEntry;

    fn index(&self, index: usize) -> &FrameHistoryEntry {
        &self.entries[index]
    }
}

impl std::ops::IndexMut<usize> for FrameHistory {
    fn index_mut(&mut self, index: usize) -> &mut FrameHistoryEntry {
        self.bump_generation();
        &mut self.entries[index]
    }
}

impl From<Vec<FrameHistoryEntry>> for FrameHistory {
    fn from(entries: Vec<FrameHistoryEntry>) -> Self {
        Self {
            entries,
            generation: next_frame_history_generation(),
        }
    }
}

impl FromIterator<FrameHistoryEntry> for FrameHistory {
    fn from_iter<I: IntoIterator<Item = FrameHistoryEntry>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl<'a> IntoIterator for &'a FrameHistory {
    type Item = &'a FrameHistoryEntry;
    type IntoIter = std::slice::Iter<'a, FrameHistoryEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

impl<'a> IntoIterator for &'a mut FrameHistory {
    type Item = &'a mut FrameHistoryEntry;
    type IntoIter = std::slice::IterMut<'a, FrameHistoryEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

/// The upsert/sort/dedupe key for a frame: which radar, which scan.
/// Invariant: histories are kept ascending by this ordering (site id, then
/// scan time) by every install route — [`FrameIdentity`] equality is what
/// makes a re-delivered scan replace instead of duplicate.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct FrameIdentity {
    pub site_id: String,
    pub scan_time_utc: DateTime<Utc>,
}

/// Completeness ladder for a frame. Invariant: the ORDER of the priority
/// ladder in [`frame_status_priority`] is load-bearing for upsert rule (c);
/// add variants only with a census-style review of the four install paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameStatus {
    Local,
    Preview,
    LivePartial,
    LiveComplete,
    Complete,
    Stale,
}

impl FrameStatus {
    /// The status word shown in frame-status text lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Preview => "preview",
            Self::LivePartial => "live partial",
            Self::LiveComplete => "live complete",
            Self::Complete => "complete",
            Self::Stale => "stale",
        }
    }
}

/// Derive the history key for a decoded volume. Invariant: this is THE
/// identity derivation (census Appendix B) — every install path uses it,
/// so a frame can never enter two histories under different keys.
pub fn frame_identity_for_volume(volume: &RadarVolume) -> FrameIdentity {
    FrameIdentity {
        site_id: volume.site.id.clone(),
        scan_time_utc: volume.volume_time.with_timezone(&Utc),
    }
}

/// Wrap a decoded load as a history entry, deriving its identity.
pub fn frame_history_entry_from_decoded(decoded: DecodedLoad) -> FrameHistoryEntry {
    let identity = frame_identity_for_volume(&decoded.volume);
    FrameHistoryEntry {
        identity,
        path: decoded.path,
        volume: decoded.volume,
        timings: Some(decoded.timings),
        status: decoded.status,
        source_label: decoded.source_label,
    }
}

/// Upsert priority ladder (census D2): Preview 0 < LivePartial 1 <
/// Complete/Stale 2 < LiveComplete/Local 3. A same-identity frame replaces
/// the stored one only at strictly higher priority, or at equal priority
/// from a different path (rule (c)).
pub fn frame_status_priority(status: FrameStatus) -> u8 {
    match status {
        FrameStatus::Preview => 0,
        FrameStatus::LivePartial => 1,
        FrameStatus::Complete | FrameStatus::Stale => 2,
        FrameStatus::LiveComplete | FrameStatus::Local => 3,
    }
}

/// Upsert rule (a) (census D2): a live-partial refetch of the SAME path
/// replaces the stored live-partial only when it actually carries more
/// radials — the realtime chunk chain re-delivers a growing volume under
/// one path, and a byte-identical refetch must not thrash caches.
pub fn live_partial_frame_has_new_data(
    incoming: &FrameHistoryEntry,
    existing: &FrameHistoryEntry,
) -> bool {
    incoming.status == FrameStatus::LivePartial
        && existing.status == FrameStatus::LivePartial
        && incoming.path == existing.path
        && (incoming.volume.metadata.decoded_radial_count
            > existing.volume.metadata.decoded_radial_count
            || volume_total_radials(incoming.volume.as_ref())
                > volume_total_radials(existing.volume.as_ref()))
}

/// Total radial count across all cuts — the tie-breaker for rule (a) when
/// the decoder's metadata counter has not advanced.
pub fn volume_total_radials(volume: &RadarVolume) -> usize {
    volume.cuts.iter().map(|cut| cut.radials.len()).sum()
}

/// Cross-site guard predicate (census D3, Appendix B): true when any stored
/// frame belongs to a different site than `site_id`. Textually identical in
/// the three legacy paths that had it; the engine applies it on every
/// install so a history loop can never mix radars.
pub fn history_contains_other_site(history: &[FrameHistoryEntry], site_id: &str) -> bool {
    history
        .iter()
        .any(|frame| frame.identity.site_id != site_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn entry(site: &str, minute: u32, path: &str) -> FrameHistoryEntry {
        let volume = Arc::new(RadarVolume::new(
            radar_core::RadarSite::new(site),
            Utc.with_ymd_and_hms(2026, 6, 9, 5, minute, 0).unwrap(),
        ));
        FrameHistoryEntry {
            identity: frame_identity_for_volume(&volume),
            path: PathBuf::from(path),
            volume,
            timings: None,
            status: FrameStatus::Complete,
            source_label: format!("test {site}"),
        }
    }

    /// Pins the bump discipline the v0.28 spine relies on: EVERY mutation
    /// route takes a fresh stamp, and no read route does. A new mutating
    /// method added without a bump will fail here (and, structurally, the
    /// absence of `DerefMut` means it cannot exist by accident).
    #[test]
    fn frame_history_generation_bumps_on_every_mutation_route_and_no_read_route() {
        let mut history = FrameHistory::new();
        let mut last = history.generation();
        let mut expect_bump = |history: &FrameHistory, route: &str| {
            assert_ne!(history.generation(), last, "{route} must bump");
            last = history.generation();
        };

        history.push(entry("KEAX", 0, "a"));
        expect_bump(&history, "push");
        history.push(entry("KEAX", 5, "b"));
        expect_bump(&history, "push (second)");
        history.sort_by(|left, right| left.identity.cmp(&right.identity));
        expect_bump(&history, "sort_by");
        let _ = history.iter_mut();
        expect_bump(&history, "iter_mut");
        let _ = (&mut history).into_iter();
        expect_bump(&history, "&mut IntoIterator");
        history[0].source_label = "rewritten".to_owned();
        expect_bump(&history, "IndexMut");
        let _ = history.remove(0);
        expect_bump(&history, "remove");
        history.clear();
        expect_bump(&history, "clear");

        // Read routes: no bump.
        history.push(entry("KEAX", 10, "c"));
        let stable = history.generation();
        let _ = history.len();
        let _ = history.first();
        let _ = &history[0];
        for _frame in &history {}
        let _ = history.iter().count();
        assert_eq!(
            history.generation(),
            stable,
            "read-only access must never bump"
        );
    }

    /// `take_entries` drains the whole Vec out for off-thread disposal,
    /// leaves the history empty, and bumps the generation (same content
    /// snapshot change as `clear`, just handing the frames back).
    #[test]
    fn take_entries_drains_the_history_and_bumps_the_generation() {
        let mut history = FrameHistory::new();
        history.push(entry("KEAX", 0, "a"));
        history.push(entry("KEAX", 5, "b"));
        let before = history.generation();

        let drained = history.take_entries();

        assert_eq!(drained.len(), 2, "every entry handed back to the caller");
        assert!(history.is_empty(), "history left empty");
        assert_ne!(
            history.generation(),
            before,
            "take_entries bumps the invalidation spine like clear"
        );
    }

    /// Clones keep the stamp of the content they copied (equal stamp =>
    /// equal content stays true), while rebuilding from a Vec takes a
    /// fresh stamp (the Vec may differ from anything previously stamped).
    #[test]
    fn frame_history_clone_keeps_stamp_and_from_vec_takes_a_fresh_one() {
        let mut history = FrameHistory::new();
        history.push(entry("KEAX", 0, "a"));
        let clone = history.clone();
        assert_eq!(history.generation(), clone.generation());

        let rebuilt = FrameHistory::from(vec![entry("KEAX", 0, "a")]);
        assert_ne!(rebuilt.generation(), history.generation());

        let collected: FrameHistory = std::iter::once(entry("KEAX", 0, "a")).collect();
        assert_ne!(collected.generation(), history.generation());
        assert_ne!(collected.generation(), rebuilt.generation());
    }

    /// The rule-(a) predicate fires only for LivePartial→LivePartial
    /// same-path refetches that grew (census D2 tier a).
    #[test]
    fn live_partial_new_data_requires_same_path_live_partials_that_grew() {
        let mut existing = entry("KTLX", 0, "chunk");
        existing.status = FrameStatus::LivePartial;
        let mut incoming = existing.clone();

        assert!(
            !live_partial_frame_has_new_data(&incoming, &existing),
            "byte-identical refetch must not replace"
        );

        let mut grown_volume = (*incoming.volume).clone();
        grown_volume.metadata.decoded_radial_count = 10;
        incoming.volume = Arc::new(grown_volume);
        assert!(live_partial_frame_has_new_data(&incoming, &existing));

        incoming.status = FrameStatus::Complete;
        assert!(
            !live_partial_frame_has_new_data(&incoming, &existing),
            "rule (a) is LivePartial-only by census decision D2"
        );
    }
}
