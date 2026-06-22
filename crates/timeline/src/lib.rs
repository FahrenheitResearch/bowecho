//! Timeline and animation state for live/archive volume playback.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineFrame {
    pub site_id: String,
    pub volume_time: DateTime<Utc>,
    pub volume_id: String,
    pub frame_time: DateTime<Utc>,
    pub cut: Option<TimelineCut>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineCut {
    pub cut_index: usize,
    pub elevation_hundredths_deg: i16,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DataPackManifest {
    pub schema_version: u16,
    pub id: String,
    pub label: String,
    pub window: DataPackWindow,
    pub radars: Vec<DataPackRadar>,
    pub timeline_mode: TimelineMode,
    pub low_tilt: Option<LowTiltPolicy>,
    pub view: DataPackView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DataPackWindow {
    pub start_utc: DateTime<Utc>,
    pub end_utc: DateTime<Utc>,
    pub anchor_utc: DateTime<Utc>,
    #[serde(default)]
    pub pad_scans: usize,
    #[serde(default = "default_max_frames")]
    pub max_frames: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DataPackRadar {
    pub site_id: String,
    #[serde(default)]
    pub role: RadarRole,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum RadarRole {
    #[default]
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum TimelineMode {
    #[default]
    Volume,
    LowTilt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LowTiltPolicy {
    #[serde(default = "default_low_tilt_hundredths")]
    pub max_elevation_hundredths_deg: i16,
    #[serde(default = "default_prefer_complete")]
    pub prefer_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DataPackView {
    pub focus_lat: f32,
    pub focus_lon: f32,
    pub range_km: f32,
    #[serde(default)]
    pub follow_target: Option<FollowTarget>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FollowTarget {
    pub lat: f32,
    pub lon: f32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveScan {
    pub site_id: String,
    pub volume_time: DateTime<Utc>,
    pub object_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveWindowRequest {
    pub site_id: String,
    pub start_utc: DateTime<Utc>,
    pub end_utc: DateTime<Utc>,
    pub anchor_utc: DateTime<Utc>,
    pub pad_scans: usize,
    pub max_frames: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveWindowPlan {
    pub scans: Vec<ArchiveScan>,
    pub selected_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TiltCandidate {
    pub cut_index: usize,
    pub elevation_hundredths_deg: i16,
    pub time: DateTime<Utc>,
    pub complete: bool,
}

pub fn plan_archive_window(
    scans: &[ArchiveScan],
    request: &ArchiveWindowRequest,
) -> Option<ArchiveWindowPlan> {
    if request.max_frames == 0 {
        return None;
    }
    let mut scans = scans
        .iter()
        .filter(|scan| scan.site_id.eq_ignore_ascii_case(&request.site_id))
        .cloned()
        .collect::<Vec<_>>();
    if scans.is_empty() {
        return None;
    }
    scans.sort_by(|left, right| {
        left.volume_time
            .cmp(&right.volume_time)
            .then_with(|| left.object_key.cmp(&right.object_key))
    });
    scans.dedup_by(|left, right| {
        left.volume_time == right.volume_time && left.object_key == right.object_key
    });

    let end_utc = request.end_utc.max(request.start_utc);
    let mut start = scans
        .partition_point(|scan| scan.volume_time < request.start_utc)
        .min(scans.len().saturating_sub(1));
    let mut end = scans
        .partition_point(|scan| scan.volume_time <= end_utc)
        .saturating_sub(1)
        .max(start);
    start = start.saturating_sub(request.pad_scans);
    end = (end + request.pad_scans).min(scans.len() - 1);

    if end + 1 - start > request.max_frames {
        start = end + 1 - request.max_frames;
    }

    let window = scans[start..=end].to_vec();
    let selected_index = window
        .iter()
        .enumerate()
        .min_by_key(|(_, scan)| {
            (scan.volume_time - request.anchor_utc)
                .num_milliseconds()
                .unsigned_abs()
        })
        .map(|(index, _)| index)
        .unwrap_or(0);
    Some(ArchiveWindowPlan {
        scans: window,
        selected_index,
    })
}

pub fn low_tilt_frames_for_volume(
    site_id: impl Into<String>,
    volume_time: DateTime<Utc>,
    volume_id: impl Into<String>,
    candidates: &[TiltCandidate],
    policy: LowTiltPolicy,
) -> Vec<TimelineFrame> {
    let site_id = site_id.into();
    let volume_id = volume_id.into();
    let mut lows = candidates
        .iter()
        .filter(|candidate| {
            candidate.elevation_hundredths_deg <= policy.max_elevation_hundredths_deg
        })
        .cloned()
        .collect::<Vec<_>>();
    if lows.is_empty() {
        return Vec::new();
    }
    lows.sort_by(|left, right| {
        left.time
            .cmp(&right.time)
            .then_with(|| left.cut_index.cmp(&right.cut_index))
    });

    if policy.prefer_complete && lows.iter().any(|candidate| candidate.complete) {
        lows.retain(|candidate| candidate.complete);
    }

    lows.into_iter()
        .map(|candidate| TimelineFrame {
            site_id: site_id.clone(),
            volume_time,
            volume_id: volume_id.clone(),
            frame_time: candidate.time,
            cut: Some(TimelineCut {
                cut_index: candidate.cut_index,
                elevation_hundredths_deg: candidate.elevation_hundredths_deg,
                complete: candidate.complete,
            }),
        })
        .collect()
}

pub fn nearest_frame_index(frames: &[TimelineFrame], target: DateTime<Utc>) -> Option<usize> {
    frames
        .iter()
        .enumerate()
        .min_by_key(|(_, frame)| {
            (frame.frame_time - target)
                .num_milliseconds()
                .unsigned_abs()
        })
        .map(|(index, _)| index)
}

pub fn frame_at_or_before_index(frames: &[TimelineFrame], target: DateTime<Utc>) -> Option<usize> {
    frames
        .partition_point(|frame| frame.frame_time <= target)
        .checked_sub(1)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    Stopped,
    Playing,
}

fn default_max_frames() -> usize {
    24
}

fn default_low_tilt_hundredths() -> i16 {
    100
}

fn default_prefer_complete() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone as _, Timelike as _};

    fn t(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 5, 7, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn scan(minute: u32) -> ArchiveScan {
        ArchiveScan {
            site_id: "KTLX".to_owned(),
            volume_time: t(20, minute),
            object_key: format!("KTLX20240507_20{minute:02}00"),
        }
    }

    #[test]
    fn playback_states_are_distinct() {
        assert_ne!(PlaybackState::Stopped, PlaybackState::Playing);
    }

    #[test]
    fn archive_window_pads_and_caps_while_keeping_event_tail() {
        let scans = (0..=45).step_by(5).map(scan).collect::<Vec<_>>();
        let plan = plan_archive_window(
            &scans,
            &ArchiveWindowRequest {
                site_id: "KTLX".to_owned(),
                start_utc: t(20, 12),
                end_utc: t(20, 34),
                anchor_utc: t(20, 21),
                pad_scans: 1,
                max_frames: 5,
            },
        )
        .unwrap();

        assert_eq!(
            plan.scans
                .iter()
                .map(|scan| scan.volume_time.minute())
                .collect::<Vec<_>>(),
            vec![15, 20, 25, 30, 35]
        );
        assert_eq!(plan.selected_index, 1);
    }

    #[test]
    fn archive_window_ignores_other_sites() {
        let mut scans = vec![scan(10), scan(15)];
        scans.push(ArchiveScan {
            site_id: "KFDR".to_owned(),
            volume_time: t(20, 12),
            object_key: "KFDR20240507_201200".to_owned(),
        });

        let plan = plan_archive_window(
            &scans,
            &ArchiveWindowRequest {
                site_id: "KTLX".to_owned(),
                start_utc: t(20, 9),
                end_utc: t(20, 16),
                anchor_utc: t(20, 12),
                pad_scans: 0,
                max_frames: 8,
            },
        )
        .unwrap();

        assert_eq!(plan.scans.len(), 2);
        assert!(plan.scans.iter().all(|scan| scan.site_id == "KTLX"));
    }

    #[test]
    fn low_tilt_mode_turns_sails_revisits_into_timeline_frames() {
        let candidates = vec![
            TiltCandidate {
                cut_index: 0,
                elevation_hundredths_deg: 50,
                time: t(20, 0),
                complete: true,
            },
            TiltCandidate {
                cut_index: 1,
                elevation_hundredths_deg: 50,
                time: t(20, 2),
                complete: true,
            },
            TiltCandidate {
                cut_index: 2,
                elevation_hundredths_deg: 240,
                time: t(20, 3),
                complete: true,
            },
        ];

        let frames = low_tilt_frames_for_volume(
            "KTLX",
            t(20, 0),
            "vol-1",
            &candidates,
            LowTiltPolicy {
                max_elevation_hundredths_deg: 100,
                prefer_complete: true,
            },
        );

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].cut.as_ref().unwrap().cut_index, 0);
        assert_eq!(frames[1].cut.as_ref().unwrap().cut_index, 1);
    }

    #[test]
    fn low_tilt_mode_falls_back_to_incomplete_when_needed() {
        let candidates = vec![TiltCandidate {
            cut_index: 0,
            elevation_hundredths_deg: 50,
            time: t(20, 0),
            complete: false,
        }];

        let frames = low_tilt_frames_for_volume(
            "KTLX",
            t(20, 0),
            "vol-1",
            &candidates,
            LowTiltPolicy {
                max_elevation_hundredths_deg: 100,
                prefer_complete: true,
            },
        );

        assert_eq!(frames.len(), 1);
        assert!(!frames[0].cut.as_ref().unwrap().complete);
    }

    #[test]
    fn sync_helpers_choose_nearest_or_prior_frame() {
        let frames = [10, 20, 30]
            .into_iter()
            .map(|minute| TimelineFrame {
                site_id: "KTLX".to_owned(),
                volume_time: t(20, minute),
                volume_id: minute.to_string(),
                frame_time: t(20, minute),
                cut: None,
            })
            .collect::<Vec<_>>();

        assert_eq!(nearest_frame_index(&frames, t(20, 24)), Some(1));
        assert_eq!(frame_at_or_before_index(&frames, t(20, 24)), Some(1));
        assert_eq!(frame_at_or_before_index(&frames, t(20, 5)), None);
    }
}
