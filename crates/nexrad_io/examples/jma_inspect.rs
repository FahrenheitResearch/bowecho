//! Print a decoded JMA radar GRIB2 tar in the exact text format of the
//! `jma-radar-bridge` crate's `inspect` subcommand, so the two decoders can
//! be cross-validated with a plain `diff` (station ids, sweep counts, gate
//! and radial counts, ranges, non-missing counts).
//!
//! With `--samples`, prints fixed-position sampled gate values instead
//! (the same positions a sibling harness prints from the bridge's own
//! `Sweep::value`), for gate-for-gate value comparison.
//!
//! Usage: cargo run -p nexrad_io --example jma_inspect -- <tar> [--samples]

use radar_core::{ElevationCut, MomentStorage, MomentType};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: cargo run -p nexrad_io --example jma_inspect -- <jma-tar> [--samples]");
        std::process::exit(2);
    };
    let samples = matches!(args.next().as_deref(), Some("--samples"));

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("read {path}: {err}");
            std::process::exit(1);
        }
    };
    let volumes = match nexrad_io::jma::decode_jma_tar_volumes(&bytes, None) {
        Ok(volumes) => volumes,
        Err(err) => {
            eprintln!("decode failed: {err}");
            std::process::exit(1);
        }
    };

    for volume in &volumes {
        if samples {
            for (index, cut) in volume.cuts.iter().enumerate() {
                print_cut_samples(&volume.site.id, index, cut);
            }
        } else {
            println!(
                "{}  station={}  sweeps={}",
                volume.volume_time.format("%Y-%m-%dT%H:%M:%SZ"),
                volume.site.id,
                volume.cuts.len()
            );
            for (index, cut) in volume.cuts.iter().enumerate() {
                print_cut_inspect_line(index, cut);
            }
        }
    }
}

/// Mirror of the bridge's per-sweep `inspect` line.
fn print_cut_inspect_line(index: usize, cut: &ElevationCut) {
    let Some((moment, grid)) = cut.moments.iter().next() else {
        return;
    };
    let gates = grid.gate_range.gate_count;
    let rays = cut.radials.len();
    let max_range_m =
        grid.gate_range.first_gate_m as f32 + gates as f32 * grid.gate_range.gate_spacing_m as f32;
    let non_missing = match &grid.storage {
        MomentStorage::F32(values) => values.iter().filter(|value| !value.is_nan()).count(),
        other => other.len(),
    };
    println!(
        "  sweep {index:02} {:>3} elev={:>5.2} deg gates={gates} rays={rays} range={:.1} km non-missing={non_missing}",
        short3(moment),
        cut.elevation_deg,
        max_range_m / 1000.0,
    );
}

/// Fixed sample positions shared with the bridge-side harness.
fn print_cut_samples(station: &str, index: usize, cut: &ElevationCut) {
    let Some((_, grid)) = cut.moments.iter().next() else {
        return;
    };
    let gates = grid.gate_range.gate_count;
    let rays = cut.radials.len();
    if gates == 0 || rays == 0 {
        return;
    }
    for (ray, gate) in [
        (0, 0),
        (0, gates / 2),
        (rays / 4, gates / 3),
        (rays / 2, 10.min(gates - 1)),
        (rays - 1, gates - 1),
    ] {
        let value = match grid.scaled_value(ray, gate) {
            Some(value) if value.is_nan() => "NaN".to_owned(),
            Some(value) => format!("{value:.4}"),
            None => "NaN".to_owned(),
        };
        println!("{station} s{index:02} v[{ray},{gate}]={value}");
    }
}

fn short3(moment: &MomentType) -> &'static str {
    match moment {
        MomentType::Reflectivity => "REF",
        MomentType::Velocity => "VEL",
        _ => "UNK",
    }
}
