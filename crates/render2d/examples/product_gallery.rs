// Render the full product set for a scan to PNGs through the SAME
// ViewportMomentCache path the GUI uses — visual proof every product + palette
// works end to end (base moments, dual-pol, dealiased velocity, the derived
// volumetric/shear products). usage: product_gallery <l2-file> <out-dir>

use std::path::PathBuf;

use color_tables::{ColorTableFamily, ColorTableSet};
use image::{ImageBuffer, Rgba};
use radar_core::{MomentType, RadarVolume};
use render2d::{
    ECHO_TOP_THRESHOLD_DBZ, ViewportMomentCache, ViewportRasterOptions, azimuthal_shear_grid,
    composite_reflectivity_grid, echo_top_grid, mehs_grid, radial_divergence_grid,
    reflectivity_cross_section, viewport_rgba_buffer_len, vil_density_grid, vil_grid,
};

fn lowest_cut_with(volume: &RadarVolume, moment: &MomentType) -> Option<usize> {
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(moment))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
        .map(|(i, _)| i)
}

fn save_cache(
    volume: &RadarVolume,
    cache: &ViewportMomentCache,
    opts: ViewportRasterOptions,
    path: &str,
) {
    let mut px = vec![0u8; viewport_rgba_buffer_len(opts)];
    let Ok((w, h)) = cache.render_moment_rgba_into(volume, opts, &mut px) else {
        eprintln!("render failed: {path}");
        return;
    };
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
    let input = PathBuf::from(
        args.next()
            .ok_or("usage: product_gallery <l2-file> <out-dir>")?,
    );
    let dir = args
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into());
    let volume: RadarVolume = nexrad_io::decode_volume_from_path(&input)?;
    let tables = ColorTableSet::default();

    // Full-disk viewport centred on the radar (~±250 km).
    let opts = ViewportRasterOptions {
        width: 900,
        height: 900,
        radar_x_px: 450.0,
        radar_y_px: 450.0,
        km_per_px_x: 0.56,
        km_per_px_y: 0.56,
    };

    let ref_cut = lowest_cut_with(&volume, &MomentType::Reflectivity);
    let vel_cut = lowest_cut_with(&volume, &MomentType::Velocity);

    // Base + dual-pol moments via their normal caches.
    let base = [
        ("REF", MomentType::Reflectivity),
        ("CC", MomentType::CorrelationCoefficient),
        ("ZDR", MomentType::DifferentialReflectivity),
        ("SW", MomentType::SpectrumWidth),
    ];
    for (name, moment) in base {
        if let Some(cut) = lowest_cut_with(&volume, &moment) {
            if let Ok(c) = ViewportMomentCache::new_with_color_tables(&volume, cut, moment, &tables)
            {
                save_cache(&volume, &c, opts, &format!("{dir}/gallery_{name}.png"));
            }
        }
    }

    // Dealiased velocity.
    if let Some(cut) = vel_cut {
        if let Ok(c) =
            ViewportMomentCache::new_dealiased_velocity_with_color_tables(&volume, cut, &tables)
        {
            save_cache(
                &volume,
                &c,
                opts,
                &format!("{dir}/gallery_VEL_dealiased.png"),
            );
        }
    }

    // Derived volume products on the base reflectivity tilt.
    if let Some(base_idx) = ref_cut {
        let derived: Vec<(&str, Option<radar_core::MomentGrid>, ColorTableFamily)> = vec![
            (
                "CREF",
                composite_reflectivity_grid(&volume),
                ColorTableFamily::Reflectivity,
            ),
            (
                "EchoTops",
                echo_top_grid(&volume, ECHO_TOP_THRESHOLD_DBZ),
                ColorTableFamily::EchoTops,
            ),
            ("VIL", vil_grid(&volume), ColorTableFamily::Vil),
            (
                "VILDensity",
                vil_density_grid(&volume),
                ColorTableFamily::VilDensity,
            ),
            (
                "MEHS",
                mehs_grid(&volume, 3200.0, 6400.0),
                ColorTableFamily::HailSize,
            ),
        ];
        for (name, grid, family) in derived {
            if let Some(grid) = grid {
                if let Ok(c) =
                    ViewportMomentCache::new_derived(&volume, base_idx, grid, family, &tables)
                {
                    save_cache(&volume, &c, opts, &format!("{dir}/gallery_{name}.png"));
                }
            }
        }
    }

    // Per-cut velocity derivatives.
    if let Some(cut) = vel_cut {
        let velocity = volume.cuts[cut].moments.get(&MomentType::Velocity).unwrap();
        for (name, grid) in [
            ("AzShear", azimuthal_shear_grid(&volume.cuts[cut], velocity)),
            (
                "Divergence",
                radial_divergence_grid(&volume.cuts[cut], velocity),
            ),
        ] {
            if let Ok(c) = ViewportMomentCache::new_derived(
                &volume,
                cut,
                grid,
                ColorTableFamily::AzimuthalShear,
                &tables,
            ) {
                save_cache(&volume, &c, opts, &format!("{dir}/gallery_{name}.png"));
            }
        }
    }

    // A cross-section through the strongest composite cell, colorized REF.
    if let Some(comp) = composite_reflectivity_grid(&volume) {
        if let Some(base_idx) = ref_cut {
            let bc = &volume.cuts[base_idx];
            let bg = bc.moments.get(&MomentType::Reflectivity).unwrap();
            let (mut best, mut rg) = (f32::NEG_INFINITY, (0usize, 0usize));
            for r in 0..comp.radial_count() {
                for g in 0..comp.gate_range.gate_count {
                    if let Some(v) = comp
                        .scaled_value(r, g)
                        .filter(|v| v.is_finite() && *v > best)
                    {
                        best = v;
                        rg = (r, g);
                    }
                }
            }
            let az = bc.radials[bg.radial_indices[rg.0]].azimuth_deg.to_radians();
            let rkm = (bg.gate_range.first_gate_m as f32
                + rg.1 as f32 * bg.gate_range.gate_spacing_m as f32)
                / 1000.0;
            let (e, n) = (rkm * az.sin(), rkm * az.cos());
            if let Some(xs) = reflectivity_cross_section(
                &volume,
                (e - 30.0, n),
                (e + 30.0, n),
                700,
                320,
                18_000.0,
            ) {
                let table = tables.for_family(ColorTableFamily::Reflectivity);
                let mut img =
                    ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(700, 320, Rgba([15, 17, 20, 255]));
                for y in 0..320 {
                    for x in 0..700 {
                        let v = xs.values[y * 700 + x];
                        if v.is_finite() {
                            let c = table.color_for_value(v);
                            if c[3] > 0 {
                                img.put_pixel(x as u32, y as u32, Rgba([c[0], c[1], c[2], 255]));
                            }
                        }
                    }
                }
                let path = format!("{dir}/gallery_CrossSection.png");
                img.save(&path)?;
                println!("wrote {path}");
            }
        }
    }
    Ok(())
}
