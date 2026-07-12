//! CfRadial 1.x decoder (classic-netCDF radar moments).
//!
//! Format reference: M. Dixon and W.-C. Lee, "CfRadial Data File Format —
//! CF-compliant netCDF Format for Moments Data for RADAR and LIDAR",
//! NCAR/EOL, version 1.4 (2016) (versions 1.1–1.4 share the layout read
//! here). CfRadial 1 files are classic netCDF (`CDF\x01`/`CDF\x02`) with:
//! - dimensions `time` (rays, usually unlimited) and `range` (gates),
//! - per-ray `azimuth(time)`, `elevation(time)`, optional
//!   `nyquist_velocity(time)`, `prt(time)`, `unambiguous_range(time)`,
//!   `pulse_count(time)`, and `independent_samples(time)`,
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

use std::collections::BTreeSet;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentRow, MomentType, RadarSite, RadarVolume, Radial,
    RayInstrumentMetadata, ScanLegMetadata, ScanMode, VcpInfo,
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
    // Keep physical timing/sample quantities aligned to their source rays.
    // VCP Appendix-C PRF values elsewhere in the file are source-table CODES,
    // not frequencies; they remain in ScanLegMetadata and never feed these
    // physical variables.
    let ray_prt_s = aligned_time_f32s(&file, "prt", time_dim, n_rays, time_units_scale);
    let ray_unambiguous_range_km = aligned_time_f32s(
        &file,
        "unambiguous_range",
        time_dim,
        n_rays,
        range_units_to_km_scale,
    );
    let ray_pulse_count = aligned_time_u32s(&file, "pulse_count", time_dim, n_rays);
    let ray_independent_samples =
        aligned_time_f32s(&file, "independent_samples", time_dim, n_rays, |_| 1.0);
    let has_ray_instrument_metadata = ray_prt_s.is_some()
        || ray_unambiguous_range_km.is_some()
        || ray_pulse_count.is_some()
        || ray_independent_samples.is_some();

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
    volume.metadata.radar_frequency_mhz = cfradial_radar_frequency_mhz(&file);
    volume.metadata.beam_width_h_deg =
        cfradial_beam_width_deg(&file, &["radar_beam_width_h", "radar_beam_width_h_deg"]);
    volume.metadata.beam_width_v_deg =
        cfradial_beam_width_deg(&file, &["radar_beam_width_v", "radar_beam_width_v_deg"]);
    volume.metadata.pulse_width_us = cfradial_pulse_width_us(&file);
    volume.metadata.prt_s = cfradial_prt_s(&file);
    volume.metadata.unambiguous_range_km = cfradial_unambiguous_range_km(&file);
    volume.metadata.scan_name = metadata_text(&file, "scan_name");
    volume.metadata.scan_id = metadata_text(&file, "scan_id").or_else(|| {
        file.gattr_f64("scan_id")
            .filter(|value| value.is_finite())
            .map(|value| {
                if value.fract() == 0.0 {
                    format!("{value:.0}")
                } else {
                    value.to_string()
                }
            })
    });
    volume.vcp = file
        .gattr_f64("vcp_pattern")
        .filter(|value| value.is_finite() && value.fract() == 0.0)
        .and_then(|value| u16::try_from(value as i64).ok())
        .filter(|pattern| *pattern > 0)
        .map(|pattern| VcpInfo { pattern });
    volume.metadata.vcp_source_document = metadata_text(&file, "vcp_source_document");
    volume.metadata.vcp_source_revision = metadata_text(&file, "vcp_source_revision");
    volume.metadata.vcp_source_rda_build = metadata_text(&file, "vcp_source_rda_build");
    volume.metadata.vcp_source_figure = metadata_text(&file, "vcp_source_figure");
    volume.metadata.vcp_pulse_length = metadata_text(&file, "vcp_pulse_length");
    volume.metadata.vcp_adaptations = metadata_text(&file, "vcp_adaptations");
    volume.metadata.polarization = metadata_text(&file, "polarization");
    volume.metadata.calibration = metadata_text(&file, "calibration");
    volume.metadata.forward_operator = metadata_text(&file, "forward_operator");
    volume.metadata.forward_operator_config = metadata_text(&file, "forward_operator_config");
    volume.metadata.source_model = metadata_text(&file, "source_model");
    volume.metadata.microphysics_scheme = metadata_text(&file, "microphysics_scheme");
    volume.metadata.scattering_model = metadata_text(&file, "scattering_model");

    // Ray times (seconds offset from time_coverage_start).
    let ray_seconds = read_f64s(&file, "time").ok();
    let source_row_indices = read_f64s(&file, "vcp_source_row_index").ok();
    let vcp_azimuth_rates = read_f64s(&file, "vcp_azimuth_rate").ok();
    let vcp_source_periods = read_f64s(&file, "vcp_source_period").ok();
    let vcp_waveform_codes = read_f64s(&file, "vcp_waveform_code").ok();
    let vcp_moment_coverage_codes = read_f64s(&file, "vcp_moment_coverage_code").ok();
    let surveillance_prf_codes = read_f64s(&file, "vcp_surveillance_prf_code").ok();
    let surveillance_pulse_counts = read_f64s(&file, "vcp_surveillance_pulse_count").ok();
    let doppler_prf_codes = read_f64s(&file, "vcp_doppler_prf_code").ok();
    let doppler_pulse_counts = read_f64s(&file, "vcp_doppler_pulse_count").ok();
    let has_scan_leg_metadata = source_row_indices.is_some()
        || vcp_azimuth_rates.is_some()
        || vcp_source_periods.is_some()
        || vcp_waveform_codes.is_some()
        || vcp_moment_coverage_codes.is_some()
        || surveillance_prf_codes.is_some()
        || surveillance_pulse_counts.is_some()
        || doppler_prf_codes.is_some()
        || doppler_pulse_counts.is_some();

    // Field variables: anything shaped (time, range).
    let fields: Vec<&NcVar> = file
        .vars
        .values()
        .filter(|var| var.dim_ids.as_slice() == [time_dim, range_dim])
        .collect();
    if fields.is_empty() {
        return Err(invalid("CfRadial volume has no (time, range) fields"));
    }

    // Build sweep geometry first, then read each full (time, range) field
    // once and distribute its rows across every sweep. The former
    // sweep-outer loop reread and reconverted each full field once per sweep.
    let mut sweeps = Vec::with_capacity(sweep_count);
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
            fallback_fixed_angle(
                sweep_modes.get(sweep).copied().flatten(),
                &azimuth[start_ray..=end_ray],
                &elevation[start_ray..=end_ray],
            )
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
            if has_ray_instrument_metadata {
                cut.ray_instrument_metadata.push(RayInstrumentMetadata {
                    prt_s: aligned_at(&ray_prt_s, ray),
                    unambiguous_range_km: aligned_at(&ray_unambiguous_range_km, ray),
                    pulse_count: aligned_at(&ray_pulse_count, ray),
                    independent_samples: aligned_at(&ray_independent_samples, ray),
                });
            }
        }

        sweeps.push(DecodedSweep {
            start_ray,
            end_ray,
            cut,
            scan_leg: ScanLegMetadata {
                source_row_index: numeric_u16_at(&source_row_indices, sweep),
                elevation_deg: has_scan_leg_metadata.then_some(fixed),
                azimuth_rate_deg_per_second: numeric_f32_at(&vcp_azimuth_rates, sweep),
                source_period_seconds: numeric_f32_at(&vcp_source_periods, sweep),
                waveform: numeric_u8_at(&vcp_waveform_codes, sweep)
                    .and_then(waveform_from_code)
                    .map(str::to_owned),
                moment_coverage: numeric_u8_at(&vcp_moment_coverage_codes, sweep)
                    .and_then(moment_coverage_from_code)
                    .map(str::to_owned),
                surveillance_prf_code: numeric_u8_at(&surveillance_prf_codes, sweep),
                surveillance_pulse_count: numeric_u16_at(&surveillance_pulse_counts, sweep),
                doppler_prf_code: numeric_u8_at(&doppler_prf_codes, sweep),
                doppler_pulse_count: numeric_u16_at(&doppler_pulse_counts, sweep),
            },
        });
    }
    if sweeps.is_empty() {
        return Err(invalid("CfRadial volume decoded no sweeps"));
    }

    let expected_values = n_rays
        .checked_mul(n_gates)
        .ok_or_else(|| invalid("CfRadial field dimensions overflow addressable memory"))?;
    let mut canonical_fields = BTreeSet::new();
    for field in fields {
        let moment = match canonical_moment(&field.name) {
            Some(moment) if canonical_fields.insert(moment.clone()) => moment,
            _ => MomentType::Unknown(field.name.clone()),
        };
        let values = read_field_physical(&file, field)?;
        if values.len() < expected_values {
            return Err(invalid(format!(
                "CfRadial field '{}' has {} values; expected at least {expected_values}",
                field.name,
                values.len()
            )));
        }
        for sweep in &mut sweeps {
            if !scan_leg_allows_moment(&sweep.scan_leg, &moment) {
                continue;
            }
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
            for (radial_index, ray) in (sweep.start_ray..=sweep.end_ray).enumerate() {
                let row_start = ray * n_gates;
                let row = &values[row_start..row_start + n_gates];
                grid.push_row(radial_index, MomentRow::F32(row.to_vec()))?;
            }
            sweep.cut.moments.insert(moment.clone(), grid);
        }
    }
    sweeps.sort_by(|left, right| left.cut.elevation_deg.total_cmp(&right.cut.elevation_deg));
    if sweeps
        .iter()
        .any(|sweep| sweep.scan_leg != ScanLegMetadata::default())
    {
        volume.metadata.scan_legs = sweeps.iter().map(|sweep| sweep.scan_leg.clone()).collect();
    }
    volume.cuts = sweeps.into_iter().map(|sweep| sweep.cut).collect();
    volume.metadata.decoded_radial_count = volume.cuts.iter().map(|cut| cut.radials.len()).sum();
    volume.metadata.message_count = sweep_count;
    Ok(volume)
}

