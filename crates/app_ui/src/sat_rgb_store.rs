//! Store writer for already-rendered, explicitly geolocated satellite RGB.
//!
//! This is the bridge for providers that expose a regular longitude/latitude
//! image (for example, a WMS `GetMap` response) rather than instrument-native
//! radiances. The input RGB byte order and the generated grid use the same
//! north-to-south row order, so no image resampling or implicit row flip occurs
//! here. The resulting run is indistinguishable from the existing baked GOES /
//! Himawari RGB runs to the satellite player, radar-map layer, and native plot:
//! it owns one `grid.rwg`, three `rgb_r` / `rgb_g` / `rgb_b` surface planes,
//! `tHHMM.rws` frames, and a `run.json` manifest.

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Timelike, Utc};
use rustwx_core::{GridShape, LatLonGrid, MAX_GRID_CELLS};
use rw_sat::store::{WrittenFrame, frame_file_name};
use rw_store::format::RwsWriterInfo;
use rw_store::grid::{GridFile, write_grid};
use rw_store::lock::RunLock;
use rw_store::run::{RwsHourEntry, RwsRunManifest};
use rw_store::writer::HourWriter;

pub(crate) const RGB_R_VAR: &str = "rgb_r";
pub(crate) const RGB_G_VAR: &str = "rgb_g";
pub(crate) const RGB_B_VAR: &str = "rgb_b";

/// Source/product/model tokens become path components. Keeping each one short
/// avoids platform path-limit surprises and refuses ambiguous truncation.
const MAX_STORE_TOKEN_LEN: usize = 64;
const RUN_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

/// Geographic WMS bounds in conventional west/south/east/north order.
///
/// Antimeridian-crossing boxes are deliberately rejected. WMS servers can
/// represent those as two ordinary requests, while a single discontinuous
/// longitude axis would make the app's inverse map lookup ambiguous.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LonLatBounds {
    pub west_deg: f64,
    pub south_deg: f64,
    pub east_deg: f64,
    pub north_deg: f64,
}

/// A decoded 24-bit RGB WMS image with an optional separate alpha plane.
/// `rgb` is tightly packed, row-major `[R,G,B, R,G,B, ...]`; row zero is the
/// north (top) edge of the image. `alpha`, when present, contains exactly one
/// byte per pixel in the same order. Alpha zero becomes `NaN` in all three
/// stored color planes, which is the baked-RGB store contract for transparent
/// map/player pixels. The store has no fractional-alpha plane, so alpha 1..255
/// is retained as opaque RGB rather than silently composited against a color.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RegularLonLatRgb<'a> {
    pub width: usize,
    pub height: usize,
    pub bounds: LonLatBounds,
    pub rgb: &'a [u8],
    pub alpha: Option<&'a [u8]>,
}

/// Stable, non-secret identity attached to a WMS RGB frame.
///
/// `model` is the satellite-store model directory (for example `mtg-i1`).
/// Within that directory, equal `source_id` + `product_id` + UTC day values
/// share one loop whenever their grids are bit-identical. A changed WMS size
/// or bounding box receives a suffixed sibling run instead of corrupting the
/// existing loop. `extra_metadata` must never contain API credentials.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RgbSatelliteMetadata {
    pub source_id: String,
    pub provider: String,
    pub instrument: String,
    pub satellite: String,
    pub model: String,
    pub product_id: String,
    pub product_title: String,
    pub sector: String,
    pub scan_start_utc: DateTime<Utc>,
    pub scan_end_utc: DateTime<Utc>,
    pub extra_metadata: serde_json::Value,
}

