//! CfRadial 1.x decoder (classic-netCDF radar moments).
//!
//! Format reference: M. Dixon and W.-C. Lee, "CfRadial Data File Format —
//! CF-compliant netCDF Format for Moments Data for RADAR and LIDAR",
//! NCAR/EOL, version 1.4 (2016) (versions 1.1–1.4 share the layout read
//! here). CfRadial 1 files are classic netCDF (`CDF\x01`/`CDF\x02`) with:
//! - dimensions `time` (rays, usually unlimited) and `range` (gates),
//! - per-ray `azimuth(time)`, `elevation(time)`, optional
//!   `nyquist_velocity(time)`,
//! - per-sweep `fixed_angle(sweep)`, `sweep_start_ray_index(sweep)`,
//!   `sweep_end_ray_index(sweep)`, `sweep_mode(sweep, string_length)`,
//! - scalar `latitude`/`longitude`/`altitude`, `time_coverage_start`,
//! - field variables dimensioned `(time, range)`, optionally packed with
//!   `scale_factor`/`add_offset` and flagged with `_FillValue`
//!   (CF packing: physical = raw * scale_factor + add_offset).
//!
//! CfRadial 2 is netCDF-4 (HDF5 container) and is rejected by the routing
//! layer with an explicit message — it never reaches this module.
//!
//! Fields decode into F32 moment grids (NaN = fill); sweeps become
//! elevation cuts. For RHI sweeps the fixed angle is the AZIMUTH and lands
//! in `ElevationCut::elevation_deg`, matching the DORADE decoder's RHI
//! convention; `sweep_mode` is surfaced as [`radar_core::ScanMode`].

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentRow, MomentType, RadarSite, RadarVolume, Radial,
    ScanMode,
};

use crate::dorade::canonical_moment;
pub use crate::netcdf3::looks_like_netcdf3_bytes;
use crate::netcdf3::{Nc3File, NcArray, NcVar};
use crate::{NexradError, Result};

