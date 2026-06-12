//! Zip-archive ingest for mobile research radar data (DOW/COW/RaXPol).
//!
//! Field deployments are distributed as `.zip` files holding DORADE
//! sweepfiles (`swp.*`) and/or GR2-style `.msg31` Level II twins, often for
//! several radars in one archive (e.g. a Goodland deployment zip carries
//! `DORADE/DOW7/...` next to `DORADE/COW2/...`). This module discovers radar
//! members, groups DORADE sweeps into volume scans, and decodes everything
//! into [`radar_core::RadarVolume`]s.
//!
//! Lift-and-improve of `gurt-rs/src/archive.rs`. Divergences:
//! - **Volume grouping**: the reference treated every archive member as its
//!   own single-sweep "volume". Here DORADE sweeps are grouped per
//!   instrument into ascending fixed-angle runs ordered by sweep start
//!   time, the way a VCP executes: a 2009 Goshen DOW7 volume spread across
//!   `Tilt 0.5/ ... Tilt 4.0/` member directories reassembles into one
//!   five-cut volume scan, while a COW2 single-tilt 12-second surveillance
//!   sequence becomes one frame per sweep instead of a 24-cut blob. VOLD
//!   volume numbers are deliberately NOT the key: the corpus shows they are
//!   writer-dependent (Goshen DOW7 increments per sweep, COW2 per volume
//!   scan), so elevation-run segmentation is the only convention that holds
//!   across radars. A new run also starts after a 15-minute gap
//!   (deployment pause).
//! - **Member classification by content**: members are sniffed (DORADE
//!   descriptor magic, `AR2V` volume header, gzip/bzip2 wrappers) rather
//!   than trusted by extension alone; naming is only a pre-filter.
//! - **Parallel decode**: members decode on the rayon pool.
//!
//! Sibling-directory grouping for loose (non-zip) sweepfiles lives here too:
//! opening one `swp.*` file pulls in the rest of its ascending run from the
//! same directory.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use radar_core::RadarVolume;
use rayon::prelude::*;
use zip::ZipArchive;

use crate::dorade::{
    append_dorade_sweep, decode_dorade_sweep_volume, finalize_dorade_volume,
    looks_like_dorade_bytes, looks_like_dorade_name, peek_dorade_sweep,
};
use crate::{NexradError, Result, decode_volume_from_bytes};

const ZIP_MAGIC: &[u8; 4] = b"PK\x03\x04";
/// Empty-archive variant of the zip magic ("PK\x05\x06") is not radar data.
const VOLUME_HEADER_MAGIC: &[u8; 4] = b"AR2V";

/// `true` when the buffer starts with a local-file zip signature.
pub fn looks_like_zip_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == ZIP_MAGIC
}

/// `true` when the path claims to be a zip archive.
pub fn looks_like_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

/// One decoded volume scan plus where it came from inside the archive.
#[derive(Clone, Debug)]
pub struct MobileVolume {
    pub volume: RadarVolume,
    /// Display label: first member name of the group (`swp....` or `*.msg31`).
    pub member_label: String,
    /// Number of archive members merged into this volume.
    pub member_count: usize,
}

/// Decode every radar volume in a zip archive, sorted by scan time.
///
/// DORADE members group per instrument into ascending fixed-angle runs (see
/// module docs); `.msg31`/`AR2V` members decode one volume each. Non-radar
/// members are ignored; corrupt members fail the whole load with a
/// descriptive error (a deployment archive with undecodable scans should be
/// visible, not silently thinner).
pub fn decode_mobile_archive_from_path(path: &Path) -> Result<Vec<MobileVolume>> {
    let members = read_radar_members(path)?;
    if members.is_empty() {
        return Err(NexradError::InvalidMessage {
            offset: 0,
            reason: format!(
                "zip archive {} contains no radar members (swp.* or .msg31/AR2V)",
                path.display()
            ),
        });
    }
    decode_members(path, members)
}

