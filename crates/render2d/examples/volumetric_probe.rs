// Verify volume-derived products on a real scan: render composite reflectivity
// to PNG (REF palette) and print numeric stats for composite / echo-top / VIL.
//
// usage: cargo run --release -p render2d --example volumetric_probe -- <l2-file> <out-prefix>

use std::path::PathBuf;

use image::{ImageBuffer, Rgba};
use radar_core::{MomentType, RadarVolume};
use render2d::{
    ECHO_TOP_THRESHOLD_DBZ, RasterOptions, composite_reflectivity_grid, echo_top_grid,
    render_moment_image, vil_grid,
};

fn stats(grid: &radar_core::MomentGrid, label: &str, scale: f32, unit: &str) {
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    let mut n = 0u64;
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f64;
    for r in 0..rows {
        for g in 0..gates {
            if let Some(v) = grid.scaled_value(r, g).filter(|v| v.is_finite()) {
                n += 1;
                min = min.min(v);
                max = max.max(v);
                sum += v as f64;
            }
        }
    }
    if n == 0 {
        println!("{label}: no data");
        return;
    }
    println!(
        "{label}: n={n} min={:.1}{unit} max={:.1}{unit} mean={:.1}{unit}",
        min * scale,
        max * scale,
        (sum / n as f64) as f32 * scale,
    );
}

fn save_on_black(img: &ImageBuffer<Rgba<u8>, Vec<u8>>, path: &str) {
    let (w, h) = img.dimensions();
    let mut out = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(w, h, Rgba([16, 16, 18, 255]));
    for (x, y, px) in img.enumerate_pixels() {
        let a = px.0[3] as u32;
        if a == 0 {
            continue;
        }
        let bg = out.get_pixel(x, y).0;
        let bl = |c: u8, b: u8| ((c as u32 * a + b as u32 * (255 - a)) / 255) as u8;
        out.put_pixel(
            x,
            y,
            Rgba([
                bl(px.0[0], bg[0]),
                bl(px.0[1], bg[1]),
                bl(px.0[2], bg[2]),
                255,
            ]),
        );
    }
    out.save(path).expect("save png");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(
        args.next()
            .ok_or("usage: volumetric_probe <l2-file> <prefix>")?,
    );
    let prefix = args
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "vol".into());

    let mut volume: RadarVolume = nexrad_io::decode_volume_from_path(&input)?;

    let base_idx = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Reflectivity))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
        .map(|(i, _)| i)
        .ok_or("no reflectivity")?;
    println!(
        "site={} time={} base REF cut=#{base_idx} cuts={}",
        volume.site.id,
        volume.volume_time,
        volume.cuts.len()
    );

    use std::time::Instant;
    let t = Instant::now();
    let composite = composite_reflectivity_grid(&volume).ok_or("composite failed")?;
    let t_comp = t.elapsed();
    let t = Instant::now();
    let echo = echo_top_grid(&volume, ECHO_TOP_THRESHOLD_DBZ).ok_or("echo failed")?;
    let t_echo = t.elapsed();
    let t = Instant::now();
    let vil = vil_grid(&volume).ok_or("vil failed")?;
    let t_vil = t.elapsed();
    println!(
        "timing: composite={:.1}ms echo_top={:.1}ms vil={:.1}ms",
        t_comp.as_secs_f64() * 1e3,
        t_echo.as_secs_f64() * 1e3,
        t_vil.as_secs_f64() * 1e3
    );

    stats(&composite, "composite", 1.0, " dBZ");
    // echo tops stored in metres above radar -> report kft.
    stats(&echo, "echo_top", 0.003_280_84, " kft");
    stats(&vil, "vil", 1.0, " kg/m2");

    // Render composite with the reflectivity palette for visual verification.
    volume.cuts[base_idx]
        .moments
        .insert(MomentType::Reflectivity, composite);
    let opts = RasterOptions {
        width: 1400,
        height: 1400,
        range_fraction: 80,
    };
    let img = render_moment_image(&volume, base_idx, MomentType::Reflectivity, opts)?;
    let path = format!("{prefix}_composite.png");
    save_on_black(&img, &path);
    println!("wrote {path}");
    Ok(())
}