struct DecodedSweep {
    start_ray: usize,
    end_ray: usize,
    cut: ElevationCut,
    scan_leg: ScanLegMetadata,
}

fn numeric_at(values: &Option<Vec<f64>>, index: usize) -> Option<f64> {
    values
        .as_ref()?
        .get(index)
        .copied()
        .filter(|value| value.is_finite() && *value != -9999.0)
}

fn numeric_f32_at(values: &Option<Vec<f64>>, index: usize) -> Option<f32> {
    numeric_at(values, index)
        .filter(|value| value.abs() <= f32::MAX as f64)
        .map(|value| value as f32)
}

fn numeric_u8_at(values: &Option<Vec<f64>>, index: usize) -> Option<u8> {
    let value = numeric_at(values, index)?;
    (value.fract() == 0.0)
        .then(|| u8::try_from(value as i64).ok())
        .flatten()
}

fn numeric_u16_at(values: &Option<Vec<f64>>, index: usize) -> Option<u16> {
    let value = numeric_at(values, index)?;
    (value.fract() == 0.0)
        .then(|| u16::try_from(value as i64).ok())
        .flatten()
}

fn waveform_from_code(code: u8) -> Option<&'static str> {
    Some(match code {
        1 => "CS",
        2 => "CD/W",
        3 => "B",
        4 => "CD/WO",
        5 => "SZCS",
        6 => "SZCD",
        _ => return None,
    })
}