/// Decode a CfRadial 1.x byte buffer into the shared radar model.
pub fn decode_cfradial1_volume(bytes: &[u8]) -> Result<RadarVolume> {
    let file = Nc3File::open(bytes)?;
    let dim = |name: &str| file.dims.iter().position(|(dim_name, _)| dim_name == name);
    let (Some(time_dim), Some(range_dim)) = (dim("time"), dim("range")) else {
        return Err(invalid(
            "netCDF file lacks time/range dimensions — not CfRadial 1.x",
        ));
    };
    let n_rays = file.dims[time_dim].1;
    let n_gates = file.dims[range_dim].1;
    if n_rays == 0 || n_gates == 0 {
        return Err(invalid("CfRadial volume has no rays or gates"));
    }

    let azimuth = read_f64s(&file, "azimuth")?;
    let elevation = read_f64s(&file, "elevation")?;
    if azimuth.len() < n_rays || elevation.len() < n_rays {
        return Err(invalid("azimuth/elevation shorter than the time dimension"));
    }
    let nyquist = read_f64s(&file, "nyquist_velocity").ok();

    // Gate geometry: range(range) gate centers in metres (spec §5.5); the
    // start_range/gate spacing attributes are optional, so derive from the
    // coordinate values themselves.
    let range = read_f64s(&file, "range")?;
    if range.len() < 2 {
        return Err(invalid("range coordinate needs at least two gates"));
    }
    let spacing = (range[1] - range[0]).round().max(1.0);
    // Center of first gate − half a gate = range to gate start.
    let first_gate = (range[0] - spacing / 2.0).round();
    let gate_range = GateRange {
        first_gate_m: first_gate as i32,
        gate_spacing_m: spacing as i32,
        gate_count: n_gates,
    };

    // Sweep index ranges; a missing sweep dimension means one sweep.
    let fixed_angles = read_f64s(&file, "fixed_angle").unwrap_or_default();
    let sweep_starts = read_f64s(&file, "sweep_start_ray_index").unwrap_or_default();
    let sweep_ends = read_f64s(&file, "sweep_end_ray_index").unwrap_or_default();
    let sweep_count = fixed_angles.len().max(1);
    let sweep_modes = read_sweep_modes(&file, sweep_count);

    let mut volume = RadarVolume {
        site: parse_site(&file),
        ..RadarVolume::default()
    };
    if let Some(start) = parse_time_coverage_start(&file) {
        volume.volume_time = start;
    }
    volume.metadata.archive_version = Some(
        file.gattr_str("version")
            .map(str::to_owned)
            .unwrap_or_else(|| "CfRadial-1".to_owned()),
    );
    volume.metadata.compression = Some("cfradial1-netcdf3".to_owned());
    volume.metadata.scan_mode = combined_scan_mode(&sweep_modes);

    // Ray times (seconds offset from time_coverage_start).
    let ray_seconds = read_f64s(&file, "time").ok();

    // Field variables: anything shaped (time, range).
    let fields: Vec<&NcVar> = file
        .vars
        .values()
        .filter(|var| var.dim_ids.as_slice() == [time_dim, range_dim])
        .collect();
    if fields.is_empty() {
        return Err(invalid("CfRadial volume has no (time, range) fields"));
    }

    for sweep in 0..sweep_count {
        let start_ray = sweep_starts.get(sweep).map(|v| *v as usize).unwrap_or(0);
        let end_ray = sweep_ends
            .get(sweep)
            .map(|v| (*v as usize).min(n_rays.saturating_sub(1)))
            .unwrap_or(n_rays.saturating_sub(1));
        if start_ray > end_ray || end_ray >= n_rays {
            volume.metadata.skipped_message_count += 1;
            continue;
        }
        let fixed = fixed_angles.get(sweep).copied().unwrap_or_else(|| {
            // No fixed_angle variable: mean elevation (or azimuth for RHI).
            let rays = &elevation[start_ray..=end_ray];
            rays.iter().sum::<f64>() / rays.len() as f64
        }) as f32;
        let mut cut = ElevationCut::new(fixed, Some(sweep.min(255) as u8));
        for ray in start_ray..=end_ray {
            let time_offset_ms = ray_seconds
                .as_ref()
                .and_then(|seconds| seconds.get(ray))
                .map(|seconds| (seconds * 1000.0) as i32)
                .unwrap_or(0);
            cut.radials.push(Radial {
                azimuth_deg: (azimuth[ray] as f32).rem_euclid(360.0),
                elevation_deg: elevation[ray] as f32,
                time_offset_ms,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: nyquist
                    .as_ref()
                    .and_then(|values| values.get(ray))
                    .map(|value| *value as f32)
                    .filter(|value| *value > 0.0),
                radial_status: None,
            });
        }

        for field in &fields {
            let moment = match canonical_moment(&field.name) {
                Some(moment) if !cut.moments.contains_key(&moment) => moment,
                _ => MomentType::Unknown(field.name.clone()),
            };
            let values = read_field_physical(&file, field)?;
            let mut grid = MomentGrid {
                moment: moment.clone(),
                gate_range: gate_range.clone(),
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: Vec::new(),
                storage: radar_core::MomentStorage::F32(Vec::new()),
            };
            for (radial_index, ray) in (start_ray..=end_ray).enumerate() {
                let row = &values[ray * n_gates..(ray + 1) * n_gates];
                grid.push_row(radial_index, MomentRow::F32(row.to_vec()))?;
            }
            cut.moments.insert(moment, grid);
        }
        volume.cuts.push(cut);
    }
    if volume.cuts.is_empty() {
        return Err(invalid("CfRadial volume decoded no sweeps"));
    }
    volume
        .cuts
        .sort_by(|left, right| left.elevation_deg.total_cmp(&right.elevation_deg));
    volume.metadata.decoded_radial_count = volume.cuts.iter().map(|cut| cut.radials.len()).sum();
    volume.metadata.message_count = sweep_count;
    Ok(volume)
}

/// Apply CF packing (physical = raw·scale_factor + add_offset) and
/// `_FillValue`/`missing_value` masking; everything lands in f32.
fn read_field_physical(file: &Nc3File<'_>, var: &NcVar) -> Result<Vec<f32>> {
    let scale = var.attr_f64("scale_factor").unwrap_or(1.0);
    let offset = var.attr_f64("add_offset").unwrap_or(0.0);
    let fill = var
        .attr_f64("_FillValue")
        .or_else(|| var.attr_f64("missing_value"));
    let raw = file.read_var(&var.name)?;
    let count = raw.len();
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let value = raw.get_f64(index);
        match value {
            Some(value) if Some(value) != fill && value.is_finite() => {
                out.push((value * scale + offset) as f32);
            }
            _ => out.push(f32::NAN),
        }
    }
    Ok(out)
}

fn read_f64s(file: &Nc3File<'_>, name: &str) -> Result<Vec<f64>> {
    let raw = file.read_var(name)?;
    let count = raw.len();
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        out.push(
            raw.get_f64(index)
                .ok_or_else(|| invalid(format!("variable '{name}' is not numeric")))?,
        );
    }
    Ok(out)
}

