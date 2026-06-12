//! TOR TRACKS validation probe: feed a sequence of Level II volumes, build
//! the rotation-tracks max-composite (render2d::tracks), report per-frame
//! coverage / TDS counts, and write the accumulated swath as a PNG using the
//! display ramp (transparent below 0.003 s^-1 on black for inspection).
//!
//! Usage: cargo run -p render2d --example tor_tracks_probe --release -- \
//!   C:\path\to\KXXX...V06 [more volumes ...] [--png out.png]
//!
//! References: Mahalik et al. 2019 (WAF 34, doi:10.1175/WAF-D-18-0165.1);
//! Miller et al. 2013 (28th Conf. IIPS); Smith et al. 2016 (BAMS 97,
//! doi:10.1175/BAMS-D-14-00173.1).

use render2d::tracks::{
    TracksGridSpec, detect_tds_gates, low_level_azshear_cartesian, max_composite_into,
    rotation_track_color,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut paths = Vec::new();
    let mut png_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--png" {
            png_path = args.next();
        } else {
            paths.push(arg);
        }
    }
    if paths.is_empty() {
        eprintln!("usage: tor_tracks_probe <volume> [volume ...] [--png out.png]");
        std::process::exit(2);
    }

    let spec = TracksGridSpec::default();
    let size = spec.size();
    let mut accumulator = vec![f32::NAN; spec.cell_count()];
    println!(
        "grid: {size}x{size} cells, {:.1} km half-extent, {:.2} km cells",
        spec.half_extent_km, spec.cell_km
    );

    for path in &paths {
        let raw = std::fs::read(path)?;
        let volume = nexrad_io::decode_volume_from_bytes(&raw)?;
        let start = std::time::Instant::now();
        let frame = low_level_azshear_cartesian(&volume, &spec);
        let frame_ms = start.elapsed().as_secs_f32() * 1000.0;
        let sites = render2d::detect_rotation_sites(&volume);
        let tds = detect_tds_gates(&volume, &sites);
        max_composite_into(&mut accumulator, &frame);

        let finite = frame.iter().filter(|v| v.is_finite()).count();
        let display = frame
            .iter()
            .filter(|v| v.is_finite() && **v >= render2d::tracks::TRACK_DISPLAY_FLOOR_E3)
            .count();
        let peak = frame
            .iter()
            .filter(|v| v.is_finite())
            .fold(0.0f32, |acc, &v| acc.max(v));
        println!(
            "{}: {} covered cells, {} display-strength, peak {:.1}e-3/s, {} rot sites, {} TDS gates ({frame_ms:.0} ms)",
            volume.volume_time.format("%H:%M:%SZ"),
            finite,
            display,
            peak,
            sites.len(),
            tds.len()
        );
        for site in sites.iter().take(4) {
            println!(
                "    site az={:5.1} rng={:5.1}km vrot={:4.1} rank={} {:?}",
                site.azimuth_deg,
                site.ground_range_m / 1000.0,
                site.vrot_mps,
                site.rank,
                site.strength
            );
        }
    }

    let display = accumulator
        .iter()
        .filter(|v| v.is_finite() && **v >= render2d::tracks::TRACK_DISPLAY_FLOOR_E3)
        .count();
    let peak = accumulator
        .iter()
        .filter(|v| v.is_finite())
        .fold(0.0f32, |acc, &v| acc.max(v));
    println!(
        "ACCUMULATED: {display} display-strength cells, peak {peak:.1}e-3/s over {} frames",
        paths.len()
    );

    if let Some(out) = png_path {
        let mut image = image::RgbaImage::new(size as u32, size as u32);
        for (index, pixel) in image.pixels_mut().enumerate() {
            let [r, g, b, a] = rotation_track_color(accumulator[index]);
            // Composite over black so the swath is inspectable standalone.
            let alpha = a as f32 / 255.0;
            *pixel = image::Rgba([
                (r as f32 * alpha) as u8,
                (g as f32 * alpha) as u8,
                (b as f32 * alpha) as u8,
                255,
            ]);
        }
        image.save(&out)?;
        println!("wrote {out}");
    }
    Ok(())
}