fn moment_coverage_from_code(code: u8) -> Option<&'static str> {
    Some(match code {
        1 => "surveillance",
        2 => "doppler",
        3 => "all",
        _ => return None,
    })
}

fn scan_leg_allows_moment(scan_leg: &ScanLegMetadata, moment: &MomentType) -> bool {
    match scan_leg.moment_coverage.as_deref() {
        Some("surveillance") => !matches!(moment, MomentType::Velocity | MomentType::SpectrumWidth),
        Some("doppler") => matches!(moment, MomentType::Velocity | MomentType::SpectrumWidth),
        _ => true,
    }
}

/// CfRadial's fixed angle is elevation for PPI sweeps and azimuth for RHI
/// sweeps. Azimuth needs a circular mean so a 359-degree/1-degree RHI points
/// north, rather than being mislabeled as 180 degrees when `fixed_angle` is
/// absent.
fn fallback_fixed_angle(mode: Option<ScanMode>, azimuth: &[f64], elevation: &[f64]) -> f64 {
    if mode == Some(ScanMode::Rhi) {
        circular_mean_degrees(azimuth)
            .or_else(|| azimuth.iter().copied().find(|value| value.is_finite()))
            .map(|value| value.rem_euclid(360.0))
            .unwrap_or(0.0)
    } else {
        arithmetic_mean(elevation).unwrap_or(0.0)
    }
}

fn circular_mean_degrees(values: &[f64]) -> Option<f64> {
    let mut sin_sum = 0.0;
    let mut cos_sum = 0.0;
    let mut count = 0usize;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        let radians = value.to_radians();
        sin_sum += radians.sin();
        cos_sum += radians.cos();
        count += 1;
    }
    if count == 0 || sin_sum.hypot(cos_sum) <= f64::EPSILON {
        return None;
    }
    Some(sin_sum.atan2(cos_sum).to_degrees().rem_euclid(360.0))
}

