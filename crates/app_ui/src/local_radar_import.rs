//! Time-coherent assembly for local ODIM/CfRadial/JMA file sets.
//!
//! Split-product files from one physical scan may have slightly different
//! write timestamps, but sequential scans must never be collapsed into one
//! volume. Parts are grouped against the earliest timestamp in each group;
//! separate groups become separate loop frames.

use std::collections::BTreeSet;
use std::path::PathBuf;

use radar_core::{MergeReport, RadarVolume};

/// Split-product writers can stamp individual products seconds apart. This
/// remains well below an operational volume cadence while covering the
/// 40-second spread already exercised by `merge_radar_volumes` fixtures.
const SAME_SCAN_TOLERANCE_MILLISECONDS: i64 = 90_000;

pub(crate) struct LocalRadarPart {
    pub path: PathBuf,
    pub format_label: &'static str,
    pub volume: RadarVolume,
}

pub(crate) struct LocalRadarFrame {
    pub paths: Vec<PathBuf>,
    pub volume: RadarVolume,
    pub report: MergeReport,
    format_labels: BTreeSet<&'static str>,
}

impl LocalRadarFrame {
    pub fn source_label(&self, index: usize, frame_count: usize) -> String {
        let formats = self
            .format_labels
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .join("+");
        let group = if frame_count > 1 {
            format!(" time group {}/{frame_count}", index + 1)
        } else {
            String::new()
        };
        let mut label = format!(
            "{formats} merged set{group} ({} files, {} moments merged",
            self.paths.len(),
            self.report.merged_moments
        );
        if self.report.moment_collisions > 0 || self.report.skipped_geometry > 0 {
            label.push_str(", WARNING:");
            if self.report.moment_collisions > 0 {
                label.push_str(&format!(
                    " {} duplicate moments kept first",
                    self.report.moment_collisions
                ));
            }
            if self.report.skipped_geometry > 0 {
                if self.report.moment_collisions > 0 {
                    label.push(',');
                }
                label.push_str(&format!(
                    " {} cuts skipped for incompatible geometry",
                    self.report.skipped_geometry
                ));
            }
        }
        label.push(')');
        label
    }
}

pub(crate) fn merge_time_coherent_parts(
    mut parts: Vec<LocalRadarPart>,
) -> Result<Vec<LocalRadarFrame>, String> {
    let Some(first) = parts.first() else {
        return Err("no local radar files to assemble".to_owned());
    };
    let expected_site = first.volume.site.id.clone();
    for part in &parts[1..] {
        if part.volume.site.id != expected_site {
            return Err(format!(
                "local file set spans different radar sites: '{}' and '{}'; select one site at a time",
                expected_site, part.volume.site.id
            ));
        }
    }

    parts.sort_by(|left, right| {
        left.volume
            .volume_time
            .cmp(&right.volume.volume_time)
            .then_with(|| left.path.cmp(&right.path))
    });

    let mut groups: Vec<Vec<LocalRadarPart>> = Vec::new();
    for part in parts {
        let joins_last_group = groups
            .last()
            .and_then(|group| group.first())
            .is_some_and(|anchor| {
                part.volume
                    .volume_time
                    .signed_duration_since(anchor.volume.volume_time)
                    .num_milliseconds()
                    <= SAME_SCAN_TOLERANCE_MILLISECONDS
            });
        if joins_last_group && let Some(group) = groups.last_mut() {
            group.push(part);
        } else {
            groups.push(vec![part]);
        }
    }

    groups
        .into_iter()
        .map(|group| {
            let paths = group
                .iter()
                .map(|part| part.path.clone())
                .collect::<Vec<_>>();
            let format_labels = group.iter().map(|part| part.format_label).collect();
            let volumes = group
                .into_iter()
                .map(|part| part.volume)
                .collect::<Vec<_>>();
            let (mut volume, report) = radar_core::merge_radar_volumes(volumes)?;
            volume.metadata.source_path = Some(
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" + "),
            );
            Ok(LocalRadarFrame {
                paths,
                volume,
                report,
                format_labels,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use radar_core::{ElevationCut, GateRange, MomentGrid, MomentType, RadarSite};

    fn part(site: &str, seconds: i64, name: &str, moment: MomentType) -> LocalRadarPart {
        let mut volume = RadarVolume::new(
            RadarSite::new(site),
            Utc.timestamp_opt(seconds, 0)
                .single()
                .expect("valid test timestamp"),
        );
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.moments.insert(
            moment.clone(),
            MomentGrid::new_u8(
                moment,
                GateRange {
                    first_gate_m: 0,
                    gate_spacing_m: 250,
                    gate_count: 0,
                },
                1.0,
                0.0,
                None,
                None,
            ),
        );
        volume.cuts.push(cut);
        LocalRadarPart {
            path: PathBuf::from(name),
            format_label: "ODIM_H5",
            volume,
        }
    }

    #[test]
    fn nearby_split_products_merge_but_sequential_scans_become_loop_frames() {
        let frames = merge_time_coherent_parts(vec![
            part("TEST", 340, "second-vel.h5", MomentType::Velocity),
            part("TEST", 0, "first-ref.h5", MomentType::Reflectivity),
            part("TEST", 300, "second-ref.h5", MomentType::Reflectivity),
            part("TEST", 40, "first-vel.h5", MomentType::Velocity),
        ])
        .expect("assemble time groups");

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].volume.volume_time.timestamp(), 0);
        assert_eq!(frames[1].volume.volume_time.timestamp(), 300);
        assert_eq!(frames[0].paths.len(), 2);
        assert_eq!(frames[1].paths.len(), 2);
        assert_eq!(frames[0].report.merged_moments, 1);
        assert_eq!(frames[1].report.merged_moments, 1);
    }

    #[test]
    fn grouping_uses_earliest_time_not_a_transitive_chain() {
        let frames = merge_time_coherent_parts(vec![
            part("TEST", 0, "a.h5", MomentType::Reflectivity),
            part("TEST", 80, "b.h5", MomentType::Velocity),
            part(
                "TEST",
                160,
                "c.h5",
                MomentType::DifferentialReflectivity,
            ),
        ])
        .expect("assemble time groups");

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].paths.len(), 2);
        assert_eq!(frames[1].paths.len(), 1);
    }

    #[test]
    fn different_sites_are_rejected_instead_of_mixed_into_one_history() {
        let error = merge_time_coherent_parts(vec![
            part("ONE", 0, "one.h5", MomentType::Reflectivity),
            part("TWO", 0, "two.h5", MomentType::Velocity),
        ])
        .err()
        .expect("different sites must fail");

        assert!(error.contains("different radar sites"), "{error}");
        assert!(error.contains("ONE") && error.contains("TWO"), "{error}");
    }

    #[test]
    fn merge_data_loss_counters_are_visible_in_the_frame_label() {
        let frames = merge_time_coherent_parts(vec![
            part("TEST", 0, "a.h5", MomentType::Reflectivity),
            part("TEST", 0, "b.h5", MomentType::Reflectivity),
        ])
        .expect("assemble duplicate parts");
        let label = frames[0].source_label(0, 1);

        assert_eq!(frames[0].report.moment_collisions, 1);
        assert!(label.contains("WARNING"), "{label}");
        assert!(label.contains("duplicate moments kept first"), "{label}");
    }
}
