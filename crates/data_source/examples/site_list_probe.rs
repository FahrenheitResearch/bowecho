//! Probe the site catalog exactly as the app builds it: how many sites,
//! how many TDWRs (Txxx), and whether they carry coordinates.
fn main() {
    let sites = data_source::fetch_level2_radar_sites(7).expect("catalog");
    let tdwr: Vec<_> = sites
        .iter()
        .filter(|s| s.level2_id.starts_with('T'))
        .collect();
    println!("total sites: {}", sites.len());
    println!("TDWR (Txxx): {}", tdwr.len());
    for site in tdwr.iter().take(5) {
        println!(
            "  {} {:?} lat={:?} lon={:?}",
            site.level2_id, site.name, site.latitude_deg, site.longitude_deg
        );
    }
}
