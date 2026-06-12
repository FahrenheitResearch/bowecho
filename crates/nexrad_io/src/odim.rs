//! ODIM_H5 polar volume/scan decoder (the European/research HDF5 standard).
//!
//! Information model: D. B. Michelson, R. Lewandowski, M. Szewczykowski,
//! H. Beekhuis, and G. Haase, "EUMETNET OPERA weather radar information
//! model for implementation with the HDF5 file format" (ODIM_H5), EUMETNET
//! OPERA Working Document WD_2008_03 (v2.2, 2014; v2.4, 2021). Layout:
//! `/what` (object, date, time, source), `/where` (lat, lon, height),
//! `/datasetN` per sweep with `where` (elangle, nbins, nrays, rstart,
//! rscale) and `dataM` per quantity with `what` (quantity, gain, offset,
//! nodata, undetect) and the `data` plane (nrays x nbins).
//!
//! Decodes `PVOL` (polar volume) and `SCAN` (single sweep) objects into
//! [`RadarVolume`]. Conventions honored from the spec:
//! - physical = `gain * raw + offset` (Table 6 "what" group attributes) —
//!   inverted into the moment grid's `(raw - offset) / scale` storage.
//! - `nodata` and `undetect` both display as "no echo"; `undetect` cells
//!   are remapped onto the `nodata` sentinel so compact storage keeps one
//!   transparent code.
//! - `rstart` is km to the start of the first bin; `rscale` is the bin
//!   spacing in metres (Table 5 "where" for polar data).
//! - Rays are stored north-relative in scan order: ray `i` spans
//!   `[i, i+1) * 360/nrays` degrees, so its center azimuth is
//!   `(i + 0.5) * 360 / nrays` (the `a1gate` index only records where the
//!   antenna started radiating in time, not a storage rotation).
//!
//! Known limitations (explicit, not silent): non-polar objects (ELEV/RHI
//! cross-section products, CVOL, IMAGE) are rejected with a clear error;
//! 8/16-bit unsigned and float data planes are supported (the only types
//! OPERA members emit).

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use radar_core::{
    GateRange, MomentGrid, MomentRow, MomentType, RadarSite, RadarVolume, Radial, ScanMode,
};

pub use crate::hdf5lite::looks_like_hdf5_bytes;
use crate::hdf5lite::{H5Attr, H5Data, H5File};
use crate::{NexradError, Result};

/// Decode an ODIM_H5 PVOL/SCAN byte buffer into the shared radar model.
pub fn decode_odim_h5_volume(bytes: &[u8]) -> Result<RadarVolume> {
    let file = H5File::open(bytes)?;
    let object = file
        .attr("/what", "object")
        .and_then(|attr| attr.as_str().map(str::to_owned))
        .ok_or_else(|| {
            invalid(
                "HDF5 file has no /what 'object' attribute — not ODIM_H5 \
                 (CfRadial2/other HDF5 radar formats are not supported yet)",
            )
        })?;
    if object != "PVOL" && object != "SCAN" {
        return Err(invalid(format!(
            "ODIM_H5 object '{object}' unsupported (PVOL and SCAN only)"
        )));
    }

    let mut volume = RadarVolume {
        site: parse_site(&file),
        ..RadarVolume::default()
    };
    if let Some(time) = parse_datetime(&file, "/what") {
        volume.volume_time = time;
    }
    volume.metadata.archive_version = file
        .attr("/what", "version")
        .and_then(|attr| attr.as_str().map(str::to_owned))
        .or(Some("ODIM_H5".to_owned()));
    volume.metadata.compression = Some("odim-h5".to_owned());
    volume.metadata.scan_mode = Some(ScanMode::Ppi);
    let root_nyquist = attr_f64(&file, "/how", "NI");

    let mut dataset_names: Vec<String> = file
        .child_names("/")
        .into_iter()
        .filter(|name| {
            name.strip_prefix("dataset")
                .is_some_and(|rest| rest.parse::<u32>().is_ok())
        })
        .collect();
    dataset_names.sort_by_key(|name| name[7..].parse::<u32>().unwrap_or(u32::MAX));
    if dataset_names.is_empty() {
        return Err(invalid("ODIM_H5 volume has no /datasetN groups"));
    }

    for name in &dataset_names {
        decode_sweep(&file, name, root_nyquist, &mut volume)?;
    }
    volume
        .cuts
        .sort_by(|left, right| left.elevation_deg.total_cmp(&right.elevation_deg));
    volume.metadata.decoded_radial_count = volume.cuts.iter().map(|cut| cut.radials.len()).sum();
    volume.metadata.message_count = dataset_names.len();
    Ok(volume)
}

