// Verify azimuthal shear on a real scan: compute LLSD az-shear on the lowest
// velocity tilt and render it (velocity diverging palette) so rotational
// couplets show as green/red dipoles. usage: shear_probe <l2-file> <out.png>

use std::path::PathBuf;

use image::{ImageBuffer, Rgba};
use radar_core::{MomentType, RadarVolume};
use render2d::{RasterOptions, azimuthal_shear_grid, render_moment_image};

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
    out.save(path).expect("save");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(
        args.next()
            .ok_or("usage: shear_probe <l2-file> <out.png>")?,
    );
    let out = args
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shear.png".into());
    let mut volume: RadarVolume = nexrad_io::decode_volume_from_path(&input)?;

    let idx = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Velocity))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
        .map(|(i, _)| i)
        .ok_or("no velocity")?;

    let shear = {
        let cut = &volume.cuts[idx];
        azimuthal_shear_grid(cut, cut.moments.get(&MomentType::Velocity).unwrap())
    };
    let (mut n, mut maxabs) = (0u64, 0.0f32);
    for r in 0..shear.radial_count() {
        for g in 0..shear.gate_range.gate_count {
            if let Some(v) = shear.scaled_value(r, g).filter(|v| v.is_finite()) {
                n += 1;
                maxabs = maxabs.max(v.abs());
            }
        }
    }
    println!("az-shear cut #{idx}: n={n} max|shear|={maxabs:.1} x10^-3 s^-1");

    // Render via the velocity diverging palette (insert shear as Velocity).
    volume.cuts[idx].moments.insert(MomentType::Velocity, shear);
    let opts = RasterOptions {
        width: 1400,
        height: 1400,
        range_fraction: 60,
    };
    let img = render_moment_image(&volume, idx, MomentType::Velocity, opts)?;
    save_on_black(&img, &out);
    println!("wrote {out}");
    Ok(())
}