fn arithmetic_mean(values: &[f64]) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0usize;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        sum += value;
        count += 1;
    }
    if count == 0 {
        None
    } else {
        Some(sum / count as f64)
    }
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

fn cfradial_radar_frequency_mhz(file: &Nc3File<'_>) -> Option<u32> {
    // CfRadial's instrument-parameter coordinate is a numeric
    // `frequency(frequency)` variable in Hz. Prefer it over the historical
    // global-attribute spellings below so a standards-compliant file wins
    // even when a stale compatibility attribute is also present.
    if let Some(value) = numeric_var_first(file, "frequency")
        && let Some(mhz) = normalize_frequency_mhz(value)
    {
        return Some(mhz);
    }
    for name in [
        "radar_frequency",
        "frequency",
        "frequency_ghz",
        "instrument_frequency",
    ] {
        if let Some(value) = file.gattr_f64(name)
            && let Some(mhz) = normalize_frequency_mhz(value)
        {
            return Some(mhz);
        }
    }
    for name in ["radar_wavelength", "radar_wavelength_cm", "wavelength"] {
        if let Some(value) = file.gattr_f64(name)
            && let Some(mhz) = frequency_mhz_from_wavelength(value)
        {
            return Some(mhz);
        }
    }
    None
}

fn cfradial_beam_width_deg(file: &Nc3File<'_>, names: &[&str]) -> Option<f32> {
    for name in names {
        if let Some(value) = numeric_var_first(file, name).or_else(|| file.gattr_f64(name)) {
            let units = file
                .vars
                .get(*name)
                .and_then(|var| var.attr_str("units"))
                .unwrap_or("degrees")
                .to_ascii_lowercase();
            let degrees = if units.contains("rad") {
                value.to_degrees()
            } else {
                value
            };
            if degrees.is_finite() && degrees > 0.0 && degrees <= 180.0 {
                return Some(degrees as f32);
            }
        }
    }
    None
}

fn cfradial_pulse_width_us(file: &Nc3File<'_>) -> Option<f32> {
    if let Some(seconds) = time_var_seconds(file, "pulse_width") {
        return positive_f32(seconds * 1.0e6);
    }
    file.gattr_f64("pulse_width_us")
        .and_then(positive_f32)
        .or_else(|| {
            file.gattr_f64("pulse_width")
                .and_then(|seconds| positive_f32(seconds * 1.0e6))
        })
}

fn cfradial_prt_s(file: &Nc3File<'_>) -> Option<f32> {
    // A time-aligned `prt(time)` belongs to each ray, not to the volume.
    // Only a true scalar variable participates in this legacy volume-level
    // fallback.
    if let Some(value) = numeric_scalar_var_first(file, "prt") {
        let scale = time_units_scale(file.vars.get("prt").and_then(|var| var.attr_str("units")));
        return positive_f32(value * scale);
    }
    file.gattr_f64("prt_s")
        .or_else(|| file.gattr_f64("prt"))
        .and_then(positive_f32)
}

fn cfradial_unambiguous_range_km(file: &Nc3File<'_>) -> Option<f32> {
    // As with PRT, do not collapse varying per-ray values to the first ray.
    if let Some(value) = numeric_scalar_var_first(file, "unambiguous_range") {
        let scale = range_units_to_km_scale(
            file.vars
                .get("unambiguous_range")
                .and_then(|var| var.attr_str("units")),
        );
        return positive_f32(value * scale);
    }
    file.gattr_f64("unambiguous_range_km")
        .and_then(positive_f32)
        .or_else(|| {
            file.gattr_f64("unambiguous_range")
                .and_then(|meters| positive_f32(meters / 1000.0))
        })
}

fn numeric_var_first(file: &Nc3File<'_>, name: &str) -> Option<f64> {
    let values = file.read_var(name).ok()?;
    (0..values.len()).find_map(|index| {
        values
            .get_f64(index)
            .filter(|value| value.is_finite() && *value > 0.0)
    })
}

fn numeric_scalar_var_first(file: &Nc3File<'_>, name: &str) -> Option<f64> {
    file.vars.get(name)?.dim_ids.is_empty().then_some(())?;
    numeric_var_first(file, name)
}

