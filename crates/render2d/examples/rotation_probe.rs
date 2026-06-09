// Sanity-check rotation detection on a real volume: prints detected sites.
// usage: rotation_probe <l2-file>
use radar_core::RadarVolume;
use render2d::{detect_rotation_sites, rotation_features_per_tilt};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: rotation_probe <l2-file>")?;
    let volume: RadarVolume = nexrad_io::decode_volume_from_path(path.as_ref())?;
    for (elev, count, best) in rotation_features_per_tilt(&volume) {
        println!("tilt {elev:5.2}: {count} 2D features, best rank {best}");
    }
    let start = Instant::now();
    let sites = detect_rotation_sites(&volume);
    println!(
        "{} site(s) in {:.0} ms",
        sites.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    for site in &sites {
        println!(
            "  {:?} R{} az {:6.1} rng {:6.1} km Vrot {:4.1} m/s GTG {:4.1} depth {} tilts / {:.1} km",
            site.strength,
            site.rank,
            site.azimuth_deg,
            site.ground_range_m / 1000.0,
            site.vrot_mps,
            site.gate_to_gate_dv_mps,
            site.depth_tilts,
            site.depth_m / 1000.0,
        );
    }
    Ok(())
}