/// Write one regular lon/lat RGB image into the shared satellite store.
///
/// The returned [`WrittenFrame`] supplies the `model`, `run`, and `hhmm` needed
/// for `SatRequest::ScanAndSelect` (or equivalent `Runs` + `SelectFrame`
/// responses) after the caller wires this module into the satellite worker.
pub(crate) fn write_regular_lonlat_rgb_frame(
    store_root: &Path,
    image: RegularLonLatRgb<'_>,
    metadata: &RgbSatelliteMetadata,
    written_unix: u64,
) -> Result<WrittenFrame, String> {
    let validated = validate_input(image, metadata)?;
    let grid = regular_lonlat_grid(image.width, image.height, image.bounds)?;
    let (r, g, b) = split_rgb_planes(image.rgb, image.alpha, validated.pixel_count);

    let model = sanitize_store_token(&metadata.model);
    let source = sanitize_store_token(&metadata.source_id);
    let product = sanitize_store_token(&metadata.product_id);
    let day = metadata.scan_start_utc.format("%Y%m%d").to_string();
    let hhmm = (metadata.scan_start_utc.hour() * 100 + metadata.scan_start_utc.minute()) as u16;
    // `_rgb_` is an application contract: the current run filter recognizes
    // baked three-plane frames by this marker. `wms` prevents a product named
    // `true_color` from being mislabeled as the AHI-specific recipe.
    let run_base = format!("{source}_rgb_wms_{product}_{day}");

    let model_dir = store_root.join(&model);
    let resolved = resolve_run(&model_dir, &run_base, &grid)?;
    let run_dir = model_dir.join(&resolved.run_name);
    std::fs::create_dir_all(&run_dir).map_err(|error| error.to_string())?;
    let _lock = RunLock::acquire(&run_dir, RUN_LOCK_TIMEOUT).map_err(|error| error.to_string())?;

    let grid_path = run_dir.join("grid.rwg");
    let grid_hash = match resolved.existing_grid_hash {
        Some(hash) => hash,
        None => write_grid(&grid_path, &grid, None).map_err(|error| error.to_string())?,
    };

    let started = Instant::now();
    let selector = rgb_selector(image, metadata);
    let writer_build = concat!("bowecho app_ui ", env!("CARGO_PKG_VERSION"));
    let mut writer = HourWriter::new(
        &model,
        &resolved.run_name,
        hhmm,
        image.width,
        image.height,
        &grid_hash,
        writer_build,
    );
    writer
        .add_surface2d(RGB_R_VAR, "rgb8", selector, &r)
        .map_err(|error| error.to_string())?;
    writer
        .add_surface2d(RGB_G_VAR, "rgb8", serde_json::Value::Null, &g)
        .map_err(|error| error.to_string())?;
    writer
        .add_surface2d(RGB_B_VAR, "rgb8", serde_json::Value::Null, &b)
        .map_err(|error| error.to_string())?;
    let file_name = frame_file_name(hhmm);
    let frame_path = run_dir.join(&file_name);
    writer
        .finish(&frame_path)
        .map_err(|error| error.to_string())?;
    let encode_ms = started.elapsed().as_millis() as u64;
    let bytes = std::fs::metadata(&frame_path)
        .map_err(|error| error.to_string())?
        .len();

    let manifest_path = run_dir.join("run.json");
    let writer_info = RwsWriterInfo {
        name: "bowecho".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        build: writer_build.to_string(),
    };
    let mut manifest = RwsRunManifest::load_or_new(
        &manifest_path,
        &model,
        &resolved.run_name,
        &grid_hash,
        image.width,
        image.height,
        writer_info,
    )
    .map_err(|error| error.to_string())?;
    manifest.register_hour(
        hhmm,
        RwsHourEntry {
            file: file_name,
            lead_seconds: None,
            valid_unix: None,
            written_unix,
            encode_ms,
            variables: vec![
                RGB_R_VAR.to_string(),
                RGB_G_VAR.to_string(),
                RGB_B_VAR.to_string(),
            ],
            source_provenance: Vec::new(),
        },
    );
    manifest
        .save(&manifest_path)
        .map_err(|error| error.to_string())?;

    Ok(WrittenFrame {
        model,
        run: resolved.run_name,
        hhmm,
        scan_time_utc: metadata.scan_start_utc,
        path: frame_path,
        bytes,
        encode_ms,
        grid_hash,
        created_run: resolved.created,
        variable: RGB_R_VAR.to_string(),
    })
}

#[derive(Debug, Clone, Copy)]
struct ValidatedInput {
    pixel_count: usize,
}