fn time_var_seconds(file: &Nc3File<'_>, name: &str) -> Option<f64> {
    let value = numeric_var_first(file, name)?;
    let units = file
        .vars
        .get(name)
        .and_then(|var| var.attr_str("units"))
        .unwrap_or("seconds")
        .trim()
        .to_ascii_lowercase();
    let seconds = if units.contains("microsecond") || matches!(units.as_str(), "us" | "µs") {
        value * 1.0e-6
    } else if units.contains("millisecond") || units == "ms" {
        value * 1.0e-3
    } else {
        value
    };
    seconds.is_finite().then_some(seconds)
}

/// Read a numeric variable only when it is exactly aligned to the CfRadial
/// `time` dimension. A scalar, sweep-level value, or malformed short array is
/// not silently broadcast across rays.
fn aligned_time_values(
    file: &Nc3File<'_>,
    name: &str,
    time_dim: usize,
    n_rays: usize,
) -> Option<Vec<f64>> {
    let var = file.vars.get(name)?;
    (var.dim_ids.as_slice() == [time_dim]).then_some(())?;
    let values = read_f64s(file, name).ok()?;
    (values.len() == n_rays).then_some(values)
}

fn aligned_time_f32s(
    file: &Nc3File<'_>,
    name: &str,
    time_dim: usize,
    n_rays: usize,
    units_scale: impl FnOnce(Option<&str>) -> f64,
) -> Option<Vec<Option<f32>>> {
    let scale = units_scale(file.vars.get(name).and_then(|var| var.attr_str("units")));
    aligned_time_values(file, name, time_dim, n_rays).map(|values| {
        values
            .into_iter()
            .map(|value| positive_f32(value * scale))
            .collect()
    })
}

fn aligned_time_u32s(
    file: &Nc3File<'_>,
    name: &str,
    time_dim: usize,
    n_rays: usize,
) -> Option<Vec<Option<u32>>> {
    aligned_time_values(file, name, time_dim, n_rays).map(|values| {
        values
            .into_iter()
            .map(|value| {
                (value.is_finite()
                    && value > 0.0
                    && value.fract() == 0.0
                    && value <= u32::MAX as f64)
                    .then_some(value as u32)
            })
            .collect()
    })
}

fn aligned_at<T: Copy>(values: &Option<Vec<Option<T>>>, ray: usize) -> Option<T> {
    values
        .as_ref()
        .and_then(|values| values.get(ray))
        .copied()
        .flatten()
}

fn time_units_scale(units: Option<&str>) -> f64 {
    let units = units.unwrap_or("seconds").trim().to_ascii_lowercase();
    if units.contains("microsecond") || matches!(units.as_str(), "us" | "µs") {
        1.0e-6
    } else if units.contains("millisecond") || units == "ms" {
        1.0e-3
    } else {
        1.0
    }
}

fn range_units_to_km_scale(units: Option<&str>) -> f64 {
    let units = units.unwrap_or("meters").trim().to_ascii_lowercase();
    if units.contains("kilometer") || units.contains("kilometre") || units == "km" {
        1.0
    } else {
        1.0e-3
    }
}

fn positive_f32(value: f64) -> Option<f32> {
    (value.is_finite() && value > 0.0 && value <= f32::MAX as f64).then_some(value as f32)
}

fn metadata_text(file: &Nc3File<'_>, name: &str) -> Option<String> {
    file.gattr_str(name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn normalize_frequency_mhz(value: f64) -> Option<u32> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let mhz = if value > 1.0e6 {
        value / 1.0e6
    } else if value > 1000.0 {
        value
    } else {
        value * 1000.0
    };
    (1000.0..=12_000.0)
        .contains(&mhz)
        .then_some(mhz.round() as u32)
}

fn frequency_mhz_from_wavelength(value: f64) -> Option<u32> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let meters = if value > 1.0 { value / 100.0 } else { value };
    let mhz = 299.792_458 / meters;
    (1000.0..=12_000.0)
        .contains(&mhz)
        .then_some(mhz.round() as u32)
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

    #[test]
    fn rhi_fixed_angle_fallback_uses_wrap_aware_azimuth_mean() {
        let fixed =
            fallback_fixed_angle(Some(ScanMode::Rhi), &[359.0, 0.0, 1.0], &[10.0, 20.0, 30.0]);
        assert!(!(0.01..=359.99).contains(&fixed), "fixed angle was {fixed}");
    }

    #[test]
    fn ppi_fixed_angle_fallback_still_uses_mean_elevation() {
        let fixed =
            fallback_fixed_angle(Some(ScanMode::Ppi), &[80.0, 90.0, 100.0], &[0.4, 0.5, 0.6]);
        assert!((fixed - 0.5).abs() < 1.0e-9);
    }
}