/// Decode every radar volume under a deployment FOLDER (recursive, a few
/// levels). Research data ships as directories of per-sweep DORADE files
/// — one file per tilt — so the folder, not the file, is the natural
/// open unit (field report). Same sniffing and volume grouping as zips.
pub fn decode_mobile_dir_from_path(dir: &Path) -> Result<Vec<MobileVolume>> {
    let mut members = Vec::new();
    collect_dir_members(dir, dir, &mut members, 0)?;
    if members.is_empty() {
        return Err(NexradError::InvalidMessage {
            offset: 0,
            reason: format!(
                "folder {} contains no radar files (swp.* sweepfiles or .msg31/AR2V)",
                dir.display()
            ),
        });
    }
    members.sort_by(|left, right| left.name.cmp(&right.name));
    decode_members(dir, members)
}

/// Deployment trees are shallow (day/instrument levels); the cap only
/// guards against scanning an accidentally-chosen huge root.
const MAX_DIR_DEPTH: usize = 4;

fn collect_dir_members(
    root: &Path,
    dir: &Path,
    members: &mut Vec<RadarMember>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_DIR_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|source| NexradError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_dir_members(root, &path, members, depth + 1)?;
            continue;
        }
        let name = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if !plausible_radar_member_name(&name) {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|source| NexradError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if looks_like_dorade_bytes(&bytes)
            || bytes.starts_with(VOLUME_HEADER_MAGIC)
            || bytes.starts_with(&[0x1f, 0x8b])
            || bytes.starts_with(b"BZh")
        {
            members.push(RadarMember { name, bytes });
        }
    }
    Ok(())
}

#[derive(Debug)]
struct RadarMember {
    name: String,
    bytes: Vec<u8>,
}

fn read_radar_members(path: &Path) -> Result<Vec<RadarMember>> {
    let file = File::open(path).map_err(|source| NexradError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut archive = ZipArchive::new(file).map_err(|err| NexradError::InvalidMessage {
        offset: 0,
        reason: format!("not a readable zip archive: {err}"),
    })?;

    let mut members = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| NexradError::InvalidMessage {
                offset: 0,
                reason: format!("zip entry {index}: {err}"),
            })?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        if !plausible_radar_member_name(&name) {
            continue;
        }
        let mut bytes = Vec::with_capacity(entry.size().min(usize::MAX as u64) as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|err| NexradError::InvalidMessage {
                offset: 0,
                reason: format!("zip entry {name}: {err}"),
            })?;
        // Content sniff: extension pre-filter only narrows the candidates.
        if looks_like_dorade_bytes(&bytes)
            || bytes.starts_with(VOLUME_HEADER_MAGIC)
            || bytes.starts_with(&[0x1f, 0x8b])
            || bytes.starts_with(b"BZh")
        {
            members.push(RadarMember { name, bytes });
        }
    }
    members.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(members)
}

/// Names worth opening: `swp.*` sweepfiles and Level II-style members.
fn plausible_radar_member_name(name: &str) -> bool {
    let file_name = name.rsplit('/').next().unwrap_or("");
    if file_name.is_empty() || file_name.starts_with('.') {
        return false;
    }
    if looks_like_dorade_name(file_name) {
        return true;
    }
    Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "msg31" | "ar2v" | "raw" | "gz" | "bz2" | "v06" | "v08"
            )
        })
}

/// Maximum start-time gap between consecutive sweeps of one volume scan.
const MAX_INTRA_VOLUME_GAP_MINUTES: i64 = 15;
/// Fixed angles within this tolerance count as "not ascending".
const FIXED_ANGLE_EPSILON_DEG: f32 = 0.05;

/// A sweep waiting to be grouped: start time + fixed angle drive the
/// ascending-run segmentation, `label` breaks time ties deterministically.
struct GroupableSweep<T> {
    start_time: Option<DateTime<Utc>>,
    fixed_angle_deg: f32,
    label: String,
    payload: T,
}

