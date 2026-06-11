//! Decode one or more DORADE sweepfiles and print volume geometry.
//!
//! Usage: cargo run -p nexrad_io --example dorade_probe -- swp.file [swp.file...]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
    if paths.is_empty() {
        eprintln!("usage: dorade_probe <swp.file> [swp.file...]");
        std::process::exit(2);
    }

    let volume = nexrad_io::dorade::decode_dorade_volume_from_paths(&paths)?;
    println!(
        "site {} ({:?}) lat {:?} lon {:?} alt {:?} m",
        volume.site.id,
        volume.site.name,
        volume.site.latitude_deg,
        volume.site.longitude_deg,
        volume.site.elevation_m
    );
    println!(
        "volume time {} | {} cuts | compression {:?} | skipped {}",
        volume.volume_time,
        volume.cuts.len(),
        volume.metadata.compression,
        volume.metadata.skipped_message_count
    );
    for (index, cut) in volume.cuts.iter().enumerate() {
        println!(
            "  cut {index}: elev {:.2} deg, {} radials, nyquist {:?}",
            cut.elevation_deg,
            cut.radials.len(),
            cut.radials.first().and_then(|r| r.nyquist_velocity_mps),
        );
        for (moment, grid) in &cut.moments {
            let mut finite = 0usize;
            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            let rows = grid.radial_count();
            for row in 0..rows {
                for gate in 0..grid.gate_range.gate_count {
                    if let Some(value) = grid.scaled_value(row, gate) {
                        finite += 1;
                        min = min.min(value);
                        max = max.max(value);
                    }
                }
            }
            println!(
                "    {moment}: {} rows x {} gates (first {} m, spacing {} m), {} finite, range [{min:.2}, {max:.2}]",
                rows,
                grid.gate_range.gate_count,
                grid.gate_range.first_gate_m,
                grid.gate_range.gate_spacing_m,
                finite
            );
        }
    }
    Ok(())
}
