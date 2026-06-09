use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use nexrad_io::decode_volume_from_path;
use radar_core::{MomentStorage, RadarVolume};

fn main() {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: cargo run -p nexrad_io --example inspect -- <level2-file>");
        std::process::exit(2);
    };

    match decode_volume_from_path(&path) {
        Ok(volume) => {
            println!("site: {}", volume.site.id);
            println!("volume_time: {}", volume.volume_time);
            if let Some(vcp) = &volume.vcp {
                println!("vcp: {}", vcp.pattern);
            }
            println!(
                "messages: {} decoded_radials: {} skipped_messages: {}",
                volume.metadata.message_count,
                volume.metadata.decoded_radial_count,
                volume.metadata.skipped_message_count
            );
            println!("cuts: {}", volume.cuts.len());
            for (index, cut) in volume.cuts.iter().enumerate() {
                let start_time = cut_start_time(&volume, index)
                    .map(|time| time.format("%H:%M:%S").to_string())
                    .unwrap_or_else(|| "--:--:--".to_owned());
                let end_time = cut_end_time(&volume, index)
                    .map(|time| time.format("%H:%M:%S").to_string())
                    .unwrap_or_else(|| "--:--:--".to_owned());
                let moments = cut
                    .moments
                    .values()
                    .map(|grid| {
                        let (storage, bytes_per_gate) = match &grid.storage {
                            MomentStorage::U8(_) => ("u8", 1),
                            MomentStorage::U16(_) => ("u16", 2),
                            MomentStorage::F32(_) => ("f32", 4),
                        };
                        let gate_bytes = grid
                            .radial_count()
                            .saturating_mul(grid.gate_range.gate_count)
                            .saturating_mul(bytes_per_gate);
                        format!(
                            "{}:{}x{} {storage} {} KiB",
                            grid.moment.short_name(),
                            grid.radial_count(),
                            grid.gate_range.gate_count,
                            gate_bytes / 1024
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "  cut #{index}: elev={:.2} deg radials={} time={start_time}-{end_time} moments=[{}]",
                    cut.elevation_deg,
                    cut.radials.len(),
                    moments
                );
            }
        }
        Err(err) => {
            eprintln!("decode failed: {err}");
            std::process::exit(1);
        }
    }
}

fn cut_start_time(volume: &RadarVolume, cut_index: usize) -> Option<DateTime<Utc>> {
    let cut = volume.cuts.get(cut_index)?;
    cut.radials
        .iter()
        .filter_map(|radial| radial_collection_time(volume, radial.time_offset_ms))
        .min()
}

fn cut_end_time(volume: &RadarVolume, cut_index: usize) -> Option<DateTime<Utc>> {
    let cut = volume.cuts.get(cut_index)?;
    cut.radials
        .iter()
        .filter_map(|radial| radial_collection_time(volume, radial.time_offset_ms))
        .max()
}

fn radial_collection_time(volume: &RadarVolume, time_offset_ms: i32) -> Option<DateTime<Utc>> {
    let midnight = volume
        .volume_time
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive))?;
    let milliseconds = chrono::Duration::milliseconds(time_offset_ms as i64);
    let midnight_candidate = midnight + milliseconds;
    let relative_candidate = volume.volume_time + milliseconds;
    let midnight_delta = (midnight_candidate - volume.volume_time)
        .num_milliseconds()
        .abs();
    let relative_delta = (relative_candidate - volume.volume_time)
        .num_milliseconds()
        .abs();
    Some(if midnight_delta <= relative_delta {
        midnight_candidate
    } else {
        relative_candidate
    })
}
