//! Decode every radar volume in a mobile-radar zip archive and summarize.
//!
//! Usage: cargo run -p nexrad_io --example mobile_archive_probe -- deployment.zip

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: mobile_archive_probe <archive.zip>");
        std::process::exit(2);
    };

    let started = Instant::now();
    let volumes = nexrad_io::mobile_archive::decode_mobile_archive_from_path(path.as_ref())?;
    let elapsed = started.elapsed();
    println!(
        "{} volumes decoded in {:.2}s",
        volumes.len(),
        elapsed.as_secs_f32()
    );
    for entry in &volumes {
        let volume = &entry.volume;
        let moments: Vec<String> = volume
            .cuts
            .first()
            .map(|cut| cut.moments.keys().map(|m| m.to_string()).collect())
            .unwrap_or_default();
        println!(
            "  {} t={} cuts={} radials={} members={} lat={:?} lon={:?} [{}] {}",
            volume.site.id,
            volume.volume_time.format("%Y-%m-%d %H:%M:%S"),
            volume.cuts.len(),
            volume.metadata.decoded_radial_count,
            entry.member_count,
            volume.site.latitude_deg,
            volume.site.longitude_deg,
            moments.join(","),
            entry.member_label,
        );
    }
    Ok(())
}
