// Time dealias_velocity_grid on every velocity cut of a volume.
// usage: cargo run --release -p render2d --example velocity_bench -- <l2-file>

use std::path::PathBuf;
use std::time::Instant;

use radar_core::{MomentType, RadarVolume};
use render2d::dealias_velocity_grid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: velocity_bench <l2-file>")?,
    );
    let volume: RadarVolume = nexrad_io::decode_volume_from_path(&input)?;

    let mut total = std::time::Duration::ZERO;
    let mut total_gates = 0usize;
    for (idx, cut) in volume.cuts.iter().enumerate() {
        let Some(grid) = cut.moments.get(&MomentType::Velocity) else {
            continue;
        };
        let gates = grid.radial_count() * grid.gate_range.gate_count;
        // warm + best-of-5 to get a stable per-cut number
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let out = dealias_velocity_grid(cut, grid);
            let dt = t.elapsed();
            std::hint::black_box(&out);
            best = best.min(dt);
        }
        total += best;
        total_gates += gates;
        println!(
            "cut#{idx:<2} elev={:>4.2} {}x{} = {gates:>7} gates  ->  {:>6.2} ms  ({:.1} Mgate/s)",
            cut.elevation_deg,
            grid.radial_count(),
            grid.gate_range.gate_count,
            best.as_secs_f64() * 1e3,
            gates as f64 / best.as_secs_f64() / 1e6,
        );
    }
    println!(
        "\nTOTAL volume dealias (best-of-5 per cut): {:.2} ms over {} gates ({:.1} Mgate/s)",
        total.as_secs_f64() * 1e3,
        total_gates,
        total_gates as f64 / total.as_secs_f64() / 1e6,
    );
    Ok(())
}
