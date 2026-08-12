//! BowEcho store bridge for SimSat's raw derived scalar products.
//!
//! SimSat v0.1.2 ships visible/RGB and thermal-band writers, but its
//! precipitable-water, cloud-top-temperature, and cloud-optical-depth products
//! are returned only as in-memory scalar arrays. This writer preserves those
//! physical values in the shared satellite store so the Satellite player can
//! color them while Native plot can reopen the original values and units.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustwx_core::{GridShape, LatLonGrid};
use rw_sat::store::frame_file_name;
use rw_store::format::RwsWriterInfo;
use rw_store::grid::{GridFile, write_grid};
use rw_store::lock::RunLock;
use rw_store::run::{RwsHourEntry, RwsRunManifest};
use rw_store::writer::HourWriter;
use simsat::camera::SatellitePreset;
use simsat::derived::DerivedField;

const SIMSAT_MODEL: &str = "simsat";
const VARIABLE_PREFIX: &str = "simsat_derived_";
const WRITER_BUILD: &str = concat!("bowecho ", env!("CARGO_PKG_VERSION"), " + simsat");

#[derive(Debug, Clone)]
pub(crate) struct DerivedFrame {
    pub(crate) nx: usize,
    pub(crate) ny: usize,
    pub(crate) values: Vec<f32>,
    pub(crate) lat: Vec<f32>,
    pub(crate) lon: Vec<f32>,
    pub(crate) sector: String,
    pub(crate) satellite: SatellitePreset,
    pub(crate) field: DerivedField,
    pub(crate) year: i32,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) hhmm: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrittenDerivedFrame {
    pub(crate) model: String,
    pub(crate) run: String,
    pub(crate) hhmm: u16,
}

pub(crate) fn variable_name(field: DerivedField) -> String {
    format!("{VARIABLE_PREFIX}{}", field.slug())
}

pub(crate) fn field_from_variable(name: &str) -> Option<DerivedField> {
    name.strip_prefix(VARIABLE_PREFIX)
        .and_then(DerivedField::parse)
}