fn validate_input(
    image: RegularLonLatRgb<'_>,
    metadata: &RgbSatelliteMetadata,
) -> Result<ValidatedInput, String> {
    if image.width < 2 || image.height < 2 {
        return Err(format!(
            "regular lon/lat RGB dimensions must both be at least 2, got {}x{}",
            image.width, image.height
        ));
    }
    let pixel_count = image
        .width
        .checked_mul(image.height)
        .ok_or_else(|| format!("RGB dimensions {}x{} overflow", image.width, image.height))?;
    if pixel_count > MAX_GRID_CELLS {
        return Err(format!(
            "RGB image has {pixel_count} pixels; shared grid safety limit is {MAX_GRID_CELLS}"
        ));
    }
    let expected_bytes = pixel_count.checked_mul(3).ok_or_else(|| {
        format!(
            "RGB byte count for {}x{} overflows",
            image.width, image.height
        )
    })?;
    if image.rgb.len() != expected_bytes {
        return Err(format!(
            "RGB byte length {} does not match {}x{}x3 ({expected_bytes})",
            image.rgb.len(),
            image.width,
            image.height
        ));
    }
    if let Some(alpha) = image.alpha
        && alpha.len() != pixel_count
    {
        return Err(format!(
            "alpha byte length {} does not match {}x{} ({pixel_count})",
            alpha.len(),
            image.width,
            image.height
        ));
    }
    validate_bounds(image.bounds)?;
    if metadata.scan_end_utc < metadata.scan_start_utc {
        return Err(format!(
            "satellite scan end {} precedes start {}",
            metadata.scan_end_utc.to_rfc3339(),
            metadata.scan_start_utc.to_rfc3339()
        ));
    }
    for (label, value) in [
        ("source_id", metadata.source_id.as_str()),
        ("provider", metadata.provider.as_str()),
        ("instrument", metadata.instrument.as_str()),
        ("satellite", metadata.satellite.as_str()),
        ("model", metadata.model.as_str()),
        ("product_id", metadata.product_id.as_str()),
        ("product_title", metadata.product_title.as_str()),
        ("sector", metadata.sector.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("satellite RGB metadata {label} is empty"));
        }
    }
    for (label, value) in [
        ("source_id", metadata.source_id.as_str()),
        ("model", metadata.model.as_str()),
        ("product_id", metadata.product_id.as_str()),
    ] {
        let token = sanitize_store_token(value);
        if token == "unknown" {
            return Err(format!(
                "satellite RGB metadata {label} contains no ASCII letters or digits"
            ));
        }
        if token.len() > MAX_STORE_TOKEN_LEN {
            return Err(format!(
                "satellite RGB metadata {label} normalizes to {} bytes; path-token limit is {MAX_STORE_TOKEN_LEN}",
                token.len()
            ));
        }
    }
    Ok(ValidatedInput { pixel_count })
}

fn validate_bounds(bounds: LonLatBounds) -> Result<(), String> {
    let values = [
        bounds.west_deg,
        bounds.south_deg,
        bounds.east_deg,
        bounds.north_deg,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        return Err("satellite RGB bounds must all be finite".to_string());
    }
    if !(-180.0..=180.0).contains(&bounds.west_deg) || !(-180.0..=180.0).contains(&bounds.east_deg)
    {
        return Err(format!(
            "satellite RGB longitudes must be within -180..180, got west={} east={}",
            bounds.west_deg, bounds.east_deg
        ));
    }
    if !(-90.0..=90.0).contains(&bounds.south_deg) || !(-90.0..=90.0).contains(&bounds.north_deg) {
        return Err(format!(
            "satellite RGB latitudes must be within -90..90, got south={} north={}",
            bounds.south_deg, bounds.north_deg
        ));
    }
    if bounds.east_deg <= bounds.west_deg {
        return Err(format!(
            "satellite RGB bounds must satisfy west < east (antimeridian crossings require two images), got {}..{}",
            bounds.west_deg, bounds.east_deg
        ));
    }
    if bounds.north_deg <= bounds.south_deg {
        return Err(format!(
            "satellite RGB bounds must satisfy south < north, got {}..{}",
            bounds.south_deg, bounds.north_deg
        ));
    }
    Ok(())
}

