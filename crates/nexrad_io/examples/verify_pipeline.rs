//! Cross-check the pipelined block-bzip decode against the independent
//! normalize-then-parse path on real Level II files.
//!
//! Usage: cargo run --release -p nexrad_io --example verify_pipeline -- <file>...

use nexrad_io::{
    decode_volume_from_bytes, decode_volume_from_bytes_with_bzip_preview, normalize_archive_bytes,
};

fn main() {
    let mut all_ok = true;
    for path in std::env::args().skip(1) {
        let raw = std::fs::read(&path).unwrap();
        let pipelined = match decode_volume_from_bytes(&raw) {
            Ok(volume) => volume,
            Err(err) => {
                println!("{path}: decode error: {err}");
                continue;
            }
        };
        let (normalized, compression) = normalize_archive_bytes(&raw).unwrap();
        let reference = decode_volume_from_bytes(&normalized).unwrap();

        let cuts_match = pipelined.cuts == reference.cuts;
        let site_match = pipelined.site == reference.site;
        let vcp_match = pipelined.vcp == reference.vcp;
        let radials_match =
            pipelined.metadata.decoded_radial_count == reference.metadata.decoded_radial_count;

        let mut preview_radials = None;
        let with_preview = decode_volume_from_bytes_with_bzip_preview(&raw, 360, |preview| {
            preview_radials = Some(preview.metadata.decoded_radial_count);
        })
        .unwrap();
        let preview_full_match = with_preview.cuts == pipelined.cuts;

        let ok = cuts_match && site_match && vcp_match && radials_match && preview_full_match;
        all_ok &= ok;
        println!(
            "{path}: compression={compression:?} cuts={} radials={} preview_radials={preview_radials:?} \
             cuts_match={cuts_match} site_match={site_match} vcp_match={vcp_match} \
             radials_match={radials_match} preview_full_match={preview_full_match} => {}",
            pipelined.cuts.len(),
            pipelined.metadata.decoded_radial_count,
            if ok { "OK" } else { "MISMATCH" }
        );
    }
    if !all_ok {
        std::process::exit(1);
    }
}