fn decode_sweep(
    file: &H5File<'_>,
    dataset: &str,
    root_nyquist: Option<f64>,
    volume: &mut RadarVolume,
) -> Result<()> {
    let where_path = format!("/{dataset}/where");
    let elangle = attr_f64(file, &where_path, "elangle")
        .ok_or_else(|| invalid(format!("{dataset} has no where/elangle")))?
        as f32;
    let rstart_km = attr_f64(file, &where_path, "rstart").unwrap_or(0.0);
    let rscale_m = attr_f64(file, &where_path, "rscale").unwrap_or(0.0);
    let nyquist = attr_f64(file, &format!("/{dataset}/how"), "NI")
        .or(root_nyquist)
        .map(|value| value as f32)
        .filter(|value| *value > 0.0);

    let mut data_names: Vec<String> = file
        .child_names(&format!("/{dataset}"))
        .into_iter()
        .filter(|name| {
            name.strip_prefix("data")
                .is_some_and(|rest| rest.parse::<u32>().is_ok())
        })
        .collect();
    data_names.sort_by_key(|name| name[4..].parse::<u32>().unwrap_or(u32::MAX));
    if data_names.is_empty() {
        return Err(invalid(format!("{dataset} has no dataM planes")));
    }

    // All planes in a sweep share ray geometry; read the first to size it.
    let first_plane = file.dataset(&format!("/{dataset}/{}/data", data_names[0]))?;
    let (nrays, nbins) = match first_plane.dims.as_slice() {
        [rays, bins] => (*rays, *bins),
        other => {
            return Err(invalid(format!(
                "{dataset} data has rank {} (need 2)",
                other.len()
            )));
        }
    };
    if nrays == 0 || nbins == 0 {
        return Err(invalid(format!("{dataset} data plane is empty")));
    }
    let gate_range = GateRange {
        first_gate_m: (rstart_km * 1000.0).round() as i32,
        gate_spacing_m: (rscale_m.round() as i32).max(1),
        gate_count: nbins,
    };

    let mut skipped_planes = 0usize;
    let mut cut = radar_core::ElevationCut::new(elangle, None);
    for ray in 0..nrays {
        cut.radials.push(Radial {
            azimuth_deg: ((ray as f32 + 0.5) * 360.0 / nrays as f32).rem_euclid(360.0),
            elevation_deg: elangle,
            time_offset_ms: 0,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: nyquist,
            radial_status: None,
        });
    }

    for plane_name in &data_names {
        let what_path = format!("/{dataset}/{plane_name}/what");
        let quantity = file
            .attr(&what_path, "quantity")
            .and_then(|attr| attr.as_str().map(str::to_owned))
            .unwrap_or_else(|| plane_name.to_uppercase());
        let gain = attr_f64(file, &what_path, "gain").unwrap_or(1.0);
        let gain = if gain.abs() > 1.0e-9 { gain } else { 1.0 };
        let offset = attr_f64(file, &what_path, "offset").unwrap_or(0.0);
        let nodata = attr_f64(file, &what_path, "nodata");
        let undetect = attr_f64(file, &what_path, "undetect");

        let plane = if plane_name == &data_names[0] {
            first_plane.clone()
        } else {
            file.dataset(&format!("/{dataset}/{plane_name}/data"))?
        };
        if plane.dims.as_slice() != [nrays, nbins] {
            skipped_planes += 1;
            continue;
        }

        let moment = match canonical_quantity(&quantity) {
            Some(moment) if !cut.moments.contains_key(&moment) => moment,
            _ => MomentType::Unknown(quantity.clone()),
        };
        // ODIM physical = gain·raw + offset ⇔ grid (raw − o)/s with
        // s = 1/gain, o = −offset/gain.
        let scale = (1.0 / gain) as f32;
        let grid_offset = (-offset / gain) as f32;
        let mut grid = match &plane.data {
            H5Data::U8(values) => {
                let nodata_raw = nodata.map(|value| value as u8);
                let undetect_raw = undetect.map(|value| value as u8);
                let sentinel = nodata_raw.or(undetect_raw);
                let mut grid = MomentGrid::new_u8(
                    moment.clone(),
                    gate_range.clone(),
                    scale,
                    grid_offset,
                    sentinel,
                    None,
                );
                for (ray, row) in values.chunks_exact(nbins).enumerate() {
                    let row: Vec<u8> = row
                        .iter()
                        .map(|raw| remap_sentinel(*raw, undetect_raw, sentinel))
                        .collect();
                    grid.push_row(ray, MomentRow::U8(row))?;
                }
                grid
            }
            H5Data::U16(values) => {
                let nodata_raw = nodata.map(|value| value as u16);
                let undetect_raw = undetect.map(|value| value as u16);
                let sentinel = nodata_raw.or(undetect_raw);
                let mut grid = MomentGrid::new_u16(
                    moment.clone(),
                    gate_range.clone(),
                    scale,
                    grid_offset,
                    sentinel,
                    None,
                );
                for (ray, row) in values.chunks_exact(nbins).enumerate() {
                    let row: Vec<u16> = row
                        .iter()
                        .map(|raw| remap_sentinel(*raw, undetect_raw, sentinel))
                        .collect();
                    grid.push_row(ray, MomentRow::U16(row))?;
                }
                grid
            }
            H5Data::F32(_) | H5Data::F64(_) => {
                let physical = |raw: f64| -> f32 {
                    if Some(raw) == nodata || Some(raw) == undetect {
                        f32::NAN
                    } else {
                        (gain * raw + offset) as f32
                    }
                };
                let values: Vec<f32> = match &plane.data {
                    H5Data::F32(values) => {
                        values.iter().map(|raw| physical(f64::from(*raw))).collect()
                    }
                    H5Data::F64(values) => values.iter().map(|raw| physical(*raw)).collect(),
                    _ => unreachable!("outer match covers integer planes"),
                };
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
                for (ray, row) in values.chunks_exact(nbins).enumerate() {
                    grid.push_row(ray, MomentRow::F32(row.to_vec()))?;
                }
                grid
            }
        };
        grid.moment = moment.clone();
        cut.moments.insert(moment, grid);
    }
    volume.cuts.push(cut);
    volume.metadata.skipped_message_count += skipped_planes;
    Ok(())
}

