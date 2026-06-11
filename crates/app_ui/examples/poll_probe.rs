//! Live end-to-end probe of the URL-poll pipeline against the known
//! research-radar feeds: dir.list -> newest -> fetch_volume_bytes ->
//! decode. Run manually: `cargo run --release -p app_ui --example
//! poll_probe` (network).

fn main() {
    let feeds = [
        ("WILU", "https://mesonet-nexrad.agron.iastate.edu/level2/raw/WILU"),
        ("FWLX", "https://mesonet-nexrad.agron.iastate.edu/level2/raw/FWLX"),
        ("FUSA", "https://mesonet-nexrad.agron.iastate.edu/level2/raw/FUSA"),
        ("MZZU", "https://mesonet-nexrad.agron.iastate.edu/level2/raw/MZZU"),
    ];
    for (name, base) in feeds {
        let listing = match data_source::fetch_text(&format!("{base}/dir.list")) {
            Ok(text) => text,
            Err(error) => {
                println!("{name}: dir.list FAILED: {error}");
                continue;
            }
        };
        let Some(newest) = listing
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .filter(|entry| !entry.is_empty())
            .max_by(|a, b| a.cmp(b))
            .map(str::to_owned)
        else {
            println!("{name}: empty dir.list");
            continue;
        };
        let raw = match data_source::fetch_volume_bytes(&format!("{base}/{newest}")) {
            Ok(bytes) => bytes,
            Err(error) => {
                println!("{name}: fetch {newest} FAILED: {error}");
                continue;
            }
        };
        let decoded = if nexrad_io::dorade::looks_like_dorade_bytes(&raw) {
            nexrad_io::dorade::decode_dorade_sweep_volume(&raw)
        } else {
            nexrad_io::decode_volume_from_bytes(&raw)
        };
        match decoded {
            Ok(volume) => println!(
                "{name}: OK {newest} ({} KB) site={} lat={:?} lon={:?} time={} cuts={} moments[0]={:?}",
                raw.len() / 1024,
                volume.site.id,
                volume.site.latitude_deg,
                volume.site.longitude_deg,
                volume.volume_time,
                volume.cuts.len(),
                volume
                    .cuts
                    .first()
                    .map(|cut| cut.moments.keys().collect::<Vec<_>>())
            ),
            Err(error) => println!("{name}: decode {newest} FAILED: {error}"),
        }
    }
}
