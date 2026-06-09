// Sanity-check rotation detection on a real volume: prints detected sites.
// usage: rotation_probe <l2-file>
use radar_core::{MomentType, RadarVolume};
use render2d::detect_rotation_sites;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: rotation_probe <l2-file>")?;
    let volume: RadarVolume = nexrad_io::decode_volume_from_path(path.as_ref())?;
    let Some((index, cut)) = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Velocity))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
    else {
        return Err("no velocity".into());
    };
    let velocity = cut.moments.get(&MomentType::Velocity).unwrap();
    let start = Instant::now();
    let sites = detect_rotation_sites(cut, velocity);
    println!(
        "cut #{index} elev {:.2} -> {} sites in {:.0} ms",
        cut.elevation_deg,
        sites.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    for site in sites.iter().take(8) {
        println!(
            "  az {:6.1}  rng {:6.1} km  shear {:+.4}/s  gates {:3}  tvs {}",
            site.azimuth_deg,
            site.ground_range_m / 1000.0,
            site.peak_shear_s,
            site.gate_count,
            site.tvs
        );
    }
    Ok(())
}
