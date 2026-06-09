use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const LOW_CORE_PREVIEW_THREADS: usize = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(input) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: cargo run -p nexrad_io --example bench_decode -- <level2-file>");
        std::process::exit(2);
    };

    let read_start = Instant::now();
    let raw = fs::read(&input)?;
    let read_elapsed = read_start.elapsed();

    let normalize_start = Instant::now();
    let (normalized, compression) = nexrad_io::normalize_archive_bytes(&raw)?;
    let normalize_elapsed = normalize_start.elapsed();

    let preview_start = Instant::now();
    let preview = nexrad_io::decode_bzip_block_preview_from_bytes(&raw, 180)?;
    let preview_elapsed = preview_start.elapsed();

    let gzip_preview_start = Instant::now();
    let gzip_preview = nexrad_io::decode_gzip_preview_from_bytes(&raw, 180)?;
    let gzip_preview_elapsed = gzip_preview_start.elapsed();

    let app_preview_start = Instant::now();
    let mut app_preview = None;
    let app_preview_volume = if raw.starts_with(&[0x1f, 0x8b]) {
        nexrad_io::decode_gzip_volume_from_bytes_with_preview(&raw, 180, |volume| {
            app_preview = Some((
                app_preview_start.elapsed(),
                volume.site.id,
                volume.cuts.len(),
                volume.metadata.decoded_radial_count,
            ));
        })?
    } else if should_preview_block_bzip_loads_for_threads(rayon::current_num_threads()) {
        nexrad_io::decode_volume_from_bytes_with_bzip_preview(&raw, 180, |volume| {
            app_preview = Some((
                app_preview_start.elapsed(),
                volume.site.id,
                volume.cuts.len(),
                volume.metadata.decoded_radial_count,
            ));
        })?
    } else {
        nexrad_io::decode_volume_from_bytes(&raw)?
    };
    let app_preview_elapsed = app_preview_start.elapsed();

    let preview_full_start = Instant::now();
    let mut preview_full_preview = None;
    let preview_full_volume =
        nexrad_io::decode_volume_from_bytes_with_bzip_preview(&raw, 180, |volume| {
            preview_full_preview = Some((
                preview_full_start.elapsed(),
                volume.site.id,
                volume.cuts.len(),
                volume.metadata.decoded_radial_count,
            ));
        })?;
    let preview_full_elapsed = preview_full_start.elapsed();

    let mut parse_timings = Vec::new();
    let mut summary = None;
    for _ in 0..10 {
        let parse_start = Instant::now();
        let volume = nexrad_io::decode_normalized_volume_bytes(&normalized, compression)?;
        let parse_elapsed = parse_start.elapsed();
        summary = Some((
            volume.site.id,
            volume.cuts.len(),
            volume.metadata.decoded_radial_count,
        ));
        std::hint::black_box(summary.as_ref());
        parse_timings.push(parse_elapsed);
    }
    parse_timings.sort();

    let mut decode_timings = Vec::new();
    for _ in 0..5 {
        let decode_start = Instant::now();
        let volume = nexrad_io::decode_volume_from_bytes(&raw)?;
        std::hint::black_box(volume.metadata.decoded_radial_count);
        decode_timings.push(decode_start.elapsed());
    }
    decode_timings.sort();

    let (site, cuts, radials) = summary.expect("at least one parse iteration ran");
    println!(
        "file_bytes={} normalized_bytes={} compression={compression:?}",
        raw.len(),
        normalized.len()
    );
    println!(
        "read_ms={:.3} normalize_ms={:.3} parse_median_ms={:.3} parse_best_ms={:.3}",
        elapsed_ms(read_elapsed),
        elapsed_ms(normalize_elapsed),
        elapsed_ms(parse_timings[parse_timings.len() / 2]),
        elapsed_ms(parse_timings[0])
    );
    match preview {
        Some(volume) => println!(
            "bzip_preview_ms={:.3} site={} cuts={} radials={}",
            elapsed_ms(preview_elapsed),
            volume.site.id,
            volume.cuts.len(),
            volume.metadata.decoded_radial_count
        ),
        None => println!(
            "bzip_preview_ms={:.3} unavailable",
            elapsed_ms(preview_elapsed)
        ),
    }
    match preview_full_preview {
        Some((preview_elapsed, site, preview_cuts, preview_radials)) => println!(
            "decode_with_preview first_ms={:.3} full_ms={:.3} site={} preview_cuts={} preview_radials={} full_cuts={} full_radials={}",
            elapsed_ms(preview_elapsed),
            elapsed_ms(preview_full_elapsed),
            site,
            preview_cuts,
            preview_radials,
            preview_full_volume.cuts.len(),
            preview_full_volume.metadata.decoded_radial_count
        ),
        None => println!(
            "decode_with_preview full_ms={:.3} preview_unavailable full_cuts={} full_radials={}",
            elapsed_ms(preview_full_elapsed),
            preview_full_volume.cuts.len(),
            preview_full_volume.metadata.decoded_radial_count
        ),
    }
    match gzip_preview {
        Some(volume) => println!(
            "gzip_preview_ms={:.3} site={} cuts={} radials={}",
            elapsed_ms(gzip_preview_elapsed),
            volume.site.id,
            volume.cuts.len(),
            volume.metadata.decoded_radial_count
        ),
        None => println!(
            "gzip_preview_ms={:.3} unavailable",
            elapsed_ms(gzip_preview_elapsed)
        ),
    }
    match app_preview {
        Some((preview_elapsed, site, preview_cuts, preview_radials)) => println!(
            "app_preview first_ms={:.3} full_ms={:.3} site={} preview_cuts={} preview_radials={} full_cuts={} full_radials={}",
            elapsed_ms(preview_elapsed),
            elapsed_ms(app_preview_elapsed),
            site,
            preview_cuts,
            preview_radials,
            app_preview_volume.cuts.len(),
            app_preview_volume.metadata.decoded_radial_count
        ),
        None => println!(
            "app_preview full_ms={:.3} preview_unavailable full_cuts={} full_radials={}",
            elapsed_ms(app_preview_elapsed),
            app_preview_volume.cuts.len(),
            app_preview_volume.metadata.decoded_radial_count
        ),
    }
    println!(
        "decode_from_bytes_median_ms={:.3} decode_from_bytes_best_ms={:.3}",
        elapsed_ms(decode_timings[decode_timings.len() / 2]),
        elapsed_ms(decode_timings[0])
    );
    println!("site={site} cuts={cuts} radials={radials}");

    Ok(())
}

fn elapsed_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn should_preview_block_bzip_loads_for_threads(threads: usize) -> bool {
    threads <= LOW_CORE_PREVIEW_THREADS
}
