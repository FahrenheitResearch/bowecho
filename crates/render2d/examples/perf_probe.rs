use std::path::PathBuf;
use std::time::{Duration, Instant};

use radar_core::{MomentType, RadarVolume};
use render2d::{StormMotion, ViewportMomentCache, ViewportRasterOptions, viewport_rgba_buffer_len};

const DEFAULT_RUNS: usize = 8;
const DEFAULT_DECODE_RUNS: usize = 5;
const DEFAULT_KM_PER_PX: f32 = 0.16;
const MIN_DISPLAYABLE_RADIALS: usize = 180;
const KNOT_TO_MPS: f32 = 0.514_444;
const LOW_CORE_PREVIEW_THREADS: usize = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = parse_args().map_err(|err| format!("{err}\n\n{}", usage()))?;
    let raw = std::fs::read(&config.input)?;

    println!(
        "probe file={} runs={} decode_runs={} rayon_threads={} raw_bytes={}",
        config.input.display(),
        config.runs,
        config.decode_runs,
        rayon::current_num_threads(),
        raw.len()
    );

    let mut decode_timings = Vec::with_capacity(config.decode_runs);
    let mut volume = None;
    for _ in 0..config.decode_runs {
        let start = Instant::now();
        let decoded = nexrad_io::decode_volume_from_bytes(&raw)?;
        decode_timings.push(start.elapsed());
        volume = Some(decoded);
    }
    let volume = volume.expect("decode runs produced a volume");
    print_stats(
        "decode_from_bytes",
        "",
        config.decode_runs,
        TimingStats::from(decode_timings),
    );

    let mut preview_first_timings = Vec::with_capacity(config.decode_runs);
    let mut preview_full_timings = Vec::with_capacity(config.decode_runs);
    for _ in 0..config.decode_runs {
        let start = Instant::now();
        let mut first_preview = None;
        let decoded = if raw.starts_with(&[0x1f, 0x8b]) {
            nexrad_io::decode_gzip_volume_from_bytes_with_preview(
                &raw,
                MIN_DISPLAYABLE_RADIALS,
                |preview| {
                    std::hint::black_box(preview.metadata.decoded_radial_count);
                    first_preview.get_or_insert_with(|| start.elapsed());
                },
            )?
        } else if should_preview_block_bzip_loads_for_threads(rayon::current_num_threads()) {
            nexrad_io::decode_volume_from_bytes_with_bzip_preview(
                &raw,
                MIN_DISPLAYABLE_RADIALS,
                |preview| {
                    std::hint::black_box(preview.metadata.decoded_radial_count);
                    first_preview.get_or_insert_with(|| start.elapsed());
                },
            )?
        } else {
            nexrad_io::decode_volume_from_bytes(&raw)?
        };
        std::hint::black_box(decoded.metadata.decoded_radial_count);
        if let Some(first_preview) = first_preview {
            preview_first_timings.push(first_preview);
        }
        preview_full_timings.push(start.elapsed());
    }
    if !preview_first_timings.is_empty() {
        print_stats(
            "app_preview_first",
            "",
            preview_first_timings.len(),
            TimingStats::from(preview_first_timings),
        );
    }
    print_stats(
        "app_preview_full",
        "",
        preview_full_timings.len(),
        TimingStats::from(preview_full_timings),
    );

    println!(
        "volume site={} cuts={} radials={}",
        volume.site.id,
        volume.cuts.len(),
        volume.metadata.decoded_radial_count
    );

    for viewport in &config.viewports {
        probe_moment(&volume, MomentType::Velocity, *viewport, config.runs, "VEL")?;
        probe_dealiased_velocity(&volume, *viewport, config.runs)?;
        probe_moment(
            &volume,
            MomentType::Reflectivity,
            *viewport,
            config.runs,
            "REF",
        )?;
        probe_storm_relative_velocity(&volume, *viewport, config.runs)?;
    }

    Ok(())
}

fn should_preview_block_bzip_loads_for_threads(threads: usize) -> bool {
    threads <= LOW_CORE_PREVIEW_THREADS
}

