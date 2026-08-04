//! Pure WRF scene inventory and compatibility grouping.
//!
//! A scene is one `(path, time_index)` record.  A parsed internal `Times`
//! value is authoritative even when the filename disagrees.  A filename stamp
//! is retained as an explicit fallback provenance, and a scene with neither
//! remains untimed; this module never manufactures epoch-based timestamps.
//!
//! Grouping is intentionally stricter than "all selected wrfout files": run,
//! WRF domain, grid signature, and producer must all match. That keeps d01/d02,
//! ArWen/other-WRF, and geometrically incompatible grids out of one loop.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WrfRunId(pub String);

impl From<&str> for WrfRunId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WrfDomainId(pub u16);

impl WrfDomainId {
    pub fn label(self) -> String {
        format!("d{:02}", self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WrfRunDomain {
    pub run: WrfRunId,
    pub domain: WrfDomainId,
}

/// Stable file/content identity supplied by the ingest boundary.  It is kept
/// separate from the display path so a caller can preserve identity across a
/// move, cache alias, or re-open.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WrfSourceIdentity(pub String);

impl From<&str> for WrfSourceIdentity {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// Model producer carried separately from the rw-store model slug. ArWen
/// writes ordinary WRF output, so it stays in the `wrf` storage lane while
/// this identity prevents a mixed ArWen/other-WRF selection from being
/// silently merged into one run.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WrfProducerIdentity {
    Wrf,
    Arwen { version: String },
}

impl WrfProducerIdentity {
    pub fn from_gpuwm_version(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::Wrf;
        };
        let mut version = String::new();
        let mut previous_space = false;
        for character in raw
            .trim_matches(|character: char| character.is_whitespace() || character == '\0')
            .chars()
        {
            if version.len() >= 64 {
                break;
            }
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+') {
                version.push(character);
                previous_space = false;
            } else if character.is_whitespace() && !previous_space && !version.is_empty() {
                version.push(' ');
                previous_space = true;
            }
        }
        let version = version.trim().to_owned();
        if version.is_empty() {
            Self::Wrf
        } else {
            Self::Arwen { version }
        }
    }

    pub fn display_label(&self) -> String {
        match self {
            Self::Wrf => "WRF".to_owned(),
            Self::Arwen { version } => format!("ArWen {version}"),
        }
    }
}

/// Compatibility signature for a WRF mass grid.
///
/// `horizontal_coordinate_digest` is computed by the future NetCDF inventory
/// adapter over projection/XLAT/XLONG identity.  Keeping it opaque here makes
/// this module pure while still preventing equal-shaped but shifted/rotated
/// grids from being grouped.  Grid spacing is normalized to millimetres so
/// equality and ordering do not depend on floating-point NaN semantics.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WrfGridSignature {
    pub nx: usize,
    pub ny: usize,
    pub nz: Option<usize>,
    pub dx_millimeters: Option<u64>,
    pub dy_millimeters: Option<u64>,
    pub projection_identity: String,
    pub horizontal_coordinate_digest: u64,
}

impl WrfGridSignature {
    #[allow(clippy::too_many_arguments)]
    pub fn from_meters(
        nx: usize,
        ny: usize,
        nz: Option<usize>,
        dx_meters: Option<f64>,
        dy_meters: Option<f64>,
        projection_identity: impl Into<String>,
        horizontal_coordinate_digest: u64,
    ) -> Self {
        Self {
            nx,
            ny,
            nz,
            dx_millimeters: dx_meters.and_then(grid_spacing_millimeters),
            dy_millimeters: dy_meters.and_then(grid_spacing_millimeters),
            projection_identity: projection_identity.into(),
            horizontal_coordinate_digest,
        }
    }
}

fn grid_spacing_millimeters(meters: f64) -> Option<u64> {
    let millimeters = meters * 1_000.0;
    (meters.is_finite() && meters > 0.0 && millimeters <= u64::MAX as f64)
        .then(|| millimeters.round() as u64)
}

