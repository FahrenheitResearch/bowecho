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
//! - Doppler-velocity no-data recovery: some IRIS exporters (AEMET Spain,
//!   IRIS 10.3) copy the REFLECTIVITY `what` group onto the velocity plane —
//!   the VRADH `nodata`/`undetect` carry the dBZ sentinels (e.g. 95.5 / -32)
//!   while no-echo velocity gates are filled with the physical `offset`
//!   (0 m/s for gain=1/offset=0). Those fill gates match neither declared
//!   sentinel, so the base decode leaves a spurious 0 m/s wall over the whole
//!   plane. `recover_copied_whatgroup_velocity_nodata` masks a velocity gate
//!   sitting on `offset` when the co-located reflectivity gate is no-echo,
//!   and only when the velocity sentinels equal the reflectivity sentinels
//!   (the copied-what-group signature) — genuine 0 m/s gates with echo, and
//!   conformant writers with distinct velocity sentinels, are untouched.
//! - `rstart` is km to the start of the first bin; `rscale` is the bin
//!   spacing in metres (Table 5 "where" for polar data). Implausibly large
//!   `rstart` values are reinterpreted as metres — see
//!   `first_gate_m_from_rstart` for the writer quirk that requires it.
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
use std::collections::BTreeMap;

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
    volume.metadata.radar_frequency_mhz = odim_radar_frequency_mhz(&file);
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
        first_gate_m: first_gate_m_from_rstart(rstart_km),
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

    let mut canonical_priorities = BTreeMap::<MomentType, u8>::new();
    let mut plane_meta = BTreeMap::<MomentType, PlaneNoData>::new();
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

        let canonical = canonical_quantity(&quantity);
        let Some(moment) = decode_moment_for_quantity(
            &quantity,
            canonical.clone(),
            &canonical_priorities,
            &cut.moments,
        ) else {
            continue;
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
        if let Some(canonical) = canonical {
            canonical_priorities.insert(canonical, canonical_quantity_priority(&quantity));
        }
        plane_meta.insert(
            moment.clone(),
            PlaneNoData {
                nodata,
                undetect,
                offset,
            },
        );
        cut.moments.insert(moment, grid);
    }
    recover_copied_whatgroup_velocity_nodata(&mut cut, &plane_meta);
    volume.cuts.push(cut);
    volume.metadata.skipped_message_count += skipped_planes;
    Ok(())
}

/// A first bin starting this many km downrange is physically implausible
/// for a PVOL/SCAN: conformant writers put `rstart` at 0 or within a few km
/// (0.0 across the bejab/bewid/norst fixtures, 22 sweeps), and even
/// long-pulse blind ranges are single-digit km. Values beyond this are
/// metre-valued writer output (see `first_gate_m_from_rstart`).
const RSTART_SANE_MAX_KM: f64 = 20.0;

/// Metres to the start of the first bin, from the `where/rstart` attribute.
///
/// ODIM_H5 defines `rstart` in km (Table 5, polar "where"), but AEMET Spain
/// (IRIS 8.13/10.3 exports; all 11 sites surveyed on the OPERA ORD bucket,
/// 2026-07-07) writes it in METRES: observed 125/167/200 across the network.
/// Read as km those would start every ray 125–200 km downrange — past the
/// 150 km extent of the very sweeps they describe (299 bins x 500 m) — while
/// as metres they are classic IRIS range-start values on the same scale as
/// the bin spacing. A physical-sanity rule beats sniffing the IRIS source
/// string: other IRIS exports write conformant km, and any writer whose
/// first bin "starts" > [`RSTART_SANE_MAX_KM`] out is reporting metres.
/// (Metre-valued quirk output below the threshold is indistinguishable from
/// km, but IRIS range starts sit at gate-size scale — hundreds of metres —
/// and 0 reads identically in either unit.)
fn first_gate_m_from_rstart(rstart: f64) -> i32 {
    if rstart > RSTART_SANE_MAX_KM {
        // Reinterpret as metres (writer quirk documented above).
        rstart.round() as i32
    } else {
        (rstart * 1000.0).round() as i32
    }
}

/// Remap `undetect` onto the grid's single transparent sentinel.
fn remap_sentinel<T: Copy + PartialEq>(raw: T, undetect: Option<T>, sentinel: Option<T>) -> T {
    match (undetect, sentinel) {
        (Some(undetect), Some(sentinel)) if raw == undetect => sentinel,
        _ => raw,
    }
}