fn probe_dealiased_velocity(
    volume: &RadarVolume,
    viewport: ViewportRasterOptions,
    runs: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(cut) = first_cut_with_moment(volume, &MomentType::Velocity) else {
        return Ok(());
    };
    let mut pixels = vec![0; viewport_rgba_buffer_len(viewport)];
    let viewport_label = viewport_name(viewport);
    let mut last_cache = None;

    let build = time_runs(runs, || {
        let cache = ViewportMomentCache::new_dealiased_velocity(volume, cut)?;
        std::hint::black_box(cache.cut_index());
        last_cache = Some(cache);
        Ok(())
    })?;
    let cache = last_cache.expect("DVEL cache build run produced a cache");
    print_stats(
        "moment_cache_build",
        &format!(" product=DVEL cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(build),
    );

    let direct = time_runs(runs, || {
        cache.render_moment_rgba_into(volume, viewport, &mut pixels)?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "viewport_direct",
        &format!(" product=DVEL cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(direct),
    );

    let mut last_sample_cache = None;
    let sample_build = time_runs(runs, || {
        let sample_cache = cache.build_sample_cache(volume, viewport)?;
        std::hint::black_box(sample_cache.sample_count());
        last_sample_cache = Some(sample_cache);
        Ok(())
    })?;
    let sample_cache = last_sample_cache.expect("DVEL sample cache build produced a cache");
    print_stats(
        "sample_cache_build",
        &format!(
            " product=DVEL cut={cut} viewport={viewport_label} samples={} storage_bytes={}",
            sample_cache.sample_count(),
            sample_cache.storage_bytes()
        ),
        runs,
        TimingStats::from(sample_build),
    );

    let cached = time_runs(runs, || {
        cache.render_moment_rgba_with_sample_cache(volume, &sample_cache, &mut pixels)?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "sample_cache_render",
        &format!(" product=DVEL cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(cached),
    );

    cache.render_moment_rgba_with_sample_cache(volume, &sample_cache, &mut pixels)?;
    let reuse = time_runs(runs, || {
        cache.render_moment_rgba_with_sample_cache_reusing_transparency(
            volume,
            &sample_cache,
            &mut pixels,
        )?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "sample_cache_reuse",
        &format!(" product=DVEL cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(reuse),
    );

    let storm_motion = StormMotion {
        direction_deg: 45.0,
        speed_mps: 35.0 * KNOT_TO_MPS,
    };
    let direct = time_runs(runs, || {
        cache.render_storm_relative_velocity_rgba_into(
            volume,
            storm_motion,
            viewport,
            &mut pixels,
        )?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "viewport_direct",
        &format!(" product=DSRV cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(direct),
    );

    let cached = time_runs(runs, || {
        cache.render_storm_relative_velocity_rgba_with_sample_cache(
            volume,
            storm_motion,
            &sample_cache,
            &mut pixels,
        )?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "sample_cache_render",
        &format!(" product=DSRV cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(cached),
    );

    cache.render_storm_relative_velocity_rgba_with_sample_cache(
        volume,
        storm_motion,
        &sample_cache,
        &mut pixels,
    )?;
    let reuse = time_runs(runs, || {
        cache.render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency(
            volume,
            storm_motion,
            &sample_cache,
            &mut pixels,
        )?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "sample_cache_reuse",
        &format!(" product=DSRV cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(reuse),
    );

    Ok(())
}

fn probe_moment(
    volume: &RadarVolume,
    moment: MomentType,
    viewport: ViewportRasterOptions,
    runs: usize,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(cut) = first_cut_with_moment(volume, &moment) else {
        return Ok(());
    };
    let cache = ViewportMomentCache::new(volume, cut, moment)?;
    let mut pixels = vec![0; viewport_rgba_buffer_len(viewport)];
    let viewport_label = viewport_name(viewport);

    let direct = time_runs(runs, || {
        cache.render_moment_rgba_into(volume, viewport, &mut pixels)?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "viewport_direct",
        &format!(" product={label} cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(direct),
    );

    let mut last_sample_cache = None;
    let build = time_runs(runs, || {
        let sample_cache = cache.build_sample_cache(volume, viewport)?;
        std::hint::black_box(sample_cache.sample_count());
        last_sample_cache = Some(sample_cache);
        Ok(())
    })?;
    let sample_cache = last_sample_cache.expect("sample cache build run produced a cache");
    print_stats(
        "sample_cache_build",
        &format!(
            " product={label} cut={cut} viewport={viewport_label} samples={} storage_bytes={}",
            sample_cache.sample_count(),
            sample_cache.storage_bytes()
        ),
        runs,
        TimingStats::from(build),
    );

    let mut last_geometry_cache = None;
    let geometry_build = time_runs(runs, || {
        let geometry_cache = cache.build_geometry_cache(volume, viewport)?;
        std::hint::black_box(geometry_cache.sample_count());
        last_geometry_cache = Some(geometry_cache);
        Ok(())
    })?;
    let geometry_cache = last_geometry_cache.expect("geometry cache build run produced a cache");
    print_stats(
        "geometry_cache_build",
        &format!(
            " product={label} cut={cut} viewport={viewport_label} samples={} storage_bytes={}",
            geometry_cache.sample_count(),
            geometry_cache.storage_bytes()
        ),
        runs,
        TimingStats::from(geometry_build),
    );

    let mut last_geometry_sample_cache = None;
    let geometry_resolve = time_runs(runs, || {
        let sample_cache = cache.build_sample_cache_from_geometry_cache(volume, &geometry_cache)?;
        std::hint::black_box(sample_cache.sample_count());
        last_geometry_sample_cache = Some(sample_cache);
        Ok(())
    })?;
    let geometry_sample_cache =
        last_geometry_sample_cache.expect("geometry cache resolve run produced a sample cache");
    print_stats(
        "geometry_cache_resolve",
        &format!(
            " product={label} cut={cut} viewport={viewport_label} samples={} storage_bytes={}",
            geometry_sample_cache.sample_count(),
            geometry_sample_cache.storage_bytes()
        ),
        runs,
        TimingStats::from(geometry_resolve),
    );

    let cached = time_runs(runs, || {
        cache.render_moment_rgba_with_sample_cache(volume, &sample_cache, &mut pixels)?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "sample_cache_render",
        &format!(" product={label} cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(cached),
    );

    cache.render_moment_rgba_with_sample_cache(volume, &sample_cache, &mut pixels)?;
    let reuse = time_runs(runs, || {
        cache.render_moment_rgba_with_sample_cache_reusing_transparency(
            volume,
            &sample_cache,
            &mut pixels,
        )?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "sample_cache_reuse",
        &format!(" product={label} cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(reuse),
    );

    Ok(())
}

fn probe_storm_relative_velocity(
    volume: &RadarVolume,
    viewport: ViewportRasterOptions,
    runs: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let moment = MomentType::Velocity;
    let Some(cut) = first_cut_with_moment(volume, &moment) else {
        return Ok(());
    };
    let storm_motion = StormMotion {
        direction_deg: 45.0,
        speed_mps: 35.0 * KNOT_TO_MPS,
    };
    let cache = ViewportMomentCache::new(volume, cut, moment)?;
    let sample_cache = cache.build_sample_cache(volume, viewport)?;
    let palette_cache = cache.build_storm_relative_velocity_palette_cache(volume, storm_motion)?;
    let Some(palette_cache) = palette_cache else {
        return Ok(());
    };

    let mut pixels = vec![0; viewport_rgba_buffer_len(viewport)];
    let viewport_label = viewport_name(viewport);

    let direct = time_runs(runs, || {
        cache.render_storm_relative_velocity_rgba_into_with_palette_cache(
            volume,
            storm_motion,
            &palette_cache,
            viewport,
            &mut pixels,
        )?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "viewport_direct",
        &format!(" product=SRV cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(direct),
    );

    let cached = time_runs(runs, || {
        cache.render_storm_relative_velocity_rgba_with_sample_cache_and_palette_cache(
            volume,
            storm_motion,
            &palette_cache,
            &sample_cache,
            &mut pixels,
        )?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "sample_cache_render",
        &format!(" product=SRV cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(cached),
    );

    cache.render_storm_relative_velocity_rgba_with_sample_cache_and_palette_cache(
        volume,
        storm_motion,
        &palette_cache,
        &sample_cache,
        &mut pixels,
    )?;
    let reuse = time_runs(runs, || {
        cache.render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency_and_palette_cache(
            volume,
            storm_motion,
            &palette_cache,
            &sample_cache,
            &mut pixels,
        )?;
        std::hint::black_box(&pixels);
        Ok(())
    })?;
    print_stats(
        "sample_cache_reuse",
        &format!(" product=SRV cut={cut} viewport={viewport_label}"),
        runs,
        TimingStats::from(reuse),
    );

    Ok(())
}

fn time_runs<F>(runs: usize, mut run: F) -> Result<Vec<Duration>, Box<dyn std::error::Error>>
where
    F: FnMut() -> Result<(), Box<dyn std::error::Error>>,
{
    let mut timings = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        run()?;
        timings.push(start.elapsed());
    }
    Ok(timings)
}

fn first_cut_with_moment(volume: &RadarVolume, moment: &MomentType) -> Option<usize> {
    volume
        .cuts
        .iter()
        .position(|cut| cut.moments.contains_key(moment))
}

#[derive(Clone, Copy, Debug)]
struct TimingStats {
    best_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

impl TimingStats {
    fn from(mut timings: Vec<Duration>) -> Self {
        timings.sort();
        let len = timings.len();
        Self {
            best_ms: elapsed_ms(timings[0]),
            p50_ms: elapsed_ms(timings[percentile_index(len, 50)]),
            p95_ms: elapsed_ms(timings[percentile_index(len, 95)]),
            max_ms: elapsed_ms(timings[len - 1]),
        }
    }
}

fn print_stats(kind: &str, fields: &str, runs: usize, stats: TimingStats) {
    println!(
        "perf kind={kind}{fields} runs={runs} best_ms={:.3} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        stats.best_ms, stats.p50_ms, stats.p95_ms, stats.max_ms
    );
}

fn percentile_index(len: usize, percentile: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    ((len - 1) * percentile + 50) / 100
}

#[derive(Debug)]
struct BenchConfig {
    input: PathBuf,
    runs: usize,
    decode_runs: usize,
    viewports: Vec<ViewportRasterOptions>,
}

fn parse_args() -> Result<BenchConfig, String> {
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let mut runs = DEFAULT_RUNS;
    let mut decode_runs = DEFAULT_DECODE_RUNS;
    let mut viewports = Vec::new();
    let mut input = None;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].to_string_lossy();
        match arg.as_ref() {
            "--runs" => {
                index += 1;
                runs = parse_usize_arg(args.get(index), "--runs")?;
            }
            "--decode-runs" => {
                index += 1;
                decode_runs = parse_usize_arg(args.get(index), "--decode-runs")?;
            }
            "--viewport" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--viewport needs WIDTHxHEIGHT".to_owned())?
                    .to_string_lossy();
                viewports.push(parse_viewport(&value)?);
            }
            "--help" | "-h" => return Err(usage()),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ => {
                if input.is_some() {
                    return Err(format!("unexpected extra input {}", arg));
                }
                input = Some(PathBuf::from(args.remove(index)));
                continue;
            }
        }
        index += 1;
    }

    let input = input.ok_or_else(|| "missing Level II input file".to_owned())?;
    if viewports.is_empty() {
        viewports.push(parse_viewport("1320x820")?);
        viewports.push(parse_viewport("1920x1080")?);
    }

    Ok(BenchConfig {
        input,
        runs: runs.max(1),
        decode_runs: decode_runs.max(1),
        viewports,
    })
}

fn parse_usize_arg(value: Option<&std::ffi::OsString>, name: &str) -> Result<usize, String> {
    value
        .ok_or_else(|| format!("{name} needs a positive integer"))?
        .to_string_lossy()
        .parse::<usize>()
        .map_err(|_| format!("{name} needs a positive integer"))
}

fn parse_viewport(value: &str) -> Result<ViewportRasterOptions, String> {
    let Some((width, height)) = value.split_once('x') else {
        return Err(format!("invalid viewport {value}; expected WIDTHxHEIGHT"));
    };
    let width = width
        .parse::<u32>()
        .map_err(|_| format!("invalid viewport width in {value}"))?
        .max(1);
    let height = height
        .parse::<u32>()
        .map_err(|_| format!("invalid viewport height in {value}"))?
        .max(1);
    Ok(ViewportRasterOptions {
        width,
        height,
        radar_x_px: width as f32 * 0.5,
        radar_y_px: height as f32 * 0.5,
        km_per_px_x: DEFAULT_KM_PER_PX,
        km_per_px_y: DEFAULT_KM_PER_PX,
    })
}

fn viewport_name(viewport: ViewportRasterOptions) -> String {
    format!("{}x{}", viewport.width, viewport.height)
}

fn usage() -> String {
    "usage: cargo run --release -p render2d --example perf_probe -- [--runs N] [--decode-runs N] [--viewport WIDTHxHEIGHT] <level2-file>".to_owned()
}

fn elapsed_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