/// `sweep_mode(sweep, string_length)` char matrix → per-sweep scan modes.
fn read_sweep_modes(file: &Nc3File<'_>, sweep_count: usize) -> Vec<Option<ScanMode>> {
    let Some(var) = file.vars.get("sweep_mode") else {
        return vec![None; sweep_count];
    };
    let dims = file.var_dims(var);
    let (rows, width) = match dims.as_slice() {
        [rows, width] => (*rows, *width),
        _ => return vec![None; sweep_count],
    };
    let Ok(NcArray::Char(chars)) = file.read_var("sweep_mode") else {
        return vec![None; sweep_count];
    };
    (0..sweep_count)
        .map(|sweep| {
            if sweep >= rows {
                return None;
            }
            let raw = &chars[sweep * width..(sweep + 1) * width];
            let text = raw.split(|byte| *byte == 0).next().unwrap_or_default();
            Some(scan_mode_from_str(String::from_utf8_lossy(text).trim()))
        })
        .collect()
}

/// CfRadial 1.4 §5.8 sweep_mode vocabulary.
fn scan_mode_from_str(mode: &str) -> ScanMode {
    match mode {
        "azimuth_surveillance" | "sector" | "manual_ppi" => ScanMode::Ppi,
        "rhi" | "manual_rhi" => ScanMode::Rhi,
        "vertical_pointing" => ScanMode::VerticalPointing,
        _ => ScanMode::Other,
    }
}

/// One volume-level mode when every sweep agrees; mixed scans report Other.
fn combined_scan_mode(modes: &[Option<ScanMode>]) -> Option<ScanMode> {
    let mut all = modes.iter().flatten();
    let first = *all.next()?;
    if all.all(|mode| *mode == first) {
        Some(first)
    } else {
        Some(ScanMode::Other)
    }
}

fn parse_site(file: &Nc3File<'_>) -> RadarSite {
    let scalar = |name: &str| -> Option<f32> {
        file.read_var(name)
            .ok()
            .and_then(|array| array.get_f64(0))
            .map(|value| value as f32)
    };
    let id = file
        .gattr_str("instrument_name")
        .or_else(|| file.gattr_str("site_name"))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("CFRAD")
        .to_owned();
    RadarSite {
        id,
        name: file.gattr_str("site_name").map(str::to_owned),
        latitude_deg: scalar("latitude"),
        longitude_deg: scalar("longitude"),
        elevation_m: scalar("altitude"),
    }
}

fn parse_time_coverage_start(file: &Nc3File<'_>) -> Option<DateTime<Utc>> {
    // Either a char variable or a global attribute, ISO8601 "...Z".
    let text = match file.read_var("time_coverage_start") {
        Ok(NcArray::Char(chars)) => {
            let bytes: Vec<u8> = chars.into_iter().take_while(|byte| *byte != 0).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => file.gattr_str("time_coverage_start")?.to_owned(),
    };
    let trimmed = text.trim().trim_end_matches('Z');
    let naive = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S"))
        .ok()?;
    Some(Utc.from_utc_datetime(&naive))
}

fn invalid(reason: impl Into<String>) -> NexradError {
    NexradError::InvalidMessage {
        offset: 0,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_mode_vocabulary_maps_to_scan_modes() {
        assert_eq!(scan_mode_from_str("azimuth_surveillance"), ScanMode::Ppi);
        assert_eq!(scan_mode_from_str("sector"), ScanMode::Ppi);
        assert_eq!(scan_mode_from_str("rhi"), ScanMode::Rhi);
        assert_eq!(scan_mode_from_str("manual_rhi"), ScanMode::Rhi);
        assert_eq!(
            scan_mode_from_str("vertical_pointing"),
            ScanMode::VerticalPointing
        );
        assert_eq!(scan_mode_from_str("coplane"), ScanMode::Other);
    }

    #[test]
    fn mixed_sweep_modes_collapse_to_other() {
        assert_eq!(
            combined_scan_mode(&[Some(ScanMode::Ppi), Some(ScanMode::Ppi)]),
            Some(ScanMode::Ppi)
        );
        assert_eq!(
            combined_scan_mode(&[Some(ScanMode::Ppi), Some(ScanMode::Rhi)]),
            Some(ScanMode::Other)
        );
        assert_eq!(combined_scan_mode(&[None, None]), None);
        assert_eq!(
            combined_scan_mode(&[None, Some(ScanMode::Rhi)]),
            Some(ScanMode::Rhi)
        );
    }
}