/// Provenance-bearing scene time.  Only [`Self::InternalTimes`] is an
/// authoritative model time; consumers can choose whether filename fallbacks
/// are sufficient for their operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WrfSceneTime {
    InternalTimes {
        valid_time: DateTime<Utc>,
        raw: String,
    },
    FilenameFallback {
        valid_time: DateTime<Utc>,
        invalid_internal_times: Option<String>,
    },
    Unavailable {
        invalid_internal_times: Option<String>,
    },
}

impl WrfSceneTime {
    /// Resolve one scene time without inventing offsets for missing records.
    /// A valid internal `Times[timeidx]` always wins over the filename.
    pub fn from_sources(internal_times: Option<&str>, path: &Path) -> Self {
        if let Some(raw) = internal_times
            && let Some(valid_time) = parse_wrf_internal_time(raw)
        {
            return Self::InternalTimes {
                valid_time,
                raw: raw.to_string(),
            };
        }

        let invalid_internal_times = internal_times.map(str::to_string);
        match parse_wrf_filename_time(path) {
            Some(valid_time) => Self::FilenameFallback {
                valid_time,
                invalid_internal_times,
            },
            None => Self::Unavailable {
                invalid_internal_times,
            },
        }
    }

    pub fn valid_time(&self) -> Option<&DateTime<Utc>> {
        match self {
            Self::InternalTimes { valid_time, .. } | Self::FilenameFallback { valid_time, .. } => {
                Some(valid_time)
            }
            Self::Unavailable { .. } => None,
        }
    }

    pub const fn is_authoritative(&self) -> bool {
        matches!(self, Self::InternalTimes { .. })
    }

    pub fn invalid_internal_times(&self) -> Option<&str> {
        match self {
            Self::InternalTimes { .. } => None,
            Self::FilenameFallback {
                invalid_internal_times,
                ..
            }
            | Self::Unavailable {
                invalid_internal_times,
            } => invalid_internal_times.as_deref(),
        }
    }
}

/// Parse a WRF `Times` record (`YYYY-MM-DD_HH:MM:SS`) as UTC.
pub fn parse_wrf_internal_time(raw: &str) -> Option<DateTime<Utc>> {
    let cleaned = raw.trim_matches(|ch: char| ch.is_whitespace() || ch == '\0');
    NaiveDateTime::parse_from_str(cleaned, "%Y-%m-%d_%H:%M:%S")
        .ok()
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Parse a wrfout-style filename timestamp.  Both Windows-safe underscores
/// and the conventional colon form are accepted.
pub fn parse_wrf_filename_time(path: &Path) -> Option<DateTime<Utc>> {
    let name = path.file_name()?.to_str()?;
    let bytes = name.as_bytes();
    if bytes.len() < 19 {
        return None;
    }

    for candidate in bytes.windows(19) {
        let separators_match = candidate[4] == b'-'
            && candidate[7] == b'-'
            && candidate[10] == b'_'
            && matches!(candidate[13], b':' | b'_')
            && matches!(candidate[16], b':' | b'_');
        let digits_match = candidate
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7 | 10 | 13 | 16) || byte.is_ascii_digit());
        if !separators_match || !digits_match {
            continue;
        }

        let candidate = std::str::from_utf8(candidate).ok()?;
        let normalized = candidate.replace([':', '_'], "_");
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%d_%H_%M_%S") {
            return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
        }
    }
    None
}

/// Extract the standard `dNN` component from a wrfout-style filename.
pub fn parse_wrf_domain_id(path: &Path) -> Option<WrfDomainId> {
    let name = path.file_name()?.to_str()?;
    name.split(['_', '.']).find_map(|token| {
        let digits = token.strip_prefix('d')?;
        (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| digits.parse::<u16>().ok().map(WrfDomainId))
            .flatten()
    })
}

/// One inventory record for one WRF time index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrfScene {
    pub path: PathBuf,
    pub time_index: usize,
    pub run_domain: WrfRunDomain,
    pub grid_signature: WrfGridSignature,
    pub producer: WrfProducerIdentity,
    pub source_identity: WrfSourceIdentity,
    pub time: WrfSceneTime,
}