pub(crate) fn write_derived_frame(
    store_root: &Path,
    frame: &DerivedFrame,
) -> Result<WrittenDerivedFrame, String> {
    let cell_count = frame
        .nx
        .checked_mul(frame.ny)
        .ok_or_else(|| format!("derived frame {}x{} overflows", frame.nx, frame.ny))?;
    if frame.values.len() != cell_count
        || frame.lat.len() != cell_count
        || frame.lon.len() != cell_count
    {
        return Err(format!(
            "derived frame {}x{} needs {cell_count} cells; values={}, lat={}, lon={}",
            frame.nx,
            frame.ny,
            frame.values.len(),
            frame.lat.len(),
            frame.lon.len()
        ));
    }

    let shape = GridShape::new(frame.nx, frame.ny).map_err(|error| error.to_string())?;
    let grid = LatLonGrid::new(shape, frame.lat.clone(), frame.lon.clone())
        .map_err(|error| error.to_string())?;
    let sector = simsat::store_out::sanitize_store_token(&frame.sector);
    let day = format!("{:04}{:02}{:02}", frame.year, frame.month, frame.day);
    let run_base = format!("{sector}_scalar_{}_{day}", frame.satellite.slug());
    let model_dir = store_root.join(SIMSAT_MODEL);
    let candidates = matching_run_candidates(&model_dir, &run_base)?;
    let matching = find_matching_grid(&model_dir, &candidates, frame, &grid)?;
    let (run, existing_hash) =
        matching.unwrap_or_else(|| (first_free_run_name(&candidates, &run_base), None::<String>));

    let run_dir = model_dir.join(&run);
    std::fs::create_dir_all(&run_dir).map_err(|error| error.to_string())?;
    let _lock =
        RunLock::acquire(&run_dir, Duration::from_secs(60)).map_err(|error| error.to_string())?;
    let grid_path = run_dir.join("grid.rwg");
    let grid_hash = match existing_hash {
        Some(hash) => hash,
        None => write_grid(&grid_path, &grid, None).map_err(|error| error.to_string())?,
    };

    let variable = variable_name(frame.field);
    let selector = serde_json::json!({
        "simsat": {
            "kind": "derived_scalar",
            "field": frame.field.slug(),
            "label": frame.field.label(),
        },
        "satellite": {
            "provider": "simsat",
            "instrument": "synthetic-derived",
            "satellite": frame.satellite.slug(),
            "model": SIMSAT_MODEL,
            "product": frame.field.slug(),
            "sector": sector,
            "scan_start_utc": format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:00Z",
                frame.year,
                frame.month,
                frame.day,
                frame.hhmm / 100,
                frame.hhmm % 100
            ),
        }
    });
    let started = Instant::now();
    let mut writer = HourWriter::new(
        SIMSAT_MODEL,
        &run,
        frame.hhmm,
        frame.nx,
        frame.ny,
        &grid_hash,
        WRITER_BUILD,
    );
    writer
        .add_surface2d(&variable, frame.field.units(), selector, &frame.values)
        .map_err(|error| error.to_string())?;
    let file_name = frame_file_name(frame.hhmm);
    writer
        .finish(&run_dir.join(&file_name))
        .map_err(|error| error.to_string())?;

    let manifest_path = run_dir.join("run.json");
    let mut manifest = RwsRunManifest::load_or_new(
        &manifest_path,
        SIMSAT_MODEL,
        &run,
        &grid_hash,
        frame.nx,
        frame.ny,
        RwsWriterInfo {
            name: "bowecho-simsat".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: WRITER_BUILD.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    manifest.register_hour(
        frame.hhmm,
        RwsHourEntry {
            file: file_name,
            lead_seconds: None,
            valid_unix: None,
            written_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
            encode_ms: started.elapsed().as_millis() as u64,
            variables: vec![variable],
            source_provenance: Vec::new(),
        },
    );
    manifest
        .save(&manifest_path)
        .map_err(|error| error.to_string())?;

    Ok(WrittenDerivedFrame {
        model: SIMSAT_MODEL.to_owned(),
        run,
        hhmm: frame.hhmm,
    })
}

fn matching_run_candidates(model_dir: &Path, run_base: &str) -> Result<Vec<String>, String> {
    let mut candidates = Vec::new();
    if model_dir.is_dir() {
        for entry in std::fs::read_dir(model_dir).map_err(|error| error.to_string())? {
            let name = entry
                .map_err(|error| error.to_string())?
                .file_name()
                .to_string_lossy()
                .to_string();
            if name == run_base || name.starts_with(&format!("{run_base}_")) {
                candidates.push(name);
            }
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn find_matching_grid(
    model_dir: &Path,
    candidates: &[String],
    frame: &DerivedFrame,
    grid: &LatLonGrid,
) -> Result<Option<(String, Option<String>)>, String> {
    for run in candidates {
        let path = model_dir.join(run).join("grid.rwg");
        if !path.is_file() {
            continue;
        }
        let existing = GridFile::open(&path).map_err(|error| error.to_string())?;
        if existing.nx == frame.nx
            && existing.ny == frame.ny
            && coords_bit_identical(&existing.lat, &grid.lat_deg)
            && coords_bit_identical(&existing.lon, &grid.lon_deg)
        {
            return Ok(Some((run.clone(), Some(existing.hash))));
        }
    }
    Ok(None)
}

fn coords_bit_identical(left: &[f32], right: &[f32]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

fn first_free_run_name(candidates: &[String], run_base: &str) -> String {
    for suffix in 1usize.. {
        let candidate = if suffix == 1 {
            run_base.to_owned()
        } else {
            format!("{run_base}_{suffix}")
        };
        if !candidates.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("an unbounded numeric suffix always has a free value")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rw_store::reader::HourReader;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bowecho-simsat-derived-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn frame(field: DerivedField, hhmm: u16) -> DerivedFrame {
        DerivedFrame {
            nx: 2,
            ny: 2,
            values: vec![1.0, 2.0, f32::NAN, 4.0],
            lat: vec![30.0, 30.0, 31.0, 31.0],
            lon: vec![-101.0, -100.0, -101.0, -100.0],
            sector: format!("hrrr_20260710_t19z_{}_geo", field.slug()),
            satellite: SatellitePreset::GoesEast,
            field,
            year: 2026,
            month: 7,
            day: 10,
            hhmm,
        }
    }

    #[test]
    fn derived_variable_names_round_trip_every_field() {
        for field in DerivedField::ALL {
            let name = variable_name(field);
            assert_eq!(field_from_variable(&name), Some(field));
        }
        assert_eq!(field_from_variable("rgb_r"), None);
    }

    #[test]
    fn derived_frames_preserve_raw_values_and_join_one_loop() {
        let dir = test_dir("loop");
        let first = frame(DerivedField::PrecipitableWater, 2000);
        let second = frame(DerivedField::PrecipitableWater, 2100);
        let written_first = write_derived_frame(&dir, &first).unwrap();
        let written_second = write_derived_frame(&dir, &second).unwrap();
        assert_eq!(written_first.run, written_second.run);

        let run_dir = dir.join(&written_first.model).join(&written_first.run);
        let manifest: RwsRunManifest =
            serde_json::from_slice(&std::fs::read(run_dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(
            manifest.hours.keys().copied().collect::<Vec<_>>(),
            [2000, 2100]
        );
        let reader = HourReader::open(&run_dir.join(frame_file_name(2100))).unwrap();
        let variable = variable_name(DerivedField::PrecipitableWater);
        let decoded = reader.read_full_2d(&variable).unwrap();
        assert_eq!(decoded.len(), second.values.len());
        for (decoded, expected) in decoded.iter().zip(&second.values) {
            if expected.is_nan() {
                assert!(decoded.is_nan());
            } else {
                assert!((decoded - expected).abs() < 1.0e-3);
            }
        }
        let meta = reader.meta();
        let stored = meta
            .variables
            .iter()
            .find(|candidate| candidate.name == variable)
            .unwrap();
        assert_eq!(stored.units, "mm");
        assert_eq!(stored.selector["simsat"]["field"], "pw");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
