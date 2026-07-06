//! Build a max-REF swath from a real loop of Level II volumes and render it
//! (plus the newest single frame, for comparison) to PNG.
//!
//! Usage:
//!   cargo run -p render2d --example max_swath_png -- <out_dir> <file1> <file2> ...
//!
//! The swath PNG should show a BROADER reflectivity footprint than the single
//! newest frame — the union of where the storm has been across the loop.

use std::path::PathBuf;

use radar_core::MomentType;
use render2d::{RasterOptions, SwathAggregation, base_tilt_cut, max_value_swath, render_moment_png};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(out_dir) = args.next().map(PathBuf::from) else {
        eprintln!("usage: max_swath_png <out_dir> <vol1> <vol2> ...");
        std::process::exit(2);
    };
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    let paths: Vec<PathBuf> = args.map(PathBuf::from).collect();
    if paths.is_empty() {
        eprintln!("no input volumes");
        std::process::exit(2);
    }

    let mut volumes = Vec::new();
    for path in &paths {
        match nexrad_io::decode_volume_from_path(path) {
            Ok(volume) => {
                println!(
                    "decoded {} -> {} cuts, {} @ {}",
                    path.display(),
                    volume.cuts.len(),
                    volume.site.id,
                    volume.volume_time
                );
                volumes.push(volume);
            }
            Err(err) => eprintln!("decode {} failed: {err}", path.display()),
        }
    }
    if volumes.is_empty() {
        eprintln!("nothing decoded");
        std::process::exit(1);
    }

    let refs: Vec<&radar_core::RadarVolume> = volumes.iter().collect();
    let options = RasterOptions {
        width: 1200,
        height: 1200,
        range_fraction: 96,
    };

    // Newest single frame, base reflectivity — the "current scan" reference.
    let newest = refs
        .iter()
        .max_by_key(|v| v.volume_time)
        .copied()
        .unwrap();
    if let Some(cut) = base_tilt_cut(newest, &MomentType::Reflectivity) {
        let out = out_dir.join("single_frame_ref.png");
        render_moment_png(newest, cut, MomentType::Reflectivity, &out, options).unwrap();
        println!("wrote {}", out.display());
    }

    // Max-REF swath over the whole loop.
    let swath = max_value_swath(&refs, MomentType::Reflectivity, SwathAggregation::Max)
        .expect("swath");
    report_coverage("REF swath", &swath, &MomentType::Reflectivity);
    let out = out_dir.join("max_ref_swath.png");
    render_moment_png(&swath, 0, MomentType::Reflectivity, &out, options).unwrap();
    println!("wrote {}", out.display());

    // Max-|V| swath (second toggle).
    if let Some(swath) =
        max_value_swath(&refs, MomentType::Velocity, SwathAggregation::MaxMagnitude)
    {
        report_coverage("|V| swath", &swath, &MomentType::Velocity);
        let out = out_dir.join("max_vel_swath.png");
        render_moment_png(&swath, 0, MomentType::Velocity, &out, options).unwrap();
        println!("wrote {}", out.display());
    }
}

/// Print how many gates carry a finite value — a swath should cover far more
/// than any single frame.
fn report_coverage(label: &str, volume: &radar_core::RadarVolume, moment: &MomentType) {
    let grid = &volume.cuts[0].moments[moment];
    let (mut finite, mut total) = (0usize, 0usize);
    for row in 0..grid.radial_count() {
        for gate in 0..grid.gate_range.gate_count {
            total += 1;
            if grid.scaled_value(row, gate).is_some_and(|v| v.is_finite()) {
                finite += 1;
            }
        }
    }
    println!(
        "{label}: {finite}/{total} gates finite ({:.1}%), {} rows x {} gates",
        100.0 * finite as f64 / total.max(1) as f64,
        grid.radial_count(),
        grid.gate_range.gate_count
    );
}
