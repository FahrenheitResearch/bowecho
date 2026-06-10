fn main() {
    let url =
        "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/8/97/62";
    match data_source::fetch_bytes(url) {
        Ok(bytes) => println!(
            "OK {} bytes, magic {:02x?}",
            bytes.len(),
            &bytes[..4.min(bytes.len())]
        ),
        Err(err) => println!("FETCH ERROR: {err}"),
    }
}
