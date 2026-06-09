// Render native vs smoothed reflectivity PNGs for visual comparison.
// usage: smooth_probe <l2-file> <out-dir>
use color_tables::{ColorTableFamily, ColorTableSet};
use image::{ImageBuffer, Rgba};
use radar_core::{MomentType, RadarVolume};
use render2d::{
    ViewportMomentCache, ViewportRasterOptions, smooth_moment_grid, viewport_rgba_buffer_len,
};
use std::time::Instant;

fn save(
    volume: &RadarVolume,
    cache: &ViewportMomentCache,
    options: ViewportRasterOptions,
    path: &str,
) {
    let mut px = vec![0u8; viewport_rgba_buffer_len(options)];
    let (w, h) = cache
        .render_moment_rgba_into(volume, options, &mut px)
        .expect("render");
    let mut img = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(w, h, Rgba([15, 17, 20, 255]));
    for (i, p) in px.chunks_exact(4).enumerate() {
        let a = p[3] as u32;
        if a == 0 {
            continue;
        }
        let (x, y) = (i as u32 % w, i as u32 / w);
        let bg = img.get_pixel(x, y).0;
        let bl = |c: u8, b: u8| ((c as u32 * a + b as u32 * (255 - a)) / 255) as u8;
        img.put_pixel(
            x,
            y,
            Rgba([bl(p[0], bg[0]), bl(p[1], bg[1]), bl(p[2], bg[2]), 255]),
        );
    }
    img.save(path).expect("save");
    println!("wrote {path}");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = args.next().ok_or("usage: smooth_probe <l2> <dir>")?;
    let dir = args
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into());
    let volume: RadarVolume = nexrad_io::decode_volume_from_path(input.as_ref())?;
    let tables = ColorTableSet::default();
    // Zoomed view (~±60 km) where smoothing is most visible.
    let options = ViewportRasterOptions {
        width: 900,
        height: 900,
        radar_x_px: 450.0,
        radar_y_px: 450.0,
        km_per_px_x: 0.135,
        km_per_px_y: 0.135,
    };
    let cut = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Reflectivity))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
        .map(|(i, _)| i)
        .ok_or("no REF")?;

    let native = ViewportMomentCache::new_with_color_tables(
        &volume,
        cut,
        MomentType::Reflectivity,
        &tables,
    )?;
    save(
        &volume,
        &native,
        options,
        &format!("{dir}/smooth_native.png"),
    );

    let grid = volume.cuts[cut]
        .moments
        .get(&MomentType::Reflectivity)
        .unwrap();
    let start = Instant::now();
    let smoothed_grid = smooth_moment_grid(grid);
    println!(
        "smoothing pass: {:.1} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    let smoothed = ViewportMomentCache::new_derived(
        &volume,
        cut,
        smoothed_grid,
        ColorTableFamily::Reflectivity,
        &tables,
    )?;
    save(&volume, &smoothed, options, &format!("{dir}/smooth_on.png"));
    Ok(())
}
