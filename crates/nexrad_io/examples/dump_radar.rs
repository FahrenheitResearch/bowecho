//! Dump a decoded radar volume (any supported local format) as comparable
//! text — the validation harness for real-file golden checks.
//!
//! Mirrors the app's file-open routing (`sniff_local_radar_kind` in app_ui):
//! magic bytes pick the decoder (zip / DORADE / HDF5-ODIM / netCDF3-CfRadial
//! / Archive II), then the volume prints site, per-cut geometry, and a fixed
//! set of sampled gate values per moment. An independent Python reader
//! (h5py / netCDF4) emits the same sample positions so the two outputs can
//! be diffed mechanically — the golden-fixture discipline used for DORADE.
//!
//! Usage: cargo run -p nexrad_io --example dump_radar -- <file>

use std::path::{Path, PathBuf};

use radar_core::{MomentStorage, RadarVolume};

fn main() {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: cargo run -p nexrad_io --example dump_radar -- <radar-file>");
        std::process::exit(2);
    };

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("read {}: {err}", path.display());
            std::process::exit(1);
        }
    };

    let (kind, result) = decode_like_the_app(&path, &bytes);
    println!("kind: {kind}");
    let volume = match result {
        Ok(volume) => volume,
        Err(err) => {
            eprintln!("decode failed: {err}");
            std::process::exit(1);
        }
    };
    dump(&volume);
}

/// Magic-byte routing in the same precedence order as
/// `app_ui::sniff_local_radar_kind` (zip > DORADE > HDF5 > CDF > Archive II).
fn decode_like_the_app(path: &Path, bytes: &[u8]) -> (&'static str, Result<RadarVolume, String>) {
    let head = &bytes[..bytes.len().min(8)];
    if nexrad_io::mobile_archive::looks_like_zip_bytes(head) {
        let result = nexrad_io::mobile_archive::decode_mobile_archive_from_path(path)
            .map_err(|err| err.to_string())
            .and_then(|mut volumes| {
                if volumes.is_empty() {
                    Err("archive holds no volumes".to_owned())
                } else {
                    Ok(volumes.remove(0).volume)
                }
            });
        return ("MobileArchive", result);
    }
    if nexrad_io::dorade::looks_like_dorade_bytes(head) {
        return (
            "DoradeSweep",
            nexrad_io::dorade::decode_dorade_sweep_volume(bytes).map_err(|err| err.to_string()),
        );
    }
    if nexrad_io::odim::looks_like_hdf5_bytes(head) {
        return (
            "OdimH5",
            nexrad_io::odim::decode_odim_h5_volume(bytes).map_err(|err| err.to_string()),
        );
    }
    if nexrad_io::cfradial::looks_like_netcdf3_bytes(head) {
        return (
            "CfRadial",
            nexrad_io::cfradial::decode_cfradial1_volume(bytes).map_err(|err| err.to_string()),
        );
    }
    (
        "NexradLevel2",
        nexrad_io::decode_volume_from_bytes(bytes).map_err(|err| err.to_string()),
    )
}

fn dump(volume: &RadarVolume) {
    println!(
        "site: id={} name={} lat={} lon={} elev_m={}",
        volume.site.id,
        volume.site.name.as_deref().unwrap_or("-"),
        fmt_opt(volume.site.latitude_deg),
        fmt_opt(volume.site.longitude_deg),
        fmt_opt(volume.site.elevation_m),
    );
    println!("time: {}", volume.volume_time.format("%Y-%m-%dT%H:%M:%SZ"));
    println!(
        "scan_mode: {:?} cuts={} radials={}",
        volume.metadata.scan_mode,
        volume.cuts.len(),
        volume.metadata.decoded_radial_count
    );

    for (index, cut) in volume.cuts.iter().enumerate() {
        let first = cut.radials.first();
        println!(
            "cut {index} elev={:.3} radials={} az0={} el0={} nyq0={} gate0={} spacing={} gates={}",
            cut.elevation_deg,
            cut.radials.len(),
            fmt_opt(first.map(|radial| radial.azimuth_deg)),
            fmt_opt(first.map(|radial| radial.elevation_deg)),
            fmt_opt(first.and_then(|radial| radial.nyquist_velocity_mps)),
            first.map(|r| r.gate_range.first_gate_m).unwrap_or(0),
            first.map(|r| r.gate_range.gate_spacing_m).unwrap_or(0),
            first.map(|r| r.gate_range.gate_count).unwrap_or(0),
        );
        for (moment, grid) in &cut.moments {
            let rows = grid.radial_count();
            let bins = grid.gate_range.gate_count;
            let storage = match &grid.storage {
                MomentStorage::U8(_) => "u8",
                MomentStorage::U16(_) => "u16",
                MomentStorage::F32(_) => "f32",
            };
            println!(
                "  moment {} storage={storage} rows={rows} bins={bins}",
                moment.short_name()
            );
            if rows == 0 || bins == 0 {
                continue;
            }
            for (ray, bin) in sample_positions(rows, bins) {
                println!(
                    "    v[{ray},{bin}]={}",
                    match grid.scaled_value(ray, bin) {
                        Some(value) if value.is_nan() => "None".to_owned(),
                        Some(value) => format!("{value:.4}"),
                        None => "None".to_owned(),
                    }
                );
            }
        }
    }
}

/// The fixed sample set shared with the Python reference reader.
fn sample_positions(rows: usize, bins: usize) -> [(usize, usize); 5] {
    [
        (0, 0),
        (0, bins / 2),
        (rows / 4, bins / 3),
        (rows / 2, 10.min(bins - 1)),
        (rows - 1, bins - 1),
    ]
}

fn fmt_opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|v| DisplayF(v).to_string())
        .unwrap_or_else(|| "None".to_owned())
}

/// Format helper: floats to 4 decimals, everything else via Display.
struct DisplayF<T>(T);

impl<T: std::fmt::Display> std::fmt::Display for DisplayF<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = self.0.to_string();
        match text.parse::<f64>() {
            Ok(value) => write!(f, "{value:.4}"),
            Err(_) => f.write_str(&text),
        }
    }
}