impl WrfScene {
    pub fn locator(&self) -> WrfSceneLocator {
        WrfSceneLocator {
            path: self.path.clone(),
            time_index: self.time_index,
            source_identity: self.source_identity.clone(),
        }
    }
}

/// Deterministic diagnostic handle for one scene.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrfSceneLocator {
    pub path: PathBuf,
    pub time_index: usize,
    pub source_identity: WrfSourceIdentity,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WrfSceneGroupKey {
    pub run_domain: WrfRunDomain,
    pub grid_signature: WrfGridSignature,
    pub producer: WrfProducerIdentity,
}

impl WrfSceneGroupKey {
    pub fn is_compatible(&self, scene: &WrfScene) -> bool {
        self.run_domain == scene.run_domain
            && self.grid_signature == scene.grid_signature
            && self.producer == scene.producer
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateSceneTime {
    pub valid_time: DateTime<Utc>,
    /// Stable source order, never caller selection order.
    pub scenes: Vec<WrfSceneLocator>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NonmonotonicSceneTime {
    /// Previous known time in stable `(path, timeidx, source identity)` order.
    pub previous_scene: WrfSceneLocator,
    pub previous_time: DateTime<Utc>,
    /// Current scene whose time moved backwards.
    pub scene: WrfSceneLocator,
    pub time: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidInternalTime {
    pub scene: WrfSceneLocator,
    pub raw: String,
    pub used_filename_fallback: bool,
}

/// Diagnostics are split into stable lists so duplicate and nonmonotonic
/// conditions cannot be hidden by final chronological sorting.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WrfSceneDiagnostics {
    pub duplicate_times: Vec<DuplicateSceneTime>,
    pub nonmonotonic_times: Vec<NonmonotonicSceneTime>,
    pub unavailable_times: Vec<WrfSceneLocator>,
    pub invalid_internal_times: Vec<InvalidInternalTime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrfSceneGroup {
    pub key: WrfSceneGroupKey,
    /// Chronological playback order; filename fallbacks are allowed but remain
    /// provenance-marked, and untimed scenes sort last.
    pub scenes: Vec<WrfScene>,
    pub diagnostics: WrfSceneDiagnostics,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WrfSceneInventory {
    /// Deterministic `(run, domain, grid signature, producer)` order.
    pub groups: Vec<WrfSceneGroup>,
}

impl WrfSceneInventory {
    pub fn from_scenes(scenes: impl IntoIterator<Item = WrfScene>) -> Self {
        let mut groups: BTreeMap<WrfSceneGroupKey, Vec<WrfScene>> = BTreeMap::new();
        for scene in scenes {
            let key = WrfSceneGroupKey {
                run_domain: scene.run_domain.clone(),
                grid_signature: scene.grid_signature.clone(),
                producer: scene.producer.clone(),
            };
            groups.entry(key).or_default().push(scene);
        }

        let groups = groups
            .into_iter()
            .map(|(key, mut scenes)| {
                let diagnostics = diagnose_source_order(&scenes);
                scenes.sort_by(playback_order);
                WrfSceneGroup {
                    key,
                    scenes,
                    diagnostics,
                }
            })
            .collect();
        Self { groups }
    }
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn source_order(left: &WrfScene, right: &WrfScene) -> Ordering {
    normalized_path(&left.path)
        .cmp(&normalized_path(&right.path))
        .then_with(|| left.time_index.cmp(&right.time_index))
        .then_with(|| left.source_identity.cmp(&right.source_identity))
}

fn time_provenance_rank(time: &WrfSceneTime) -> u8 {
    match time {
        WrfSceneTime::InternalTimes { .. } => 0,
        WrfSceneTime::FilenameFallback { .. } => 1,
        WrfSceneTime::Unavailable { .. } => 2,
    }
}

fn playback_order(left: &WrfScene, right: &WrfScene) -> Ordering {
    match (left.time.valid_time(), right.time.valid_time()) {
        (Some(left_time), Some(right_time)) => left_time
            .cmp(right_time)
            .then_with(|| time_provenance_rank(&left.time).cmp(&time_provenance_rank(&right.time)))
            .then_with(|| source_order(left, right)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => source_order(left, right),
    }
}

fn diagnose_source_order(scenes: &[WrfScene]) -> WrfSceneDiagnostics {
    let mut source_scenes: Vec<&WrfScene> = scenes.iter().collect();
    source_scenes.sort_by(|left, right| source_order(left, right));

    let mut diagnostics = WrfSceneDiagnostics::default();
    let mut by_time: BTreeMap<DateTime<Utc>, Vec<WrfSceneLocator>> = BTreeMap::new();
    let mut previous_known: Option<(&WrfScene, DateTime<Utc>)> = None;

    for scene in source_scenes {
        if let Some(raw) = scene.time.invalid_internal_times() {
            diagnostics
                .invalid_internal_times
                .push(InvalidInternalTime {
                    scene: scene.locator(),
                    raw: raw.to_string(),
                    used_filename_fallback: matches!(
                        &scene.time,
                        WrfSceneTime::FilenameFallback { .. }
                    ),
                });
        }

        let Some(valid_time) = scene.time.valid_time().cloned() else {
            diagnostics.unavailable_times.push(scene.locator());
            continue;
        };
        by_time
            .entry(valid_time.to_owned())
            .or_default()
            .push(scene.locator());

        if let Some((previous_scene, previous_time)) = previous_known
            && valid_time < previous_time
        {
            diagnostics.nonmonotonic_times.push(NonmonotonicSceneTime {
                previous_scene: previous_scene.locator(),
                previous_time: previous_time.to_owned(),
                scene: scene.locator(),
                time: valid_time.to_owned(),
            });
        }
        previous_known = Some((scene, valid_time));
    }

    diagnostics.duplicate_times = by_time
        .into_iter()
        .filter_map(|(valid_time, scenes)| {
            (scenes.len() > 1).then_some(DuplicateSceneTime { valid_time, scenes })
        })
        .collect();
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn internal(raw: &str) -> WrfSceneTime {
        WrfSceneTime::from_sources(Some(raw), Path::new("no-filename-time"))
    }

    fn grid(digest: u64) -> WrfGridSignature {
        WrfGridSignature::from_meters(
            400,
            300,
            Some(50),
            Some(3_000.0),
            Some(3_000.0),
            "lambert:30:60:-98",
            digest,
        )
    }

    fn scene(
        path: &str,
        time_index: usize,
        run: &str,
        domain: u16,
        grid_digest: u64,
        source: &str,
        time: WrfSceneTime,
    ) -> WrfScene {
        WrfScene {
            path: PathBuf::from(path),
            time_index,
            run_domain: WrfRunDomain {
                run: run.into(),
                domain: WrfDomainId(domain),
            },
            grid_signature: grid(grid_digest),
            producer: WrfProducerIdentity::Wrf,
            source_identity: source.into(),
            time,
        }
    }

    fn hour(time: &WrfSceneTime) -> u32 {
        use chrono::Timelike;
        time.valid_time().unwrap().hour()
    }

    #[test]
    fn internal_times_is_authoritative_over_a_disagreeing_filename() {
        let resolved = WrfSceneTime::from_sources(
            Some("2026-05-10_01:00:00"),
            Path::new("wrfout_d01_2026-05-10_04_00_00"),
        );
        assert!(resolved.is_authoritative());
        assert_eq!(hour(&resolved), 1);
        assert!(matches!(resolved, WrfSceneTime::InternalTimes { .. }));
    }

    #[test]
    fn filename_fallback_and_unavailable_time_never_fabricate_an_epoch() {
        let fallback = WrfSceneTime::from_sources(
            Some("not-a-wrf-time"),
            Path::new("wrfout_d01_2026-05-10_04_00_00"),
        );
        assert_eq!(hour(&fallback), 4);
        assert!(matches!(
            fallback,
            WrfSceneTime::FilenameFallback {
                invalid_internal_times: Some(_),
                ..
            }
        ));

        let unavailable = WrfSceneTime::from_sources(None, Path::new("wrfout_d01_unknown"));
        assert!(unavailable.valid_time().is_none());
        assert!(matches!(
            unavailable,
            WrfSceneTime::Unavailable {
                invalid_internal_times: None
            }
        ));
    }

    #[test]
    fn filename_and_domain_parsers_accept_normal_wrf_names() {
        let colon = Path::new("wrfout_d02_2026-05-10_04:30:00");
        let underscore = Path::new("wrfout_d03_2026-05-10_04_30_00");
        assert_eq!(
            parse_wrf_filename_time(colon),
            parse_wrf_filename_time(underscore)
        );
        assert_eq!(parse_wrf_domain_id(colon), Some(WrfDomainId(2)));
        assert_eq!(parse_wrf_domain_id(underscore), Some(WrfDomainId(3)));
        assert!(parse_wrf_filename_time(Path::new("wrfout_d01_2026-02-30_00_00_00")).is_none());
    }

    #[test]
    fn feedback_v03412_arwen_identity_is_bounded_and_separates_other_wrf_scenes() {
        assert_eq!(
            WrfProducerIdentity::from_gpuwm_version(Some(" 1.5.1\0 ")),
            WrfProducerIdentity::Arwen {
                version: "1.5.1".to_owned()
            }
        );
        assert_eq!(
            WrfProducerIdentity::from_gpuwm_version(Some("\0 \t")),
            WrfProducerIdentity::Wrf
        );

        let mut arwen = scene(
            "run/arwen",
            0,
            "run",
            1,
            10,
            "arwen",
            internal("2026-05-10_01:00:00"),
        );
        arwen.producer = WrfProducerIdentity::Arwen {
            version: "1.5.1".to_owned(),
        };
        let ordinary = scene(
            "run/wrf",
            0,
            "run",
            1,
            10,
            "wrf",
            internal("2026-05-10_02:00:00"),
        );
        assert_eq!(
            WrfSceneInventory::from_scenes([arwen, ordinary])
                .groups
                .len(),
            2
        );
    }

    #[test]
    fn groups_separate_runs_domains_and_incompatible_grids_then_sort_times() {
        let scenes = vec![
            scene(
                "run-a/wrfout_d01_b",
                0,
                "run-a",
                1,
                10,
                "a-b",
                internal("2026-05-10_02:00:00"),
            ),
            scene(
                "run-a/wrfout_d02_a",
                0,
                "run-a",
                2,
                10,
                "a-d02",
                internal("2026-05-10_01:00:00"),
            ),
            scene(
                "run-a/wrfout_d01_a",
                0,
                "run-a",
                1,
                10,
                "a-a",
                internal("2026-05-10_01:00:00"),
            ),
            scene(
                "run-a/remesh/wrfout_d01_a",
                0,
                "run-a",
                1,
                99,
                "a-remesh",
                internal("2026-05-10_01:00:00"),
            ),
            scene(
                "run-b/wrfout_d01_a",
                0,
                "run-b",
                1,
                10,
                "b-a",
                internal("2026-05-10_01:00:00"),
            ),
        ];

        let inventory = WrfSceneInventory::from_scenes(scenes);
        assert_eq!(inventory.groups.len(), 4);
        let compatible = inventory
            .groups
            .iter()
            .find(|group| {
                group.key.run_domain.run == WrfRunId("run-a".to_string())
                    && group.key.run_domain.domain == WrfDomainId(1)
                    && group.key.grid_signature.horizontal_coordinate_digest == 10
            })
            .unwrap();
        assert_eq!(compatible.scenes.len(), 2);
        assert_eq!(hour(&compatible.scenes[0].time), 1);
        assert_eq!(hour(&compatible.scenes[1].time), 2);
        assert!(compatible.key.is_compatible(&compatible.scenes[0]));
    }

    #[test]
    fn duplicate_and_nonmonotonic_times_are_deterministic_before_playback_sort() {
        let a = scene(
            "run/wrfout_d01_00",
            0,
            "run",
            1,
            10,
            "source-a",
            internal("2026-05-10_01:00:00"),
        );
        let b = scene(
            "run/wrfout_d01_01",
            0,
            "run",
            1,
            10,
            "source-b",
            internal("2026-05-10_01:00:00"),
        );
        let c = scene(
            "run/wrfout_d01_02",
            0,
            "run",
            1,
            10,
            "source-c",
            internal("2026-05-10_00:00:00"),
        );

        let forward = WrfSceneInventory::from_scenes([a.clone(), b.clone(), c.clone()]);
        let shuffled = WrfSceneInventory::from_scenes([c, a, b]);
        assert_eq!(forward, shuffled);

        let group = &forward.groups[0];
        assert_eq!(group.diagnostics.duplicate_times.len(), 1);
        assert_eq!(group.diagnostics.duplicate_times[0].scenes.len(), 2);
        assert_eq!(group.diagnostics.nonmonotonic_times.len(), 1);
        assert_eq!(
            group.diagnostics.nonmonotonic_times[0]
                .scene
                .source_identity,
            WrfSourceIdentity("source-c".to_string())
        );
        assert_eq!(hour(&group.scenes[0].time), 0);
    }

    #[test]
    fn missing_multi_time_records_repeat_the_fallback_instead_of_inventing_hours() {
        let path = "run/wrfout_d01_2026-05-10_04_00_00";
        let first = scene(
            path,
            0,
            "run",
            1,
            10,
            "one-file",
            WrfSceneTime::from_sources(None, Path::new(path)),
        );
        let second = scene(
            path,
            1,
            "run",
            1,
            10,
            "one-file",
            WrfSceneTime::from_sources(None, Path::new(path)),
        );

        let inventory = WrfSceneInventory::from_scenes([second, first]);
        let group = &inventory.groups[0];
        assert_eq!(
            group.scenes[0].time.valid_time(),
            group.scenes[1].time.valid_time()
        );
        assert_eq!(group.diagnostics.duplicate_times.len(), 1);
        assert!(group.diagnostics.nonmonotonic_times.is_empty());
    }

    #[test]
    fn internal_times_are_checked_in_timeidx_order_within_one_file() {
        let path = "run/wrfout_d01_one_file";
        let first = scene(
            path,
            0,
            "run",
            1,
            10,
            "one-file",
            internal("2026-05-10_02:00:00"),
        );
        let second = scene(
            path,
            1,
            "run",
            1,
            10,
            "one-file",
            internal("2026-05-10_01:00:00"),
        );

        let inventory = WrfSceneInventory::from_scenes([second, first]);
        let issue = &inventory.groups[0].diagnostics.nonmonotonic_times[0];
        assert_eq!(issue.previous_scene.time_index, 0);
        assert_eq!(issue.scene.time_index, 1);
    }

    #[test]
    fn invalid_internal_and_unavailable_times_are_reported_with_full_locators() {
        let bad = scene(
            "run/not-timestamped",
            7,
            "run",
            1,
            10,
            "bad-source",
            WrfSceneTime::from_sources(Some("bad Times value"), Path::new("not-timestamped")),
        );
        let inventory = WrfSceneInventory::from_scenes([bad]);
        let diagnostics = &inventory.groups[0].diagnostics;
        assert_eq!(diagnostics.invalid_internal_times.len(), 1);
        assert!(!diagnostics.invalid_internal_times[0].used_filename_fallback);
        assert_eq!(diagnostics.unavailable_times.len(), 1);
        assert_eq!(diagnostics.unavailable_times[0].time_index, 7);
        assert_eq!(
            diagnostics.unavailable_times[0].source_identity,
            WrfSourceIdentity("bad-source".to_string())
        );
    }
}