fn regular_lonlat_grid(
    width: usize,
    height: usize,
    bounds: LonLatBounds,
) -> Result<LatLonGrid, String> {
    validate_bounds(bounds)?;
    let shape = GridShape::new(width, height).map_err(|error| error.to_string())?;
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| format!("RGB grid dimensions {width}x{height} overflow"))?;
    let lon_step = (bounds.east_deg - bounds.west_deg) / width as f64;
    let lat_step = (bounds.north_deg - bounds.south_deg) / height as f64;
    let lon_axis = (0..width)
        .map(|column| (bounds.west_deg + (column as f64 + 0.5) * lon_step) as f32)
        .collect::<Vec<_>>();
    let lat_axis = (0..height)
        .map(|row| (bounds.north_deg - (row as f64 + 0.5) * lat_step) as f32)
        .collect::<Vec<_>>();
    let mut lat = Vec::with_capacity(pixel_count);
    let mut lon = Vec::with_capacity(pixel_count);
    for &latitude in &lat_axis {
        lat.extend(std::iter::repeat_n(latitude, width));
        lon.extend_from_slice(&lon_axis);
    }
    LatLonGrid::new(shape, lat, lon).map_err(|error| error.to_string())
}

fn split_rgb_planes(
    rgb: &[u8],
    alpha: Option<&[u8]>,
    pixel_count: usize,
) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let mut r = Vec::with_capacity(pixel_count);
    let mut g = Vec::with_capacity(pixel_count);
    let mut b = Vec::with_capacity(pixel_count);
    for (index, pixel) in rgb.chunks_exact(3).enumerate() {
        if alpha.is_some_and(|alpha| alpha[index] == 0) {
            r.push(f32::NAN);
            g.push(f32::NAN);
            b.push(f32::NAN);
        } else {
            r.push(f32::from(pixel[0]));
            g.push(f32::from(pixel[1]));
            b.push(f32::from(pixel[2]));
        }
    }
    (r, g, b)
}

fn rgb_selector(image: RegularLonLatRgb<'_>, metadata: &RgbSatelliteMetadata) -> serde_json::Value {
    serde_json::json!({
        "satellite": {
            "provider": metadata.provider,
            "instrument": metadata.instrument,
            "satellite": metadata.satellite,
            "model": metadata.model,
            "product": metadata.product_id,
            "sector": metadata.sector,
            "layer": format!("rgb_wms_{}", sanitize_store_token(&metadata.product_id)),
            "source_variable": "WMS RGB",
            "scan_start_utc": metadata.scan_start_utc.to_rfc3339(),
            "scan_end_utc": metadata.scan_end_utc.to_rfc3339(),
            "projection": {
                "kind": "regular_lonlat",
                "crs": "OGC:CRS84",
                "pixel_registration": "center",
                "row_order": "north_to_south",
            },
            "composite": {
                "style": metadata.product_id,
                "title": metadata.product_title,
                "source_id": metadata.source_id,
            },
            "metadata": {
                "bbox_west_deg": image.bounds.west_deg,
                "bbox_south_deg": image.bounds.south_deg,
                "bbox_east_deg": image.bounds.east_deg,
                "bbox_north_deg": image.bounds.north_deg,
                "width": image.width,
                "height": image.height,
                "alpha": {
                    "present": image.alpha.is_some(),
                    "transparent_value": 0,
                    "nonzero_policy": "opaque",
                },
                "extra": metadata.extra_metadata,
            },
        }
    })
}

struct ResolvedRun {
    run_name: String,
    existing_grid_hash: Option<String>,
    created: bool,
}