/// Group one instrument's sweeps (already time-sorted) into volume scans:
/// a scan continues while the fixed angle strictly ascends and sweeps stay
/// within [`MAX_INTRA_VOLUME_GAP_MINUTES`] of each other.
fn segment_volume_runs<T>(mut sweeps: Vec<GroupableSweep<T>>) -> Vec<Vec<GroupableSweep<T>>> {
    sweeps.sort_by(|left, right| {
        left.start_time
            .cmp(&right.start_time)
            .then_with(|| left.label.cmp(&right.label))
    });
    let mut runs: Vec<Vec<GroupableSweep<T>>> = Vec::new();
    for sweep in sweeps {
        let continues_run = runs.last().and_then(|run| run.last()).is_some_and(|last| {
            let ascending = sweep.fixed_angle_deg > last.fixed_angle_deg + FIXED_ANGLE_EPSILON_DEG;
            let close_in_time = match (last.start_time, sweep.start_time) {
                (Some(previous), Some(current)) => {
                    (current - previous).num_minutes() <= MAX_INTRA_VOLUME_GAP_MINUTES
                }
                _ => true,
            };
            ascending && close_in_time
        });
        if continues_run {
            runs.last_mut().expect("run exists").push(sweep);
        } else {
            runs.push(vec![sweep]);
        }
    }
    runs
}

