//! Live end-to-end probe of the split-volume international ODIM providers
//! (SHMU Slovakia, DWD Germany, CHMI Czechia): plan -> download -> decode
//! -> merge, against the real open-data endpoints.
//!
//! For each requested provider this fetches the newest [`FramePlan`] for
//! one site, downloads every part with `data_source::fetch_volume_bytes`,
//! decodes each through the shared `nexrad_io::decode_supported_volume_bytes`
//! router (ODIM_H5 per the EUMETNET OPERA Data Information Model; Michelson
//! et al., OPERA WP 2.1/2.2, v2.2-2.3), assembles the parts with
//! `radar_core::merge_radar_volumes`, and prints the merged volume's site,
//! cut count, moments per cut, and the `MergeReport` counters (including
//! `skipped_geometry`, which is expected to fire on CHMI's supplemental
//! 1.5-degree task cut whose gate spacing differs from the full volume's
//! same-elevation cut).
//!
//! Usage:
//!   cargo run -p nexrad_io --example intl_assembly_probe -- [shmu|dwd|dwd-full|chmi|all] [site]
//!
//! `dwd-full` probes `DwdProvider::with_dual_pol()` (ZDR/RhoHV/PhiDP
//! sweeps included, ~50 parts); it is not part of `all`. Default sites:
//! shmu=skjav, dwd=asb, chmi=brd. Exit code 1 when any requested probe
//! fails.

use data_source::international::{ChmiProvider, DwdProvider, IntlProvider, ShmuProvider};
use radar_core::RadarVolume;

fn main() {
    let mut args = std::env::args().skip(1);
    let selection = args.next().unwrap_or_else(|| "all".to_owned());
    let site_override = args.next();

    let providers: Vec<(&str, Box<dyn IntlProvider>, &str)> = vec![
        ("shmu", Box::new(ShmuProvider::new()), "skjav"),
        ("dwd", Box::new(DwdProvider::new()), "asb"),
        ("dwd-full", Box::new(DwdProvider::with_dual_pol()), "asb"),
        ("chmi", Box::new(ChmiProvider::new()), "brd"),
    ];

    let mut ran = 0usize;
    let mut failures = 0usize;
    for (key, provider, default_site) in &providers {
        let included = if selection == "all" {
            *key != "dwd-full"
        } else {
            selection == *key
        };
        if !included {
            continue;
        }
        ran += 1;
        let site = site_override.as_deref().unwrap_or(default_site);
        println!("==== {} ({key}) site={site}", provider.label());
        if let Err(err) = probe(provider.as_ref(), site) {
            eprintln!("PROBE FAILED [{key}/{site}]: {err}");
            failures += 1;
        }
        println!();
    }

    if ran == 0 {
        eprintln!("unknown provider '{selection}' (expected shmu, dwd, dwd-full, chmi, or all)");
        std::process::exit(2);
    }
    if failures > 0 {
        std::process::exit(1);
    }
}

fn probe(provider: &dyn IntlProvider, site: &str) -> Result<(), String> {
    let plan = provider.latest(site)?;
    println!("identity: {}", plan.identity);
    println!("parts: {} (merge={})", plan.parts.len(), plan.merge);

    let mut volumes: Vec<RadarVolume> = Vec::with_capacity(plan.parts.len());
    for part in &plan.parts {
        let bytes = data_source::fetch_volume_bytes(&part.url)
            .map_err(|err| format!("download {}: {err}", part.url))?;
        let volume = nexrad_io::decode_supported_volume_bytes(&bytes)
            .map_err(|err| format!("decode {}: {err}", part.url))?;
        println!(
            "  part {} -> {} bytes, site={}, cuts={}, moments[cut0]={}",
            short_name(&part.url),
            bytes.len(),
            volume.site.id,
            volume.cuts.len(),
            volume
                .cuts
                .first()
                .map_or_else(String::new, cut_moment_names),
        );
        volumes.push(volume);
    }

    let (merged, report) = radar_core::merge_radar_volumes(volumes)?;
    println!(
        "merged: site={} ({}) time={} cuts={}",
        merged.site.id,
        merged.site.name.as_deref().unwrap_or("-"),
        merged.volume_time.format("%Y-%m-%dT%H:%M:%SZ"),
        merged.cuts.len()
    );
    for (index, cut) in merged.cuts.iter().enumerate() {
        println!(
            "  cut {index:2} elev={:6.2} radials={:4} moments: {}",
            cut.elevation_deg,
            cut.radials.len(),
            cut_moment_names(cut)
        );
    }
    println!(
        "merge report: merged_moments={} skipped_geometry={} moment_collisions={}",
        report.merged_moments, report.skipped_geometry, report.moment_collisions
    );
    Ok(())
}

fn cut_moment_names(cut: &radar_core::ElevationCut) -> String {
    cut.moments
        .keys()
        .map(|moment| moment.short_name().to_owned())
        .collect::<Vec<_>>()
        .join(",")
}

fn short_name(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}