fn resolve_run(model_dir: &Path, run_base: &str, grid: &LatLonGrid) -> Result<ResolvedRun, String> {
    let mut candidates = Vec::new();
    if model_dir.is_dir() {
        for entry in std::fs::read_dir(model_dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == run_base || name.starts_with(&format!("{run_base}_")) {
                candidates.push(name);
            }
        }
    }
    candidates.sort();

    for name in &candidates {
        let grid_path = model_dir.join(name).join("grid.rwg");
        if !grid_path.is_file() {
            continue;
        }
        let existing = GridFile::open(&grid_path).map_err(|error| error.to_string())?;
        if existing.nx == grid.shape.nx
            && existing.ny == grid.shape.ny
            && coords_bit_identical(&existing.lat, &grid.lat_deg)
            && coords_bit_identical(&existing.lon, &grid.lon_deg)
        {
            return Ok(ResolvedRun {
                run_name: name.clone(),
                existing_grid_hash: Some(existing.hash),
                created: false,
            });
        }
    }

    let run_name = (1usize..)
        .map(|suffix| {
            if suffix == 1 {
                run_base.to_string()
            } else {
                format!("{run_base}_{suffix}")
            }
        })
        .find(|name| !candidates.contains(name))
        .ok_or_else(|| {
            format!(
                "could not allocate a run name below {}",
                model_dir.display()
            )
        })?;
    Ok(ResolvedRun {
        run_name,
        existing_grid_hash: None,
        created: true,
    })
}

