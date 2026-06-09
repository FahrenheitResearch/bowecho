// Verify the vertical cross-section on a real scan: locate the strongest
// composite-reflectivity cell, slice a W->E section through it, and render it
// (REF palette) to a PNG so the convective vertical structure is visible.
//
// usage: cargo run --release -p render2d --example cross_section_probe -- <l2-file> <out.png>

use std::path::PathBuf;

use color_tables::builtin_reflectivity_table;
use image::{ImageBuffer, Rgba};
use radar_core::{MomentType, RadarVolume};
use render2d::{composite_reflectivity_grid, reflectivity_cross_section};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(
        args.next()
            .ok_or("usage: cross_section_probe <l2-file> <out.png>")?,
    );
    let out = args
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "xs.png".into());

    let volume: RadarVolume = nexrad_io::decode_volume_from_path(&input)?;

    // base reflectivity cut (lowest elevation)
    let (base_idx, base_cut) = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Reflectivity))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
        .ok_or("no reflectivity")?;
    let base_grid = base_cut.moments.get(&MomentType::Reflectivity).unwrap();

    let comp = composite_reflectivity_grid(&volume).ok_or("composite failed")?;

    // locate strongest composite cell
    let rows = comp.radial_count();
    let gates = comp.gate_range.gate_count;
    let (mut best, mut best_rg) = (f32::NEG_INFINITY, (0usize, 0usize));
    for r in 0..rows {
        for g in 0..gates {
            if let Some(v) = comp
                .scaled_value(r, g)
                .filter(|v| v.is_finite() && *v > best)
            {
                best = v;
                best_rg = (r, g);
            }
        }
    }
    let (br, bg) = best_rg;
    let az = base_cut.radials[base_grid.radial_indices[br]]
        .azimuth_deg
        .to_radians();
    let range_km = (base_grid.gate_range.first_gate_m as f32
        + bg as f32 * base_grid.gate_range.gate_spacing_m as f32)
        / 1000.0;
    let cx_e = range_km * az.sin();
    let cx_n = range_km * az.cos();
    println!(
        "max composite {best:.1} dBZ at az={:.1} range={range_km:.1} km -> (E {cx_e:.1}, N {cx_n:.1}) km; base cut #{base_idx}",
        az.to_degrees()
    );

    let half = 30.0f32;
    let (w, h, top_m) = (700usize, 320usize, 18_000.0f32);
    let xs = reflectivity_cross_section(
        &volume,
        (cx_e - half, cx_n),
        (cx_e + half, cx_n),
        w,
        h,
        top_m,
    )
    .ok_or("cross section failed")?;

    let table = builtin_reflectivity_table();
    let mut img =
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(w as u32, h as u32, Rgba([14, 16, 20, 255]));
    let mut n_filled = 0u64;
    for y in 0..h {
        for x in 0..w {
            let v = xs.values[y * w + x];
            if v.is_finite() {
                n_filled += 1;
                let c = table.color_for_value(v);
                if c[3] > 0 {
                    img.put_pixel(x as u32, y as u32, Rgba([c[0], c[1], c[2], 255]));
                }
            }
        }
    }
    // height gridlines every ~3 km
    for kft in (3..=15).step_by(3) {
        let z = kft as f32 * 1000.0;
        let y = ((1.0 - z / top_m) * (h as f32 - 1.0)).round() as u32;
        for x in (0..w as u32).step_by(6) {
            img.put_pixel(x, y.min(h as u32 - 1), Rgba([70, 70, 80, 255]));
        }
    }
    img.save(&out)?;
    println!(
        "section {:.0} km long, top {:.0} m, {n_filled} filled cells -> wrote {out}",
        xs.length_m / 1000.0,
        xs.top_m
    );
    Ok(())
}