/// Remap `undetect` onto the grid's single transparent sentinel.
fn remap_sentinel<T: Copy + PartialEq>(raw: T, undetect: Option<T>, sentinel: Option<T>) -> T {
    match (undetect, sentinel) {
        (Some(undetect), Some(sentinel)) if raw == undetect => sentinel,
        _ => raw,
    }
}

/// Map ODIM quantity codes (spec Table 16) onto the canonical moment set.
fn canonical_quantity(quantity: &str) -> Option<MomentType> {
    match quantity {
        "DBZH" | "DBZV" | "TH" | "TV" | "DBZ" => Some(MomentType::Reflectivity),
        "VRADH" | "VRADV" | "VRAD" | "VRADDH" => Some(MomentType::Velocity),
        "WRADH" | "WRADV" | "WRAD" => Some(MomentType::SpectrumWidth),
        // The unfiltered dual-pol spellings come in both orders in the
        // wild: spec-style trailing U (ZDRU) and DWD's leading U (UZDR,
        // URHOHV — live opendata.dwd.de sweep files, 2026-06-12).
        "ZDR" | "ZDRU" | "UZDR" => Some(MomentType::DifferentialReflectivity),
        "RHOHV" | "RHOHVU" | "URHOHV" => Some(MomentType::CorrelationCoefficient),
        "PHIDP" | "PHIDPU" | "UPHIDP" => Some(MomentType::DifferentialPhase),
        "KDP" | "KDPU" => Some(MomentType::SpecificDifferentialPhase),
        _ => None,
    }
}