/// The `what` no-data attributes of the plane a moment was decoded from,
/// captured alongside its grid so a sweep-level pass can cross-reference them.
struct PlaneNoData {
    nodata: Option<f64>,
    undetect: Option<f64>,
    offset: f64,
}

/// A velocity gate within this distance of the plane's physical `offset` is
/// treated as sitting exactly on the collapsed no-data / zero code. The gate
/// spacing of any Doppler quantum (≈0.3 m/s for a 40 m/s Nyquist) is orders of
/// magnitude larger, so this only ever catches the exact `offset` fill.
const VELOCITY_OFFSET_EPS: f32 = 1.0e-6;

/// Recover no-echo Doppler-velocity gates that a copied-`what`-group writer
/// (AEMET Spain / IRIS 10.3) leaves decoding to a spurious `offset` (0 m/s).
///
/// Such writers stamp the velocity plane's `nodata`/`undetect` with the
/// REFLECTIVITY sentinels while filling no-echo velocity gates with the
/// physical `offset`, so those gates match no declared sentinel and survive
/// the base decode as a wall of 0 m/s. There is no per-plane attribute that
/// distinguishes the fill from a genuine 0 m/s reading, so the file's own
/// reflectivity no-echo mask is the only ground truth: a velocity gate on
/// `offset` with no co-located reflectivity echo is the writer's collapsed
/// no-data code; the same value where reflectivity IS present is a real
/// 0 m/s reading and is preserved.
///
/// Guarded by the copied-what-group signature (velocity sentinels equal the
/// reflectivity sentinels) so conformant writers — which give velocity its own
/// distinct sentinels, already masked by the base decode — are never touched.
/// Also a no-op unless both planes share ray/gate geometry.
fn recover_copied_whatgroup_velocity_nodata(
    cut: &mut radar_core::ElevationCut,
    plane_meta: &BTreeMap<MomentType, PlaneNoData>,
) {
    let (Some(vel_meta), Some(ref_meta)) = (
        plane_meta.get(&MomentType::Velocity),
        plane_meta.get(&MomentType::Reflectivity),
    ) else {
        return;
    };
    // Copied-what-group signature: the velocity plane carries the reflectivity
    // plane's no-data sentinels verbatim (and at least one is present).
    let sentinels_copied = vel_meta.nodata == ref_meta.nodata
        && vel_meta.undetect == ref_meta.undetect
        && (vel_meta.nodata.is_some() || vel_meta.undetect.is_some());
    if !sentinels_copied {
        return;
    }

    let (Some(velocity), Some(reflectivity)) = (
        cut.moments.get(&MomentType::Velocity),
        cut.moments.get(&MomentType::Reflectivity),
    ) else {
        return;
    };
    let rows = velocity.radial_indices.len();
    let gates = velocity.gate_range.gate_count;
    if reflectivity.radial_indices.len() != rows || reflectivity.gate_range.gate_count != gates {
        return; // differing geometry: do not risk mis-masking
    }

    let offset = vel_meta.offset as f32;
    let mut targets: Vec<usize> = Vec::new();
    for row in 0..rows {
        for gate in 0..gates {
            // Reflectivity no-echo: None (u8/u16 sentinel) or NaN (float).
            let ref_no_echo = reflectivity
                .scaled_value(row, gate)
                .is_none_or(|z| !z.is_finite());
            if !ref_no_echo {
                continue;
            }
            if let Some(value) = velocity.scaled_value(row, gate)
                && value.is_finite()
                && (value - offset).abs() <= VELOCITY_OFFSET_EPS
            {
                targets.push(row * gates + gate);
            }
        }
    }
    if targets.is_empty() {
        return;
    }
    if let Some(velocity) = cut.moments.get_mut(&MomentType::Velocity) {
        mask_gates_no_data(velocity, &targets);
    }
}

