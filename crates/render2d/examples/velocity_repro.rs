// Reproduction harness for velocity dealias spokes + color-table edge cases.
// Renders raw velocity and dealiased velocity (current algorithm) to PNGs so
// the radial spoke artifacts are directly visible.
//
// usage: cargo run --release -p render2d --example velocity_repro -- <level2-file> <out-prefix>

use std::path::PathBuf;

use image::{ImageBuffer, Rgba};
use radar_core::{MomentType, RadarVolume};
use render2d::{RasterOptions, dealias_velocity_grid, render_moment_image};

/// Composite an RGBA image over a dark background (radar displays are black)
/// and save, so near-white strong velocities are visible.
fn save_on_black(
    img: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (w, h) = img.dimensions();
    let mut out = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(w, h, Rgba([16, 16, 18, 255]));
    for (x, y, px) in img.enumerate_pixels() {
        let a = px.0[3] as u32;
        if a == 0 {
            continue;
        }
        let bg = out.get_pixel(x, y).0;
        let blend = |c: u8, b: u8| ((c as u32 * a + b as u32 * (255 - a)) / 255) as u8;
        out.put_pixel(
            x,
            y,
            Rgba([
                blend(px.0[0], bg[0]),
                blend(px.0[1], bg[1]),
                blend(px.0[2], bg[2]),
                255,
            ]),
        );
    }
    out.save(path)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(
        args.next()
            .ok_or("usage: velocity_repro <level2-file> <prefix>")?,
    );
    let prefix = args
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "velrepro".to_string());

    let mut volume: RadarVolume = nexrad_io::decode_volume_from_path(&input)?;

    // Lowest-elevation cut that actually carries velocity.
    let mut chosen: Option<(usize, f32)> = None;
    for (idx, cut) in volume.cuts.iter().enumerate() {
        if cut.moments.contains_key(&MomentType::Velocity) {
            match chosen {
                Some((_, e)) if cut.elevation_deg >= e => {}
                _ => chosen = Some((idx, cut.elevation_deg)),
            }
        }
    }
    let (cut_index, elev) = chosen.ok_or("no velocity moment in volume")?;

    let cut = &volume.cuts[cut_index];
    let grid = cut.moments.get(&MomentType::Velocity).unwrap();
    let mut nyqs: Vec<f32> = grid
        .radial_indices
        .iter()
        .filter_map(|ri| cut.radials.get(*ri)?.nyquist_velocity_mps)
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    nyqs.sort_by(f32::total_cmp);
    let nyq = nyqs.get(nyqs.len() / 2).copied().unwrap_or(f32::NAN);

    println!(
        "site={} time={} cut=#{cut_index} elev={elev:.2} rows={} gates={} nyquist={nyq:.2} m/s",
        volume.site.id,
        volume.volume_time,
        grid.radial_count(),
        grid.gate_range.gate_count,
    );

    let range_fraction = std::env::args()
        .nth(3)
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(70);
    let opts = RasterOptions {
        width: 1600,
        height: 1600,
        range_fraction,
    };

    // 1) Raw (aliased) velocity.
    let raw_path = format!("{prefix}_raw.png");
    let raw_img = render_moment_image(&volume, cut_index, MomentType::Velocity, opts)?;
    save_on_black(&raw_img, &raw_path)?;
    println!("wrote {raw_path}");

    // 2) Dealiased velocity via current production algorithm.
    let dealiased = {
        let cut = &volume.cuts[cut_index];
        let grid = cut.moments.get(&MomentType::Velocity).unwrap();
        dealias_velocity_grid(cut, grid)
    };
    volume.cuts[cut_index]
        .moments
        .insert(MomentType::Velocity, dealiased);
    let deal_path = format!("{prefix}_dealiased.png");
    let deal_img = render_moment_image(&volume, cut_index, MomentType::Velocity, opts)?;
    save_on_black(&deal_img, &deal_path)?;
    println!("wrote {deal_path}");

    Ok(())
}