fn parse_site(file: &H5File<'_>) -> RadarSite {
    let source = file
        .attr("/what", "source")
        .and_then(|attr| attr.as_str().map(str::to_owned))
        .unwrap_or_default();
    let (id, name) = site_identity_from_source(&source);
    RadarSite {
        id,
        name,
        latitude_deg: attr_f64(file, "/where", "lat").map(|value| value as f32),
        longitude_deg: attr_f64(file, "/where", "lon").map(|value| value as f32),
        elevation_m: attr_f64(file, "/where", "height").map(|value| value as f32),
    }
}

/// Pick site id + display name out of the `/what` `source` attribute:
/// comma-separated "TYP:value" identifier pairs (spec Table 3), e.g.
/// "WMO:02606,RAD:SE50,PLC:Karlskrona,NOD:sekkr". Preference is
/// NOD > RAD > WMO regardless of pair order — operational files (RMI
/// Belgium, met.no) list WMO first but NOD is the canonical OPERA site
/// code (validated against bejab/norst sample volumes).
fn site_identity_from_source(source: &str) -> (String, Option<String>) {
    let (mut nod, mut rad, mut wmo, mut name) = (None, None, None, None);
    for pair in source.split(',') {
        let Some((key, value)) = pair.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "NOD" => nod = Some(value.to_uppercase()),
            "RAD" => rad = Some(value.to_owned()),
            "WMO" => wmo = Some(value.to_owned()),
            "PLC" => name = Some(value.to_owned()),
            _ => {}
        }
    }
    let id = nod.or(rad).or(wmo).unwrap_or_else(|| "ODIM".to_owned());
    (id, name)
}

fn parse_datetime(file: &H5File<'_>, group: &str) -> Option<chrono::DateTime<Utc>> {
    let date = file.attr(group, "date")?.as_str()?.to_owned();
    let time = file.attr(group, "time")?.as_str()?.to_owned();
    let date = NaiveDate::parse_from_str(&date, "%Y%m%d").ok()?;
    let time = NaiveTime::parse_from_str(&time, "%H%M%S").ok()?;
    Some(Utc.from_utc_datetime(&NaiveDateTime::new(date, time)))
}

fn attr_f64(file: &H5File<'_>, path: &str, name: &str) -> Option<f64> {
    file.attr(path, name).as_ref().and_then(H5Attr::as_f64)
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
    fn quantity_codes_map_to_moments() {
        assert_eq!(canonical_quantity("DBZH"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_quantity("VRADH"), Some(MomentType::Velocity));
        assert_eq!(canonical_quantity("WRADH"), Some(MomentType::SpectrumWidth));
        assert_eq!(
            canonical_quantity("RHOHV"),
            Some(MomentType::CorrelationCoefficient)
        );
        assert_eq!(canonical_quantity("QIND"), None);
    }

    #[test]
    fn site_identity_prefers_nod_over_wmo_regardless_of_pair_order() {
        // Real operational source strings put WMO first; NOD must win.
        let (id, name) = site_identity_from_source(
            "WMO:06410,RAD:BX42,PLC:Jabbeke,NOD:bejab,CTY:605,CMT:bejab_scan_v3_Z_dBZ",
        );
        assert_eq!(id, "BEJAB");
        assert_eq!(name.as_deref(), Some("Jabbeke"));
        // No NOD: fall back RAD, then WMO; empty values are skipped.
        let (id, _) = site_identity_from_source("RAD:AU40,PLC:CapFlat,CTY:500,STN:70341");
        assert_eq!(id, "AU40");
        let (id, _) = site_identity_from_source("WMO:01104,NOD:");
        assert_eq!(id, "01104");
        let (id, _) = site_identity_from_source("CMT:whatever");
        assert_eq!(id, "ODIM");
    }

    #[test]
    fn undetect_remaps_to_the_nodata_sentinel() {
        assert_eq!(remap_sentinel(0u8, Some(0), Some(255)), 255);
        assert_eq!(remap_sentinel(7u8, Some(0), Some(255)), 7);
        assert_eq!(remap_sentinel(0u8, None, Some(255)), 0);
    }
}