fn decode_members(archive_path: &Path, members: Vec<RadarMember>) -> Result<Vec<MobileVolume>> {
    // Split DORADE sweeps from Level II members, peeking DORADE headers for
    // the grouping metadata.
    let mut per_instrument: BTreeMap<String, Vec<GroupableSweep<RadarMember>>> = BTreeMap::new();
    let mut level2_members: Vec<RadarMember> = Vec::new();
    for member in members {
        if looks_like_dorade_bytes(&member.bytes) {
            let header =
                peek_dorade_sweep(&member.bytes).map_err(|err| with_member(&member.name, err))?;
            per_instrument
                .entry(header.instrument)
                .or_default()
                .push(GroupableSweep {
                    start_time: header.start_time,
                    fixed_angle_deg: header.fixed_angle_deg,
                    label: member.name.clone(),
                    payload: member,
                });
        } else {
            level2_members.push(member);
        }
    }

    let archive_label = archive_path.display().to_string();
    let mut volumes: Vec<MobileVolume> = Vec::new();

    let runs: Vec<Vec<GroupableSweep<RadarMember>>> = per_instrument
        .into_values()
        .flat_map(segment_volume_runs)
        .collect();
    let dorade_volumes: Vec<MobileVolume> = runs
        .into_par_iter()
        .map(|run| {
            let mut volume = RadarVolume::default();
            for sweep in &run {
                append_dorade_sweep(&sweep.payload.bytes, &mut volume)
                    .map_err(|err| with_member(&sweep.payload.name, err))?;
            }
            finalize_dorade_volume(&mut volume);
            let member_label = run[0].payload.name.clone();
            volume.metadata.source_path = Some(format!("{archive_label}::{member_label}"));
            Ok(MobileVolume {
                volume,
                member_label,
                member_count: run.len(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    volumes.extend(dorade_volumes);

    let level2_volumes: Vec<MobileVolume> = level2_members
        .into_par_iter()
        .map(|member| {
            let mut volume = decode_volume_from_bytes(&member.bytes)
                .map_err(|err| with_member(&member.name, err))?;
            volume.metadata.source_path = Some(format!("{archive_label}::{}", member.name));
            Ok(MobileVolume {
                volume,
                member_label: member.name,
                member_count: 1,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    volumes.extend(level2_volumes);

    volumes.sort_by(|left, right| {
        left.volume
            .volume_time
            .cmp(&right.volume.volume_time)
            .then_with(|| left.member_label.cmp(&right.member_label))
    });
    Ok(volumes)
}

fn with_member(name: &str, err: NexradError) -> NexradError {
    NexradError::InvalidMessage {
        offset: 0,
        reason: format!("archive member {name}: {err}"),
    }
}

/// Descriptor blocks live at the head of a sweepfile; this is enough bytes
/// to peek COMM/SSWB/VOLD/RADD/PARM*/CELV/CSFD/SWIB without reading rays.
const PEEK_HEAD_BYTES: usize = 64 * 1024;

/// Decode the full volume scan a loose sweepfile belongs to.
///
/// Scans the file's directory for sibling `swp.*` files from the same
/// instrument, segments them into ascending fixed-angle runs (see module
/// docs), and decodes the run containing `path` as one volume. Sibling
/// headers are peeked from the first [`PEEK_HEAD_BYTES`] only, so opening a
/// file in a large deployment directory stays cheap.
pub fn decode_dorade_volume_for_path(path: &Path) -> Result<RadarVolume> {
    let bytes = std::fs::read(path).map_err(|source| NexradError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let header = peek_dorade_sweep(&bytes)?;

    let mut sweeps: Vec<GroupableSweep<PathBuf>> = vec![GroupableSweep {
        start_time: header.start_time,
        fixed_angle_deg: header.fixed_angle_deg,
        label: path.display().to_string(),
        payload: path.to_path_buf(),
    }];
    if let Some(directory) = path.parent()
        && let Ok(entries) = std::fs::read_dir(directory)
    {
        for entry in entries.flatten() {
            let sibling = entry.path();
            if sibling == *path || !sibling.is_file() {
                continue;
            }
            if !crate::dorade::looks_like_dorade_path(&sibling) {
                continue;
            }
            let Some(head) = read_file_head(&sibling, PEEK_HEAD_BYTES) else {
                continue;
            };
            let Ok(sibling_header) = peek_dorade_sweep(&head) else {
                continue;
            };
            if sibling_header.instrument == header.instrument {
                sweeps.push(GroupableSweep {
                    start_time: sibling_header.start_time,
                    fixed_angle_deg: sibling_header.fixed_angle_deg,
                    label: sibling.display().to_string(),
                    payload: sibling,
                });
            }
        }
    }

    let runs = segment_volume_runs(sweeps);
    let run = runs
        .into_iter()
        .find(|run| run.iter().any(|sweep| sweep.payload == *path))
        .expect("opened sweep belongs to one run");

    if run.len() == 1 {
        let mut volume = decode_dorade_sweep_volume(&bytes)?;
        volume.metadata.source_path = Some(path.display().to_string());
        return Ok(volume);
    }

    let mut volume = RadarVolume::default();
    for sweep in &run {
        let data = if sweep.payload == *path {
            bytes.clone()
        } else {
            std::fs::read(&sweep.payload).map_err(|source| NexradError::Io {
                path: sweep.payload.display().to_string(),
                source,
            })?
        };
        append_dorade_sweep(&data, &mut volume).map_err(|err| with_member(&sweep.label, err))?;
    }
    finalize_dorade_volume(&mut volume);
    volume.metadata.source_path = Some(path.display().to_string());
    Ok(volume)
}

fn read_file_head(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut head = vec![0u8; limit];
    let mut filled = 0usize;
    loop {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(count) => {
                filled += count;
                if filled == head.len() {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    head.truncate(filled);
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Synthetic sweep start times: a fixed base plus per-sweep offsets.
    const BASE_UNIX: i32 = 1_779_404_114; // 2026-05-21T22:55:14Z

    /// Minimal synthetic big-endian DORADE sweep (mirrors dorade.rs tests).
    fn synthetic_sweep(instrument: &[u8; 4], start_offset_s: i32, fixed_angle: f32) -> Vec<u8> {
        fn block(id: &[u8; 4], len: usize) -> Vec<u8> {
            let mut bytes = vec![0u8; len];
            bytes[..4].copy_from_slice(id);
            bytes[4..8].copy_from_slice(&(len as i32).to_be_bytes());
            bytes
        }
        let mut bytes = Vec::new();

        let mut sswb = block(b"SSWB", 200);
        sswb[12..16].copy_from_slice(&(BASE_UNIX + start_offset_s).to_be_bytes());
        bytes.extend(sswb);

        let mut vold = block(b"VOLD", 72);
        vold[10..12].copy_from_slice(&7i16.to_be_bytes());
        vold[36..38].copy_from_slice(&2026i16.to_be_bytes());
        vold[38..40].copy_from_slice(&5i16.to_be_bytes());
        vold[40..42].copy_from_slice(&21i16.to_be_bytes());
        bytes.extend(vold);

        let mut radd = block(b"RADD", 144);
        radd[8..12].copy_from_slice(instrument);
        radd[50..52].copy_from_slice(&8i16.to_be_bytes());
        radd[80..84].copy_from_slice(&(-103.29f32).to_bits().to_be_bytes());
        radd[84..88].copy_from_slice(&39.74f32.to_bits().to_be_bytes());
        radd[88..92].copy_from_slice(&1.519f32.to_bits().to_be_bytes());
        bytes.extend(radd);

        let mut parm = block(b"PARM", 216);
        parm[8..11].copy_from_slice(b"DBZ");
        parm[78..80].copy_from_slice(&2i16.to_be_bytes());
        parm[92..96].copy_from_slice(&100.0f32.to_bits().to_be_bytes());
        parm[100..104].copy_from_slice(&(-32768i32).to_be_bytes());
        parm[200..204].copy_from_slice(&2i32.to_be_bytes());
        parm[204..208].copy_from_slice(&50.0f32.to_bits().to_be_bytes());
        parm[208..212].copy_from_slice(&100.0f32.to_bits().to_be_bytes());
        bytes.extend(parm);

        let mut swib = block(b"SWIB", 40);
        swib[16..20].copy_from_slice(&1i32.to_be_bytes());
        swib[32..36].copy_from_slice(&fixed_angle.to_bits().to_be_bytes());
        bytes.extend(swib);

        let mut ryib = block(b"RYIB", 44);
        ryib[24..28].copy_from_slice(&45.0f32.to_bits().to_be_bytes());
        ryib[28..32].copy_from_slice(&fixed_angle.to_bits().to_be_bytes());
        bytes.extend(ryib);

        let mut rdat = block(b"RDAT", 20);
        rdat[8..11].copy_from_slice(b"DBZ");
        rdat[16..18].copy_from_slice(&1000i16.to_be_bytes());
        rdat[18..20].copy_from_slice(&2000i16.to_be_bytes());
        bytes.extend(rdat);

        bytes
    }

    fn write_zip(path: &Path, members: &[(&str, Vec<u8>)]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes) in members {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn groups_zip_members_into_ascending_elevation_runs_per_instrument() {
        let dir = std::env::temp_dir().join("bowecho_mobile_archive_group_test");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("deployment.zip");
        write_zip(
            &zip_path,
            &[
                // One ascending 0.5°→1.0° run split across tilt member
                // directories, then a new run, then a second radar, plus
                // chaff that must be ignored.
                (
                    "Tilt 0.5/swp.1260521225514.TST1.0.0.5_SUR_v7",
                    synthetic_sweep(b"TST1", 0, 0.5),
                ),
                (
                    "Tilt 1.0/swp.1260521225520.TST1.0.1.0_SUR_v7",
                    synthetic_sweep(b"TST1", 6, 1.0),
                ),
                (
                    "Tilt 0.5/swp.1260521225600.TST1.0.0.5_SUR_v8",
                    synthetic_sweep(b"TST1", 46, 0.5),
                ),
                (
                    "OTHER/swp.1260521225514.TST2.0.0.5_SUR_v7",
                    synthetic_sweep(b"TST2", 0, 0.5),
                ),
                ("README.txt", b"not radar data".to_vec()),
            ],
        );

        let volumes = decode_mobile_archive_from_path(&zip_path).unwrap();

        assert_eq!(volumes.len(), 3);
        let two_cut = volumes
            .iter()
            .find(|v| v.volume.site.id == "TST1" && v.member_count == 2)
            .expect("two-cut TST1 volume");
        assert_eq!(two_cut.volume.cuts.len(), 2);
        assert!(two_cut.volume.cuts[0].elevation_deg < two_cut.volume.cuts[1].elevation_deg);
        assert!(
            volumes
                .iter()
                .any(|v| v.volume.site.id == "TST1" && v.member_count == 1)
        );
        assert!(volumes.iter().any(|v| v.volume.site.id == "TST2"));
        std::fs::remove_file(&zip_path).ok();
    }

    #[test]
    fn same_elevation_sequences_become_one_volume_per_sweep() {
        // COW2-style single-tilt surveillance: 1.0°, 1.0°, 1.0° must NOT
        // merge into one volume.
        let runs = segment_volume_runs(vec![
            GroupableSweep {
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX), 0),
                fixed_angle_deg: 1.0,
                label: "a".into(),
                payload: (),
            },
            GroupableSweep {
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX) + 12, 0),
                fixed_angle_deg: 1.0,
                label: "b".into(),
                payload: (),
            },
            GroupableSweep {
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX) + 24, 0),
                fixed_angle_deg: 1.0,
                label: "c".into(),
                payload: (),
            },
        ]);
        assert_eq!(runs.len(), 3);
    }

    #[test]
    fn long_time_gap_splits_an_ascending_run() {
        let runs = segment_volume_runs(vec![
            GroupableSweep {
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX), 0),
                fixed_angle_deg: 0.5,
                label: "a".into(),
                payload: (),
            },
            GroupableSweep {
                // Ascending but an hour later: deployment pause.
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX) + 3600, 0),
                fixed_angle_deg: 1.0,
                label: "b".into(),
                payload: (),
            },
        ]);
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn rejects_archive_without_radar_members() {
        let dir = std::env::temp_dir().join("bowecho_mobile_archive_empty_test");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("empty.zip");
        write_zip(&zip_path, &[("README.txt", b"nothing here".to_vec())]);

        let err = decode_mobile_archive_from_path(&zip_path).unwrap_err();
        assert!(err.to_string().contains("no radar members"));
        std::fs::remove_file(&zip_path).ok();
    }

    #[test]
    fn loose_sweepfile_groups_directory_siblings_from_same_run() {
        let dir = std::env::temp_dir().join("bowecho_mobile_archive_loose_test");
        std::fs::create_dir_all(&dir).unwrap();
        // Ascending same-instrument run → grouped; the next run → excluded.
        let low = dir.join("swp.1260521225514.TST1.0.0.5_SUR_v7");
        let high = dir.join("swp.1260521225520.TST1.0.1.0_SUR_v7");
        let other = dir.join("swp.1260521225600.TST1.0.0.5_SUR_v8");
        std::fs::write(&low, synthetic_sweep(b"TST1", 0, 0.5)).unwrap();
        std::fs::write(&high, synthetic_sweep(b"TST1", 6, 1.0)).unwrap();
        std::fs::write(&other, synthetic_sweep(b"TST1", 46, 0.5)).unwrap();

        let volume = decode_dorade_volume_for_path(&low).unwrap();

        assert_eq!(volume.site.id, "TST1");
        assert_eq!(volume.cuts.len(), 2);
        for path in [&low, &high, &other] {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn zip_sniffers_match_magic_and_extension() {
        assert!(looks_like_zip_bytes(b"PK\x03\x04rest"));
        assert!(!looks_like_zip_bytes(b"PK\x05\x06"));
        assert!(looks_like_zip_path(Path::new("c:/data/deploy.ZIP")));
        assert!(!looks_like_zip_path(Path::new("c:/data/deploy.tar")));
    }

    #[test]
    fn member_name_prefilter_accepts_observed_layouts() {
        assert!(plausible_radar_member_name(
            "DORADE/COW2/swp.1260516225229.COW2.515.1.0_SUR_v237"
        ));
        assert!(plausible_radar_member_name(
            "GR2 MSG31/COW2/nexrad.20260516_225229_COW2_v237_SUR.msg31"
        ));
        assert!(!plausible_radar_member_name("GR2 - README.txt"));
        assert!(!plausible_radar_member_name("DORADE/COW2/"));
    }
}