/// Set the given flat gate indices to the grid's transparent no-data code:
/// NaN for float storage, the `nodata` sentinel for integer storage (integer
/// grids with no sentinel are left unchanged — nothing transparent to write).
fn mask_gates_no_data(grid: &mut MomentGrid, targets: &[usize]) {
    let sentinel = grid.nodata;
    match &mut grid.storage {
        radar_core::MomentStorage::F32(values) => {
            for &index in targets {
                if let Some(slot) = values.get_mut(index) {
                    *slot = f32::NAN;
                }
            }
        }
        radar_core::MomentStorage::U8(values) => {
            let Some(sentinel) = sentinel else { return };
            let sentinel = sentinel as u8;
            for &index in targets {
                if let Some(slot) = values.get_mut(index) {
                    *slot = sentinel;
                }
            }
        }
        radar_core::MomentStorage::U16(values) => {
            let Some(sentinel) = sentinel else { return };
            for &index in targets {
                if let Some(slot) = values.get_mut(index) {
                    *slot = sentinel;
                }
            }
        }
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

fn canonical_quantity_priority(quantity: &str) -> u8 {
    match quantity {
        // Prefer filtered reflectivity when a file carries both DBZH and
        // unfiltered TH/TV. ODIM writers do not guarantee plane order.
        "DBZH" | "DBZV" => 30,
        "DBZ" => 25,
        "TH" | "TV" => 10,
        // Prefer filtered dual-pol spellings over explicitly unfiltered ones.
        "ZDR" | "RHOHV" | "PHIDP" | "KDP" => 30,
        "ZDRU" | "UZDR" | "RHOHVU" | "URHOHV" | "PHIDPU" | "UPHIDP" | "KDPU" => 10,
        _ => 20,
    }
}

fn decode_moment_for_quantity(
    quantity: &str,
    canonical: Option<MomentType>,
    canonical_priorities: &BTreeMap<MomentType, u8>,
    existing_moments: &BTreeMap<MomentType, MomentGrid>,
) -> Option<MomentType> {
    match canonical {
        Some(moment) => {
            let priority = canonical_quantity_priority(quantity);
            let existing_priority = canonical_priorities.get(&moment).copied();
            if existing_priority.is_none_or(|existing| priority > existing) {
                Some(moment)
            } else {
                None
            }
        }
        None => {
            let unknown = MomentType::Unknown(quantity.to_owned());
            (!existing_moments.contains_key(&unknown)).then_some(unknown)
        }
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

fn odim_radar_frequency_mhz(file: &H5File<'_>) -> Option<u32> {
    for name in ["frequency", "freq", "radar_frequency", "radar_frequency_hz"] {
        if let Some(value) = attr_f64(file, "/how", name)
            && let Some(mhz) = normalize_frequency_mhz(value)
        {
            return Some(mhz);
        }
    }
    for name in ["wavelength", "radar_wavelength", "wavelength_cm"] {
        if let Some(value) = attr_f64(file, "/how", name)
            && let Some(mhz) = frequency_mhz_from_wavelength(value)
        {
            return Some(mhz);
        }
    }
    None
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
    fn filtered_odim_quantities_win_duplicate_canonical_moments() {
        let mut priorities = BTreeMap::new();
        let moments = BTreeMap::new();
        let reflectivity = Some(MomentType::Reflectivity);

        assert_eq!(
            decode_moment_for_quantity("TH", reflectivity.clone(), &priorities, &moments),
            Some(MomentType::Reflectivity)
        );
        priorities.insert(MomentType::Reflectivity, canonical_quantity_priority("TH"));
        assert_eq!(
            decode_moment_for_quantity("DBZH", reflectivity.clone(), &priorities, &moments),
            Some(MomentType::Reflectivity)
        );
        priorities.insert(
            MomentType::Reflectivity,
            canonical_quantity_priority("DBZH"),
        );
        assert_eq!(
            decode_moment_for_quantity("TH", reflectivity, &priorities, &moments),
            None
        );
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
    fn rstart_beyond_sane_range_reinterprets_as_metres() {
        // Spec-conformant km values pass through unchanged.
        assert_eq!(first_gate_m_from_rstart(0.0), 0);
        assert_eq!(first_gate_m_from_rstart(0.05), 50);
        assert_eq!(first_gate_m_from_rstart(5.0), 5_000);
        // Just under the physical-sanity bound: still km.
        assert_eq!(first_gate_m_from_rstart(19.9), 19_900);
        assert_eq!(first_gate_m_from_rstart(20.0), 20_000);
        // AEMET metre-valued rstart (125/167/200 observed across the
        // network, 2026-07-07): reinterpreted as metres, not 125+ km.
        assert_eq!(first_gate_m_from_rstart(125.0), 125);
        assert_eq!(first_gate_m_from_rstart(167.0), 167);
        assert_eq!(first_gate_m_from_rstart(200.0), 200);
    }

    #[test]
    fn undetect_remaps_to_the_nodata_sentinel() {
        assert_eq!(remap_sentinel(0u8, Some(0), Some(255)), 255);
        assert_eq!(remap_sentinel(7u8, Some(0), Some(255)), 7);
        assert_eq!(remap_sentinel(0u8, None, Some(255)), 0);
    }

    /// Build a one-ray float moment grid (offset 0, scale 1) for the recovery
    /// tests: NaN encodes no-echo, finite values are physical readings.
    fn float_grid(moment: MomentType, row: Vec<f32>) -> MomentGrid {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 500,
            gate_count: row.len(),
        };
        let mut grid = MomentGrid {
            moment,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: Vec::new(),
            storage: radar_core::MomentStorage::F32(Vec::new()),
        };
        grid.push_row(0, MomentRow::F32(row)).expect("push row");
        grid
    }

    fn copied_sentinel_cut() -> radar_core::ElevationCut {
        // gate0: Z no-echo, V=0  -> collapsed no-data fill (must mask)
        // gate1: Z echo,    V=0  -> genuine 0 m/s reading (must keep)
        // gate2: Z no-echo, V=5  -> velocity off `offset` (must keep)
        // gate3: Z echo,    V=3  -> ordinary gate (must keep)
        let reflectivity = float_grid(
            MomentType::Reflectivity,
            vec![f32::NAN, 10.0, f32::NAN, 20.0],
        );
        let velocity = float_grid(MomentType::Velocity, vec![0.0, 0.0, 5.0, 3.0]);
        let mut cut = radar_core::ElevationCut::new(0.5, None);
        cut.moments.insert(MomentType::Reflectivity, reflectivity);
        cut.moments.insert(MomentType::Velocity, velocity);
        cut
    }

    #[test]
    fn copied_whatgroup_recovery_masks_only_no_echo_offset_gates() {
        let mut cut = copied_sentinel_cut();
        // Copied-what-group signature: velocity carries the reflectivity
        // sentinels (the AEMET/IRIS bug), so recovery engages.
        let mut plane_meta = BTreeMap::new();
        let sentinels = |offset| PlaneNoData {
            nodata: Some(95.5),
            undetect: Some(-32.0),
            offset,
        };
        plane_meta.insert(MomentType::Reflectivity, sentinels(-32.0));
        plane_meta.insert(MomentType::Velocity, sentinels(0.0));

        recover_copied_whatgroup_velocity_nodata(&mut cut, &plane_meta);
        let vel = cut.moments.get(&MomentType::Velocity).unwrap();
        assert!(
            vel.scaled_value(0, 0).unwrap().is_nan(),
            "no-echo 0 m/s fill masked"
        );
        assert_eq!(
            vel.scaled_value(0, 1),
            Some(0.0),
            "genuine 0 m/s with echo kept"
        );
        assert_eq!(
            vel.scaled_value(0, 2),
            Some(5.0),
            "velocity off offset kept even without echo"
        );
        assert_eq!(vel.scaled_value(0, 3), Some(3.0), "ordinary gate kept");
    }

    #[test]
    fn distinct_velocity_sentinels_are_never_reflectivity_gated() {
        // Velocity declares its OWN sentinels (not the reflectivity ones):
        // a conformant writer. Recovery must not touch the plane, so the
        // 0 m/s gate co-located with no-echo reflectivity survives unchanged.
        let mut cut = copied_sentinel_cut();
        let mut plane_meta = BTreeMap::new();
        plane_meta.insert(
            MomentType::Reflectivity,
            PlaneNoData {
                nodata: Some(95.5),
                undetect: Some(-32.0),
                offset: -32.0,
            },
        );
        plane_meta.insert(
            MomentType::Velocity,
            PlaneNoData {
                nodata: Some(-999.0),
                undetect: Some(888.0),
                offset: 0.0,
            },
        );

        recover_copied_whatgroup_velocity_nodata(&mut cut, &plane_meta);
        let vel = cut.moments.get(&MomentType::Velocity).unwrap();
        assert_eq!(
            vel.scaled_value(0, 0),
            Some(0.0),
            "distinct-sentinel writer left untouched"
        );
    }
}
