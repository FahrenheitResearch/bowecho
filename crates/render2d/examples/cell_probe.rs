// Identify storm cells on real volumes: count, positions, timing.
// usage: cell_probe <l2-file> [...]
use radar_core::RadarVolume;
use render2d::identify_storm_cells;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for path in std::env::args().skip(1) {
        let volume: RadarVolume =
            nexrad_io::decode_volume_from_path(path.as_ref() as &std::path::Path)?;
        let start = Instant::now();
        let cells = identify_storm_cells(&volume);
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{} -> {} cells in {ms:.1} ms",
            path.rsplit(['/', '\\']).next().unwrap_or(&path),
            cells.len()
        );
        for cell in cells.iter().take(8) {
            println!(
                "   ({:7.1}, {:7.1}) km  {:4.1} dBZ  {:6.1} km2  r_eq {:4.1}",
                cell.east_km, cell.north_km, cell.max_dbz, cell.area_km2, cell.eq_radius_km
            );
        }
    }
    Ok(())
}
