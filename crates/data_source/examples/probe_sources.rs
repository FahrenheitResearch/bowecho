fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let requested_site = args
        .iter()
        .find(|arg| !arg.starts_with("--"))
        .map(|site| site.to_ascii_uppercase())
        .unwrap_or_else(|| "KTLX".to_owned());
    let should_download = args.iter().any(|arg| arg == "--download");
    let sites = data_source::list_recent_level2_sites(7)?;
    println!("level2_sites={}", sites.len());

    let site = sites
        .iter()
        .find(|site| site.level2_id == requested_site)
        .cloned()
        .unwrap_or_else(|| data_source::RadarSite::new(&requested_site));

    let l2 = data_source::latest_level2_object(&site.level2_id, 7)?;
    println!("latest_l2={} bytes={}", l2.key, l2.size);
    let realtime = data_source::latest_realtime_level2_volume(&site.level2_id)?;
    println!(
        "latest_realtime={} id={} chunks={} complete={} bytes={}",
        realtime.volume_time,
        realtime.volume_id,
        realtime.chunks.len(),
        realtime.complete,
        realtime.total_size
    );
    if should_download {
        let cache_dir = std::env::temp_dir()
            .join("radar-rs-probe")
            .join(&site.level2_id);
        let downloaded = data_source::download_realtime_volume(&realtime, &cache_dir)?;
        println!(
            "downloaded_realtime={} cache_hit={} bytes={}",
            downloaded.path.display(),
            downloaded.cache_hit,
            downloaded.object.size
        );
    }

    // --poll N: time repeated latest-volume polls (the app's 1 Hz live loop)
    // to show listing-cache behaviour.
    if let Some(polls) = args
        .iter()
        .position(|arg| arg == "--poll")
        .and_then(|index| args.get(index + 1))
        .and_then(|count| count.parse::<usize>().ok())
    {
        for poll in 0..polls {
            let start = std::time::Instant::now();
            let volume = data_source::latest_realtime_level2_volume(&site.level2_id)?;
            println!(
                "poll={poll} ms={:.1} id={} chunks={} complete={}",
                start.elapsed().as_secs_f64() * 1000.0,
                volume.volume_id,
                volume.chunks.len(),
                volume.complete
            );
        }
    }

    Ok(())
}