fn coords_bit_identical(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

fn sanitize_store_token(value: &str) -> String {
    let sanitized = value
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use chrono::TimeZone;
    use rw_store::reader::HourReader;

    fn bounds() -> LonLatBounds {
        LonLatBounds {
            west_deg: -10.0,
            south_deg: 40.0,
            east_deg: 10.0,
            north_deg: 50.0,
        }
    }

    fn metadata(hour: u32, minute: u32) -> RgbSatelliteMetadata {
        RgbSatelliteMetadata {
            source_id: "eumetsat-mtg-wms".to_string(),
            provider: "eumetsat".to_string(),
            instrument: "fci".to_string(),
            satellite: "Meteosat-12 / MTI1".to_string(),
            model: "mtg-i1".to_string(),
            product_id: "eview-natural-colour".to_string(),
            product_title: "FCI natural colour".to_string(),
            sector: "europe".to_string(),
            scan_start_utc: Utc.with_ymd_and_hms(2026, 7, 11, hour, minute, 0).unwrap(),
            scan_end_utc: Utc
                .with_ymd_and_hms(2026, 7, 11, hour, minute + 9, 35)
                .unwrap(),
            extra_metadata: serde_json::json!({"collection": "EO:EUM:DAT:0662"}),
        }
    }

    fn image<'a>(rgb: &'a [u8]) -> RegularLonLatRgb<'a> {
        RegularLonLatRgb {
            width: 2,
            height: 2,
            bounds: bounds(),
            rgb,
            alpha: None,
        }
    }

    fn temp_store(tag: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bowecho-sat-rgb-{tag}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn regular_grid_uses_wms_pixel_centers_in_top_down_order() {
        let grid = regular_lonlat_grid(2, 2, bounds()).expect("grid");
        assert_eq!(grid.lon_deg, vec![-5.0, 5.0, -5.0, 5.0]);
        assert_eq!(grid.lat_deg, vec![47.5, 47.5, 42.5, 42.5]);
    }

    #[test]
    fn validation_rejects_bad_sizes_bounds_bytes_and_times() {
        let rgb = [0_u8; 12];
        let mut bad = image(&rgb);
        bad.width = 1;
        assert!(validate_input(bad, &metadata(12, 0)).is_err());

        let short = image(&rgb[..11]);
        assert!(validate_input(short, &metadata(12, 0)).is_err());

        let mut wrong_alpha = image(&rgb);
        wrong_alpha.alpha = Some(&[255, 0, 255]);
        assert!(validate_input(wrong_alpha, &metadata(12, 0)).is_err());

        let mut inverted = image(&rgb);
        inverted.bounds.east_deg = inverted.bounds.west_deg;
        assert!(validate_input(inverted, &metadata(12, 0)).is_err());

        let mut reversed_time = metadata(12, 0);
        reversed_time.scan_end_utc = reversed_time.scan_start_utc - chrono::Duration::seconds(1);
        assert!(validate_input(image(&rgb), &reversed_time).is_err());
    }

    #[test]
    fn writer_round_trips_rgb_and_groups_same_source_product_day() {
        let store = temp_store("roundtrip");
        let _ = std::fs::remove_dir_all(&store);
        let first_rgb = [1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let second_rgb = [21_u8, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32];

        let first = write_regular_lonlat_rgb_frame(&store, image(&first_rgb), &metadata(12, 0), 1)
            .expect("first frame");
        let second =
            write_regular_lonlat_rgb_frame(&store, image(&second_rgb), &metadata(12, 10), 2)
                .expect("second frame");

        assert!(first.created_run);
        assert!(!second.created_run);
        assert_eq!(first.model, "mtg_i1");
        assert_eq!(first.run, second.run);
        assert!(first.run.contains("_rgb_"));
        assert!(first.path.is_file());
        assert!(second.path.is_file());

        let grid = GridFile::open(&store.join(&first.model).join(&first.run).join("grid.rwg"))
            .expect("stored grid");
        assert_eq!(grid.lat, vec![47.5, 47.5, 42.5, 42.5]);
        assert_eq!(grid.lon, vec![-5.0, 5.0, -5.0, 5.0]);

        let reader = HourReader::open(&first.path).expect("stored hour");
        assert_eq!(
            reader.read_full_2d(RGB_R_VAR).expect("red"),
            vec![1.0, 4.0, 7.0, 10.0]
        );
        assert_eq!(
            reader.read_full_2d(RGB_G_VAR).expect("green"),
            vec![2.0, 5.0, 8.0, 11.0]
        );
        assert_eq!(
            reader.read_full_2d(RGB_B_VAR).expect("blue"),
            vec![3.0, 6.0, 9.0, 12.0]
        );
        let red = reader
            .meta()
            .variables
            .iter()
            .find(|variable| variable.name == RGB_R_VAR)
            .expect("red metadata");
        assert_eq!(red.selector["satellite"]["provider"], "eumetsat");
        assert_eq!(
            red.selector["satellite"]["projection"]["row_order"],
            "north_to_south"
        );

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn alpha_zero_round_trips_as_nan_in_every_rgb_plane() {
        let store = temp_store("alpha");
        let _ = std::fs::remove_dir_all(&store);
        let rgb = [1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        // Pixel 1 and 3 are transparent. Partial alpha is intentionally kept
        // opaque because the shared baked-RGB frame has no fractional alpha.
        let alpha = [255_u8, 0, 128, 0];
        let mut rgba = image(&rgb);
        rgba.alpha = Some(&alpha);
        let frame =
            write_regular_lonlat_rgb_frame(&store, rgba, &metadata(13, 0), 1).expect("alpha frame");
        let reader = HourReader::open(&frame.path).expect("stored hour");
        let r = reader.read_full_2d(RGB_R_VAR).expect("red");
        let g = reader.read_full_2d(RGB_G_VAR).expect("green");
        let b = reader.read_full_2d(RGB_B_VAR).expect("blue");

        assert_eq!((r[0], g[0], b[0]), (1.0, 2.0, 3.0));
        assert!(r[1].is_nan() && g[1].is_nan() && b[1].is_nan());
        assert_eq!((r[2], g[2], b[2]), (7.0, 8.0, 9.0));
        assert!(r[3].is_nan() && g[3].is_nan() && b[3].is_nan());

        let red = reader
            .meta()
            .variables
            .iter()
            .find(|variable| variable.name == RGB_R_VAR)
            .expect("red metadata");
        assert_eq!(
            red.selector["satellite"]["metadata"]["alpha"]["present"],
            true
        );
        assert_eq!(
            red.selector["satellite"]["metadata"]["alpha"]["nonzero_policy"],
            "opaque"
        );

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn changed_wms_grid_gets_a_suffixed_sibling_run() {
        let store = temp_store("grid-change");
        let _ = std::fs::remove_dir_all(&store);
        let rgb = [0_u8; 12];
        let first = write_regular_lonlat_rgb_frame(&store, image(&rgb), &metadata(12, 0), 1)
            .expect("first frame");
        let mut shifted = image(&rgb);
        shifted.bounds.west_deg = -9.0;
        let second = write_regular_lonlat_rgb_frame(&store, shifted, &metadata(12, 10), 2)
            .expect("shifted frame");

        assert_ne!(first.run, second.run);
        assert_eq!(second.run, format!("{}_2", first.run));
        assert!(second.created_run);

        let _ = std::fs::remove_dir_all(&store);
    }
}
