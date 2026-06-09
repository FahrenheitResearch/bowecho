// Print each cut's elevation + moments (to design REF/CC sourcing for
// rotation detection on split cuts).
use radar_core::RadarVolume;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("usage: cut_probe <l2>")?;
    let volume: RadarVolume = nexrad_io::decode_volume_from_path(path.as_ref())?;
    for (i, cut) in volume.cuts.iter().enumerate().take(10) {
        let moments: Vec<String> = cut.moments.keys().map(|m| format!("{m:?}")).collect();
        println!(
            "#{i:02} {:5.2}deg radials={} {:?}",
            cut.elevation_deg,
            cut.radials.len(),
            moments
        );
    }
    Ok(())
}
